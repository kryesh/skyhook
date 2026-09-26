//! Shared, cancellation-safe HTTP/SSE transport. A dropped stream drops its HTTP body.
use super::Timeouts;
use crate::provider::{ProviderError, ProviderErrorKind};
use reqwest::{
    Client, Response,
    header::{CONTENT_TYPE, HeaderMap, RETRY_AFTER},
};
use serde_json::Value;
use std::time::Duration;

mod sse;
use sse::response_stream;
pub(crate) use sse::{SseEvent, SseStream};

const MAX_ERROR_BYTES: usize = 16 * 1024;

/// The media type requested and read.
pub(crate) const EVENT_STREAM: &str = "text/event-stream";

pub(crate) fn client() -> Result<Client, ProviderError> {
    Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        // The runtime owns retries; disable automatic protocol-nack retries here.
        .retry(reqwest::retry::never())
        .build()
        .map_err(http_error)
}

pub(crate) fn http_error(error: reqwest::Error) -> ProviderError {
    ProviderError {
        kind: if error.is_builder() {
            ProviderErrorKind::InvalidRequest
        } else if error.is_timeout() {
            ProviderErrorKind::Timeout
        } else {
            ProviderErrorKind::Transport
        },
        // Do not expose request URLs: they can contain caller-provided secrets.
        message: error.without_url().to_string(),
    }
}

fn timeout_error(phase: &str) -> ProviderError {
    ProviderError {
        kind: ProviderErrorKind::Timeout,
        message: format!("provider HTTP {phase} timeout"),
    }
}

/// A non-success response, for the codec to read.
#[derive(Debug)]
pub(crate) struct Rejection {
    pub status: u16,
    /// The JSON body, or its text when it is not JSON.
    pub body: Value,
    pub retry_after: Option<Duration>,
}

impl Rejection {
    pub(crate) fn unauthorized(&self) -> bool {
        self.status == reqwest::StatusCode::UNAUTHORIZED
    }
}

/// Why an attempt produced no event stream.
#[derive(Debug)]
pub(crate) enum Failure {
    Rejected(Rejection),
    Other(ProviderError),
}

impl From<ProviderError> for Failure {
    fn from(error: ProviderError) -> Self {
        Self::Other(error)
    }
}

/// Make one HTTP/SSE attempt; the agent runtime owns retries. Returns the
/// response headers with the stream. Dropping the stream cancels its body.
pub(crate) async fn post_sse(
    client: &Client,
    url: &str,
    headers: HeaderMap,
    body: &Value,
    timeouts: Timeouts,
) -> Result<(HeaderMap, SseStream), Failure> {
    let deadline = tokio::time::Instant::now() + timeouts.startup;
    let sent = tokio::time::timeout_at(
        deadline,
        client.post(url).headers(headers).json(body).send(),
    )
    .await;
    let response = match sent {
        Ok(Ok(response)) => response,
        Err(_) => return Err(timeout_error("startup").into()),
        Ok(Err(error)) => return Err(http_error(error).into()),
    };
    if !response.status().is_success() {
        let rejection = rejection(response, deadline, timeouts.read_idle).await;
        return Err(Failure::Rejected(rejection));
    }
    // Some services stream events without labelling them; only a declared
    // other type is refused, and the event parser judges the rest.
    let declared = response.headers().get(CONTENT_TYPE);
    if declared.is_some_and(|header| {
        let essence = header.to_str().unwrap_or("").split(';').next();
        !essence
            .unwrap_or("")
            .trim()
            .eq_ignore_ascii_case(EVENT_STREAM)
    }) {
        return Err(ProviderError::protocol("provider returned a non-SSE content type").into());
    }
    let headers = response.headers().clone();
    Ok((headers, response_stream(response, timeouts.read_idle)))
}

