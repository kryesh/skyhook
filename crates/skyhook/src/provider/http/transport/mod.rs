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
pub(crate) use sse::{Batches, SseEvent, SseStream, flatten};

const MAX_ERROR_BYTES: usize = 16 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// The most reading a rejection's body may take, within the startup budget.
const REJECTION_READ: Duration = Duration::from_secs(5);

/// The media type requested and read.
pub(crate) const EVENT_STREAM: &str = "text/event-stream";

/// Providers' HTTP client, built at their first request rather than with them: the
/// first client built reads this machine's trusted roots, which runs off the async
/// workers. Clones share it and its connections.
#[derive(Clone, Default)]
pub(crate) struct LazyClient(std::sync::Arc<tokio::sync::OnceCell<Client>>);

impl LazyClient {
    pub(crate) async fn get(&self) -> Result<&Client, ProviderError> {
        let build = || async {
            tokio::task::spawn_blocking(client)
                .await
                .map_err(|error| ProviderErrorKind::Transport.error(error.to_string()))?
        };
        self.0.get_or_try_init(build).await
    }
}

pub(crate) fn client() -> Result<Client, ProviderError> {
    crate::tls::client_builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        // The runtime owns retries; disable automatic protocol-nack retries here.
        .retry(reqwest::retry::never())
        .build()
        .map_err(http_error)
}

pub(crate) fn http_error(error: reqwest::Error) -> ProviderError {
    let kind = if error.is_builder() {
        ProviderErrorKind::InvalidRequest
    } else if error.is_timeout() {
        ProviderErrorKind::Timeout
    } else {
        ProviderErrorKind::Transport
    };
    // Do not expose request URLs: they can contain caller-provided secrets.
    kind.error(error.without_url().to_string())
}

