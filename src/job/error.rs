//! Job manager errors.
use super::*;

/// Why a destination cannot accept input in its current lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputUnavailableReason {
    State(JobState),
    CancellationRequested,
    ResumeUnavailable,
    /// The session is closing, which restarts nothing.
    Closing,
}

impl std::fmt::Display for InputUnavailableReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::State(state) => write!(formatter, "job is {}", state.presented()),
            Self::CancellationRequested => formatter.write_str("cancellation has been requested"),
            Self::ResumeUnavailable => {
                formatter.write_str("no retained resume handler is available")
            }
            Self::Closing => formatter.write_str("the session is closing"),
        }
    }
}

impl<O> From<JobError> for crate::tool::invocation::OperationError<O> {
    fn from(error: JobError) -> Self {
        use crate::tool::diagnostic::Cause;
        match error {
            JobError::Session(error) => error.into(),
            JobError::InputClosed(_) => Self::input_closed(),
            JobError::Output(error) => Self::from_facts(error.into_facts().0, None),
            JobError::Unknown(job) => Self::cause(Cause::UnknownJob { job }),
            JobError::NotTerminal(job) => Self::cause(Cause::JobNotTerminal { job }),
            JobError::AlreadyTerminal(job) => Self::cause(Cause::JobAlreadyTerminal { job }),
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
    #[error("job {0} is not terminal")]
    NotTerminal(JobId),
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
    /// The journaled change was admitted, but the live phase moved underneath it.
    #[error("job {0} changed phase while its change was journaled")]
    PhaseMoved(JobId),
    #[error("journal entry {sequence} changes a job's phase illegally")]
    IllegalTransition { sequence: u64 },
    #[error("job {0} did not launch an agent")]
    NoChild(JobId),
    #[error("child agent does not belong to job {0}")]
    ChildOwnerMismatch(JobId),
    #[error("child message text does not match the committed assistant text")]
    ChildTextMismatch,
    /// The detached task publishing `owner` stopped before reporting.
    #[error("{owner} owner lost: {source}")]
    OwnerLost {
        owner: &'static str,
        #[source]
        source: tokio::task::JoinError,
    },
    /// Saving or reading the job's output failed; the facts name the stage.
    #[error(transparent)]
    Output(Box<crate::tool::ToolError>),
}
