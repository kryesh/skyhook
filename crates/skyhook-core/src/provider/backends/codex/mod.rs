//! Skyhook-owned ChatGPT subscription provider. Authentication never imports the
//! official client's credentials. Both transports share the Responses codec.
//! Connection, routing affinity and continuation belong to one context. Only a
//! failed handshake may fall back: the provider never replays after a write attempt.
//! The runtime may explicitly reset/reinvoke an eligible uncommitted response.
pub mod auth;

#[cfg(test)]
mod recovery_tests;

use super::{
    common::{bind_reasoning_scope, filter_reasoning_scope, reasoning_scope},
    responses, transport,
};
use crate::provider::{
    CodexWebSocketError, Provider, ProviderContext, ProviderError, ProviderErrorKind,
    ProviderFuture, ResponseStream,
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
    tungstenite::{self, Message, client::IntoClientRequest, protocol::frame::coding::CloseCode},
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
    name: String,
    auth: auth::AuthManager,
    client: reqwest::Client,
    endpoint: String,
    ws_endpoint: String,
}
impl CodexProvider {
    /// No credentials are read and no login is required until invocation.
    pub fn new() -> Result<Self, ProviderError> {
        Ok(Self {
            name: "codex".into(),
            auth: auth::AuthManager::new().map_err(auth_error)?,
            client: transport::client()?,
            endpoint: ENDPOINT.into(),
            ws_endpoint: WS_ENDPOINT.into(),
        })
    }

