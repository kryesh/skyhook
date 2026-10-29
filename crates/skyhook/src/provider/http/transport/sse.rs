//! SSE body streaming and byte framing, independent of HTTP retry policy.
use super::{http_error, timeout_error};
use crate::provider::{ProviderError, ProviderErrorKind::Protocol};
use futures_util::{
    StreamExt,
    stream::{self, BoxStream},
};
use reqwest::Response;
use std::{collections::VecDeque, time::Duration};

const MAX_EVENT_BYTES: usize = 8 * 1024 * 1024;
pub(crate) type SseStream = BoxStream<'static, Result<SseEvent, ProviderError>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// A stateful reader: each input, and the end of input, yields a batch of outputs.
pub(crate) trait Batches: Send + 'static {
    type Input: Send + 'static;
    type Output: Send + 'static;

    fn push(&mut self, input: Self::Input) -> Result<Vec<Self::Output>, ProviderError>;

    fn finish(&mut self) -> Result<Vec<Self::Output>, ProviderError>;

    /// Whether nothing more is read after `output`.
    fn ends(_output: &Self::Output) -> bool {
        false
    }
}

/// `reader`'s outputs over `input`, in order. The stream ends after the first
/// error, the input's or the reader's, or once an output ends it.
pub(crate) fn flatten<B: Batches>(
    input: BoxStream<'static, Result<B::Input, ProviderError>>,
    reader: B,
) -> BoxStream<'static, Result<B::Output, ProviderError>> {
    let state = (input, reader, VecDeque::new(), false);
    Box::pin(stream::unfold(
        state,
        |(mut input, mut reader, mut pending, mut done)| async move {
            loop {
                if let Some(output) = pending.pop_front() {
                    done |= B::ends(&output);
                    return Some((Ok(output), (input, reader, pending, done)));
                }
                if done {
                    return None;
                }
                let batch = match input.next().await {
                    Some(Ok(item)) => reader.push(item),
                    Some(Err(error)) => Err(error),
                    None => {
                        done = true;
                        reader.finish()
                    }
                };
                match batch {
                    Ok(outputs) => pending.extend(outputs),
                    Err(error) => return Some((Err(error), (input, reader, pending, true))),
                }
            }
        },
    ))
}

pub(super) fn response_stream(response: Response, read_idle: Duration) -> SseStream {
    let chunks = stream::unfold(response, move |mut response| async move {
        let chunk = match tokio::time::timeout(read_idle, response.chunk()).await {
            Err(_) => Err(timeout_error("read")),
            Ok(Err(error)) => Err(http_error(error)),
            Ok(Ok(Some(bytes))) => Ok(bytes),
            Ok(Ok(None)) => return None,
        };
        Some((chunk, response))
    });
    flatten(Box::pin(chunks), SseParser::default())
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
                        return Err(Protocol.error("SSE event exceeds size limit"));
                    }
                }
            }
        }
        Ok(events)
    }
    fn consume_line(&mut self, events: &mut Vec<SseEvent>) -> Result<(), ProviderError> {
        let bytes = std::mem::take(&mut self.line);
        let line = std::str::from_utf8(&bytes)
            .map_err(|_| Protocol.error("invalid UTF-8 in SSE stream"))?;
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
            return Err(Protocol.error("SSE event exceeds size limit"));
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
}

impl Batches for SseParser {
    type Input = bytes::Bytes;
    type Output = SseEvent;

    fn push(&mut self, bytes: bytes::Bytes) -> Result<Vec<SseEvent>, ProviderError> {
        SseParser::push(self, &bytes)
    }

    fn finish(&mut self) -> Result<Vec<SseEvent>, ProviderError> {
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
    use super::super::tests::{Plan, Server, stalled_response};
    use super::super::{client, post_sse};
    use super::*;
    use crate::provider::{ProviderErrorKind, http::Timeouts};
    use crate::tests::bounded;
    use futures_util::StreamExt;
    use reqwest::header::HeaderMap;

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
    async fn body_failure_after_acceptance_is_not_replayed() {
        let wire = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 999\r\n\r\ndata: first\n\n";
        let mut server = Server::start(vec![Plan::reply(wire)]).await;
        let client = client().unwrap();
        let post = post_sse(
            &client,
            &server.url,
            HeaderMap::new(),
            "null",
            Timeouts::default(),
        );
        let (_, mut stream) = bounded(post).await.unwrap();
        assert_eq!(stream.next().await.unwrap().unwrap().data, "first");
        assert!(stream.next().await.unwrap().is_err());
        assert!(stream.next().await.is_none());
        server.request().await;
        assert!(server.finish().await.is_empty(), "unexpected replay");
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_body_times_out_after_the_idle_limit() {
        let idle = Duration::from_secs(1);
        for prefix in ["data: first\n\n", ""] {
            let start = tokio::time::Instant::now();
            let mut stream = response_stream(stalled_response(200, prefix), idle);
            if !prefix.is_empty() {
                assert_eq!(stream.next().await.unwrap().unwrap().data, "first");
            }
            let error = stream.next().await.unwrap().unwrap_err();
            assert_eq!(error.kind(), ProviderErrorKind::Timeout);
            assert_eq!(start.elapsed(), idle);
            assert!(stream.next().await.is_none());
        }
    }
}
