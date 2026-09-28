//! api.openai.com: the `developer` role, `prompt_cache_key` on Chat as well,
//! no reasoning replay on Chat, strict schemas, and organisation headers.

use super::{Dialect, DialectConfig, DialectError, Profile, UnsupportedCodec};
use crate::provider::{
    ProviderErrorKind,
    codec::{
        CacheKey, Codec, CodecName, Identity, SchemaConstraint, ToolNames,
        chat_completions::{self, EmptyContent, ReasoningReplay, SystemRole},
        path,
        responses::{self, ReasoningSummary},
    },
    http::{
        Transport,
        errors::{ErrorRule, ErrorSignals, Field},
        headers::HeaderText,
    },
};

fn chat() -> chat_completions::Dialect {
    chat_completions::Dialect {
        system: SystemRole::Developer,
        reasoning_replay: ReasoningReplay::Unsupported,
        tool_names: ToolNames::OpenAi,
        schema: SchemaConstraint::OpenAiStrict,
        empty_content: EmptyContent::Null,
        identity: Identity {
            cache_key: CacheKey::Body(const { path("prompt_cache_key") }),
            user_id: None,
        },
        ..chat_completions::Dialect::compatible()
    }
}

/// The standard API with OpenAI's tool-name alphabet; proxies share it.
pub(super) fn responses() -> responses::Dialect {
    responses::Dialect {
        tool_names: ToolNames::OpenAi,
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
        /// Responses only. Reasoning summaries need a verified organisation; an
        /// unverified one is refused with HTTP 400, so it opts out here.
        #[serde(default, skip_serializing_if = "ReasoningSummary::is_requested")]
        pub reasoning_summary: ReasoningSummary => default,
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
    fn admit(&self, codec: CodecName) -> Result<Profile, DialectError> {
        if !self.reasoning_summary.is_requested() && codec != CodecName::Responses {
            return Err(Error::SummaryNeedsResponses.into());
        }
        let codec = match codec {
            CodecName::ChatCompletions => Codec::ChatCompletions(chat()),
            CodecName::Responses => Codec::Responses(responses::Dialect {
                reasoning_summary: self.reasoning_summary,
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
            profile.discriminate(&(&self.organization, &self.project));
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
        let config: Config = crate::yaml::parse(
            "{reasoning_summary: unsupported, organization: org_1, project: proj_1}",
        )
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
        assert_eq!(scope("{}"), scope("reasoning_summary: unsupported"));
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
    fn an_explicit_preset_default_overrides_an_inherited_opt_out() {
        use crate::provider::settings::{Patch as _, Settings};

        let lower: Patch = crate::yaml::parse("reasoning_summary: unsupported").unwrap();
        let higher: Patch = crate::yaml::parse("reasoning_summary: requested").unwrap();
        assert_eq!(
            crate::yaml::to_string(&higher).unwrap(),
            "reasoning_summary: requested\n"
        );
        let config = Config::resolve(&lower.overlay(&higher)).unwrap();
        assert_eq!(config.reasoning_summary, ReasoningSummary::Requested);
        assert!(config.admit(CodecName::ChatCompletions).is_ok());
    }

    #[test]
    fn summaries_opt_out_on_responses_only() {
        let quiet = Config {
            reasoning_summary: ReasoningSummary::Unsupported,
            ..Config::default()
        };
        assert_eq!(
            quiet.admit(CodecName::ChatCompletions).err(),
            Some(Error::SummaryNeedsResponses.into())
        );
    }
}
