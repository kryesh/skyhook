//! Root-session owned MCP transports. No shell interpolation or implicit reconnect/replay.

use std::{collections::HashMap, io, path::Path, process::Stdio, sync::Arc, time::Duration};

use crate::tool::diagnostic::opaque_io;
use futures_util::{Stream, StreamExt, TryStreamExt, stream::BoxStream};
use reqwest::header::{HeaderName, HeaderValue};
use rmcp::{
    RoleClient, ServiceExt,
    model::{ClientJsonRpcMessage, ClientRequest, ErrorData, JsonRpcMessage, ServerJsonRpcMessage},
    service::{ClientInitializeError, RunningService},
    transport::{
        DynamicTransportError, StreamableHttpClientTransport,
        common::client_side_sse::NeverRetry,
        streamable_http_client::{
            StreamableHttpClient, StreamableHttpClientTransportConfig, StreamableHttpError,
            StreamableHttpPostResponse,
        },
    },
};
use tokio::process::{Child, Command};

use super::{
    config::{CommandSpec, McpConnection, McpServerConfig},
    manager::McpError,
};
use crate::provider::dialect::{ConfiguredHeaders, Sources};

pub(crate) type Client = RunningService<RoleClient, ()>;

/// The largest MCP message read from a server: an HTTP body, an SSE event, or one
/// stdio line. A larger stdio line is discarded on its own and the server stays usable;
/// the call waiting for it ends at its timeout, since the discarded line cannot name it.
pub(crate) const MAX_MESSAGE_BYTES: usize = crate::limits::WHOLE_VALUE_BYTES;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const READINESS_POLL: Duration = Duration::from_millis(100);

/// Only processes created by this session enter this type. Each command is the
/// leader of a fresh process group, so shutdown also reaches descendants.
/// Dropping it kills the group and the child; Tokio reaps a dropped child as a
/// best-effort fallback for a cancelled owner future.
pub(crate) struct OwnedProcess {
    group: crate::process_group::ProcessGroup,
    child: Child,
}

impl OwnedProcess {
    fn spawn(spec: &CommandSpec, workspace: &Path, piped: bool) -> Result<Self, McpError> {
        let mut command = Command::new(&spec.argv[0]);
        command
            .args(&spec.argv[1..])
            .envs(&spec.env)
            .current_dir(spec.directory(workspace))
            .kill_on_drop(true);
        command.stdin(if piped { Stdio::piped() } else { Stdio::null() });
        command.stdout(if piped { Stdio::piped() } else { Stdio::null() });
        // Never leave an unread stderr pipe that could deadlock initialization.
        command.stderr(Stdio::null());
        command.process_group(0);
        let child = command.spawn().map_err(McpError::Io)?;
        Ok(Self {
            group: crate::process_group::ProcessGroup::led_by(&child),
            child,
        })
    }

