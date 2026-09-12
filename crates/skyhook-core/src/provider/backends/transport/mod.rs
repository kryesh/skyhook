//! Shared, cancellation-safe HTTP/SSE transport. A dropped stream drops its HTTP body.
use super::errors::classify_error;
use crate::provider::{ProviderError, ProviderErrorKind, ProviderTimeouts};
use reqwest::{
    Client, Response,
    header::{ACCEPT, CONTENT_TYPE, HeaderMap, RETRY_AFTER},
};
use serde_json::Value;
use std::time::Duration;

mod sse;
use sse::response_stream;
pub(crate) use sse::{SseEvent, SseStream};

const MAX_ERROR_BYTES: usize = 16 * 1024;

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
        retry_after: None,
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
        retry_after: None,
        kind: ProviderErrorKind::Timeout,
        message: format!("provider HTTP {phase} timeout"),
    }
}

/// Make one HTTP/SSE attempt. Provider retries are owned centrally by the agent
/// runtime so every backend receives the same policy. Once a response
/// is accepted, dropping this stream cancels its body and transport state.
#[cfg(test)]
pub(crate) async fn post_sse(
    client: &Client,
    url: &str,
    headers: HeaderMap,
    body: &Value,
) -> Result<SseStream, ProviderError> {
    post_sse_with_timeouts(client, url, headers, body, ProviderTimeouts::default()).await
}

/// Default-timeout entry point for continuation protocols.
pub(crate) async fn post_sse_once(
    client: &Client,
    url: &str,
    headers: HeaderMap,
    body: &Value,
) -> Result<SseStream, ProviderError> {
    post_sse_with_timeouts(client, url, headers, body, ProviderTimeouts::default()).await
}

pub(crate) async fn post_sse_with_timeouts(
    client: &Client,
    url: &str,
    mut headers: HeaderMap,
    body: &Value,
    timeouts: ProviderTimeouts,
) -> Result<SseStream, ProviderError> {
    headers.insert(ACCEPT, "text/event-stream".parse().expect("static header"));
    let deadline = tokio::time::Instant::now() + timeouts.startup;
    let sent = tokio::time::timeout_at(
        deadline,
        client.post(url).headers(headers).json(body).send(),
    )
    .await;
    let response = match sent {
        Ok(Ok(response)) => response,
        Err(_) => return Err(timeout_error("startup")),
        Ok(Err(error)) => return Err(http_error(error)),
    };
    if !response.status().is_success() {
        return Err(status_error(response, deadline, timeouts.read_idle).await);
    }
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|header| header.to_str().ok())
        .unwrap_or("");
    if !content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .eq_ignore_ascii_case("text/event-stream")
    {
        return Err(ProviderError::protocol(
            "provider returned a non-SSE content type",
        ));
    }
    Ok(response_stream(response, timeouts.read_idle))
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

