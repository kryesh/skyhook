//! Provider-neutral agent harness and runtime APIs.

mod error;
mod interaction;
mod profile;
mod runtime;

pub use error::HarnessError;
pub use interaction::{
    Question, QuestionError, QuestionFuture, QuestionHandler, QuestionOption, RuntimeEvent,
};
pub use profile::AgentProfile;
pub use runtime::{Harness, HarnessBuilder, SessionHandle};

pub(crate) use interaction::QuestionOutput;