    pub(crate) async fn shutdown(&mut self) {
        self.group.kill();
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
}

/// Classify only a failed network connection as launchable. HTTP status errors,
/// TLS/authentication errors, malformed responses and protocol negotiation are
/// deliberately *not* reasons to run a configured command.
fn unreachable(error: &ClientInitializeError) -> bool {
    let ClientInitializeError::TransportError { error, .. } = error else {
        return false;
    };
    let Some(StreamableHttpError::Client(error)) = error
        .error
        .downcast_ref::<StreamableHttpError<reqwest::Error>>()
    else {
        return false;
    };
    if !error.is_connect() {
        return false;
    }
    // reqwest also categorizes TLS failures as connect errors. Only concrete
    // networking IO failures (not InvalidData/Other certificate errors) qualify.
    // Reset/aborted connections can be TLS handshake failures on reachable
    // endpoints, so they are deliberately not launchable.
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(current) = source {
        if let Some(io) = current.downcast_ref::<std::io::Error>() {
            return matches!(
                io.kind(),
                std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::NotConnected
                    | std::io::ErrorKind::AddrNotAvailable
                    | std::io::ErrorKind::NetworkUnreachable
                    | std::io::ErrorKind::HostUnreachable
                    | std::io::ErrorKind::TimedOut
            );
        }
        source = current.source();
    }
    false
}

/// Inspect concrete SDK causes, never SDK Display strings: they may carry full
/// endpoints, headers, server-controlled messages, or rejected request values.
pub(super) fn transport_error(error: DynamicTransportError) -> McpError {
    let error = match error.error.downcast::<io::Error>() {
        Ok(error) => return McpError::Io(opaque_io(*error)),
        Err(error) => error,
    };
    let Some(error) = error.downcast_ref::<HttpError>() else {
        return McpError::Transport;
    };
    match error {
        StreamableHttpError::Client(error) => http_error(error),
        StreamableHttpError::Io(error) => McpError::Io(opaque_io(error)),
        StreamableHttpError::AuthRequired(_) => McpError::AuthenticationRequired,
        StreamableHttpError::InsufficientScope(_) => McpError::InsufficientScope,
        StreamableHttpError::SessionExpired => McpError::SessionExpired,
        StreamableHttpError::SessionRecoveryTimeout
        | StreamableHttpError::ControlRequestTimeout => McpError::Timeout,
        StreamableHttpError::UnexpectedEndOfStream
        | StreamableHttpError::TransportChannelClosed => McpError::TransportClosed,
        StreamableHttpError::Deserialize(_) => McpError::Decode,
        StreamableHttpError::UnexpectedServerResponse(_)
        | StreamableHttpError::UnexpectedContentType(_)
        | StreamableHttpError::MissingSessionIdInResponse => McpError::UnexpectedResponse,
        _ => McpError::Transport,
    }
}

fn http_error(error: &reqwest::Error) -> McpError {
    if let Some(status) = error.status() {
        return McpError::HttpStatus(status.as_u16());
    }
    if error.is_timeout() {
        return McpError::Timeout;
    }
    let mut source = std::error::Error::source(error);
    while let Some(error) = source {
        if let Some(error) = error.downcast_ref::<io::Error>() {
            return McpError::Io(opaque_io(error));
        }
        source = error.source();
    }
    if error.is_decode() {
        McpError::Decode
    } else {
        McpError::Transport
    }
}

fn initialize_error(error: ClientInitializeError) -> McpError {
    match error {
        ClientInitializeError::TransportError { error, .. } => transport_error(error),
        ClientInitializeError::JsonRpcError(error) => McpError::JsonRpc(error.code.0),
        ClientInitializeError::ConnectionClosed(_) => McpError::TransportClosed,
        ClientInitializeError::Cancelled => McpError::Cancelled,
        ClientInitializeError::NoCompatibleProtocolVersion { .. }
        | ClientInitializeError::NoPreferredProtocolVersion => McpError::ProtocolVersion,
        ClientInitializeError::LegacyFallbackFailed { fallback, .. } => initialize_error(*fallback),
        ClientInitializeError::ExpectedInitResponse(_)
        | ClientInitializeError::ExpectedInitResult(_)
        | ClientInitializeError::ConflictInitResponseId(..)
        | ClientInitializeError::UncorrelatedErrorResponse { .. } => McpError::UnexpectedResponse,
        _ => McpError::Transport,
    }
}

/// The connection's configuration, its headers read as a provider reads its own: each
/// environment value now, each command run once for the connection.
async fn http_config(
    endpoint: &str,
    headers: &ConfiguredHeaders,
) -> Result<StreamableHttpClientTransportConfig, McpError> {
    let mut transport = StreamableHttpClientTransportConfig::with_uri(endpoint.to_owned());
    // A call with an uncertain outcome must never be automatically sent again.
    transport.reinit_on_expired_session = false;
    transport.retry_config = Arc::new(NeverRetry::default());
    transport.max_sse_event_size = MAX_MESSAGE_BYTES;
    let mut sources = Sources::default();
    let headers = headers.pending().try_map(|value| sources.read(value))?;
    let resolved = headers.resolve().await.map_err(McpError::HeaderCommand)?;
    let resolved = resolved.map();
    let resolved = resolved
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()));
    transport.custom_headers.extend(resolved);
    Ok(transport)
}

// rmcp's reqwest adapter bounds SSE events, but collects JSON/error bodies without
// a limit. Own the POST parser so every body is bounded BEFORE parsing. Preserve
// reqwest::Error as the transport error type for unreachable() classification.
#[derive(Clone)]
struct BoundedHttpClient {
    inner: reqwest::Client,
    timeout: Duration,
}

type HttpError = StreamableHttpError<reqwest::Error>;

