//! The conventions that vary between Messages servers.
use reqwest::header::{HeaderName, HeaderValue};

use crate::provider::{
    codec::{BodyPath, CacheTtl, Effort, EffortLevels, Identity, StateRole, ToolNames, path},
    http::{Headers, headers::Value},
};

/// Messages thinking settings: `off` and `adaptive` select a thinking mode;
/// the rest are `output_config.effort` levels under adaptive thinking.
const MESSAGES_EFFORT: EffortLevels =
    EffortLevels(&["off", "adaptive", "low", "medium", "high", "xhigh", "max"]);

/// What happens to a signed thinking block whose conversation prefix changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ThinkingBinding {
    /// The service rejects the request; the runtime unbinds client-side first.
    Unenforced,
    /// The service drops the block instead of failing (`block_binding`), backing up
    /// the client-side unbinding.
    DropOnMismatch,
}

/// How system-role state is sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SystemState {
    /// Plain system messages; a superseded state is omitted.
    Plain,
    /// System messages the model reads for one turn (`clear_at`) unless persisted, so a
    /// superseded state stays in the conversation without being read.
    TurnScoped,
}

const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Beta features a request announces: a list header, which the betas a vendor
/// dialect and the configuration name join.
pub(crate) const BETA_HEADER: HeaderName = HeaderName::from_static("anthropic-beta");

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Dialect {
    /// Where `max_output` goes; the API requires it.
    pub output_limit: BodyPath,
    pub effort: Effort,
    pub tool_names: ToolNames,
    pub identity: Identity,
    /// Lifetime written on each cache breakpoint; None takes the service default.
    pub cache_ttl: Option<CacheTtl>,
    pub thinking_binding: ThinkingBinding,
    pub state_role: StateRole,
    pub system_state: SystemState,
}

impl Dialect {
    pub(crate) fn standard() -> Self {
        Self {
            output_limit: const { path("max_tokens") },
            effort: Effort {
                path: const { path("output_config.effort") },
                levels: MESSAGES_EFFORT,
            },
            tool_names: ToolNames::Anthropic,
            identity: Identity::OMITTED,
            cache_ttl: None,
            thinking_binding: ThinkingBinding::Unenforced,
            state_role: StateRole::User,
            system_state: SystemState::Plain,
        }
    }

    /// The API version every request names, and the beta list vendors add to.
    pub(crate) fn headers(&self) -> Headers {
        let mut headers = Headers::default();
        headers.insert(
            HeaderName::from_static("anthropic-version"),
            Value::Fixed(HeaderValue::from_static(ANTHROPIC_VERSION)),
        );
        headers.list(BETA_HEADER, []);
        headers
    }
}
