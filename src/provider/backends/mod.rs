//! Native standard protocol providers and shared codecs.

mod anthropic;
mod api_key_command;
mod chat;
pub mod codex;
mod common;
mod errors;
pub(crate) mod responses;
pub(crate) mod transport;

#[cfg(test)]
pub(crate) use chat::validate_schema as validate_chat_schema;

use crate::provider::{
    Provider, ProviderContext, ProviderError, ResponseStream,
    protocol::{ContextId, ModelRequest, ResponseEvent, Scope},
};
use futures_util::{StreamExt, TryStreamExt, stream};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use std::collections::VecDeque;
use std::time::Duration;

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
pub(crate) enum Protocol {
    Chat {
        reasoning_replay: ChatReasoningReplay,
    },
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
    scope: Scope,
    timeouts: ProviderTimeouts,
}

/// Startup is a per-attempt deadline; read-idle resets per body chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProviderTimeouts {
    pub startup: Duration,
    pub read_idle: Duration,
}

impl Default for ProviderTimeouts {
    fn default() -> Self {
        Self {
            startup: Duration::from_secs(600),
            read_idle: Duration::from_secs(600),
        }
    }
}

/// Parsed and validated provider settings; construct through [`NativeSettings::new`].
#[derive(Clone)]
pub(crate) struct NativeSettings {
    endpoint: reqwest::Url,
    protocol: Protocol,
    timeouts: ProviderTimeouts,
}

impl NativeSettings {
    #[cfg(test)]
    pub(crate) fn timeouts(&self) -> ProviderTimeouts {
        self.timeouts
    }

    pub(crate) fn new(
        base: &str,
        protocol: Protocol,
        timeouts: ProviderTimeouts,
    ) -> Result<Self, ProviderError> {
        validate_timeouts(timeouts)?;
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
        let suffix = match protocol {
            Protocol::Chat { .. } => "chat/completions",
            Protocol::Responses => "responses",
            Protocol::Anthropic => "messages",
        };
        let path = format!("{}/{suffix}", url.path().trim_end_matches('/'));
        url.set_path(&path);
        Ok(Self {
            endpoint: url,
            protocol,
            timeouts,
        })
    }

    pub(crate) fn build(
        self,
        name: &str,
        api_key: Option<String>,
    ) -> Result<NativeProvider, ProviderError> {
        let mut headers = HeaderMap::new();
        if matches!(self.protocol, Protocol::Anthropic) {
            headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        }
        if let Some(key) = api_key {
            let (header, value) = match self.protocol {
                Protocol::Anthropic => (reqwest::header::HeaderName::from_static("x-api-key"), key),
                _ => (AUTHORIZATION, format!("Bearer {key}")),
            };
            let mut value = HeaderValue::from_str(&value)
                .map_err(|_| common::invalid("invalid API credential header"))?;
            value.set_sensitive(true);
            headers.insert(header, value);
        }
        let endpoint: String = self.endpoint.into();
        let scope = common::reasoning_scope(name, &endpoint);
        Ok(NativeProvider {
            client: transport::client()?,
            endpoint,
            headers,
            api_key_command: None,
            protocol: self.protocol,
            scope,
            timeouts: self.timeouts,
        })
    }
}

fn validate_timeouts(timeouts: ProviderTimeouts) -> Result<(), ProviderError> {
    if [timeouts.startup, timeouts.read_idle]
        .into_iter()
        .any(|value| value.is_zero() || std::time::Instant::now().checked_add(value).is_none())
    {
        return Err(common::invalid(
            "startup_timeout_secs and read_idle_timeout_secs must be positive, representable durations",
        ));
    }
    Ok(())
}

impl NativeProvider {
    /// Resolve the credential by running `/bin/sh -c command` on first invocation,
    /// overriding any direct credential. Only a successful result is cached.
    pub fn with_api_key_command(mut self, command: String) -> Result<Self, ProviderError> {
        if command.trim().is_empty() {
            return Err(common::invalid("api_key_command must not be blank"));
        }
        self.api_key_command = Some(api_key_command::ApiKeyCommand::new(command));
        Ok(self)
    }
}

impl Provider for NativeProvider {
    fn open_context(&self, context: ContextId) -> Result<Box<dyn ProviderContext>, ProviderError> {
        Ok(Box::new(NativeContext {
            provider: self.clone(),
            context,
        }))
    }
}
struct NativeContext {
    provider: NativeProvider,
    context: ContextId,
}

