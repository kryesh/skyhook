//! Provider-neutral agent harness and runtime APIs.

mod error;
pub mod handle;
pub mod harness;
pub mod interaction;
pub mod profile;
mod runtime;
pub mod todo;

pub use error::HarnessError;
pub use handle::SessionHandle;
pub use harness::{Harness, HarnessBuilder};
pub use interaction::{
    Question, QuestionError, QuestionFuture, QuestionHandler, QuestionOption, RuntimeEvent,
};
pub use todo::{TodoItem, TodoStatus};
