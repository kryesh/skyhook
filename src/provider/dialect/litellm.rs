//! A LiteLLM proxy. The model alias hides which family serves it, and the proxy
//! translates each codec to that family, so the entry names the upstream: Claude
//! behind Chat replays signed `thinking_blocks` and takes `cache_control` on
//! content parts, while OpenAI behind Chat rejects both; Messages goes through
//! the proxy's translation whatever the upstream. Every codec takes the proxy's
//! virtual key as a bearer token; session affinity and spend logs key on
//! `x-litellm-session-id`; deployment tags ride `x-litellm-tags`.

use reqwest::header::HeaderValue;
use serde::Serialize;

use super::{Dialect, DialectConfig, DialectError, Profile};
use crate::{
    named_enum::named_enum,
    newtype::string_newtype,
    provider::{
        ProviderErrorKind,
        codec::{
            Codec, CodecName, SchemaConstraint, ToolNames,
            chat_completions::{self, Cache, EmptyContent, ReasoningReplay},
            header, messages, path,
        },
        http::{
            Transport,
            errors::{ErrorRule, ErrorSignals, Field},
        },
    },
};

named_enum! {
    /// The family behind an alias.
    #[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
    pub enum Upstream {
        Openai = "openai",
        Anthropic = "anthropic",
        Bedrock = "bedrock",
    }
}

impl Upstream {
    /// Bedrock serves Claude with the same conventions as Anthropic.
    const fn is_claude(self) -> bool {
        matches!(self, Self::Anthropic | Self::Bedrock)
    }

    /// Replayed state means something different behind each family.
    fn scope(self) -> String {
        format!("{}:{self}", Dialect::Litellm)
    }
}

fn chat_openai() -> chat_completions::Dialect {
    chat_completions::Dialect {
        reasoning_replay: ReasoningReplay::Unsupported,
        empty_content: EmptyContent::Null,
        tool_names: ToolNames::OpenAi,
        schema: SchemaConstraint::OpenAiStrict,
        ..chat_completions::Dialect::compatible()
    }
}

/// Claude's tool-name alphabet is OpenAI's; `strict` is not honoured, so the
/// schema goes as written.
fn chat_claude() -> chat_completions::Dialect {
    chat_completions::Dialect {
        reasoning_replay: ReasoningReplay::ThinkingBlocks(const { path("thinking_blocks") }),
        empty_content: EmptyContent::Null,
        tool_names: ToolNames::OpenAi,
        cache: Cache::ContentPartBreakpoints { ttl: None },
        ..chat_completions::Dialect::compatible()
    }
}

/// Proxy errors name an overflow only in the message, and an exhausted
/// virtual-key budget by its type.
const ERRORS: ErrorSignals = ErrorSignals {
    rules: &[
        ErrorRule {
            message: Some("ContextWindowExceededError"),
            ..ErrorRule::kind(ProviderErrorKind::ContextWindowExceeded)
        },
        ErrorRule {
            field: Some(Field::Equals("type", "budget_exceeded")),
            ..ErrorRule::kind(ProviderErrorKind::Billing)
        },
    ],
    ..ErrorSignals::NONE
};

