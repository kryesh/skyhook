//! api.openai.com: the `developer` role, `prompt_cache_key` on Chat as well,
//! no reasoning replay on Chat, strict schemas, and organisation headers.

use reqwest::header::{HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};

use super::{Common, Dialect, DialectConfig, DialectError, Profile, UnsupportedCodec};
use crate::provider::{
    codec::{
        CacheKey, Codec, CodecName, Identity, SchemaConstraint, ToolNames,
        chat_completions::{self, EmptyContent, ReasoningReplay, SystemRole},
        path, responses,
    },
    http::{
        Transport,
        errors::{ErrorRule, ErrorSignals, Field, RuleKind},
        headers::Value,
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
            ..ErrorRule::kind(RuleKind::Billing)
        },
        ErrorRule {
            field: Some(Field::Equals("code", "project_spend_limit_exceeded")),
            ..ErrorRule::kind(RuleKind::Billing)
        },
        ErrorRule {
            field: Some(Field::Equals("code", "organization_usage_limit_exceeded")),
            ..ErrorRule::kind(RuleKind::Billing)
        },
    ],
    ..ErrorSignals::NONE
};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Responses only. Reasoning summaries need a verified organisation; an
    /// unverified one is refused with HTTP 400, so it opts out here.
    #[serde(default = "yes", skip_serializing_if = "is_yes")]
    pub reasoning_summary: bool,
    /// Sent as `OpenAI-Organization`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization: Option<String>,
    /// Sent as `OpenAI-Project`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
}

const fn yes() -> bool {
    true
}

const fn is_yes(value: &bool) -> bool {
    *value
}

impl Default for Config {
    fn default() -> Self {
        Self {
            reasoning_summary: true,
            organization: None,
            project: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("reasoning_summary applies only to the responses codec")]
    SummaryNeedsResponses,
    #[error("organization is not a valid header value")]
    Organization,
    #[error("project is not a valid header value")]
    Project,
}

impl DialectConfig for Config {
    fn admit(&self, _: &Common, codec: CodecName) -> Result<Profile, DialectError> {
        if !self.reasoning_summary && codec != CodecName::Responses {
            return Err(Error::SummaryNeedsResponses.into());
        }
        let codec = match codec {
            CodecName::ChatCompletions => Codec::ChatCompletions(chat()),
            CodecName::Responses => Codec::Responses(responses::Dialect {
                reasoning_summary: if self.reasoning_summary {
                    responses::ReasoningSummary::Requested
                } else {
                    responses::ReasoningSummary::Unsupported
                },
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
        // Identity headers tied to the key.
        for (error, header, value) in [
            (
                Error::Organization,
                "openai-organization",
                &self.organization,
            ),
            (Error::Project, "openai-project", &self.project),
        ] {
            if let Some(value) = value {
                let value = HeaderValue::from_str(value).map_err(|_| error)?;
                profile
                    .headers
                    .insert(HeaderName::from_static(header), Value::Fixed(value));
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
        let config = Config {
            reasoning_summary: false,
            organization: Some("org_1".into()),
            project: Some("proj_1".into()),
        };
        let head = head(&config, CodecName::Responses, "k").await;
        assert_eq!(header_values(&head, "authorization"), ["Bearer k"]);
        assert_eq!(header_values(&head, "openai-organization"), ["org_1"]);
        assert_eq!(header_values(&head, "openai-project"), ["proj_1"]);
        let bad = Config {
            organization: Some("bad\nvalue".into()),
            ..Config::default()
        };
        assert_eq!(
            bad.admit(&Common::default(), CodecName::Responses).err(),
            Some(Error::Organization.into())
        );
    }

    #[test]
    fn summaries_opt_out_on_responses_only_and_messages_is_not_spoken() {
        let admit = |config: &Config, codec| config.admit(&Common::default(), codec).err();
        let quiet = Config {
            reasoning_summary: false,
            ..Config::default()
        };
        assert_eq!(
            admit(&quiet, CodecName::ChatCompletions),
            Some(Error::SummaryNeedsResponses.into())
        );
        assert_eq!(
            admit(&Config::default(), CodecName::Messages),
            Some(
                UnsupportedCodec {
                    dialect: Dialect::Openai,
                    codec: CodecName::Messages
                }
                .into()
            )
        );
    }
}
