//! openrouter.ai: Chat with the `reasoning` object, verbatim `reasoning_details`
//! replay (plain `reasoning` for a response without readable details), `cache_control` on
//! content parts, usage on every stream, and `x-session-id` affinity; Messages
//! for Anthropic models, with their tool-name alphabet; Responses stateless with
//! the same affinity header and a top-level `cache_control`. Chat and Responses
//! take OpenAI's tool-name alphabet and strict schemas.
//! Provider routing rides the body and bearer keys authenticate on every codec;
//! attribution headers name Skyhook.

use reqwest::header::HeaderValue;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{BaseUrl, Dialect, DialectConfig, DialectError, Profile};
use crate::provider::{
    ProviderErrorKind,
    codec::{
        BodyPath, BreakpointBudget, CacheControl, CacheTtl, Codec, CodecName, Locations, NonEmpty,
        StateRole, ToolNames, UnsignedReasoning,
        chat_completions::{self, EmptyContent, NativeReplay, ReasoningReplay, UsageRequest},
        header, messages, path, responses,
    },
    http::{
        Fields, Transport,
        errors::{ErrorSignals, error_object},
    },
};

fn chat() -> chat_completions::Dialect {
    chat_completions::Dialect {
        effort: NonEmpty::one(const { path("reasoning.effort") }),
        reasoning_replay: ReasoningReplay::Details(NativeReplay {
            objects: NonEmpty::one(const { path("reasoning_details") }),
            unsigned: UnsignedReasoning::Replayed(Locations::one(const { path("reasoning") })),
        }),
        empty_content: EmptyContent::Null,
        usage_request: UsageRequest::Implicit,
        cache_control: CacheControl::Sent { ttl: None },
        // The strictest cap among the services behind it.
        breakpoints: BreakpointBudget::new(4),
        ..chat_completions::Dialect::compatible()
    }
}

/// Moderation names its reasons under `error.metadata`. A 402 during a request
/// is transient when the router says when to retry; without a hint it is
/// exhausted credit.
fn statuses(
    status: Option<u16>,
    native: &serde_json::Value,
    retry_after: Option<Duration>,
) -> Option<ProviderErrorKind> {
    #[derive(Deserialize)]
    struct Metadata {
        reasons: Option<serde_json::Value>,
    }
    #[derive(Deserialize)]
    struct Moderated {
        metadata: Metadata,
    }
    match status? {
        403 => {
            let moderated = Moderated::deserialize(error_object(native)).ok()?;
            moderated
                .metadata
                .reasons
                .map(|_| ProviderErrorKind::InvalidRequest)
        }
        402 => retry_after.map(|_| ProviderErrorKind::Unavailable),
        _ => None,
    }
}

/// The router's canonical error types, which every codec's errors carry.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ErrorType {
    ContextLengthExceeded,
    MaxTokensExceeded,
    TokenLimitExceeded,
    StringTooLong,
    Authentication,
    PermissionDenied,
    PaymentRequired,
    RateLimitExceeded,
    ProviderOverloaded,
    ProviderUnavailable,
    InvalidRequest,
    InvalidPrompt,
    NotFound,
    PreconditionFailed,
    PayloadTooLarge,
    Unprocessable,
    ContentPolicyViolation,
    Refusal,
    InvalidImage,
    ImageTooLarge,
    ImageTooSmall,
    UnsupportedImageFormat,
    ImageNotFound,
    ImageDownloadFailed,
    Timeout,
    /// Masked internal errors and upstream ones outside every category say
    /// no more than the status.
    Server,
    Unmapped,
    #[serde(other)]
    Unknown,
}

/// The type where Messages and Responses carry it: in `error`, and beside it.
#[derive(Deserialize)]
struct Typed {
    error_type: ErrorType,
}

/// Chat carries it under `error.metadata`.
#[derive(Deserialize)]
struct Metadata {
    metadata: Typed,
}

