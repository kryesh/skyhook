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
    AgentActivity, ContextGauge, ContextUsage, Observation, ObservationSnapshot, ObservedActivity,
    ObservedEvent, ObservedResponse, Settlement, StoppedReading,
};
pub use runtime::{
    ContinueOutcome, Harness, HarnessBuilder, QueuedPrompt, QueuedPromptCancellation,
    QueuedPromptCommitted, QueuedPromptReceipt, Selection, SessionHandle, SessionMode,
    SessionModel,
};
pub(crate) use todo::ReplaceError;
pub use todo::{TodoItem, TodoStatus};

pub(crate) use interaction::QuestionOutput;
pub(crate) use observation::{Responses, Views, live};
#[cfg(test)]
pub(crate) use runtime::stopped_turn;
pub(crate) use runtime::{Agents, ModelEntry, activities};
