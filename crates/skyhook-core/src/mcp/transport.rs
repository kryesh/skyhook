//! Root-session owned MCP transports. No shell interpolation or implicit reconnect/replay.

use std::{
    collections::HashMap,
    io,
    pin::Pin,
    process::Stdio,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use futures_util::{Stream, StreamExt, TryStreamExt, stream::BoxStream};
use reqwest::header::{HeaderName, HeaderValue};
use rmcp::{
    RoleClient, ServiceExt,
    model::{ClientJsonRpcMessage, ClientRequest, ErrorData, JsonRpcMessage, ServerJsonRpcMessage},
    service::{ClientInitializeError, RunningService},
    transport::{
        StreamableHttpClientTransport,
        common::client_side_sse::NeverRetry,
        streamable_http_client::{
            StreamableHttpClient, StreamableHttpClientTransportConfig, StreamableHttpError,
            StreamableHttpPostResponse,
        },
    },
};
use tokio::{
    io::{AsyncRead, ReadBuf},
    process::{Child, Command},
};

use super::{
    config::{McpServerConfig, McpTransport},
    manager::McpError,
};

pub(crate) type Client = RunningService<RoleClient, ()>;

// Bound raw stdio frames before rmcp's default (unbounded) read_until buffer.
// Keep the counter in the reader so cancelled receive futures cannot reset it.
pub(crate) const MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;
struct BoundedLines<R> {
    inner: R,
    length: usize,
    failed: bool,
}

impl<R> BoundedLines<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            length: 0,
            failed: false,
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for BoundedLines<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.failed {
            return Poll::Ready(Err(io::Error::other("MCP frame limit exceeded")));
        }
        let before = buf.filled().len();
        match Pin::new(&mut this.inner).poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                for byte in &buf.filled()[before..] {
                    if *byte == b'\n' {
                        this.length = 0;
                    } else {
                        this.length += 1;
                        if this.length > MAX_MESSAGE_BYTES {
                            this.failed = true;
                            buf.set_filled(before);
                            return Poll::Ready(Err(io::Error::other("MCP frame limit exceeded")));
                        }
                    }
                }
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

/// Only processes created by this session enter this type. On Unix each command
/// is the leader of a fresh process group, so shutdown also reaches descendants.
pub(crate) struct OwnedProcess {
    child: Child,
    #[cfg(unix)]
    group: Option<i32>,
}

impl OwnedProcess {
    fn spawn(config: &McpServerConfig, piped: bool) -> Result<Self, McpError> {
        let argv = config
            .start_command
            .as_ref()
            .filter(|args| !args.is_empty())
            .ok_or_else(|| {
                McpError::Configuration("start_command must contain an executable".into())
            })?;
        let mut command = Command::new(&argv[0]);
        command
            .args(&argv[1..])
            .envs(&config.env)
            .kill_on_drop(true);
        if let Some(cwd) = &config.cwd {
            command.current_dir(cwd);
        }
        command.stdin(if piped { Stdio::piped() } else { Stdio::null() });
        command.stdout(if piped { Stdio::piped() } else { Stdio::null() });
        // Never leave an unread stderr pipe that could deadlock initialization.
        command.stderr(Stdio::null());
        #[cfg(unix)]
        command.process_group(0);
        let child = command
            .spawn()
            .map_err(|e| McpError::Startup(format!("could not spawn MCP command: {}", e.kind())))?;
        #[cfg(unix)]
        let group = child.id().and_then(|id| i32::try_from(id).ok());
        Ok(Self {
            child,
            #[cfg(unix)]
            group,
        })
    }

    fn kill_group(&mut self) {
        #[cfg(unix)]
        if let Some(group) = self.group.take() {
            // SAFETY: group is the positive PID of our unreaped child, created
            // with process_group(0). Negative PID targets only its process group.
            unsafe {
                libc::kill(-group, libc::SIGKILL);
            }
        }
        let _ = self.child.start_kill();
    }

    pub(crate) async fn shutdown(&mut self) {
        self.kill_group();
        let _ = self.child.wait().await;
    }
}

impl Drop for OwnedProcess {
    fn drop(&mut self) {
        // Explicit manager shutdown awaits wait(). Tokio's Child drop performs
        // best-effort reaping as a fallback for a cancelled owner future.
        self.kill_group();
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

fn http_config(config: &McpServerConfig) -> Result<StreamableHttpClientTransportConfig, McpError> {
    let url = config
        .url
        .as_deref()
        .ok_or_else(|| McpError::Configuration("HTTP transport requires url".into()))?;
    let parsed =
        reqwest::Url::parse(url).map_err(|_| McpError::Configuration("invalid MCP URL".into()))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(McpError::Configuration(
            "MCP URL must use http or https".into(),
        ));
    }
    let mut transport = StreamableHttpClientTransportConfig::with_uri(url.to_owned());
    // A call with an uncertain outcome must never be automatically sent again.
    transport.reinit_on_expired_session = false;
    transport.retry_config = Arc::new(NeverRetry::default());
    transport.max_sse_event_size = MAX_MESSAGE_BYTES;
    for (header, variable) in &config.headers_env {
        let name = HeaderName::from_bytes(header.as_bytes())
            .map_err(|_| McpError::Configuration("invalid MCP header name".into()))?;
        let value = std::env::var(variable).map_err(|_| {
            McpError::Configuration(format!(
                "MCP header environment variable {variable:?} is missing or not Unicode"
            ))
        })?;
        let mut value = HeaderValue::from_str(&value).map_err(|_| {
            McpError::Configuration(format!(
                "invalid value in MCP header environment variable {variable:?}"
            ))
        })?;
        value.set_sensitive(true);
        transport.custom_headers.insert(name, value);
    }
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
        max_sse_event_size: usize,
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
            let bytes = bounded_bytes(
                response.bytes_stream(),
                MAX_MESSAGE_BYTES.min(max_sse_event_size),
            );
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
        Err(StreamableHttpError::UnexpectedServerResponse(
            format!("invalid MCP HTTP response (status {status})").into(),
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
        self.post_message_with_max_sse_event_size(
            uri,
            message,
            session_id,
            auth_header,
            custom_headers,
            MAX_MESSAGE_BYTES,
        )
        .await
    }

    async fn post_message_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
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
        Self::parse_post_response(response, &message, attached, max_sse_event_size).await
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxStream<'static, Result<sse_stream::Sse, sse_stream::Error>>, HttpError> {
        self.get_stream_with_max_sse_event_size(
            uri,
            session_id,
            last_event_id,
            auth_header,
            custom_headers,
            MAX_MESSAGE_BYTES,
        )
        .await
    }

    async fn get_stream_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<BoxStream<'static, Result<sse_stream::Sse, sse_stream::Error>>, HttpError> {
        // Bound establishment, not the lifetime of the long-lived GET stream.
        // The built-in adapter applies its raw per-event limiter before parsing.
        tokio::time::timeout(
            self.timeout,
            self.inner.get_stream_with_max_sse_event_size(
                uri,
                session_id,
                last_event_id,
                auth_header,
                custom_headers,
                MAX_MESSAGE_BYTES.min(max_sse_event_size),
            ),
        )
        .await
        .map_err(|_| {
            StreamableHttpError::UnexpectedServerResponse(
                "MCP HTTP stream establishment timed out".into(),
            )
        })?
        .map_err(redact_http_error)
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), HttpError> {
        tokio::time::timeout(
            self.timeout,
            self.inner
                .delete_session(uri, session_id, auth_header, custom_headers),
        )
        .await
        .map_err(|_| {
            StreamableHttpError::UnexpectedServerResponse(
                "MCP HTTP session deletion timed out".into(),
            )
        })?
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

/// The caller supplies an outer startup deadline and retains ownership outside
/// that future, so timeout/cancellation still explicitly kills and reaps children.
pub(crate) async fn connect(
    config: &McpServerConfig,
    process: &mut Option<OwnedProcess>,
) -> Result<Client, McpError> {
    match config.transport {
        McpTransport::Stdio => {
            *process = Some(OwnedProcess::spawn(config, true)?);
            let child = &mut process.as_mut().expect("process just created").child;
            let stdout = child.stdout.take().expect("piped stdout");
            let stdin = child.stdin.take().expect("piped stdin");
            ().serve((BoundedLines::new(stdout), stdin))
                .await
                .map_err(|_| McpError::Startup("stdio MCP initialization failed".into()))
        }
        McpTransport::StreamableHttp => {
            let transport = http_config(config)?;
            let client = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .connect_timeout(Duration::from_secs(3))
                .build()
                .map_err(|_| McpError::Startup("could not build MCP HTTP client".into()))?;
            let client = BoundedHttpClient {
                inner: client,
                timeout: Duration::from_secs(
                    config.call_timeout_secs.max(config.startup_timeout_secs),
                ),
            };
            match http_connect(client.clone(), transport.clone()).await {
                Ok(service) => return Ok(service),
                Err(error) if unreachable(&error) && config.start_command.is_some() => {}
                Err(_) => {
                    return Err(McpError::Startup(
                        "HTTP MCP initialization failed (server not launchable)".into(),
                    ));
                }
            }
            *process = Some(OwnedProcess::spawn(config, false)?);
            // Retry only establishment, before any tool has been advertised or
            // invoked. Authentication/protocol failures stop readiness immediately.
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                match http_connect(client.clone(), transport.clone()).await {
                    Ok(service) => return Ok(service),
                    Err(error) if unreachable(&error) => {}
                    Err(_) => {
                        return Err(McpError::Startup(
                            "launched HTTP MCP server failed initialization".into(),
                        ));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn http_transport_errors_strip_url_secrets_before_sdk_logging() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let error = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("http://{address}/mcp?token=private-secret"))
            .send()
            .await
            .unwrap_err();
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
        let chunks =
            futures_util::stream::iter([Ok::<_, io::Error>(b"ab".to_vec()), Ok(b"cd".to_vec())]);
        assert_eq!(
            bounded_bytes(chunks, 4)
                .try_collect::<Vec<_>>()
                .await
                .unwrap()
                .len(),
            2
        );
        let chunks =
            futures_util::stream::iter([Ok::<_, io::Error>(b"ab".to_vec()), Ok(b"cd".to_vec())]);
        assert!(
            bounded_bytes(chunks, 3)
                .try_collect::<Vec<_>>()
                .await
                .is_err()
        );
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
            timeout: Duration::from_secs(5),
        }
    }

    fn ping_message() -> ClientJsonRpcMessage {
        serde_json::from_value(serde_json::json!({"jsonrpc":"2.0", "id":1, "method":"ping"}))
            .unwrap()
    }

    #[tokio::test]
    async fn http_json_and_error_bodies_are_bounded_before_parsing() {
        for (status, content_type) in [("200 OK", "application/json"), ("500 Error", "text/plain")]
        {
            let (uri, server) =
                http_fixture(status, content_type, vec![b'x'; MAX_MESSAGE_BYTES + 1]).await;
            let result = test_http_client()
                .post_message(uri, ping_message(), None, None, HashMap::new())
                .await;
            assert!(
                matches!(result, Err(StreamableHttpError::UnexpectedServerResponse(ref text)) if text.contains("byte limit"))
            );
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn http_small_json_is_accepted() {
        let (uri, server) = http_fixture(
            "200 OK",
            "application/json",
            br#"{"jsonrpc":"2.0","id":1,"result":{}}"#.to_vec(),
        )
        .await;
        let result = test_http_client()
            .post_message(uri, ping_message(), None, None, HashMap::new())
            .await;
        assert!(matches!(result, Ok(StreamableHttpPostResponse::Json(..))));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn http_post_and_get_bound_sse_before_an_event_terminator() {
        for post in [true, false] {
            let (uri, server) = http_fixture("200 OK", "text/event-stream", vec![b'x'; 100]).await;
            let client = test_http_client();
            let mut stream = if post {
                match client
                    .post_message_with_max_sse_event_size(
                        uri,
                        ping_message(),
                        None,
                        None,
                        HashMap::new(),
                        32,
                    )
                    .await
                    .unwrap()
                {
                    StreamableHttpPostResponse::Sse(stream, _) => stream,
                    _ => panic!("expected SSE"),
                }
            } else {
                client
                    .get_stream_with_max_sse_event_size(uri, None, None, None, HashMap::new(), 32)
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
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            http_connect(test_http_client(), config),
        )
        .await
        .unwrap();
        match result {
            Err(error) => assert!(!unreachable(&error)),
            Ok(_) => panic!("plaintext endpoint unexpectedly negotiated TLS"),
        }
        server.await.unwrap();
    }

    #[tokio::test]
    async fn http_post_has_a_transport_deadline() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let uri: Arc<str> = format!("http://{}/mcp", listener.local_addr().unwrap()).into();
        let server = tokio::spawn(async move {
            let (_socket, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });
        let mut client = test_http_client();
        client.timeout = Duration::from_millis(25);
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            client.post_message(uri, ping_message(), None, None, HashMap::new()),
        )
        .await
        .unwrap();
        assert!(
            matches!(result, Err(StreamableHttpError::Client(ref error)) if error.is_timeout())
        );
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }

    #[tokio::test]
    async fn stdio_frame_limit_applies_before_newline_and_stays_failed() {
        let data = vec![b'x'; MAX_MESSAGE_BYTES + 1];
        let mut reader = BoundedLines::new(data.as_slice());
        let mut output = Vec::new();
        assert!(reader.read_to_end(&mut output).await.is_err());
        let mut byte = [0; 1];
        assert!(reader.read(&mut byte).await.is_err());
    }

    #[tokio::test]
    async fn stdio_frame_counter_resets_per_line_not_per_read() {
        let mut data = vec![b'x'; MAX_MESSAGE_BYTES];
        data.push(b'\n');
        data.extend_from_slice(b"next line\n");
        let mut reader = BoundedLines::new(data.as_slice());
        let mut output = Vec::new();
        reader.read_to_end(&mut output).await.unwrap();
        assert_eq!(output, data);
    }

    #[tokio::test]
    async fn cancelled_stdio_read_keeps_partial_frame_limit() {
        use tokio::io::AsyncWriteExt;
        let (input, mut output) = tokio::io::duplex(32);
        let mut reader = BoundedLines::new(input);
        reader.length = MAX_MESSAGE_BYTES - 1;
        output.write_all(b"x").await.unwrap();
        let mut byte = [0; 1];
        reader.read_exact(&mut byte).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), reader.read_exact(&mut byte))
                .await
                .is_err()
        );
        output.write_all(b"x").await.unwrap();
        assert!(reader.read_exact(&mut byte).await.is_err());
    }
}
