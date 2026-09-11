//! Per-context connection ownership, reuse probing, and handshake-only HTTP fallback.
use super::super::{common::filter_reasoning_scope, responses, transport};
use super::recovery::{socket_error, websocket_error};
use super::transport::{DEADLINE, Socket, http_stream, is_transport_metadata, ws_stream};
use super::{CodexProvider, auth_error, error};
use crate::provider::{
    CodexWebSocketError, ProviderContext, ProviderError, ProviderErrorKind, ProviderFuture,
    protocol::ModelRequest,
};
use futures_util::{SinkExt, StreamExt};
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::{sync::Mutex, time::Instant};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

// Retire before the observed ~five-minute server/proxy idle timeout.
const MAX_IDLE: Duration = Duration::from_secs(240);
const REUSE_PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_PROBE_FRAMES: usize = 32;

#[derive(Default)]
pub(super) struct Session {
    pub(super) socket: Option<Socket>,
    pub(super) reusable_since: Option<Instant>,
    pub(super) continuation: Option<Continuation>,
    pub(super) affinity: Option<HeaderValue>,
    pub(super) http_only: bool,
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

pub(super) struct Continuation {
    pub(super) id: String,
    pub(super) input: Vec<Value>,
    pub(super) settings: Value,
}
pub(super) struct Context {
    provider: CodexProvider,
    correlation: String,
    session: Arc<Mutex<Session>>,
}
impl Context {
    pub(super) fn new(provider: CodexProvider, correlation: String) -> Self {
        Self {
            provider,
            correlation,
            session: Arc::new(Mutex::new(Session::default())),
        }
    }
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
pub(super) fn websocket_request(
    body: &Value,
    previous: Option<&Continuation>,
) -> (Value, Value, Vec<Value>) {
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

#[cfg(test)]
mod tests {
    use super::super::super::common::tests::request as base_request;
    use super::super::auth;
    use super::super::transport::tests::{
        mock_socket, reasoning_tool_output, reasoning_tool_request,
    };
    use super::*;
    use crate::provider::backends::transport::tests::read_request as read_http_request;
    use crate::provider::{Provider, ProviderRecovery, ResponseChunk};
    use tokio_tungstenite::WebSocketStream;

    fn request() -> ModelRequest {
        ModelRequest {
            correlation: Some("context".into()),
            ..base_request("gpt-5")
        }
    }

    fn provider(address: std::net::SocketAddr, directory: &std::path::Path) -> CodexProvider {
        CodexProvider {
            name: "codex".into(),
            auth: auth::test_manager(directory.to_owned()),
            client: transport::client().unwrap(),
            endpoint: format!("http://{address}/responses"),
            ws_endpoint: format!("ws://{address}/responses"),
        }
    }

    async fn read_request<S>(socket: &mut WebSocketStream<S>) -> Value
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        loop {
            match socket.next().await.unwrap().unwrap() {
                Message::Ping(payload) => socket.send(Message::Pong(payload)).await.unwrap(),
                Message::Text(text) => return serde_json::from_str(&text).unwrap(),
                frame => panic!("unexpected test frame: {frame:?}"),
            }
        }
    }

    async fn completed<S>(socket: &mut WebSocketStream<S>, id: &str)
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        socket.send(Message::Text(json!({"type":"response.completed", "response":{"id":id,"status":"completed","output":[]}}).to_string().into())).await.unwrap();
    }

