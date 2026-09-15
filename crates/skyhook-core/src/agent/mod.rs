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
    AgentActivity, ContextUsage, LiveResponse, Observation, ObservationSnapshot, ObservedEvent,
};
pub use runtime::{
    Harness, HarnessBuilder, PreparedQueuedPrompt, PromptOptions, QueueConflict, QueuedPrompt,
    QueuedPromptCancellation, QueuedPromptCommit, QueuedPromptError, QueuedPromptIdentity,
    QueuedPromptRecovery, QueuedPromptToken, RecoveredQueuedPrompt, RecoveredQueuedPromptState,
    SessionHandle,
};
pub use todo::{TodoItem, TodoSnapshot, TodoStatus};

pub(crate) use interaction::QuestionOutput;
