//! Why model attempts, agent turns and compaction rounds failed, as journaled.

use skyhook_base::enums::Kinds;
use thiserror::Error;

use crate::provider::ProviderErrorKind;

/// A compaction round that installed no checkpoint, as journaled by kind. A
/// failed summary attempt journals its own failure, which `Summary` names.
#[derive(Clone, Debug, PartialEq, Eq, Error, serde::Serialize, Kinds)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
#[kinds(name = FaultKind)]
pub enum CompactionFault {
    #[error("summarization was truncated")]
    Truncated,
    #[error("summarizer returned a tool call")]
    ToolCall,
    /// The parser's account of the answer it rejected.
    #[error("invalid structured continuation: {0}")]
    Continuation(String),
    #[error("the summary selected an unknown job")]
    UnknownJob,
    #[error("a selected job's output cannot be presented")]
    HandoverOutput,
    #[error("{0}")]
    Checkpoint(crate::session::CheckpointError),
    #[error("compaction was interrupted")]
    Interrupted,
    /// The summary request exceeded the model's context window; a smaller one may fit.
    #[error("the summary exceeded the context window")]
    Overflowed,
    #[error("summarization failed: {0}")]
    Summary(FailureKind),
    /// A failure outside the summary's model attempts, such as journaling it.
    #[error("{0}")]
    Other(String),
}

impl CompactionFault {
    /// Whether a fresh summary can succeed: the model's answer was unusable, or the
    /// todos it reconciled changed meanwhile. A broken invariant would fail again.
    pub fn retryable(&self) -> bool {
        match self {
            Self::Truncated
            | Self::ToolCall
            | Self::Continuation(_)
            | Self::UnknownJob
            | Self::HandoverOutput
            | Self::Overflowed => true,
            Self::Checkpoint(reason) => *reason == crate::session::CheckpointError::StaleTodos,
            Self::Interrupted | Self::Summary(_) | Self::Other(_) => false,
        }
    }

    /// Whether a round that failed with this fault leaves the compaction need for the
    /// next request: a round cut short, or one a fresh summary could pass. A summary that
    /// overflowed already shrank within its round, so the next request decides instead:
    /// it fits, or its own overflow compacts.
    pub fn keeps_need(&self) -> bool {
        match self {
            Self::Overflowed => false,
            Self::Interrupted => true,
            other => other.retryable(),
        }
    }
}

/// Why a model attempt, or the agent turn it ended, failed, as journaled.
#[derive(Clone, Debug, PartialEq, Eq, Error, serde::Serialize, Kinds)]
#[serde(into = "FailureReport")]
#[kinds(name = FailureKind, serde(rename_all = "snake_case"))]
pub enum Failure {
    #[error("the model declined to respond: {0}")]
    Refused(String),
    #[error("the output limit was reached before any answer or tool call")]
    OutputLimit,
    #[error("{1}: {0}")]
    Provider(String, ProviderErrorKind),
    #[error("{0}")]
    Other(String),
}

/// A failure as reported: its kind, a provider failure's class, and its rendered text.
#[derive(serde::Serialize)]
struct FailureReport {
    kind: FailureKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<ProviderErrorKind>,
    message: String,
}

impl From<Failure> for FailureReport {
    fn from(failure: Failure) -> Self {
        Self {
            kind: failure.kind(),
            provider: match failure {
                Failure::Provider(_, class) => Some(class),
                _ => None,
            },
            message: failure.to_string(),
        }
    }
}

/// Why an agent turn ended without an answer. Carried by agent activity, so an
/// owner recognises an interrupt or refusal without reading rendered messages.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum TurnFailure {
    #[error("agent turn was interrupted")]
    Interrupted,
    #[error("child agent cancelled")]
    Cancelled,
    #[error(transparent)]
    Failed(#[from] Failure),
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_failure_reports_its_kind_provider_class_and_text() {
        let provider = Failure::Provider("down".into(), ProviderErrorKind::Transport);
        let report = json!({"kind":"provider","provider":"Transport","message":"Transport: down"});
        assert_eq!(serde_json::to_value(provider).unwrap(), report);
        let limit = json!({"kind":"output_limit","message":"the output limit was reached before any answer or tool call"});
        assert_eq!(serde_json::to_value(Failure::OutputLimit).unwrap(), limit);
    }
}
