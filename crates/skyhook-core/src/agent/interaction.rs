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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<QuestionOption>,
}

/// A suspended child's question batch, returned in its agent job's output.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub(crate) struct QuestionOutput {
    pub questions: Vec<Question>,
}

pub type QuestionFuture =
    Pin<Box<dyn Future<Output = Result<Value, QuestionError>> + Send + 'static>>;

pub trait QuestionHandler: Send + Sync {
    /// Present a runtime-merged question batch. A single question accepts any JSON answer;
    /// multiple questions require an object keyed by each stable `Question::id`.
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
    Record(Box<EventRecord>),
    TextDelta {
        agent: AgentId,
        request: u64,
        text: String,
    },
    ReasoningDelta {
        agent: AgentId,
        request: u64,
        text: String,
    },
    ResponseSettled {
        agent: AgentId,
        request: u64,
        message: Option<u64>,
        error: Option<String>,
    },
    Activity {
        agent: AgentId,
        activity: super::AgentActivity,
    },
    Context {
        agent: AgentId,
        tokens: u64,
        capacity: u64,
    },
    TurnCompleted {
        agent: AgentId,
        text: String,
    },
}
