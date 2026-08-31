//! Supervised jobs and durable progress cursors.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::{Mutex, Notify, broadcast, mpsc};

use crate::{
    identity::{AgentId, JobId},
    media::ImageReference,
    session::{EventRecord, SessionError, SessionEvent, SessionStore},
    tool::{ProgressSink, ToolOutput},
};

mod persistence;
mod progress;

const JOB_INPUT_CAPACITY: usize = 32;

#[derive(Clone, Copy, Debug, Deserialize, schemars::JsonSchema, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    AwaitingApproval,
    Running,
    WaitingInput,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

impl JobState {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Interrupted
        )
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct JobProgressRecord {
    pub sequence: u64,
    pub timestamp_millis: i64,
    pub kind: String,
    pub data: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct JobEnvelope {
    pub job_id: JobId,
    pub parent_job: Option<JobId>,
    pub tool: String,
    pub state: JobState,
    pub output: Option<Value>,
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct JobCompletion {
    pub agent: AgentId,
    pub job: JobId,
}

struct JobEntry {
    agent: AgentId,
    parent: Option<JobId>,
    tool: String,
    state: JobState,
    output: Option<Value>,
    images: Vec<ImageReference>,
    error: Option<String>,
    accepts_input: bool,
    input: mpsc::Sender<Value>,
    cancellation: Arc<AtomicBool>,
    notify: Arc<Notify>,
    next_progress: u64,
    claimed: bool,
    injected: bool,
    background: bool,
}

struct JobManagerInner {
    store: SessionStore,
    jobs: Mutex<HashMap<JobId, JobEntry>>,
    next_id: AtomicU64,
    completions: broadcast::Sender<JobCompletion>,
}

#[derive(Clone)]
pub struct JobManager {
    inner: Arc<JobManagerInner>,
}

pub struct JobLease {
    pub id: JobId,
    pub cancellation: Arc<AtomicBool>,
    pub input: mpsc::Receiver<Value>,
}

impl JobManager {
    #[must_use]
    pub fn new(store: SessionStore) -> Self {
        Self::with_jobs(store, HashMap::new(), 1)
    }

    fn with_jobs(store: SessionStore, jobs: HashMap<JobId, JobEntry>, next_id: u64) -> Self {
        let (completions, _) = broadcast::channel(256);
        Self {
            inner: Arc::new(JobManagerInner {
                store,
                jobs: Mutex::new(jobs),
                next_id: AtomicU64::new(next_id),
                completions,
            }),
        }
    }

    pub async fn restore(store: SessionStore, records: &[EventRecord]) -> Result<Self, JobError> {
        persistence::restore(store, records).await
    }

    #[must_use]
    pub fn subscribe_completions(&self) -> broadcast::Receiver<JobCompletion> {
        self.inner.completions.subscribe()
    }

    #[must_use]
    pub fn store(&self) -> &SessionStore {
        &self.inner.store
    }

    pub async fn create(
        &self,
        agent: AgentId,
        parent: Option<JobId>,
        tool: String,
        arguments: Value,
        accepts_input: bool,
        background: bool,
    ) -> Result<JobLease, JobError> {
        let raw = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let id = JobId::new(raw).map_err(|error| JobError::Internal(error.to_string()))?;
        let (input, input_rx) = mpsc::channel(JOB_INPUT_CAPACITY);
        let cancellation = Arc::new(AtomicBool::new(false));
        self.inner
            .store
            .append(
                agent.clone(),
                SessionEvent::JobCreated {
                    job: id,
                    parent,
                    tool: tool.clone(),
                    arguments,
                    accepts_input,
                    background,
                },
            )
            .await?;
        self.inner.jobs.lock().await.insert(
            id,
            JobEntry {
                agent,
                parent,
                tool,
                state: JobState::Queued,
                output: None,
                images: Vec::new(),
                error: None,
                accepts_input,
                input,
                cancellation: cancellation.clone(),
                notify: Arc::new(Notify::new()),
                next_progress: 1,
                claimed: false,
                injected: false,
                background,
            },
        );
        Ok(JobLease {
            id,
            cancellation,
            input: input_rx,
        })
    }

    pub async fn transition(&self, id: JobId, state: JobState) -> Result<(), JobError> {
        if state.is_terminal() {
            return Err(JobError::InvalidTransition);
        }
        let agent = {
            let mut jobs = self.inner.jobs.lock().await;
            let entry = jobs.get_mut(&id).ok_or(JobError::Unknown(id))?;
            let valid = matches!(
                (entry.state, state),
                (
                    JobState::Queued,
                    JobState::AwaitingApproval | JobState::Running
                ) | (JobState::AwaitingApproval, JobState::Running)
            );
            if !valid {
                return Err(JobError::InvalidTransition);
            }
            entry.state = state;
            entry.agent.clone()
        };
        self.inner
            .store
            .append(agent, SessionEvent::JobStateChanged { job: id, state })
            .await?;
        Ok(())
    }

    pub async fn finish(
        &self,
        id: JobId,
        result: Result<ToolOutput, String>,
        terminal_override: Option<JobState>,
    ) -> Result<(), JobError> {
        self.finish_inner(id, result, None, terminal_override).await
    }

    pub async fn finish_failed(
        &self,
        id: JobId,
        error: String,
        output: Option<ToolOutput>,
        terminal_override: Option<JobState>,
    ) -> Result<(), JobError> {
        self.finish_inner(id, Err(error), output, terminal_override)
            .await
    }

    async fn finish_inner(
        &self,
        id: JobId,
        result: Result<ToolOutput, String>,
        failure_output: Option<ToolOutput>,
        terminal_override: Option<JobState>,
    ) -> Result<(), JobError> {
        let (agent, state, output, images, error, notify, background) = {
            let mut jobs = self.inner.jobs.lock().await;
            let entry = jobs.get_mut(&id).ok_or(JobError::Unknown(id))?;
            if entry.state.is_terminal() {
                return Err(JobError::AlreadyTerminal(id));
            }
            let (state, output, images, error) = match result {
                Ok(output) => (
                    terminal_override.unwrap_or(JobState::Completed),
                    Some(output.value),
                    output.images,
                    None,
                ),
                Err(error) => {
                    let (output, images) = failure_output.map_or((None, Vec::new()), |output| {
                        (Some(output.value), output.images)
                    });
                    (
                        terminal_override.unwrap_or(JobState::Failed),
                        output,
                        images,
                        Some(error),
                    )
                }
            };
            if !state.is_terminal() {
                return Err(JobError::InvalidTransition);
            }
            entry.state = state;
            entry.output.clone_from(&output);
            entry.images.clone_from(&images);
            entry.error.clone_from(&error);
            (
                entry.agent.clone(),
                state,
                output,
                images,
                error,
                entry.notify.clone(),
                entry.background,
            )
        };
        let output_path = match &output {
            Some(value) => Some(self.inner.store.write_job_output(id, value).await?),
            None => None,
        };
        self.inner
            .store
            .append(
                agent.clone(),
                SessionEvent::JobFinished {
                    job: id,
                    state,
                    output_path,
                    error,
                    images,
                },
            )
            .await?;
        notify.notify_waiters();
        if background {
            let _ = self
                .inner
                .completions
                .send(JobCompletion { agent, job: id });
        }
        Ok(())
    }

    pub async fn snapshot(&self, id: JobId) -> Result<JobEnvelope, JobError> {
        let jobs = self.inner.jobs.lock().await;
        let entry = jobs.get(&id).ok_or(JobError::Unknown(id))?;
        Ok(JobEnvelope {
            job_id: id,
            parent_job: entry.parent,
            tool: entry.tool.clone(),
            state: entry.state,
            output: entry.output.clone(),
            error: entry.error.clone(),
        })
    }

    pub async fn owner(&self, id: JobId) -> Result<AgentId, JobError> {
        self.inner
            .jobs
            .lock()
            .await
            .get(&id)
            .map(|entry| entry.agent.clone())
            .ok_or(JobError::Unknown(id))
    }

    pub async fn list(&self, owner: &AgentId) -> Vec<JobEnvelope> {
        let jobs = self.inner.jobs.lock().await;
        let mut output = jobs
            .iter()
            .filter(|(_, entry)| &entry.agent == owner)
            .map(|(id, entry)| JobEnvelope {
                job_id: *id,
                parent_job: entry.parent,
                tool: entry.tool.clone(),
                state: entry.state,
                output: entry.output.clone(),
                error: entry.error.clone(),
            })
            .collect::<Vec<_>>();
        output.sort_by_key(|job| job.job_id);
        output
    }

    pub async fn has_running(&self, owner: &AgentId) -> bool {
        self.inner
            .jobs
            .lock()
            .await
            .values()
            .any(|entry| &entry.agent == owner && !entry.state.is_terminal())
    }

    pub async fn wait(
        &self,
        id: JobId,
        timeout: Option<Duration>,
        claim: bool,
    ) -> Result<JobEnvelope, JobError> {
        loop {
            let (snapshot, notify, deliverable, claimed_agent) = {
                let mut jobs = self.inner.jobs.lock().await;
                let entry = jobs.get_mut(&id).ok_or(JobError::Unknown(id))?;
                let deliverable = entry.state.is_terminal()
                    || (entry.state == JobState::WaitingInput && !entry.claimed && !entry.injected);
                let claimed_agent = if deliverable && claim && !entry.claimed && !entry.injected {
                    entry.claimed = true;
                    Some(entry.agent.clone())
                } else {
                    None
                };
                let output = if entry.state == JobState::WaitingInput && !deliverable {
                    None
                } else {
                    entry.output.clone()
                };
                (
                    JobEnvelope {
                        job_id: id,
                        parent_job: entry.parent,
                        tool: entry.tool.clone(),
                        state: entry.state,
                        output,
                        error: entry.error.clone(),
                    },
                    entry.notify.clone(),
                    deliverable,
                    claimed_agent,
                )
            };
            if deliverable {
                if let Some(agent) = claimed_agent {
                    self.inner
                        .store
                        .append(agent, SessionEvent::JobClaimed { job: id })
                        .await?;
                }
                return Ok(snapshot);
            }
            let notified = notify.notified();
            if let Some(timeout) = timeout {
                if tokio::time::timeout(timeout, notified).await.is_err() {
                    return Ok(snapshot);
                }
            } else {
                notified.await;
            }
        }
    }

    pub(crate) async fn wait_foreground(&self, id: JobId) -> Result<JobEnvelope, JobError> {
        loop {
            let (snapshot, notify, detached, claimed_agent) = {
                let mut jobs = self.inner.jobs.lock().await;
                let entry = jobs.get_mut(&id).ok_or(JobError::Unknown(id))?;
                let claimed_agent =
                    if entry.state == JobState::WaitingInput && !entry.claimed && !entry.injected {
                        entry.claimed = true;
                        Some(entry.agent.clone())
                    } else {
                        None
                    };
                (
                    JobEnvelope {
                        job_id: id,
                        parent_job: entry.parent,
                        tool: entry.tool.clone(),
                        state: entry.state,
                        output: entry.output.clone(),
                        error: entry.error.clone(),
                    },
                    entry.notify.clone(),
                    entry.background,
                    claimed_agent,
                )
            };
            if snapshot.state.is_terminal() || snapshot.state == JobState::WaitingInput {
                if let Some(agent) = claimed_agent {
                    self.inner
                        .store
                        .append(agent, SessionEvent::JobClaimed { job: id })
                        .await?;
                }
                return Ok(snapshot);
            }
            if detached {
                return Ok(snapshot);
            }
            notify.notified().await;
        }
    }

    pub async fn claim(&self, id: JobId) -> Result<(), JobError> {
        let agent = {
            let mut jobs = self.inner.jobs.lock().await;
            let entry = jobs.get_mut(&id).ok_or(JobError::Unknown(id))?;
            if !entry.state.is_terminal() && entry.state != JobState::WaitingInput {
                return Err(JobError::NotTerminal(id));
            }
            if entry.claimed || entry.injected {
                return Ok(());
            }
            entry.claimed = true;
            entry.agent.clone()
        };
        self.inner
            .store
            .append(agent, SessionEvent::JobClaimed { job: id })
            .await?;
        Ok(())
    }

    pub async fn send(&self, id: JobId, value: Value) -> Result<(), JobError> {
        let sender = {
            let jobs = self.inner.jobs.lock().await;
            let entry = jobs.get(&id).ok_or(JobError::Unknown(id))?;
            if !entry.accepts_input {
                return Err(JobError::InputUnsupported(id));
            }
            if !matches!(entry.state, JobState::Running | JobState::WaitingInput) {
                return Err(JobError::NotRunning(id));
            }
            entry.input.clone()
        };
        sender
            .send(value)
            .await
            .map_err(|_| JobError::InputClosed(id))
    }

    pub async fn cancel(&self, id: JobId) -> Result<JobEnvelope, JobError> {
        let (terminal, cancellation) = {
            let jobs = self.inner.jobs.lock().await;
            let entry = jobs.get(&id).ok_or(JobError::Unknown(id))?;
            (entry.state.is_terminal(), entry.cancellation.clone())
        };
        if !terminal {
            cancellation.store(true, Ordering::Relaxed);
        }
        self.snapshot(id).await
    }

    pub async fn cancel_all(&self, owner: &AgentId) -> usize {
        let jobs = self.inner.jobs.lock().await;
        let mut cancelled = 0;
        for entry in jobs.values() {
            if &entry.agent == owner && !entry.state.is_terminal() {
                entry.cancellation.store(true, Ordering::Relaxed);
                cancelled += 1;
            }
        }
        cancelled
    }

    pub async fn is_background(&self, id: JobId) -> Result<bool, JobError> {
        self.inner
            .jobs
            .lock()
            .await
            .get(&id)
            .map(|entry| entry.background)
            .ok_or(JobError::Unknown(id))
    }

    /// Suspend a running job until its caller supplies input. The payload is the
    /// externally visible question envelope.
    pub async fn request_input(&self, id: JobId, output: Value) -> Result<(), JobError> {
        let (agent, notify) = {
            let mut jobs = self.inner.jobs.lock().await;
            let entry = jobs.get_mut(&id).ok_or(JobError::Unknown(id))?;
            if entry.state != JobState::Running {
                return Err(JobError::InvalidTransition);
            }
            entry.state = JobState::WaitingInput;
            entry.output = Some(output);
            entry.error = None;
            entry.claimed = false;
            entry.injected = false;
            entry.background = true;
            (entry.agent.clone(), entry.notify.clone())
        };
        self.inner
            .store
            .append(
                agent.clone(),
                SessionEvent::JobStateChanged {
                    job: id,
                    state: JobState::WaitingInput,
                },
            )
            .await?;
        notify.notify_waiters();
        let _ = self
            .inner
            .completions
            .send(JobCompletion { agent, job: id });
        Ok(())
    }

    pub async fn resume_input(&self, id: JobId) -> Result<(), JobError> {
        let (agent, notify) = {
            let mut jobs = self.inner.jobs.lock().await;
            let entry = jobs.get_mut(&id).ok_or(JobError::Unknown(id))?;
            if entry.state != JobState::WaitingInput {
                return Err(JobError::InvalidTransition);
            }
            entry.state = JobState::Running;
            entry.output = None;
            entry.claimed = false;
            entry.injected = false;
            (entry.agent.clone(), entry.notify.clone())
        };
        self.inner
            .store
            .append(
                agent,
                SessionEvent::JobStateChanged {
                    job: id,
                    state: JobState::Running,
                },
            )
            .await?;
        notify.notify_waiters();
        Ok(())
    }

    /// Atomically reserves every pending notification for an agent so queued
    /// wake-up signals cannot inject an explicitly claimed result a second time.
    pub async fn take_pending(&self, owner: &AgentId) -> Result<Vec<JobEnvelope>, JobError> {
        let pending = {
            let mut jobs = self.inner.jobs.lock().await;
            let mut pending = jobs
                .iter_mut()
                .filter(|(_, entry)| {
                    &entry.agent == owner
                        && entry.background
                        && (entry.state.is_terminal() || entry.state == JobState::WaitingInput)
                        && !entry.claimed
                        && !entry.injected
                })
                .map(|(id, entry)| {
                    entry.injected = true;
                    (
                        *id,
                        entry.agent.clone(),
                        JobEnvelope {
                            job_id: *id,
                            parent_job: entry.parent,
                            tool: entry.tool.clone(),
                            state: entry.state,
                            output: entry.output.clone(),
                            error: entry.error.clone(),
                        },
                    )
                })
                .collect::<Vec<_>>();
            pending.sort_by_key(|(id, _, _)| *id);
            pending
        };
        for (job, agent, _) in &pending {
            self.inner
                .store
                .append(agent.clone(), SessionEvent::JobInjected { job: *job })
                .await?;
        }
        Ok(pending
            .into_iter()
            .map(|(_, _, envelope)| envelope)
            .collect())
    }

    pub async fn publish_progress(
        &self,
        id: JobId,
        kind: String,
        data: Value,
    ) -> Result<(), JobError> {
        progress::publish(self, id, kind, data).await
    }

    pub async fn images(&self, id: JobId) -> Result<Vec<ImageReference>, JobError> {
        self.inner
            .jobs
            .lock()
            .await
            .get(&id)
            .map(|entry| entry.images.clone())
            .ok_or(JobError::Unknown(id))
    }

    pub async fn events(
        &self,
        id: JobId,
        after: u64,
        limit: usize,
    ) -> Result<Vec<JobProgressRecord>, JobError> {
        progress::events(self, id, after, limit).await
    }

    #[must_use]
    pub fn progress_sink(&self, id: JobId) -> Arc<dyn ProgressSink> {
        progress::sink(self.clone(), id)
    }
}

#[derive(Debug, Error)]
pub enum JobError {
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error("unknown job {0}")]
    Unknown(JobId),
    #[error("job {0} already has a terminal result")]
    AlreadyTerminal(JobId),
    #[error("job {0} is not terminal")]
    NotTerminal(JobId),
    #[error("job {0} is not running")]
    NotRunning(JobId),
    #[error("job {0} does not accept input")]
    InputUnsupported(JobId),
    #[error("job {0} input channel is closed")]
    InputClosed(JobId),
    #[error("invalid job state transition")]
    InvalidTransition,
    #[error("internal job error: {0}")]
    Internal(String),
}

#[cfg(test)]
mod tests {
    use schemars::JsonSchema;
    use serde::Deserialize;

    use super::*;
    use crate::tool::{ToolOptions, ToolRegistryBuilder, executor::ToolExecutor, policy::AllowAll};

    #[derive(Deserialize, JsonSchema)]
    struct Echo {
        value: String,
    }

    #[tokio::test]
    async fn foreground_and_background_share_one_job_path() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::create(root.path()).await.unwrap();
        let agent = AgentId::root(store.id());
        let mut builder = ToolRegistryBuilder::default();
        builder
            .register::<Echo, String, _, _>(
                "echo",
                "echo",
                ToolOptions::new(Vec::new()).background(),
                |_context, input| async move { Ok(input.value) },
            )
            .unwrap();
        let jobs = JobManager::new(store);
        let executor = ToolExecutor::new(
            builder.build(),
            Arc::new(AllowAll),
            jobs.clone(),
            root.path().to_path_buf(),
        );
        let foreground = executor
            .execute(
                agent.clone(),
                "echo",
                serde_json::json!({"value":"a"}),
                None,
            )
            .await
            .unwrap();
        assert_eq!(foreground.output.value, "a");
        let background = executor
            .execute(
                agent,
                "echo",
                serde_json::json!({"value":"b", "bg":true}),
                None,
            )
            .await
            .unwrap();
        assert!(background.background);
        assert_eq!(
            jobs.wait(background.job, None, true).await.unwrap().output,
            Some(serde_json::json!("b"))
        );
    }

    #[tokio::test]
    async fn restore_interrupts_active_jobs_and_advances_ids() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::create(root.path()).await.unwrap();
        let session = store.id();
        let agent = AgentId::root(session);
        let manager = JobManager::new(store.clone());
        let lease = manager
            .create(
                agent.clone(),
                None,
                "long_task".to_owned(),
                serde_json::json!({}),
                false,
                true,
            )
            .await
            .unwrap();
        manager
            .transition(lease.id, JobState::Running)
            .await
            .unwrap();
        drop(lease);
        drop(manager);
        drop(store);

        let (store, records) = SessionStore::open(root.path(), session).await.unwrap();
        let restored = JobManager::restore(store, &records).await.unwrap();
        assert_eq!(
            restored
                .snapshot(JobId::new(1).unwrap())
                .await
                .unwrap()
                .state,
            JobState::Interrupted
        );
        let next = restored
            .create(
                agent,
                None,
                "next".to_owned(),
                serde_json::json!({}),
                false,
                false,
            )
            .await
            .unwrap();
        assert_eq!(next.id.get(), 2);
    }

    #[tokio::test]
    async fn waiting_input_is_claimed_or_injected_exactly_once() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::create(root.path()).await.unwrap();
        let agent = AgentId::root(store.id());
        let manager = JobManager::new(store);
        let lease = manager
            .create(
                agent.clone(),
                None,
                "agent".to_owned(),
                serde_json::json!({}),
                true,
                false,
            )
            .await
            .unwrap();
        manager
            .transition(lease.id, JobState::Running)
            .await
            .unwrap();
        manager
            .request_input(
                lease.id,
                serde_json::json!({"kind":"questions","question_id":"q-2"}),
            )
            .await
            .unwrap();

        let question = manager.wait(lease.id, None, true).await.unwrap();
        assert_eq!(question.state, JobState::WaitingInput);
        assert_eq!(question.output.unwrap()["question_id"], "q-2");
        assert_eq!(
            manager.take_pending(&agent).await.unwrap(),
            Vec::<JobEnvelope>::new()
        );
        let repeated = manager
            .wait(lease.id, Some(Duration::from_millis(1)), true)
            .await
            .unwrap();
        assert_eq!(repeated.state, JobState::WaitingInput);
        assert_eq!(repeated.output, None);

        manager.resume_input(lease.id).await.unwrap();
        manager
            .request_input(
                lease.id,
                serde_json::json!({"kind":"questions","question_id":"q-3"}),
            )
            .await
            .unwrap();
        let injected = manager.take_pending(&agent).await.unwrap();
        assert_eq!(injected.len(), 1);
        assert_eq!(injected[0].output.as_ref().unwrap()["question_id"], "q-3");
        assert_eq!(
            manager.take_pending(&agent).await.unwrap(),
            Vec::<JobEnvelope>::new()
        );
    }
}
