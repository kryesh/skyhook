//! Errors surfaced by harness construction and agent execution.

use thiserror::Error;

use crate::session::{CompactionFault, Failure, TurnFailure};
use crate::target::TargetError;
use crate::tool::catalog::RegistryError;
use crate::{job::JobError, provider::ProviderError, session::SessionError, tool::ToolError};

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
    Execution(ToolError),
    #[error(transparent)]
    Job(JobError),
    #[error("host capability `{0}` has not been declared")]
    UndeclaredHostCapability(crate::tool::policy::HostCapability),
    /// Tools name the workspace as text, which this one is not.
    #[error("workspace {} is not valid Unicode", .0.display())]
    UnspelledWorkspace(std::path::PathBuf),
    #[error("session belongs to host `{session}`, cannot activate as `{host}`")]
    HostMismatch {
        session: crate::identity::HostName,
        host: crate::identity::HostName,
    },
    #[error("a default model is required")]
    MissingDefaultModel,
    #[error("unknown model `{0}`")]
    UnknownModel(crate::provider::model::ModelRef),
    #[error("agent `{0}` has no journaled model to resume under")]
    NoRecordedModel(crate::identity::AgentId),
    #[error("unknown mode `{0}`")]
    UnknownMode(crate::config::mode::ModeName),
    #[error(transparent)]
    Prompt(#[from] crate::prompts::TemplateError),
    #[error("invalid model `{model}`: {error}")]
    InvalidModel {
        model: crate::provider::model::ModelRef,
        error: crate::provider::model::LimitsError,
    },
    #[error("compaction failed: {0}")]
    Compaction(#[from] CompactionError),
    #[error("agent stopped before completing the request")]
    AgentStopped,
    #[error("the output limit was reached before any answer or tool call")]
    OutputLimit,
    #[error("the model declined to respond: {0}")]
    Refused(String),
    #[error("agent turn was interrupted")]
    Interrupted,
    /// The harness's [`ResumeHook`](super::ResumeHook) failed; the next work calls it again.
    #[error("the session could not resume: {0}")]
    Resume(Box<dyn std::error::Error + Send + Sync>),
    /// The harness's [`HostState`](super::HostState) could not read an agent's events; the
    /// request they would ride is not made.
    #[error("host events could not be read: {0}")]
    HostEvents(Box<dyn std::error::Error + Send + Sync>),
    #[error("child-agent nesting limit reached")]
    ChildDepth,
    #[error("image limits were exceeded")]
    ImageLimit,
    #[error("model `{0}` does not support image inputs")]
    ImagesUnsupported(crate::provider::protocol::WireModel),
}

impl HarnessError {
    /// Whether this is the failure of the session's journal, which stopped it.
    pub fn journal_failed(&self) -> bool {
        matches!(
            self,
            Self::Session(crate::session::SessionError::JournalFailed(_))
        )
    }
}

/// A session error reaches the host in one shape, whichever layer met it.
impl From<JobError> for HarnessError {
    fn from(error: JobError) -> Self {
        match error {
            JobError::Session(error) => Self::Session(error),
            JobError::Interrupted => Self::Interrupted,
            error => Self::Job(error),
        }
    }
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
    UnknownJob(skyhook_worker::job::JobId),
    #[error("handover job {job} output cannot be presented: {source}")]
    HandoverOutput {
        job: skyhook_worker::job::JobId,
        #[source]
        source: ToolError,
    },
    #[error(transparent)]
    Checkpoint(#[from] crate::session::CheckpointError),
}

impl From<&HarnessError> for CompactionFault {
    fn from(error: &HarnessError) -> Self {
        match error {
            HarnessError::Interrupted => Self::Interrupted,
            HarnessError::Provider(error)
                if error.kind() == crate::provider::ProviderErrorKind::ContextWindowExceeded =>
            {
                Self::Overflowed
            }
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

impl From<&ProviderError> for Failure {
    fn from(error: &ProviderError) -> Self {
        Self::Provider(error.message.clone(), error.kind())
    }
}

impl From<&HarnessError> for Failure {
    fn from(error: &HarnessError) -> Self {
        match error {
            HarnessError::Refused(detail) => Self::Refused(detail.clone()),
            HarnessError::OutputLimit => Self::OutputLimit,
            HarnessError::Provider(error) => error.into(),
            error => Self::Other(error.to_string()),
        }
    }
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
    use super::*;
    use crate::session::FailureKind;

    #[test]
    fn only_a_model_failure_is_a_summary_fault() {
        let refused = HarnessError::Refused("no".into());
        let summary = CompactionFault::Summary(FailureKind::Refused);
        assert_eq!(CompactionFault::from(&refused), summary);
        let depth = HarnessError::ChildDepth;
        let other = CompactionFault::from(&depth);
        assert_eq!(other, CompactionFault::Other(depth.to_string()));
    }
}
