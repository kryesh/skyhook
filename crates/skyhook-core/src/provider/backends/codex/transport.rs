//! WebSocket and HTTP response streams share the Responses codec and continuation assembly.
use super::super::{common::bind_reasoning_scope, responses, transport};
use super::{
    error,
    recovery::{close_error, socket_error, websocket_error},
    session::{Connection, Continuation, ReusableConnection, Session},
};
use crate::provider::{
    CodexWebSocketError, ProviderError, ProviderErrorKind, ResponseStream,
    protocol::{Message as ProtocolMessage, ModelRequest, ResponseAssembler, ResponseChunk},
};
use futures_util::{SinkExt, StreamExt, stream};
use serde_json::{Map, Value};
use std::{collections::VecDeque, time::Duration};
use tokio::{net::TcpStream, sync::OwnedMutexGuard, time::Instant};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};

pub(super) const DEADLINE: Duration = Duration::from_secs(120);
pub(super) type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct WsState {
    connection: Option<Connection>,
    session: OwnedMutexGuard<Session>,
    decoder: responses::Decoder,
    pending: VecDeque<Result<ResponseChunk, ProviderError>>,
    done: bool,
    failed: bool,
    settings: Map<String, Value>,
    input: Vec<Value>,
    completed: Option<Continuation>,
    assembler: ResponseAssembler,
    scope: String,
}
pub(super) fn ws_stream(
    connection: Connection,
    session: OwnedMutexGuard<Session>,
    decoder: responses::Decoder,
    settings: Map<String, Value>,
    input: Vec<Value>,
    scope: String,
) -> ResponseStream {
    Box::pin(stream::unfold(
        WsState {
            connection: Some(connection),
            session,
            decoder,
            pending: VecDeque::new(),
            done: false,
            failed: false,
            settings,
            input,
            completed: None,
            assembler: ResponseAssembler::default(),
            scope,
        },
        |mut state| async move {
            loop {
                if let Some(chunk) = state.pending.pop_front() {
                    return Some((chunk, state));
                }
                if state.done {
                    if !state.failed {
                        *state.session = Session::Reusable(Box::new(ReusableConnection {
                            connection: state.connection.take().unwrap(),
                            since: Instant::now(),
                            continuation: state.completed.take(),
                        }));
                    }
                    return None;
                }
                let frame = tokio::time::timeout(
                    DEADLINE,
                    state.connection.as_mut().unwrap().socket.next(),
                )
                .await;
                let decoded = match frame {
                    Ok(Some(Ok(Message::Text(text)))) => match serde_json::from_str::<Value>(&text)
                    {
                        Ok(event) => {
                            if is_transport_metadata(&event) {
                                continue;
                            }
                            let terminal = matches!(
                                event.get("type").and_then(Value::as_str),
                                Some(
                                    "response.completed"
                                        | "response.failed"
                                        | "response.incomplete"
                                        | "error"
                                )
                            );
                            if event["type"] == "response.completed"
                                && let Some(id) = event["response"]["id"].as_str()
                            {
                                let input = state.input.clone();
                                state.completed = Some(Continuation {
                                    id: id.into(),
                                    input,
                                    settings: state.settings.clone(),
                                });
                            }
                            let mut result = state.decoder.feed(event);
                            if let Ok(chunks) = &mut result {
                                for chunk in chunks {
                                    bind_reasoning_scope(chunk, &state.scope);
                                    if let Err(error) = state.assembler.push(chunk) {
                                        result = Err(error);
                                        break;
                                    }
                                }
                            }
                            if terminal {
                                state.done = true;
                                if let Ok(chunks) = &mut result {
                                    match state.decoder.finish() {
                                        Ok(final_chunks) => chunks.extend(final_chunks),
                                        Err(error) => result = Err(error),
                                    }
                                }
                                // Reuse the shared encoder to canonicalize final blocks exactly
                                // as the runtime will replay them. Native output includes IDs
                                // and status fields not present in ordinary text/tool history.
                                let assembled = if result.is_ok() {
                                    match std::mem::take(&mut state.assembler).finish() {
                                        Ok((items, _, _)) => Some(items),
                                        Err(error) => {
                                            result = Err(error);
                                            None
                                        }
                                    }
                                } else {
                                    None
                                };
                                if let (Some(completed), Some(items)) =
                                    (&mut state.completed, assembled)
                                {
                                    let replay = ModelRequest {
                                        model: completed.settings["model"]
                                            .as_str()
                                            .unwrap_or_default()
                                            .into(),
                                        system: vec![],
                                        tail: Vec::new(),
                                        history_lifetime: Default::default(),
                                        history: vec![ProtocolMessage::Assistant(items)],
                                        tools: vec![],
                                        response_schema: None,
                                        reasoning: None,
                                        max_output_tokens: None,
                                        correlation: None,
                                        blobs: Default::default(),
                                    };
                                    match responses::encode(&replay) {
                                        Ok(encoded) => {
                                            completed.input = state.input.clone();
                                            completed.input.extend(encoded.input);
                                        }
                                        Err(_) => state.completed = None,
                                    }
                                }
                            }
                            result
                        }
                        Err(_) => Err(error(
                            ProviderErrorKind::Protocol,
                            "invalid Codex WebSocket JSON",
                        )),
                    },
                    Ok(Some(Ok(Message::Ping(payload)))) => {
                        match tokio::time::timeout(
                            DEADLINE,
                            state
                                .connection
                                .as_mut()
                                .unwrap()
                                .socket
                                .send(Message::Pong(payload)),
                        )
                        .await
                        {
                            Ok(Ok(())) => continue,
                            Ok(Err(error)) => Err(socket_error(error, CodexWebSocketError::Ping)),
                            Err(_) => Err(websocket_error(CodexWebSocketError::Ping)),
                        }
                    }
                    Ok(Some(Ok(Message::Pong(_)))) => continue,
                    Ok(Some(Ok(Message::Binary(_)))) => Err(error(
                        ProviderErrorKind::Protocol,
                        "unexpected Codex binary frame",
                    )),
                    Err(_) => Err(websocket_error(CodexWebSocketError::ReadTimeout)),
                    Ok(None) => Err(websocket_error(CodexWebSocketError::EndOfStream)),
                    Ok(Some(Err(error))) => Err(socket_error(error, CodexWebSocketError::Read)),
                    Ok(Some(Ok(Message::Close(frame)))) => {
                        Err(close_error(frame.map(|frame| frame.code)))
                    }
                    Ok(Some(Ok(Message::Frame(_)))) => {
                        Err(ProviderError::protocol("unexpected Codex WebSocket frame"))
                    }
                };
                match decoded {
                    Ok(chunks) => state.pending.extend(chunks.into_iter().map(Ok)),
                    Err(error) => {
                        state.failed = true;
                        state.done = true;
                        state.pending.push_back(Err(error));
                    }
                }
            }
        },
    ))
}
// Subscription quota notifications are transport metadata, not Responses output.
// Recognize only the documented/observed type; unknown semantic events still fail.
pub(in super::super) fn is_transport_metadata(event: &Value) -> bool {
    matches!(
        event.get("type").and_then(Value::as_str),
        Some("codex.rate_limits" | "codex.response.metadata" | "responsesapi.websocket_timing")
    )
}

