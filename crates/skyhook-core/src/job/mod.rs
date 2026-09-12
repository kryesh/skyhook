//! Supervised jobs and durable saved output.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::{
    sync::{Mutex, Notify, OwnedMutexGuard, broadcast, mpsc},
    task::AbortHandle,
};
pub(crate) use tokio_util::sync::CancellationToken;

use crate::{
    execution::ExecutionLocation,
    identity::{AgentId, JobId},
    media::ImageReference,
    provider::protocol::Message,
    session::{EventRecord, SessionError, SessionEvent, SessionStore},
    tool::{
        ToolOutput,
        policy::{Capability, CapabilitySet},
    },
};

pub(crate) mod output;
pub use output::{OutputArgs as JobOutputQuery, omit_null_fields};
mod cancellation;
mod delivery;
mod input;
mod lifecycle;
mod messages;
mod views;

pub(crate) use delivery::PendingDelivery;
pub use views::JobEnvelope;
pub(crate) use views::{ActiveJob, ActiveJobLocation, presented_job_schema};
mod persistence;
mod progress;

const JOB_INPUT_CAPACITY: usize = 32;

#[derive(Clone, Copy, Debug, Deserialize, schemars::JsonSchema, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    #[schemars(skip)]
    AwaitingApproval,
    Running,
    WaitingInput,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

impl JobState {
    /// Agent-facing projection; authorization remains a host concern.
    #[must_use]
    pub const fn presented(self) -> Self {
        match self {
            Self::AwaitingApproval => Self::Queued,
            state => state,
        }
    }

    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Interrupted
        )
    }
}

pub(crate) enum JobOutcome {
    Completed(ToolOutput),
    Failed {
        message: String,
        output: Option<ToolOutput>,
        denial: Option<crate::tool::Denial>,
    },
    Cancelled,
    Interrupted,
}

impl From<crate::tool::ToolError> for JobOutcome {
    fn from(error: crate::tool::ToolError) -> Self {
        use crate::tool::{Denial, ToolError};
        let message = match &error {
            crate::tool::ToolError::Denied(reason) => reason.clone(),
            error => error.concise_message(),
        };
        let (output, denial) = match error {
            ToolError::Cancelled => return Self::Cancelled,
            ToolError::Interrupted => return Self::Interrupted,
            ToolError::Denied(_) => (None, Some(Denial::permission_denied())),
            ToolError::FailedWithOutput { output, .. } => (Some(*output), None),
            _ => (None, None),
        };
        Self::Failed {
            message,
            output,
            denial,
        }
    }
}

#[derive(Clone, Debug)]
pub struct JobCompletion {
    pub agent: AgentId,
    pub job: JobId,
}

/// Explicitly installed by live child agents; ordinary input-capable tools cannot restart.
pub(crate) type ResumeHandler = Arc<
    dyn Fn(
            Option<Value>,
            mpsc::Receiver<Value>,
        )
            -> futures_util::future::BoxFuture<'static, Result<ToolOutput, crate::tool::ToolError>>
        + Send
        + Sync,
>;

struct JobEntry {
    origin: Option<crate::session::ModelCallOrigin>,
    output_schema: Option<Value>,
    agent: AgentId,
    parent: Option<JobId>,
    tool: String,
    name: Option<String>,
    created_at_millis: i64,
    state: JobState,
    output: Option<Value>,
    images: Vec<ImageReference>,
    error: Option<String>,
    denial: Option<crate::tool::Denial>,
    accepts_input: bool,
    resume: Option<ResumeHandler>,
    input: mpsc::Sender<Value>,
    cancellation: CancellationToken,
    notify: Arc<Notify>,
    operation: Arc<Mutex<()>>,
    task_abort: Option<AbortHandle>,
    cancellation_watchdog_started: bool,
    delivery: DeliveryState,
    child: Option<AgentId>,
    messages: Vec<AgentMessage>,
    last_agent_message: Option<u64>,
    background: bool,
    authorization_scope: Option<u64>,
    location: ExecutionLocation,
}

impl JobEntry {
    fn new(spec: JobSpec, created_at_millis: i64) -> (Self, mpsc::Receiver<Value>) {
        let (input, receiver) = mpsc::channel(JOB_INPUT_CAPACITY);
        (
            Self {
                origin: spec.origin,
                output_schema: spec.output_schema,
                agent: spec.agent,
                parent: spec.parent,
                tool: spec.tool,
                name: spec.name,
                created_at_millis,
                state: JobState::Queued,
                output: None,
                images: Vec::new(),
                error: None,
                denial: None,
                accepts_input: spec.accepts_input,
                resume: None,
                input,
                cancellation: CancellationToken::new(),
                notify: Arc::new(Notify::new()),
                operation: Arc::new(Mutex::new(())),
                task_abort: None,
                cancellation_watchdog_started: false,
                delivery: DeliveryState::Pending,
                child: None,
                messages: Vec::new(),
                last_agent_message: None,
                background: spec.background,
                authorization_scope: spec.authorization_scope,
                location: spec.location,
            },
            receiver,
        )
    }

