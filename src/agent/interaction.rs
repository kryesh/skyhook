//! Host interaction contracts and streamed runtime events.

use std::{future::Future, pin::Pin};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::{identity::AgentId, session::EventRecord};

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize, PartialEq, Eq)]
pub struct QuestionOption {
    /// Short answer label returned to the agent.
    pub label: String,
    /// Explanation of this choice's effect or tradeoff.
    pub description: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize, PartialEq, Eq)]
pub struct Question {
    /// Stable identifier used to associate the answer with this question.
    pub id: String,
    /// Complete question shown to the user or owning parent agent.
    pub prompt: String,
    /// Suggested mutually exclusive answers. An empty list permits free-form input.
    #[serde(default)]
    pub options: Vec<QuestionOption>,
}

pub type QuestionFuture =
    Pin<Box<dyn Future<Output = Result<Value, QuestionError>> + Send + 'static>>;

pub trait QuestionHandler: Send + Sync {
    fn ask(&self, agent: AgentId, questions: Vec<Question>) -> QuestionFuture;
}

#[derive(Clone, Debug, Error)]
pub enum QuestionError {
    #[error("no host question handler is configured")]
    Unavailable,
    #[error("question failed: {0}")]
    Failed(String),
}

#[derive(Clone, Debug)]
pub enum RuntimeEvent {
    Record(EventRecord),
    TextDelta { agent: AgentId, text: String },
    ReasoningDelta { agent: AgentId, text: String },
    TurnCompleted { agent: AgentId, text: String },
}
