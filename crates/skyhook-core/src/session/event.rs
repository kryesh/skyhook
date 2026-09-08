//! Durable session event schema and replay validation.

use std::{
    collections::HashSet,
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    execution::ExecutionLocation,
    identity::{AgentId, JobId, SessionId},
    job::JobState,
    media::ImageReference,
    provider::protocol::{Message, ModelRequest, Usage},
    target::TargetDefinition,
};

use super::{SESSION_FORMAT_VERSION, SessionError};

/// An exact request message, either journal-backed or request-specific.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContextMessage {
    Source { sequence: u64 },
    Inline { message: Message },
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModelPurpose {
    Agent,
    Compaction,
}

/// Durable replacement of one agent's model-visible history.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct CompactionCheckpoint {
    /// Version of the structured continuation schema used for this checkpoint.
    pub schema_version: u16,
    pub previous: Option<u64>,
    pub frontier: u64,
    pub message: Message,
    /// Reconciled owner todos, activated atomically with this history replacement.
    pub todos: Vec<crate::agent::TodoItem>,
    pub retained: Vec<u64>,
    pub request: u64,
    pub max_context: u64,
    pub before_tokens: u64,
    pub after_tokens: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ModelCallOrigin {
    pub message: u64,
    pub call_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    SessionStarted {
        targets: Vec<TargetDefinition>,
    },
    TargetsUpserted {
        targets: Vec<TargetDefinition>,
    },
    AgentStarted {
        parent: Option<AgentId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        owner_job: Option<JobId>,
        model_profile: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_context: Option<u64>,
        agent_profile: Option<String>,
        location: ExecutionLocation,
    },
    TodosReplaced {
        items: Vec<crate::agent::TodoItem>,
    },
    /// Model applied to a submitted user turn; never a pending UI selection.
    ModelChanged {
        model_profile: String,
        max_context: u64,
    },
    MessageCommitted {
        message: Message,
    },
    /// Host-facing conversation status, excluded from model-visible history.
    Status {
        message: String,
    },
    /// Shared request fields; template.messages is empty because history is already journaled.
    ModelContext {
        provider: String,
        template: ModelRequest,
    },
    /// A provider call with exact ordered messages, independent of future state/configuration.
    ModelRequested {
        context: u64,
        messages: Vec<ContextMessage>,
        purpose: ModelPurpose,
    },
    Compaction {
        checkpoint: CompactionCheckpoint,
    },
    ModelFailed {
        request: u64,
        attempt: u8,
        error: String,
    },
    CompactionSkipped {
        request: u64,
        reason: String,
    },
    CompactionFailed {
        request: Option<u64>,
        error: String,
    },
    Usage {
        request: Option<u64>,
        usage: Usage,
    },
    JobCreated {
        job: JobId,
        parent: Option<JobId>,
        origin: Option<ModelCallOrigin>,
        tool: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        arguments: Value,
        output_schema: Option<Value>,
        accepts_input: bool,
        background: bool,
        location: ExecutionLocation,
    },
    JobStateChanged {
        job: JobId,
        state: JobState,
    },
    JobFinished {
        job: JobId,
        state: JobState,
        output_path: Option<PathBuf>,
        error: Option<String>,
        images: Vec<ImageReference>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        denial: Option<crate::tool::Denial>,
    },
    JobClaimed {
        job: JobId,
    },
    JobInjected {
        job: JobId,
    },
    QuestionOpened {
        job: JobId,
        question_id: String,
        questions: Value,
    },
    QuestionResolved {
        job: JobId,
        question_id: String,
        answers: Value,
    },
    AgentCompleted,
    AgentInterrupted,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct EventRecord {
    pub version: u16,
    pub sequence: u64,
    pub timestamp_millis: i64,
    pub agent: AgentId,
    pub event: SessionEvent,
}

pub(super) fn validate_records(records: &[EventRecord], id: SessionId) -> Result<(), SessionError> {
    let mut expected = 1;
    let mut jobs = HashSet::new();
    let mut terminal = HashSet::new();
    for (index, record) in records.iter().enumerate() {
        if record.version != SESSION_FORMAT_VERSION {
            return Err(SessionError::UnsupportedVersion(record.version));
        }
        if record.sequence != expected {
            return Err(SessionError::InvalidSequence {
                expected,
                actual: record.sequence,
            });
        }
        if record.agent.session() != id {
            return Err(SessionError::WrongSession);
        }
        match &record.event {
            SessionEvent::Compaction { .. } => {
                super::request::validate_compaction(&records[..index], record)?;
            }
            SessionEvent::ModelRequested { .. } => {
                super::request::validate_request(&records[..index], record)?;
            }
            SessionEvent::JobCreated { job, .. } if !jobs.insert(*job) => {
                return Err(SessionError::DuplicateJob(*job));
            }
            SessionEvent::JobStateChanged { job, .. }
            | SessionEvent::JobClaimed { job }
            | SessionEvent::JobInjected { job }
                if !jobs.contains(job) =>
            {
                return Err(SessionError::UnknownJob(*job));
            }
            SessionEvent::JobFinished {
                job,
                state,
                output_path,
                ..
            } => {
                if !jobs.contains(job) {
                    return Err(SessionError::UnknownJob(*job));
                }
                if !state.is_terminal() || !terminal.insert(*job) {
                    return Err(SessionError::DuplicateTerminal(*job));
                }
                if output_path
                    .as_deref()
                    .is_some_and(|path| !is_safe_artifact_path(path))
                {
                    return Err(SessionError::UnsafeArtifactPath);
                }
            }
            _ => {}
        }
        expected = expected.saturating_add(1);
    }
    Ok(())
}

pub(crate) fn is_safe_artifact_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}
