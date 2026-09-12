//! WebSocket and HTTP response streams share the Responses codec and continuation assembly.
use super::super::{common::bind_reasoning_scope, responses, transport};
use super::{
    error,
    recovery::{close_error, socket_error, websocket_error},
    session::{Continuation, Session},
};
use crate::provider::{
    CodexWebSocketError, ProviderError, ProviderErrorKind, ResponseStream,
    protocol::{Message as ProtocolMessage, ModelRequest, ResponseAssembler, ResponseChunk},
};
use futures_util::{SinkExt, StreamExt, stream};
use reqwest::header::HeaderValue;
use serde_json::Value;
use std::{collections::VecDeque, time::Duration};
use tokio::{net::TcpStream, sync::OwnedMutexGuard, time::Instant};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};

pub(super) const DEADLINE: Duration = Duration::from_secs(120);
pub(super) type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct WsState {
    socket: Option<Socket>,
    session: OwnedMutexGuard<Session>,
    decoder: responses::Decoder,
    pending: VecDeque<Result<ResponseChunk, ProviderError>>,
    done: bool,
    failed: bool,
    settings: Value,
    input: Vec<Value>,
    completed: Option<Continuation>,
    affinity: Option<HeaderValue>,
    assembler: ResponseAssembler,
    scope: String,
}
pub(super) fn ws_stream(
    socket: Socket,
    mut session: OwnedMutexGuard<Session>,
    decoder: responses::Decoder,
    settings: Value,
    input: Vec<Value>,
    scope: String,
) -> ResponseStream {
    let affinity = session.affinity.take();
    session.reusable_since = None;
    session.continuation = None;
    Box::pin(stream::unfold(
        WsState {
            socket: Some(socket),
            session,
            decoder,
            pending: VecDeque::new(),
            done: false,
            failed: false,
            settings,
            input,
            completed: None,
            affinity,
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
                        state.session.socket = state.socket.take();
                        state.session.reusable_since = Some(Instant::now());
                        state.session.continuation = state.completed.take();
                        state.session.affinity = state.affinity.take();
                    }
                    return None;
                }
                let frame =
                    tokio::time::timeout(DEADLINE, state.socket.as_mut().unwrap().next()).await;
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
                                        messages: vec![ProtocolMessage::Assistant(items)],
                                        tools: vec![],
                                        response_schema: None,
                                        reasoning: None,
                                        max_output_tokens: None,
                                        correlation: None,
                                    };
                                    match responses::encode(&replay) {
                                        Ok(encoded) => {
                                            completed.input = state.input.clone();
                                            if let Some(output) = encoded["input"].as_array() {
                                                completed.input.extend(output.iter().cloned());
                                            }
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
                            state.socket.as_mut().unwrap().send(Message::Pong(payload)),
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
    use serde_json::json;
    use std::sync::Arc;
    use tokio::sync::Mutex;
    use tokio_tungstenite::tungstenite::{self, protocol::frame::coding::CloseCode};

    pub(in super::super) async fn mock_socket(
        events: Vec<Value>,
    ) -> (Socket, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            for event in events {
                socket
                    .send(Message::Text(event.to_string().into()))
                    .await
                    .unwrap();
            }
            socket.close(None).await.unwrap();
        });
        let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}"))
            .await
            .unwrap();
        (socket, server)
    }

    fn message_item(text: &str) -> Value {
        json!({"type":"message", "id":"msg_1", "status":"completed", "role":"assistant", "content":[{"type":"output_text","text":text,"annotations":[]}]})
    }

    fn observed_codex_events() -> Vec<Value> {
        let item = message_item("OK");
        vec![
            json!({"type":"codex.rate_limits","rate_limits":{}}),
            json!({"type":"codex.response.metadata","metadata":{}}),
            json!({"type":"response.output_item.added","output_index":0,"item":message_item("")}),
            json!({"type":"response.output_text.delta","output_index":0,"item_id":"msg_1","content_index":0,"delta":"OK"}),
            json!({"type":"response.output_item.done","output_index":0,"item":item}),
            json!({"type":"response.output_item.added","output_index":1,"item":{"type":"reasoning","id":"rs_1","summary":[]}}),
            json!({"type":"response.reasoning_summary_text.delta","output_index":1,"item_id":"rs_1","summary_index":0,"delta":"summary"}),
            json!({"type":"response.reasoning_summary_text.done","output_index":1,"item_id":"rs_1","summary_index":0,"text":"summary"}),
            json!({"type":"response.output_item.done","output_index":1,"item":{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"summary"}],"encrypted_content":"ciphertext"}}),
            json!({"type":"responsesapi.websocket_timing","timing":{}}),
            json!({"type":"response.completed","response":{"id":"resp_1","status":"completed","output":[],"usage":{"input_tokens":5,"output_tokens":1}}}),
        ]
    }

    #[tokio::test]
    async fn observed_metadata_and_empty_terminal_output_work_on_both_transports() {
        let events = observed_codex_events();
        let (socket, server) = mock_socket(events.clone()).await;
        let session = Arc::new(Mutex::new(Session::default()));
        let chunks = ws_stream(
            socket,
            session.clone().lock_owned().await,
            responses::Decoder::codex("gpt-5.6-terra".into()),
            json!({"model":"gpt-5.6-terra"}),
            vec![],
            "scope".into(),
        )
        .collect::<Vec<_>>()
        .await;
        assert!(chunks.iter().all(Result::is_ok), "{chunks:?}");
        assert!(chunks.iter().any(|c|matches!(c,Ok(ResponseChunk::BlockEnded{content:crate::provider::protocol::BlockContent::Text{text},..}) if text=="OK")));
        assert!(chunks.iter().any(|c| matches!(
            c,
            Ok(ResponseChunk::ResponseEnded {
                stop_reason: crate::provider::protocol::StopReason::EndTurn
            })
        )));
        assert!(chunks.iter().any(|chunk| matches!(chunk,
            Ok(ResponseChunk::BlockEnded {
                content: crate::provider::protocol::BlockContent::Reasoning { text }, ..
            }) if text == "summary")));
        assert!(chunks.iter().any(|chunk| matches!(chunk,
            Ok(ResponseChunk::ItemEnded { replay: Some(replay), .. })
                if replay.payload["encrypted_content"] == "ciphertext")));
        let session = session.lock().await;
        assert_eq!(
            session.continuation.as_ref().unwrap().input.len(),
            2,
            "continuation must include streamed output despite empty terminal output"
        );
        drop(session);
        server.await.unwrap();
        let events = Box::pin(
            stream::iter(events.into_iter().map(|event| {
                Ok(transport::SseEvent {
                    event: None,
                    data: event.to_string(),
                })
            }))
            .chain(stream::pending()),
        );
        let session = Arc::new(Mutex::new(Session::default()));
        let mut http = http_stream(
            events,
            responses::Decoder::codex("gpt-5.6-terra".into()),
            session.clone().lock_owned().await,
            "scope".into(),
        );
        assert!(session.try_lock().is_err());
        let http_chunks =
            tokio::time::timeout(Duration::from_secs(1), http.by_ref().collect::<Vec<_>>())
                .await
                .expect("terminal response must not wait for HTTP EOF");
        assert_eq!(chunks, http_chunks);
        assert!(session.try_lock().is_ok());
    }

    #[tokio::test]
    async fn interrupted_websocket_does_not_replay_or_preserve_continuation() {
        let item = message_item("");
        let (socket, server) = mock_socket(vec![
            json!({"type":"response.output_item.added","output_index":0,"item":item}),
            json!({"type":"response.output_text.delta","output_index":0,"item_id":"msg_1","content_index":0,"delta":"visible"}),
        ]).await;
        let session = Arc::new(Mutex::new(Session::default()));
        let events = ws_stream(
            socket,
            session.clone().lock_owned().await,
            responses::Decoder::codex("gpt-5".into()),
            json!({"model":"gpt-5"}),
            vec![],
            "scope".into(),
        )
        .collect::<Vec<_>>()
        .await;
        assert!(events.iter().any(|event| matches!(event, Ok(ResponseChunk::BlockDelta { delta:crate::provider::protocol::ContentDelta::Text(text), .. }) if text == "visible")));
        assert!(events.last().unwrap().is_err());
        let session = session.lock().await;
        assert!(session.socket.is_none());
        assert!(session.continuation.is_none());
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
            messages: vec![
                ProtocolMessage::Assistant(vec![
                    reasoning,
                    AssistantItem::tool_call(
                        "fc_1",
                        1,
                        ToolCall {
                            id: "call_1".into(),
                            name: "inspect".into(),
                            arguments: json!({"path":"test"}),
                        },
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
        let (socket, server) = mock_socket(vec![
            json!({"type":"response.output_item.added", "output_index":0,
                "item":{"type":"reasoning", "id":"rs_private", "summary":[]}}),
            json!({"type":"response.output_item.done", "output_index":0,
                "item":{"type":"reasoning", "id":"rs_private", "summary":[]}}),
            json!({"type":"response.completed", "response":{"id":"resp_native", "status":"completed", "output":output}}),
        ]).await;
        let mut request = reasoning_tool_request(&scope);
        let mut initial = request.clone();
        initial.messages.clear();
        let (_, settings, input) = websocket_request(&responses::encode(&initial).unwrap(), None);
        let session = Arc::new(Mutex::new(Session::default()));
        let chunks = ws_stream(
            socket,
            session.clone().lock_owned().await,
            responses::Decoder::codex(request.model.clone()),
            settings,
            input,
            scope.clone(),
        )
        .collect::<Vec<_>>()
        .await;
        let mut assembler = ResponseAssembler::default();
        for chunk in chunks {
            assembler.push(&chunk.unwrap()).unwrap();
        }
        request.messages[0] = ProtocolMessage::Assistant(assembler.finish().unwrap().0);
        let original = resume_request(&request).await;
        let mut matching = original.clone();
        filter_reasoning_scope(&mut matching, &scope);
        let full = responses::encode(&matching).unwrap();
        let session = session.lock().await;
        let continuation = session.continuation.as_ref().unwrap();
        assert_eq!(continuation.input, full["input"].as_array().unwrap()[..2]);
        assert_eq!(continuation.input[0], output[0]);
        let (wire, _, _) = websocket_request(&full, Some(continuation));
        assert_eq!(wire["previous_response_id"], "resp_native");
        assert_eq!(wire["input"], json!([full["input"][2].clone()]));
        assert_eq!(wire["input"][0]["call_id"], "call_1");

        // Reconnects and setting changes (including fallback compaction) must
        // send complete compatible native state, never a continuation suffix.
        let (reconnected, _, _) = websocket_request(&full, None);
        assert!(reconnected.get("previous_response_id").is_none());
        assert_eq!(reconnected["input"], full["input"]);
        let mut changed_settings = full.clone();
        changed_settings["text"] = json!({"format":{"type":"json_object"}});
        let (fallback, _, _) = websocket_request(&changed_settings, Some(continuation));
        assert!(fallback.get("previous_response_id").is_none());
        assert_eq!(fallback["input"][0], output[0]);
        let mut compacted = full.clone();
        compacted["input"].as_array_mut().unwrap().insert(
            0,
            json!({"role":"user", "content":[{"type":"input_text", "text":"compacted history"}]}),
        );
        let (fallback, _, _) = websocket_request(&compacted, Some(continuation));
        assert!(fallback.get("previous_response_id").is_none());
        assert_eq!(fallback["input"][1], output[0]);

        let mut foreign = original.clone();
        filter_reasoning_scope(
            &mut foreign,
            &reasoning_scope("other", "https://codex.example/responses"),
        );
        let foreign_body = responses::encode(&foreign).unwrap();
        let (foreign_wire, _, _) = websocket_request(&foreign_body, Some(continuation));
        assert!(foreign_wire.get("previous_response_id").is_none());
        assert_eq!(foreign_wire["input"].as_array().unwrap().len(), 2);
        assert_eq!(responses::encode(&original).unwrap()["input"][0], output[0]);
        drop(session);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn truncated_tool_preserves_native_reasoning_and_usage_on_both_transports() {
        use crate::provider::protocol::StopReason;
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
        let session = Arc::new(Mutex::new(Session::default()));
        let ws = ws_stream(
            socket,
            session.clone().lock_owned().await,
            responses::Decoder::codex("gpt-5".into()),
            json!({"model":"gpt-5"}),
            vec![],
            "scope".into(),
        )
        .collect::<Vec<_>>()
        .await;
        assert!(session.lock().await.continuation.is_none());
        server.await.unwrap();
        let events = Box::pin(stream::iter(frames.into_iter().map(|frame| {
            Ok(transport::SseEvent {
                event: None,
                data: frame.to_string(),
            })
        })));
        let http = http_stream(
            events,
            responses::Decoder::codex("gpt-5".into()),
            session.clone().lock_owned().await,
            "scope".into(),
        )
        .collect::<Vec<_>>()
        .await;
        for chunks in [ws, http] {
            let mut assembler = ResponseAssembler::default();
            for chunk in chunks {
                assembler.push(&chunk.unwrap()).unwrap();
            }
            let (items, usage, reason) = assembler.finish().unwrap();
            assert_eq!(reason, StopReason::MaxTokens);
            assert_eq!(usage.output_tokens, 13);
            assert_eq!(items.len(), 1);
            let replay = items[0].replay.as_ref().unwrap();
            assert_eq!(replay.scope, "scope");
            assert_eq!(replay.payload, native);
        }
    }

    #[tokio::test]
    async fn dropping_response_clears_socket_and_continuation() {
        let (socket, server) = mock_socket(vec![]).await;
        let session = Arc::new(Mutex::new(Session {
            affinity: Some(HeaderValue::from_static("cancelled-affinity")),
            continuation: Some(Continuation {
                id: "cancelled".into(),
                input: vec![],
                settings: json!({}),
            }),
            reusable_since: Some(Instant::now()),
            ..Session::default()
        }));
        let events = ws_stream(
            socket,
            session.clone().lock_owned().await,
            responses::Decoder::codex("gpt-5".into()),
            json!({"model":"gpt-5"}),
            vec![],
            "scope".into(),
        );
        drop(events);
        let session = session.lock().await;
        assert!(session.socket.is_none());
        assert!(session.continuation.is_none());
        assert!(session.affinity.is_none());
        assert!(session.reusable_since.is_none());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn read_timeout_is_eligible_and_retires_connection_state() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let _socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            stop_rx.await.unwrap();
        });
        let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}"))
            .await
            .unwrap();
        let session = Arc::new(Mutex::new(Session {
            affinity: Some(HeaderValue::from_static("secret-affinity")),
            ..Session::default()
        }));
        let mut response = ws_stream(
            socket,
            session.clone().lock_owned().await,
            responses::Decoder::codex("gpt-5".into()),
            json!({}),
            vec![],
            "scope".into(),
        );
        tokio::time::pause();
        let error = response.next().await.unwrap().unwrap_err();
        assert_eq!(
            error.kind,
            ProviderErrorKind::CodexWebSocket(CodexWebSocketError::ReadTimeout)
        );
        assert_eq!(error.recovery(), Some(ProviderRecovery::ResetContext));
        drop(response);
        let session = session.lock().await;
        assert!(session.socket.is_none());
        assert!(session.continuation.is_none());
        assert!(session.affinity.is_none());
        stop_tx.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn native_events_use_sanitized_provider_neutral_error_categories() {
        for (frame, kind) in [
            (Message::Text("not JSON SECRET".into()), ProviderErrorKind::Protocol),
            (Message::Binary(b"SECRET".to_vec().into()), ProviderErrorKind::Protocol),
            (Message::Text(json!({"type":"error","error":{"code":"invalid_api_key","message":"Bearer SECRET"}}).to_string().into()), ProviderErrorKind::Authentication),
            (Message::Text(json!({"type":"response.failed","response":{"error":{"code":"request_cancelled","message":"SECRET"}}}).to_string().into()), ProviderErrorKind::Response),
            (Message::Text(json!({"type":"response.aborted","message":"SECRET"}).to_string().into()), ProviderErrorKind::Protocol),
            (Message::Close(Some(tungstenite::protocol::CloseFrame { code: CloseCode::from(4001), reason: "Bearer SECRET".into() })), ProviderErrorKind::Response),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (tcp, _) = listener.accept().await.unwrap();
                let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
                socket.send(frame).await.unwrap();
            });
            let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}")).await.unwrap();
            let session = Arc::new(Mutex::new(Session {
                affinity: Some(HeaderValue::from_static("SECRET")),
                ..Session::default()
            }));
            let chunks = ws_stream(socket, session.clone().lock_owned().await,
                responses::Decoder::codex("gpt-5".into()), json!({}), vec![], "scope".into())
                .collect::<Vec<_>>().await;
            let error = chunks.last().unwrap().as_ref().unwrap_err();
            assert_eq!(error.kind, kind);
            assert_eq!(error.recovery().is_some(), kind == ProviderErrorKind::Response);
            assert!(!format!("{error:?} {error}").contains("SECRET"));
            let session = session.lock().await;
            assert!(session.socket.is_none());
            assert!(session.continuation.is_none());
            assert!(session.affinity.is_none());
            server.await.unwrap();
        }
    }
}
