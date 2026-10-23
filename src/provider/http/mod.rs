//! The one HTTP provider: a codec, a transport dialect, the composed request
//! headers with their credentials, and the timeouts. Dialect modules construct it.

use std::{sync::Arc, time::Duration};

use futures_util::{TryStreamExt, stream};
use reqwest::Url;

use crate::{
    media::ImageLimits,
    provider::{
        Provider, ProviderContext, ProviderError, ResponseStream,
        codec::{self, BodyPath, Codec, CodecName},
        protocol::{AssistantItem, ContextId, Message, ModelRequest, ResponseEvent, Scope},
    },
};
use images::FittedImages;

pub(crate) mod auth;
pub(crate) mod errors;
pub(crate) mod headers;
pub(crate) mod images;
pub(crate) mod session;
pub(crate) mod transport;

pub(crate) use headers::Headers;
use headers::Value;
pub(crate) use session::Session;

/// Conventions of the HTTP exchange around the codec: a per-turn session
/// header, the vendor's error evidence, and body fields the vendor takes
/// whatever the codec.
#[derive(Clone, Debug)]
pub(crate) struct Transport {
    pub session: Session,
    pub errors: errors::ErrorSignals,
    pub fields: Fields,
}

impl Transport {
    pub(crate) fn plain() -> Self {
        Self {
            session: Session::None,
            errors: errors::ErrorSignals::NONE,
            fields: Fields::default(),
        }
    }
}

/// Body fields the dialect or request settings add beside the codec's, which
/// placements stay clear of.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Fields {
    pub every: Vec<(BodyPath, serde_json::Value)>,
    /// Placed after `every` on requests that constrain the response to a schema.
    pub constrained: Vec<(BodyPath, serde_json::Value)>,
}

impl Fields {
    pub(crate) fn paths(&self) -> impl Iterator<Item = &BodyPath> {
        self.every
            .iter()
            .chain(&self.constrained)
            .map(|(path, _)| path)
    }