/// Parse a single Retry-After field without retaining server-controlled text.
/// HTTP-date and delay-seconds are both supported; past dates mean no delay.
/// Ambiguous, malformed, or unrepresentable delays fall back to runtime backoff.
pub(crate) fn retry_after(headers: &HeaderMap, now: std::time::SystemTime) -> Option<Duration> {
    let mut values = headers.get_all(RETRY_AFTER).iter();
    let value = values.next()?.to_str().ok()?.trim();
    if values.next().is_some() {
        return None;
    }
    let delay = if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        Duration::from_secs(value.parse().ok()?)
    } else {
        httpdate::parse_http_date(value)
            .ok()?
            .duration_since(now)
            .unwrap_or_default()
    };
    // A maliciously large value must not overflow the timer implementation.
    std::time::Instant::now().checked_add(delay)?;
    Some(delay)
}

async fn rejection(
    mut response: Response,
    startup_deadline: tokio::time::Instant,
    read_idle: Duration,
) -> Rejection {
    let status = response.status().as_u16();
    let retry_after = retry_after(response.headers(), std::time::SystemTime::now());
    let mut body = Vec::new();
    // Error diagnostics must not extend the startup budget. Keep both a small
    // total diagnostic cap and the configured per-read idle limit.
    let deadline = startup_deadline.min(tokio::time::Instant::now() + Duration::from_secs(5));
    let _ = tokio::time::timeout_at(deadline, async {
        while body.len() < MAX_ERROR_BYTES {
            match tokio::time::timeout(read_idle, response.chunk()).await {
                Ok(Ok(Some(chunk))) => {
                    body.extend_from_slice(&chunk[..chunk.len().min(MAX_ERROR_BYTES - body.len())])
                }
                _ => break,
            }
        }
    })
    .await;
    // Non-JSON bodies (proxy HTML, plain text) still carry the explanation.
    let body = serde_json::from_slice(&body)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&body).into_owned()));
    Rejection {
        status,
        body,
        retry_after,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use futures_util::StreamExt;

    /// A failure as a Chat provider reports it.
    fn reported(failure: Failure) -> ProviderError {
        use crate::provider::{codec::Codec, http::errors::ErrorSignals};
        match failure {
            Failure::Rejected(rejection) => {
                let chat = crate::provider::codec::chat_completions::Dialect::compatible();
                Codec::ChatCompletions(chat).error(&rejection, ErrorSignals::NONE)
            }
            Failure::Other(error) => error,
        }
    }

    #[test]
    fn retry_after_accepts_seconds_and_http_dates_without_capping_server_delays() {
        let now = std::time::UNIX_EPOCH + Duration::from_secs(784111717);
        let parse = |values: &[&str]| {
            let mut headers = HeaderMap::new();
            for value in values {
                headers.append(RETRY_AFTER, value.parse().unwrap());
            }
            retry_after(&headers, now)
        };
        for (value, expected) in [
            ("0", Some(0)),
            ("1", Some(1)),
            ("120", Some(120)),
            ("Sun, 06 Nov 1994 08:49:37 GMT", Some(60)),
            ("Sunday, 06-Nov-94 08:49:37 GMT", Some(60)),
            ("Sun Nov  6 08:49:37 1994", Some(60)),
            ("Sun, 06 Nov 1994 08:47:37 GMT", Some(0)),
            ("", None),
            ("-1", None),
            ("+1", None),
            ("1.5", None),
            ("10, 20", None),
            ("invalid", None),
            ("18446744073709551615", None),
        ] {
            assert_eq!(
                parse(&[value]),
                expected.map(Duration::from_secs),
                "{value}"
            );
        }
        assert_eq!(parse(&["5", "10"]), None);
    }

    /// Every value of `wanted` in a raw request head, in wire order.
    pub(crate) fn header_values(head: &str, wanted: &str) -> Vec<String> {
        head.lines()
            .filter_map(|line| line.split_once(':'))
            .filter(|(header, _)| header.eq_ignore_ascii_case(wanted))
            .map(|(_, value)| value.trim().to_owned())
            .collect()
    }

    pub(crate) fn reply(status: &str, headers: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: {}\r\n{headers}\r\n{body}",
            body.len()
        )
    }
    pub(crate) async fn read_request(socket: &mut tokio::net::TcpStream) -> String {
        use tokio::io::AsyncReadExt;
        let mut bytes = Vec::new();
        loop {
            let mut buffer = [0u8; 4096];
            let n = socket.read(&mut buffer).await.unwrap();
            if n == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..n]);
            if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&bytes[..end]);
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .and_then(|s| s.parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if bytes.len() >= end + 4 + length {
                    break;
                }
            }
        }
        String::from_utf8(bytes).unwrap()
    }

    /// One scripted connection. Connections beyond the script never answer.
    pub(crate) struct Plan {
        wire: Option<String>,
        pub(super) delay: Duration,
        /// Hold the connection open after writing instead of closing it.
        pub(super) stall_body: bool,
    }

    impl Plan {
        pub(crate) fn reply(wire: String) -> Self {
            Self {
                wire: Some(wire),
                delay: Duration::ZERO,
                stall_body: false,
            }
        }

        /// An SSE response that ends at once, for tests that read only the request.
        pub(crate) fn empty_stream() -> Self {
            Self::reply(reply("200 OK", "Content-Type: text/event-stream\r\n", ""))
        }

        pub(super) fn stalled_headers() -> Self {
            Self {
                wire: None,
                delay: Duration::ZERO,
                stall_body: true,
            }
        }
    }

    /// Scripted HTTP fixture which keeps accepting, so replays are observed.
    pub(crate) struct Server {
        pub(crate) url: String,
        address: std::net::SocketAddr,
        requests: tokio::sync::mpsc::UnboundedReceiver<String>,
        /// Connections accepted, against requests taken by the test.
        accepted: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        taken: usize,
        task: tokio::task::JoinHandle<()>,
    }

    impl Server {
        pub(crate) async fn start(plans: Vec<Plan>) -> Self {
            use tokio::io::AsyncWriteExt;
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let url = format!("http://{address}/responses");
            let (tx, requests) = tokio::sync::mpsc::unbounded_channel();
            let accepted = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let count = accepted.clone();
            let task = tokio::spawn(async move {
                let plans = std::sync::Arc::new(std::sync::Mutex::new(plans.into_iter()));
                let mut connections = tokio::task::JoinSet::new();
                loop {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let (tx, plans) = (tx.clone(), plans.clone());
                    connections.spawn(async move {
                        let request = read_request(&mut socket).await;
                        // A connection without a request (the probe below) uses no plan.
                        let plan = (!request.is_empty()).then(|| plans.lock().unwrap().next());
                        let plan = plan.flatten().unwrap_or_else(Plan::stalled_headers);
                        tx.send(request).unwrap();
                        tokio::time::sleep(plan.delay).await;
                        if let Some(wire) = plan.wire {
                            // Timed-out attempts may close the connection before writing.
                            let _ = socket.write_all(wire.as_bytes()).await;
                        }
                        if plan.stall_body {
                            std::future::pending::<()>().await;
                        }
                    });
                }
            });
            Self {
                url,
                address,
                requests,
                accepted,
                taken: 0,
                task,
            }
        }

        pub(crate) async fn request(&mut self) -> String {
            self.taken += 1;
            tokio::time::timeout(Duration::from_secs(3), self.requests.recv())
                .await
                .expect("expected HTTP request")
                .unwrap()
        }

        /// Requests not yet taken, once the client's calls have returned. An empty
        /// probe connection flushes the accept queue, so every connection the client
        /// opened is counted, even a replay that never completed its request.
        pub(crate) async fn unclaimed(&mut self) -> Vec<String> {
            drop(tokio::net::TcpStream::connect(self.address).await.unwrap());
            let mut requests = Vec::new();
            loop {
                match self.request().await {
                    probe if probe.is_empty() => break,
                    request => requests.push(request),
                }
            }
            let accepted = self.accepted.load(std::sync::atomic::Ordering::SeqCst);
            assert_eq!(accepted, self.taken, "unexpected connection");
            requests
        }

        pub(crate) async fn finish(mut self) -> Vec<String> {
            self.unclaimed().await
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            // Aborting the accept task drops its JoinSet and all held sockets.
            self.task.abort();
        }
    }

    pub(super) fn timeouts(startup_ms: u64, read_idle_ms: u64) -> Timeouts {
        Timeouts {
            startup: Duration::from_millis(startup_ms),
            read_idle: Duration::from_millis(read_idle_ms),
        }
    }

    /// Posts once to a single-plan server, asserting one request and no replay.
    async fn rejected(plan: Plan, timeouts: Timeouts) -> Failure {
        let mut server = Server::start(vec![plan]).await;
        let error = tokio::time::timeout(
            Duration::from_secs(2),
            post_sse(
                &client().unwrap(),
                &server.url,
                HeaderMap::new(),
                &Value::Null,
                timeouts,
            ),
        )
        .await
        .expect("rejection must not extend configured budgets")
        .err()
        .unwrap();
        server.request().await;
        assert!(server.finish().await.is_empty(), "unexpected replay");
        error
    }

    #[tokio::test]
    async fn wire_headers_body_and_eof() {
        // An unlabelled event stream is accepted as readily as a labelled one.
        for content_type in ["Content-Type: text/event-stream; charset=utf-8\r\n", ""] {
            wire_round_trip(content_type).await;
        }
    }

    async fn wire_round_trip(content_type: &str) {
        let body = "data: one\r\n\r\ndata: two";
        let server = Server::start(vec![Plan::reply(reply("200 OK", content_type, body))]).await;
        let body = serde_json::json!({"model":"literal-model", "stream":true});
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer test-key".parse().unwrap());
        let (_, events) = post_sse(
            &client().unwrap(),
            &server.url,
            headers,
            &body,
            Timeouts::default(),
        )
        .await
        .unwrap();
        let events = events
            .map(|event| event.map(|event| (event.event, event.data)))
            .collect::<Vec<_>>()
            .await;
        assert_eq!(
            events.into_iter().collect::<Result<Vec<_>, _>>().unwrap(),
            [(None, "one".to_owned()), (None, "two".to_owned())]
        );
        let requests = server.finish().await;
        assert_eq!(requests.len(), 1);
        assert!(requests[0].contains("authorization: Bearer test-key"));
        let wire: Value =
            serde_json::from_str(requests[0].split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(wire, body);
    }

    #[tokio::test]
    async fn http_rejections_carry_status_message_and_retry_after_without_replay() {
        use ProviderErrorKind::*;
        let json = "Content-Type: application/json\r\n";
        let refused = r#"{"error":{"message":"upstream rejected the request"}}"#;
        let context = r#"{"error":{"code":"context_length_exceeded","message":"prompt too long"}}"#;
        let throttle = r#"{"error":{"code":"rate_limit_exceeded","message":"slow down"}}"#;
        // (status, headers, body, kind, the server's explanation carried verbatim)
        for (status, headers, body, kind, explanation) in [
            (
                "429 Too Many Requests",
                "Retry-After: 120\r\n",
                throttle,
                RateLimited {
                    retry_after: Some(Duration::from_secs(120)),
                },
                ": slow down",
            ),
            // Non-JSON bodies (proxy text) still carry the explanation.
            (
                "503 Service Unavailable",
                "",
                "upstream unavailable",
                Unavailable { retry_after: None },
                ": upstream unavailable",
            ),
            (
                "500 Internal Server Error",
                "",
                refused,
                Unavailable { retry_after: None },
                ": upstream rejected the request",
            ),
            (
                "401 Unauthorized",
                json,
                refused,
                Authentication,
                ": upstream rejected the request",
            ),
            (
                "400 Bad Request",
                "",
                context,
                ContextWindowExceeded,
                ": prompt too long",
            ),
            ("200 OK", json, "{}", Protocol, "non-SSE content type"),
        ] {
            let failure = rejected(
                Plan::reply(reply(status, headers, body)),
                timeouts(100, 100),
            )
            .await;
            let unauthorized = matches!(&failure, Failure::Rejected(r) if r.unauthorized());
            assert_eq!(unauthorized, status.starts_with("401"), "{status}");
            let error = reported(failure);
            assert_eq!(error.kind, kind, "{status}");
            assert!(
                error.message.ends_with(explanation),
                "{status}: {}",
                error.message
            );
            assert_eq!(error.message.contains("429"), error.retry_after().is_some());
        }
        // An empty response closes the socket after consuming the full POST.
        let closed = reported(rejected(Plan::reply(String::new()), timeouts(100, 100)).await);
        assert_eq!(closed.kind, Transport);
        assert!(closed.is_retryable());
    }

    #[tokio::test]
    async fn stalled_rejection_body_respects_remaining_startup_and_idle_limits() {
        // Part of the startup budget is consumed before the rejection headers.
        for budget in [timeouts(200, 5000), timeouts(5000, 100)] {
            let head = "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: 999\r\n\r\n";
            let mut plan = Plan::reply(head.into());
            plan.delay = Duration::from_millis(100);
            plan.stall_body = true;
            // Preserve the known rejection classification even if diagnostics stall.
            let failure = rejected(plan, budget).await;
            assert!(matches!(&failure, Failure::Rejected(r) if r.unauthorized()));
            assert_eq!(reported(failure).kind, ProviderErrorKind::Authentication);
        }
    }

    #[tokio::test]
    async fn timeouts_are_one_attempt_and_a_new_call_gets_a_fresh_deadline() {
        let mut server = Server::start(vec![
            Plan::stalled_headers(),
            Plan::reply(reply(
                "200 OK",
                "Content-Type: text/event-stream\r\n",
                "data: done\n\n",
            )),
        ])
        .await;
        let (client, url) = (client().unwrap(), server.url.clone());
        let body = serde_json::json!({"model":"cold-model", "stream":true});
        let post = || post_sse(&client, &url, HeaderMap::new(), &body, timeouts(100, 100));
        let error = reported(post().await.err().unwrap());
        assert_eq!(error.kind, ProviderErrorKind::Timeout);
        assert_eq!(error.message, "provider HTTP startup timeout");
        let first = server.request().await;
        assert!(server.unclaimed().await.is_empty(), "unexpected replay");
        let (_, mut stream) = post().await.unwrap();
        assert_eq!(stream.next().await.unwrap().unwrap().data, "done");
        assert!(stream.next().await.is_none());
        assert_eq!(first, server.request().await);

        // The HTTP client's own timeout is classified the same way.
        let impatient = Client::builder()
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap();
        let headers = HeaderMap::new();
        let error = post_sse(&impatient, &url, headers, &body, timeouts(2000, 2000))
            .await
            .err()
            .map(reported)
            .unwrap();
        assert_eq!(error.kind, ProviderErrorKind::Timeout);
        server.request().await;
        assert!(server.finish().await.is_empty(), "unexpected replay");
    }

    #[tokio::test]
    async fn dropping_pending_send_cancels_the_request() {
        let mut server = Server::start(vec![Plan::stalled_headers()]).await;
        let client = client().unwrap();
        let url = server.url.clone();
        let mut request = Box::pin(post_sse(
            &client,
            &url,
            HeaderMap::new(),
            &Value::Null,
            Timeouts::default(),
        ));
        tokio::select! {
            _ = &mut request => panic!("request finished before cancellation"),
            _ = server.request() => {}
        }
        drop(request);
        assert!(server.finish().await.is_empty(), "unexpected replay");
    }

    #[tokio::test]
    async fn pre_response_failures_are_classified_and_sanitized() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let closed = format!("http://{}/responses", listener.local_addr().unwrap());
        drop(listener);
        let client = client().unwrap();
        for (url, kind) in [
            (closed.as_str(), ProviderErrorKind::Transport),
            ("http://[invalid", ProviderErrorKind::InvalidRequest),
        ] {
            // Bounded: a hang reports a timeout instead of the expected kind.
            let error = post_sse(
                &client,
                url,
                HeaderMap::new(),
                &Value::Null,
                timeouts(3000, 3000),
            )
            .await
            .err()
            .map(reported)
            .unwrap();
            assert_eq!(error.kind, kind);
            assert!(!error.message.contains(url));
        }
    }
}