impl ProviderContext for NativeContext {
    fn invoke(&mut self, mut request: ModelRequest) -> ResponseStream {
        let provider = self.provider.clone();
        let context = self.context.clone();
        let started = async move {
            if request.model.trim().is_empty() {
                return Err(common::invalid("model must not be empty"));
            }
            common::filter_reasoning_scope(&mut request, &provider.scope);
            let (body, decoder) = match provider.protocol {
                Protocol::Chat { reasoning_replay } => {
                    let body = chat::encode(&request, reasoning_replay)?;
                    let decoder =
                        chat::Decoder::new(request.model, provider.scope).for_request(&body);
                    (body, Decoder::Chat(decoder))
                }
                Protocol::Responses => (
                    responses::encode(&request, &context)?,
                    Decoder::Responses(responses::Decoder::new(request.model, provider.scope)),
                ),
                Protocol::Anthropic => (
                    anthropic::encode(&request)?,
                    Decoder::Anthropic(anthropic::Decoder::new(request.model, provider.scope)),
                ),
            };
            let mut headers = provider.headers;
            if let Some(command) = &provider.api_key_command {
                let header = command.header(provider.protocol).await?;
                match provider.protocol {
                    Protocol::Chat { .. } | Protocol::Responses => {
                        headers.insert(AUTHORIZATION, header);
                    }
                    Protocol::Anthropic => {
                        headers.insert("x-api-key", header);
                    }
                }
            }
            let (_, events) = transport::post_sse_with_timeouts(
                &provider.client,
                &provider.endpoint,
                headers,
                &body,
                provider.timeouts,
            )
            .await?;
            Ok(decode_stream(events, decoder))
        };
        Box::pin(stream::once(started).try_flatten())
    }
}

