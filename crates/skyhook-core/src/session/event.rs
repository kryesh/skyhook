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
    provider::protocol::{Message, Usage},
    target::TargetDefinition,
};

use super::{SESSION_FORMAT_VERSION, SessionError};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    SessionStarted {
        targets: Vec<TargetDefinition>,
    },
    TargetUpserted {
        target: TargetDefinition,
    },
    AgentStarted {
        parent: Option<AgentId>,
        model_profile: String,
        agent_profile: Option<String>,
        location: ExecutionLocation,
    },
    MessageCommitted {
        message: Message,
    },
    Usage {
        usage: Usage,
    },
    JobCreated {
        job: JobId,
        parent: Option<JobId>,
        tool: String,
        arguments: Value,
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
    for record in records {
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
