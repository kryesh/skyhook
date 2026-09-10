//! Native standard protocol providers and shared codecs.

mod anthropic;
mod api_key_command;
mod chat;
pub mod codex;
mod common;
mod errors;
pub(crate) mod responses;
pub(crate) mod transport;

use crate::provider::{
    Provider, ProviderContext, ProviderError, ProviderFuture, ProviderTimeouts, ResponseStream,
    protocol::{ModelRequest, ResponseChunk},
};
use futures_util::{StreamExt, stream};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use std::collections::VecDeque;

#[derive(Clone, Copy, Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OpenAiApi {
    ChatCompletions,
    Responses,
}

/// Provider-wide reasoning replay serialization for Chat Completions endpoints.
/// Defaults to the widely supported `reasoning_content` field; use `Unsupported`
/// to omit reasoning from requests. Native protocols are unaffected.
#[derive(Clone, Copy, Debug, Default, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChatReasoningReplay {
    Unsupported,
    #[default]
    ReasoningContent,
    Reasoning,
}

#[derive(Clone, Copy)]
enum Protocol {
    Chat,
    Responses,
    Anthropic,
}

#[derive(Clone)]
pub struct NativeProvider {
    client: reqwest::Client,
    endpoint: String,
    headers: HeaderMap,
    api_key_command: Option<api_key_command::ApiKeyCommand>,
    protocol: Protocol,
    scope: String,
    timeouts: ProviderTimeouts,
    chat_reasoning_replay: ChatReasoningReplay,
}

/// `base_url` is an explicit API root (for example `https://api.openai.com/v1`).
/// No model aliases, vendor presets, or implicit API-version segments are applied.
pub fn openai_compatible(
    name: impl Into<String>,
    base_url: impl Into<String>,
    api: OpenAiApi,
    api_key: Option<String>,
) -> Result<NativeProvider, ProviderError> {
    let name = name.into();
    let (suffix, protocol) = match api {
        OpenAiApi::ChatCompletions => ("chat/completions", Protocol::Chat),
        OpenAiApi::Responses => ("responses", Protocol::Responses),
    };
    let mut headers = HeaderMap::new();
    if let Some(key) = api_key {
        let mut value = HeaderValue::from_str(&format!("Bearer {key}"))
            .map_err(|_| common::invalid("invalid API credential header"))?;
        value.set_sensitive(true);
        headers.insert(AUTHORIZATION, value);
    }
    let endpoint = endpoint(&base_url.into(), suffix)?;
    let scope = common::reasoning_scope(&name, &endpoint);
    Ok(NativeProvider {
        client: transport::client()?,
        endpoint,
        headers,
        api_key_command: None,
        protocol,
        scope,
        timeouts: ProviderTimeouts::default(),
        chat_reasoning_replay: ChatReasoningReplay::default(),
    })
}

/// `base_url` is an explicit API root (for example `https://api.anthropic.com/v1`).
pub fn anthropic_api(
    name: impl Into<String>,
    base_url: impl Into<String>,
    api_key: Option<String>,
) -> Result<NativeProvider, ProviderError> {
    let name = name.into();
    let mut headers = HeaderMap::new();
    headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
    if let Some(key) = api_key {
        let mut value = HeaderValue::from_str(&key)
            .map_err(|_| common::invalid("invalid API credential header"))?;
        value.set_sensitive(true);
        headers.insert("x-api-key", value);
    }
    let endpoint = endpoint(&base_url.into(), "messages")?;
    let scope = common::reasoning_scope(&name, &endpoint);
    Ok(NativeProvider {
        client: transport::client()?,
        endpoint,
        headers,
        api_key_command: None,
        protocol: Protocol::Anthropic,
        scope,
        timeouts: ProviderTimeouts::default(),
        chat_reasoning_replay: ChatReasoningReplay::default(),
    })
}

