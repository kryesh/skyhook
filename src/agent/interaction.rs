//! Host interaction contracts and streamed runtime events.

use std::{future::Future, pin::Pin};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::session::RequestSeq;
use crate::{identity::AgentId, session::EventRecord};

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize, PartialEq, Eq)]
pub struct QuestionOption {
    /// Short answer label returned as a string, or as `answer` alongside an optional `comment`.
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
    /// Suggested mutually exclusive answers; free-form input is always allowed. Selecting a
    /// suggestion returns its label, or {"answer": "label", "comment": "text"} with a non-blank comment.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<QuestionOption>,
}

/// A suspended child's question batch, returned in its agent job's output.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, Serialize, PartialEq, Eq)]
pub(crate) struct QuestionOutput {
    pub questions: Vec<Question>,
}

pub type QuestionFuture =
    Pin<Box<dyn Future<Output = Result<Value, QuestionError>> + Send + 'static>>;

pub trait QuestionHandler: Send + Sync {
    /// Present a runtime-merged question batch. A single question accepts any JSON answer;
    /// multiple questions require an object keyed by each stable `Question::id`.
    ///
    /// Host interfaces return a selected suggestion's label or free-form input as a string.
    /// A suggestion with a non-whitespace comment returns
    /// `{"answer": "selected label", "comment": "user text"}` instead; blank comments leave
    /// the label as a string. Each value in a multi-question answer object uses the same
    /// format. The runtime preserves these answer values without interpreting their fields.
    ///
    /// `background` is true if any ask, or any enclosing tool job up to the owning
    /// agent, runs in the background; a mixed batch is a background batch. Hosts may
    /// dismiss and reopen a foreground batch by retaining its pending future, but
    /// cancelling a background batch must resolve the future with an error: the
    /// running agent receives the failure and it cannot be undone.
    fn ask(&self, agent: AgentId, questions: Vec<Question>, background: bool) -> QuestionFuture;
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
    /// Provider-native item/block lifecycle, validated by the observation reducer.
    ResponseEvent {
        agent: AgentId,
        request: RequestSeq,
        event: crate::provider::protocol::ResponseEvent,
    },
    ResponseSettled {
        agent: AgentId,
        request: RequestSeq,
        settlement: super::Settlement,
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