    /// Bind private replay to a configured provider identity, including aliases
    /// using the same Codex endpoint. The default embedding identity is `codex`.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    fn replay_scope(&self) -> String {
        reasoning_scope(&self.name, &self.endpoint)
    }
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
        session.affinity = None;
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
    fn reset(&mut self) {
        // Do not wait on a stream-owned lock. A retired stream can only return
        // its socket to the detached session, never to the new conversation state.
        self.session = Arc::new(Mutex::new(Session::default()));
    }

    fn invoke(&mut self, mut request: ModelRequest) -> ProviderFuture {
        let provider = self.provider.clone();
        let correlation = self.correlation.clone();
        let session = self.session.clone();
        Box::pin(async move {
            let scope = provider.replay_scope();
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
                // Keep affinity out of the shared session while a request is in
                // flight. Failed writes and cancellation drop all routing state.
                session.reusable_since = None;
                let affinity = session.affinity.take();
                let (wire, settings, input) =
                    websocket_request(&body, session.continuation.take().as_ref());
                // Removing continuation before writing invalidates it on cancellation.
                tokio::time::timeout(
                    DEADLINE,
                    socket.send(Message::Text(wire.to_string().into())),
                )
                .await
                .map_err(|_| websocket_error(CodexWebSocketError::WriteTimeout))?
                .map_err(|error| socket_error(error, CodexWebSocketError::Write))?;
                session.affinity = affinity;
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
    affinity: Option<HeaderValue>,
    assembler: ResponseAssembler,
    scope: String,
}
fn ws_stream(
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
fn websocket_error(category: CodexWebSocketError) -> ProviderError {
    let message = match category {
        CodexWebSocketError::EndOfStream => "Codex WebSocket EOF before completion",
        CodexWebSocketError::Closed => "Codex WebSocket closed before completion",
        CodexWebSocketError::Read => "Codex WebSocket read failed",
        CodexWebSocketError::ReadTimeout => "Codex WebSocket read timed out",
        CodexWebSocketError::Ping => "Codex WebSocket ping response failed",
        CodexWebSocketError::Write => "Codex WebSocket write failed",
        CodexWebSocketError::WriteTimeout => "Codex WebSocket write timed out",
    };
    error(ProviderErrorKind::CodexWebSocket(category), message)
}

fn socket_error(native: tungstenite::Error, operation: CodexWebSocketError) -> ProviderError {
    use tungstenite::{Error, error::ProtocolError};
    match native {
        Error::ConnectionClosed | Error::AlreadyClosed => {
            websocket_error(CodexWebSocketError::Closed)
        }
        Error::Protocol(ProtocolError::ResetWithoutClosingHandshake) => {
            websocket_error(CodexWebSocketError::EndOfStream)
        }
        Error::Io(_) | Error::Tls(_) => websocket_error(operation),
        // Do not turn malformed frames, payloads, or local request errors into
        // replay eligibility. In particular, never include native error text.
        _ => ProviderError::protocol("Codex WebSocket protocol failure"),
    }
}

fn close_error(code: Option<CloseCode>) -> ProviderError {
    match code {
        Some(
            CloseCode::Protocol
            | CloseCode::Unsupported
            | CloseCode::Invalid
            | CloseCode::Size
            | CloseCode::Extension,
        ) => ProviderError::protocol("Codex WebSocket rejected protocol or payload"),
        Some(CloseCode::Policy) => error(
            ProviderErrorKind::Authentication,
            "Codex WebSocket policy rejection",
        ),
        None
        | Some(
            CloseCode::Normal
            | CloseCode::Away
            | CloseCode::Status
            | CloseCode::Abnormal
            | CloseCode::Restart
            | CloseCode::Again
            | CloseCode::Error,
        ) => websocket_error(CodexWebSocketError::Closed),
        // Unknown application codes can represent permanent authentication or
        // request rejection. Only allowlisted connection-loss codes are eligible.
        _ => error(
            ProviderErrorKind::Response,
            "Codex WebSocket application rejection",
        ),
    }
}

// Subscription quota notifications are transport metadata, not Responses output.
// Recognize only the documented/observed type; unknown semantic events still fail.
pub(super) fn is_transport_metadata(event: &Value) -> bool {
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
    super::decode_stream(events, super::Decoder::Codex(decoder), scope, session)
}
#[cfg(test)]
mod tests {
    use super::*;
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

    fn reasoning_tool_output() -> Vec<Value> {
        vec![
            json!({"type":"reasoning", "id":"rs_private", "summary":[],
                "encrypted_content":"opaque+/=", "future_native":{"state":"keep"}}),
            json!({"type":"function_call", "id":"fc_1", "call_id":"call_1",
                "name":"inspect", "arguments":"{\"path\":\"test\"}", "status":"completed"}),
        ]
    }

    fn reasoning_tool_request(scope: &str) -> ModelRequest {
        use crate::provider::protocol::{AssistantItem, ToolCall, ToolResult};
        let mut replay = super::super::common::reasoning_envelope(
            "responses",
            "gpt-5",
            reasoning_tool_output()[0].clone(),
        );
        replay.scope = scope.into();
        let mut reasoning = AssistantItem::reasoning("rs_private", 0, "", Some(replay));
        reasoning.blocks.clear();
        ModelRequest {
            model: "gpt-5".into(),
            system: vec![],
            tools: vec![],
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
            response_schema: None,
            reasoning: None,
            max_output_tokens: None,
            correlation: None,
        }
    }

    #[test]
    fn configured_provider_aliases_do_not_share_private_replay() {
        let default = CodexProvider::new().unwrap();
        assert_eq!(default.replay_scope(), reasoning_scope("codex", ENDPOINT));
        let a = default.clone().with_name("alias-a");
        let b = default.with_name("alias-b");
        assert_eq!(a.endpoint, b.endpoint);
        assert_ne!(a.replay_scope(), b.replay_scope());
        let original = reasoning_tool_request(&a.replay_scope());
        let mut same = original.clone();
        filter_reasoning_scope(&mut same, &a.replay_scope());
        assert_eq!(
            responses::encode(&same).unwrap()["input"][0],
            reasoning_tool_output()[0]
        );
        let mut foreign = original.clone();
        filter_reasoning_scope(&mut foreign, &b.replay_scope());
        let wire = responses::encode(&foreign).unwrap();
        assert_eq!(wire["input"].as_array().unwrap().len(), 2);
        assert_eq!(wire["input"][0]["type"], "function_call");
        assert_eq!(wire["input"][1]["type"], "function_call_output");
        assert_eq!(
            responses::encode(&original).unwrap()["input"][0],
            reasoning_tool_output()[0]
        );
    }

    #[tokio::test]
    async fn native_reasoning_continuation_and_full_replay_survive_save_resume() {
        use super::super::common::tests::resume_request;
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
            name: "codex".into(),
            auth: auth::test_manager(directory.path().to_owned()),
            client: transport::client().unwrap(),
            endpoint: format!("http://{address}/responses"),
            ws_endpoint: format!("ws://{address}/responses"),
        };
        let mut context = provider.open_context("context".into()).unwrap();
        let scope = provider.replay_scope();
        let mut request = reasoning_tool_request(&scope);
        request.max_output_tokens = Some(100);
        request.correlation = Some("context".into());
        let request = super::super::common::tests::resume_request(&request).await;
        let expected_input = responses::encode(&request).unwrap()["input"].clone();
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
        assert_eq!(body["input"], expected_input);
        assert_eq!(body["input"][0], reasoning_tool_output()[0]);
        assert_eq!(body["input"][1]["call_id"], body["input"][2]["call_id"]);
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
                name: "codex".into(),
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
}
