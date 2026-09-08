//! Skyhook-owned ChatGPT subscription provider. Authentication never imports the
//! official client's credentials. Both transports share the Responses codec.
//! Connection, routing affinity and continuation belong to one context. Only a
//! failed handshake may fall back: after a write attempt nothing is replayed.
pub mod auth;

use super::{
    common::{bind_reasoning_scope, filter_reasoning_scope, reasoning_scope},
    responses, transport,
};
use crate::provider::{
    Provider, ProviderContext, ProviderError, ProviderErrorKind, ProviderFuture, ResponseStream,
    protocol::{Message as ProtocolMessage, ModelRequest, ResponseAssembler, ResponseChunk},
};
use futures_util::{SinkExt, StreamExt, stream};
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::{Value, json};
use std::{collections::VecDeque, sync::Arc, time::Duration};
use tokio::{
    net::TcpStream,
    sync::{Mutex, OwnedMutexGuard},
    time::Instant,
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, client::IntoClientRequest},
};

const ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/responses";
const WS_ENDPOINT: &str = "wss://chatgpt.com/backend-api/codex/responses";
const DEADLINE: Duration = Duration::from_secs(120);
// Retire before the observed ~five-minute server/proxy idle timeout.
const MAX_IDLE: Duration = Duration::from_secs(240);
const REUSE_PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_PROBE_FRAMES: usize = 32;
type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Clone)]
pub struct CodexProvider {
    auth: auth::AuthManager,
    client: reqwest::Client,
    endpoint: String,
    ws_endpoint: String,
}
impl CodexProvider {
    /// No credentials are read and no login is required until invocation.
    pub fn new() -> Result<Self, ProviderError> {
        Ok(Self {
            auth: auth::AuthManager::new().map_err(auth_error)?,
            client: transport::client()?,
            endpoint: ENDPOINT.into(),
            ws_endpoint: WS_ENDPOINT.into(),
        })
    }
}
pub fn codex_oauth() -> Result<CodexProvider, ProviderError> {
    CodexProvider::new()
}
impl Provider for CodexProvider {
    fn open_context(&self, correlation: String) -> Result<Box<dyn ProviderContext>, ProviderError> {
        Ok(Box::new(Context {
            provider: self.clone(),
            correlation,
            session: Arc::new(Mutex::new(Session::default())),
        }))
    }
}
#[derive(Default)]
struct Session {
    socket: Option<Socket>,
    reusable_since: Option<Instant>,
    continuation: Option<Continuation>,
    affinity: Option<HeaderValue>,
    http_only: bool,
}
/// A socket retained between turns is unpolled and may have a queued close,
/// ping, or EOF. Probe it before a model request, never by replaying a request.
/// Take all connection-bound state first: cancellation cannot leave a partially
/// probed socket or its continuation available to a subsequent invocation.
async fn prepare_reuse(session: &mut Session) {
    let Some(mut socket) = session.socket.take() else {
        session.reusable_since = None;
        session.continuation = None;
        return;
    };
    let since = session.reusable_since.take();
    let continuation = session.continuation.take();
    let affinity = session.affinity.take();
    if since.is_none_or(|since| since.elapsed() >= MAX_IDLE) {
        return;
    }
    if matches!(
        tokio::time::timeout(REUSE_PROBE_TIMEOUT, probe_socket(&mut socket)).await,
        Ok(true)
    ) {
        session.socket = Some(socket);
        session.reusable_since = Some(Instant::now());
        session.continuation = continuation;
        session.affinity = affinity;
    }
    // A failed probe retires the connection. The caller establishes a fresh
    // socket (or safely falls back to HTTP), sending full history exactly once.
}

