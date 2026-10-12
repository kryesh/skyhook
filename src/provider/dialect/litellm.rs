//! A LiteLLM proxy. The model alias hides which family serves it, and the proxy
//! translates each codec to that family, so the entry names the upstream. Anthropic
//! takes Anthropic's conventions on every codec: signed `thinking_blocks` and
//! `cache_control` on Chat, and its own Messages conventions, since the proxy forwards
//! Messages as sent. OpenAI takes OpenAI's. Bedrock serves both families, so its codec
//! decides: OpenAI's conventions on Chat and Responses, with Chat replaying the signed
//! `thinking_blocks` its Bedrock path returns, and standard Messages on Messages.
//! Every codec takes the proxy's virtual key as a bearer token; deployment tags ride
//! `x-litellm-tags`.
//!
//! The context identity rides `x-litellm-session-id`, which the proxy's session
//! affinity and spend logs key on but forwards to no upstream. Chat and Responses
//! under OpenAI's conventions also carry `prompt_cache_key`, which the proxy passes
//! to the service for its own cache affinity.

use reqwest::header::HeaderValue;
use serde::Serialize;

use super::{Dialect, DialectConfig, DialectError, Profile};
use crate::{
    named_enum::named_enum,
    newtype::string_newtype,
    provider::{
        ProviderErrorKind,
        codec::{
            CacheKey, Codec, CodecName, Locations, NonEmpty, SchemaConstraint, StateRole,
            ToolNames,
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
    /// Whether the upstream takes Anthropic's conventions whatever the codec. Bedrock's
    /// codec decides instead, and the proxy's Bedrock path drops the turn-scoped beta
    /// Anthropic's Messages conventions need.
    const fn is_anthropic(self) -> bool {
        matches!(self, Self::Anthropic)
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

/// OpenAI's conventions, replaying the signed `thinking_blocks` the proxy returns, from
/// which its Bedrock path rebuilds the model's reasoning.
fn chat_bedrock() -> chat_completions::Dialect {
    chat_completions::Dialect {
        reasoning_replay: thinking_blocks(),
        ..chat_openai()
    }
}

/// Anthropic's tool-name alphabet is OpenAI's; `strict` is not honoured, so the
/// schema goes as written.
fn chat_anthropic() -> chat_completions::Dialect {
    chat_completions::Dialect {
        reasoning_replay: thinking_blocks(),
        empty_content: EmptyContent::Null,
        tool_names: ToolNames::OpenAi,
        cache: Cache::ContentPartBreakpoints { ttl: None },
        ..chat_completions::Dialect::compatible()
    }
}

fn thinking_blocks() -> ReasoningReplay {
    ReasoningReplay::ThinkingBlocks(NonEmpty::one(const { path("thinking_blocks") }))
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
        /// The role runtime state takes: `user` sends it as user content; `system`
        /// needs a server that accepts system messages mid-conversation. Anthropic
        /// behind Messages defaults to `system`, turn-scoped; every other upstream
        /// and codec to `user`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub state_role: Option<StateRole> => optional(StateRole),
    }
}

impl DialectConfig for Config {
    fn admit(&self, codec: CodecName) -> Result<Profile, DialectError> {
        let anthropic = self.upstream.is_anthropic();
        let anthropic_messages = codec == CodecName::Messages && anthropic;
        let mut conventions = match codec {
            CodecName::ChatCompletions => Codec::ChatCompletions(match self.upstream {
                Upstream::Anthropic => chat_anthropic(),
                Upstream::Openai => chat_openai(),
                Upstream::Bedrock => chat_bedrock(),
            }),
            CodecName::Responses => Codec::Responses(super::openai::responses()),
            CodecName::Messages if anthropic_messages => {
                Codec::Messages(super::anthropic::messages())
            }
            CodecName::Messages => Codec::Messages(messages::Dialect::standard()),
        };
        let session = Locations::one(header("x-litellm-session-id"));
        conventions.identity_mut().cache_key = if codec != CodecName::Messages && !anthropic {
            session.with(CacheKey::Body(const { path("prompt_cache_key") }))
        } else {
            session
        };
        let mut profile = Profile {
            scope: self.upstream.scope(),
            ..Profile::new(conventions, transport(), Dialect::Litellm)
        };
        if anthropic_messages {
            // These conventions send state as turn-scoped system messages, which the
            // bare upstream scope does not mean.
            profile.discriminate("messages", &"anthropic");
            super::anthropic::state(&mut profile, self.state_role);
        } else {
            profile.state_role(self.state_role, StateRole::User);
        }
        if !self.tags.is_empty() {
            profile.discriminate("tags", &self.tags);
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
        let scope_for = |yaml, codec| {
            let config = crate::yaml::parse::<Config>(yaml).unwrap();
            config.admit(codec).unwrap().scope
        };
        let scope = |yaml| scope_for(yaml, CodecName::ChatCompletions);
        // Anthropic's conventions behind Messages are a conversation of their own.
        let messages = scope_for("upstream: anthropic", CodecName::Messages);
        assert_ne!(messages, Upstream::Anthropic.scope());
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
            state_role: None,
        };
        let responses = head(&openai, CodecName::Responses, "k").await;
        assert_eq!(header_values(&responses, "x-litellm-session-id"), ["ctx"]);
        assert_eq!(Upstream::Bedrock.scope(), "litellm:bedrock");
        assert!(crate::yaml::parse::<Config>("{upstream: openai, tags: ['a,b']}").is_err());
    }

    /// Bedrock serves both families, so its codec decides: OpenAI's conventions on Chat and
    /// Responses, where Anthropic keeps its own, with Chat still replaying signed thinking.
    #[test]
    fn bedrock_takes_openai_conventions_and_replays_thinking_on_chat() {
        let codec = |upstream, codec| {
            let config = Config {
                upstream,
                tags: Vec::new(),
                state_role: None,
            };
            config.admit(codec).unwrap().codec
        };
        assert_eq!(
            codec(Upstream::Bedrock, CodecName::Responses),
            codec(Upstream::Openai, CodecName::Responses)
        );
        let chat = |upstream| match codec(upstream, CodecName::ChatCompletions) {
            Codec::ChatCompletions(dialect) => dialect,
            other => panic!("Chat conventions: {other:?}"),
        };
        let (bedrock, openai) = (chat(Upstream::Bedrock), chat(Upstream::Openai));
        assert_eq!(
            bedrock.reasoning_replay,
            chat(Upstream::Anthropic).reasoning_replay
        );
        assert_eq!(
            chat_completions::Dialect {
                reasoning_replay: openai.reasoning_replay.clone(),
                ..bedrock
            },
            openai
        );
    }

    /// Chat and Responses behind OpenAI or Bedrock carry the identity in the body for the
    /// service as well as in the session header for the proxy; Messages and Anthropic
    /// carry the header alone.
    #[test]
    fn the_service_gets_the_identity_behind_openai_and_bedrock() {
        let session = Locations::one(header("x-litellm-session-id"));
        let both = session
            .clone()
            .with(CacheKey::Body(path("prompt_cache_key")));
        use CodecName::{ChatCompletions, Messages, Responses};
        for (upstream, codec, expected) in [
            (Upstream::Openai, ChatCompletions, &both),
            (Upstream::Openai, Responses, &both),
            (Upstream::Openai, Messages, &session),
            (Upstream::Bedrock, ChatCompletions, &both),
            (Upstream::Bedrock, Responses, &both),
            (Upstream::Bedrock, Messages, &session),
            (Upstream::Anthropic, ChatCompletions, &session),
            (Upstream::Anthropic, Responses, &session),
            (Upstream::Anthropic, Messages, &session),
        ] {
            let config = Config {
                upstream,
                tags: Vec::new(),
                state_role: None,
            };
            let admitted = config.admit(codec).unwrap().codec;
            assert_eq!(
                &admitted.identity().cache_key,
                expected,
                "{upstream} {codec}"
            );
        }
    }

    /// Anthropic behind Messages takes the anthropic dialect's conventions: turn-scoped system
    /// state, thinking dropped on a binding mismatch, and the betas both need. Every
    /// other upstream and codec keeps user-role state and announces no beta.
    #[tokio::test]
    async fn anthropic_behind_messages_takes_the_anthropic_dialects_conventions() {
        let config = |yaml| crate::yaml::parse::<Config>(yaml).unwrap();
        let betas = async |config: &Config, codec| {
            header_values(&head(config, codec, "k").await, "anthropic-beta")
        };
        let anthropic = config("upstream: anthropic");
        let mut expected = Codec::Messages(messages::Dialect {
            state_role: StateRole::System,
            ..super::super::anthropic::messages()
        });
        expected.identity_mut().cache_key = Locations::one(header("x-litellm-session-id"));
        assert_eq!(
            anthropic.admit(CodecName::Messages).unwrap().codec,
            expected
        );
        assert_eq!(
            betas(&anthropic, CodecName::Messages).await,
            ["thinking-binding-controls-2026-08-01, mid-conversation-system-clear-at-2026-08-21"]
        );
        for (config, codec) in [
            (config("upstream: bedrock"), CodecName::Messages),
            (config("upstream: openai"), CodecName::Messages),
            (anthropic, CodecName::ChatCompletions),
        ] {
            let mut admitted = config.admit(codec).unwrap().codec;
            assert_eq!(*admitted.state_role_mut(), StateRole::User, "{codec}");
            assert!(betas(&config, codec).await.is_empty(), "{codec}");
        }
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
            state_role: None,
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