fn redact_http_error(error: HttpError) -> HttpError {
    // The SDK may log transport errors before the manager maps them. Strip URL
    // credentials/query parameters at the source as well as at the tool boundary.
    match error {
        StreamableHttpError::Client(error) => StreamableHttpError::Client(error.without_url()),
        other => other,
    }
}

fn bounded_bytes<S, B, E>(stream: S, limit: usize) -> impl Stream<Item = Result<B, io::Error>>
where
    S: Stream<Item = Result<B, E>> + Unpin,
    B: AsRef<[u8]>,
    E: std::error::Error + Send + Sync + 'static,
{
    futures_util::stream::try_unfold((stream, limit), |(mut stream, left)| async move {
        let Some(chunk) = stream.next().await else {
            return Ok(None);
        };
        let chunk = chunk.map_err(io::Error::other)?;
        let remaining = left
            .checked_sub(chunk.as_ref().len())
            .ok_or_else(|| io::Error::other("MCP HTTP response byte limit exceeded"))?;
        Ok(Some((chunk, (stream, remaining))))
    })
}

fn body_error() -> HttpError {
    // Never include an untrusted response body (potentially secrets) in errors.
    StreamableHttpError::UnexpectedServerResponse(
        "MCP HTTP body unreadable or byte limit exceeded".into(),
    )
}