impl ErrorType {
    fn kind(self) -> Option<ProviderErrorKind> {
        use ProviderErrorKind::*;
        Some(match self {
            Self::ContextLengthExceeded => ContextWindowExceeded,
            Self::Authentication | Self::PermissionDenied => Authentication,
            Self::PaymentRequired => Billing,
            Self::RateLimitExceeded => RateLimited,
            Self::ProviderOverloaded | Self::ProviderUnavailable => Unavailable,
            Self::Timeout => Timeout,
            Self::MaxTokensExceeded
            | Self::TokenLimitExceeded
            | Self::StringTooLong
            | Self::InvalidRequest
            | Self::InvalidPrompt
            | Self::NotFound
            | Self::PreconditionFailed
            | Self::PayloadTooLarge
            | Self::Unprocessable
            | Self::ContentPolicyViolation
            | Self::Refusal
            | Self::InvalidImage
            | Self::ImageTooLarge
            | Self::ImageTooSmall
            | Self::UnsupportedImageFormat
            | Self::ImageNotFound
            | Self::ImageDownloadFailed => InvalidRequest,
            Self::Server | Self::Unmapped | Self::Unknown => return None,
        })
    }

    fn chat(native: &serde_json::Value) -> Option<ProviderErrorKind> {
        let error = Metadata::deserialize(error_object(native)).ok()?;
        error.metadata.error_type.kind()
    }

    fn messages(native: &serde_json::Value) -> Option<ProviderErrorKind> {
        Typed::deserialize(error_object(native))
            .ok()?
            .error_type
            .kind()
    }

    fn responses(native: &serde_json::Value) -> Option<ProviderErrorKind> {
        Typed::deserialize(native).ok()?.error_type.kind()
    }
}

fn transport(codec: CodecName) -> Transport {
    let errors = match codec {
        CodecName::ChatCompletions => ErrorSignals(|status, native, retry_after| {
            statuses(status, native, retry_after).or_else(|| ErrorType::chat(native))
        }),
        CodecName::Responses => ErrorSignals(|status, native, retry_after| {
            statuses(status, native, retry_after).or_else(|| ErrorType::responses(native))
        }),
        CodecName::Messages => ErrorSignals(|status, native, retry_after| {
            statuses(status, native, retry_after).or_else(|| ErrorType::messages(native))
        }),
    };
    // A response schema routes only to endpoints that honour it, routing or not.
    let constrained = vec![(const { path("provider.require_parameters") }, json!(true))];
    Transport {
        errors,
        fields: Fields {
            every: Vec::new(),
            constrained,
        },
        ..Transport::plain()
    }
}

crate::provider::settings::settings! {
    /// The `provider` preferences object and the `models` fallback list.
    #[derive(Default)]
    pub struct Routing => RoutingPatch {
        @flatten { pub provider: ProviderPreferences }
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub fallback_models: Vec<String> => default,
    }
}

