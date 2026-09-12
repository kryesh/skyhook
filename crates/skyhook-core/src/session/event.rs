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
    /// A new attempt starts for an existing frozen logical request. This resets
    /// its live output in host projections without changing model history.
    ModelAttemptStarted {
        request: u64,
        attempt: u64,
    },
    ModelFailed {
        request: u64,
        attempt: u64,
        error: String,
    },
    /// A failed model request will be retried after a recovery delay.
    /// This is host-facing status, not model-visible conversation history.
    ModelRecoveryScheduled {
        /// Sequence of the failed `ModelRequested` event.
        request: u64,
        /// The next logical invocation (not an HTTP transport retry).
        attempt: u64,
        /// Historical bounded limits remain readable; None means retry indefinitely.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_attempts: Option<u64>,
        /// Backoff scheduled before the next invocation.
        delay_millis: u64,
        /// The connection failure that triggered this recovery.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_event_round_trips_without_changing_existing_log_format() {
        let id = SessionId::from_bytes([42; 16]);
        let agent = AgentId::root(id);
        // A failure from existing version-2 logs requires no new fields.
        let failure: SessionEvent = serde_json::from_str(
            r#"{"type":"model_failed","request":7,"attempt":1,"error":"connection lost"}"#,
        )
        .unwrap();
        assert_eq!(
            failure,
            SessionEvent::ModelFailed {
                request: 7,
                attempt: 1,
                error: "connection lost".into(),
            }
        );
        let events = [
            failure,
            SessionEvent::ModelRecoveryScheduled {
                request: 7,
                attempt: 2,
                max_attempts: Some(3),
                delay_millis: 1000,
                error: "connection lost".into(),
            },
            SessionEvent::ModelRecoveryScheduled {
                request: 7,
                attempt: 300,
                max_attempts: None,
                delay_millis: 30_000,
                error: "provider HTTP 429 error [code=rate_limit_exceeded]".into(),
            },
            SessionEvent::ModelAttemptStarted {
                request: 7,
                attempt: 300,
            },
        ];
        let records: Vec<_> = events
            .into_iter()
            .enumerate()
            .map(|(index, event)| EventRecord {
                version: SESSION_FORMAT_VERSION,
                sequence: index as u64 + 1,
                timestamp_millis: 0,
                agent: agent.clone(),
                event,
            })
            .collect();
        let encoded = serde_json::to_string(&records).unwrap();
        assert!(encoded.contains("model_recovery_scheduled"));
        let decoded: Vec<EventRecord> = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, records);
        validate_records(&decoded, id).unwrap();
        // Recovery status and its error are host-only, never prompt content.
        assert!(
            crate::session::project_history(&decoded, &agent)
                .unwrap()
                .is_empty()
        );
    }
}