async fn status_error(
    mut response: Response,
    startup_deadline: tokio::time::Instant,
    read_idle: Duration,
) -> ProviderError {
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
    let native: Option<Value> = serde_json::from_slice(&body).ok();
    let mut error = classify_error(Some(status), &native.unwrap_or(Value::Null));
    error.retry_after = retry_after;
    error
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use futures_util::StreamExt;

    #[test]
    fn retry_after_accepts_seconds_and_http_dates_without_capping_server_delays() {
        let now = std::time::UNIX_EPOCH + Duration::from_secs(784111717);
        for (value, expected) in [
            ("0", 0),
            ("1", 1),
            ("120", 120),
            ("Sun, 06 Nov 1994 08:49:37 GMT", 60),
            ("Sunday, 06-Nov-94 08:49:37 GMT", 60),
            ("Sun Nov  6 08:49:37 1994", 60),
            ("Sun, 06 Nov 1994 08:47:37 GMT", 0),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(RETRY_AFTER, value.parse().unwrap());
            assert_eq!(
                retry_after(&headers, now),
                Some(Duration::from_secs(expected)),
                "{value}"
            );
        }
        for value in [
            "",
            "-1",
            "+1",
            "1.5",
            "10, 20",
            "invalid",
            "18446744073709551615",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(RETRY_AFTER, value.parse().unwrap());
            assert_eq!(retry_after(&headers, now), None, "{value}");
        }
        let mut headers = HeaderMap::new();
        headers.append(RETRY_AFTER, "5".parse().unwrap());
        headers.append(RETRY_AFTER, "10".parse().unwrap());
        assert_eq!(retry_after(&headers, now), None);
    }

    #[tokio::test]
    async fn rate_limit_error_preserves_server_delay_and_safe_diagnostics() {
        let (url, server) = server(vec![reply(
            "429 Too Many Requests",
            "Retry-After: 120\r\n",
            r#"{"error":{"code":"rate_limit_exceeded","message":"private prompt or secret"}}"#,
        )])
        .await;
        let error = match post_sse(
            &client().unwrap(),
            &url,
            HeaderMap::new(),
            &serde_json::json!({}),
        )
        .await
        {
            Ok(_) => panic!("expected rate limit"),
            Err(error) => error,
        };
        assert_eq!(error.kind, ProviderErrorKind::RateLimited);
        assert_eq!(error.retry_after, Some(Duration::from_secs(120)));
        assert!(error.message.contains("429"));
        assert!(error.message.contains("rate_limit_exceeded"));
        assert!(!error.message.contains("private"));
        assert!(!error.message.contains("secret"));
        assert_eq!(server.await.unwrap().len(), 1, "HTTP itself must not retry");
    }

    pub(super) fn reply(status: &str, headers: &str, body: &str) -> String {
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

    /// Scripted HTTP fixture with full raw requests, shared with backend acceptance tests.
    pub(crate) async fn server(
        replies: Vec<String>,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/responses", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for reply in replies {
                let (mut socket, _) = listener.accept().await.unwrap();
                requests.push(read_request(&mut socket).await);
                socket.write_all(reply.as_bytes()).await.unwrap();
            }
            requests
        });
        (url, task)
    }

    // Keep listening after every scripted response, so negative replay assertions
    // detect actual extra connections rather than a server which has already exited.
    pub(super) struct ObservedServer {
        pub(super) url: String,
        requests: tokio::sync::mpsc::UnboundedReceiver<(String, tokio::time::Instant)>,
        task: tokio::task::JoinHandle<()>,
        connections: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        observed_requests: usize,
    }

    pub(super) struct ResponsePlan {
        delay: Duration,
        wire: Option<String>,
        pub(super) stall_body: bool,
    }

    impl ResponsePlan {
        pub(super) fn reply(wire: String) -> Self {
            Self {
                delay: Duration::ZERO,
                wire: Some(wire),
                stall_body: false,
            }
        }

        fn stalled_headers() -> Self {
            Self {
                delay: Duration::ZERO,
                wire: None,
                stall_body: false,
            }
        }
    }

    impl ObservedServer {
        pub(super) async fn start(plans: Vec<ResponsePlan>) -> Self {
            use tokio::io::AsyncWriteExt;
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/responses", listener.local_addr().unwrap());
            let (tx, requests) = tokio::sync::mpsc::unbounded_channel();
            let connection_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let accepted = connection_count.clone();
            let task = tokio::spawn(async move {
                let mut plans = plans.into_iter();
                let mut connections = tokio::task::JoinSet::new();
                loop {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    accepted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let plan = plans.next().unwrap_or_else(ResponsePlan::stalled_headers);
                    let tx = tx.clone();
                    connections.spawn(async move {
                        let received = tokio::time::Instant::now();
                        let request = read_request(&mut socket).await;
                        tx.send((request, received)).unwrap();
                        tokio::time::sleep(plan.delay).await;
                        if let Some(wire) = plan.wire {
                            // Timed-out attempts may close the connection before writing.
                            let _ = socket.write_all(wire.as_bytes()).await;
                            if !plan.stall_body {
                                return;
                            }
                        }
                        std::future::pending::<()>().await;
                        drop(socket);
                    });
                }
            });
            Self {
                url,
                requests,
                task,
                connections: connection_count,
                observed_requests: 0,
            }
        }

        pub(super) async fn request(&mut self) -> (String, tokio::time::Instant) {
            let request = tokio::time::timeout(Duration::from_secs(3), self.requests.recv())
                .await
                .expect("expected HTTP request")
                .unwrap();
            self.observed_requests += 1;
            request
        }

        pub(super) async fn no_more_requests(mut self) {
            assert!(
                tokio::time::timeout(Duration::from_millis(650), self.requests.recv())
                    .await
                    .is_err(),
                "unexpected replay"
            );
            assert_eq!(
                self.connections.load(std::sync::atomic::Ordering::SeqCst),
                self.observed_requests,
                "unexpected connection, even without a complete request"
            );
            self.task.abort();
            let _ = (&mut self.task).await;
        }
    }

    impl Drop for ObservedServer {
        fn drop(&mut self) {
            // Aborting the accept task drops its JoinSet and all held sockets.
            self.task.abort();
        }
    }

    pub(super) fn short_timeouts() -> ProviderTimeouts {
        ProviderTimeouts {
            startup: Duration::from_millis(100),
            read_idle: Duration::from_millis(100),
        }
    }

    fn successful_reply() -> String {
        reply(
            "200 OK",
            "Content-Type: text/event-stream\r\n",
            "data: done\n\n",
        )
    }

    #[tokio::test]
    async fn wire_headers_body_and_eof() {
        let mut server = ObservedServer::start(vec![ResponsePlan::reply(reply(
            "200 OK",
            "Content-Type: text/event-stream; charset=utf-8\r\n",
            "data: one\r\n\r\ndata: two",
        ))])
        .await;
        let body = serde_json::json!({"model":"literal-model", "stream":true});
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer test-key".parse().unwrap());
        let events = post_sse(&client().unwrap(), &server.url, headers, &body)
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert_eq!(
            events.into_iter().collect::<Result<Vec<_>, _>>().unwrap(),
            vec![
                SseEvent {
                    event: None,
                    data: "one".into()
                },
                SseEvent {
                    event: None,
                    data: "two".into()
                }
            ]
        );
        let (request, _) = server.request().await;
        assert!(request.contains("authorization: Bearer test-key"));
        assert!(request.contains("accept: text/event-stream"));
        let wire: Value = serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(wire, body);
        server.no_more_requests().await;
    }

    #[tokio::test]
    async fn stalled_rejection_body_respects_remaining_startup_and_idle_limits() {
        for startup_limited in [true, false] {
            let mut plan = ResponsePlan::reply("HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: 999\r\n\r\n".into());
            // Most of the startup budget is consumed before the rejection headers.
            plan.delay = Duration::from_millis(100);
            plan.stall_body = true;
            let mut server = ObservedServer::start(vec![plan]).await;
            let timeouts = if startup_limited {
                ProviderTimeouts {
                    startup: Duration::from_millis(200),
                    read_idle: Duration::from_secs(5),
                }
            } else {
                ProviderTimeouts {
                    startup: Duration::from_secs(5),
                    read_idle: Duration::from_millis(100),
                }
            };
            let error = tokio::time::timeout(
                Duration::from_secs(1),
                post_sse_with_timeouts(
                    &client().unwrap(),
                    &server.url,
                    HeaderMap::new(),
                    &serde_json::json!({}),
                    timeouts,
                ),
            )
            .await
            .expect("diagnostic body must not extend configured budgets")
            .err()
            .unwrap();
            // Preserve the known rejection classification even if diagnostics stall.
            assert_eq!(error.kind, ProviderErrorKind::Authentication);
            server.request().await;
            server.no_more_requests().await;
        }
    }

    #[tokio::test]
    async fn startup_timeout_is_one_attempt_and_a_new_call_gets_a_fresh_deadline() {
        let mut server = ObservedServer::start(vec![
            ResponsePlan::stalled_headers(),
            ResponsePlan::reply(successful_reply()),
        ])
        .await;
        let client = client().unwrap();
        let body = serde_json::json!({"model":"cold-model", "stream":true});
        let error = post_sse_with_timeouts(
            &client,
            &server.url,
            HeaderMap::new(),
            &body,
            short_timeouts(),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(error.kind, ProviderErrorKind::Timeout);
        assert_eq!(error.message, "provider HTTP startup timeout");
        let first = server.request().await.0;
        assert!(
            tokio::time::timeout(Duration::from_millis(250), server.requests.recv())
                .await
                .is_err()
        );
        let mut stream = post_sse_with_timeouts(
            &client,
            &server.url,
            HeaderMap::new(),
            &body,
            short_timeouts(),
        )
        .await
        .unwrap();
        assert_eq!(stream.next().await.unwrap().unwrap().data, "done");
        assert!(stream.next().await.is_none());
        assert_eq!(first, server.request().await.0);
        server.no_more_requests().await;
    }

    #[tokio::test]
    async fn dropping_pending_send_cancels_the_request() {
        let mut server = ObservedServer::start(vec![ResponsePlan::stalled_headers()]).await;
        let client = client().unwrap();
        let url = server.url.clone();
        let mut request = Box::pin(post_sse_with_timeouts(
            &client,
            &url,
            HeaderMap::new(),
            &Value::Null,
            short_timeouts(),
        ));
        tokio::select! {
            _ = &mut request => panic!("request finished before cancellation"),
            _ = server.request() => {}
        }
        drop(request);
        server.no_more_requests().await;
    }

    #[tokio::test]
    async fn transient_http_rejections_are_sanitized_and_returned_without_transport_retries() {
        for (status, kind) in [
            ("408 Request Timeout", ProviderErrorKind::Timeout),
            ("429 Too Many Requests", ProviderErrorKind::RateLimited),
            ("500 Internal Server Error", ProviderErrorKind::Response),
            ("502 Bad Gateway", ProviderErrorKind::Response),
            ("503 Service Unavailable", ProviderErrorKind::Response),
            ("504 Gateway Timeout", ProviderErrorKind::Timeout),
        ] {
            let mut server = ObservedServer::start(vec![ResponsePlan::reply(reply(
                status,
                "Retry-After: 0\r\n",
                r#"{"error":{"message":"secret prompt"}}"#,
            ))])
            .await;
            let error = post_sse_with_timeouts(
                &client().unwrap(),
                &server.url,
                HeaderMap::new(),
                &Value::Null,
                short_timeouts(),
            )
            .await
            .err()
            .unwrap();
            assert_eq!(error.kind, kind);
            assert!(error.recovery().is_some());
            assert!(!error.message.contains("secret"));
            assert!(!error.message.contains("attempt"));
            server.request().await;
            server.no_more_requests().await;
        }
    }

    #[tokio::test]
    async fn permanent_http_rejections_and_malformed_success_are_not_replayed() {
        for status in [
            "400 Bad Request",
            "401 Unauthorized",
            "403 Forbidden",
            "404 Not Found",
            "200 OK",
        ] {
            let mut server = ObservedServer::start(vec![ResponsePlan::reply(reply(
                status,
                "Content-Type: application/json\r\n",
                r#"{"error":{"message":"secret prompt"}}"#,
            ))])
            .await;
            let error = post_sse_with_timeouts(
                &client().unwrap(),
                &server.url,
                HeaderMap::new(),
                &Value::Null,
                short_timeouts(),
            )
            .await
            .err()
            .unwrap();
            if status == "200 OK" {
                assert_eq!(error.kind, ProviderErrorKind::Protocol);
            }
            assert!(!error.message.contains("secret"));
            server.request().await;
            server.no_more_requests().await;
        }
    }

    #[tokio::test]
    async fn connect_failures_return_sanitized_transient_errors() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/responses", listener.local_addr().unwrap());
        drop(listener);
        let error = tokio::time::timeout(
            Duration::from_secs(2),
            post_sse_with_timeouts(
                &client().unwrap(),
                &url,
                HeaderMap::new(),
                &Value::Null,
                short_timeouts(),
            ),
        )
        .await
        .unwrap()
        .err()
        .unwrap();
        assert_eq!(error.kind, ProviderErrorKind::Transport);
        assert!(!error.message.contains("HTTP attempts"));
        assert!(!error.message.contains(&url));
    }

    #[tokio::test]
    async fn transient_error_body_uses_the_remaining_startup_deadline() {
        let mut last = ResponsePlan::reply("HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: 999\r\n\r\n".into());
        last.delay = Duration::from_millis(60);
        last.stall_body = true;
        let mut server = ObservedServer::start(vec![last]).await;
        let error = tokio::time::timeout(
            Duration::from_secs(2),
            post_sse_with_timeouts(
                &client().unwrap(),
                &server.url,
                HeaderMap::new(),
                &Value::Null,
                ProviderTimeouts {
                    startup: Duration::from_millis(100),
                    read_idle: Duration::from_secs(5),
                },
            ),
        )
        .await
        .expect("error body must not extend the final startup budget")
        .err()
        .unwrap();
        assert_eq!(error.kind, ProviderErrorKind::Response);
        assert!(!error.message.contains("HTTP attempts"));
        server.request().await;
        server.no_more_requests().await;
    }

    #[tokio::test]
    async fn reqwest_send_timeouts_return_without_retrying() {
        let mut server = ObservedServer::start(vec![ResponsePlan::stalled_headers()]).await;
        let client = Client::builder()
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap();
        let error = post_sse_with_timeouts(
            &client,
            &server.url,
            HeaderMap::new(),
            &Value::Null,
            ProviderTimeouts {
                startup: Duration::from_secs(2),
                read_idle: Duration::from_secs(2),
            },
        )
        .await
        .err()
        .unwrap();
        assert_eq!(error.kind, ProviderErrorKind::Timeout);
        server.request().await;
        server.no_more_requests().await;
    }

    #[tokio::test]
    async fn codex_once_does_not_replay_a_transient_http_rejection() {
        let mut server = ObservedServer::start(vec![ResponsePlan::reply(reply(
            "503 Service Unavailable",
            "Retry-After: 0\r\n",
            "reflected secret",
        ))])
        .await;
        let error = post_sse_once(
            &client().unwrap(),
            &server.url,
            HeaderMap::new(),
            &Value::Null,
        )
        .await
        .err()
        .unwrap();
        assert_eq!(error.kind, ProviderErrorKind::Response);
        assert!(!error.message.contains("HTTP attempts"));
        assert!(!error.message.contains("secret"));
        server.request().await;
        server.no_more_requests().await;
    }

    #[tokio::test]
    async fn connection_closed_after_post_returns_without_replaying() {
        // An empty response closes the socket after consuming the full POST.
        let mut server = ObservedServer::start(vec![ResponsePlan::reply(String::new())]).await;
        let error = post_sse_with_timeouts(
            &client().unwrap(),
            &server.url,
            HeaderMap::new(),
            &Value::Null,
            short_timeouts(),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(error.kind, ProviderErrorKind::Transport);
        server.request().await;
        server.no_more_requests().await;
    }

    #[tokio::test]
    async fn request_builder_errors_are_not_transient() {
        let error = post_sse_with_timeouts(
            &client().unwrap(),
            "http://[invalid",
            HeaderMap::new(),
            &Value::Null,
            short_timeouts(),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
        assert!(!error.message.contains("HTTP attempts"));
    }

    #[tokio::test]
    async fn llama_context_errors_are_sanitized() {
        let mut server = ObservedServer::start(vec![ResponsePlan::reply(reply(
            "400 Bad Request", "",
            r#"{"error":{"code":400,"type":"exceed_context_size_error","message":"secret prompt"}}"#,
        ))]).await;
        let error = post_sse(
            &client().unwrap(),
            &server.url,
            HeaderMap::new(),
            &Value::Null,
        )
        .await
        .err()
        .unwrap();
        assert_eq!(error.kind, ProviderErrorKind::ContextWindowExceeded);
        assert!(!error.message.contains("secret"));
        server.request().await;
        server.no_more_requests().await;
    }
}
