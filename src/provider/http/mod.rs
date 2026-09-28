//! The one HTTP provider: a codec, a transport dialect, the composed request
//! headers with their credentials, and the timeouts. Dialect modules construct it.

use std::time::Duration;

use futures_util::{TryStreamExt, stream};
use reqwest::Url;

use crate::provider::{
    Provider, ProviderContext, ProviderError, ResponseStream,
    codec::{self, Codec, CodecName},
    protocol::{AssistantItem, ContextId, Message, ModelRequest, ResponseEvent, Scope},
};

pub(crate) mod auth;
pub(crate) mod errors;
pub(crate) mod headers;
pub(crate) mod session;
pub(crate) mod transport;

pub(crate) use headers::Headers;
use headers::Value;
pub(crate) use session::Session;

/// Conventions of the HTTP exchange around the codec: a per-turn session
/// header and the vendor's error evidence.
#[derive(Clone, Debug)]
pub(crate) struct Transport {
    pub session: Session,
    pub errors: errors::ErrorSignals,
}

impl Transport {
    pub(crate) fn plain() -> Self {
        Self {
            session: Session::None,
            errors: errors::ErrorSignals::NONE,
        }
    }
}

/// Startup is a per-attempt deadline; read-idle resets per body chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Timeouts {
    pub startup: Duration,
    pub read_idle: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            startup: Duration::from_secs(600),
            read_idle: Duration::from_secs(600),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "startup_timeout_secs and read_idle_timeout_secs must be positive, representable durations"
)]
pub struct TimeoutError;

impl Timeouts {
    pub(crate) fn validate(self) -> Result<(), TimeoutError> {
        if [self.startup, self.read_idle]
            .into_iter()
            .any(|value| value.is_zero() || std::time::Instant::now().checked_add(value).is_none())
        {
            return Err(TimeoutError);
        }
        Ok(())
    }
}

/// What a dialect module assembles for one complete model profile.
pub(crate) struct Build<'a> {
    /// The client shared by every profile under the configured provider.
    pub client: reqwest::Client,
    /// The configured provider name; with the endpoint, codec and dialect it
    /// scopes private reasoning replay.
    pub name: &'a str,
    /// The dialect's replay-scope tag.
    pub scope_tag: &'a str,
    pub endpoint: Url,
    pub codec: Codec,
    pub transport: Transport,
    /// Every source the entry composes, credentials and `Accept` last.
    pub headers: Headers,
    pub timeouts: Timeouts,
}

#[derive(Clone)]
pub(crate) struct HttpProvider {
    client: reqwest::Client,
    endpoint: Url,
    codec: Codec,
    transport: Transport,
    headers: Headers,
    scope: Scope,
    timeouts: Timeouts,
}

impl HttpProvider {
    /// Admission validated the timeouts and the shared client is already built.
    pub(crate) fn new(build: Build<'_>) -> Self {
        let scope = reasoning_scope(
            build.name,
            build.endpoint.as_str(),
            build.codec.name(),
            build.scope_tag,
        );
        Self {
            client: build.client,
            endpoint: build.endpoint,
            codec: build.codec,
            transport: build.transport,
            headers: build.headers,
            scope,
            timeouts: build.timeouts,
        }
    }

    #[cfg(test)]
    pub(crate) fn scope(&self) -> &Scope {
        &self.scope
    }
}

/// Provider-bound provenance prevents replaying private reasoning to a different
/// endpoint, codec or dialect even when protocol and model names happen to match.
fn reasoning_scope(name: &str, endpoint: &str, codec: CodecName, dialect: &str) -> Scope {
    let codec = codec.path_suffix();
    let digest = crate::sha256_hex(format!("{name}\0{endpoint}\0{codec}\0{dialect}"));
    Scope::try_from(digest).expect("digest is nonblank")
}

fn filter_reasoning_scope(request: &mut ModelRequest, scope: &Scope) {
    for message in request.messages_mut() {
        if let Message::Assistant(items) = message {
            for item in items {
                if let AssistantItem::Reasoning { replay, .. } = item
                    && replay
                        .as_ref()
                        .is_some_and(|replay| replay.provenance.scope != *scope)
                {
                    *replay = None;
                }
            }
        }
    }
}

impl Provider for HttpProvider {
    fn open_context(&self, context: ContextId) -> Result<Box<dyn ProviderContext>, ProviderError> {
        Ok(Box::new(Context {
            provider: self.clone(),
            context,
            session: session::SessionState::default(),
        }))
    }
}

struct Context {
    provider: HttpProvider,
    context: ContextId,
    session: session::SessionState,
}

