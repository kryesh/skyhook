//! api.openai.com: the `developer` role, `prompt_cache_key` on Chat as well,
//! no reasoning replay on Chat, strict schemas, and organisation headers.

use super::{BaseUrl, Dialect, DialectConfig, DialectError, Profile, UnsupportedCodec};
use crate::provider::{
    ProviderErrorKind,
    codec::{
        CacheKey, Codec, CodecName, Identity, Locations, SchemaConstraint, StateRole, ToolNames,
        chat_completions::{self, EmptyContent, ReasoningReplay, SystemRole},
        path,
        responses::{self, ReasoningSummary},
    },
    http::{
        Transport,
        errors::{ErrorRule, ErrorSignals, Field},
        headers::HeaderText,
    },
    settings::Nullable,
};

fn chat() -> chat_completions::Dialect {
    chat_completions::Dialect {
        system: SystemRole::Developer,
        state_role: StateRole::System,
        reasoning_replay: ReasoningReplay::Unsupported,
        tool_names: ToolNames::OpenAi,
        schema: SchemaConstraint::OpenAiStrict,
        empty_content: EmptyContent::Null,
        identity: Identity {
            cache_key: Locations::one(CacheKey::Body(const { path("prompt_cache_key") })),
            user_id: Locations::NONE,
        },
        ..chat_completions::Dialect::compatible()
    }
}

/// The standard API with OpenAI's tool-name alphabet; proxies share it.
pub(super) fn responses() -> responses::Dialect {
    responses::Dialect {
        tool_names: ToolNames::OpenAi,
        state_role: StateRole::System,
        ..responses::Dialect::stateless()
    }
}

/// Spend and usage limits refuse with 429 like a rate limit, but waiting does
/// not lift them.
const ERRORS: ErrorSignals = ErrorSignals {
    rules: &[
        ErrorRule {
            field: Some(Field::Equals("code", "organization_spend_limit_exceeded")),
            ..ErrorRule::kind(ProviderErrorKind::Billing)
        },
        ErrorRule {
            field: Some(Field::Equals("code", "project_spend_limit_exceeded")),
            ..ErrorRule::kind(ProviderErrorKind::Billing)
        },
        ErrorRule {
            field: Some(Field::Equals("code", "organization_usage_limit_exceeded")),
            ..ErrorRule::kind(ProviderErrorKind::Billing)
        },
    ],
    ..ErrorSignals::NONE
};

/// Provider-only options; request settings are declared separately.
pub type Options = crate::provider::settings::Empty;

crate::provider::settings::settings! {
    #[derive(Default)]
    pub struct Config => Patch {
        /// Responses only: the summary detail requested, `auto` by default. Summaries
        /// need a verified organisation; an unverified one is refused with HTTP 400, so
        /// null requests none.
        #[serde(default, skip_serializing_if = "Nullable::is_default")]
        pub reasoning_summary: Nullable<ReasoningSummary> => default,
        /// Sent as `OpenAI-Organization`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub organization: Option<HeaderText> => default,
        /// Sent as `OpenAI-Project`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub project: Option<HeaderText> => default,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("reasoning_summary applies only to the responses codec")]
    SummaryNeedsResponses,
}

impl DialectConfig for Config {
    fn base_url() -> BaseUrl {
        BaseUrl::Default("https://api.openai.com/v1")
    }

    fn admit(&self, codec: CodecName) -> Result<Profile, DialectError> {
        if matches!(self.reasoning_summary, Nullable::Value(_)) && codec != CodecName::Responses {
            return Err(Error::SummaryNeedsResponses.into());
        }
        let codec = match codec {
            CodecName::ChatCompletions => Codec::ChatCompletions(chat()),
            CodecName::Responses => Codec::Responses(responses::Dialect {
                reasoning_summary: self.reasoning_summary.or(ReasoningSummary::default()),
                ..responses()
            }),
            CodecName::Messages => {
                return Err(UnsupportedCodec {
                    dialect: Dialect::Openai,
                    codec,
                }
                .into());
            }
        };
        let transport = Transport {
            errors: ERRORS,
            ..Transport::plain()
        };
        let mut profile = Profile::new(codec, transport, Dialect::Openai);
        if self.organization.is_some() || self.project.is_some() {
            profile.discriminate("account", &(&self.organization, &self.project));
        }
        // Identity headers tied to the key.
        for (header, value) in [
            ("openai-organization", &self.organization),
            ("openai-project", &self.project),
        ] {
            if let Some(value) = value {
                profile.fixed(header, value.value());
            }
        }
        Ok(profile)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{dialect::tests::head, http::transport::tests::header_values};

    #[tokio::test]
    async fn key_and_identity_headers_ride_every_request() {
        let config: Config =
            crate::yaml::parse("{reasoning_summary: null, organization: org_1, project: proj_1}")
                .unwrap();
        let head = head(&config, CodecName::Responses, "k").await;
        assert_eq!(header_values(&head, "authorization"), ["Bearer k"]);
        assert_eq!(header_values(&head, "openai-organization"), ["org_1"]);
        assert_eq!(header_values(&head, "openai-project"), ["proj_1"]);
        assert!(crate::yaml::parse::<Config>("organization: \"bad\\nvalue\"").is_err());
    }

    #[test]
    fn replay_identity_tracks_tenant_headers_not_summary_preferences() {
        let scope = |yaml| {
            crate::yaml::parse::<Config>(yaml)
                .unwrap()
                .admit(CodecName::Responses)
                .unwrap()
                .scope
        };
        assert_eq!(scope("{}"), scope("reasoning_summary: null"));
        let tenant = scope("organization: org_1\nproject: project_1");
        for other in [
            "{}",
            "organization: org_2\nproject: project_1",
            "organization: org_1\nproject: project_2",
        ] {
            assert_ne!(tenant, scope(other));
        }
    }

    #[test]
    fn the_summary_requested_is_auto_unless_another_or_none_is_chosen() {
        for (reasoning_summary, requested) in [
            (Nullable::Default, Some(ReasoningSummary::Auto)),
            (Nullable::Null, None),
            (
                Nullable::Value(ReasoningSummary::Detailed),
                Some(ReasoningSummary::Detailed),
            ),
        ] {
            let config = Config {
                reasoning_summary,
                ..Config::default()
            };
            let Codec::Responses(dialect) = config.admit(CodecName::Responses).unwrap().codec
            else {
                panic!("Responses conventions");
            };
            assert_eq!(dialect.reasoning_summary, requested);
        }
    }

    /// Only a chosen detail needs Responses: an inherited null requests nothing anywhere.
    #[test]
    fn summaries_are_chosen_on_responses_only() {
        let summary = |reasoning_summary| Config {
            reasoning_summary,
            ..Config::default()
        };
        assert!(
            summary(Nullable::Null)
                .admit(CodecName::ChatCompletions)
                .is_ok()
        );
        assert_eq!(
            summary(Nullable::Value(ReasoningSummary::Concise))
                .admit(CodecName::ChatCompletions)
                .err(),
            Some(Error::SummaryNeedsResponses.into())
        );
    }
}
