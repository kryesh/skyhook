//! Shared, cancellation-safe HTTP/SSE transport. A dropped stream drops its HTTP body.
use crate::provider::{ProviderError, ProviderErrorKind};
use futures_util::stream::{self, BoxStream};
use reqwest::{
    Client, Response,
    header::{ACCEPT, CONTENT_TYPE, HeaderMap},
};
use serde_json::Value;
use std::{collections::VecDeque, time::Duration};

pub(crate) const START_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const READ_TIMEOUT: Duration = Duration::from_secs(120);
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

/// Retry only explicit rate-limit rejections and connect failures,
/// at most twice. Once response headers are accepted no request is ever replayed,
/// including malformed streams, EOF, idle timeouts, or caller cancellation.
pub(crate) async fn post_sse(
    client: &Client,
    url: &str,
    headers: HeaderMap,
    body: &Value,
) -> Result<SseStream, ProviderError> {
    post_sse_policy(client, url, headers, body, 3).await
}

/// Codex and other continuation protocols can disable even overload retries.
pub(crate) async fn post_sse_once(
    client: &Client,
    url: &str,
    headers: HeaderMap,
    body: &Value,
) -> Result<SseStream, ProviderError> {
    post_sse_policy(client, url, headers, body, 1).await
}

async fn post_sse_policy(
    client: &Client,
    url: &str,
    mut headers: HeaderMap,
    body: &Value,
    attempts: u32,
) -> Result<SseStream, ProviderError> {
    headers.insert(ACCEPT, "text/event-stream".parse().expect("static header"));
    let deadline = tokio::time::Instant::now() + START_TIMEOUT;
    for attempt in 0..attempts {
        let sent = tokio::time::timeout_at(
            deadline,
            client.post(url).headers(headers.clone()).json(body).send(),
        )
        .await
        .map_err(|_| timeout_error("startup"))?;
        let response = match sent {
            Ok(response) => response,
            Err(error) if error.is_connect() && attempt + 1 < attempts => {
                tokio::time::sleep(Duration::from_millis(200 * (1 << attempt))).await;
                continue;
            }
            Err(error) => return Err(http_error(error)),
        };
        let status = response.status();
        if !status.is_success() {
            if status.as_u16() == 429 && attempt + 1 < attempts {
                let retry_after = response.headers().get("retry-after");
                let delay = match retry_after {
                    Some(value) => value
                        .to_str()
                        .ok()
                        .and_then(|s| s.parse::<u64>().ok())
                        .filter(|s| *s <= 2)
                        .map(Duration::from_secs),
                    None => Some(Duration::from_millis(200 * (1 << attempt))),
                };
                // Never retry earlier than Retry-After. Long or HTTP-date values
                // exceed our small retry policy and are returned to the caller.
                if let Some(delay) =
                    delay.filter(|delay| tokio::time::Instant::now() + *delay < deadline)
                {
                    drop(response);
                    tokio::time::sleep(delay).await;
                    continue;
                }
            }
            return Err(status_error(response).await);
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
        return Ok(response_stream(response));
    }
    unreachable!("bounded retry loop always returns")
}

async fn status_error(mut response: Response) -> ProviderError {
    let status = response.status().as_u16();
    let mut body = Vec::new();
    // Bound the whole error-body read, not only individual chunks.
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        while body.len() < MAX_ERROR_BYTES {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    body.extend_from_slice(&chunk[..chunk.len().min(MAX_ERROR_BYTES - body.len())])
                }
                _ => break,
            }
        }
    })
    .await;
    let native: Option<Value> = serde_json::from_slice(&body).ok();
    let code = native
        .as_ref()
        .and_then(|v| {
            v.pointer("/error/code")
                .or_else(|| v.pointer("/error/type"))
        })
        .and_then(Value::as_str)
        .unwrap_or("");
    let kind = if matches!(code, "context_length_exceeded" | "context_window_exceeded") {
        ProviderErrorKind::ContextWindowExceeded
    } else {
        match status {
            401 | 403 => ProviderErrorKind::Authentication,
            408 | 504 => ProviderErrorKind::Timeout,
            429 => ProviderErrorKind::RateLimited,
            400 | 404 | 413 | 422 => ProviderErrorKind::InvalidRequest,
            _ => ProviderErrorKind::Response,
        }
    };
    // Never echo service bodies: proxies and API errors can reflect credentials,
    // request text, or image payloads. Only local classifications enter logs.
    ProviderError {
        kind,
        message: format!("provider HTTP {status}"),
    }
}

fn response_stream(response: Response) -> SseStream {
    struct State {
        response: Response,
        parser: SseParser,
        pending: VecDeque<SseEvent>,
        done: bool,
    }
    Box::pin(stream::unfold(
        State {
            response,
            parser: SseParser::default(),
            pending: VecDeque::new(),
            done: false,
        },
        |mut state| async move {
            loop {
                if let Some(event) = state.pending.pop_front() {
                    return Some((Ok(event), state));
                }
                if state.done {
                    return None;
                }
                let read = tokio::time::timeout(READ_TIMEOUT, state.response.chunk()).await;
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
