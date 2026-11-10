//! api.anthropic.com: the Messages API with its version header, `x-api-key`,
//! prefix-bound thinking, cache breakpoint lifetimes, and workspace-scoped keys.

use crate::provider::codec::errors::{ErrorSignals, error_object};
use serde::Deserialize;

use super::{BaseUrl, Dialect, DialectConfig, DialectError, Scheme, Wire};
use crate::provider::{
    ProviderErrorKind,
    codec::{
        CacheTtl, Codec, CodecName, StateRole, UnsignedReasoning,
        messages::{self, SystemState, ThinkingBinding},
    },
    http::{Transport, headers::HeaderText},
};
use skyhook_media::ImageLimits;

/// What the service accepts of an image: 8000 px, and 7.5 MiB, which is 10 MiB
/// as base64.
const IMAGES: ImageLimits = ImageLimits::new::<8000, { 15 * 512 * 1024 }>();

/// Anthropic's Messages conventions, wherever the service is reached.
/// Signed thinking is bound to the exact conversation before it;
/// letting the service drop a mismatched block keeps mode switches and compaction
/// from failing, and the runtime still unbinds client-side. System-role state is
/// turn-scoped, which keeps that conversation fixed as the state changes. The service
/// takes back only thinking it signed.
pub(super) fn messages() -> messages::Dialect {
    messages::Dialect {
        thinking_binding: ThinkingBinding::DropOnMismatch,
        unsigned: UnsignedReasoning::Dropped,
        system_state: SystemState::TurnScoped,
        ..messages::Dialect::standard()
    }
}

/// The service's beta for `block_binding`.
const BINDING_BETA: &str = "thinking-binding-controls-2026-08-01";

/// The service's beta for turn-scoped (`clear_at`) system messages.
const CLEAR_AT_BETA: &str = "mid-conversation-system-clear-at-2026-08-21";

/// Send state in the configured role, `system` by default, and announce the betas
/// Anthropic's conventions need: block binding, and turn-scoped system messages when
/// state takes the system role.
pub(super) fn state(wire: &mut Wire, configured: Option<StateRole>) {
    let state_role = wire.state_role(configured, StateRole::System);
    let turn_scoped = (state_role == StateRole::System).then_some(CLEAR_AT_BETA);
    wire.listed(
        messages::BETA_HEADER,
        [BINDING_BETA].into_iter().chain(turn_scoped),
    );
}

/// A spend cap is reported as a rate limit without a retry hint.
const ERRORS: ErrorSignals = ErrorSignals(|status, native, _| {
    #[derive(Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum Code {
        EnforcedSpendLimitReached,
    }
    #[derive(Deserialize)]
    struct Details {
        error_code: Code,
    }
    #[derive(Deserialize)]
    struct SpendCap {
        details: Details,
    }
    if status != Some(429) {
        return None;
    }
    let SpendCap {
        details: Details {
            error_code: Code::EnforcedSpendLimitReached,
        },
    } = SpendCap::deserialize(error_object(native)).ok()?;
    Some(ProviderErrorKind::Billing)
});

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
        /// The role runtime state takes: `system` (default) needs a model that supports
        /// mid-conversation system messages; `user` sends it as user content.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub state_role: Option<StateRole> => optional(StateRole),
    }
}

impl DialectConfig for Config {
    fn base_url() -> BaseUrl {
        BaseUrl::Default("https://api.anthropic.com/v1")
    }

