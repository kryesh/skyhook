//! api.anthropic.com: the Messages API with its version header, `x-api-key`,
//! prefix-bound thinking, cache breakpoint lifetimes, and workspace-scoped keys.

use reqwest::header::{HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};

use super::{
    BuildError, Common, Connection, Dialect, DialectConfig, DialectError, Pending, Profile, Scheme,
    UnsupportedCodec,
};
use crate::provider::{
    codec::{
        CacheTtl, Codec, CodecName,
        messages::{self, ThinkingBinding},
    },
    http::{
        Headers, Transport,
        errors::{ErrorRule, ErrorSignals, Field, RuleKind},
        headers::Value,
    },
};

/// Signed thinking is bound to the exact conversation before it; letting the
/// service drop a mismatched block keeps mode switches and compaction from
/// failing, and the runtime still unbinds client-side.
fn messages() -> messages::Dialect {
    messages::Dialect {
        thinking_binding: ThinkingBinding::DropOnMismatch,
        ..messages::Dialect::anthropic()
    }
}

/// A spend cap is reported as a rate limit without a retry hint.
const ERRORS: ErrorSignals = ErrorSignals {
    rules: &[ErrorRule {
        status: Some(429),
        field: Some(Field::Equals(
            "details.error_code",
            "enforced_spend_limit_reached",
        )),
        ..ErrorRule::kind(RuleKind::Billing)
    }],
    ..ErrorSignals::NONE
};

fn transport() -> Transport {
    Transport {
        errors: ERRORS,
        ..Transport::plain()
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Sent as `anthropic-workspace-id`; required by keys that span workspaces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_ttl: Option<CacheTtl>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("workspace_id is not a valid header value")]
    Workspace,
}

impl DialectConfig for Config {
    fn admit(&self, _: &Common, codec: CodecName) -> Result<Profile, DialectError> {
        if codec != CodecName::Messages {
            return Err(UnsupportedCodec {
                dialect: Dialect::Anthropic,
                codec,
            }
            .into());
        }
        let codec = Codec::Messages(messages::Dialect {
            cache_ttl: self.cache_ttl,
            ..messages()
        });
        let mut profile = Profile::new(codec, transport(), Dialect::Anthropic);
        if let Some(workspace) = &self.workspace_id {
            let value = HeaderValue::from_str(workspace).map_err(|_| Error::Workspace)?;
            profile.headers.insert(
                HeaderName::from_static("anthropic-workspace-id"),
                Value::Fixed(value),
            );
        }
        Ok(profile)
    }

    /// The key travels as the codec's own, `x-api-key`.
    fn credentials(
        &self,
        profile: &Profile,
        connection: &Connection,
    ) -> Result<Headers<Pending>, BuildError> {
        Ok(super::key(
            connection,
            Scheme::standard(profile.codec.name()),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{
        Provider,
        dialect::{
            Sourced,
            tests::{common, head, heads, provider, request},
        },
        http::transport::tests::header_values,
    };
    use futures_util::StreamExt;

    #[tokio::test]
    async fn key_and_workspace_ride_the_version_header() {
        let keyed = head(&Config::default(), CodecName::Messages, "k").await;
        assert!(keyed.starts_with("POST /v1/messages HTTP/1.1"));
        assert_eq!(header_values(&keyed, "x-api-key"), ["k"]);
        assert!(header_values(&keyed, "authorization").is_empty());
        assert_eq!(header_values(&keyed, "anthropic-version"), ["2023-06-01"]);
        let configured = Config {
            workspace_id: Some("wrkspc_1".into()),
            cache_ttl: Some(CacheTtl::OneHour),
        };
        assert_eq!(
            configured
                .admit(&Common::default(), CodecName::Messages)
                .unwrap()
                .codec,
            Codec::Messages(messages::Dialect {
                cache_ttl: Some(CacheTtl::OneHour),
                ..messages()
            })
        );
        let mut heads = heads(1, async |root| {
            let mut common = common(root, Some(Sourced::Literal("k".into())));
            // A configured beta joins the dialect's rather than replacing them.
            common.headers.insert(
                "anthropic-beta".into(),
                Sourced::Literal("context-1m-2025-08-07".into()),
            );
            let provider = provider("test", &configured, CodecName::Messages, &common);
            let mut context = provider.open_context("c".parse().unwrap()).unwrap();
            drop(context.invoke(request()).next().await);
        })
        .await;
        let head = heads.remove(0);
        assert_eq!(header_values(&head, "x-api-key"), ["k"]);
        assert_eq!(header_values(&head, "anthropic-workspace-id"), ["wrkspc_1"]);
        assert_eq!(
            header_values(&head, "anthropic-beta"),
            ["thinking-binding-controls-2026-08-01, context-1m-2025-08-07"]
        );
    }

    #[test]
    fn a_spend_cap_is_billing_not_a_rate_limit() {
        use crate::provider::{ProviderErrorKind, dialect::tests::rejected};
        let kind = |body| rejected(&Config::default(), CodecName::Messages, 429, body, None);
        let capped = serde_json::json!({"error":{"type":"rate_limit_error",
            "details":{"error_code":"enforced_spend_limit_reached"}}});
        assert_eq!(kind(capped), ProviderErrorKind::Billing);
        let limited = serde_json::json!({"error":{"type":"rate_limit_error"}});
        assert!(matches!(
            kind(limited),
            ProviderErrorKind::RateLimited { .. }
        ));
    }

    #[test]
    fn speaks_only_messages() {
        let chat = CodecName::ChatCompletions;
        assert_eq!(
            Config::default()
                .admit(&Common::default(), chat)
                .unwrap_err(),
            UnsupportedCodec {
                dialect: Dialect::Anthropic,
                codec: chat
            }
            .into()
        );
    }
}
