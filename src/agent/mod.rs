//! Provider-neutral agent harness and runtime APIs.

mod error;
mod interaction;
mod observation;
mod runtime;
pub(crate) mod todo;

pub use error::{CompactionError, CompactionFault, Failure, HarnessError, TurnFailure};
pub(crate) use error::{FailureKind, FaultKind};
pub use interaction::{
    Question, QuestionAnswer, QuestionError, QuestionFuture, QuestionHandler, QuestionOption,
    QuestionReply, RuntimeEvent,
};
pub use observation::{
    AgentActivity, Applied, ContextGauge, ContextUsage, Observation, ObservationSnapshot,
    ObservedActivity, ObservedEvent, ObservedResponse, Settlement, StoppedReading, Views,
};
pub use runtime::{
    ContinueOutcome, Harness, HarnessBuilder, QueuedPrompt, QueuedPromptCancellation,
    QueuedPromptCommitted, QueuedPromptReceipt, Selection, SessionHandle, SessionMode,
    SessionModel,
};
pub(crate) use todo::ReplaceError;
pub use todo::{TodoItem, TodoStatus};

pub(crate) use interaction::QuestionOutput;
pub(crate) use observation::live;
pub(crate) use runtime::{Agents, ModelEntry, activities, activity, observed_agents};
