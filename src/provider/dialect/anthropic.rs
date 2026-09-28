//! api.anthropic.com: the Messages API with its version header, `x-api-key`,
//! prefix-bound thinking, cache breakpoint lifetimes, and workspace-scoped keys.

use super::{Dialect, DialectConfig, DialectError, Profile, Scheme, UnsupportedCodec};
use crate::provider::{
    ProviderErrorKind,
    codec::{
        CacheTtl, Codec, CodecName,
        messages::{self, ThinkingBinding},
    },
    http::{
        Transport,
        errors::{ErrorRule, ErrorSignals, Field},
        headers::HeaderText,
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
        ..ErrorRule::kind(ProviderErrorKind::Billing)
    }],
    ..ErrorSignals::NONE
};

fn transport() -> Transport {
    Transport {
        errors: ERRORS,
        ..Transport::plain()
    }
}

/// Provider-only options; request settings are declared separately.
pub type Options = crate::provider::settings::Empty;

crate::provider::settings::settings! {
    #[derive(Default)]
    pub struct Config => Patch {
        /// Sent as `anthropic-workspace-id`; required by keys that span workspaces.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub workspace_id: Option<HeaderText> => default,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub cache_ttl: Option<CacheTtl> => default,
    }
}

impl DialectConfig for Config {
    /// The key travels as the codec's own, `x-api-key`.
    fn admit(&self, codec: CodecName) -> Result<Profile, DialectError> {
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
        let mut profile = Profile {
            key: Scheme::XApiKey,
            ..Profile::new(codec, transport(), Dialect::Anthropic)
        };
        if let Some(workspace) = &self.workspace_id {
            profile.discriminate(workspace);
            profile.fixed("anthropic-workspace-id", workspace.value());
        }
        Ok(profile)
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

    #[test]
    fn replay_identity_tracks_workspace_not_cache_lifetime() {
        let scope = |yaml| {
            crate::yaml::parse::<Config>(yaml)
                .unwrap()
                .admit(CodecName::Messages)
                .unwrap()
                .scope
        };
        assert_eq!(scope("{}"), scope("cache_ttl: 1h"));
        assert_ne!(scope("{}"), scope("workspace_id: workspace_1"));
        assert_ne!(
            scope("workspace_id: workspace_1"),
            scope("workspace_id: workspace_2")
        );
    }

    #[tokio::test]
    async fn key_and_workspace_ride_the_version_header() {
        let keyed = head(&Config::default(), CodecName::Messages, "k").await;
        assert!(keyed.starts_with("POST /v1/messages HTTP/1.1"));
        assert_eq!(header_values(&keyed, "x-api-key"), ["k"]);
        assert!(header_values(&keyed, "authorization").is_empty());
        assert_eq!(header_values(&keyed, "anthropic-version"), ["2023-06-01"]);
        let configured = Config {
            workspace_id: Some("wrkspc_1".parse().unwrap()),
            cache_ttl: Some(CacheTtl::OneHour),
        };
        assert_eq!(
            configured.admit(CodecName::Messages).unwrap().codec,
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
        assert!(matches!(kind(limited), ProviderErrorKind::RateLimited));
    }
}
