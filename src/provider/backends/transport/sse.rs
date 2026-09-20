//! SSE body streaming and byte framing, independent of HTTP retry policy.
use super::{http_error, timeout_error};
use crate::provider::ProviderError;
use futures_util::stream::{self, BoxStream};
use reqwest::Response;
use std::{collections::VecDeque, time::Duration};

const MAX_EVENT_BYTES: usize = 8 * 1024 * 1024;
pub(crate) type SseStream = BoxStream<'static, Result<SseEvent, ProviderError>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

pub(super) fn response_stream(response: Response, read_idle: Duration) -> SseStream {
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
mod tests {
    use super::super::tests::{Plan, Server, timeouts};
    use super::super::{client, post_sse_with_timeouts};
    use super::*;
    use crate::provider::ProviderErrorKind;
    use futures_util::StreamExt;
    use reqwest::header::HeaderMap;
    use serde_json::Value;

    #[test]
    fn all_boundaries_multiline_utf8_crlf_bom_and_eof() {
        let wire = "\u{feff}:comment\r\nevent: update\r\ndata: hé\r\ndata: world\r\n\r\ndata: tail";
        let wire = wire.as_bytes();
        for split in 0..=wire.len() {
            let mut parser = SseParser::default();
            let mut out = parser.push(&wire[..split]).unwrap();
            out.extend(parser.push(&wire[split..]).unwrap());
            out.extend(parser.finish().unwrap());
            let out: Vec<_> = out
                .into_iter()
                .map(|event| (event.event, event.data))
                .collect();
            let update = (Some("update".to_owned()), "hé\nworld".to_owned());
            assert_eq!(out, [update, (None, "tail".to_owned())]);
        }
    }

    #[test]
    fn bad_utf8_and_oversize_are_errors() {
        assert!(SseParser::default().push(&[255, b'\n']).is_err());
        let oversized = vec![b'x'; MAX_EVENT_BYTES + 1];
        assert!(SseParser::default().push(&oversized).is_err());
    }

    #[tokio::test]
    async fn body_failure_or_idle_timeout_after_acceptance_is_not_replayed() {
        // (partial output, stalled body): a closed body fails; a stalled one times out.
        for (partial_output, stall_body) in [(true, false), (false, true), (true, true)] {
            let body = if partial_output {
                "data: first\n\n"
            } else {
                ""
            };
            let wire = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 999\r\n\r\n{body}"
            );
            let mut plan = Plan::reply(wire);
            plan.stall_body = stall_body;
            let mut server = Server::start(vec![plan]).await;
            let client = client().unwrap();
            let post = post_sse_with_timeouts(
                &client,
                &server.url,
                HeaderMap::new(),
                &Value::Null,
                timeouts(100, 100),
            );
            let (_, mut stream) = post.await.unwrap();
            if partial_output {
                assert_eq!(stream.next().await.unwrap().unwrap().data, "first");
            }
            let error = stream.next().await.unwrap().unwrap_err();
            if stall_body {
                assert_eq!(error.kind, ProviderErrorKind::Timeout);
            }
            assert!(stream.next().await.is_none());
            server.request().await;
            assert!(server.finish().await.is_empty(), "unexpected replay");
        }
    }
}
