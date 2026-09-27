//! Errors surfaced by harness construction and agent execution.

use thiserror::Error;

use crate::{
    job::JobError,
    provider::{ProviderError, ProviderErrorKind},
    session::SessionError,
    target::TargetError,
    tool::{RegistryError, ToolError},
};

#[derive(Debug, Error)]
pub enum HarnessError {
    #[error("workspace or configuration I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    Registry(#[from] RegistryError),
    #[error(transparent)]
    Target(#[from] TargetError),
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error(transparent)]
    Execution(#[from] ToolError),
    #[error(transparent)]
    Job(#[from] JobError),
    #[error("a default model is required")]
    MissingDefaultModel,
    #[error("unknown model `{0}`")]
    UnknownModel(crate::provider::profile::ModelRef),
    #[error("agent `{0}` has no journaled model to resume under")]
    NoRecordedModel(crate::identity::AgentId),
    #[error("unknown mode `{0}`")]
    UnknownMode(crate::tool::policy::ModeName),
    #[error("mode `{0}` cannot be applied: only the root agent changes mode")]
    ModeChangeUnsupported(crate::tool::policy::ModeName),
    #[error("invalid model `{model}`: {error}")]
    InvalidModel {
        model: crate::provider::profile::ModelRef,
        error: crate::provider::profile::LimitsError,
    },
    #[error("compaction failed: {0}")]
    Compaction(#[from] CompactionError),
    #[error("agent stopped before completing the request")]
    AgentStopped,
    #[error("duplicate question id `{0}`")]
    DuplicateQuestion(String),
    #[error("child question has no owning agent job")]
    UnownedQuestion,
    #[error("answers to multiple questions must be keyed by question id")]
    UnkeyedAnswers,
    #[error("answer contains no pending question ids")]
    UnmatchedAnswers,
    #[error("provider returned no assistant content")]
    EmptyResponse,
    #[error("the output limit was reached before any answer or tool call")]
    OutputLimit,
    /// The model declined to answer. Deterministic for a given request, so the
    /// runtime never retries it: only a parent agent or a human may retry,
    /// optionally on another model.
    #[error("the model declined to respond: {0}")]
    Refused(String),
    #[error("provider aborted response")]
    ProviderAborted,
    #[error("agent turn was interrupted")]
    Interrupted,
    #[error("child-agent nesting limit reached")]
    ChildDepth,
    #[error("image limits were exceeded")]
    ImageLimit,
    #[error("model `{0}` does not support image inputs")]
    ImagesUnsupported(crate::provider::protocol::WireModel),
    #[error("session start event is missing")]
    MissingSessionStart,
}

/// Why a compaction attempt installed no checkpoint.
#[derive(Debug, Error)]
pub enum CompactionError {
    #[error("summarization was truncated; original history is retained")]
    Truncated,
    #[error("summarizer returned a tool call; no tools were executed")]
    ToolCall,
    #[error("invalid structured continuation: {0}")]
    Continuation(#[source] serde_json::Error),
    #[error("unknown handover job {0}")]
    UnknownJob(crate::identity::JobId),
    #[error("handover job {job} output cannot be presented: {source}")]
    HandoverOutput {
        job: crate::identity::JobId,
        #[source]
        source: ToolError,
    },
    #[error(transparent)]
    Checkpoint(#[from] crate::session::CheckpointError),
}

impl CompactionError {
    /// Whether a fresh summary can succeed: the model's answer was unusable, or the
    /// todos it reconciled changed meanwhile. A broken invariant would fail again.
    #[must_use]
    pub fn retryable(&self) -> bool {
        use crate::session::CheckpointError::StaleTodos;
        !matches!(self, Self::Checkpoint(reason) if *reason != StaleTodos)
    }
}

crate::named_enum::detailed_enum! {
    /// A compaction round that installed no checkpoint, as journaled by kind. A
    /// failed summary attempt journals its own failure, which `Summary` names.
    #[derive(Clone, Debug, PartialEq, Eq, Error, serde::Serialize)]
    #[serde(tag = "kind", content = "detail")]
    pub enum CompactionFault / FaultKind {
        #[error("summarization was truncated")]
        Truncated = "truncated",
        #[error("summarizer returned a tool call")]
        ToolCall = "tool_call",
        /// The parser's account of the answer it rejected.
        #[error("invalid structured continuation: {0}")]
        Continuation(String) = "continuation",
        #[error("the summary selected an unknown job")]
        UnknownJob = "unknown_job",
        #[error("a selected job's output cannot be presented")]
        HandoverOutput = "handover_output",
        #[error("{0}")]
        Checkpoint(crate::session::CheckpointError) = "checkpoint",
        #[error("compaction was interrupted")]
        Interrupted = "interrupted",
        #[error("summarization failed: {0}")]
        Summary(FailureKind) = "summary",
        /// A failure outside the summary's model attempts, such as journaling it.
        #[error("{0}")]
        Other(String) = "other",
    }
}

impl From<&HarnessError> for CompactionFault {
    fn from(error: &HarnessError) -> Self {
        match error {
            HarnessError::Interrupted => Self::Interrupted,
            HarnessError::Compaction(compaction) => match compaction {
                CompactionError::Truncated => Self::Truncated,
                CompactionError::ToolCall => Self::ToolCall,
                CompactionError::Continuation(error) => Self::Continuation(error.to_string()),
                CompactionError::UnknownJob(_) => Self::UnknownJob,
                CompactionError::HandoverOutput { .. } => Self::HandoverOutput,
                CompactionError::Checkpoint(reason) => Self::Checkpoint(*reason),
            },
            error => match Failure::from(error) {
                Failure::Other(detail) => Self::Other(detail),
                failure => Self::Summary(failure.kind()),
            },
        }
    }
}

crate::named_enum::detailed_enum! {
    /// Why a model attempt, or the agent turn it ended, failed, as journaled.
    #[derive(Clone, Debug, PartialEq, Eq, Error, serde::Serialize)]
    #[serde(into = "FailureReport")]
    pub enum Failure / FailureKind {
        #[error("the model declined to respond: {0}")]
        Refused(String) = "refused",
        #[error("provider aborted response")]
        Aborted = "aborted",
        #[error("provider returned no assistant content")]
        Empty = "empty",
        #[error("the output limit was reached before any answer or tool call")]
        OutputLimit = "output_limit",
        #[error("{1}: {0}")]
        Provider(String, ProviderErrorKind) = "provider",
        #[error("{0}")]
        Other(String) = "other",
    }
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

impl From<&ProviderError> for Failure {
    fn from(error: &ProviderError) -> Self {
        Self::Provider(error.message.clone(), error.kind())
    }
}

impl From<&HarnessError> for Failure {
    fn from(error: &HarnessError) -> Self {
        match error {
            HarnessError::Refused(detail) => Self::Refused(detail.clone()),
            HarnessError::ProviderAborted => Self::Aborted,
            HarnessError::EmptyResponse => Self::Empty,
            HarnessError::OutputLimit => Self::OutputLimit,
            HarnessError::Provider(error) => error.into(),
            error => Self::Other(error.to_string()),
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

impl From<&HarnessError> for TurnFailure {
    fn from(error: &HarnessError) -> Self {
        match error {
            HarnessError::Interrupted => Self::Interrupted,
            error => Self::Failed(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn only_a_model_failure_is_a_summary_fault() {
        let refused = HarnessError::Refused("no".into());
        let summary = CompactionFault::Summary(FailureKind::Refused);
        assert_eq!(CompactionFault::from(&refused), summary);
        let journaling = HarnessError::MissingSessionStart;
        let other = CompactionFault::from(&journaling);
        assert_eq!(other, CompactionFault::Other(journaling.to_string()));
    }

    #[test]
    fn a_failure_reports_its_kind_provider_class_and_text() {
        let provider = Failure::Provider("down".into(), ProviderErrorKind::Transport);
        let report = json!({"kind":"provider","provider":"Transport","message":"Transport: down"});
        assert_eq!(serde_json::to_value(provider).unwrap(), report);
        let empty = json!({"kind":"empty","message":"provider returned no assistant content"});
        assert_eq!(serde_json::to_value(Failure::Empty).unwrap(), empty);
    }
}