fn transport() -> Transport {
    Transport {
        errors: ERRORS,
        ..Transport::plain()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("tags must be nonblank, without commas or control characters")]
pub struct InvalidTag;

string_newtype! {
    /// A deployment tag. Tags travel comma-separated in one header.
    pub struct Tag(InvalidTag) = |tag| {
        let valid = !tag.trim().is_empty() && !tag.contains(',');
        HeaderValue::from_str(tag).ok().filter(|_| valid).map(drop).ok_or(InvalidTag)
    };
}

/// Provider-only options; request settings are declared separately.
pub type Options = crate::provider::settings::Empty;

crate::provider::settings::settings! {
    pub struct Config => Patch {
        pub upstream: Upstream => required,
        /// Sent as `x-litellm-tags`, for tag-based routing and spend logs.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub tags: Vec<Tag> => default,
    }
}

impl DialectConfig for Config {
    /// The bridge to Claude may drop `prompt_cache_key`, so affinity travels as
    /// a header whatever the upstream and codec.
    fn admit(&self, codec: CodecName) -> Result<Profile, DialectError> {
        let claude = self.upstream.is_claude();
        let mut conventions = match codec {
            CodecName::ChatCompletions => {
                Codec::ChatCompletions(if claude { chat_claude() } else { chat_openai() })
            }
            CodecName::Responses => Codec::Responses(super::openai::responses()),
            CodecName::Messages => Codec::Messages(messages::Dialect::anthropic()),
        };
        conventions.identity_mut().cache_key = header("x-litellm-session-id");
        let mut profile = Profile {
            scope: self.upstream.scope(),
            ..Profile::new(conventions, transport(), Dialect::Litellm)
        };
        if !self.tags.is_empty() {
            profile.discriminate(&self.tags);
            let tags: Vec<_> = self.tags.iter().map(Tag::as_str).collect();
            let value = HeaderValue::from_str(&tags.join(",")).expect("tags join into header text");
            profile.fixed("x-litellm-tags", value);
        }
        Ok(profile)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{dialect::tests::head, http::transport::tests::header_values};

    #[test]
    fn replay_identity_tracks_upstream_and_routing_tags() {
        let scope = |yaml| {
            crate::yaml::parse::<Config>(yaml)
                .unwrap()
                .admit(CodecName::ChatCompletions)
                .unwrap()
                .scope
        };
        let tagged = scope("upstream: anthropic\ntags: [deployment-a]");
        for other in [
            "upstream: anthropic",
            "upstream: anthropic\ntags: [deployment-b]",
            "upstream: bedrock\ntags: [deployment-a]",
        ] {
            assert_ne!(tagged, scope(other));
        }
    }

    #[tokio::test]
    async fn virtual_key_session_and_tags_ride_every_request() {
        let config: Config =
            crate::yaml::parse("{upstream: bedrock, tags: [team-a, prod]}").unwrap();
        let chat = head(&config, CodecName::ChatCompletions, "sk-virtual").await;
        assert_eq!(header_values(&chat, "authorization"), ["Bearer sk-virtual"]);
        assert_eq!(header_values(&chat, "x-litellm-session-id"), ["ctx"]);
        assert_eq!(header_values(&chat, "x-litellm-tags"), ["team-a,prod"]);
        // Affinity rides the session header behind every upstream.
        let openai = Config {
            upstream: Upstream::Openai,
            tags: Vec::new(),
        };
        let responses = head(&openai, CodecName::Responses, "k").await;
        assert_eq!(header_values(&responses, "x-litellm-session-id"), ["ctx"]);
        assert_eq!(Upstream::Bedrock.scope(), "litellm:bedrock");
        assert!(crate::yaml::parse::<Config>("{upstream: openai, tags: ['a,b']}").is_err());
    }

    /// The proxy names an overflow only in its message, and a spent budget by
    /// type, whichever codec carried the request.
    #[test]
    fn overflow_and_spent_budget_are_read_from_proxy_errors() {
        use crate::provider::{ProviderErrorKind, dialect::tests::rejected};
        let overflow = serde_json::json!({"error":{"message":"litellm.ContextWindowExceededError: litellm.BadRequestError: prompt is too long","type":null,"code":"400"}});
        let spent = serde_json::json!({"error":{"message":"Budget has been exceeded!",
            "type":"budget_exceeded","param":null,"code":"400"}});
        let config = Config {
            upstream: Upstream::Anthropic,
            tags: Vec::new(),
        };
        for codec in [CodecName::ChatCompletions, CodecName::Messages] {
            let kind = |body| rejected(&config, codec, 400, body, None);
            assert_eq!(
                kind(overflow.clone()),
                ProviderErrorKind::ContextWindowExceeded
            );
            assert_eq!(kind(spent.clone()), ProviderErrorKind::Billing);
        }
    }
}
