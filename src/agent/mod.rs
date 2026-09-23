//! Provider-neutral agent harness and runtime APIs.

mod error;
mod interaction;
mod observation;
mod runtime;
mod todo;

pub use error::HarnessError;
pub use interaction::{
    Question, QuestionError, QuestionFuture, QuestionHandler, QuestionOption, RuntimeEvent,
};
pub use observation::{
    AgentActivity, ContextUsage, Observation, ObservationSnapshot, ObservedEvent, ObservedResponse,
};
pub use runtime::{
    ContinueOptions, ContinueOutcome, Harness, HarnessBuilder, PromptOptions, QueuedPrompt,
    QueuedPromptCancellation, QueuedPromptReceipt, SessionHandle,
};
pub use todo::{TodoItem, TodoSnapshot, TodoStatus};

pub(crate) use interaction::QuestionOutput;
