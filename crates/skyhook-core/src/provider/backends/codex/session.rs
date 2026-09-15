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
use serde_json::{Map, Value, json};
use std::{sync::Arc, time::Duration};
use tokio::{sync::Mutex, time::Instant};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

// Retire before the observed ~five-minute server/proxy idle timeout.
const MAX_IDLE: Duration = Duration::from_secs(240);
const REUSE_PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_PROBE_FRAMES: usize = 32;

/// Only a drained successful response can install a reusable connection. The
/// session lock is retained by startup/stream work, never by a return-on-Drop hook.
#[derive(Default)]
pub(super) enum Session {
    #[default]
    Disconnected,
    HttpOnly,
    Reusable(Box<ReusableConnection>),
}

/// Freshly connected or request-owned routing state has no idle timestamp or
/// continuation. Those belong only to a successfully completed reusable bundle.
pub(super) struct Connection {
    pub(super) socket: Socket,
    pub(super) affinity: Option<HeaderValue>,
}
pub(super) struct ReusableConnection {
    pub(super) connection: Connection,
    pub(super) since: Instant,
    pub(super) continuation: Option<Continuation>,
}

/// Take the entire connection before awaiting the probe. Cancellation, timeout,
/// stale sockets and malformed trailing frames leave the session disconnected.
async fn prepare_reuse(session: &mut Session) {
    let previous = std::mem::take(session);
    let Session::Reusable(mut reusable) = previous else {
        *session = previous;
        return;
    };
    if reusable.since.elapsed() >= MAX_IDLE {
        return;
    }
    if matches!(
        tokio::time::timeout(
            REUSE_PROBE_TIMEOUT,
            probe_socket(&mut reusable.connection.socket)
        )
        .await,
        Ok(true)
    ) {
        reusable.since = Instant::now();
        *session = Session::Reusable(reusable);
    }
    // Failed probes cause a fresh connection/full-history request, never replay.
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
    pub(super) settings: Map<String, Value>,
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
            adapt_subscription_request(&mut body);
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
            let (connection, continuation) = match std::mem::take(&mut *session) {
                Session::Reusable(reusable) => {
                    let ReusableConnection {
                        connection,
                        continuation,
                        ..
                    } = *reusable;
                    if let Some(affinity) = &connection.affinity {
                        headers.insert("x-codex-turn-state", affinity.clone());
                    }
                    (Some(connection), continuation)
                }
                Session::HttpOnly => {
                    *session = Session::HttpOnly;
                    (None, None)
                }
                Session::Disconnected => match connect(&provider.ws_endpoint, &headers).await {
                    Ok((socket, affinity)) => (Some(Connection { socket, affinity }), None),
                    // A throttle is not a capability rejection. Do not immediately
                    // retry through HTTP or replay after a request write.
                    Err(error)
                        if error.retry_after.is_some()
                            || matches!(
                                error.kind,
                                ProviderErrorKind::RateLimited
                                    | ProviderErrorKind::Timeout
                                    | ProviderErrorKind::Response
                            ) =>
                    {
                        return Err(error);
                    }
                    // No response.create has been sent: full-history fallback is safe.
                    Err(_) => {
                        *session = Session::HttpOnly;
                        (None, None)
                    }
                },
            };
            if let Some(mut connection) = connection {
                let prepared = websocket_request(body, continuation.as_ref());
                // Routing and continuation are request-owned before the write;
                // failed or cancelled writes cannot restore any connection state.
                tokio::time::timeout(
                    DEADLINE,
                    connection.socket.send(Message::Text(
                        prepared.wire_request.into_wire().to_string().into(),
                    )),
                )
                .await
                .map_err(|_| websocket_error(CodexWebSocketError::WriteTimeout))?
                .map_err(|error| socket_error(error, CodexWebSocketError::Write))?;
                return Ok(ws_stream(
                    connection,
                    session,
                    responses::Decoder::codex(request.model),
                    prepared.settings,
                    prepared.full_input,
                    scope,
                ));
            }
            let body = body.into_wire();
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
    .map_err(|native| match native {
        tokio_tungstenite::tungstenite::Error::Http(response) => {
            let body = response
                .body()
                .as_ref()
                .filter(|body| body.len() <= 16 * 1024)
                .and_then(|body| serde_json::from_slice(body).ok())
                .unwrap_or(Value::Null);
            let mut error =
                super::super::errors::classify_error(Some(response.status().as_u16()), &body);
            error.retry_after =
                transport::retry_after(response.headers(), std::time::SystemTime::now());
            error
        }
        _ => error(
            ProviderErrorKind::Transport,
            "Codex WebSocket handshake failed",
        ),
    })?;
    Ok((
        socket,
        response.headers().get("x-codex-turn-state").cloned(),
    ))
}
/// Subscription wire constraints; no second request/response codec. All other
/// settings (including opaque vendor metadata) survive without reconstruction.
fn adapt_subscription_request(body: &mut responses::EncodedRequest) {
    body.settings.insert("store".into(), json!(false));
    body.settings
        .entry("instructions")
        .or_insert_with(|| json!(""));
    body.settings
        .insert("include".into(), json!(["reasoning.encrypted_content"]));
    // The subscription service does not accept an output-token limit; the
    // profile value remains a local context budget only.
    body.settings.remove("max_output_tokens");
}