impl ProviderContext for Context {
    fn invoke(&mut self, mut request: ModelRequest) -> ResponseStream {
        let provider = self.provider.clone();
        let context = self.context.clone();
        let session = self.session.clone();
        let started = async move {
            filter_reasoning_scope(&mut request, &provider.scope);
            let encoded = provider.codec.encode(&request, &context)?;
            // The codec's placements, then the turn's session header, replace
            // the composed headers before any value is produced.
            let mut headers = provider.headers.clone();
            for (name, value) in &encoded.headers {
                headers.insert(name.clone(), Value::Fixed(value.clone()));
            }
            if let Some((name, value)) = session.prepare(&provider.transport.session, &request) {
                headers.insert(name, Value::Fixed(value));
            }
            let headers = headers.resolve().await?;
            let body = encoded.body.to_string();
            let decoder = provider.codec.decoder(
                request.model.into(),
                provider.scope,
                body.as_bytes(),
                provider.transport.errors,
            );
            let (response, events) = transport::post_sse(
                &provider.client,
                provider.endpoint.as_str(),
                headers.map(),
                body,
                provider.timeouts,
            )
            .await
            .map_err(|failure| match failure {
                transport::Failure::Rejected(rejection) => {
                    let error = provider.codec.error(&rejection, provider.transport.errors);
                    if rejection.unauthorized() {
                        headers.unauthorized(error)
                    } else {
                        error
                    }
                }
                transport::Failure::Other(error) => error,
            })?;
            headers.served();
            session.observe(&provider.transport.session, &response);
            Ok::<_, ProviderError>(transport::flatten(events, decoder))
        };
        Box::pin(stream::once(started).try_flatten())
    }
}

impl transport::Batches for codec::Decoder {
    type Input = transport::SseEvent;
    type Output = ResponseEvent;

    fn push(&mut self, event: transport::SseEvent) -> Result<Vec<ResponseEvent>, ProviderError> {
        self.decode(&event)
    }

    fn finish(&mut self) -> Result<Vec<ResponseEvent>, ProviderError> {
        codec::Decoder::finish(self)
    }

