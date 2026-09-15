//! Durable session event schema and replay validation.

use std::{
    collections::{HashMap, HashSet},
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    execution::ExecutionLocation,
    identity::{AgentId, EventId, JobId, QueueAttemptId, SessionId},
    job::{JobRole, JobState},
    media::ImageRef,
    provider::protocol::{HistoryLifetime, Message, ModelRequest, Usage, UserContent},
    target::TargetDefinition,
};

use super::{SESSION_FORMAT_VERSION, SessionError};

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

/// Immutable accepted submission. Session and destination agent are bound by its record.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct QueueIntent {
    pub attempt: QueueAttemptId,
    pub content: Vec<UserContent>,
    pub model: Option<String>,
}

/// Final resolution of an intent; NotCommitted explicitly abandons that attempt.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum QueueSettlement {
    Committed { event: EventId },
    NotCommitted,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    QueueIntent {
        intent: QueueIntent,
    },
    QueueSettlement {
        attempt: QueueAttemptId,
        settlement: QueueSettlement,
    },
    QueueAcknowledged {
        attempt: QueueAttemptId,
    },
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
    /// Shared request fields; template history and tail are empty because history is already journaled.
    ModelContext {
        provider: String,
        template: ModelRequest,
    },
    /// A provider call with exact ordered messages, independent of future state/configuration.
    ModelRequested {
        context: u64,
        /// Journal sequences of the committed messages or compaction checkpoints sent as history.
        history: Vec<u64>,
        /// Request-specific messages sent after history.
        tail: Vec<Message>,
        #[serde(default)]
        history_lifetime: HistoryLifetime,
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
        role: JobRole,
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
        images: Vec<ImageRef>,
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

impl SessionEvent {
    /// The attempt a queue event inherently names.
    pub(crate) fn queue_attempt(&self) -> Option<QueueAttemptId> {
        match self {
            Self::QueueIntent { intent } => Some(intent.attempt),
            Self::QueueSettlement { attempt, .. } | Self::QueueAcknowledged { attempt } => {
                Some(*attempt)
            }
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct EventRecord {
    pub id: EventId,
    pub queue_attempt: Option<QueueAttemptId>,
    pub version: u16,
    pub sequence: u64,
    pub timestamp_millis: i64,
    pub agent: AgentId,
    pub event: SessionEvent,
}

impl EventRecord {
    pub(crate) fn append_identity(&self) -> super::AppendIdentity {
        super::AppendIdentity {
            event: self.id,
            queue_attempt: self.queue_attempt,
            session: self.agent.session(),
            sequence: self.sequence,
        }
    }
}

pub(super) fn validate_records(records: &[EventRecord], id: SessionId) -> Result<(), SessionError> {
    let mut expected = 1;
    let mut event_ids = HashSet::new();
    let mut jobs = HashSet::new();
    let mut terminal = HashMap::new();
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
        if !event_ids.insert(record.id) {
            return Err(SessionError::DuplicateEvent(record.id));
        }
        super::queue::validate_queue_record(&records[..index], record)?;
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
            SessionEvent::JobStateChanged {
                job,
                state: JobState::Running,
            } => {
                // Retained jobs reuse their identity, not a terminal invocation.
                // Cancellation remains final; only supported resumable outcomes
                // reopen the per-invocation terminal-publication obligation.
                if matches!(
                    terminal.get(job),
                    Some(JobState::Completed | JobState::Failed | JobState::Interrupted)
                ) {
                    terminal.remove(job);
                }
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
                let allowed = match terminal.get(job) {
                    None => true,
                    Some(JobState::Interrupted) => *state == JobState::Cancelled,
                    Some(_) => false,
                };
                if !state.is_terminal() || !allowed {
                    return Err(SessionError::DuplicateTerminal(*job));
                }
                terminal.insert(*job, *state);
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
    fn recovery_event_round_trips_in_v3_log_format() {
        let id = SessionId::from_bytes([42; 16]);
        let agent = AgentId::root(id);
        // A failure from version-3 logs requires no new fields.
        let failure: SessionEvent = serde_json::from_str(
            r#"{"type":"model_failed","request":7,"attempt":1,"error":"connection lost"}"#,
        )
        .unwrap();
        let expected = SessionEvent::ModelFailed {
            request: 7,
            attempt: 1,
            error: "connection lost".into(),
        };
        assert_eq!(failure, expected);
        let scheduled = |attempt, max_attempts, delay_millis, error: &str| {
            SessionEvent::ModelRecoveryScheduled {
                request: 7,
                attempt,
                max_attempts,
                delay_millis,
                error: error.into(),
            }
        };
        let events = [
            failure,
            scheduled(2, Some(3), 1000, "connection lost"),
            scheduled(
                300,
                None,
                30_000,
                "provider HTTP 429 error [code=rate_limit_exceeded]",
            ),
            SessionEvent::ModelAttemptStarted {
                request: 7,
                attempt: 300,
            },
        ];
        let records: Vec<_> = events
            .into_iter()
            .enumerate()
            .map(|(index, event)| EventRecord {
                id: crate::identity::EventId::generate().unwrap(),
                queue_attempt: None,
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

    #[tokio::test]
    async fn retained_outcomes_reopen_only_after_a_running_reset_or_interrupted_cancellation() {
        let finished = |job, state| SessionEvent::JobFinished {
            job,
            state,
            output_path: None,
            error: None,
            images: Vec::new(),
            denial: None,
        };
        let states = [
            JobState::Completed,
            JobState::Failed,
            JobState::Interrupted,
            JobState::Cancelled,
        ];
        for first in states {
            for reset in [false, true] {
                for second in states {
                    let reopened_by_reset = reset && first != JobState::Cancelled;
                    // Without a reset only a duplicate cancellation is exercised.
                    if !reopened_by_reset && second != JobState::Cancelled {
                        continue;
                    }
                    let root = tempfile::tempdir().unwrap();
                    let store = crate::session::SessionStore::create(root.path())
                        .await
                        .unwrap();
                    let agent = AgentId::root(store.id());
                    let job = JobId::new(1).unwrap();
                    let created = SessionEvent::JobCreated {
                        job,
                        parent: None,
                        origin: None,
                        tool: "agent".into(),
                        role: JobRole::Agent,
                        name: None,
                        arguments: Value::Null,
                        output_schema: None,
                        accepts_input: true,
                        background: true,
                        location: ExecutionLocation::root(root.path().to_path_buf()),
                    };
                    let mut events = vec![created, finished(job, first)];
                    if reset {
                        events.push(SessionEvent::JobStateChanged {
                            job,
                            state: JobState::Running,
                        });
                    }
                    events.push(finished(job, second));
                    for event in events {
                        store.append(agent.clone(), event).await.unwrap();
                    }
                    let id = store.id();
                    store.close().await.unwrap();
                    drop(store); // Closing the writer does not release a live owner's lock.
                    // Exercise the physical archive validator, not just JobManager's
                    // independent permissive projection of an in-memory record slice.
                    let reopened = crate::session::SessionStore::open(root.path(), id).await;
                    if !(reopened_by_reset
                        || (first == JobState::Interrupted && second == JobState::Cancelled))
                    {
                        assert!(
                            matches!(reopened, Err(SessionError::DuplicateTerminal(id)) if id == job)
                        );
                        continue;
                    }
                    let (store, records) = reopened.unwrap();
                    assert_eq!(records.len(), 3 + usize::from(reset));
                    let jobs = crate::job::JobManager::restore(store.clone(), &records)
                        .await
                        .unwrap();
                    assert_eq!(jobs.metadata(job).await.unwrap().state, second);
                    drop(jobs);
                    store.close().await.unwrap();
                }
            }
        }
    }
}