    fn suspended(&self) -> bool {
        self.state == JobState::Interrupted && self.resume.is_some()
    }

    fn deliverable(&self) -> bool {
        // Keep parent waits pending across a retryable interruption. Snapshots
        // still expose the interrupted state to the user.
        !self.suspended() && (self.state.is_terminal() || self.state == JobState::WaitingInput)
    }

    fn reserve_delivery(&mut self, delivery: DeliveryState) -> Option<AgentId> {
        if !self.deliverable() || self.delivery != DeliveryState::Pending {
            return None;
        }
        self.delivery = delivery;
        Some(self.agent.clone())
    }

    fn metadata(&self, id: JobId) -> JobEnvelope {
        JobEnvelope {
            id,
            parent: self.parent,
            tool: self.tool.clone(),
            name: self.name.clone(),
            state: self.state,
            output: None,
            error: self.error.clone(),
            location: self.location.clone(),
            denial: self.denial.clone(),
        }
    }

    fn envelope(&self, id: JobId) -> JobEnvelope {
        JobEnvelope {
            id,
            parent: self.parent,
            tool: self.tool.clone(),
            name: self.name.clone(),
            state: self.state,
            output: self.output.clone(),
            error: self.error.clone(),
            location: self.location.clone(),
            denial: self.denial.clone(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DeliveryState {
    Pending,
    Claimed,
    Injected,
}

#[derive(Clone, Copy)]
enum WaitMode {
    Foreground,
    Explicit { claim: bool },
    Terminal,
}

impl DeliveryState {
    fn event(self, job: JobId) -> SessionEvent {
        match self {
            Self::Claimed => SessionEvent::JobClaimed { job },
            Self::Injected => SessionEvent::JobInjected { job },
            Self::Pending => unreachable!("pending delivery has no event"),
        }
    }
}

struct JobManagerInner {
    store: SessionStore,
    jobs: Mutex<HashMap<JobId, JobEntry>>,
    progress: Mutex<progress::Progress>,
    delivery_operation: Arc<Mutex<()>>,
    next_id: AtomicU64,
    completions: broadcast::Sender<JobCompletion>,
}

#[derive(Clone)]
pub struct JobManager {
    inner: Arc<JobManagerInner>,
}

/// A visible child reply identified by its source MessageCommitted sequence.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct AgentMessage {
    pub id: JobId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub message: u64,
    pub text: String,
}

pub struct JobLease {
    pub id: JobId,
    pub(crate) cancellation: CancellationToken,
    pub input: mpsc::Receiver<Value>,
}

#[derive(Clone, Debug)]
pub struct JobSpec {
    pub origin: Option<crate::session::ModelCallOrigin>,
    pub output_schema: Option<Value>,
    pub agent: AgentId,
    pub parent: Option<JobId>,
    pub tool: String,
    pub name: Option<String>,
    pub arguments: Value,
    pub accepts_input: bool,
    pub background: bool,
    pub authorization_scope: Option<u64>,
    pub location: ExecutionLocation,
}

#[cfg(test)]
impl JobSpec {
    pub(crate) fn test(agent: AgentId, tool: impl Into<String>) -> Self {
        Self {
            origin: None,
            output_schema: None,
            agent,
            parent: None,
            tool: tool.into(),
            name: None,
            arguments: Value::Object(serde_json::Map::new()),
            accepts_input: false,
            background: false,
            authorization_scope: None,
            location: ExecutionLocation::root(".".into()),
        }
    }
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
                progress: Mutex::new(progress::Progress::default()),
                delivery_operation: Arc::new(Mutex::new(())),
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
    use super::*;

    pub(super) async fn runtime() -> (tempfile::TempDir, JobManager, AgentId) {
        let runtime = crate::tests::TestRuntime::new().await;
        (runtime.root, runtime.jobs, runtime.agent)
    }

    // Small fixture operations keep tests focused on the delivery/state boundary
    // under test. Error-path calls deliberately bypass these success-only helpers.
    impl JobManager {
        pub(super) async fn test_lease(&self, spec: JobSpec) -> JobLease {
            self.create(spec).await.unwrap()
        }

        pub(super) async fn test_create(&self, spec: JobSpec) -> JobId {
            self.test_lease(spec).await.id
        }

        pub(super) async fn test_finish(&self, job: JobId, output: Value) {
            self.finish(job, JobOutcome::Completed(ToolOutput::new(output)))
                .await
                .unwrap();
        }

        pub(super) async fn test_append(&self, agent: AgentId, event: SessionEvent) -> u64 {
            self.store().append(agent, event).await.unwrap().sequence
        }

        pub(super) async fn test_replay(&self) -> Self {
            Self::restore(self.store().clone(), &self.store().records().await)
                .await
                .unwrap()
        }
    }
}
