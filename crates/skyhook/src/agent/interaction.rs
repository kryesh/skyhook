//! Host interaction contracts and streamed runtime events.

use std::sync::Arc;
use std::{collections::HashMap, future::Future, pin::Pin};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::session::RequestSeq;
use crate::{
    identity::{AgentId, JobId},
    session::EventRecord,
};

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
/// Presentation never shortens it.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, Serialize, PartialEq, Eq)]
pub(crate) struct QuestionOutput {
    #[schemars(transform = crate::tool::output::complete)]
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

pub type HostStateFuture<'a> = Pin<Box<dyn Future<Output = Option<crate::Prose>> + Send + 'a>>;

/// The host's own section of each agent's runtime state.
///
/// Rendered by the host, placed by the harness: it joins every request's state block
/// and the compaction summary's, so what it says is never lost to compaction.
pub trait HostState: Send + Sync {
    /// The section for `agent`'s next request in a session with the host's tables,
    /// `extension`; `None` adds nothing to that request. An interrupt drops the
    /// future and ends the request it was for.
    fn state<'a>(
        &'a self,
        extension: &'a crate::session::ExtensionDb,
        agent: &'a AgentId,
    ) -> HostStateFuture<'a>;
}

pub type ResumeFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(), Box<dyn std::error::Error + Send + Sync>>> + Send + 'a>>;

/// What this launch changes about the work of a session it reopens.
pub trait ResumeHook: Send + Sync {
    /// Called once a reopened session is about to take its first work in this
    /// harness: a prompt, queued prompts, a continue or a script, or
    /// [`SessionHandle::resume_work`](super::SessionHandle::resume_work). A session
    /// only viewed never reaches it. A failure refuses that work and the next work
    /// calls it again. An interrupt while it runs withdraws the work waiting on it
    /// and drops it unfinished, so the next work calls it again. Shutdown does the
    /// same, and returns only once it is dropped.
    fn resumed<'a>(&'a self, extension: &'a crate::session::ExtensionDb) -> ResumeFuture<'a>;
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
    Record(Arc<EventRecord>),
    /// A job as the record that follows leaves it.
    Job(Box<crate::job::JobEnvelope>),
    /// Jobs the session no longer holds: what last moved them is their last view,
    /// until a record moves one again, which reports it anew.
    JobsUnheld(Vec<JobId>),
    /// `request`'s response streamed after it was last read: read it with
    /// [`SessionStore::response`](crate::session::SessionStore::response). No further
    /// one is posted for it until it is read again, or a record of its agent applies.
    ResponseChanged {
        agent: AgentId,
        request: RequestSeq,
    },
    ResponseSettled {
        agent: AgentId,
        request: RequestSeq,
        settlement: super::Settlement,
    },
    /// An agent's activity moved, as the session's state reads it; none once the
    /// agent no longer runs, as a parked child does not.
    Activity {
        agent: AgentId,
        activity: Option<super::ObservedActivity>,
    },
    TurnCompleted {
        agent: AgentId,
    },
    /// Run `invocation` of `job` captured output after its output was last read.
    /// No further one is posted for that run until its output is read again or
    /// the run finishes.
    OutputChanged {
        job: JobId,
        invocation: crate::job::Invocation,
    },
    /// The phase the session moved to.
    Phase(crate::session::SessionPhase),
}