enum Decoder {
    Chat(chat::Decoder),
    Responses(responses::Decoder),
    Codex(responses::Decoder),
    Anthropic(anthropic::Decoder),
}
impl Decoder {
    fn decode(&mut self, event: &transport::SseEvent) -> Result<Vec<ResponseEvent>, ProviderError> {
        match self {
            Self::Chat(d) => d.decode(event),
            Self::Responses(d) => d.decode(event),
            Self::Codex(d) => d.decode_filtered(event, codex::is_transport_metadata),
            Self::Anthropic(d) => d.decode(event),
        }
    }
    fn finish(&mut self) -> Result<Vec<ResponseEvent>, ProviderError> {
        match self {
            Self::Chat(d) => d.finish(),
            Self::Responses(d) | Self::Codex(d) => d.finish(),
            Self::Anthropic(d) => d.finish(),
        }
    }
}
fn decode_stream(frames: transport::SseStream, decoder: Decoder) -> ResponseStream {
    struct State {
        frames: transport::SseStream,
        decoder: Decoder,
        pending: VecDeque<ResponseEvent>,
        done: bool,
    }
    Box::pin(stream::unfold(
        State {
            frames,
            decoder,
            pending: VecDeque::new(),
            done: false,
        },
        |mut state| async move {
            loop {
                if let Some(event) = state.pending.pop_front() {
                    // Terminal protocol events close HTTP immediately rather than
                    // waiting for an upstream connection to close or idle timeout.
                    if matches!(event, ResponseEvent::End(_)) {
                        state.done = true;
                    }
                    return Some((Ok(event), state));
                }
                if state.done {
                    return None;
                }
                let result = match state.frames.next().await {
                    Some(Ok(frame)) => state.decoder.decode(&frame),
                    Some(Err(error)) => Err(error),
                    None => {
                        state.done = true;
                        state.decoder.finish()
                    }
                };
                match result {
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

#[cfg(test)]
mod tests {
    use super::common::tests::{Reduced, reduce};
    use super::*;
    use crate::provider::protocol::{
        CutReason, Message, Outcome, ToolDefinition, ToolResult, UserContent,
    };
    use serde_json::{Value, json};
    use std::{sync::Arc, time::Duration};

    type Events = Vec<Result<transport::SseEvent, ProviderError>>;

    fn sse(frames: impl IntoIterator<Item = Value>) -> Events {
        let event = |data| Ok(transport::SseEvent { event: None, data });
        frames
            .into_iter()
            .map(|frame| event(frame.to_string()))
            .collect()
    }

    #[test]
    fn direct_native_settings_cannot_bypass_protocol_or_timer_validation() {
        let url = "https://example.com/custom/v1/";
        let defaults = ProviderTimeouts::default;
        let chat = Protocol::Chat {
            reasoning_replay: ChatReasoningReplay::Reasoning,
        };
        for protocol in [chat, Protocol::Responses] {
            let settings = NativeSettings::new(url, protocol, defaults()).unwrap();
            let provider = settings.build("arbitrary/vendor", None).unwrap();
            for timeouts in [
                ProviderTimeouts {
                    startup: Duration::ZERO,
                    ..defaults()
                },
                ProviderTimeouts {
                    read_idle: Duration::MAX,
                    ..defaults()
                },
            ] {
                assert!(NativeSettings::new(url, protocol, timeouts).is_err());
            }
            assert!(provider.with_api_key_command("  ".into()).is_err());
        }
        let settings = |url: &str| NativeSettings::new(url, Protocol::Responses, defaults());
        let endpoint = settings(url).unwrap().endpoint;
        assert_eq!(endpoint.as_str(), "https://example.com/custom/v1/responses");
        for invalid in [
            "/v1",
            "ftp://example.com/v1",
            "https://user:secret@example.com/v1",
            "https://example.com/v1?q=secret",
            "https://example.com/v1#fragment",
        ] {
            assert!(
                !settings(invalid)
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("secret")
            );
        }
    }

    /// Reduce a response whose transport never reaches EOF.
    async fn reduce_without_eof(events: Events, decoder: Decoder) -> Reduced {
        let events = Box::pin(stream::iter(events).chain(stream::pending()));
        let decoded = decode_stream(events, decoder);
        let events: Vec<_> = tokio::time::timeout(Duration::from_secs(1), decoded.collect())
            .await
            .expect("terminal response must not wait for HTTP EOF");
        reduce(events.into_iter().map(Result::unwrap))
    }

    #[tokio::test]
    async fn terminal_event_closes_stream_without_waiting_for_upstream_eof() {
        let mut events =
            sse([json!({"choices":[{"index":0,"delta":{"content":"OK"},"finish_reason":"stop"}]})]);
        events.push(Ok(transport::SseEvent {
            event: None,
            data: "[DONE]".into(),
        }));
        let decoder = Decoder::Chat(chat::Decoder::new("model".into(), common::tests::scope()));
        let reduced = reduce_without_eof(events, decoder).await;
        assert_eq!(
            (reduced.items().len(), reduced.completion.outcome()),
            (1, Outcome::Answer)
        );
    }

    /// Codex reports truncation with an empty terminal `output`: the partial call is
    /// discarded, while streamed native reasoning keeps its scoped replay and usage.
    #[tokio::test]
    async fn codex_truncated_tool_preserves_native_reasoning_and_usage() {
        let native = json!({"type":"reasoning", "id":"rs_private", "summary":[],
            "encrypted_content":"opaque+/=", "future_native":{"state":"keep"}});
        let call = json!({"type":"function_call", "id":"fc_1", "call_id":"call_1",
            "name":"inspect", "arguments":"{\"path\":"});
        let events = sse([
            json!({"type":"response.output_item.added", "output_index":0, "item":native}),
            json!({"type":"response.output_item.done", "output_index":0, "item":native}),
            json!({"type":"response.output_item.added", "output_index":1, "item":call}),
            json!({"type":"response.output_item.done", "output_index":1, "item":call}),
            json!({"type":"response.incomplete", "response":{"id":"truncated", "status":"incomplete",
                "incomplete_details":{"reason":"max_output_tokens"}, "output":[],
                "usage":{"input_tokens":8,"output_tokens":13}}}),
        ]);
        let decoder = Decoder::Codex(responses::Decoder::codex(
            "gpt-5".into(),
            common::tests::scope(),
        ));
        let reduced = reduce_without_eof(events, decoder).await;
        assert_eq!(
            (
                reduced.completion.outcome(),
                reduced.usage.output_tokens,
                reduced.items().len()
            ),
            (Outcome::Cut(CutReason::MaxTokens), 13, 1)
        );
        let replay = reduced.items()[0].replay().unwrap();
        assert_eq!(
            (replay.provenance.scope.as_str(), &replay.payload),
            ("scope", &native)
        );
    }

    // Synthetic llama-swap/vLLM acceptance exercises the configured provider and
    // replay scope, while sharing transport's complete HTTP request reader.
    async fn serve(frames: Vec<Vec<Value>>) -> (String, transport::tests::Server) {
        let plans = frames
            .into_iter()
            .map(|frames| {
                let body: String = frames
                    .iter()
                    .map(|frame| format!("data: {frame}\n\n"))
                    .collect();
                let body = body + "data: [DONE]\n\n";
                transport::tests::Plan::reply(transport::tests::reply(
                    "200 OK",
                    "Content-Type: text/event-stream\r\n",
                    &body,
                ))
            })
            .collect();
        let server = transport::tests::Server::start(plans).await;
        let root = format!("{}/v1", server.url.trim_end_matches("/responses"));
        (root, server)
    }

    fn request(model: &str, messages: Vec<Message>) -> ModelRequest {
        ModelRequest {
            history: messages,
            tools: vec![ToolDefinition {
                name: "lookup".into(),
                description: "Find a value".into(),
                input_schema: json!({"type":"object", "properties":{"q":{"type":"string"}}}),
            }],
            max_output_tokens: Some(512),
            ..common::tests::request(model)
        }
    }

    async fn complete(context: &mut dyn ProviderContext, request: ModelRequest) -> Reduced {
        let events: Vec<_> = context.invoke(request).collect().await;
        reduce(events.into_iter().map(Result::unwrap))
    }

    #[tokio::test]
    async fn chat_variations_and_scoped_reasoning_round_trip_through_public_interface() {
        tokio::time::timeout(Duration::from_secs(10), async {
            for (policy, field) in [
                (None, Some("reasoning_content")),
                (Some(ChatReasoningReplay::Reasoning), Some("reasoning")),
                (Some(ChatReasoningReplay::Unsupported), None),
            ] {
                let model = "served-model";
                    let first = vec![
                        json!({"choices":[{"delta":{"reasoning_content":"plan ","content":"checking"}}],
                            "usage":{"prompt_tokens":100,"completion_tokens":1,"total_tokens":101}}),
                        json!({"choices":[{"index":0,"delta":{"reasoning":"step","tool_calls":[
                            {"index":0,"id":"call_a","type":"function","function":{"name":"lookup"}},
                            {"index":0,"function":{"arguments":"{\"q\":\"x\"}"}}
                        ]}}]}),
                        json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
                        json!({"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":6,
                            "total_tokens":106,"prompt_tokens_details":{"cached_tokens":80}}}),
                    ];
                    let answer = vec![
                        json!({"choices":[{"index":0,"delta":{"content":"done"},"finish_reason":"stop"}]}),
                    ];
                    let (url, server) = serve(vec![first, answer.clone(), answer]).await;
                    let protocol = Protocol::Chat { reasoning_replay: policy.unwrap_or_default() };
                    let configured = |name: &str| {
                        NativeSettings::new(&url, protocol, ProviderTimeouts::default())
                            .unwrap()
                            .build(name, None)
                            .unwrap()
                    };
                    let provider: Arc<dyn Provider> = Arc::new(configured("local"));
                    let mut context = provider.open_context("initial".into()).unwrap();
                    let user = Message::User(vec![UserContent::Text {
                        text: "find x".into(),
                    }]);
                    let reduced = complete(&mut *context, request(model, vec![user.clone()])).await;
                    assert_eq!(reduced.completion.outcome(), Outcome::ToolUse);
                    let usage = reduced.usage;
                    let tokens = (usage.input_tokens, usage.cached_input_tokens, usage.output_tokens);
                    assert_eq!(tokens, (20, 80, 6));
                    let items = reduced.completion.items().to_vec();
                    // Persist/restore canonical history without any backend-specific request fields.
                    let serialized = serde_json::to_vec(&Message::Assistant(items)).unwrap();
                    let assistant: Message = serde_json::from_slice(&serialized).unwrap();
                    let history = vec![
                        user,
                        assistant.clone(),
                        Message::Tool(vec![ToolResult {
                            call_id: "call_a".into(),
                            name: "lookup".into(),
                            result: json!({"value":42}),
                            is_error: false,
                            images: Vec::new(),
                        }]),
                    ];
                    drop(context);
                    let mut resumed = provider.open_context("resumed".into()).unwrap();
                    complete(&mut *resumed, request(model, history.clone())).await;
                    // Same URL/model, different configured provider identity: no private replay crossing.
                    let foreign: Arc<dyn Provider> = Arc::new(configured("other-provider"));
                    let mut foreign_context = foreign.open_context("foreign".into()).unwrap();
                    complete(&mut *foreign_context, request(model, history)).await;
                    assert_eq!(serde_json::to_vec(&assistant).unwrap(), serialized);
                    let requests: Vec<Value> = server
                        .finish()
                        .await
                        .iter()
                        .map(|request| {
                            assert!(request.starts_with("POST /v1/chat/completions HTTP/1.1"));
                            serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap()
                        })
                        .collect();
                    assert!(requests[0].get("n").is_none());
                    let assistant = &requests[1]["messages"][1];
                    assert_eq!(assistant["content"], "checking");
                    for candidate in ["reasoning_content", "reasoning"] {
                        let replayed = (field == Some(candidate)).then(|| json!("plan step"));
                        assert_eq!(assistant.get(candidate), replayed.as_ref());
                        assert!(requests[2]["messages"][1].get(candidate).is_none());
                    }
                    for request in &requests[1..] {
                        assert_eq!(request["messages"][1]["tool_calls"][0]["id"], "call_a");
                    }
                    assert_eq!(requests[1]["messages"][2]["tool_call_id"], "call_a");
            }
        })
        .await
        .unwrap();
    }
}