    #[tokio::test]
    // Tungstenite's handshake callback requires its unboxed HTTP ErrorResponse.
    #[allow(clippy::result_large_err)]
    async fn runtime_reinvoke_uses_fresh_socket_full_history_without_affinity_or_hidden_replay() {
        use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
        // Run both explicit runtime reset and provider-only failure retirement.
        for explicit_reset in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let (retired_tx, retired_rx) = tokio::sync::oneshot::channel();
            let (retry_tx, retry_rx) = tokio::sync::oneshot::channel();
            let server = tokio::spawn(async move {
                let (tcp, _) = listener.accept().await.unwrap();
                let mut socket = tokio_tungstenite::accept_hdr_async(
                    tcp,
                    |request: &Request, mut response: Response| {
                        assert!(!request.headers().contains_key("x-codex-turn-state"));
                        response.headers_mut().insert(
                            "x-codex-turn-state",
                            HeaderValue::from_static("connection-secret-affinity"),
                        );
                        Ok(response)
                    },
                )
                .await
                .unwrap();
                let first = read_request(&mut socket).await;
                completed(&mut socket, "committed-response").await;
                let second = read_request(&mut socket).await;
                assert_eq!(second["previous_response_id"], "committed-response");
                assert_eq!(second["input"], json!([]));
                // Start an uncommitted response, then lose the connection abruptly.
                socket.send(Message::Text(json!({"type":"response.output_item.added", "output_index":0,
                    "item":{"id":"partial","type":"function_call","call_id":"not-executed","name":"local_tool","arguments":""}}).to_string().into())).await.unwrap();
                drop(socket);
                // There must be no provider-owned request replay while the runtime
                // is deciding whether this response is safe to retry.
                assert!(
                    tokio::time::timeout(Duration::from_millis(30), listener.accept())
                        .await
                        .is_err()
                );
                retired_tx.send(()).unwrap();
                retry_rx.await.unwrap();
                let (tcp, _) = listener.accept().await.unwrap();
                let mut socket = tokio_tungstenite::accept_hdr_async(
                    tcp,
                    |request: &Request, response: Response| {
                        assert!(!request.headers().contains_key("x-codex-turn-state"));
                        Ok(response)
                    },
                )
                .await
                .unwrap();
                let third = read_request(&mut socket).await;
                assert!(third.get("previous_response_id").is_none());
                assert_eq!(third["input"], first["input"]);
                assert!(!third.to_string().contains("not-executed"));
                completed(&mut socket, "retried-response").await;
            });
            let directory = tempfile::tempdir().unwrap();
            let provider = provider(address, directory.path());
            let mut context = provider.open_context("context".into()).unwrap();
            let first = context
                .invoke(request())
                .await
                .unwrap()
                .collect::<Vec<_>>()
                .await;
            assert!(first.iter().all(Result::is_ok), "{first:?}");
            let failed = context
                .invoke(request())
                .await
                .unwrap()
                .collect::<Vec<_>>()
                .await;
            let error = failed.last().unwrap().as_ref().unwrap_err();
            assert_eq!(error.recovery(), Some(ProviderRecovery::ResetContext));
            retired_rx.await.unwrap();
            if explicit_reset {
                context.reset();
            }
            retry_tx.send(()).unwrap();
            let final_chunks = context
                .invoke(request())
                .await
                .unwrap()
                .collect::<Vec<_>>()
                .await;
            assert!(final_chunks.iter().all(Result::is_ok), "{final_chunks:?}");
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn reset_detaches_even_a_locked_session_and_clears_http_fallback() {
        let directory = tempfile::tempdir().unwrap();
        let provider = provider("127.0.0.1:1".parse().unwrap(), directory.path());
        let old = Arc::new(Mutex::new(Session {
            continuation: Some(Continuation {
                id: "old".into(),
                input: vec![],
                settings: json!({}),
            }),
            affinity: Some(HeaderValue::from_static("old-affinity")),
            http_only: true,
            ..Session::default()
        }));
        let mut context = Context {
            provider,
            correlation: "context".into(),
            session: old.clone(),
        };
        let _old_guard = old.lock().await;
        context.reset();
        assert!(!Arc::ptr_eq(&context.session, &old));
        let session = context.session.try_lock().unwrap();
        assert!(session.socket.is_none());
        assert!(session.reusable_since.is_none());
        assert!(session.continuation.is_none());
        assert!(session.affinity.is_none());
        assert!(!session.http_only);
    }

    #[test]
    fn credentials_are_sensitive_headers() {
        let headers = auth_headers("secret", "account", "session").unwrap();
        assert!(headers["authorization"].is_sensitive());
        assert!(headers["chatgpt-account-id"].is_sensitive());
        assert!(auth_headers("bad\ntoken", "account", "session").is_err());
    }

    #[tokio::test]
    async fn rejected_handshake_falls_back_once_with_full_http_history() {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for index in 0..2 {
                let (mut tcp, _) = listener.accept().await.unwrap();
                requests.push(read_http_request(&mut tcp).await);
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
        let provider = provider(address, directory.path());
        let mut context = provider.open_context("context".into()).unwrap();
        let scope = provider.replay_scope();
        let mut request = reasoning_tool_request(&scope);
        request.max_output_tokens = Some(100);
        request.correlation = Some("context".into());
        let request = super::super::super::common::tests::resume_request(&request).await;
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
        assert_eq!(body["reasoning"], json!({"summary":"auto"}));
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
            assert_eq!(wire["reasoning"], json!({"summary":"auto"}));
            assert!(wire.get("previous_response_id").is_none());
            assert_eq!(wire["input"].as_array().unwrap().len(), 1);
            ws.send(Message::Text(json!({"type":"response.completed","response":{"id":"fresh","status":"completed","output":[]}}).to_string().into())).await.unwrap();
        });
        let directory = tempfile::tempdir().unwrap();
        let session = Arc::new(Mutex::new(retained_session(socket)));
        let mut context = Context {
            provider: provider(address, directory.path()),
            correlation: "test".into(),
            session: session.clone(),
        };
        let request = ModelRequest {
            correlation: Some("test".into()),
            ..base_request("fixture")
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
