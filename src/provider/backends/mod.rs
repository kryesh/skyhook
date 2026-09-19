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
    scope: String,
    timeouts: ProviderTimeouts,
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
                Protocol::Chat { reasoning_replay } => {
                    let body = chat::encode(&request, reasoning_replay)?;
                    let decoder = chat::Decoder::new(request.model).for_request(&body);
                    (body, Decoder::Chat(decoder))
                }
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
                    Protocol::Chat { .. } | Protocol::Responses => {
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
            Ok(decode_stream(events, decoder, provider.scope))
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
fn decode_stream(events: transport::SseStream, decoder: Decoder, scope: String) -> ResponseStream {
    struct State {
        events: transport::SseStream,
        decoder: Decoder,
        pending: VecDeque<ResponseChunk>,
        done: bool,
        scope: String,
    }
    Box::pin(stream::unfold(
        State {
            events,
            decoder,
            pending: VecDeque::new(),
            done: false,
            scope,
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
    use crate::provider::protocol::{
        AssistantItem, Message, ResponseAssembler, StopReason, ToolDefinition, ToolResult, Usage,
        UserContent,
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

    #[tokio::test]
    async fn replay_is_bound_before_public_exposure_on_terminal_enrichment_and_error() {
        let keep = json!({"keep":[false,null,7]});
        // Anthropic closes and emits signed reasoning before the message terminal
        // event, so an ensuing transport error tests real exposed replay.
        let signed =
            json!({"type":"thinking", "thinking":"summary", "signature":"opaque", "vendor":keep});
        // Responses holds reasoning open until the terminal snapshot enriches it.
        let encrypted = json!({"type":"reasoning", "id":"rs_1", "summary":[],
            "encrypted_content":"opaque", "vendor":keep});
        let plain = json!({"type":"reasoning", "id":"rs_1", "summary":[]});
        for fail_after_item in [false, true] {
            let (native, frames, decoder) = if fail_after_item {
                let frames = [
                    json!({"type":"message_start", "message":{"id":"m", "type":"message", "role":"assistant", "model":"model", "content":[], "usage":{"input_tokens":1,"output_tokens":0}}}),
                    json!({"type":"content_block_start", "index":0, "content_block":signed}),
                    json!({"type":"content_block_stop", "index":0}),
                ];
                let decoder = Decoder::Anthropic(anthropic::Decoder::new("model".into()));
                (&signed, frames, decoder)
            } else {
                let frames = [
                    json!({"type":"response.output_item.added", "output_index":0, "item":plain}),
                    json!({"type":"response.output_item.done", "output_index":0, "item":plain}),
                    json!({"type":"response.completed", "response":{"id":"r_1", "status":"completed", "output":[encrypted]}}),
                ];
                let decoder = Decoder::Responses(responses::Decoder::new("model".into()));
                (&encrypted, frames, decoder)
            };
            let mut events = sse(frames);
            if fail_after_item {
                events.push(Err(ProviderError {
                    retry_after: None,
                    kind: crate::provider::ProviderErrorKind::Transport,
                    message: "scripted disconnect".into(),
                }));
            }
            let events = Box::pin(stream::iter(events));
            let chunks: Vec<_> = decode_stream(events, decoder, "provider-scope".into())
                .collect()
                .await;
            let replays: Vec<_> = chunks
                .iter()
                .filter_map(|chunk| match chunk {
                    Ok(ResponseChunk::ItemEnded {
                        replay: Some(replay),
                        ..
                    }) => Some((replay.scope.as_str(), &replay.payload)),
                    _ => None,
                })
                .collect();
            assert_eq!(replays, [("provider-scope", native)], "{chunks:?}");
            match chunks.last() {
                Some(Err(_)) => assert!(fail_after_item),
                last => assert!(matches!(
                    last,
                    Some(Ok(ResponseChunk::ResponseEnded { .. }))
                )),
            }
        }
    }

    /// Assemble a response whose transport never reaches EOF.
    async fn assemble_without_eof(
        events: Events,
        decoder: Decoder,
    ) -> (Vec<AssistantItem>, Usage, StopReason) {
        let events = Box::pin(stream::iter(events).chain(stream::pending()));
        let mut decoded = decode_stream(events, decoder, "scope".into());
        let mut assembler = ResponseAssembler::default();
        tokio::time::timeout(Duration::from_secs(1), async {
            while let Some(chunk) = decoded.next().await {
                assembler.push(&chunk.unwrap()).unwrap();
            }
        })
        .await
        .expect("terminal response must not wait for HTTP EOF");
        assembler.finish().unwrap()
    }

    #[tokio::test]
    async fn terminal_event_closes_stream_without_waiting_for_upstream_eof() {
        let mut events =
            sse([json!({"choices":[{"index":0,"delta":{"content":"OK"},"finish_reason":"stop"}]})]);
        events.push(Ok(transport::SseEvent {
            event: None,
            data: "[DONE]".into(),
        }));
        let decoder = Decoder::Chat(chat::Decoder::new("model".into()));
        let (items, _, reason) = assemble_without_eof(events, decoder).await;
        assert_eq!((items.len(), reason), (1, StopReason::EndTurn));
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
        let decoder = Decoder::Codex(responses::Decoder::codex("gpt-5".into()));
        let (items, usage, reason) = assemble_without_eof(events, decoder).await;
        assert_eq!(
            (reason, usage.output_tokens, items.len()),
            (StopReason::MaxTokens, 13, 1)
        );
        let replay = items[0].replay.as_ref().unwrap();
        assert_eq!((replay.scope.as_str(), &replay.payload), ("scope", &native));
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

    async fn complete(
        context: &mut dyn ProviderContext,
        request: ModelRequest,
    ) -> ResponseAssembler {
        let mut stream = context.invoke(request).await.unwrap();
        let mut assembler = ResponseAssembler::default();
        while let Some(event) = stream.next().await {
            assembler.push(&event.unwrap()).unwrap();
        }
        assembler
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
                    let (items, usage, stop) = complete(&mut *context, request(model, vec![user.clone()]))
                        .await
                        .finish()
                        .unwrap();
                    assert_eq!(stop, StopReason::ToolUse);
                    let tokens = (usage.input_tokens, usage.cached_input_tokens, usage.output_tokens);
                    assert_eq!(tokens, (20, 80, 6));
                    assert!(items.iter().filter_map(|item| item.replay.as_ref()).all(|replay| !replay.scope.is_empty()));
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
                    complete(&mut *resumed, request(model, history.clone())).await.finish().unwrap();
                    // Same URL/model, different configured provider identity: no private replay crossing.
                    let foreign: Arc<dyn Provider> = Arc::new(configured("other-provider"));
                    let mut foreign_context = foreign.open_context("foreign".into()).unwrap();
                    complete(&mut *foreign_context, request(model, history)).await.finish().unwrap();
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