    /// Terminal protocol events close HTTP immediately rather than waiting for
    /// an upstream connection to close or idle timeout.
    fn ends(event: &ResponseEvent) -> bool {
        matches!(event, ResponseEvent::End(_))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::provider::{
        codec::{
            chat_completions,
            common::{
                self,
                tests::{Reduced, reasoning_item, reduce, request, scope},
            },
            responses,
        },
        protocol::{Binding, BlockId, Outcome, ReplayFormat, TextBlock, ToolCall, ToolResult},
    };
    use futures_util::StreamExt;
    use serde_json::{Value, json};
    use std::sync::Arc;
    use transport::tests::{Plan, Server, header_values, reply};

    type Events = Vec<Result<transport::SseEvent, ProviderError>>;

    /// A Chat-shaped request with one tool, for provider round trips.
    pub(crate) fn chat_request(model: &str, messages: Vec<Message>) -> ModelRequest {
        ModelRequest {
            history: messages,
            tools: vec![crate::provider::protocol::ToolDefinition {
                name: "lookup".into(),
                description: "Find a value".into(),
                input_schema: json!({"type":"object", "properties":{"q":{"type":"string"}}}),
            }],
            max_output_tokens: std::num::NonZeroU64::new(512),
            ..request(model)
        }
    }

    /// A tool loop whose assistant turn replays [`reasoning_item`] under `scope`.
    pub(crate) fn reasoning_tool_request(scope: &Scope) -> ModelRequest {
        let replay = common::replay(
            ReplayFormat::Responses,
            "gpt-5",
            scope,
            reasoning_item(),
            Binding::Free,
        );
        // Native reasoning without a readable summary replays from its envelope alone.
        let reasoning = AssistantItem::Reasoning {
            id: "rs_1".to_owned().try_into().unwrap(),
            position: 0.into(),
            blocks: Vec::new(),
            replay: Some(replay),
        };
        ModelRequest {
            history: vec![
                Message::Assistant(vec![
                    reasoning,
                    AssistantItem::tool_call(
                        "fc_1",
                        1,
                        ToolCall::new("call_1", "inspect", json!({"path":"test"})).unwrap(),
                    ),
                ]),
                Message::Tool(vec![ToolResult {
                    call_id: "call_1".into(),
                    name: "inspect".into(),
                    result: json!({"ok":true}),
                    images: vec![],
                    is_error: false,
                }]),
            ],
            max_output_tokens: std::num::NonZeroU64::new(100),
            ..request("gpt-5")
        }
    }

    #[test]
    fn replay_scope_names_the_provider_endpoint_codec_and_dialect() {
        let endpoint = "https://a.example/v1/responses";
        let responses = CodecName::Responses;
        let a = reasoning_scope("api", endpoint, responses, "compatible");
        let b = reasoning_scope(
            "api",
            "https://b.example/v1/responses",
            responses,
            "compatible",
        );
        for other in [
            b.clone(),
            reasoning_scope("alias", endpoint, responses, "compatible"),
            reasoning_scope("api", endpoint, CodecName::ChatCompletions, "compatible"),
            reasoning_scope("api", endpoint, responses, "openai"),
        ] {
            assert_ne!(a, other);
        }
        let native = json!({"type":"reasoning","encrypted_content":"private"});
        let replay = common::replay(
            ReplayFormat::Responses,
            "same-model",
            &a,
            native,
            Binding::Free,
        );
        let mut item = AssistantItem::reasoning("reasoning-0", 0, "summary", Some(replay));
        let AssistantItem::Reasoning { blocks, .. } = &mut item else {
            unreachable!()
        };
        blocks.push(TextBlock {
            id: BlockId::try_from("summary-1".to_owned()).unwrap(),
            position: 1.into(),
            text: "second summary".into(),
        });
        let expected_text = item.reasoning_text();
        let request = ModelRequest {
            history: vec![Message::Assistant(vec![item])],
            ..request("same-model")
        };
        let mut matching = request.clone();
        filter_reasoning_scope(&mut matching, &a);
        assert_eq!(matching, request);
        let mut foreign = request;
        filter_reasoning_scope(&mut foreign, &b);
        let Message::Assistant(parts) = &foreign.history[0] else {
            unreachable!()
        };
        assert!(parts[0].replay().is_none());
        assert_eq!(parts[0].reasoning_text(), expected_text);
    }

    fn sse(frames: impl IntoIterator<Item = Value>) -> Events {
        let event = |data| Ok(transport::SseEvent { event: None, data });
        frames
            .into_iter()
            .map(|frame| event(frame.to_string()))
            .collect()
    }

    /// Reduce a response whose transport never reaches EOF.
    async fn reduce_without_eof(events: Events, decoder: codec::Decoder) -> Reduced {
        let events = Box::pin(stream::iter(events).chain(stream::pending()));
        let decoded = transport::flatten(events, decoder);
        // A terminal response must not wait for HTTP EOF.
        let events: Vec<_> = crate::tests::bounded(decoded.collect()).await;
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
        let decoder = codec::Decoder::ChatCompletions(chat_completions::Decoder::new(
            "model".into(),
            scope(),
            chat_completions::ReasoningFormat::Text,
            errors::ErrorSignals::NONE,
            crate::media::BlobDigest::of(b""),
        ));
        let reduced = reduce_without_eof(events, decoder).await;
        assert_eq!(
            (reduced.items().len(), reduced.completion.outcome()),
            (1, Outcome::Answer)
        );
    }

    /// A Chat server answering each request with the given SSE frames, and its API root.
    pub(crate) async fn serve(frames: Vec<Vec<Value>>) -> (String, Server) {
        let plans = frames
            .into_iter()
            .map(|frames| {
                let body: String = frames
                    .iter()
                    .map(|frame| format!("data: {frame}\n\n"))
                    .collect();
                let body = body + "data: [DONE]\n\n";
                Plan::reply(reply(
                    "200 OK",
                    "Content-Type: text/event-stream\r\n",
                    &body,
                ))
            })
            .collect();
        let server = Server::start(plans).await;
        (format!("{}/v1", server.root()), server)
    }

    /// One request through a context, reduced.
    pub(crate) async fn complete(
        context: &mut dyn ProviderContext,
        request: ModelRequest,
    ) -> Reduced {
        let events: Vec<_> = context.invoke(request).collect().await;
        reduce(events.into_iter().map(Result::unwrap))
    }

    #[tokio::test]
    async fn chat_round_trip_persists_history_and_scopes_replay_by_provider_name() {
        use crate::provider::{
            Provider,
            dialect::{self, Placement, Placements, Sourced, compatible},
            protocol::{Message, Outcome, ToolResult, UserContent},
        };
        crate::tests::bounded(async {
            for (replay, field) in [
                (None, Some("reasoning_content")),
                (Some(Placement::Field(codec::path("reasoning"))), Some("reasoning")),
                (Some(Placement::Omitted), None),
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
                let placements = Placements {
                    reasoning_replay: replay,
                    ..Placements::default()
                };
                let common = dialect::tests::common(&url, Some(Sourced::Literal("k".into())));
                let settings = compatible::Config::default();
                let chat = codec::CodecName::ChatCompletions;
                let configured =
                    |name| dialect::tests::placed(name, &settings, &placements, chat, &common);
                let provider: Arc<dyn Provider> = Arc::new(configured("local"));
                let mut context = provider.open_context("initial".parse().unwrap()).unwrap();
                let user = Message::User(vec![UserContent::Text {
                    text: "find x".into(),
                }]);
                let reduced =
                    complete(&mut *context, chat_request(model, vec![user.clone()])).await;
                assert_eq!(reduced.completion.outcome(), Outcome::ToolUse);
                let usage = reduced.usage;
                let tokens = (
                    usage.input_tokens,
                    usage.cached_input_tokens,
                    usage.output_tokens,
                );
                assert_eq!(tokens, (20, 80, 6));
                let items = reduced.completion.items().to_vec();
                // Persist/restore canonical history without any codec-specific request fields.
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
                let mut resumed = provider.open_context("resumed".parse().unwrap()).unwrap();
                complete(&mut *resumed, chat_request(model, history.clone())).await;
                // Same URL and model under another configured provider name: no
                // private replay crosses.
                let foreign: Arc<dyn Provider> = Arc::new(configured("other-provider"));
                let mut foreign_context =
                    foreign.open_context("foreign".parse().unwrap()).unwrap();
                complete(&mut *foreign_context, chat_request(model, history)).await;
                assert_eq!(serde_json::to_vec(&assistant).unwrap(), serialized);
                let requests: Vec<Value> = server
                    .finish()
                    .await
                    .iter()
                    .map(|request| {
                        assert!(request.starts_with("POST /v1/chat/completions HTTP/1.1"));
                        assert_eq!(header_values(request, "authorization"), ["Bearer k"]);
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
        .await;
    }

    /// The transport's `Accept` displaces an entry's before any value is
    /// produced: the entry's command never runs, so it holds no lease that
    /// would turn a later 401 on the fixed key into an expiry.
    #[tokio::test]
    async fn entry_accept_is_displaced_before_its_command_runs() {
        use crate::provider::{
            Provider, ProviderErrorKind,
            dialect::{self, Sourced, compatible},
            protocol::UserContent,
        };
        let dir = tempfile::tempdir().unwrap();
        let count = dir.path().join("count");
        let answer =
            json!({"choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":"stop"}]});
        let stream = format!("data: {answer}\n\ndata: [DONE]\n\n");
        let events = "Content-Type: text/event-stream\r\n";
        let refused = r#"{"error":{"message":"revoked"}}"#;
        let server = Server::start(vec![
            Plan::reply(reply("200 OK", events, &stream)),
            Plan::reply(reply("401 Unauthorized", "", refused)),
        ])
        .await;
        let root = format!("{}/v1", server.url.trim_end_matches("/responses"));
        let mut common = dialect::tests::common(&root, Some(Sourced::Literal("k".into())));
        let command = format!(
            "printf x >> '{}'; printf text/html",
            count.to_str().unwrap()
        );
        common
            .headers
            .insert("accept".into(), Sourced::Command { command });
        let settings = compatible::Config::default();
        let chat = codec::CodecName::ChatCompletions;
        let provider = dialect::tests::provider("test", &settings, chat, &common);
        let mut context = provider.open_context("c".parse().unwrap()).unwrap();
        let user = Message::User(vec![UserContent::Text { text: "hi".into() }]);
        let request = chat_request("model", vec![user]);
        complete(&mut *context, request.clone()).await;
        let events: Vec<_> = context.invoke(request).collect().await;
        let error = events.into_iter().find_map(Result::err).unwrap();
        assert_eq!(error.kind(), ProviderErrorKind::Authentication);
        for request in server.finish().await {
            assert_eq!(header_values(&request, "accept"), ["text/event-stream"]);
        }
        assert!(!count.exists());
    }

    /// A Responses provider at `server` whose bearer key a command produces.
    fn keyed_by_command(server: &Server, session: Session, command: String) -> HttpProvider {
        use headers::{CommandValue, ValueField};
        let key = CommandValue::new(command, ValueField::ApiKey).with_prefix(Some("Bearer "));
        let mut composed = Headers::default();
        composed.insert(reqwest::header::AUTHORIZATION, headers::Value::Command(key));
        HttpProvider::new(Build {
            client: transport::client().unwrap(),
            name: "test",
            scope_tag: "compatible",
            endpoint: Url::parse(&server.url).unwrap(),
            codec: codec::Codec::Responses(responses::Dialect::stateless()),
            transport: Transport {
                session,
                ..Transport::plain()
            },
            headers: composed,
            timeouts: Timeouts::default(),
        })
    }

    /// A 401 on a command key's first use is a plain authentication failure,
    /// whatever its body says; once the key has been accepted, a 401 reports it
    /// expired. Either way the next request carries a fresh key.
    #[tokio::test]
    async fn unauthorized_command_keys_are_refetched_and_expire_only_after_acceptance() {
        use crate::provider::{Provider, ProviderErrorKind::*};
        let dir = tempfile::tempdir().unwrap();
        let count = format!("'{}'", dir.path().join("count").to_str().unwrap());
        let done = json!({"type":"response.completed",
            "response":{"id":"r","status":"completed","output":[]}});
        let stream = format!("data: {done}\n\n");
        let accepted = || {
            Plan::reply(reply(
                "200 OK",
                "Content-Type: text/event-stream\r\n",
                &stream,
            ))
        };
        let limited = r#"{"error":{"code":"rate_limit_exceeded"}}"#;
        let refused = || {
            Plan::reply(reply(
                "401 Unauthorized",
                "Content-Type: application/json\r\n",
                limited,
            ))
        };
        let plans = vec![refused(), accepted(), refused(), accepted()];
        let server = Server::start(plans).await;
        let command = format!("printf x >> {count}; printf key-%s \"$(wc -c < {count})\"");
        let provider = keyed_by_command(&server, Session::None, command);
        let mut context = provider.open_context("c".parse().unwrap()).unwrap();
        let request = reasoning_tool_request(provider.scope());
        let mut failures = Vec::new();
        for _ in 0..4 {
            let events: Vec<_> = context.invoke(request.clone()).collect().await;
            failures.push(
                events
                    .iter()
                    .find_map(|event| event.as_ref().err().map(|error| error.kind())),
            );
        }
        assert_eq!(
            failures,
            [Some(Authentication), None, Some(CredentialExpired), None]
        );
        let keys: Vec<_> = server
            .finish()
            .await
            .iter()
            .map(|request| header_values(request, "authorization"))
            .collect();
        assert_eq!(
            keys,
            [
                ["Bearer key-1"],
                ["Bearer key-2"],
                ["Bearer key-2"],
                ["Bearer key-3"]
            ]
        );
    }

    /// Credentials resolve only for a request that passed validation, placements
    /// override them, and a sticky turn header is held across the turn.
    #[tokio::test]
    async fn invoke_validates_before_credentials_and_holds_turn_state() {
        use crate::provider::Provider;
        let dir = tempfile::tempdir().unwrap();
        let count = dir.path().join("count");
        let quoted = format!("'{}'", count.to_str().unwrap());
        let done = json!({"type":"response.completed",
            "response":{"id":"r","status":"completed","output":[]}});
        let sticky = |token: &str| {
            let headers = format!("Content-Type: text/event-stream\r\nx-turn: {token}\r\n");
            Plan::reply(reply("200 OK", &headers, &format!("data: {done}\n\n")))
        };
        let server = Server::start(vec![sticky("first"), sticky("ignored")]).await;
        let session = Session::StickyTurn {
            header: reqwest::header::HeaderName::from_static("x-turn"),
        };
        let command = format!("printf x >> {quoted}; printf resolved-key");
        let provider = keyed_by_command(&server, session, command);
        let mut context = provider.open_context("c".parse().unwrap()).unwrap();
        let mut request = reasoning_tool_request(provider.scope());
        let turn = request.history.clone();
        // Neither an unpolled nor an invalid invocation resolves the credential.
        drop(context.invoke(request.clone()));
        let mut unnamed = request.clone();
        unnamed.response_schema = Some(crate::provider::protocol::ResponseSchema {
            name: String::new(),
            schema: json!({"type":"object"}),
        });
        assert!(context.invoke(unnamed).next().await.unwrap().is_err());
        assert!(!count.exists());
        request.history = vec![Message::User(vec![
            crate::provider::protocol::UserContent::Text {
                text: "start".into(),
            },
        ])];
        complete(&mut *context, request.clone()).await;
        request.history.extend(turn);
        complete(&mut *context, request).await;
        let requests = server.finish().await;
        assert_eq!(requests.len(), 2);
        for request in &requests {
            assert_eq!(
                header_values(request, "authorization"),
                ["Bearer resolved-key"]
            );
        }
        assert!(header_values(&requests[0], "x-turn").is_empty());
        assert_eq!(header_values(&requests[1], "x-turn"), ["first"]);
        assert_eq!(std::fs::read(&count).unwrap(), b"x");
    }
}
