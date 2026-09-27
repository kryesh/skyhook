//! Provider-neutral agent harness and runtime APIs.

mod error;
mod interaction;
mod observation;
mod runtime;
mod todo;

pub use error::{CompactionError, CompactionFault, Failure, HarnessError, TurnFailure};
pub(crate) use error::{FailureKind, FaultKind};
pub use interaction::{
    Question, QuestionAnswer, QuestionError, QuestionFuture, QuestionHandler, QuestionOption,
    QuestionReply, RuntimeEvent,
};
pub use observation::{
    AgentActivity, ContextUsage, Observation, ObservationSnapshot, ObservedEvent, ObservedResponse,
    Settlement,
};
pub use runtime::{
    ContinueOutcome, Harness, HarnessBuilder, QueuedPrompt, QueuedPromptCancellation,
    QueuedPromptReceipt, Selection, SessionHandle, SessionMode, SessionModel,
};
pub use todo::{TodoItem, TodoStatus};

pub(crate) use interaction::QuestionOutput;
pub(crate) use runtime::{Catalog, ModelEntry};
