//! api.openai.com: the `developer` role, `prompt_cache_key` on Chat as well,
//! no reasoning replay on Chat, strict schemas, and organisation headers.

use serde::Deserialize;

use super::{BaseUrl, Dialect, DialectConfig, DialectError, Profile};
use crate::provider::{
    ProviderErrorKind,
    codec::{
        CacheKey, Codec, CodecName, Identity, Locations, SchemaConstraint, StateRole, ToolNames,
        UnsignedReasoning,
        chat_completions::{self, EmptyContent, ReasoningReplay, SystemRole},
        path,
        responses::{self, ReasoningSummary},
    },
    http::{
        Transport,
        errors::{ErrorSignals, error_object},
        headers::HeaderText,
    },
};

fn chat() -> chat_completions::Dialect {
    chat_completions::Dialect {
        system: SystemRole::Developer,
        state_role: StateRole::System,
        reasoning_replay: ReasoningReplay::Unsupported,
        empty_content: EmptyContent::Null,
        identity: Identity {
            cache_key: Locations::one(CacheKey::Body(const { path("prompt_cache_key") })),
            user_id: Locations::NONE,
        },
        ..chat_completions::Dialect::compatible()
    }
}

/// The standard API's Responses conventions; proxies share them. Nothing is stored,
/// so a reasoning item without its encrypted state cannot be resolved.
pub(super) fn responses() -> responses::Dialect {
    responses::Dialect {
        state_role: StateRole::System,
        unsigned: UnsignedReasoning::Dropped,
        reasoning_summary: Some(ReasoningSummary::Auto),
        ..responses::Dialect::stateless()
    }
}

/// OpenAI's tool-name alphabet, and response schemas flagged `strict`;
/// services that front OpenAI's models share them.
pub(super) fn conventions(codec: &mut Codec) {
    *codec.tool_names_mut() = ToolNames::OpenAi;
    if let Some(schema) = codec.schema_mut() {
        *schema = SchemaConstraint::Strict;
    }
}

/// Spend and usage limits refuse with 429 like a rate limit, but waiting does
/// not lift them.
const ERRORS: ErrorSignals = ErrorSignals(|_, native, _| {
    #[derive(Deserialize)]
    enum Limit {
        #[serde(rename = "organization_spend_limit_exceeded")]
        OrganizationSpend,
        #[serde(rename = "project_spend_limit_exceeded")]
        ProjectSpend,
        #[serde(rename = "organization_usage_limit_exceeded")]
        OrganizationUsage,
    }
    #[derive(Deserialize)]
    struct Limited {
        code: Limit,
    }
    match Limited::deserialize(error_object(native)).ok()?.code {
        Limit::OrganizationSpend | Limit::ProjectSpend | Limit::OrganizationUsage => {
            Some(ProviderErrorKind::Billing)
        }
    }
});

/// Provider-only options; request settings are declared separately.
pub type Options = crate::provider::settings::Empty;

crate::provider::settings::settings! {
    #[derive(Default)]
    pub struct Config => Patch {
        /// Sent as `OpenAI-Organization`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub organization: Option<HeaderText> => default,
        /// Sent as `OpenAI-Project`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub project: Option<HeaderText> => default,
    }
}

impl DialectConfig for Config {
    fn base_url() -> BaseUrl {
        BaseUrl::Default("https://api.openai.com/v1")
    }

    fn admit(&self, codec: CodecName) -> Result<Profile, DialectError> {
        let mut codec = match codec {
            CodecName::ChatCompletions => Codec::ChatCompletions(chat()),
            CodecName::Responses => Codec::Responses(responses()),
            CodecName::Messages => {
                return Err(DialectError::UnsupportedCodec {
                    dialect: Dialect::Openai,
                    codec,
                });
            }
        };
        conventions(&mut codec);
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
        let config: Config = crate::yaml::parse("{organization: org_1, project: proj_1}").unwrap();
        let head = head(&config, CodecName::Responses, "k").await;
        assert_eq!(header_values(&head, "authorization"), ["Bearer k"]);
        assert_eq!(header_values(&head, "openai-organization"), ["org_1"]);
        assert_eq!(header_values(&head, "openai-project"), ["proj_1"]);
        assert!(crate::yaml::parse::<Config>("organization: \"bad\\nvalue\"").is_err());
    }

    #[test]
    fn replay_identity_tracks_tenant_headers() {
        let scope = |yaml| {
            crate::yaml::parse::<Config>(yaml)
                .unwrap()
                .admit(CodecName::Responses)
                .unwrap()
                .scope
        };
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
    fn spend_limits_are_billing_not_a_rate_limit() {
        use crate::provider::{ProviderErrorKind, dialect::tests::rejected};
        let kind = |code| {
            let body = serde_json::json!({"error":{"code":code}});
            rejected(&Config::default(), CodecName::Responses, 429, body, None)
        };
        assert_eq!(
            kind("project_spend_limit_exceeded"),
            ProviderErrorKind::Billing
        );
        assert_eq!(kind("rate_limit_exceeded"), ProviderErrorKind::RateLimited);
    }
}
