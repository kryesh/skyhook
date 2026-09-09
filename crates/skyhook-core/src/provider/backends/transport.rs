//! Shared, cancellation-safe HTTP/SSE transport. A dropped stream drops its HTTP body.
use super::errors::classify_error;
use crate::provider::{ProviderError, ProviderErrorKind, ProviderTimeouts};
use futures_util::stream::{self, BoxStream};
use reqwest::{
    Client, Response,
    header::{ACCEPT, CONTENT_TYPE, HeaderMap},
};
use serde_json::Value;
use std::{collections::VecDeque, time::Duration};

const MAX_EVENT_BYTES: usize = 8 * 1024 * 1024;
const MAX_ERROR_BYTES: usize = 16 * 1024;
pub(crate) type SseStream = BoxStream<'static, Result<SseEvent, ProviderError>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

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

fn response_stream(response: Response, read_idle: Duration) -> SseStream {
    struct State {
        response: Response,
        parser: SseParser,
        pending: VecDeque<SseEvent>,
        done: bool,
        read_idle: Duration,
    }
    Box::pin(stream::unfold(
        State {
            response,
            parser: SseParser::default(),
            pending: VecDeque::new(),
            done: false,
            read_idle,
        },
        |mut state| async move {
            loop {
                if let Some(event) = state.pending.pop_front() {
                    return Some((Ok(event), state));
                }
                if state.done {
                    return None;
                }
                let read = tokio::time::timeout(state.read_idle, state.response.chunk()).await;
                let parsed = match read {
                    Err(_) => Err(timeout_error("read")),
                    Ok(Err(error)) => Err(http_error(error)),
                    Ok(Ok(Some(bytes))) => state.parser.push(&bytes),
                    Ok(Ok(None)) => {
                        state.done = true;
                        state.parser.finish()
                    }
                };
                match parsed {
                    Ok(events) => state.pending.extend(events),
                    Err(error) => {
                        state.done = true;
                        return Some((Err(error), state));
                    }
                }
            }
        },
    ))
}

/// Byte framing before UTF-8 decoding supports codepoints and CRLF split across
/// arbitrary network chunks. Comments and unknown SSE metadata are harmless.
#[derive(Default)]
pub(crate) struct SseParser {
    line: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
    size: usize,
    skip_lf: bool,
    started: bool,
}
impl SseParser {
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Result<Vec<SseEvent>, ProviderError> {
        let mut events = Vec::new();
        for &byte in bytes {
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            match byte {
                b'\r' | b'\n' => {
                    self.consume_line(&mut events)?;
                    self.skip_lf = byte == b'\r';
                }
                _ => {
                    self.line.push(byte);
                    if self.line.len() + self.size > MAX_EVENT_BYTES {
                        return Err(ProviderError::protocol("SSE event exceeds size limit"));
                    }
                }
            }
        }
        Ok(events)
    }
    fn consume_line(&mut self, events: &mut Vec<SseEvent>) -> Result<(), ProviderError> {
        let bytes = std::mem::take(&mut self.line);
        let line = std::str::from_utf8(&bytes)
            .map_err(|_| ProviderError::protocol("invalid UTF-8 in SSE stream"))?;
        let line = if !self.started {
            self.started = true;
            line.trim_start_matches('\u{feff}')
        } else {
            line
        };
        if line.is_empty() {
            self.dispatch(events);
            return Ok(());
        }
        if line.starts_with(':') {
            return Ok(());
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        // Count framing too: an attacker must not bypass the memory bound with
        // millions of empty data fields (each still allocates a vector entry).
        self.size += line.len() + 1;
        if self.size > MAX_EVENT_BYTES {
            return Err(ProviderError::protocol("SSE event exceeds size limit"));
        }
        match field {
            "event" => self.event = Some(value.to_owned()),
            "data" => self.data.push(value.to_owned()),
            _ => {}
        }
        Ok(())
    }
    fn dispatch(&mut self, events: &mut Vec<SseEvent>) {
        if !self.data.is_empty() {
            events.push(SseEvent {
                event: self.event.take(),
                data: self.data.join("\n"),
            });
        }
        self.event = None;
        self.data.clear();
        self.size = 0;
    }
    pub(crate) fn finish(&mut self) -> Result<Vec<SseEvent>, ProviderError> {
        let mut events = Vec::new();
        if !self.line.is_empty() {
            self.consume_line(&mut events)?;
        }
        // Providers sometimes omit the terminal blank line. Preserve the final
        // payload; protocol decoders still require their own terminal event.
        self.dispatch(&mut events);
        Ok(events)
    }
}

#[cfg(test)]
#[path = "transport_tests.rs"]
mod wire_tests;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_boundaries_multiline_utf8_crlf_bom_and_eof() {
        let wire = "\u{feff}:comment\r\nevent: update\r\ndata: hé\r\ndata: world\r\n\r\ndata: tail"
            .as_bytes();
        for split in 0..=wire.len() {
            let mut parser = SseParser::default();
            let mut out = parser.push(&wire[..split]).unwrap();
            out.extend(parser.push(&wire[split..]).unwrap());
            out.extend(parser.finish().unwrap());
            assert_eq!(
                out,
                vec![
                    SseEvent {
                        event: Some("update".into()),
                        data: "hé\nworld".into()
                    },
                    SseEvent {
                        event: None,
                        data: "tail".into()
                    }
                ]
            );
        }
    }
    #[test]
    fn bad_utf8_and_oversize_are_errors() {
        assert!(SseParser::default().push(&[255, b'\n']).is_err());
        assert!(
            SseParser::default()
                .push(&vec![b'x'; MAX_EVENT_BYTES + 1])
                .is_err()
        );
    }
}
