//! openrouter.ai: Chat with the `reasoning` object, verbatim `reasoning_details`
//! replay, `cache_control` on content parts, usage on every stream, provider
//! routing in the body, and `x-session-id` affinity; Messages for Anthropic
//! models; Responses stateless with the same affinity header. Bearer keys on
//! every codec; attribution headers name Skyhook.

use reqwest::header::{HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};

use super::{Common, Dialect, DialectConfig, DialectError, Profile};
use crate::provider::{
    ProviderErrorKind,
    codec::{
        CacheTtl, Codec, CodecName, Effort, Identity, OPENAI_EFFORT, SchemaConstraint, ToolNames,
        chat_completions::{self, Cache, EmptyContent, ReasoningReplay, Routing, UsageRequest},
        header, messages, path, responses,
    },
    http::{
        Transport,
        errors::{ErrorRule, ErrorSignals, Field, Reader, RetryAfter, RuleKind, error_object},
        headers::Value,
    },
};

const fn session() -> Identity {
    Identity {
        cache_key: header("x-session-id"),
        user_id: None,
    }
}

fn chat(ttl: Option<CacheTtl>, routing: Option<Routing>) -> chat_completions::Dialect {
    chat_completions::Dialect {
        effort: Effort {
            path: const { path("reasoning.effort") },
            levels: OPENAI_EFFORT,
        },
        reasoning_replay: ReasoningReplay::Details(const { path("reasoning_details") }),
        empty_content: EmptyContent::Null,
        usage_request: UsageRequest::Implicit,
        tool_names: ToolNames::OpenAi,
        schema: SchemaConstraint::OpenAiStrict,
        identity: session(),
        cache: Cache::ContentPartBreakpoints { ttl },
        routing,
        ..chat_completions::Dialect::compatible()
    }
}

fn responses() -> responses::Dialect {
    responses::Dialect {
        identity: session(),
        ..super::openai::responses()
    }
}

fn messages(ttl: Option<CacheTtl>) -> messages::Dialect {
    messages::Dialect {
        identity: session(),
        cache_ttl: ttl,
        ..messages::Dialect::anthropic()
    }
}

/// Moderation names its reasons under `error.metadata`. A 402 during a request
/// is transient when the router says when to retry; without a hint it is
/// exhausted credit.
const RULES: &[ErrorRule] = &[
    ErrorRule {
        status: Some(403),
        field: Some(Field::Present("metadata.reasons")),
        ..ErrorRule::kind(RuleKind::InvalidRequest)
    },
    ErrorRule {
        status: Some(402),
        retry_after: RetryAfter::Required,
        ..ErrorRule::kind(RuleKind::Unavailable)
    },
];

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
            Self::RateLimitExceeded => RateLimited { retry_after: None },
            Self::ProviderOverloaded | Self::ProviderUnavailable => {
                Unavailable { retry_after: None }
            }
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
    let read: Reader = match codec {
        CodecName::ChatCompletions => ErrorType::chat,
        CodecName::Responses => ErrorType::responses,
        CodecName::Messages => ErrorType::messages,
    };
    Transport {
        errors: ErrorSignals { rules: RULES, read },
        ..Transport::plain()
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Chat only: provider routing preferences and fallback models.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<Routing>,
    /// Cache breakpoint lifetime for models that take one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_ttl: Option<CacheTtl>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("routing applies only to the chat_completions codec")]
    RoutingNeedsChat,
}

impl DialectConfig for Config {
    fn admit(&self, _: &Common, codec: CodecName) -> Result<Profile, DialectError> {
        if self.routing.is_some() && codec != CodecName::ChatCompletions {
            return Err(Error::RoutingNeedsChat.into());
        }
        let transport = transport(codec);
        let codec = match codec {
            CodecName::ChatCompletions => {
                Codec::ChatCompletions(chat(self.cache_ttl, self.routing.clone()))
            }
            CodecName::Responses => Codec::Responses(responses()),
            CodecName::Messages => Codec::Messages(messages(self.cache_ttl)),
        };
        let mut profile = Profile::new(codec, transport, Dialect::Openrouter);
        for (name, value) in [
            ("http-referer", "https://github.com/kryesh/skyhook"),
            ("x-openrouter-title", "Skyhook"),
        ] {
            profile.headers.insert(
                HeaderName::from_static(name),
                Value::Fixed(HeaderValue::from_static(value)),
            );
        }
        Ok(profile)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{dialect::tests::head, http::transport::tests::header_values};

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
                ("rate_limit_exceeded", RateLimited { retry_after: None }),
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
            RateLimited { retry_after: None }
        );
    }

    /// Moderation is named by its reasons; a 402 is transient only with a
    /// retry hint, which rides the kind.
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
        let hint = Some(Duration::from_secs(3));
        assert_eq!(
            kind(402, json!({}), hint),
            ProviderErrorKind::Unavailable { retry_after: hint }
        );
        assert_eq!(kind(402, json!({}), None), ProviderErrorKind::Billing);
    }

    #[test]
    fn routing_is_chat_only_and_parsed_strictly() {
        let parse = |text: &str| crate::yaml::parse::<Config>(text);
        let routed =
            parse("routing: {order: [anthropic], zdr: true, fallback_models: [x/y]}").unwrap();
        assert!(parse("routing: {sort: price}").is_err());
        assert!(
            routed
                .admit(&Common::default(), CodecName::ChatCompletions)
                .is_ok()
        );
        assert_eq!(
            routed
                .admit(&Common::default(), CodecName::Messages)
                .unwrap_err(),
            Error::RoutingNeedsChat.into()
        );
    }
}