fn endpoint(base: &str, suffix: &str) -> Result<String, ProviderError> {
    let mut url = reqwest::Url::parse(base)
        .map_err(|_| common::invalid("base_url must be an absolute HTTP(S) API-root URL"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(common::invalid(
            "base_url must be HTTP(S), without credentials, query, or fragment",
        ));
    }
    let path = format!("{}/{suffix}", url.path().trim_end_matches('/'));
    url.set_path(&path);
    Ok(url.into())
}

impl NativeProvider {
    /// Resolve a credential lazily on the first valid invocation, overriding any
    /// direct credential. Successful headers are shared across clones/contexts;
    /// failures and cancelled attempts are not cached. Runs /bin/sh -c inside
    /// the invocation future, before HTTP startup timeouts begin, with no separate
    /// command deadline. Dropping that future kills the immediate child process
    /// (not necessarily its descendants); no detached task owns the command.
    #[must_use]
    pub fn with_api_key_command(mut self, command: String) -> Self {
        self.api_key_command = Some(api_key_command::ApiKeyCommand::new(command));
        self
    }

    #[must_use]
    pub fn with_chat_reasoning_replay(mut self, policy: ChatReasoningReplay) -> Self {
        self.chat_reasoning_replay = policy;
        self
    }

    #[must_use]
    pub fn with_timeouts(mut self, timeouts: ProviderTimeouts) -> Self {
        self.timeouts = timeouts;
        self
    }
}

impl Provider for NativeProvider {
    fn open_context(&self, correlation: String) -> Result<Box<dyn ProviderContext>, ProviderError> {
        Ok(Box::new(NativeContext {
            provider: self.clone(),
            correlation,
        }))
    }
}
struct NativeContext {
    provider: NativeProvider,
    correlation: String,
}

impl ProviderContext for NativeContext {
    fn invoke(&mut self, mut request: ModelRequest) -> ProviderFuture {
        let provider = self.provider.clone();
        let correlation = self.correlation.clone();
        Box::pin(async move {
            if request
                .correlation
                .as_ref()
                .is_some_and(|value| value != &correlation)
            {
                return Err(common::invalid(
                    "request correlation does not match its native context",
                ));
            }
            if request.model.trim().is_empty() {
                return Err(common::invalid("model must not be empty"));
            }
            common::filter_reasoning_scope(&mut request, &provider.scope);
            let (body, decoder) = match provider.protocol {
                Protocol::Chat => (
                    chat::encode(&request, provider.chat_reasoning_replay)?,
                    Decoder::Chat(chat::Decoder::new(request.model)),
                ),
                Protocol::Responses => (
                    responses::encode(&request)?,
                    Decoder::Responses(responses::Decoder::new(request.model)),
                ),
                Protocol::Anthropic => (
                    anthropic::encode(&request)?,
                    Decoder::Anthropic(anthropic::Decoder::new(request.model)),
                ),
            };
            let mut headers = provider.headers;
            if let Some(command) = &provider.api_key_command {
                let header = command.header(provider.protocol).await?;
                match provider.protocol {
                    Protocol::Chat | Protocol::Responses => {
                        headers.insert(AUTHORIZATION, header);
                    }
                    Protocol::Anthropic => {
                        headers.insert("x-api-key", header);
                    }
                }
            }
            let events = transport::post_sse_with_timeouts(
                &provider.client,
                &provider.endpoint,
                headers,
                &body,
                provider.timeouts,
            )
            .await?;
            Ok(decode_stream(events, decoder, provider.scope, ()))
        })
    }
}

enum Decoder {
    Chat(chat::Decoder),
    Responses(responses::Decoder),
    Codex(responses::Decoder),
    Anthropic(anthropic::Decoder),
}
impl Decoder {
    fn decode(&mut self, event: &transport::SseEvent) -> Result<Vec<ResponseChunk>, ProviderError> {
        match self {
            Self::Chat(d) => d.decode(event),
            Self::Responses(d) => d.decode(event),
            Self::Codex(d) => d.decode_filtered(event, codex::is_transport_metadata),
            Self::Anthropic(d) => d.decode(event),
        }
    }
    fn finish(&mut self) -> Result<Vec<ResponseChunk>, ProviderError> {
        match self {
            Self::Chat(d) => d.finish(),
            Self::Responses(d) | Self::Codex(d) => d.finish(),
            Self::Anthropic(d) => d.finish(),
        }
    }
}
fn decode_stream<G: Send + 'static>(
    events: transport::SseStream,
    decoder: Decoder,
    scope: String,
    guard: G,
) -> ResponseStream {
    struct State<G> {
        events: transport::SseStream,
        decoder: Decoder,
        pending: VecDeque<ResponseChunk>,
        done: bool,
        scope: String,
        // Codex holds its context lock until the stream ends or is dropped.
        _guard: G,
    }
    Box::pin(stream::unfold(
        State {
            events,
            decoder,
            pending: VecDeque::new(),
            done: false,
            scope,
            _guard: guard,
        },
        |mut state| async move {
            loop {
                if let Some(mut chunk) = state.pending.pop_front() {
                    common::bind_reasoning_scope(&mut chunk, &state.scope);
                    // Terminal protocol events close HTTP immediately rather than
                    // waiting for an upstream connection to close or idle timeout.
                    if matches!(chunk, ResponseChunk::ResponseEnded { .. }) {
                        state.done = true;
                    }
                    return Some((Ok(chunk), state));
                }
                if state.done {
                    return None;
                }
                let result = match state.events.next().await {
                    Some(Ok(event)) => state.decoder.decode(&event),
                    Some(Err(error)) => Err(error),
                    None => {
                        state.done = true;
                        state.decoder.finish()
                    }
                };
                match result {
                    Ok(chunks) => state.pending.extend(chunks),
                    Err(error) => {
                        state.done = true;
                        return Some((Err(error), state));
                    }
                }
            }
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn terminal_event_closes_stream_without_waiting_for_upstream_eof() {
        use crate::provider::protocol::{ResponseAssembler, StopReason};
        use std::time::Duration;
        let events = stream::iter([
            Ok(transport::SseEvent {
                event: None,
                data: serde_json::json!({"choices":[{"index":0,"delta":{"content":"OK"},"finish_reason":"stop"}]}).to_string(),
            }),
            Ok(transport::SseEvent { event: None, data: "[DONE]".into() }),
        ]).chain(stream::pending());
        let mut decoded = decode_stream(
            Box::pin(events),
            Decoder::Chat(chat::Decoder::new("model".into())),
            "scope".into(),
            (),
        );
        let mut assembler = ResponseAssembler::default();
        tokio::time::timeout(Duration::from_secs(1), async {
            while let Some(chunk) = decoded.next().await {
                assembler.push(&chunk.unwrap()).unwrap();
            }
        })
        .await
        .expect("terminal response must not wait for HTTP EOF");
        let (items, _, reason) = assembler.finish().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(reason, StopReason::EndTurn);
    }
}