    /// The key travels as the codec's own, `x-api-key`.
    fn admit(&self, codec: CodecName) -> Result<Wire, DialectError> {
        if codec != CodecName::Messages {
            return Err(DialectError::UnsupportedCodec {
                dialect: Dialect::Anthropic,
                codec,
            });
        }
        let codec = Codec::Messages(messages::Dialect {
            cache_ttl: self.cache_ttl,
            ..messages()
        });
        let mut wire = Wire {
            key: Scheme::XApiKey,
            ..Wire::new(codec, transport(), Dialect::Anthropic, IMAGES)
        };
        state(&mut wire, self.state_role);
        if let Some(workspace) = &self.workspace_id {
            wire.discriminate("workspace_id", workspace);
            wire.fixed("anthropic-workspace-id", workspace.value());
        }
        Ok(wire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::dialect::tests::replay_scope;
    use crate::provider::dialect::tests::{ROOT, common, head, provider, request};
    use skyhook_worker::secret::Credential;
    use std::assert_matches;

    /// The service takes back only thinking it signed, so unsigned thinking stays
    /// local even while the context is unsigned; a server that accepts it gets it.
    #[test]
    fn unsigned_thinking_is_never_sent() {
        use crate::provider::{
            codec::common::tests::envelope,
            dialect::compatible,
            protocol::{AssistantItem, Binding, Message, ReplayFormat, entries},
        };
        use serde_json::json;
        let mut request = request();
        let thinking = |payload, binding| {
            let replay = envelope(
                ReplayFormat::Messages,
                request.model.as_str(),
                payload,
                binding,
            );
            AssistantItem::reasoning("r", 0, "", Some(replay))
        };
        let unsigned = json!({"type":"thinking", "thinking":"open"});
        let turn = vec![
            thinking(unsigned.clone(), Binding::Free),
            AssistantItem::text("t", 1, "said"),
        ];
        request.context.extend(entries([Message::Assistant(turn)]));
        let sent = |settings: &dyn DialectConfig| {
            let codec = settings.admit(CodecName::Messages).unwrap().codec;
            let body = codec
                .encode(&request, Default::default(), &"c".parse().unwrap())
                .unwrap()
                .body;
            body["messages"][1]["content"][0].clone()
        };
        assert_eq!(sent(&Config::default())["type"], "text");
        assert_eq!(sent(&compatible::Config::default()), unsigned);
    }

    #[test]
    fn replay_identity_tracks_workspace_not_cache_lifetime() {
        let scope = |yaml| replay_scope::<Config>(yaml, CodecName::Messages);
        assert_eq!(scope("{}"), scope("cache_ttl: 1h"));
        // State in another role than the default changes the conversation replay saw.
        assert_eq!(scope("{}"), scope("state_role: system"));
        assert_ne!(scope("{}"), scope("state_role: user"));
        // Each part names its setting: equal values of different settings differ.
        assert_ne!(scope("state_role: user"), scope("workspace_id: user"));
        assert_ne!(scope("{}"), scope("workspace_id: workspace_1"));
        assert_ne!(
            scope("workspace_id: workspace_1"),
            scope("workspace_id: workspace_2")
        );
    }

    #[tokio::test]
    async fn key_and_workspace_ride_the_version_header() {
        // State in the user role needs only the block-binding beta.
        let user = Config {
            state_role: Some(StateRole::User),
            ..Config::default()
        };
        let binding_only = head(&user, CodecName::Messages, "k").await;
        assert_eq!(
            binding_only.header("anthropic-beta"),
            ["thinking-binding-controls-2026-08-01"]
        );
        let keyed = head(&Config::default(), CodecName::Messages, "k").await;
        assert_eq!(keyed.endpoint.path(), "/v1/messages");
        assert_eq!(keyed.header("x-api-key"), ["k"]);
        assert!(keyed.header("authorization").is_empty());
        assert_eq!(keyed.header("anthropic-version"), ["2023-06-01"]);
        let configured = Config {
            workspace_id: Some("wrkspc_1".parse().unwrap()),
            cache_ttl: Some(CacheTtl::OneHour),
            state_role: None,
        };
        let mut common = common(ROOT, Some(Credential::Literal("k".into())));
        // A configured beta joins the dialect's rather than replacing them.
        common.headers.insert(
            "anthropic-beta".into(),
            Credential::Literal("context-1m-2025-08-07".into()),
        );
        let provider = provider("test", &configured, CodecName::Messages, &common);
        let sent = provider.sent(request()).await.unwrap();
        assert_eq!(sent.header("x-api-key"), ["k"]);
        assert_eq!(sent.header("anthropic-workspace-id"), ["wrkspc_1"]);
        assert_eq!(
            sent.header("anthropic-beta"),
            [
                "thinking-binding-controls-2026-08-01, mid-conversation-system-clear-at-2026-08-21, context-1m-2025-08-07"
            ]
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
        assert_matches!(kind(limited), ProviderErrorKind::RateLimited);
    }
}