crate::provider::settings::settings! {
    /// The `provider` object. `require_parameters` is forced on whenever a response
    /// schema is sent, so no endpoint that ignores it is routed to.
    #[derive(Default)]
    pub struct ProviderPreferences => ProviderPreferencesPatch {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub order: Vec<String> => default,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub only: Vec<String> => default,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub ignore: Vec<String> => default,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub allow_fallbacks: Option<bool> => default,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub require_parameters: Option<bool> => default,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub data_collection: Option<DataCollection> => default,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub quantizations: Vec<String> => default,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub zdr: Option<bool> => default,
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DataCollection {
    Allow,
    Deny,
}

impl Routing {
    fn fields(&self) -> Vec<(BodyPath, serde_json::Value)> {
        let mut every = Vec::new();
        if self.provider != ProviderPreferences::default() {
            let provider = serde_json::to_value(&self.provider).expect("preferences serialize");
            every.push((const { path("provider") }, provider));
        }
        if !self.fallback_models.is_empty() {
            every.push((const { path("models") }, json!(self.fallback_models)));
        }
        every
    }
}

/// Provider-only options; request settings are declared separately.
pub type Options = crate::provider::settings::Empty;

crate::provider::settings::settings! {
    #[derive(Default)]
    pub struct Config => Patch {
        /// Provider routing preferences and fallback models.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub routing: Option<Routing> => object(Routing),
        /// Cache breakpoint lifetime for models that take one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub cache_ttl: Option<CacheTtl> => default,
        /// The role runtime state takes: `user` (default) sends it as user content;
        /// `system` needs a server that accepts system messages mid-conversation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub state_role: Option<StateRole> => optional(StateRole),
    }
}

impl DialectConfig for Config {
    fn base_url() -> BaseUrl {
        BaseUrl::Default("https://openrouter.ai/api/v1")
    }

    fn admit(&self, codec: CodecName) -> Result<Profile, DialectError> {
        let transport = transport(codec);
        // Each codec keeps its own family's conventions.
        let openai = |mut codec| {
            super::openai::conventions(&mut codec);
            codec
        };
        let mut codec = match codec {
            CodecName::ChatCompletions => openai(Codec::ChatCompletions(chat())),
            CodecName::Responses => openai(Codec::Responses(responses::Dialect {
                cache_control: CacheControl::Sent { ttl: None },
                unsigned: UnsignedReasoning::Replayed(()),
                ..super::openai::responses()
            })),
            CodecName::Messages => Codec::Messages(messages::Dialect {
                tool_names: ToolNames::Anthropic,
                ..messages::Dialect::standard()
            }),
        };
        if let Some(ttl) = codec.cache_ttl_mut() {
            *ttl = self.cache_ttl;
        }
        codec.identity_mut().cache_key = Locations::one(header("x-session-id"));
        let mut profile = Profile::new(
            codec,
            transport,
            Dialect::Openrouter,
            super::compatible::IMAGES,
        );
        profile.state_role(self.state_role, StateRole::User);
        if let Some(routing) = &self.routing {
            profile.transport.fields.every = routing.fields();
            profile.discriminate("routing", routing);
        }
        for (name, value) in [
            ("http-referer", "https://github.com/kryesh/skyhook"),
            ("x-openrouter-title", "Skyhook"),
        ] {
            profile.fixed(name, HeaderValue::from_static(value));
        }
        Ok(profile)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{dialect::tests::head, http::transport::tests::header_values};

    const CODECS: [CodecName; 3] = [
        CodecName::ChatCompletions,
        CodecName::Responses,
        CodecName::Messages,
    ];

    #[test]
    fn replay_identity_tracks_routing_not_cache_lifetime() {
        for codec in CODECS {
            let scope = |yaml| {
                crate::yaml::parse::<Config>(yaml)
                    .unwrap()
                    .admit(codec)
                    .unwrap()
                    .scope
            };
            assert_eq!(scope("{}"), scope("cache_ttl: 1h"));
            let routed = scope("routing: {order: [first], fallback_models: [backup]}");
            for other in [
                "{}",
                "routing: {order: [second], fallback_models: [backup]}",
                "routing: {order: [first], fallback_models: [other]}",
                "routing: {order: [first], only: [first], fallback_models: [backup]}",
                "routing: {order: [first], ignore: [second], fallback_models: [backup]}",
            ] {
                assert_ne!(routed, scope(other), "{codec}: {other}");
            }
        }
    }

    #[test]
    fn routing_overlay_is_fieldwise_and_null_resets_the_whole_object() {
        use crate::provider::settings::{Patch as _, Settings};
        use serde_json::json;

        let lower: Patch = serde_json::from_value(json!({"routing": {
            "order": ["first"], "only": ["first", "second"], "allow_fallbacks": true,
            "require_parameters": true, "fallback_models": ["fallback"]
        }}))
        .unwrap();
        let higher_json = json!({"routing": {
            "order": [], "ignore": ["third"], "allow_fallbacks": false, "require_parameters": null
        }});
        let higher: Patch = serde_json::from_value(higher_json.clone()).unwrap();
        assert_eq!(serde_json::to_value(&higher).unwrap(), higher_json);
        let merged = lower.overlay(&higher);
        let config = Config::resolve(&merged).unwrap();
        let routing = config.routing.unwrap();
        assert!(routing.provider.order.is_empty());
        assert_eq!(routing.provider.only, ["first", "second"]);
        assert_eq!(routing.provider.ignore, ["third"]);
        assert_eq!(routing.provider.allow_fallbacks, Some(false));
        assert_eq!(routing.provider.require_parameters, None);
        assert_eq!(routing.fallback_models, ["fallback"]);

        let clear: Patch = serde_json::from_value(json!({"routing": null})).unwrap();
        let cleared = merged.overlay(&clear);
        assert_eq!(Config::resolve(&cleared).unwrap().routing, None);
        let empty: Patch = serde_json::from_value(json!({"routing": {}})).unwrap();
        assert_eq!(
            Config::resolve(&cleared.overlay(&empty)).unwrap().routing,
            Some(Routing::default())
        );
        assert_eq!(lower.overlay(&empty), lower);
        assert!(serde_json::from_value::<Patch>(json!({"routing": {"unknown": 1}})).is_err());
    }

    /// Chat marks the system prompt and each earlier request's prefix end within the
    /// strictest upstream cap, so an upstream that reads only at marked boundaries
    /// reuses the previous request's cache; the oldest seams give way first.
    #[tokio::test]
    async fn chat_keeps_earlier_prefix_ends_within_the_breakpoint_budget() {
        use crate::provider::{
            codec::{Cache, common, common::tests::user},
            protocol::{AssistantItem, Message, SystemSegment, entries},
        };
        let mut codec = Config::default()
            .admit(CodecName::ChatCompletions)
            .unwrap()
            .codec;
        *codec.cache_mut().unwrap() = Cache::Explicit;
        let mut request = super::super::tests::request();
        request.system = vec![SystemSegment {
            text: "stable".into(),
            cache: true,
        }];
        let marked = |request: &crate::provider::protocol::ModelRequest| -> Vec<bool> {
            let context = "session".parse().unwrap();
            let body = codec
                .encode(request, Default::default(), &context)
                .unwrap()
                .body;
            let messages = body["messages"].as_array().unwrap();
            messages
                .iter()
                .map(|message| message.to_string().contains("cache_control"))
                .collect()
        };
        assert_eq!(marked(&request), [true, true]);
        let exchange = |request: &mut crate::provider::protocol::ModelRequest, turn| {
            let answer = AssistantItem::text("a", 0, "answer");
            (request.context).extend(entries([Message::Assistant(vec![answer]), user(turn)]));
        };
        exchange(&mut request, "two");
        assert_eq!(marked(&request), [true, true, false, true]);
        exchange(&mut request, "three");
        exchange(&mut request, "four");
        // The system prompt and the three newest prefix ends fill the budget.
        assert_eq!(
            marked(&request),
            [true, false, false, true, false, true, false, true]
        );
        // A history without text adds no breakpoint of its own, so a system prompt
        // filling the budget still fits.
        request.system = vec![request.system[0].clone(); 4];
        request.context = entries([Message::User(vec![
            crate::provider::protocol::UserContent::Attachment {
                attachment: crate::media::AttachmentRef::Image(common::tests::image()),
            },
        ])]);
        let images = common::tests::loaded(&mut request).await;
        let context = "session".parse().unwrap();
        let body = codec
            .encode(&request, images.prepared(), &context)
            .unwrap()
            .body;
        let markers = serde_json::Value::Object(body).to_string();
        assert_eq!(markers.matches("cache_control").count(), 4);
    }

    /// Every codec's body carries the preferences and fallbacks; a response
    /// schema forces parameter support, with or without routing.
    #[tokio::test]
    async fn routing_rides_every_codec_body() {
        use crate::provider::{
            dialect::tests::{request, sent},
            protocol::ResponseSchema,
        };
        use serde_json::json;
        let parse = |text: &str| crate::yaml::parse::<Config>(text);
        assert!(parse("routing: {sort: price}").is_err());
        let routed = parse(
            "routing: {order: [a], only: [a, b/turbo], ignore: [c], allow_fallbacks: false, fallback_models: [x/y]}",
        )
        .unwrap();
        let mut constrained = request();
        constrained.response_schema = Some(
            ResponseSchema::new(
                "answer",
                json!({"type":"object","properties":{},"required":[],"additionalProperties":false}),
            )
            .unwrap(),
        );
        // Placements stay clear of the fields routing adds.
        let profile = routed.admit(CodecName::Responses).unwrap();
        let placed: super::super::Placements =
            crate::yaml::parse("cache_key: {body: provider.sort}").unwrap();
        let codec = placed.apply(profile.codec).unwrap();
        assert!(matches!(
            super::super::placements::check(&codec, &profile.transport.fields),
            Err(super::super::PlacementError::Reserved { .. })
        ));
        crate::tests::bounded(async {
            for codec in CODECS {
                let (_, body) = sent(&routed, codec, "k", request()).await;
                assert_eq!(
                    body["provider"],
                    json!({"order":["a"], "only":["a","b/turbo"], "ignore":["c"], "allow_fallbacks":false}),
                    "{codec}"
                );
                assert_eq!(body["models"], json!(["x/y"]), "{codec}");
                let (_, body) = sent(&routed, codec, "k", constrained.clone()).await;
                assert_eq!(body["provider"]["require_parameters"], true, "{codec}");
                assert_eq!(body["provider"]["only"], json!(["a","b/turbo"]), "{codec}");
                let (_, body) = sent(&Config::default(), codec, "k", constrained.clone()).await;
                assert_eq!(body["provider"], json!({"require_parameters":true}), "{codec}");
                assert!(body.get("models").is_none());
                let (_, body) = sent(&Config::default(), codec, "k", request()).await;
                assert!(body.get("provider").is_none());
            }
        })
        .await;
    }

    #[tokio::test]
    async fn key_session_and_attribution_ride_every_request() {
        let head = head(&Config::default(), CodecName::ChatCompletions, "sk-or").await;
        assert_eq!(header_values(&head, "authorization"), ["Bearer sk-or"]);
        assert_eq!(header_values(&head, "x-session-id"), ["ctx"]);
        assert_eq!(header_values(&head, "x-openrouter-title"), ["Skyhook"]);
        assert_eq!(header_values(&head, "http-referer").len(), 1);
    }

    /// Each codec carries the canonical type in its own place, over HTTP and
    /// mid-stream; it names the kind over the codec's code and the status.
    #[test]
    fn canonical_error_types_name_the_kind_on_every_codec() {
        use crate::provider::{
            ProviderErrorKind::*,
            dialect::tests::{rejected, streamed},
        };
        use serde_json::json;
        type Envelope = fn(&str) -> serde_json::Value;
        let chat: Envelope = |kind| json!({"error":{"message":"m","metadata":{"error_type":kind}}});
        let messages: Envelope = |kind| json!({"type":"error","error":{"type":"api_error","message":"m","error_type":kind}});
        let responses: Envelope =
            |kind| json!({"error":{"code":"server_error","message":"m"},"error_type":kind});
        let failed: Envelope = |kind| {
            json!({"type":"response.failed","response":{"status":"failed",
                "error":{"code":"server_error","message":"m"},"error_type":kind}})
        };
        let settings = Config::default();
        for (codec, http, stream) in [
            (CodecName::ChatCompletions, chat, chat),
            (CodecName::Messages, messages, messages),
            (CodecName::Responses, responses, failed),
        ] {
            for (name, kind) in [
                ("authentication", Authentication),
                ("context_length_exceeded", ContextWindowExceeded),
                ("rate_limit_exceeded", RateLimited),
            ] {
                assert_eq!(rejected(&settings, codec, 400, http(name), None), kind);
                assert_eq!(streamed(&settings, codec, stream(name)), kind, "{name}");
            }
        }
        // An unknown type leaves the codec's reading.
        let unknown =
            json!({"type":"error","error":{"type":"rate_limit_error","error_type":"new"}});
        assert_eq!(
            streamed(&settings, CodecName::Messages, unknown),
            RateLimited
        );
    }

    /// Moderation is named by its reasons, and a 403 without them stays an
    /// authentication failure; a 402 is transient only with a retry hint.
    #[test]
    fn moderation_and_retry_hints_name_the_condition() {
        use crate::provider::{ProviderErrorKind, dialect::tests::rejected};
        use serde_json::json;
        use std::time::Duration;
        let chat = CodecName::ChatCompletions;
        let kind = |status, native, hint| rejected(&Config::default(), chat, status, native, hint);
        assert_eq!(
            kind(
                403,
                json!({"error":{"code":403,"message":"flagged","metadata":{"reasons":["x"]}}}),
                None
            ),
            ProviderErrorKind::InvalidRequest
        );
        for native in [
            json!({"error":{"code":403,"message":"forbidden"}}),
            json!({"error":{"code":403,"message":"forbidden","metadata":{"reasons":null}}}),
        ] {
            assert_eq!(kind(403, native, None), ProviderErrorKind::Authentication);
        }
        let hint = Some(Duration::from_secs(3));
        assert_eq!(kind(402, json!({}), hint), ProviderErrorKind::Unavailable);
        assert_eq!(kind(402, json!({}), None), ProviderErrorKind::Billing);
    }
}
