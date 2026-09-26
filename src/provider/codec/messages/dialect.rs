//! The conventions that vary between Messages servers.
use reqwest::header::{HeaderName, HeaderValue};

use crate::provider::{
    codec::{BodyPath, CacheTtl, Effort, EffortLevels, Identity, ToolNames, path},
    http::{Headers, headers::Value},
};

/// Anthropic thinking settings: `off` and `adaptive` select a thinking mode;
/// the rest are `output_config.effort` levels under adaptive thinking.
const ANTHROPIC_EFFORT: EffortLevels =
    EffortLevels(&["off", "adaptive", "low", "medium", "high", "xhigh", "max"]);

/// What happens to a signed thinking block whose conversation prefix changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ThinkingBinding {
    /// The service rejects the request; the runtime unbinds client-side first.
    Unenforced,
    /// The service drops the block instead of failing (`block_binding`, under
    /// its beta), backing up the client-side unbinding.
    DropOnMismatch,
}

const BINDING_BETA: &str = "thinking-binding-controls-2026-08-01";

/// Beta features a request announces: a list header, which configured and
/// dialect-selected betas join.
const BETA_HEADER: HeaderName = HeaderName::from_static("anthropic-beta");

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
}

impl Dialect {
    pub(crate) fn anthropic() -> Self {
        Self {
            output_limit: const { path("max_tokens") },
            effort: Effort {
                path: const { path("output_config.effort") },
                levels: ANTHROPIC_EFFORT,
            },
            tool_names: ToolNames::Anthropic,
            identity: Identity::OMITTED,
            cache_ttl: None,
            thinking_binding: ThinkingBinding::Unenforced,
        }
    }

    /// The API version every request names, and the betas these conventions need.
    pub(crate) fn headers(&self) -> Headers {
        let mut headers = Headers::default();
        headers.insert(
            HeaderName::from_static("anthropic-version"),
            Value::Fixed(HeaderValue::from_static("2023-06-01")),
        );
        let betas =
            (self.thinking_binding == ThinkingBinding::DropOnMismatch).then_some(BINDING_BETA);
        headers.list(
            BETA_HEADER,
            betas.map(|beta| Value::Fixed(HeaderValue::from_static(beta))),
        );
        headers
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn every_request_names_the_version_and_only_needed_betas() {
        let fixed = |binding| async move {
            let dialect = Dialect {
                thinking_binding: binding,
                ..Dialect::anthropic()
            };
            dialect.headers().resolve().await.unwrap().map()
        };
        let bound = fixed(ThinkingBinding::DropOnMismatch).await;
        assert_eq!(bound["anthropic-version"], "2023-06-01");
        assert_eq!(bound[BETA_HEADER], BINDING_BETA);
        let plain = fixed(ThinkingBinding::Unenforced).await;
        assert_eq!(plain["anthropic-version"], "2023-06-01");
        assert!(!plain.contains_key(BETA_HEADER));
    }
}
