//! Host interaction contracts and streamed runtime events.

use std::{collections::HashMap, future::Future, pin::Pin};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
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

/// One question's answer as the host collected it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum QuestionAnswer {
    /// Free-form input, or a chosen suggestion's label.
    Text(String),
    /// A chosen suggestion's label with the user's non-blank comment.
    Commented { answer: String, comment: String },
}

/// A host's answers to one batch, keyed by `Question::id`.
pub type QuestionReply = HashMap<String, QuestionAnswer>;

pub type QuestionFuture =
    Pin<Box<dyn Future<Output = Result<QuestionReply, QuestionError>> + Send + 'static>>;

pub trait QuestionHandler: Send + Sync {
    /// Present a runtime-merged question batch and answer each question by its id.
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
        /// Epoch milliseconds, on the journal's clock.
        at: i64,
    },
    Context {
        agent: AgentId,
        usage: super::ContextUsage,
    },
    TurnCompleted {
        agent: AgentId,
    },
}