fn timeout_error(phase: &str) -> ProviderError {
    ProviderErrorKind::Timeout.error(format!("provider HTTP {phase} timeout"))
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

/// Make one HTTP/SSE attempt with a serialized JSON body; the agent runtime
/// owns retries. Returns the response headers with the stream. Dropping the
/// stream cancels its body.
pub(crate) async fn post_sse(
    client: &Client,
    url: &str,
    headers: HeaderMap,
    body: impl Into<reqwest::Body>,
    timeouts: Timeouts,
) -> Result<(HeaderMap, SseStream), Failure> {
    // The given headers replace the default content type.
    let request = client.post(url).header(CONTENT_TYPE, "application/json");
    stream(request.headers(headers).body(body).send(), timeouts).await
}

/// The startup deadline covers both the response headers and reading a rejection.
async fn stream(
    send: impl Future<Output = reqwest::Result<Response>>,
    timeouts: Timeouts,
) -> Result<(HeaderMap, SseStream), Failure> {
    let deadline = tokio::time::Instant::now() + timeouts.startup;
    let response = match tokio::time::timeout_at(deadline, send).await {
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
        return Err(ProviderErrorKind::Protocol
            .error("provider returned a non-SSE content type")
            .into());
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
    let deadline = startup_deadline.min(tokio::time::Instant::now() + REJECTION_READ);
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
    use crate::tests::{Plan, Server, reply};
    use futures_util::StreamExt;
    use std::assert_matches;

    /// An SSE frame, named `event` if at all.
    pub(crate) fn sse(event: Option<&str>, data: impl Into<String>) -> SseEvent {
        SseEvent {
            event: event.map(str::to_owned),
            data: data.into(),
        }
    }

    /// `value` as an SSE frame named by its `type`, if it has one.
    pub(crate) fn event(value: serde_json::Value) -> SseEvent {
        sse(value["type"].as_str(), value.to_string())
    }

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

    /// A `status` response whose body sends `prefix`, then never another byte.
    pub(super) fn stalled_response(status: u16, prefix: &'static str) -> Response {
        let prefix =
            futures_util::stream::iter([Ok::<_, std::io::Error>(bytes::Bytes::from(prefix))]);
        let chunks = prefix.chain(futures_util::stream::pending());
        let response = http::Response::builder().status(status);
        response
            .body(reqwest::Body::wrap_stream(chunks))
            .unwrap()
            .into()
    }

    /// Posts once to `server`, asserting it took one request and no replay.
    async fn rejected(server: &mut Server) -> Failure {
        let client = client().unwrap();
        let post = post_sse(
            &client,
            &server.url,
            HeaderMap::new(),
            "null",
            Timeouts::default(),
        );
        let error = crate::tests::bounded(post).await.err().unwrap();
        server.request().await;
        assert!(server.unclaimed().await.is_empty(), "unexpected replay");
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
            body.to_string(),
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
        let throttle = r#"{"error":{"code":"rate_limit_exceeded","message":"slow down"}}"#;
        // (status, headers, body, kind, the server's explanation carried verbatim)
        let cases = [
            (
                "429 Too Many Requests",
                "Retry-After: 120\r\n",
                throttle,
                RateLimited,
                ": slow down",
            ),
            // Non-JSON bodies (proxy text) still carry the explanation.
            (
                "503 Service Unavailable",
                "",
                "upstream unavailable",
                Unavailable,
                ": upstream unavailable",
            ),
            (
                "401 Unauthorized",
                json,
                refused,
                Authentication,
                ": upstream rejected the request",
            ),
            ("200 OK", json, "{}", Protocol, "non-SSE content type"),
        ];
        let plans = cases
            .iter()
            .map(|(status, headers, body, ..)| Plan::reply(reply(status, headers, body)));
        // An empty response closes the socket after consuming the full POST.
        let plans = plans.chain([Plan::reply(String::new())]).collect();
        let mut server = Server::start(plans).await;
        for (status, _, _, kind, explanation) in cases {
            let failure = rejected(&mut server).await;
            let unauthorized = matches!(&failure, Failure::Rejected(r) if r.unauthorized());
            assert_eq!(unauthorized, status.starts_with("401"), "{status}");
            let error = reported(failure);
            assert_eq!(error.kind(), kind, "{status}");
            assert!(
                error.message.ends_with(explanation),
                "{status}: {}",
                error.message
            );
            let hint = status.starts_with("429").then(|| Duration::from_secs(120));
            assert_eq!(error.retry_after(), hint, "{status}");
        }
        let closed = reported(rejected(&mut server).await);
        assert_eq!(closed.kind(), Transport);
        assert!(closed.is_retryable());
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_rejection_body_respects_remaining_startup_and_idle_limits() {
        let limit = Duration::from_secs(1);
        // The headers take `limit` of the startup budget; the sooner of its
        // remainder and the idle limit ends the body read.
        for (startup, read_idle) in [(2 * limit, 5 * limit), (5 * limit, limit)] {
            let start = tokio::time::Instant::now();
            let send = async {
                tokio::time::sleep(limit).await;
                Ok(stalled_response(401, ""))
            };
            let failure = stream(send, Timeouts { startup, read_idle })
                .await
                .err()
                .unwrap();
            assert_eq!(start.elapsed(), 2 * limit);
            // Preserve the known rejection classification even if diagnostics stall.
            assert_matches!(&failure, Failure::Rejected(r) if r.unauthorized());
            assert_eq!(reported(failure).kind(), ProviderErrorKind::Authentication);
        }
    }

    /// A stalled attempt's timeout, fired once the server holds its request:
    /// that request and the timeout's message.
    async fn timed_out(
        post: impl Future<Output = Result<(HeaderMap, SseStream), Failure>>,
        server: &mut Server,
        limit: Duration,
    ) -> (String, String) {
        let (result, request) = crate::tests::expire(post, server.request(), limit).await;
        let error = reported(result.err().unwrap());
        assert_eq!(error.kind(), ProviderErrorKind::Timeout);
        (request, error.message)
    }

    #[tokio::test]
    async fn timeouts_are_one_attempt_and_a_new_call_gets_a_fresh_deadline() {
        use crate::tests::bounded;
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
        let body = r#"{"model":"cold-model", "stream":true}"#;
        let post = |client| post_sse(client, &url, HeaderMap::new(), body, Timeouts::default());
        let startup = Timeouts::default().startup;
        let (first, message) = timed_out(post(&client), &mut server, startup).await;
        assert_eq!(message, "provider HTTP startup timeout");
        assert!(server.unclaimed().await.is_empty(), "unexpected replay");
        let (_, mut stream) = bounded(post(&client)).await.unwrap();
        assert_eq!(stream.next().await.unwrap().unwrap().data, "done");
        assert!(stream.next().await.is_none());
        assert_eq!(first, server.request().await);

        // The HTTP client's own timeout is classified the same way.
        let limit = Duration::from_secs(60);
        let impatient = Client::builder().timeout(limit).build().unwrap();
        let (_, message) = timed_out(post(&impatient), &mut server, limit).await;
        assert_ne!(message, "provider HTTP startup timeout");
        assert!(server.finish().await.is_empty(), "unexpected replay");
    }

    #[tokio::test]
    async fn pre_response_failures_are_classified_and_sanitized() {
        let port = crate::tests::RefusedPort::new();
        let closed = format!("http://{}/responses", port.address());
        let client = client().unwrap();
        for (url, kind) in [
            (closed.as_str(), ProviderErrorKind::Transport),
            ("http://[invalid", ProviderErrorKind::InvalidRequest),
        ] {
            let post = post_sse(&client, url, HeaderMap::new(), "null", Timeouts::default());
            let error = crate::tests::bounded(post)
                .await
                .err()
                .map(reported)
                .unwrap();
            assert_eq!(error.kind(), kind);
            assert!(!error.message.contains(url));
        }
    }
}