impl BoundedHttpClient {
    async fn parse_post_response(
        response: reqwest::Response,
        message: &ClientJsonRpcMessage,
        session_attached: bool,
    ) -> Result<StreamableHttpPostResponse, HttpError> {
        let status = response.status();
        if matches!(
            status,
            reqwest::StatusCode::ACCEPTED | reqwest::StatusCode::NO_CONTENT
        ) {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        if status == reqwest::StatusCode::NOT_FOUND && session_attached {
            return Err(StreamableHttpError::SessionExpired);
        }
        // Authentication failures must never enter legacy discovery fallback.
        if matches!(
            status,
            reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
        ) {
            return Err(StreamableHttpError::Client(
                response.error_for_status().unwrap_err().without_url(),
            ));
        }
        let status_error = response
            .error_for_status_ref()
            .err()
            .map(reqwest::Error::without_url);
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let session = response
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        if status.is_success()
            && response.content_length() == Some(0)
            && !matches!(message, ClientJsonRpcMessage::Request(_))
        {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        if status.is_success() && content_type.starts_with("text/event-stream") {
            // Deliberately cap the entire POST stream, including comments and
            // progress events. This also bounds each raw event before parsing.
            let bytes = bounded_bytes(response.bytes_stream(), MAX_MESSAGE_BYTES);
            let events = sse_stream::SseStream::from_bytes_stream(bytes).boxed();
            return Ok(StreamableHttpPostResponse::Sse(events, session));
        }
        if response
            .content_length()
            .is_some_and(|n| n > MAX_MESSAGE_BYTES as u64)
        {
            return Err(body_error());
        }
        let body = bounded_bytes(response.bytes_stream(), MAX_MESSAGE_BYTES)
            .try_fold(Vec::new(), |mut body, chunk| async move {
                body.extend_from_slice(&chunk);
                Ok(body)
            })
            .await
            .map_err(|_| body_error())?;
        if content_type.starts_with("application/json") {
            let parsed = serde_json::from_slice::<ServerJsonRpcMessage>(&body);
            match parsed {
                Ok(message)
                    if status.is_success() || matches!(message, JsonRpcMessage::Error(_)) =>
                {
                    return Ok(StreamableHttpPostResponse::Json(message, session));
                }
                _ => {}
            }
        }
        // rmcp 3.2 probes server/discover before initializing older servers.
        // Retain its legacy 4xx fallback, without copying raw response text.
        if !session_attached
            && status.is_client_error()
            && let ClientJsonRpcMessage::Request(request) = message
            && matches!(request.request, ClientRequest::DiscoverRequest(_))
        {
            return Ok(StreamableHttpPostResponse::Json(
                ServerJsonRpcMessage::error(
                    ErrorData::invalid_request(
                        format!("server/discover rejected with HTTP {status}"),
                        None,
                    ),
                    Some(request.id.clone()),
                ),
                None,
            ));
        }
        if let Some(error) = status_error {
            return Err(StreamableHttpError::Client(error));
        }
        Err(StreamableHttpError::UnexpectedServerResponse(
            "invalid MCP HTTP response".into(),
        ))
    }
}

impl StreamableHttpClient for BoundedHttpClient {
    type Error = reqwest::Error;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, HttpError> {
        let mut request = self.inner.post(uri.as_ref()).timeout(self.timeout).header(
            reqwest::header::ACCEPT,
            "text/event-stream, application/json",
        );
        if let Some(auth) = auth_header {
            request = request.bearer_auth(auth);
        }
        for (name, value) in custom_headers {
            // Match rmcp's reserved-header validation; protocol-version is
            // intentionally allowed because the worker injects it after init.
            if matches!(name.as_str(), "accept" | "mcp-session-id" | "last-event-id") {
                return Err(StreamableHttpError::ReservedHeaderConflict(
                    name.to_string(),
                ));
            }
            request = request.header(name, value);
        }
        let attached = session_id.is_some();
        if let Some(session) = session_id {
            request = request.header("mcp-session-id", session.as_ref());
        }
        let response = request
            .json(&message)
            .send()
            .await
            .map_err(|error| StreamableHttpError::Client(error.without_url()))?;
        Self::parse_post_response(response, &message, attached).await
    }

    // The worker opens this stream in a task that close aborts, and never
    // reconnects, so only the raw event size needs bounding here.
    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxStream<'static, Result<sse_stream::Sse, sse_stream::Error>>, HttpError> {
        self.inner
            .get_stream_with_max_sse_event_size(
                uri,
                session_id,
                last_event_id,
                auth_header,
                custom_headers,
                MAX_MESSAGE_BYTES,
            )
            .await
            .map_err(redact_http_error)
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), HttpError> {
        self.inner
            .delete_session(uri, session_id, auth_header, custom_headers)
            .await
            .map_err(redact_http_error)
    }
}

async fn http_connect(
    client: BoundedHttpClient,
    config: StreamableHttpClientTransportConfig,
) -> Result<Client, Box<ClientInitializeError>> {
    ().serve(StreamableHttpClientTransport::with_client(client, config))
        .await
        .map_err(Box::new)
}

enum ReadinessError {
    Unreachable,
    Failed(McpError),
}

/// Retry only establishment, before any tool has been advertised or invoked.
/// The first wait also gives the just-launched process time to open its listener.
async fn wait_for_readiness<T, Attempt, Wait>(
    mut attempt: impl FnMut() -> Attempt,
    mut wait: impl FnMut() -> Wait,
) -> Result<T, McpError>
where
    Attempt: Future<Output = Result<T, ReadinessError>>,
    Wait: Future<Output = ()>,
{
    loop {
        wait().await;
        match attempt().await {
            Ok(service) => return Ok(service),
            Err(ReadinessError::Unreachable) => {}
            Err(ReadinessError::Failed(error)) => return Err(error),
        }
    }
}

/// The caller supplies an outer startup deadline and retains ownership outside
/// that future, so timeout/cancellation still explicitly kills and reaps children.
pub(crate) async fn connect(
    config: &McpServerConfig,
    workspace: &Path,
    process: &mut Option<OwnedProcess>,
) -> Result<Client, McpError> {
    match config.connection() {
        McpConnection::Stdio(command) => {
            *process = Some(OwnedProcess::spawn(command, workspace, true)?);
            let child = &mut process.as_mut().expect("process just created").child;
            let stdout = child.stdout.take().expect("piped stdout");
            let stdin = child.stdin.take().expect("piped stdin");
            let transport = rmcp::transport::async_rw::AsyncRwTransport::new_client(stdout, stdin)
                .with_max_line_length(MAX_MESSAGE_BYTES);
            ().serve(transport).await.map_err(initialize_error)
        }
        McpConnection::Http {
            endpoint,
            headers,
            start_if_unreachable,
        } => {
            let transport = http_config(endpoint, headers).await?;
            let client = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .connect_timeout(CONNECT_TIMEOUT)
                .build()
                .map_err(|error| http_error(&error))?;
            let client = BoundedHttpClient {
                inner: client,
                timeout: config.call_timeout().max(config.startup_timeout()),
            };
            let command = match http_connect(client.clone(), transport.clone()).await {
                Ok(service) => return Ok(service),
                Err(error) if unreachable(&error) => match start_if_unreachable {
                    Some(command) => command,
                    None => {
                        return Err(initialize_error(*error));
                    }
                },
                Err(error) => return Err(initialize_error(*error)),
            };
            *process = Some(OwnedProcess::spawn(command, workspace, false)?);
            wait_for_readiness(
                || async {
                    http_connect(client.clone(), transport.clone())
                        .await
                        .map_err(|error| {
                            if unreachable(&error) {
                                ReadinessError::Unreachable
                            } else {
                                ReadinessError::Failed(initialize_error(*error))
                            }
                        })
                },
                || tokio::time::sleep(READINESS_POLL),
            )
            .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::bounded;
    use tokio::io::AsyncReadExt;

    /// Headers are read when the connection starts: literals as written, commands run,
    /// and a missing environment value refuses the connection.
    #[tokio::test]
    async fn http_headers_read_each_source_when_the_connection_starts() {
        let config = |headers: &str| -> ConfiguredHeaders {
            let text = format!(
                "transport: streamable_http\nurl: http://localhost:1/mcp\nheaders: {{{headers}}}"
            );
            let config: McpServerConfig = crate::yaml::parse(&text).unwrap();
            match config.connection() {
                McpConnection::Http { headers, .. } => headers.clone(),
                McpConnection::Stdio(_) => unreachable!("an HTTP server"),
            }
        };
        let read = config("x-literal: plain, x-command: {command: 'printf made'}");
        let transport = http_config("http://localhost:1/mcp", &read).await.unwrap();
        let sent =
            |name: &'static str| transport.custom_headers[&HeaderName::from_static(name)].clone();
        assert_eq!(
            (sent("x-literal"), sent("x-command")),
            ("plain".parse().unwrap(), "made".parse().unwrap())
        );
        let missing = config("x-missing: {env: SKYHOOK_MCP_TEST_UNSET_47213}");
        let refused = http_config("http://localhost:1/mcp", &missing).await;
        assert!(matches!(refused, Err(McpError::Header(_))));
    }

    /// Every attempt follows a wait; only an unreachable server is tried again.
    #[tokio::test]
    async fn readiness_waits_before_each_attempt_and_retries_only_unreachable() {
        use ReadinessError::{Failed, Unreachable};
        for (outcomes, ready) in [
            (vec![Err(Unreachable), Err(Unreachable), Ok(())], true),
            (
                vec![
                    Err(Unreachable),
                    Err(Failed(McpError::AuthenticationRequired)),
                ],
                false,
            ),
        ] {
            let attempts = outcomes.len();
            let mut outcomes = outcomes.into_iter();
            let steps = std::cell::RefCell::new(Vec::new());
            let result = wait_for_readiness(
                || {
                    steps.borrow_mut().push("attempt");
                    std::future::ready(outcomes.next().unwrap())
                },
                // Recorded only once the wait, pending at first, has completed.
                || async {
                    tokio::task::yield_now().await;
                    steps.borrow_mut().push("waited");
                },
            )
            .await;
            assert_eq!(steps.into_inner(), ["waited", "attempt"].repeat(attempts));
            assert_eq!(result.is_ok(), ready);
        }
    }

    #[tokio::test]
    async fn http_transport_errors_strip_url_secrets_before_sdk_logging() {
        let port = crate::tests::RefusedPort::new();
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let url = format!("http://{}/mcp?token=private-secret", port.address());
        let error = client.get(url).send().await.unwrap_err();
        let was_connect_error = error.is_connect();
        let error = redact_http_error(StreamableHttpError::Client(error));
        assert!(!error.to_string().contains("private-secret"));
        let StreamableHttpError::Client(error) = error else {
            panic!("preserves error type")
        };
        assert!(error.url().is_none());
        assert_eq!(error.is_connect(), was_connect_error);
    }

    #[tokio::test]
    async fn raw_http_limit_counts_across_chunks_and_allows_exact_limit() {
        for (limit, accepted) in [(4, true), (3, false)] {
            let chunks = futures_util::stream::iter([
                Ok::<_, io::Error>(b"ab".to_vec()),
                Ok(b"cd".to_vec()),
            ]);
            let collected = bounded_bytes(chunks, limit).try_collect::<Vec<_>>().await;
            assert_eq!(
                collected.map(|chunks| chunks.len()).ok(),
                accepted.then_some(2)
            );
        }
    }

    // Chunked encoding intentionally omits Content-Length so the tests exercise
    // streaming enforcement rather than the early header-size check.
    async fn http_fixture(
        status: &str,
        content_type: &str,
        body: Vec<u8>,
    ) -> (Arc<str>, tokio::task::JoinHandle<()>) {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let uri: Arc<str> = format!("http://{}/mcp", listener.local_addr().unwrap()).into();
        let headers = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n",
            body.len()
        );
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            assert!(socket.read(&mut request).await.unwrap() > 0);
            // Early rejection may close the connection while we're writing.
            if socket.write_all(headers.as_bytes()).await.is_ok()
                && socket.write_all(&body).await.is_ok()
            {
                let _ = socket.write_all(b"\r\n0\r\n\r\n").await;
            }
        });
        (uri, server)
    }