async fn probe_socket(socket: &mut Socket) -> bool {
    let mut nonce = [0u8; 16];
    if getrandom::fill(&mut nonce).is_err()
        || socket
            .send(Message::Ping(nonce.to_vec().into()))
            .await
            .is_err()
    {
        return false;
    }
    for _ in 0..MAX_PROBE_FRAMES {
        match socket.next().await {
            Some(Ok(Message::Pong(payload))) if payload.as_ref() == nonce => return true,
            Some(Ok(Message::Pong(_))) => {}
            Some(Ok(Message::Ping(payload))) => {
                if socket.send(Message::Pong(payload)).await.is_err() {
                    return false;
                }
            }
            Some(Ok(Message::Text(text))) => {
                // Only known out-of-band metadata may trail a completed turn.
                // Unexpected content means socket synchronization is uncertain.
                if !serde_json::from_str::<Value>(&text)
                    .ok()
                    .as_ref()
                    .is_some_and(is_transport_metadata)
                {
                    return false;
                }
            }
            _ => return false,
        }
    }
    false
}

struct Continuation {
    id: String,
    input: Vec<Value>,
    settings: Value,
}
struct Context {
    provider: CodexProvider,
    correlation: String,
    session: Arc<Mutex<Session>>,
}
impl ProviderContext for Context {
    fn invoke(&mut self, mut request: ModelRequest) -> ProviderFuture {
        let provider = self.provider.clone();
        let correlation = self.correlation.clone();
        let session = self.session.clone();
        Box::pin(async move {
            let scope = reasoning_scope("codex", &provider.endpoint);
            filter_reasoning_scope(&mut request, &scope);
            let mut body = responses::encode(&request)?;
            // Subscription wire constraints; no second request/response codec.
            body["store"] = json!(false);
            if body.get("instructions").is_none() {
                body["instructions"] = json!("");
            }
            body["include"] = json!(["reasoning.encrypted_content"]);
            // The subscription service does not accept an output-token limit;
            // the profile value remains a local context budget only.
            body.as_object_mut()
                .expect("Responses encoder returns object")
                .remove("max_output_tokens");
            if request
                .correlation
                .as_ref()
                .is_some_and(|value| value != &correlation)
            {
                return Err(error(
                    ProviderErrorKind::InvalidRequest,
                    "request correlation does not match its Codex context",
                ));
            }
            let credentials = provider.auth.credentials().await.map_err(auth_error)?;
            let mut headers = auth_headers(
                &credentials.access_token,
                &credentials.account_id,
                &correlation,
            )?;
            let mut session = session.lock_owned().await;
            prepare_reuse(&mut session).await;
            if let Some(affinity) = &session.affinity {
                headers.insert("x-codex-turn-state", affinity.clone());
            }
            if !session.http_only && session.socket.is_none() {
                match connect(&provider.ws_endpoint, &headers).await {
                    Ok((socket, affinity)) => {
                        session.socket = Some(socket);
                        session.affinity = affinity;
                    }
                    // No response.create has been sent, so full-history fallback is safe.
                    Err(_) => {
                        session.http_only = true;
                        session.continuation = None;
                    }
                }
            }
            if let Some(mut socket) = session.socket.take() {
                let (wire, settings, input) =
                    websocket_request(&body, session.continuation.take().as_ref());
                // Removing continuation before writing invalidates it on cancellation.
                tokio::time::timeout(
                    DEADLINE,
                    socket.send(Message::Text(wire.to_string().into())),
                )
                .await
                .map_err(|_| {
                    error(
                        ProviderErrorKind::Timeout,
                        "Codex WebSocket write timed out; request was not replayed",
                    )
                })?
                .map_err(|_| {
                    error(
                        ProviderErrorKind::Transport,
                        "Codex WebSocket write failed; request was not replayed",
                    )
                })?;
                return Ok(ws_stream(
                    socket,
                    session,
                    responses::Decoder::codex(request.model),
                    settings,
                    input,
                    scope,
                ));
            }
            session.continuation = None;
            let events =
                transport::post_sse_once(&provider.client, &provider.endpoint, headers, &body)
                    .await?;
            Ok(http_stream(
                events,
                responses::Decoder::codex(request.model),
                session,
                scope,
            ))
        })
    }
}
fn error(kind: ProviderErrorKind, message: &str) -> ProviderError {
    ProviderError {
        kind,
        message: message.into(),
    }
}
fn auth_error(error: auth::AuthError) -> ProviderError {
    ProviderError {
        kind: ProviderErrorKind::Authentication,
        message: error.to_string(),
    }
}
fn auth_headers(token: &str, account: &str, correlation: &str) -> Result<HeaderMap, ProviderError> {
    let mut headers = HeaderMap::new();
    let mut bearer = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| {
        error(
            ProviderErrorKind::Authentication,
            "invalid Codex access token",
        )
    })?;
    bearer.set_sensitive(true);
    headers.insert("authorization", bearer);
    let mut account = HeaderValue::from_str(account).map_err(|_| {
        error(
            ProviderErrorKind::Authentication,
            "invalid Codex account identifier",
        )
    })?;
    account.set_sensitive(true);
    headers.insert("chatgpt-account-id", account);
    headers.insert("originator", HeaderValue::from_static("skyhook"));
    headers.insert(
        "openai-beta",
        HeaderValue::from_static("responses_websockets=2026-02-06"),
    );
    headers.insert(
        "session_id",
        HeaderValue::from_str(correlation).map_err(|_| {
            error(
                ProviderErrorKind::InvalidRequest,
                "invalid context correlation header",
            )
        })?,
    );
    Ok(headers)
}
async fn connect(
    url: &str,
    headers: &HeaderMap,
) -> Result<(Socket, Option<HeaderValue>), ProviderError> {
    let mut request = url.into_client_request().map_err(|_| {
        error(
            ProviderErrorKind::InvalidRequest,
            "invalid Codex WebSocket endpoint",
        )
    })?;
    request.headers_mut().extend(headers.clone());
    let (socket, response) = tokio::time::timeout(
        Duration::from_secs(20),
        tokio_tungstenite::connect_async(request),
    )
    .await
    .map_err(|_| {
        error(
            ProviderErrorKind::Timeout,
            "Codex WebSocket handshake timed out",
        )
    })?
    .map_err(|_| {
        error(
            ProviderErrorKind::Transport,
            "Codex WebSocket handshake failed",
        )
    })?;
    Ok((
        socket,
        response.headers().get("x-codex-turn-state").cloned(),
    ))
}
fn websocket_request(body: &Value, previous: Option<&Continuation>) -> (Value, Value, Vec<Value>) {
    let mut wire = body.clone();
    let input = body
        .get("input")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut settings = body.clone();
    settings
        .as_object_mut()
        .expect("Responses encoder returns object")
        .remove("input");
    if let Some(previous) = previous
        .filter(|previous| previous.settings == settings && input.starts_with(&previous.input))
    {
        wire["previous_response_id"] = json!(previous.id);
        wire["input"] = json!(&input[previous.input.len()..]);
    }
    wire["type"] = json!("response.create");
    (wire, settings, input)
}
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
    assembler: ResponseAssembler,
    scope: String,
}
fn ws_stream(
    socket: Socket,
    session: OwnedMutexGuard<Session>,
    decoder: responses::Decoder,
    settings: Value,
    input: Vec<Value>,
    scope: String,
) -> ResponseStream {
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
                    }
                    return None;
                }
                let frame =
                    tokio::time::timeout(DEADLINE, state.socket.as_mut().unwrap().next()).await;
                let decoded = match frame {
                    Ok(Some(Ok(Message::Text(text)))) => match serde_json::from_str::<Value>(&text)
                    {
                        Ok(event) => {
                            #[cfg(test)]
                            if std::env::var_os("SKYHOOK_CODEX_TRACE_SHAPES").is_some() {
                                eprintln!(
                                    "wire shape: type={} index={} item_type={} output_types={:?} status={}",
                                    event["type"].as_str().unwrap_or("?"),
                                    event["output_index"],
                                    event["item"]["type"].as_str().unwrap_or(""),
                                    event["response"]["output"].as_array().map(|items| items
                                        .iter()
                                        .map(|item| item["type"].as_str().unwrap_or("?"))
                                        .collect::<Vec<_>>()),
                                    event["response"]["status"].as_str().unwrap_or("")
                                );
                            }
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
                        if tokio::time::timeout(
                            DEADLINE,
                            state.socket.as_mut().unwrap().send(Message::Pong(payload)),
                        )
                        .await
                        .is_ok_and(|result| result.is_ok())
                        {
                            continue;
                        }
                        Err(error(
                            ProviderErrorKind::Transport,
                            "Codex WebSocket ping failed; request was not replayed",
                        ))
                    }
                    Ok(Some(Ok(Message::Pong(_)))) => continue,
                    Ok(Some(Ok(Message::Binary(_)))) => Err(error(
                        ProviderErrorKind::Protocol,
                        "unexpected Codex binary frame",
                    )),
                    Err(_) => Err(error(
                        ProviderErrorKind::Timeout,
                        "Codex WebSocket read timed out; request was not replayed",
                    )),
                    _ => Err(error(
                        ProviderErrorKind::Transport,
                        "Codex WebSocket ended before completion; request was not replayed",
                    )),
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
fn is_transport_metadata(event: &Value) -> bool {
    matches!(
        event.get("type").and_then(Value::as_str),
        Some("codex.rate_limits" | "codex.response.metadata" | "responsesapi.websocket_timing")
    )
}

fn http_stream(
    events: transport::SseStream,
    decoder: responses::Decoder,
    session: OwnedMutexGuard<Session>,
    scope: String,
) -> ResponseStream {
    Box::pin(stream::unfold(
        (events, decoder, VecDeque::new(), false, session, scope),
        |(mut events, mut decoder, mut pending, mut done, session, scope)| async move {
            loop {
                if let Some(chunk) = pending.pop_front() {
                    return Some((chunk, (events, decoder, pending, done, session, scope)));
                }
                if done {
                    return None;
                }
                let decoded = match events.next().await {
                    Some(Ok(event)) => {
                        if serde_json::from_str::<Value>(&event.data)
                            .ok()
                            .as_ref()
                            .is_some_and(is_transport_metadata)
                        {
                            continue;
                        }
                        let chunks = decoder.decode(&event);
                        done = decoder.completed();
                        chunks
                    }
                    Some(Err(error)) => {
                        done = true;
                        Err(error)
                    }
                    None => {
                        done = true;
                        decoder.finish()
                    }
                };
                match decoded {
                    Ok(mut chunks) => {
                        for chunk in &mut chunks {
                            bind_reasoning_scope(chunk, &scope);
                        }
                        pending.extend(chunks.into_iter().map(Ok));
                    }
                    Err(error) => {
                        done = true;
                        pending.push_back(Err(error));
                    }
                }
            }
        },
    ))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn continuation_requires_exact_history_prefix_and_settings() {
        let body =
            json!({"model":"gpt-5", "input":[{"role":"user","content":"hello"}], "store":false});
        let (_, settings, input) = websocket_request(&body, None);
        let previous = Continuation {
            id: "resp_1".into(),
            input,
            settings,
        };
        let mut next = body.clone();
        next["input"]
            .as_array_mut()
            .unwrap()
            .push(json!({"role":"user","content":"next"}));
        let (wire, _, _) = websocket_request(&next, Some(&previous));
        assert_eq!(wire["previous_response_id"], "resp_1");
        assert_eq!(wire["input"].as_array().unwrap().len(), 1);
        next["model"] = json!("gpt-other");
        assert!(
            websocket_request(&next, Some(&previous))
                .0
                .get("previous_response_id")
                .is_none()
        );
        for (field, value) in [
            ("tools", json!([{"type":"function","name":"changed"}])),
            ("instructions", json!("summary request")),
            (
                "text",
                json!({"format":{"type":"json_schema","schema":{"type":"object"}}}),
            ),
            ("reasoning", json!({"effort":"high"})),
        ] {
            let mut changed = body.clone();
            changed[field] = value;
            assert!(
                websocket_request(&changed, Some(&previous))
                    .0
                    .get("previous_response_id")
                    .is_none()
            );
        }
        let mut compacted = body.clone();
        compacted["input"] = json!([]);
        assert!(
            websocket_request(&compacted, Some(&previous))
                .0
                .get("previous_response_id")
                .is_none()
        );
        next = body;
        next["input"][0]["content"] = json!("edited");
        assert!(
            websocket_request(&next, Some(&previous))
                .0
                .get("previous_response_id")
                .is_none()
        );
    }
    #[test]
    fn credentials_are_sensitive_headers() {
        let headers = auth_headers("secret", "account", "session").unwrap();
        assert!(headers["authorization"].is_sensitive());
        assert!(headers["chatgpt-account-id"].is_sensitive());
        assert!(auth_headers("bad\ntoken", "account", "session").is_err());
    }
    async fn mock_socket(events: Vec<Value>) -> (Socket, tokio::task::JoinHandle<()>) {
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
        let session = session.lock().await;
        assert_eq!(
            session.continuation.as_ref().unwrap().input.len(),
            1,
            "continuation must include streamed output despite empty terminal output"
        );
        drop(session);
        server.await.unwrap();
        let events = Box::pin(stream::iter(events.into_iter().map(|event| {
            Ok(transport::SseEvent {
                event: None,
                data: event.to_string(),
            })
        })));
        let session = Arc::new(Mutex::new(Session::default()));
        let http = http_stream(
            events,
            responses::Decoder::codex("gpt-5.6-terra".into()),
            session.lock_owned().await,
            "scope".into(),
        )
        .collect::<Vec<_>>()
        .await;
        assert_eq!(chunks, http);
    }

    #[tokio::test]
    async fn reasoning_summary_closes_before_item_and_rich_replay_survives_empty_or_absent_terminal()
     {
        use crate::provider::protocol::{BlockContent, ResponseAssembler};
        for omit_output in [false, true] {
            let native = json!({"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"summary"}],"encrypted_content":"ciphertext"});
            let mut terminal = json!({"type":"response.completed","response":{"id":"resp_1","status":"completed","output":[]}});
            if omit_output {
                terminal["response"]
                    .as_object_mut()
                    .unwrap()
                    .remove("output");
            }
            let events = vec![
                json!({"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[]}}),
                json!({"type":"response.reasoning_summary_text.delta","output_index":0,"item_id":"rs_1","summary_index":0,"delta":"summary"}),
                json!({"type":"response.reasoning_summary_text.done","output_index":0,"item_id":"rs_1","summary_index":0,"text":"summary"}),
                json!({"type":"response.reasoning_summary_part.done","output_index":0,"item_id":"rs_1","summary_index":0,"part":{"type":"summary_text","text":"summary"}}),
                json!({"type":"response.output_item.done","output_index":0,"item":native}),
                terminal,
            ];
            let (socket, server) = mock_socket(events.clone()).await;
            let session = Arc::new(Mutex::new(Session::default()));
            let chunks = ws_stream(
                socket,
                session.clone().lock_owned().await,
                responses::Decoder::codex("gpt-5".into()),
                json!({"model":"gpt-5"}),
                vec![],
                "scope".into(),
            )
            .collect::<Vec<_>>()
            .await;
            assert!(chunks.iter().all(Result::is_ok), "{chunks:?}");
            let events = Box::pin(stream::iter(events.into_iter().map(|event| {
                Ok(transport::SseEvent {
                    event: None,
                    data: event.to_string(),
                })
            })));
            let http_session = Arc::new(Mutex::new(Session::default()));
            let http = http_stream(
                events,
                responses::Decoder::codex("gpt-5".into()),
                http_session.lock_owned().await,
                "scope".into(),
            )
            .collect::<Vec<_>>()
            .await;
            assert_eq!(chunks, http);
            let closed = chunks
                .iter()
                .position(|chunk| {
                    matches!(
                        chunk,
                        Ok(ResponseChunk::BlockEnded {
                            content: BlockContent::Reasoning { .. },
                            ..
                        })
                    )
                })
                .unwrap();
            let ended = chunks
                .iter()
                .position(|chunk| {
                    matches!(
                        chunk,
                        Ok(ResponseChunk::ItemEnded {
                            replay: Some(_),
                            ..
                        })
                    )
                })
                .unwrap();
            assert!(closed < ended);
            assert_eq!(
                chunks
                    .iter()
                    .filter(|chunk| matches!(chunk, Ok(ResponseChunk::BlockEnded { .. })))
                    .count(),
                1
            );
            let mut assembler = ResponseAssembler::default();
            for chunk in chunks {
                assembler.push(&chunk.unwrap()).unwrap();
            }
            let items = assembler.finish().unwrap().0;
            assert_eq!(items[0].replay.as_ref().unwrap().payload, native);
            assert_eq!(items[0].replay.as_ref().unwrap().scope, "scope");
            assert_eq!(
                session.lock().await.continuation.as_ref().unwrap().input,
                vec![native]
            );
            server.await.unwrap();
        }
    }

    #[test]
    fn empty_terminal_output_never_hides_unfinished_items_or_weakens_standard_responses() {
        for codex in [false, true] {
            let mut decoder = if codex {
                responses::Decoder::codex("model".into())
            } else {
                responses::Decoder::new("model".into())
            };
            decoder.feed(json!({"type":"response.output_item.added","output_index":0,"item":message_item("")})).unwrap();
            if !codex {
                decoder.feed(json!({"type":"response.output_item.done","output_index":0,"item":message_item("OK")})).unwrap();
            }
            assert!(decoder.feed(json!({"type":"response.completed","response":{"id":"resp_1","status":"completed","output":[]}})).is_err());
        }
        for codex in [false, true] {
            let mut decoder = if codex {
                responses::Decoder::codex("model".into())
            } else {
                responses::Decoder::new("model".into())
            };
            decoder.feed(json!({"type":"response.output_item.added","output_index":0,"item":message_item("")})).unwrap();
            assert!(decoder.feed(json!({"type":"response.completed","response":{"id":"resp_1","status":"completed"}})).is_err());
        }
        assert!(!is_transport_metadata(
            &json!({"type":"response.output_item.done"})
        ));
        assert!(!is_transport_metadata(
            &json!({"type":"codex.unknown_semantic_event"})
        ));
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

    #[tokio::test]
    async fn completion_uses_shared_codec_for_canonical_continuation() {
        let item = message_item("answer");
        let (socket, server) = mock_socket(vec![
            json!({"type":"response.output_item.added","output_index":0,"item":item}),
            json!({"type":"response.completed","response":{"id":"resp_1","status":"completed","output":[item],"usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}}),
        ]).await;
        let session = Arc::new(Mutex::new(Session::default()));
        let other = Arc::new(Mutex::new(Session::default()));
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
        assert!(events.iter().all(Result::is_ok), "{events:?}");
        let session = session.lock().await;
        assert!(session.socket.is_some());
        let continuation = session.continuation.as_ref().unwrap();
        assert_eq!(
            continuation.input,
            vec![json!({"role":"assistant","content":[{"type":"output_text","text":"answer"}]})]
        );
        assert_eq!(continuation.id, "resp_1");
        assert!(other.lock().await.continuation.is_none());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn dropping_response_clears_socket_and_continuation() {
        let (socket, server) = mock_socket(vec![]).await;
        let session = Arc::new(Mutex::new(Session::default()));
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
        server.await.unwrap();
    }
    #[tokio::test]
    async fn rejected_handshake_falls_back_once_with_full_http_history() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for index in 0..2 {
                let (mut tcp, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let mut buffer = [0u8; 4096];
                loop {
                    let n = tcp.read(&mut buffer).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&buffer[..n]);
                    if let Some(pos) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&bytes[..pos]);
                        let len = head
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|value| value.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if bytes.len() >= pos + 4 + len {
                            break;
                        }
                    }
                }
                requests.push(String::from_utf8(bytes).unwrap());
                if index == 0 {
                    tcp.write_all(b"HTTP/1.1 426 Upgrade Required\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
                } else {
                    let body = format!(
                        "data: {}\n\n",
                        json!({"type":"response.completed","response":{"id":"r1","status":"completed","output":[]}})
                    );
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    tcp.write_all(response.as_bytes()).await.unwrap();
                }
            }
            requests
        });
        let directory = tempfile::tempdir().unwrap();
        let provider = CodexProvider {
            auth: auth::test_manager(directory.path().to_owned()),
            client: transport::client().unwrap(),
            endpoint: format!("http://{address}/responses"),
            ws_endpoint: format!("ws://{address}/responses"),
        };
        let mut context = provider.open_context("context".into()).unwrap();
        let request = ModelRequest {
            model: "gpt-5".into(),
            system: vec![],
            messages: vec![],
            tools: vec![],
            response_schema: None,
            reasoning: None,
            max_output_tokens: Some(100),
            correlation: Some("context".into()),
        };
        let events = context
            .invoke(request)
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(events.iter().all(Result::is_ok), "{events:?}");
        let requests = server.await.unwrap();
        assert!(requests[0].starts_with("GET "));
        assert!(requests[1].starts_with("POST "));
        assert!(requests[1].contains("Bearer test-access-token"));
        let body: Value =
            serde_json::from_str(requests[1].split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert!(body.get("previous_response_id").is_none());
        assert!(body.get("max_output_tokens").is_none());
        assert_eq!(body["input"], json!([]));
    }

    fn retained_session(socket: Socket) -> Session {
        Session {
            socket: Some(socket),
            reusable_since: Some(Instant::now()),
            continuation: Some(Continuation {
                id: "stale-response".into(),
                input: vec![],
                settings: json!({}),
            }),
            affinity: Some(HeaderValue::from_static("stale-affinity")),
            http_only: false,
        }
    }

    #[tokio::test]
    async fn idle_close_is_retired_before_next_model_request() {
        let (socket, closed_server) = mock_socket(vec![]).await;
        closed_server.await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let wire = ws.next().await.unwrap().unwrap().into_text().unwrap();
            let wire: Value = serde_json::from_str(&wire).unwrap();
            assert_eq!(wire["type"], "response.create");
            assert!(wire.get("previous_response_id").is_none());
            assert_eq!(wire["input"].as_array().unwrap().len(), 1);
            ws.send(Message::Text(json!({"type":"response.completed","response":{"id":"fresh","status":"completed","output":[]}}).to_string().into())).await.unwrap();
        });
        let directory = tempfile::tempdir().unwrap();
        let session = Arc::new(Mutex::new(retained_session(socket)));
        let mut context = Context {
            provider: CodexProvider {
                auth: auth::test_manager(directory.path().to_owned()),
                client: transport::client().unwrap(),
                endpoint: format!("http://{address}/responses"),
                ws_endpoint: format!("ws://{address}/responses"),
            },
            correlation: "test".into(),
            session: session.clone(),
        };
        let request = ModelRequest {
            model: "fixture".into(),
            system: vec![],
            messages: vec![ProtocolMessage::User(vec![
                crate::provider::protocol::UserContent::Text {
                    text: "next turn".into(),
                },
            ])],
            tools: vec![],
            response_schema: None,
            reasoning: None,
            max_output_tokens: None,
            correlation: Some("test".into()),
        };
        let result = tokio::time::timeout(Duration::from_secs(5), async {
            context
                .invoke(request)
                .await
                .unwrap()
                .collect::<Vec<_>>()
                .await
        })
        .await
        .unwrap();
        assert!(result.iter().all(Result::is_ok), "{result:?}");
        assert!(
            result
                .iter()
                .any(|event| matches!(event, Ok(ResponseChunk::ResponseEnded { .. })))
        );
        assert!(session.lock().await.affinity.is_none());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn expired_socket_is_retired_without_sending_even_a_probe() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            assert!(!matches!(
                ws.next().await,
                Some(Ok(Message::Text(_) | Message::Ping(_)))
            ));
        });
        let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}"))
            .await
            .unwrap();
        let mut session = retained_session(socket);
        session.reusable_since = Some(Instant::now() - MAX_IDLE);
        prepare_reuse(&mut session).await;
        assert!(
            session.socket.is_none()
                && session.continuation.is_none()
                && session.affinity.is_none()
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn healthy_probe_handles_server_ping_and_metadata_and_preserves_continuation() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            ws.send(Message::Text(
                json!({"type":"codex.rate_limits"}).to_string().into(),
            ))
            .await
            .unwrap();
            ws.send(Message::Ping(vec![7].into())).await.unwrap();
            loop {
                match ws.next().await.unwrap().unwrap() {
                    Message::Ping(payload) => {
                        ws.send(Message::Pong(payload)).await.unwrap();
                    }
                    Message::Pong(payload) if payload.as_ref() == [7] => break,
                    other => panic!("unexpected control probe frame: {other:?}"),
                }
            }
        });
        let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}"))
            .await
            .unwrap();
        let mut session = retained_session(socket);
        prepare_reuse(&mut session).await;
        assert!(session.socket.is_some());
        assert_eq!(session.continuation.as_ref().unwrap().id, "stale-response");
        assert!(session.affinity.is_some());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn unresponsive_probe_is_bounded_and_cancellation_discards_state() {
        for cancel in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (tcp, _) = listener.accept().await.unwrap();
                let _ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
                std::future::pending::<()>().await;
            });
            let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}"))
                .await
                .unwrap();
            let mut session = retained_session(socket);
            let timeout = if cancel {
                Duration::from_millis(30)
            } else {
                REUSE_PROBE_TIMEOUT + Duration::from_secs(1)
            };
            let result = tokio::time::timeout(timeout, prepare_reuse(&mut session)).await;
            assert_eq!(result.is_err(), cancel);
            assert!(
                session.socket.is_none()
                    && session.continuation.is_none()
                    && session.affinity.is_none()
            );
            server.abort();
            let _ = server.await;
        }
    }

    #[tokio::test]
    #[ignore = "requires explicit Skyhook Codex login and makes a billable live subscription request"]
    async fn live_codex_smoke() {
        let provider = CodexProvider::new().unwrap();
        let mut context = provider.open_context("skyhook-live-smoke".into()).unwrap();
        let request = ModelRequest {
            model: std::env::var("SKYHOOK_CODEX_SMOKE_MODEL").unwrap_or_else(|_| "gpt-5".into()),
            system: vec![],
            messages: vec![ProtocolMessage::User(vec![
                crate::provider::protocol::UserContent::Text {
                    text: "Reply with OK.".into(),
                },
            ])],
            tools: vec![],
            response_schema: None,
            reasoning: None,
            max_output_tokens: None,
            correlation: Some("skyhook-live-smoke".into()),
        };
        let mut response = context
            .invoke(request)
            .await
            .expect("live Codex invocation failed");
        let mut finished = false;
        while let Some(chunk) = response.next().await {
            finished |= matches!(
                chunk.expect("live Codex stream failed"),
                ResponseChunk::ResponseEnded { .. }
            );
        }
        assert!(finished);
    }
}
