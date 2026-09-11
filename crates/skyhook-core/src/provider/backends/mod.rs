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
    use crate::provider::protocol::{
        Message, ResponseAssembler, StopReason, ToolDefinition, ToolResult, UserContent,
    };
    use serde_json::{Value, json};
    use std::{sync::Arc, time::Duration};

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
    // Synthetic llama-swap/vLLM acceptance exercises the configured provider and
    // replay scope, while sharing transport's complete HTTP request reader.
    async fn serve(bodies: Vec<String>) -> (String, tokio::task::JoinHandle<Vec<Value>>) {
        let replies = bodies.into_iter().map(|body| format!(
            "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{}",
            body.len(), body
        )).collect();
        let (url, task) = transport::tests::server(replies).await;
        let root = format!("{}/v1", url.trim_end_matches("/responses"));
        let requests = tokio::spawn(async move {
            task.await
                .unwrap()
                .into_iter()
                .map(|request| {
                    assert!(request.starts_with("POST /v1/chat/completions HTTP/1.1"));
                    serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap()
                })
                .collect()
        });
        (root, requests)
    }

    fn chat_stream(frames: Vec<Value>) -> String {
        let mut body: String = frames
            .into_iter()
            .map(|frame| format!("data: {frame}\n\n"))
            .collect();
        body.push_str("data: [DONE]\n\n");
        body
    }

    fn request(model: &str, messages: Vec<Message>) -> ModelRequest {
        ModelRequest {
            messages,
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
                for model in ["served-model", "another-model"] {
                    let first = chat_stream(vec![
                        json!({"choices":[{"delta":{"reasoning_content":"plan ","content":"checking"}}],
                            "usage":{"prompt_tokens":100,"completion_tokens":1,"total_tokens":101}}),
                        json!({"choices":[{"index":0,"delta":{"reasoning":"step","tool_calls":[
                            {"index":0,"id":"call_a","type":"function","function":{"name":"lookup"}},
                            {"index":0,"function":{"arguments":"{\"q\":\"x\"}"}}
                        ]}}]}),
                        json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
                        json!({"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":6,
                            "total_tokens":106,"prompt_tokens_details":{"cached_tokens":80}}}),
                    ]);
                    let answer = chat_stream(vec![
                        json!({"choices":[{"index":0,"delta":{"content":"done"},"finish_reason":"stop"}]}),
                    ]);
                    let (url, server) = serve(vec![first, answer.clone(), answer]).await;
                    // Exercise the constructor default without a builder override.
                    let configured = |name| {
                        let provider = openai_compatible(name, &url, OpenAiApi::ChatCompletions, None).unwrap();
                        match policy {
                            Some(policy) => provider.with_chat_reasoning_replay(policy),
                            None => provider,
                        }
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
                    assert_eq!(usage.input_tokens, 20);
                    assert_eq!(usage.cached_input_tokens, 80);
                    assert_eq!(usage.output_tokens, 6);
                    assert!(
                        items
                            .iter()
                            .filter_map(|item| item.replay.as_ref())
                            .all(|replay| !replay.scope.is_empty())
                    );
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
                    complete(&mut *resumed, request(model, history.clone()))
                        .await
                        .finish()
                        .unwrap();
                    // Same URL/model, different configured provider identity: no private replay crossing.
                    let foreign: Arc<dyn Provider> = Arc::new(configured("other-provider"));
                    let mut foreign_context = foreign.open_context("foreign".into()).unwrap();
                    complete(&mut *foreign_context, request(model, history.clone()))
                        .await
                        .finish()
                        .unwrap();
                    assert_eq!(serde_json::to_vec(&assistant).unwrap(), serialized);
                    let requests = server.await.unwrap();
                    assert_eq!(requests[0]["n"], 1);
                    let assistant = &requests[1]["messages"][1];
                    assert_eq!(assistant["content"], "checking");
                    for candidate in ["reasoning_content", "reasoning"] {
                        if field == Some(candidate) {
                            assert_eq!(assistant[candidate], "plan step");
                        } else {
                            assert!(assistant.get(candidate).is_none());
                        }
                    }
                    assert_eq!(requests[1]["messages"][1]["tool_calls"][0]["id"], "call_a");
                    assert_eq!(requests[1]["messages"][2]["tool_call_id"], "call_a");
                    for candidate in ["reasoning_content", "reasoning"] {
                        assert!(requests[2]["messages"][1].get(candidate).is_none());
                    }
                    assert_eq!(requests[2]["messages"][1]["tool_calls"][0]["id"], "call_a");
                }
            }
        })
        .await
        .unwrap();
    }
}