    fn test_http_client() -> BoundedHttpClient {
        BoundedHttpClient {
            inner: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .build()
                .unwrap(),
            // Never fires on its own; a test expires it or `bounded` guards the call.
            timeout: Duration::from_secs(3600),
        }
    }

    fn ping_message() -> ClientJsonRpcMessage {
        serde_json::from_value(serde_json::json!({"jsonrpc":"2.0", "id":1, "method":"ping"}))
            .unwrap()
    }

    #[tokio::test]
    async fn http_json_and_error_bodies_are_bounded_before_parsing() {
        let oversized = vec![b'x'; MAX_MESSAGE_BYTES + 1];
        let small = br#"{"jsonrpc":"2.0","id":1,"result":{}}"#.to_vec();
        for (status, content_type, body) in [
            ("200 OK", "application/json", oversized.clone()),
            ("500 Error", "text/plain", oversized),
            ("200 OK", "application/json", small),
        ] {
            let accepted = body.len() <= MAX_MESSAGE_BYTES;
            let (uri, server) = http_fixture(status, content_type, body).await;
            let result = test_http_client()
                .post_message(uri, ping_message(), None, None, HashMap::new())
                .await;
            if accepted {
                assert!(matches!(result, Ok(StreamableHttpPostResponse::Json(..))));
            } else {
                assert!(
                    matches!(result, Err(StreamableHttpError::UnexpectedServerResponse(ref text)) if text.contains("byte limit"))
                );
            }
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn http_post_and_get_bound_sse_before_an_event_terminator() {
        for post in [true, false] {
            let body = vec![b'x'; MAX_MESSAGE_BYTES + 1];
            let (uri, server) = http_fixture("200 OK", "text/event-stream", body).await;
            let client = test_http_client();
            let mut stream = if post {
                let response = client
                    .post_message(uri, ping_message(), None, None, HashMap::new())
                    .await
                    .unwrap();
                let StreamableHttpPostResponse::Sse(stream, _) = response else {
                    panic!("expected SSE")
                };
                stream
            } else {
                client
                    .get_stream(uri, None, None, None, HashMap::new())
                    .await
                    .unwrap()
            };
            assert!(stream.next().await.unwrap().is_err());
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn malformed_https_endpoint_is_not_launchable() {
        let (uri, server) = http_fixture("200 OK", "text/plain", b"not TLS".to_vec()).await;
        let uri = uri.replacen("http:", "https:", 1);
        let mut config = StreamableHttpClientTransportConfig::with_uri(uri);
        config.reinit_on_expired_session = false;
        config.retry_config = Arc::new(NeverRetry::default());
        let result = bounded(http_connect(test_http_client(), config)).await;
        let Err(error) = result else {
            panic!("plaintext endpoint unexpectedly negotiated TLS")
        };
        assert!(!unreachable(&error));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn http_post_has_a_transport_deadline() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let uri: Arc<str> = format!("http://{}/mcp", listener.local_addr().unwrap()).into();
        let (accepted, connected) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (_socket, _) = listener.accept().await.unwrap();
            accepted.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        let client = test_http_client();
        let post = client.post_message(uri, ping_message(), None, None, HashMap::new());
        let (result, _) = crate::tests::expire(post, connected, client.timeout).await;
        assert!(
            matches!(result, Err(StreamableHttpError::Client(ref error)) if error.is_timeout())
        );
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }
}