pub(super) fn http_stream(
    events: transport::SseStream,
    decoder: responses::Decoder,
    session: OwnedMutexGuard<Session>,
    scope: String,
) -> ResponseStream {
    super::super::decode_stream(
        events,
        super::super::Decoder::Codex(decoder),
        scope,
        session,
    )
}

#[cfg(test)]
pub(super) mod tests {
    use super::super::super::common::{filter_reasoning_scope, reasoning_scope};
    use super::super::session::websocket_request;
    use super::*;
    use crate::provider::ProviderRecovery;
    use crate::provider::protocol::{BlockContent, ContentDelta, StopReason};
    use reqwest::header::HeaderValue;
    use serde_json::json;
    use std::sync::Arc;
    use tokio::sync::Mutex;
    use tokio_tungstenite::tungstenite::{self, protocol::frame::coding::CloseCode};

    type Chunks = Vec<Result<ResponseChunk, ProviderError>>;

    /// Accepts one WebSocket connection and hands its server side to `serve`.
    pub(in super::super) async fn serve_socket<F>(
        serve: impl FnOnce(WebSocketStream<TcpStream>) -> F + Send + 'static,
    ) -> (Socket, tokio::task::JoinHandle<()>)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            serve(tokio_tungstenite::accept_async(tcp).await.unwrap()).await;
        });
        let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}"))
            .await
            .unwrap();
        (socket, server)
    }

    pub(in super::super) async fn mock_socket(
        events: Vec<Value>,
    ) -> (Socket, tokio::task::JoinHandle<()>) {
        serve_socket(|mut socket| async move {
            for event in events {
                let frame = Message::Text(event.to_string().into());
                socket.send(frame).await.unwrap();
            }
            socket.close(None).await.unwrap();
        })
        .await
    }

    fn connection(socket: Socket) -> Connection {
        Connection {
            socket,
            affinity: Some(HeaderValue::from_static("test-affinity")),
        }
    }

    fn session() -> Arc<Mutex<Session>> {
        Arc::new(Mutex::new(Session::default()))
    }

    async fn ws_with(
        connection: Connection,
        session: &Arc<Mutex<Session>>,
        settings: Map<String, Value>,
        input: Vec<Value>,
        scope: &str,
    ) -> ResponseStream {
        let decoder = responses::Decoder::codex("gpt-5".into());
        let guard = session.clone().lock_owned().await;
        ws_stream(connection, guard, decoder, settings, input, scope.into())
    }

    async fn ws(socket: Socket, session: &Arc<Mutex<Session>>) -> ResponseStream {
        let settings = Map::from_iter([("model".into(), json!("gpt-5"))]);
        ws_with(connection(socket), session, settings, vec![], "scope").await
    }

    /// Streams frames as HTTP SSE that never reaches EOF; the terminal event must
    /// end the response and release the session while the stream is still alive.
    async fn http(frames: Vec<Value>, session: &Arc<Mutex<Session>>) -> Chunks {
        let events = stream::iter(frames.into_iter().map(|frame| {
            Ok(transport::SseEvent {
                event: None,
                data: frame.to_string(),
            })
        }));
        let decoder = responses::Decoder::codex("gpt-5".into());
        let guard = session.clone().lock_owned().await;
        let mut http = http_stream(
            Box::pin(events.chain(stream::pending())),
            decoder,
            guard,
            "scope".into(),
        );
        assert!(session.try_lock().is_err());
        let chunks = tokio::time::timeout(Duration::from_secs(1), http.by_ref().collect())
            .await
            .expect("terminal response must not wait for HTTP EOF");
        assert!(session.try_lock().is_ok());
        chunks
    }

    fn assembled(chunks: Chunks) -> ResponseAssembler {
        let mut assembler = ResponseAssembler::default();
        for chunk in chunks {
            assembler.push(&chunk.unwrap()).unwrap();
        }
        assembler
    }

    fn continuation(session: &Session) -> &Continuation {
        match session {
            Session::Reusable(reusable) => reusable.continuation.as_ref().unwrap(),
            _ => panic!("successful response must restore reusable connection"),
        }
    }

    fn message_item(text: &str) -> Value {
        json!({"type":"message", "id":"msg_1", "status":"completed", "role":"assistant", "content":[{"type":"output_text","text":text,"annotations":[]}]})
    }

    #[tokio::test]
    async fn observed_metadata_and_empty_terminal_output_work_on_both_transports() {
        let events = vec![
            json!({"type":"codex.rate_limits","rate_limits":{}}),
            json!({"type":"codex.response.metadata","metadata":{}}),
            json!({"type":"response.output_item.added","output_index":0,"item":message_item("")}),
            json!({"type":"response.output_text.delta","output_index":0,"item_id":"msg_1","content_index":0,"delta":"OK"}),
            json!({"type":"response.output_item.done","output_index":0,"item":message_item("OK")}),
            json!({"type":"response.output_item.added","output_index":1,"item":{"type":"reasoning","id":"rs_1","summary":[]}}),
            json!({"type":"response.reasoning_summary_text.delta","output_index":1,"item_id":"rs_1","summary_index":0,"delta":"summary"}),
            json!({"type":"response.reasoning_summary_text.done","output_index":1,"item_id":"rs_1","summary_index":0,"text":"summary"}),
            json!({"type":"response.output_item.done","output_index":1,"item":{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"summary"}],"encrypted_content":"ciphertext"}}),
            json!({"type":"responsesapi.websocket_timing","timing":{}}),
            json!({"type":"response.completed","response":{"id":"resp_1","status":"completed","output":[],"usage":{"input_tokens":5,"output_tokens":1}}}),
        ];
        let (socket, server) = mock_socket(events.clone()).await;
        let session = session();
        let chunks: Chunks = ws(socket, &session).await.collect().await;
        assert!(chunks.iter().all(Result::is_ok), "{chunks:?}");
        let any = |test: &dyn Fn(&ResponseChunk) -> bool| chunks.iter().flatten().any(test);
        assert!(any(
            &|c| matches!(c, ResponseChunk::BlockEnded { content: BlockContent::Text { text }, .. } if text == "OK")
        ));
        assert!(any(
            &|c| matches!(c, ResponseChunk::BlockEnded { content: BlockContent::Reasoning { text }, .. } if text == "summary")
        ));
        assert!(any(&|c| matches!(
            c,
            ResponseChunk::ResponseEnded {
                stop_reason: StopReason::EndTurn
            }
        )));
        assert!(any(
            &|c| matches!(c, ResponseChunk::ItemEnded { replay: Some(replay), .. }
            if replay.payload["encrypted_content"] == "ciphertext")
        ));
        assert_eq!(
            continuation(&*session.lock().await).input.len(),
            2,
            "continuation must include streamed output despite empty terminal output"
        );
        server.await.unwrap();
        assert_eq!(chunks, http(events, &self::session()).await);
    }

    #[tokio::test]
    async fn interrupted_or_dropped_websocket_does_not_replay_or_preserve_continuation() {
        let (socket, server) = mock_socket(vec![
            json!({"type":"response.output_item.added","output_index":0,"item":message_item("")}),
            json!({"type":"response.output_text.delta","output_index":0,"item_id":"msg_1","content_index":0,"delta":"visible"}),
        ]).await;
        let session = session();
        let events: Chunks = ws(socket, &session).await.collect().await;
        assert!(events.iter().flatten().any(|event| matches!(event,
            ResponseChunk::BlockDelta { delta: ContentDelta::Text(text), .. } if text == "visible")));
        assert!(events.last().unwrap().is_err());
        assert!(matches!(*session.lock().await, Session::Disconnected));
        server.await.unwrap();

        let (socket, server) = mock_socket(vec![]).await;
        let session = self::session();
        drop(ws(socket, &session).await);
        assert!(matches!(*session.lock().await, Session::Disconnected));
        server.await.unwrap();
    }

    pub(in super::super) fn reasoning_tool_output() -> Vec<Value> {
        vec![
            json!({"type":"reasoning", "id":"rs_private", "summary":[],
                "encrypted_content":"opaque+/=", "future_native":{"state":"keep"}}),
            json!({"type":"function_call", "id":"fc_1", "call_id":"call_1",
                "name":"inspect", "arguments":"{\"path\":\"test\"}", "status":"completed"}),
        ]
    }

    pub(in super::super) fn reasoning_tool_request(scope: &str) -> ModelRequest {
        use crate::provider::protocol::{AssistantItem, ToolCall, ToolResult};
        let mut replay = super::super::super::common::reasoning_envelope(
            "responses",
            "gpt-5",
            reasoning_tool_output()[0].clone(),
        );
        replay.scope = scope.into();
        let mut reasoning = AssistantItem::reasoning("rs_private", 0, "", Some(replay));
        reasoning.blocks.clear();
        ModelRequest {
            history: vec![
                ProtocolMessage::Assistant(vec![
                    reasoning,
                    AssistantItem::tool_call(
                        "fc_1",
                        1,
                        ToolCall::new("call_1", "inspect", json!({"path":"test"})).unwrap(),
                    ),
                ]),
                ProtocolMessage::Tool(vec![ToolResult {
                    call_id: "call_1".into(),
                    name: "inspect".into(),
                    result: json!({"ok":true}),
                    images: vec![],
                    is_error: false,
                }]),
            ],
            max_output_tokens: None,
            ..super::super::super::common::tests::request("gpt-5")
        }
    }

    #[tokio::test]
    async fn native_reasoning_continuation_and_full_replay_survive_save_resume() {
        use super::super::super::common::tests::resume_request;
        let scope = reasoning_scope("codex", "https://codex.example/responses");
        let output = reasoning_tool_output();
        let reasoning = json!({"type":"reasoning", "id":"rs_private", "summary":[]});
        let (socket, server) = mock_socket(vec![
            json!({"type":"response.output_item.added", "output_index":0, "item":reasoning}),
            json!({"type":"response.output_item.done", "output_index":0, "item":reasoning}),
            json!({"type":"response.completed", "response":{"id":"resp_native", "status":"completed", "output":output}}),
        ]).await;
        let mut request = reasoning_tool_request(&scope);
        let mut initial = request.clone();
        initial.history.clear();
        let prepared = websocket_request(responses::encode(&initial).unwrap(), None);
        let session = session();
        let chunks = ws_with(
            connection(socket),
            &session,
            prepared.settings,
            prepared.full_input,
            &scope,
        )
        .await
        .collect()
        .await;
        request.history[0] = ProtocolMessage::Assistant(assembled(chunks).finish().unwrap().0);
        let original = resume_request(&request).await;
        let mut matching = original.clone();
        filter_reasoning_scope(&mut matching, &scope);
        let full = responses::encode(&matching).unwrap();
        let session = session.lock().await;
        let continuation = continuation(&session);
        assert_eq!(continuation.input, full.input[..2]);
        assert_eq!(continuation.input[0], output[0]);
        let wire = |body: &responses::EncodedRequest, previous| {
            websocket_request(body.clone(), previous)
                .wire_request
                .into_wire()
        };
        let resumed = wire(&full, Some(continuation));
        assert_eq!(resumed["previous_response_id"], "resp_native");
        assert_eq!(resumed["input"], json!([full.input[2].clone()]));
        assert_eq!(resumed["input"][0]["call_id"], "call_1");

        // Reconnects, setting changes, fallback compaction and foreign scopes must
        // send complete compatible native state, never a continuation suffix.
        let mut changed_settings = full.clone();
        changed_settings
            .settings
            .insert("text".into(), json!({"format":{"type":"json_object"}}));
        let mut compacted = full.clone();
        compacted.input.insert(
            0,
            json!({"role":"user", "content":[{"type":"input_text", "text":"compacted history"}]}),
        );
        let mut foreign = original.clone();
        filter_reasoning_scope(
            &mut foreign,
            &reasoning_scope("other", "https://codex.example/responses"),
        );
        let foreign = responses::encode(&foreign).unwrap();
        assert_eq!(foreign.input.len(), 2);
        for (body, previous) in [
            (&full, None),
            (&changed_settings, Some(continuation)),
            (&compacted, Some(continuation)),
            (&foreign, Some(continuation)),
        ] {
            let wire = wire(body, previous);
            assert!(wire.get("previous_response_id").is_none());
            assert_eq!(wire["input"], json!(body.input));
        }
        assert_eq!(compacted.input[1], output[0]);
        assert_eq!(responses::encode(&original).unwrap().input[0], output[0]);
        drop(session);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn truncated_tool_preserves_native_reasoning_and_usage_on_both_transports() {
        let native = reasoning_tool_output()[0].clone();
        let mut call = reasoning_tool_output()[1].clone();
        call["arguments"] = json!("{\"path\":");
        let frames = vec![
            json!({"type":"response.output_item.added", "output_index":0, "item":native}),
            json!({"type":"response.output_item.done", "output_index":0, "item":native}),
            json!({"type":"response.output_item.added", "output_index":1, "item":call}),
            json!({"type":"response.output_item.done", "output_index":1, "item":call}),
            json!({"type":"response.incomplete", "response":{"id":"truncated", "status":"incomplete",
                "incomplete_details":{"reason":"max_output_tokens"}, "output":[],
                "usage":{"input_tokens":8,"output_tokens":13}}}),
        ];
        let (socket, server) = mock_socket(frames.clone()).await;
        let session = session();
        let ws_chunks = ws(socket, &session).await.collect().await;
        assert!(
            matches!(&*session.lock().await, Session::Reusable(reusable) if reusable.continuation.is_none()),
            "a valid abnormal stop can retain its socket, not a continuation"
        );
        server.await.unwrap();
        for chunks in [ws_chunks, http(frames, &session).await] {
            let (items, usage, reason) = assembled(chunks).finish().unwrap();
            assert_eq!(
                (reason, usage.output_tokens, items.len()),
                (StopReason::MaxTokens, 13, 1)
            );
            let replay = items[0].replay.as_ref().unwrap();
            assert_eq!((replay.scope.as_str(), &replay.payload), ("scope", &native));
        }
    }

    #[tokio::test]
    async fn read_timeout_is_eligible_and_retires_connection_state() {
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let (socket, server) = serve_socket(|socket| async move {
            stop_rx.await.unwrap();
            drop(socket);
        })
        .await;
        let session = session();
        let mut response = ws_with(connection(socket), &session, Map::new(), vec![], "scope").await;
        tokio::time::pause();
        let error = response.next().await.unwrap().unwrap_err();
        assert_eq!(
            error.kind,
            ProviderErrorKind::CodexWebSocket(CodexWebSocketError::ReadTimeout)
        );
        assert_eq!(error.recovery(), Some(ProviderRecovery::ResetContext));
        drop(response);
        assert!(matches!(*session.lock().await, Session::Disconnected));
        stop_tx.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn native_events_use_sanitized_provider_neutral_error_categories() {
        let text = |value: Value| Message::Text(value.to_string().into());
        for (frame, kind) in [
            (
                Message::Text("not JSON SECRET".into()),
                ProviderErrorKind::Protocol,
            ),
            (
                Message::Binary(b"SECRET".to_vec().into()),
                ProviderErrorKind::Protocol,
            ),
            (
                text(
                    json!({"type":"error","error":{"code":"invalid_api_key","message":"Bearer SECRET"}}),
                ),
                ProviderErrorKind::Authentication,
            ),
            (
                text(
                    json!({"type":"response.failed","response":{"error":{"code":"request_cancelled","message":"SECRET"}}}),
                ),
                ProviderErrorKind::Response,
            ),
            (
                text(json!({"type":"response.aborted","message":"SECRET"})),
                ProviderErrorKind::Protocol,
            ),
            (
                Message::Close(Some(tungstenite::protocol::CloseFrame {
                    code: CloseCode::from(4001),
                    reason: "Bearer SECRET".into(),
                })),
                ProviderErrorKind::Response,
            ),
        ] {
            let (socket, server) = serve_socket(|mut socket| async move {
                socket.send(frame).await.unwrap();
            })
            .await;
            let session = session();
            let mut connection = connection(socket);
            connection.affinity = Some(HeaderValue::from_static("SECRET"));
            let chunks: Chunks = ws_with(connection, &session, Map::new(), vec![], "scope")
                .await
                .collect()
                .await;
            let error = chunks.last().unwrap().as_ref().unwrap_err();
            assert_eq!(error.kind, kind);
            assert_eq!(
                error.recovery().is_some(),
                kind == ProviderErrorKind::Response
            );
            assert!(!format!("{error:?} {error}").contains("SECRET"));
            assert!(matches!(*session.lock().await, Session::Disconnected));
            server.await.unwrap();
        }
    }
}
