//! Provider-neutral agent harness and runtime APIs.

mod error;
mod interaction;
mod observation;
mod runtime;
mod todo;

pub use error::{HarnessError, TurnFailure};
pub use interaction::{
    Question, QuestionError, QuestionFuture, QuestionHandler, QuestionOption, RuntimeEvent,
};
pub use observation::{
    AgentActivity, ContextUsage, Observation, ObservationSnapshot, ObservedEvent, ObservedResponse,
    Settlement,
};
pub use runtime::{
    ContinueOutcome, Harness, HarnessBuilder, QueuedPrompt, QueuedPromptCancellation,
    QueuedPromptReceipt, Selection, SessionHandle, SessionMode, SessionModel,
};
pub use todo::{TodoItem, TodoSnapshot, TodoStatus};

pub(crate) use interaction::QuestionOutput;
pub(crate) use runtime::{Catalog, ModelEntry};
