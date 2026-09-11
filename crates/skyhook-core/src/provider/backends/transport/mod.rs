//! Shared, cancellation-safe HTTP/SSE transport. A dropped stream drops its HTTP body.
use super::errors::classify_error;
use crate::provider::{ProviderError, ProviderErrorKind, ProviderTimeouts};
use reqwest::{
    Client, Response,
    header::{ACCEPT, CONTENT_TYPE, HeaderMap},
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
        // This transport owns the complete HTTP attempt budget, including Codex's
        // one-attempt policy; disable reqwest's automatic protocol-nack retries.
        .retry(reqwest::retry::never())
        .build()
        .map_err(http_error)
}

pub(crate) fn http_error(error: reqwest::Error) -> ProviderError {
    ProviderError {
        kind: if error.is_timeout() {
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

/// Retry startup timeouts, pre-response send failures and transient HTTP rejections,
/// at most twice. Once a successful response is accepted no request is ever replayed,
/// including malformed streams, EOF, idle timeouts, or caller cancellation.
#[cfg(test)]
pub(crate) async fn post_sse(
    client: &Client,
    url: &str,
    headers: HeaderMap,
    body: &Value,
) -> Result<SseStream, ProviderError> {
    post_sse_with_timeouts(client, url, headers, body, ProviderTimeouts::default()).await
}

/// Codex and other continuation protocols can disable even overload retries.
pub(crate) async fn post_sse_once(
    client: &Client,
    url: &str,
    headers: HeaderMap,
    body: &Value,
) -> Result<SseStream, ProviderError> {
    post_sse_policy(client, url, headers, body, 1, ProviderTimeouts::default()).await
}

pub(crate) async fn post_sse_with_timeouts(
    client: &Client,
    url: &str,
    headers: HeaderMap,
    body: &Value,
    timeouts: ProviderTimeouts,
) -> Result<SseStream, ProviderError> {
    post_sse_policy(client, url, headers, body, 3, timeouts).await
}

async fn post_sse_policy(
    client: &Client,
    url: &str,
    mut headers: HeaderMap,
    body: &Value,
    attempts: u32,
    timeouts: ProviderTimeouts,
) -> Result<SseStream, ProviderError> {
    headers.insert(ACCEPT, "text/event-stream".parse().expect("static header"));
    for attempt in 0..attempts {
        // Startup is an HTTP-attempt budget, not a budget shared by retries or
        // their backoff. Error diagnostics use this same remaining deadline.
        let deadline = tokio::time::Instant::now() + timeouts.startup;
        let backoff = Duration::from_millis(200 * (1 << attempt));
        let sent = tokio::time::timeout_at(
            deadline,
            client.post(url).headers(headers.clone()).json(body).send(),
        )
        .await;
        let response = match sent {
            Ok(Ok(response)) => response,
            failure => {
                let (error, retryable) = match failure {
                    Err(_) => (timeout_error("startup"), true),
                    Ok(Err(error)) => {
                        // The fixed JSON request is replayable. A connection
                        // closed/reset before headers has the same ambiguity as
                        // a header timeout; builder errors are not transient.
                        let retryable =
                            error.is_connect() || error.is_timeout() || error.is_request();
                        (http_error(error), retryable)
                    }
                    Ok(Ok(_)) => unreachable!(),
                };
                if retryable && attempt + 1 < attempts {
                    // No detached work: dropping this future cancels a pending
                    // send or sleep and cannot launch a subsequent attempt.
                    tokio::time::sleep(backoff).await;
                    continue;
                }
                return Err(if retryable {
                    with_attempt_count(error, attempt + 1)
                } else {
                    error
                });
            }
        };
        let status = response.status();
        if !status.is_success() {
            let retryable = matches!(status.as_u16(), 408 | 429 | 500 | 502 | 503 | 504);
            if retryable
                && attempt + 1 < attempts
                && let Some(delay) = retry_delay(response.headers(), backoff)
            {
                drop(response);
                tokio::time::sleep(delay).await;
                continue;
            }
            let error = status_error(response, deadline, timeouts.read_idle).await;
            return Err(if retryable {
                with_attempt_count(error, attempt + 1)
            } else {
                error
            });
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|h| h.to_str().ok())
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
        return Ok(response_stream(response, timeouts.read_idle));
    }
    unreachable!("bounded retry loop always returns")
}

fn with_attempt_count(mut error: ProviderError, attempts: u32) -> ProviderError {
    error
        .message
        .push_str(&format!(" (after {attempts} HTTP attempts)"));
    error
}

fn retry_delay(headers: &HeaderMap, backoff: Duration) -> Option<Duration> {
    let mut values = headers.get_all("retry-after").iter();
    let Some(value) = values.next() else {
        return Some(backoff);
    };
    // Never retry earlier than Retry-After. Decline long, HTTP-date, ambiguous
    // or malformed values rather than exceeding this bounded short-delay policy.
    if values.next().is_some() {
        return None;
    }
    let value = value.to_str().ok()?.trim();
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let seconds = value.parse::<u64>().ok().filter(|seconds| *seconds <= 2)?;
    Some(backoff.max(Duration::from_secs(seconds)))
}

async fn status_error(
    mut response: Response,
    startup_deadline: tokio::time::Instant,
    read_idle: Duration,
) -> ProviderError {
    let status = response.status().as_u16();
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
    classify_error(Some(status), &native.unwrap_or(Value::Null))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use futures_util::StreamExt;

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
    async fn slow_first_headers_retry_with_a_fresh_startup_deadline() {
        let mut success = ResponsePlan::reply(successful_reply());
        success.delay = Duration::from_millis(40);
        let mut server =
            ObservedServer::start(vec![ResponsePlan::stalled_headers(), success]).await;
        let body = serde_json::json!({"model":"cold-model", "stream":true});
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer test-key".parse().unwrap());
        let mut stream = tokio::time::timeout(
            Duration::from_secs(2),
            post_sse_with_timeouts(
                &client().unwrap(),
                &server.url,
                headers,
                &body,
                short_timeouts(),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(stream.next().await.unwrap().unwrap().data, "done");
        assert!(stream.next().await.is_none());
        let (first, first_at) = server.request().await;
        let (second, second_at) = server.request().await;
        assert_eq!(first, second);
        // Allow for the gap between starting the timeout and loopback accept.
        assert!(second_at.duration_since(first_at) >= Duration::from_millis(280));
        server.no_more_requests().await;
    }

    #[tokio::test]
    async fn every_slow_header_attempt_gets_its_own_budget_and_final_count() {
        let mut server =
            ObservedServer::start((0..3).map(|_| ResponsePlan::stalled_headers()).collect()).await;
        let error = tokio::time::timeout(
            Duration::from_secs(3),
            post_sse_with_timeouts(
                &client().unwrap(),
                &server.url,
                HeaderMap::new(),
                &serde_json::json!({"model":"cold-model", "stream":true}),
                short_timeouts(),
            ),
        )
        .await
        .unwrap()
        .err()
        .unwrap();
        assert_eq!(error.kind, ProviderErrorKind::Timeout);
        assert_eq!(
            error.message,
            "provider HTTP startup timeout (after 3 HTTP attempts)"
        );
        let (first, first_at) = server.request().await;
        let (second, second_at) = server.request().await;
        let (third, third_at) = server.request().await;
        assert_eq!(first, second);
        assert_eq!(second, third);
        // Allow for the gap between starting the timeout and loopback accept.
        assert!(second_at.duration_since(first_at) >= Duration::from_millis(280));
        assert!(third_at.duration_since(second_at) >= Duration::from_millis(480));
        server.no_more_requests().await;
    }

    #[tokio::test]
    async fn one_attempt_policy_never_replays_a_startup_timeout() {
        let mut server = ObservedServer::start(vec![ResponsePlan::stalled_headers()]).await;
        // Same policy used by post_sse_once, with a millisecond test budget.
        let error = post_sse_policy(
            &client().unwrap(),
            &server.url,
            HeaderMap::new(),
            &Value::Null,
            1,
            short_timeouts(),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(error.kind, ProviderErrorKind::Timeout);
        assert!(error.message.ends_with("(after 1 HTTP attempts)"));
        server.request().await;
        server.no_more_requests().await;
    }

    #[tokio::test]
    async fn dropping_pending_send_or_backoff_cancels_future_attempts() {
        for during_backoff in [false, true] {
            let plan = if during_backoff {
                ResponsePlan::reply(reply("503 Service Unavailable", "", ""))
            } else {
                ResponsePlan::stalled_headers()
            };
            let mut server = ObservedServer::start(vec![plan]).await;
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
            if during_backoff {
                // Poll through the immediate rejection into its 200ms backoff.
                tokio::select! {
                    _ = &mut request => panic!("request finished during backoff"),
                    _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                }
            }
            drop(request);
            server.no_more_requests().await;
        }
    }

    #[tokio::test]
    async fn explicit_transient_http_rejections_retry_before_sse_acceptance() {
        for status in [
            "408 Request Timeout",
            "429 Too Many Requests",
            "500 Internal Server Error",
            "502 Bad Gateway",
            "503 Service Unavailable",
            "504 Gateway Timeout",
        ] {
            let mut server = ObservedServer::start(vec![
                ResponsePlan::reply(reply(
                    status,
                    "",
                    r#"{"error":{"message":"secret prompt"}}"#,
                )),
                ResponsePlan::reply(successful_reply()),
            ])
            .await;
            let mut stream = post_sse_with_timeouts(
                &client().unwrap(),
                &server.url,
                HeaderMap::new(),
                &Value::Null,
                short_timeouts(),
            )
            .await
            .unwrap();
            assert_eq!(stream.next().await.unwrap().unwrap().data, "done");
            let (first, first_at) = server.request().await;
            let (second, second_at) = server.request().await;
            assert_eq!(first, second);
            assert!(second_at.duration_since(first_at) >= Duration::from_millis(200));
            server.no_more_requests().await;
        }
    }

    #[tokio::test]
    async fn transient_http_exhaustion_counts_attempts_and_sanitizes_body() {
        for (status, kind) in [
            ("429 Too Many Requests", ProviderErrorKind::RateLimited),
            ("503 Service Unavailable", ProviderErrorKind::Response),
        ] {
            let mut server = ObservedServer::start(
                (0..3)
                    .map(|_| {
                        ResponsePlan::reply(reply(
                            status,
                            "Retry-After: 0\r\n",
                            r#"{"error":{"message":"secret prompt"}}"#,
                        ))
                    })
                    .collect(),
            )
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
            assert!(error.message.ends_with("(after 3 HTTP attempts)"));
            assert!(!error.message.contains("secret"));
            let first = server.request().await.0;
            assert_eq!(first, server.request().await.0);
            assert_eq!(first, server.request().await.0);
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
    async fn retry_after_is_never_shortened_by_backoff_or_startup_deadline() {
        let mut server = ObservedServer::start(vec![
            ResponsePlan::reply(reply("503 Service Unavailable", "Retry-After: 1\r\n", "")),
            ResponsePlan::reply(successful_reply()),
        ])
        .await;
        drop(
            post_sse_with_timeouts(
                &client().unwrap(),
                &server.url,
                HeaderMap::new(),
                &Value::Null,
                short_timeouts(),
            )
            .await
            .unwrap(),
        );
        let (_, first_at) = server.request().await;
        let (_, second_at) = server.request().await;
        assert!(second_at.duration_since(first_at) >= Duration::from_secs(1));
        server.no_more_requests().await;
    }

    #[tokio::test]
    async fn long_malformed_or_ambiguous_retry_after_is_declined() {
        for headers in [
            "Retry-After: 3\r\n",
            "Retry-After: not-a-delay\r\n",
            "Retry-After: Wed, 21 Oct 2030 07:28:00 GMT\r\n",
            "Retry-After: 0\r\nRetry-After: 1\r\n",
        ] {
            let mut server = ObservedServer::start(vec![ResponsePlan::reply(reply(
                "503 Service Unavailable",
                headers,
                "reflected secret",
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
            assert!(error.message.ends_with("(after 1 HTTP attempts)"));
            assert!(!error.message.contains("secret"));
            server.request().await;
            server.no_more_requests().await;
        }
    }

    #[tokio::test]
    async fn connect_failures_exhaust_the_transport_retry_budget() {
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
        assert!(error.message.ends_with("(after 3 HTTP attempts)"));
        assert!(!error.message.contains(&url));
    }

    #[tokio::test]
    async fn final_transient_error_body_uses_the_last_attempt_remaining_deadline() {
        let mut last = ResponsePlan::reply("HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: 999\r\n\r\n".into());
        last.delay = Duration::from_millis(60);
        last.stall_body = true;
        let mut server = ObservedServer::start(vec![
            ResponsePlan::reply(reply("503 Service Unavailable", "", "")),
            ResponsePlan::reply(reply("503 Service Unavailable", "", "")),
            last,
        ])
        .await;
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
        assert!(error.message.ends_with("(after 3 HTTP attempts)"));
        for _ in 0..3 {
            server.request().await;
        }
        server.no_more_requests().await;
    }

    #[tokio::test]
    async fn reqwest_send_timeouts_are_retried_before_headers() {
        let mut server = ObservedServer::start(vec![
            ResponsePlan::stalled_headers(),
            ResponsePlan::reply(successful_reply()),
        ])
        .await;
        let client = Client::builder()
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap();
        drop(
            post_sse_with_timeouts(
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
            .unwrap(),
        );
        assert_eq!(server.request().await.0, server.request().await.0);
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
        assert!(error.message.ends_with("(after 1 HTTP attempts)"));
        assert!(!error.message.contains("secret"));
        server.request().await;
        server.no_more_requests().await;
    }

    #[tokio::test]
    async fn connection_closed_after_post_before_headers_is_retried_with_identical_request() {
        for succeeds in [true, false] {
            // An empty wire response closes the socket after consuming the full POST.
            let mut plans = vec![ResponsePlan::reply(String::new())];
            if succeeds {
                plans.push(ResponsePlan::reply(successful_reply()));
            } else {
                plans.extend((0..2).map(|_| ResponsePlan::reply(String::new())));
            }
            let mut server = ObservedServer::start(plans).await;
            let body = serde_json::json!({"model":"cold-model", "stream":true});
            let result = post_sse_with_timeouts(
                &client().unwrap(),
                &server.url,
                HeaderMap::new(),
                &body,
                short_timeouts(),
            )
            .await;
            if succeeds {
                let mut stream = result.unwrap();
                assert_eq!(stream.next().await.unwrap().unwrap().data, "done");
            } else {
                let error = result.err().unwrap();
                assert_eq!(error.kind, ProviderErrorKind::Transport);
                assert!(error.message.ends_with("(after 3 HTTP attempts)"));
            }
            let first = server.request().await.0;
            assert_eq!(first, server.request().await.0);
            if !succeeds {
                assert_eq!(first, server.request().await.0);
            }
            server.no_more_requests().await;
        }
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
        assert_eq!(error.kind, ProviderErrorKind::Transport);
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
