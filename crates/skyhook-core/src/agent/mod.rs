//! Provider-neutral agent harness and runtime APIs.

mod error;
mod interaction;
mod observation;
mod profile;
mod runtime;
mod todo;

pub use error::HarnessError;
pub use interaction::{
    Question, QuestionError, QuestionFuture, QuestionHandler, QuestionOption, RuntimeEvent,
};
pub use observation::{
    AgentActivity, ContextUsage, LiveResponse, Observation, ObservationSnapshot, ObservedEvent,
};
pub use profile::AgentProfile;
pub use runtime::{Harness, HarnessBuilder, PromptOptions, SessionHandle};
pub use todo::{TodoItem, TodoSnapshot, TodoStatus};

pub(crate) use interaction::QuestionOutput;
