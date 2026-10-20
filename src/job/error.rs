//! Job manager errors.
use super::*;

/// Why a destination cannot accept input in its current lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputUnavailableReason {
    State(JobState),
    CancellationRequested,
    ResumeUnavailable,
}

impl std::fmt::Display for InputUnavailableReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::State(state) => write!(formatter, "job is {}", state.presented()),
            Self::CancellationRequested => formatter.write_str("cancellation has been requested"),
            Self::ResumeUnavailable => formatter.write_str("the job can no longer be resumed"),
        }
    }
}

impl<O> From<JobError> for crate::tool::invocation::OperationError<O> {
    fn from(error: JobError) -> Self {
        Self::from(&error)
    }
}

/// By reference, so one refusal of several calls fails each of them alike.
impl<O> From<&JobError> for crate::tool::invocation::OperationError<O> {
    fn from(error: &JobError) -> Self {
        use crate::tool::diagnostic::Cause;
        match error {
            JobError::Session(error) => Self::session(error),
            JobError::InputClosed(_) => Self::input_closed(),
            JobError::Output(error) => Self::from_facts(error.facts().clone(), None),
            JobError::Unknown(job) => Self::cause(Cause::UnknownJob { job: *job }),
            JobError::AlreadyTerminal(job) => Self::cause(Cause::JobAlreadyTerminal { job: *job }),
            JobError::Interrupted => Self::interrupted(),
            error => Self::failed(error),
        }
    }
}

#[derive(Debug, Error)]
pub enum JobError {
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error("unknown job {0}")]
    Unknown(JobId),
    #[error("job {0} already has a terminal result")]
    AlreadyTerminal(JobId),
    #[error("job {job} input unavailable: {reason}")]
    InputUnavailable {
        job: JobId,
        reason: InputUnavailableReason,
    },
    #[error("job {0} does not accept input")]
    InputUnsupported(JobId),
    #[error("job {0} input channel is closed")]
    InputClosed(JobId),
    #[error("job identifier space is exhausted")]
    InvalidId,
    #[error("the calling turn was interrupted")]
    Interrupted,
    #[error(
        "child `{name}` already exists as job {job}; message it with tool.job({job}).send({{value: ...}}), or pick another name"
    )]
    NameTaken { name: JobName, job: JobId },
    #[error("job {0} did not launch an agent")]
    NoChild(JobId),
    #[error("child agent does not belong to job {0}")]
    ChildOwnerMismatch(JobId),
    #[error("child message text does not match the committed assistant text")]
    ChildTextMismatch,
    /// A notification presented a delivery that is no longer pending as it read it.
    #[error("a presented delivery changed before it was committed")]
    Stale,
    /// Saving or reading the job's output failed; the facts name the stage.
    #[error(transparent)]
    Output(Box<crate::tool::ToolError>),
}