pub(super) struct PreparedWebSocketRequest {
    pub(super) wire_request: responses::EncodedRequest,
    pub(super) settings: Map<String, Value>,
    pub(super) full_input: Vec<Value>,
}

pub(super) fn websocket_request(
    body: responses::EncodedRequest,
    previous: Option<&Continuation>,
) -> PreparedWebSocketRequest {
    let responses::EncodedRequest { input, settings } = body;
    let mut wire_request = responses::EncodedRequest {
        input: input.clone(),
        settings: settings.clone(),
    };
    if let Some(previous) = previous
        .filter(|previous| previous.settings == settings && input.starts_with(&previous.input))
    {
        wire_request
            .settings
            .insert("previous_response_id".into(), json!(previous.id));
        wire_request.input = input[previous.input.len()..].to_vec();
    }
    wire_request
        .settings
        .insert("type".into(), json!("response.create"));
    PreparedWebSocketRequest {
        wire_request,
        settings,
        full_input: input,
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::common::tests::request as base_request;
    use super::super::auth;
    use super::super::transport::tests::{
        mock_socket, reasoning_tool_output, reasoning_tool_request, serve_socket,
    };
    use super::*;
    use crate::provider::backends::transport::tests::{read_request as read_http_request, reply};
    use crate::provider::{Provider, ProviderRecovery, ResponseChunk};
    use tokio::io::AsyncWriteExt;
    use tokio_tungstenite::WebSocketStream;

    fn request() -> ModelRequest {
        ModelRequest {
            correlation: Some("context".into()),
            blobs: Default::default(),
            ..base_request("gpt-5")
        }
    }

    #[test]
    fn subscription_adaptation_preserves_opaque_settings_and_explicit_history() {
        let mut body = responses::encode(&request()).unwrap();
        for (key, value) in [
            ("store", json!(true)),
            ("include", json!(["other"])),
            ("max_output_tokens", json!(100)),
            (
                "vendor_options",
                json!({"nested":[null, false, {"opaque":"keep"}]}),
            ),
            ("instructions", json!("keep instructions")),
        ] {
            body.settings.insert(key.into(), value);
        }
        let input = body.input.clone();
        let mut expected = body.settings.clone();
        expected.insert("store".into(), json!(false));
        expected.insert("include".into(), json!(["reasoning.encrypted_content"]));
        expected.remove("max_output_tokens");
        adapt_subscription_request(&mut body);
        assert_eq!(body.settings, expected);
        assert_eq!(body.input, input);

        body.settings.remove("instructions");
        adapt_subscription_request(&mut body);
        assert_eq!(body.settings["instructions"], "");
    }

    #[test]
    fn prepared_websocket_request_separates_wire_suffix_from_full_history() {
        let prefix =
            json!({"type":"reasoning", "encrypted_content":"opaque", "vendor":{"keep":[1,null]}});
        let next = json!({"role":"user", "content":[{"type":"input_text", "text":"next"}]});
        let settings = Map::from_iter([
            ("model".into(), json!("gpt-5")),
            ("vendor".into(), json!({"nested":[false,null]})),
        ]);
        let body = responses::EncodedRequest {
            input: vec![prefix.clone(), next.clone()],
            settings: settings.clone(),
        };
        let previous = Continuation {
            id: "r1".into(),
            input: vec![prefix.clone()],
            settings: settings.clone(),
        };
        let prepared = websocket_request(body.clone(), Some(&previous));
        assert_eq!(prepared.full_input, body.input);
        assert_eq!(prepared.settings, settings);
        assert_eq!(
            prepared.wire_request.into_wire(),
            json!({
                "model":"gpt-5", "vendor":{"nested":[false,null]},
                "previous_response_id":"r1", "type":"response.create", "input":[next]
            })
        );
        assert_eq!(
            websocket_request(body, None).wire_request.into_wire(),
            json!({
                "model":"gpt-5", "vendor":{"nested":[false,null]},
                "type":"response.create", "input":[prefix,next]
            })
        );
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

    fn completed(id: &str) -> Value {
        json!({"type":"response.completed", "response":{"id":id,"status":"completed","output":[]}})
    }

    async fn send<S>(socket: &mut WebSocketStream<S>, event: Value)
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        socket
            .send(Message::Text(event.to_string().into()))
            .await
            .unwrap();
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
                send(&mut socket, completed("committed-response")).await;
                let second = read_request(&mut socket).await;
                assert_eq!(second["previous_response_id"], "committed-response");
                assert_eq!(second["input"], json!([]));
                // Start an uncommitted response, then lose the connection abruptly.
                send(&mut socket, json!({"type":"response.output_item.added", "output_index":0,
                    "item":{"id":"partial","type":"function_call","call_id":"not-executed","name":"local_tool","arguments":""}})).await;
                drop(socket);
                // No provider-owned replay while the runtime decides whether to retry.
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
                send(&mut socket, completed("retried-response")).await;
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
            let last = context
                .invoke(request())
                .await
                .unwrap()
                .collect::<Vec<_>>()
                .await;
            assert!(last.iter().all(Result::is_ok), "{last:?}");
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn reset_detaches_even_a_locked_session_and_clears_http_fallback() {
        let directory = tempfile::tempdir().unwrap();
        let provider = provider("127.0.0.1:1".parse().unwrap(), directory.path());
        let old = Arc::new(Mutex::new(Session::HttpOnly));
        let mut context = Context {
            provider,
            correlation: "context".into(),
            session: old.clone(),
        };
        let _old_guard = old.lock().await;
        context.reset();
        assert!(!Arc::ptr_eq(&context.session, &old));
        assert!(matches!(
            *context.session.try_lock().unwrap(),
            Session::Disconnected
        ));
    }

    #[test]
    fn credentials_are_sensitive_headers() {
        let headers = auth_headers("secret", "account", "session").unwrap();
        assert!(headers["authorization"].is_sensitive());
        assert!(headers["chatgpt-account-id"].is_sensitive());
        assert!(auth_headers("bad\ntoken", "account", "session").is_err());
    }

    #[tokio::test]
    async fn throttled_handshake_preserves_retry_after_without_http_fallback() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            assert!(read_http_request(&mut socket).await.starts_with("GET "));
            let throttled = reply("429 Too Many Requests", "Retry-After: 120\r\n", "");
            socket.write_all(throttled.as_bytes()).await.unwrap();
            drop(socket);
            assert!(
                tokio::time::timeout(Duration::from_millis(100), listener.accept())
                    .await
                    .is_err(),
                "a throttled handshake must not immediately try the HTTP endpoint"
            );
        });
        let directory = tempfile::tempdir().unwrap();
        let provider = provider(address, directory.path());
        let mut context = provider.open_context("context".into()).unwrap();
        let Err(error) = context.invoke(request()).await else {
            panic!("expected throttled handshake")
        };
        assert_eq!(error.kind, ProviderErrorKind::RateLimited);
        assert_eq!(error.retry_after, Some(Duration::from_secs(120)));
        assert!(error.message.contains("429"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn rejected_handshake_falls_back_once_with_full_http_history() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let body = format!("data: {}\n\n", completed("r1"));
            let mut requests = Vec::new();
            for wire in [
                reply("426 Upgrade Required", "", ""),
                reply("200 OK", "Content-Type: text/event-stream\r\n", &body),
            ] {
                let (mut tcp, _) = listener.accept().await.unwrap();
                requests.push(read_http_request(&mut tcp).await);
                tcp.write_all(wire.as_bytes()).await.unwrap();
            }
            requests
        });
        let directory = tempfile::tempdir().unwrap();
        let provider = provider(address, directory.path());
        let mut context = provider.open_context("context".into()).unwrap();
        let mut request = reasoning_tool_request(&provider.replay_scope());
        request.max_output_tokens = Some(100);
        request.correlation = Some("context".into());
        let request = super::super::super::common::tests::resume_request(&request).await;
        let expected_input = json!(responses::encode(&request).unwrap().input);
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
        Session::Reusable(Box::new(ReusableConnection {
            connection: Connection {
                socket,
                affinity: Some(HeaderValue::from_static("stale-affinity")),
            },
            since: Instant::now(),
            continuation: Some(Continuation {
                id: "stale-response".into(),
                input: vec![],
                settings: Map::new(),
            }),
        }))
    }

    #[tokio::test]
    async fn cancellation_during_reuse_probe_retires_the_entire_bundle() {
        let (ping_tx, ping_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let (socket, server) = serve_socket(|mut socket| async move {
            let frame = socket.next().await.unwrap().unwrap();
            assert!(matches!(frame, Message::Ping(_)));
            ping_tx.send(()).unwrap();
            release_rx.await.unwrap();
        })
        .await;
        let mut session = retained_session(socket);
        let mut probe = Box::pin(prepare_reuse(&mut session));
        tokio::select! {
            _ = &mut probe => panic!("probe cannot finish without pong"),
            result = ping_rx => result.unwrap(),
        }
        drop(probe);
        assert!(matches!(session, Session::Disconnected));
        release_tx.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn idle_expiry_retires_the_bundle_without_a_probe_or_request() {
        // Retirement closes the socket without sending ping/model content.
        let (socket, server) = serve_socket(|mut socket| async move {
            assert!(!matches!(socket.next().await, Some(Ok(_))));
        })
        .await;
        let mut session = retained_session(socket);
        let Session::Reusable(reusable) = &mut session else {
            unreachable!()
        };
        reusable.since = Instant::now() - MAX_IDLE;
        prepare_reuse(&mut session).await;
        assert!(matches!(session, Session::Disconnected));
        server.await.unwrap();
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
            let wire = read_request(&mut ws).await;
            assert_eq!(wire["type"], "response.create");
            assert_eq!(wire["reasoning"], json!({"summary":"auto"}));
            assert!(wire.get("previous_response_id").is_none());
            assert_eq!(wire["input"].as_array().unwrap().len(), 1);
            send(&mut ws, completed("fresh")).await;
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
            blobs: Default::default(),
            ..base_request("fixture")
        };
        let invoke = async {
            context
                .invoke(request)
                .await
                .unwrap()
                .collect::<Vec<_>>()
                .await
        };
        let result = tokio::time::timeout(Duration::from_secs(5), invoke)
            .await
            .unwrap();
        assert!(result.iter().all(Result::is_ok), "{result:?}");
        assert!(
            result
                .iter()
                .flatten()
                .any(|event| matches!(event, ResponseChunk::ResponseEnded { .. }))
        );
        let session = session.lock().await;
        let Session::Reusable(reusable) = &*session else {
            panic!("successful fresh response must restore a reusable connection");
        };
        assert!(reusable.connection.affinity.is_none());
        assert_eq!(reusable.continuation.as_ref().unwrap().id, "fresh");
        server.await.unwrap();
    }
}