    fn place(
        &self,
        request: &ModelRequest,
        body: &mut serde_json::Map<String, serde_json::Value>,
    ) -> Result<(), ProviderError> {
        let constrained = request
            .response_schema
            .is_some()
            .then_some(&self.constrained);
        // These fields lead the body: gateways can route on them before reading a
        // large input array.
        let mut placed = serde_json::Map::new();
        for (path, value) in self.every.iter().chain(constrained.into_iter().flatten()) {
            let key = path.as_str().split('.').next().unwrap_or_default();
            if let Some(written) = body.shift_remove(key) {
                placed.insert(key.to_owned(), written);
            }
            path.set(&mut placed, value.clone())?;
        }
        placed.append(body);
        *body = placed;
        Ok(())
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

#[derive(Clone)]
pub(crate) struct HttpProvider {
    pub(crate) client: reqwest::Client,
    pub(crate) endpoint: Url,
    pub(crate) codec: Codec,
    pub(crate) transport: Transport,
    /// Every source the entry composes, credentials and `Accept` last.
    pub(crate) headers: Headers,
    pub(crate) scope: Scope,
    /// Admission validated these.
    pub(crate) timeouts: Timeouts,
    pub(crate) images: ImageLimits,
}

/// Provider-bound provenance prevents replaying sealed reasoning to a different
/// endpoint, codec or dialect even when protocol and model names happen to match.
/// `name` is the configured provider name and `dialect` the dialect's replay-scope tag.
pub(crate) fn reasoning_scope(
    name: &str,
    endpoint: &str,
    codec: CodecName,
    dialect: &str,
) -> Scope {
    let codec = codec.path_suffix();
    let digest = crate::sha256_hex(format!("{name}\0{endpoint}\0{codec}\0{dialect}"));
    Scope::try_from(digest).expect("digest is nonblank")
}

/// Drop sealed replay issued under another scope; readable reasoning goes wherever
/// its format and model match.
fn filter_reasoning_scope(request: &mut ModelRequest, scope: &Scope) {
    for message in request.messages_mut() {
        if let Message::Assistant(items) = message {
            for item in items {
                if let AssistantItem::Reasoning { replay, .. } = item
                    && replay.as_ref().is_some_and(|replay| {
                        replay.provenance.scope != *scope && codec::common::is_sealed(replay)
                    })
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
            images: Arc::default(),
        }))
    }
}

struct Context {
    provider: HttpProvider,
    context: ContextId,
    session: session::SessionState,
    images: Arc<tokio::sync::Mutex<FittedImages>>,
}

impl ProviderContext for Context {
    fn invoke(&mut self, mut request: ModelRequest) -> ResponseStream {
        let provider = self.provider.clone();
        let context = self.context.clone();
        let session = self.session.clone();
        let images = Arc::clone(&self.images);
        let started = async move {
            filter_reasoning_scope(&mut request, &provider.scope);
            let mut images = images.lock().await;
            images.refresh(&request, provider.images).await?;
            let mut encoded = provider
                .codec
                .encode(&request, images.prepared(), &context)?;
            drop(images);
            provider
                .transport
                .fields
                .place(&request, &mut encoded.body)?;
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
            let body = serde_json::Value::Object(encoded.body).to_string();
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
    use crate::provider::protocol::entries;
    use crate::provider::{
        codec::{
            chat_completions,
            common::{
                self,
                tests::{Reduced, reasoning_item, reduce, request, scope, tool_result, user},
            },
            responses,
        },
        protocol::{Binding, BlockId, Outcome, ReplayFormat, TextBlock, ToolCall},
    };
    use futures_util::StreamExt;
    use serde_json::{Value, json};
    use std::sync::Arc;
    use transport::tests::{Plan, Server, header_values, reply};

    type Events = Vec<Result<transport::SseEvent, ProviderError>>;

    /// A Chat-shaped request with one tool, for provider round trips.
    pub(crate) fn chat_request(model: &str, messages: Vec<Message>) -> ModelRequest {
        ModelRequest {
            context: entries(messages),
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
            context: entries(vec![
                Message::Assistant(vec![
                    reasoning,
                    AssistantItem::tool_call(
                        "fc_1",
                        1,
                        ToolCall::new("call_1", "inspect", json!({"path":"test"})).unwrap(),
                    ),
                ]),
                tool_result("call_1", "inspect", json!({"ok":true}), vec![], false),
            ]),
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
            context: entries(vec![Message::Assistant(vec![item])]),
            ..request("same-model")
        };
        let mut matching = request.clone();
        filter_reasoning_scope(&mut matching, &a);
        assert_eq!(matching, request);
        let mut foreign = request;
        filter_reasoning_scope(&mut foreign, &b);
        let Some(Message::Assistant(parts)) = foreign.messages().next() else {
            unreachable!()
        };
        assert!(parts[0].replay().is_none());
        assert_eq!(parts[0].reasoning_text(), expected_text);
        // Only sealed replay is scoped: readable reasoning replays under any.
        let readable = json!({"type":"reasoning","summary":[{"type":"summary_text","text":"s"}]});
        let free = common::replay(ReplayFormat::Responses, "m", &a, readable, Binding::Free);
        let unsigned = common::replay(ReplayFormat::Messages, "m", &a, json!({}), Binding::Free);
        let signed = common::replay(
            ReplayFormat::Messages,
            "m",
            &a,
            json!({}),
            Binding::Conversation,
        );
        let reasoning = |replay| AssistantItem::reasoning("r", 0, "", Some(replay));
        let items = vec![reasoning(free), reasoning(unsigned), reasoning(signed)];
        let mut mixed = ModelRequest {
            context: entries(vec![Message::Assistant(items)]),
            ..common::tests::request("m")
        };
        filter_reasoning_scope(&mut mixed, &b);
        let Some(Message::Assistant(parts)) = mixed.messages().next() else {
            unreachable!()
        };
        let kept: Vec<_> = parts.iter().map(|item| item.replay().is_some()).collect();
        assert_eq!(kept, [true, true, false]);
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
    async fn chat_round_trip_persists_history_and_replays_readable_reasoning_across_providers() {
        use crate::provider::{
            Provider,
            dialect::{self, Sourced, compatible},
            protocol::{Message, Outcome},
        };
        crate::tests::bounded(async {
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
            let common = dialect::tests::common(&url, Some(Sourced::Literal("k".into())));
            let settings = compatible::Config::default();
            let chat = codec::CodecName::ChatCompletions;
            let configured = |name| dialect::tests::provider(name, &settings, chat, &common);
            let provider: Arc<dyn Provider> = Arc::new(configured("local"));
            let mut context = provider.open_context("initial".parse().unwrap()).unwrap();
            let user = user("find x");
            let reduced = complete(&mut *context, chat_request(model, vec![user.clone()])).await;
            assert_eq!(reduced.completion.outcome(), Outcome::ToolUse);
            let usage = reduced.usage;
            let tokens = (
                usage.input_tokens.unwrap(),
                usage.cached_input_tokens.unwrap(),
                usage.output_tokens.unwrap(),
            );
            assert_eq!(tokens, (20, 80, 6));
            let items = reduced.completion.items().to_vec();
            // Persist/restore canonical history without any codec-specific request fields.
            let serialized = serde_json::to_vec(&Message::Assistant(items)).unwrap();
            let assistant: Message = serde_json::from_slice(&serialized).unwrap();
            let history = vec![
                user,
                assistant.clone(),
                tool_result("call_a", "lookup", json!({"value":42}), vec![], false),
            ];
            drop(context);
            let mut resumed = provider.open_context("resumed".parse().unwrap()).unwrap();
            complete(&mut *resumed, chat_request(model, history.clone())).await;
            // Same URL and model under another configured provider name: readable
            // reasoning crosses.
            let foreign: Arc<dyn Provider> = Arc::new(configured("other-provider"));
            let mut foreign_context = foreign.open_context("foreign".parse().unwrap()).unwrap();
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
            assert_eq!(assistant["reasoning_content"], "plan step");
            assert_eq!(requests[2]["messages"][1], *assistant);
            for request in &requests[1..] {
                assert_eq!(request["messages"][1]["tool_calls"][0]["id"], "call_a");
            }
            assert_eq!(requests[1]["messages"][2]["tool_call_id"], "call_a");
        })
        .await;
    }

    /// A stored image that no longer decodes is sent as a note in its place, on
    /// every request that references it, rather than failing them.
    #[tokio::test]
    async fn an_undecodable_image_is_sent_as_a_note() {
        use crate::{
            media::{AttachmentRef, BlobRef, ImageFormat, ImageRef},
            provider::{
                Provider,
                dialect::{self, Sourced, compatible},
                protocol::UserContent,
            },
        };
        crate::tests::bounded(async {
            let answer = vec![
                json!({"choices":[{"index":0,"delta":{"content":"done"},"finish_reason":"stop"}]}),
            ];
            let (url, server) = serve(vec![answer.clone(), answer]).await;
            let common = dialect::tests::common(&url, Some(Sourced::Literal("k".into())));
            let chat = codec::CodecName::ChatCompletions;
            let settings = compatible::Config::default();
            let provider = dialect::tests::provider("local", &settings, chat, &common);
            let mut context = provider.open_context("images".parse().unwrap()).unwrap();
            let broken = b"\x89PNG\r\n\x1a\nbroken".to_vec();
            let image = ImageRef {
                file: Some("shot.png".into()),
                format: ImageFormat::Png,
                blob: BlobRef::of(&broken),
            };
            let attached = Message::User(vec![UserContent::Attachment {
                attachment: AttachmentRef::Image(image.clone()),
            }]);
            let mut request = chat_request("served-model", vec![attached]);
            request.blobs.insert(image.blob, broken);
            for _ in 0..2 {
                let reduced = complete(&mut *context, request.clone()).await;
                assert_eq!(reduced.completion.outcome(), Outcome::Answer);
            }
            for request in server.finish().await {
                let body: Value =
                    serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
                // A turn left with only text is sent as text.
                let note = body["messages"][0]["content"].as_str().unwrap();
                assert!(note.starts_with("[Image shot.png not sent: image data is invalid"));
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
        let user = user("hi");
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
        let endpoint = Url::parse(&server.url).unwrap();
        let codec = codec::Codec::Responses(responses::Dialect::stateless());
        HttpProvider {
            client: transport::client().unwrap(),
            scope: reasoning_scope("test", endpoint.as_str(), codec.name(), "compatible"),
            endpoint,
            codec,
            transport: Transport {
                session,
                ..Transport::plain()
            },
            headers: composed,
            timeouts: Timeouts::default(),
            images: common::tests::LIMITS,
        }
    }

    /// A completed, empty Responses stream with extra `headers`.
    fn completed(headers: &str) -> Plan {
        let done = json!({"type":"response.completed",
            "response":{"id":"r","status":"completed","output":[]}});
        let headers = format!("Content-Type: text/event-stream\r\n{headers}");
        Plan::reply(reply("200 OK", &headers, &format!("data: {done}\n\n")))
    }

    /// A 401 on a command key's first use is a plain authentication failure,
    /// whatever its body says; once the key has been accepted, a 401 reports it
    /// expired. Either way the next request carries a fresh key.
    #[tokio::test]
    async fn unauthorized_command_keys_are_refetched_and_expire_only_after_acceptance() {
        use crate::provider::{Provider, ProviderErrorKind::*};
        let dir = tempfile::tempdir().unwrap();
        let count = format!("'{}'", dir.path().join("count").to_str().unwrap());
        let accepted = || completed("");
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
        let request = reasoning_tool_request(&provider.scope);
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
        let sticky = |token: &str| completed(&format!("x-turn: {token}\r\n"));
        let server = Server::start(vec![sticky("first"), sticky("ignored")]).await;
        let session = Session::StickyTurn {
            header: reqwest::header::HeaderName::from_static("x-turn"),
        };
        let command = format!("printf x >> {quoted}; printf resolved-key");
        let provider = keyed_by_command(&server, session, command);
        let mut context = provider.open_context("c".parse().unwrap()).unwrap();
        let mut request = reasoning_tool_request(&provider.scope);
        let turn: Vec<_> = request.messages().cloned().collect();
        // Neither an unpolled nor an invalid invocation resolves the credential.
        drop(context.invoke(request.clone()));
        let mut invalid = request.clone();
        invalid.reasoning = Some("adaptive".into());
        assert!(context.invoke(invalid).next().await.unwrap().is_err());
        assert!(!count.exists());
        request.context = entries(vec![user("start")]);
        complete(&mut *context, request.clone()).await;
        request.context.extend(entries(turn));
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
