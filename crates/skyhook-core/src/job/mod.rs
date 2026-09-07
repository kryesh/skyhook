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
    sync::{Mutex, Notify, broadcast, mpsc},
    task::AbortHandle,
};
pub(crate) use tokio_util::sync::CancellationToken;

use crate::{
    execution::ExecutionLocation,
    identity::{AgentId, JobId},
    media::ImageReference,
    session::{EventRecord, SessionError, SessionEvent, SessionStore},
    tool::{
        ToolOutput,
        policy::{Capability, CapabilitySet},
    },
};

pub(crate) mod output;
pub use output::OutputArgs as JobOutputQuery;
mod persistence;

const JOB_INPUT_CAPACITY: usize = 32;
const CANCELLATION_GRACE: Duration = Duration::from_millis(250);

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

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize, PartialEq)]
pub struct JobEnvelope<L = ExecutionLocation, T = String, V = Value> {
    pub id: JobId,
    pub parent: Option<JobId>,
    pub tool: T,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<T>,
    pub state: JobState,
    pub output: Option<V>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub console_output: String,
    pub error: Option<T>,
    pub location: L,
    #[serde(flatten)]
    pub denial: Option<crate::tool::Denial>,
}

#[derive(Serialize, JsonSchema)]
struct PresentedJob<'a> {
    id: JobId,
    state: JobState,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent: Option<JobId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace: Option<&'a std::path::Path>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<&'a Value>,
    #[serde(skip_serializing_if = "String::is_empty")]
    console_output: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a str>,
    #[serde(flatten)]
    denial: Option<crate::tool::Denial>,
}

/// Minimal job information included in the model's current runtime snapshot.
#[derive(Serialize)]
pub(crate) struct ActiveJob {
    job: JobId,
    tool: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    state: JobState,
    location: ActiveJobLocation,
    age_seconds: u64,
}

#[derive(Serialize)]
struct ActiveJobLocation {
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<String>,
    workspace: std::path::PathBuf,
}

impl JobEnvelope {
    #[cfg(test)]
    pub(crate) fn presented(
        &self,
        capabilities: &CapabilitySet,
    ) -> Result<Value, serde_json::Error> {
        self.presented_for(capabilities, None, true)
    }

    pub(crate) fn presented_for(
        &self,
        capabilities: &CapabilitySet,
        viewer: Option<&ExecutionLocation>,
        detailed: bool,
    ) -> Result<Value, serde_json::Error> {
        serde_json::to_value(PresentedJob {
            id: self.id,
            state: self.state.presented(),
            parent: self.parent.filter(|_| detailed),
            tool: detailed.then_some(self.tool.as_str()),
            name: self
                .name
                .as_deref()
                .filter(|name| detailed && !name.is_empty()),
            target: capabilities
                .contains(Capability::Targets)
                .then_some(self.location.target.as_str()),
            workspace: viewer
                .is_none_or(|location| location.workspace != self.location.workspace)
                .then_some(self.location.workspace.as_path()),
            output: self.output.as_ref(),
            console_output: self.console_output.clone(),
            error: self.error.as_deref(),
            denial: self.denial.clone(),
        })
    }
}

pub(crate) fn presented_job_schema(capabilities: &CapabilitySet, many: bool) -> Value {
    let mut envelope = serde_json::to_value(schemars::schema_for!(PresentedJob<'_>))
        .expect("job schema serializes");
    if !capabilities.contains(Capability::Targets) {
        envelope["properties"]
            .as_object_mut()
            .unwrap()
            .remove("target");
    }
    let settings = schemars::generate::SchemaSettings::default().with(|settings| {
        settings.meta_schema = None;
        settings.inline_subschemas = true;
    });
    let question = serde_json::to_value(
        settings
            .into_generator()
            .into_root_schema_for::<crate::agent::QuestionOutput>(),
    )
    .expect("question schema serializes");
    envelope["allOf"] = serde_json::json!([{
        "if":{"properties":{"state":{"const":"waiting_input"},"tool":{"const":"agent"}},"required":["state","tool"]},
        "then":{"properties":{"output":question}}
    }]);
    if many {
        let definitions = envelope
            .as_object_mut()
            .unwrap()
            .remove("$defs")
            .unwrap_or_else(|| serde_json::json!({}));
        serde_json::json!({"type":"array", "items":envelope, "$defs":definitions})
    } else {
        envelope
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
    console_output: String,
    error: Option<String>,
    denial: Option<crate::tool::Denial>,
    accepts_input: bool,
    input: mpsc::Sender<Value>,
    cancellation: CancellationToken,
    notify: Arc<Notify>,
    operation: Arc<Mutex<()>>,
    task_abort: Option<AbortHandle>,
    cancellation_watchdog_started: bool,
    delivery: DeliveryState,
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
                console_output: String::new(),
                error: None,
                denial: None,
                accepts_input: spec.accepts_input,
                input,
                cancellation: CancellationToken::new(),
                notify: Arc::new(Notify::new()),
                operation: Arc::new(Mutex::new(())),
                task_abort: None,
                cancellation_watchdog_started: false,
                delivery: DeliveryState::Pending,
                background: spec.background,
                authorization_scope: spec.authorization_scope,
                location: spec.location,
            },
            receiver,
        )
    }

    fn deliverable(&self) -> bool {
        self.state.is_terminal() || self.state == JobState::WaitingInput
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
            console_output: String::new(),
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
            console_output: self.console_output.clone(),
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
    next_id: AtomicU64,
    completions: broadcast::Sender<JobCompletion>,
}

#[derive(Clone)]
pub struct JobManager {
    inner: Arc<JobManagerInner>,
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

    pub async fn create(&self, mut spec: JobSpec) -> Result<JobLease, JobError> {
        let raw = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let id = JobId::new(raw).map_err(|error| JobError::Internal(error.to_string()))?;
        let created = self
            .inner
            .store
            .append(
                spec.agent.clone(),
                SessionEvent::JobCreated {
                    origin: spec.origin.clone(),
                    job: id,
                    parent: spec.parent,
                    tool: spec.tool.clone(),
                    name: spec.name.clone(),
                    arguments: std::mem::take(&mut spec.arguments),
                    output_schema: spec.output_schema.clone(),
                    accepts_input: spec.accepts_input,
                    background: spec.background,
                    location: spec.location.clone(),
                },
            )
            .await?;
        let (mut entry, input) = JobEntry::new(spec, created.timestamp_millis);
        let cancellation = {
            let mut jobs = self.inner.jobs.lock().await;
            if let Some(parent) = entry.parent {
                entry.cancellation = jobs
                    .get(&parent)
                    .ok_or(JobError::Unknown(parent))?
                    .cancellation
                    .child_token();
            }
            let cancellation = entry.cancellation.clone();
            jobs.insert(id, entry);
            cancellation
        };
        if cancellation.is_cancelled() {
            self.cancel(id).await?;
        }
        Ok(JobLease {
            id,
            cancellation,
            input,
        })
    }

    /// Inspect launch provenance without claiming output or changing delivery state.
    pub(crate) async fn active_launches(
        &self,
        agent: &AgentId,
    ) -> Vec<(JobId, Option<crate::session::ModelCallOrigin>)> {
        let jobs = self.inner.jobs.lock().await;
        let mut launches = Vec::new();
        for (id, entry) in jobs
            .iter()
            .filter(|(_, entry)| &entry.agent == agent && !entry.state.is_terminal())
        {
            let mut current = entry;
            let origin = loop {
                if &current.agent != agent {
                    break None;
                }
                if let Some(origin) = &current.origin {
                    break Some(origin.clone());
                }
                let Some(parent) = current.parent.and_then(|id| jobs.get(&id)) else {
                    break None;
                };
                current = parent;
            };
            launches.push((*id, origin));
        }
        launches.sort_by_key(|(id, _)| *id);
        launches
    }

    #[cfg(test)]
    pub(crate) async fn active_origins(
        &self,
        agent: &AgentId,
    ) -> Vec<crate::session::ModelCallOrigin> {
        let mut origins = Vec::new();
        for (_, origin) in self.active_launches(agent).await {
            if let Some(origin) = origin
                && !origins.contains(&origin)
            {
                origins.push(origin);
            }
        }
        origins
    }

    pub async fn transition(&self, id: JobId, state: JobState) -> Result<(), JobError> {
        if state.is_terminal() {
            return Err(JobError::InvalidTransition);
        }
        let operation = self.operation(id).await?;
        let _operation = operation.lock().await;
        let agent = {
            let jobs = self.inner.jobs.lock().await;
            let entry = jobs.get(&id).ok_or(JobError::Unknown(id))?;
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
            entry.agent.clone()
        };
        self.inner
            .store
            .append(agent, SessionEvent::JobStateChanged { job: id, state })
            .await?;
        let notify = {
            let mut jobs = self.inner.jobs.lock().await;
            let entry = jobs.get_mut(&id).ok_or(JobError::Unknown(id))?;
            entry.state = state;
            entry.notify.clone()
        };
        notify.notify_waiters();
        Ok(())
    }

    pub(crate) async fn finish(&self, id: JobId, outcome: JobOutcome) -> Result<(), JobError> {
        let operation = self.operation(id).await?;
        let _operation = operation.lock().await;
        let agent = {
            let jobs = self.inner.jobs.lock().await;
            let entry = jobs.get(&id).ok_or(JobError::Unknown(id))?;
            if entry.state.is_terminal() {
                return Err(JobError::AlreadyTerminal(id));
            }
            entry.agent.clone()
        };
        let (state, output, error, denial) = match outcome {
            JobOutcome::Completed(output) => (JobState::Completed, Some(output), None, None),
            JobOutcome::Failed {
                message,
                output,
                denial,
            } => (JobState::Failed, output, Some(message), denial),
            JobOutcome::Cancelled => (
                JobState::Cancelled,
                None,
                Some("tool was cancelled".to_owned()),
                None,
            ),
            JobOutcome::Interrupted => (
                JobState::Interrupted,
                None,
                Some("interrupted while the session was not running".to_owned()),
                None,
            ),
        };
        let (mut output, images, console_output) = output
            .map_or((None, Vec::new(), String::new()), |output| {
                (Some(output.value), output.images, output.console_output)
            });
        let directory = self.output_directory(id);
        let capture_complete = output
            .as_ref()
            .is_some_and(|value| value.get("timed_out") != Some(&Value::Bool(true)));
        if output.is_none() {
            let mut partial = serde_json::Map::new();
            for field in ["stdout", "stderr"] {
                if output::field_file(&directory, &format!("/result/{field}")).exists() {
                    partial.insert(field.to_owned(), Value::String(String::new()));
                }
            }
            if !partial.is_empty() {
                output = Some(Value::Object(partial));
            }
        }
        let document = serde_json::json!({"capture_complete":capture_complete, "result":output, "console":console_output, "error":error});
        tokio::task::spawn_blocking(move || output::save(&directory, &document))
            .await
            .map_err(|e| JobError::Internal(e.to_string()))?
            .map_err(SessionError::from)?;
        let output_path = Some(
            std::path::PathBuf::from("jobs")
                .join(id.to_string())
                .join("document.json"),
        );
        self.inner
            .store
            .append(
                agent.clone(),
                SessionEvent::JobFinished {
                    job: id,
                    state,
                    output_path,
                    error: error.clone(),
                    images: images.clone(),
                    console_output: String::new(),
                    denial: denial.clone(),
                },
            )
            .await?;
        let (notify, background) = {
            let mut jobs = self.inner.jobs.lock().await;
            let entry = jobs.get_mut(&id).ok_or(JobError::Unknown(id))?;
            if entry.state.is_terminal() {
                return Err(JobError::AlreadyTerminal(id));
            }
            entry.state = state;
            entry.output = None;
            entry.images = images;
            entry.console_output.clear();
            entry.error = error;
            entry.denial = denial;
            (entry.notify.clone(), entry.background)
        };
        notify.notify_waiters();
        if background {
            let _ = self
                .inner
                .completions
                .send(JobCompletion { agent, job: id });
        }
        Ok(())
    }

    async fn operation(&self, id: JobId) -> Result<Arc<Mutex<()>>, JobError> {
        self.inner
            .jobs
            .lock()
            .await
            .get(&id)
            .map(|entry| entry.operation.clone())
            .ok_or(JobError::Unknown(id))
    }

    pub(crate) async fn attach_task(
        &self,
        id: JobId,
        task_abort: AbortHandle,
    ) -> Result<(), JobError> {
        let mut jobs = self.inner.jobs.lock().await;
        let entry = jobs.get_mut(&id).ok_or(JobError::Unknown(id))?;
        if entry.state.is_terminal() {
            task_abort.abort();
            return Err(JobError::AlreadyTerminal(id));
        }
        entry.task_abort = Some(task_abort);
        Ok(())
    }

    /// Preserve a live, observable terminal result when durable finalization is unavailable.
    pub(crate) async fn fail_volatile(&self, id: JobId, error: String) {
        let terminal = {
            let mut jobs = self.inner.jobs.lock().await;
            let Some(entry) = jobs.get_mut(&id) else {
                return;
            };
            if entry.state.is_terminal() {
                return;
            }
            entry.state = JobState::Failed;
            entry.output = None;
            entry.images.clear();
            entry.console_output.clear();
            entry.error = Some(error);
            Some((entry.agent.clone(), entry.notify.clone(), entry.background))
        };
        if let Some((agent, notify, background)) = terminal {
            notify.notify_waiters();
            if background {
                let _ = self
                    .inner
                    .completions
                    .send(JobCompletion { agent, job: id });
            }
        }
    }

    pub(crate) async fn metadata(&self, id: JobId) -> Result<JobEnvelope, JobError> {
        let jobs = self.inner.jobs.lock().await;
        Ok(jobs.get(&id).ok_or(JobError::Unknown(id))?.metadata(id))
    }

    pub async fn snapshot(&self, id: JobId) -> Result<JobEnvelope, JobError> {
        let mut envelope = {
            let jobs = self.inner.jobs.lock().await;
            jobs.get(&id).ok_or(JobError::Unknown(id))?.envelope(id)
        };
        self.hydrate_envelope(&mut envelope).await?;
        Ok(envelope)
    }

    pub(crate) async fn cancellation_token(
        &self,
        id: JobId,
    ) -> Result<CancellationToken, JobError> {
        self.inner
            .jobs
            .lock()
            .await
            .get(&id)
            .map(|entry| entry.cancellation.clone())
            .ok_or(JobError::Unknown(id))
    }

    pub(crate) async fn authorization_scope(&self, id: JobId) -> Result<Option<u64>, JobError> {
        self.inner
            .jobs
            .lock()
            .await
            .get(&id)
            .map(|entry| entry.authorization_scope)
            .ok_or(JobError::Unknown(id))
    }

    pub async fn list(&self, owner: &AgentId) -> Vec<JobEnvelope> {
        let jobs = self.inner.jobs.lock().await;
        let mut output = jobs
            .iter()
            .filter(|(_, entry)| &entry.agent == owner)
            .map(|(id, entry)| entry.metadata(*id))
            .collect::<Vec<_>>();
        output.sort_by_key(|job| job.id);
        output
    }

    /// Lightweight current state for request-time context; never clones job output artifacts.
    pub(crate) async fn active_states(
        &self,
        owner: &AgentId,
        capabilities: &CapabilitySet,
        now_millis: i64,
    ) -> Vec<ActiveJob> {
        let jobs = self.inner.jobs.lock().await;
        let mut states = jobs
            .iter()
            .filter(|(_, entry)| &entry.agent == owner && !entry.state.is_terminal())
            .map(|(id, entry)| ActiveJob {
                job: *id,
                tool: entry.tool.clone(),
                name: entry.name.clone(),
                state: entry.state.presented(),
                location: ActiveJobLocation {
                    target: capabilities
                        .contains(Capability::Targets)
                        .then(|| entry.location.target.clone()),
                    workspace: entry.location.workspace.clone(),
                },
                age_seconds: u64::try_from(now_millis.saturating_sub(entry.created_at_millis))
                    .unwrap_or(0)
                    / 1_000,
            })
            .collect::<Vec<_>>();
        states.sort_by_key(|job| job.job);
        states
    }

    /// Associate an agent job with the child's actual workspace and target.
    /// The corresponding AgentStarted record persists this association for replay.
    pub(crate) async fn set_agent_location(
        &self,
        job: JobId,
        location: ExecutionLocation,
    ) -> Result<(), JobError> {
        let mut jobs = self.inner.jobs.lock().await;
        jobs.get_mut(&job).ok_or(JobError::Unknown(job))?.location = location;
        Ok(())
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
        let mut envelope = self
            .wait_inner(id, timeout, WaitMode::Explicit { claim })
            .await?;
        self.hydrate_envelope(&mut envelope).await?;
        Ok(envelope)
    }

    pub(crate) async fn wait_foreground(&self, id: JobId) -> Result<JobEnvelope, JobError> {
        self.wait_inner(id, None, WaitMode::Foreground).await
    }

    async fn wait_inner(
        &self,
        id: JobId,
        timeout: Option<Duration>,
        mode: WaitMode,
    ) -> Result<JobEnvelope, JobError> {
        let deadline = timeout.map(|duration| tokio::time::Instant::now() + duration);
        loop {
            let (snapshot, notified, ready, claimed_agent) = {
                let mut jobs = self.inner.jobs.lock().await;
                let entry = jobs.get_mut(&id).ok_or(JobError::Unknown(id))?;
                let notified = entry.notify.clone().notified_owned();
                let pending_question = entry.state == JobState::WaitingInput
                    && entry.delivery == DeliveryState::Pending;
                let (ready, claim) = match mode {
                    WaitMode::Foreground => (
                        entry.deliverable() || entry.background,
                        entry.state == JobState::WaitingInput,
                    ),
                    WaitMode::Explicit { claim } => {
                        (entry.state.is_terminal() || pending_question, claim)
                    }
                };
                let claimed_agent = if ready && claim {
                    entry.reserve_delivery(DeliveryState::Claimed)
                } else {
                    None
                };
                let mut snapshot = entry.envelope(id);
                if entry.state == JobState::WaitingInput && !ready {
                    snapshot.output = None;
                }
                (snapshot, notified, ready, claimed_agent)
            };
            if ready {
                self.persist_delivery(id, claimed_agent, DeliveryState::Claimed)
                    .await?;
                return Ok(snapshot);
            }
            if let Some(deadline) = deadline {
                if tokio::time::timeout_at(deadline, notified).await.is_err() {
                    return Ok(snapshot);
                }
            } else {
                notified.await;
            }
        }
    }

    async fn persist_delivery(
        &self,
        id: JobId,
        agent: Option<AgentId>,
        delivery: DeliveryState,
    ) -> Result<(), JobError> {
        if let Some(agent) = agent {
            self.inner.store.append(agent, delivery.event(id)).await?;
        }
        Ok(())
    }

    pub async fn claim(&self, id: JobId) -> Result<(), JobError> {
        let agent = {
            let mut jobs = self.inner.jobs.lock().await;
            let entry = jobs.get_mut(&id).ok_or(JobError::Unknown(id))?;
            if !entry.deliverable() {
                return Err(JobError::NotTerminal(id));
            }
            entry.reserve_delivery(DeliveryState::Claimed)
        };
        self.persist_delivery(id, agent, DeliveryState::Claimed)
            .await
    }

    pub(crate) async fn prune_claimed(&self) -> Result<usize, JobError> {
        let removed = {
            let mut jobs = self.inner.jobs.lock().await;
            let removed = jobs
                .iter()
                .filter_map(|(id, entry)| {
                    (entry.state.is_terminal() && entry.delivery == DeliveryState::Claimed)
                        .then_some(*id)
                })
                .collect::<Vec<_>>();
            for id in &removed {
                jobs.remove(id);
            }
            removed
        };
        for id in &removed {
            self.inner.store.remove_job_artifacts(*id).await?;
        }
        Ok(removed.len())
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
        let watchdogs = {
            let mut jobs = self.inner.jobs.lock().await;
            if !jobs.contains_key(&id) {
                return Err(JobError::Unknown(id));
            }
            let mut descendants = std::collections::HashSet::from([id]);
            loop {
                let before = descendants.len();
                for (child, entry) in jobs.iter() {
                    if entry
                        .parent
                        .is_some_and(|parent| descendants.contains(&parent))
                    {
                        descendants.insert(*child);
                    }
                }
                if descendants.len() == before {
                    break;
                }
            }
            let mut watchdogs = Vec::new();
            for job in descendants {
                let entry = jobs.get_mut(&job).expect("known descendant");
                entry.cancellation.cancel();
                if !entry.state.is_terminal() && !entry.cancellation_watchdog_started {
                    entry.cancellation_watchdog_started = true;
                    watchdogs.push(job);
                }
            }
            watchdogs
        };
        for job in watchdogs {
            let jobs = self.clone();
            tokio::spawn(async move {
                tokio::time::sleep(CANCELLATION_GRACE).await;
                jobs.force_cancel(job).await;
            });
        }
        let jobs = self.inner.jobs.lock().await;
        Ok(jobs.get(&id).ok_or(JobError::Unknown(id))?.metadata(id))
    }

    pub async fn cancel_all(&self, owner: &AgentId) -> usize {
        let ids = self
            .inner
            .jobs
            .lock()
            .await
            .iter()
            .filter_map(|(id, entry)| {
                (&entry.agent == owner && !entry.state.is_terminal()).then_some(*id)
            })
            .collect::<Vec<_>>();
        for id in &ids {
            let _ = self.cancel(*id).await;
        }
        ids.len()
    }

    async fn force_cancel(&self, id: JobId) {
        let task_abort = {
            let jobs = self.inner.jobs.lock().await;
            let Some(entry) = jobs.get(&id) else {
                return;
            };
            if entry.state.is_terminal() {
                return;
            }
            entry.task_abort.clone()
        };
        if let Some(task_abort) = task_abort {
            task_abort.abort();
        }
        if let Err(error) = self.finish(id, JobOutcome::Cancelled).await
            && !matches!(error, JobError::AlreadyTerminal(_))
        {
            self.fail_volatile(
                id,
                format!("job cancellation could not be persisted: {error}"),
            )
            .await;
        }
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
        let operation = self.operation(id).await?;
        let _operation = operation.lock().await;
        let agent = {
            let jobs = self.inner.jobs.lock().await;
            let entry = jobs.get(&id).ok_or(JobError::Unknown(id))?;
            if entry.state != JobState::Running {
                return Err(JobError::InvalidTransition);
            }
            entry.agent.clone()
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
        let notify = {
            let mut jobs = self.inner.jobs.lock().await;
            let entry = jobs.get_mut(&id).ok_or(JobError::Unknown(id))?;
            entry.state = JobState::WaitingInput;
            entry.output = Some(output);
            entry.error = None;
            entry.delivery = DeliveryState::Pending;
            entry.background = true;
            entry.notify.clone()
        };
        notify.notify_waiters();
        let _ = self
            .inner
            .completions
            .send(JobCompletion { agent, job: id });
        Ok(())
    }

    pub async fn resume_input(&self, id: JobId) -> Result<(), JobError> {
        let operation = self.operation(id).await?;
        let _operation = operation.lock().await;
        let agent = {
            let jobs = self.inner.jobs.lock().await;
            let entry = jobs.get(&id).ok_or(JobError::Unknown(id))?;
            if entry.state != JobState::WaitingInput {
                return Err(JobError::InvalidTransition);
            }
            entry.agent.clone()
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
        let notify = {
            let mut jobs = self.inner.jobs.lock().await;
            let entry = jobs.get_mut(&id).ok_or(JobError::Unknown(id))?;
            entry.state = JobState::Running;
            entry.output = None;
            entry.delivery = DeliveryState::Pending;
            entry.notify.clone()
        };
        notify.notify_waiters();
        Ok(())
    }

    /// Atomically reserves every pending notification for an agent so queued
    /// wake-up signals cannot inject an explicitly claimed result a second time.
    pub async fn take_pending(&self, owner: &AgentId) -> Result<Vec<JobEnvelope>, JobError> {
        let pending = {
            let mut jobs = self.inner.jobs.lock().await;
            let mut ids = jobs
                .iter()
                .filter(|(_, entry)| {
                    &entry.agent == owner
                        && entry.background
                        && entry.deliverable()
                        && entry.delivery == DeliveryState::Pending
                })
                .map(|(id, _)| *id)
                .collect::<Vec<_>>();
            ids.sort();
            let mut pending = Vec::new();
            let mut budget = 0;
            for id in ids {
                let directory = self.output_directory(id);
                let entry = jobs.get(&id).expect("selected job");
                let metadata =
                    serde_json::to_vec(&entry.metadata(id)).map_or(8192, |bytes| bytes.len());
                let estimate = output::presentation_size(&directory);
                let cost =
                    if entry.state == JobState::Completed && estimate <= output::CONTENT_BYTES {
                        estimate
                            .saturating_add(metadata)
                            .saturating_add(128)
                            .min(8192)
                    } else {
                        8192
                    };
                if !pending.is_empty() && budget + cost > 8192 {
                    break;
                }
                let entry = jobs.get_mut(&id).expect("selected job");
                if let Some(agent) = entry.reserve_delivery(DeliveryState::Injected) {
                    pending.push((id, agent, entry.envelope(id)));
                    budget += cost;
                }
            }
            pending
        };
        for (job, agent, _) in &pending {
            self.persist_delivery(*job, Some(agent.clone()), DeliveryState::Injected)
                .await?;
        }
        Ok(pending
            .into_iter()
            .map(|(_, _, envelope)| envelope)
            .collect())
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
    use crate::test_support::TestRuntime;
    use std::sync::atomic::AtomicBool;

    use schemars::JsonSchema;
    use serde::Deserialize;
    use tokio::sync::Semaphore;

    use super::*;
    use crate::tool::{
        ToolError, ToolOptions, ToolOutput, ToolRegistryBuilder,
        executor::{ExecutionError, ToolExecutor},
        policy::{AllowAll, AuthorizationRequest, Policy, PolicyFuture},
    };

    #[derive(Deserialize, JsonSchema)]
    struct Echo {
        value: String,
    }

    #[derive(Deserialize, JsonSchema)]
    struct NoArgs {}

    struct NeverAuthorize;

    impl Policy for NeverAuthorize {
        fn authorize(&self, _request: AuthorizationRequest) -> PolicyFuture<'_> {
            Box::pin(std::future::pending())
        }
    }

    #[tokio::test]
    async fn active_origins_keep_completed_launchers_and_do_not_consume_outputs() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::create(root.path()).await.unwrap();
        let agent = AgentId::root(store.id());
        let jobs = JobManager::new(store.clone());
        let origin = crate::session::ModelCallOrigin {
            message: 1,
            call_id: "script-call".into(),
        };
        let parent = jobs
            .create(JobSpec {
                origin: Some(origin.clone()),
                background: true,
                ..JobSpec::test(agent.clone(), "script")
            })
            .await
            .unwrap();
        let child = jobs
            .create(JobSpec {
                parent: Some(parent.id),
                ..JobSpec::test(agent.clone(), "shell")
            })
            .await
            .unwrap();
        let grandchild = jobs
            .create(JobSpec {
                parent: Some(child.id),
                ..JobSpec::test(agent.clone(), "nested")
            })
            .await
            .unwrap();
        jobs.finish(parent.id, JobOutcome::Completed(ToolOutput::default()))
            .await
            .unwrap();
        let before = store.records().await;
        assert_eq!(jobs.active_origins(&agent).await, vec![origin.clone()]);
        assert_eq!(jobs.active_origins(&agent).await, vec![origin.clone()]);
        assert_eq!(store.records().await, before);
        assert!(
            jobs.inner
                .jobs
                .lock()
                .await
                .get(&parent.id)
                .unwrap()
                .delivery
                == DeliveryState::Pending
        );
        assert_eq!(
            jobs.take_pending(&agent)
                .await
                .unwrap()
                .iter()
                .map(|job| job.id)
                .collect::<Vec<_>>(),
            vec![parent.id]
        );
        assert!(jobs.take_pending(&agent).await.unwrap().is_empty());
        jobs.finish(child.id, JobOutcome::Completed(ToolOutput::default()))
            .await
            .unwrap();
        assert_eq!(jobs.active_origins(&agent).await, vec![origin]);
        jobs.finish(grandchild.id, JobOutcome::Completed(ToolOutput::default()))
            .await
            .unwrap();
        assert!(jobs.active_origins(&agent).await.is_empty());
    }

    #[tokio::test]
    async fn active_origins_stop_at_agent_boundaries_and_prefer_the_nearest_call() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::create(root.path()).await.unwrap();
        let agent = AgentId::root(store.id());
        let child_agent = agent.child(1);
        let jobs = JobManager::new(store);
        let root_origin = crate::session::ModelCallOrigin {
            message: 1,
            call_id: "delegate".into(),
        };
        let child_origin = crate::session::ModelCallOrigin {
            message: 2,
            call_id: "child-script".into(),
        };
        let owner = jobs
            .create(JobSpec {
                origin: Some(root_origin.clone()),
                ..JobSpec::test(agent.clone(), "agent")
            })
            .await
            .unwrap();
        jobs.create(JobSpec {
            parent: Some(owner.id),
            ..JobSpec::test(child_agent.clone(), "host-started")
        })
        .await
        .unwrap();
        assert!(jobs.active_origins(&child_agent).await.is_empty());
        let child = jobs
            .create(JobSpec {
                parent: Some(owner.id),
                origin: Some(child_origin.clone()),
                ..JobSpec::test(child_agent.clone(), "script")
            })
            .await
            .unwrap();
        jobs.create(JobSpec {
            parent: Some(child.id),
            ..JobSpec::test(child_agent.clone(), "shell")
        })
        .await
        .unwrap();
        assert_eq!(jobs.active_origins(&agent).await, vec![root_origin]);
        assert_eq!(jobs.active_origins(&child_agent).await, vec![child_origin]);
    }

    #[tokio::test]
    async fn restored_launch_provenance_survives_interruption_and_new_descendants() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::create(root.path()).await.unwrap();
        let agent = AgentId::root(store.id());
        let jobs = JobManager::new(store.clone());
        let origin = crate::session::ModelCallOrigin {
            message: 1,
            call_id: "restored-script".into(),
        };
        let parent = jobs
            .create(JobSpec {
                origin: Some(origin.clone()),
                ..JobSpec::test(agent.clone(), "script")
            })
            .await
            .unwrap();
        let child = jobs
            .create(JobSpec {
                parent: Some(parent.id),
                ..JobSpec::test(agent.clone(), "shell")
            })
            .await
            .unwrap();
        jobs.finish(parent.id, JobOutcome::Completed(ToolOutput::default()))
            .await
            .unwrap();
        store.close().await.unwrap();
        let (store, records) = SessionStore::open(root.path(), store.id()).await.unwrap();
        let restored = JobManager::restore(store.clone(), &records).await.unwrap();
        assert_eq!(
            restored.snapshot(child.id).await.unwrap().state,
            JobState::Interrupted
        );
        assert!(restored.active_origins(&agent).await.is_empty());
        restored
            .create(JobSpec {
                parent: Some(child.id),
                ..JobSpec::test(agent.clone(), "new-descendant")
            })
            .await
            .unwrap();
        assert_eq!(restored.active_origins(&agent).await, vec![origin]);
    }

    #[tokio::test(start_paused = true)]
    async fn wait_deadline_does_not_restart_on_notifications() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::create(root.path()).await.unwrap();
        let agent = AgentId::root(store.id());
        let jobs = JobManager::new(store);
        let lease = jobs.create(JobSpec::test(agent, "pending")).await.unwrap();
        let notify = jobs
            .inner
            .jobs
            .lock()
            .await
            .get(&lease.id)
            .unwrap()
            .notify
            .clone();
        let waiter = {
            let jobs = jobs.clone();
            tokio::spawn(async move {
                jobs.wait(lease.id, Some(Duration::from_secs(10)), true)
                    .await
                    .unwrap()
            })
        };
        tokio::task::yield_now().await;
        for _ in 0..4 {
            tokio::time::advance(Duration::from_secs(2)).await;
            notify.notify_waiters();
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;
        assert!(waiter.is_finished());
        assert_eq!(waiter.await.unwrap().state, JobState::Queued);
        assert!(!lease.cancellation.is_cancelled());
    }

    #[tokio::test]
    async fn cancelling_completed_parent_cancels_descendants_and_late_starts() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::create(root.path()).await.unwrap();
        let agent = AgentId::root(store.id());
        let jobs = JobManager::new(store);
        let parent = jobs
            .create(JobSpec::test(agent.clone(), "script"))
            .await
            .unwrap();
        let child = jobs
            .create(JobSpec {
                parent: Some(parent.id),
                ..JobSpec::test(agent.child(1), "agent")
            })
            .await
            .unwrap();
        let grandchild = jobs
            .create(JobSpec {
                parent: Some(child.id),
                ..JobSpec::test(agent.child(1), "shell")
            })
            .await
            .unwrap();
        jobs.finish(parent.id, JobOutcome::Completed(ToolOutput::default()))
            .await
            .unwrap();
        jobs.cancel(parent.id).await.unwrap();
        assert!(child.cancellation.is_cancelled());
        assert!(grandchild.cancellation.is_cancelled());
        let late = jobs
            .create(JobSpec {
                parent: Some(grandchild.id),
                ..JobSpec::test(agent, "late")
            })
            .await
            .unwrap();
        assert!(late.cancellation.is_cancelled());
        let terminal = jobs
            .wait(late.id, Some(Duration::from_secs(2)), true)
            .await
            .unwrap();
        assert_eq!(terminal.state, JobState::Cancelled);
    }

    #[tokio::test]
    async fn denial_survives_replay_and_agent_views_hide_pending_authorization() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::create(root.path()).await.unwrap();
        let agent = AgentId::root(store.id());
        let jobs = JobManager::new(store.clone());
        let job = jobs.create(JobSpec::test(agent, "shell")).await.unwrap().id;
        jobs.transition(job, JobState::AwaitingApproval)
            .await
            .unwrap();
        let pending = jobs.snapshot(job).await.unwrap();
        assert_eq!(
            pending.presented(&CapabilitySet::default()).unwrap()["state"],
            "queued"
        );
        assert!(
            !presented_job_schema(&CapabilitySet::default(), false)
                .to_string()
                .contains("awaiting_approval")
        );
        jobs.finish(job, ToolError::Denied("user reason".to_owned()).into())
            .await
            .unwrap();
        let denied = jobs
            .wait(job, None, true)
            .await
            .unwrap()
            .presented(&CapabilitySet::default())
            .unwrap();
        assert_eq!(denied["code"], "permission_denied");
        assert_eq!(denied["executed"], false);
        assert_eq!(denied["error"], "user reason");
        // The persisted terminal event is the source of truth for replay.
        let session_id = store.id();
        drop(jobs);
        store.close().await.unwrap();
        drop(store);
        let (store, records) = SessionStore::open(root.path(), session_id).await.unwrap();
        let restored = JobManager::restore(store, &records).await.unwrap();
        assert_eq!(
            restored
                .snapshot(job)
                .await
                .unwrap()
                .presented(&CapabilitySet::default())
                .unwrap(),
            denied
        );
    }

    #[test]
    fn presentation_hides_only_location_target_metadata() {
        let envelope = JobEnvelope {
            id: JobId::new(1).unwrap(),
            parent: None,
            tool: "adapter".to_owned(),
            name: None,
            state: JobState::Completed,
            output: Some(serde_json::json!({"target": "application-value"})),
            console_output: String::new(),
            error: None,
            location: ExecutionLocation::named("build", "/srv/project".into()),
            denial: None,
        };
        let presented = envelope.presented(&CapabilitySet::default()).unwrap();

        assert!(presented.get("target").is_none());
        assert!(presented.get("location").is_none());
        assert_eq!(presented["workspace"], "/srv/project");
        assert_eq!(presented["output"]["target"], "application-value");
        let mut capabilities = CapabilitySet::default();
        capabilities.insert(Capability::Targets);
        let direct = envelope
            .presented_for(&capabilities, Some(&envelope.location), false)
            .unwrap();
        assert_eq!(
            direct,
            serde_json::json!({"id":1,"state":"completed","target":"build","output":{"target":"application-value"}})
        );
        let other_workspace = ExecutionLocation::named("build", "/elsewhere".into());
        assert_eq!(
            envelope
                .presented_for(&capabilities, Some(&other_workspace), false)
                .unwrap()["workspace"],
            "/srv/project"
        );
        let old = serde_json::to_value(&envelope).unwrap();
        assert!(
            serde_json::to_vec(&direct).unwrap().len() < serde_json::to_vec(&old).unwrap().len()
        );

        assert!(
            !presented_job_schema(&CapabilitySet::default(), false)
                .to_string()
                .contains("target")
        );
    }

    #[tokio::test]
    async fn registered_calls_preserve_names_and_partial_failures() {
        let runtime = TestRuntime::new().await;
        let agent = runtime.agent.clone();
        let mut builder = ToolRegistryBuilder::default();
        builder
            .register::<Echo, String, _, _>(
                "echo",
                "echo",
                ToolOptions::new(Vec::new()).background().named(),
                |_context, input| async move { Ok(input.value) },
            )
            .unwrap();
        let jobs = runtime.jobs.clone();
        let executor = runtime.executor(builder);
        let foreground = executor
            .execute(
                agent.clone(),
                "echo",
                serde_json::json!({"value":"a", "name":"foreground-echo"}),
                None,
            )
            .await
            .unwrap();
        assert_eq!(foreground.output.value, "a");
        assert_eq!(
            jobs.snapshot(foreground.job).await.unwrap().name.as_deref(),
            Some("foreground-echo")
        );
        assert_eq!(
            jobs.snapshot(foreground.job).await.unwrap().location,
            ExecutionLocation::root(runtime.root.path().to_path_buf())
        );
        let background = executor
            .execute(
                agent.clone(),
                "echo",
                serde_json::json!({"value":"b", "bg":true, "name":"background-echo"}),
                None,
            )
            .await
            .unwrap();
        assert!(background.background);
        assert_eq!(background.output.value["name"], "background-echo");
        assert_eq!(
            jobs.wait(background.job, None, true).await.unwrap().output,
            Some(serde_json::json!("b"))
        );

        let mut builder = ToolRegistryBuilder::default();
        builder
            .register::<Echo, String, _, _>(
                "fail",
                "partial failure",
                ToolOptions::default(),
                |_context, input| async move {
                    Err(ToolError::with_output(
                        "stopped",
                        ToolOutput::new(serde_json::json!(input.value)),
                    ))
                },
            )
            .unwrap();
        let failed = runtime
            .executor(builder)
            .execute(agent, "fail", serde_json::json!({"value":"partial"}), None)
            .await
            .unwrap_err();
        assert!(matches!(
            failed,
            ExecutionError::Failed {
                output: Some(output),
                ..
            } if output.value == "partial"
        ));
    }

    #[tokio::test]
    async fn ephemeral_jobs_are_removed_after_claim() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::create_ephemeral(root.path()).await.unwrap();
        let agent = AgentId::root(store.id());
        let mut builder = ToolRegistryBuilder::default();
        builder
            .register::<Echo, String, _, _>(
                "echo",
                "echo",
                ToolOptions::new(Vec::new()),
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
        let result = executor
            .execute(agent, "echo", serde_json::json!({"value":"done"}), None)
            .await
            .unwrap();
        assert_eq!(
            jobs.snapshot(result.job).await.unwrap().state,
            JobState::Completed
        );
        assert_eq!(jobs.prune_claimed().await.unwrap(), 1);
        assert!(matches!(
            jobs.snapshot(result.job).await,
            Err(JobError::Unknown(_))
        ));
    }

    #[tokio::test]
    async fn restore_interrupts_active_jobs_and_advances_ids() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::create(root.path()).await.unwrap();
        let session = store.id();
        let agent = AgentId::root(session);
        let manager = JobManager::new(store.clone());
        let lease = manager
            .create(JobSpec {
                background: true,
                ..JobSpec::test(agent.clone(), "long_task")
            })
            .await
            .unwrap();
        manager
            .transition(lease.id, JobState::Running)
            .await
            .unwrap();
        let located = manager
            .create(JobSpec {
                background: true,
                location: ExecutionLocation::named("build", "/srv/project".into()),
                ..JobSpec::test(agent.clone(), "located")
            })
            .await
            .unwrap();
        manager
            .transition(located.id, JobState::Running)
            .await
            .unwrap();
        drop(lease);
        drop(located);
        drop(manager);
        store.close().await.unwrap();
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
        assert_eq!(
            restored
                .snapshot(JobId::new(1).unwrap())
                .await
                .unwrap()
                .location,
            ExecutionLocation::root(".".into())
        );
        assert_eq!(
            restored
                .snapshot(JobId::new(2).unwrap())
                .await
                .unwrap()
                .location,
            ExecutionLocation::named("build", "/srv/project".into())
        );
        let next = restored.create(JobSpec::test(agent, "next")).await.unwrap();
        assert_eq!(next.id.get(), 3);
    }

    #[tokio::test]
    async fn waiting_input_is_claimed_or_injected_exactly_once() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::create(root.path()).await.unwrap();
        let agent = AgentId::root(store.id());
        let manager = JobManager::new(store);
        let lease = manager
            .create(JobSpec {
                accepts_input: true,
                ..JobSpec::test(agent.clone(), "agent")
            })
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

    #[tokio::test]
    async fn foreground_waits_do_not_miss_fast_completions() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::create_ephemeral(root.path()).await.unwrap();
        let agent = AgentId::root(store.id());
        let manager = JobManager::new(store);
        for _ in 0..128 {
            let lease = manager
                .create(JobSpec::test(agent.clone(), "fast"))
                .await
                .unwrap();
            manager
                .transition(lease.id, JobState::Running)
                .await
                .unwrap();
            let waiter = tokio::spawn({
                let manager = manager.clone();
                async move { manager.wait_foreground(lease.id).await.unwrap() }
            });
            tokio::task::yield_now().await;
            manager
                .finish(
                    lease.id,
                    JobOutcome::Completed(ToolOutput::new(serde_json::json!("done"))),
                )
                .await
                .unwrap();
            let result = tokio::time::timeout(Duration::from_secs(1), waiter)
                .await
                .expect("foreground waiter missed a completion notification")
                .unwrap();
            assert_eq!(result.state, JobState::Completed);
        }
    }

    #[tokio::test]
    async fn uncooperative_handlers_are_aborted_after_cancellation_grace() {
        let runtime = TestRuntime::new().await;
        let agent = runtime.agent.clone();
        let mut builder = ToolRegistryBuilder::default();
        builder
            .register::<NoArgs, String, _, _>(
                "stubborn",
                "never completes",
                ToolOptions::new(Vec::new()).background(),
                |_context, _input| async move {
                    std::future::pending::<Result<String, ToolError>>().await
                },
            )
            .unwrap();
        let jobs = runtime.jobs.clone();
        let executor = runtime.executor(builder);
        let running = executor
            .execute(agent, "stubborn", serde_json::json!({"bg": true}), None)
            .await
            .unwrap();

        jobs.cancel(running.job).await.unwrap();
        let cancelled =
            tokio::time::timeout(Duration::from_secs(2), jobs.wait(running.job, None, true))
                .await
                .expect("forced cancellation did not terminate the job")
                .unwrap();
        assert_eq!(cancelled.state, JobState::Cancelled);
    }

    #[tokio::test]
    async fn cooperative_handlers_and_pending_authorization_observe_cancellation() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::create(root.path()).await.unwrap();
        let agent = AgentId::root(store.id());
        let observed = Arc::new(AtomicBool::new(false));
        let started = Arc::new(Notify::new());
        let mut builder = ToolRegistryBuilder::default();
        builder
            .register::<NoArgs, String, _, _>(
                "cooperative",
                "wait for cancellation",
                ToolOptions::new(Vec::new()).background(),
                {
                    let observed = observed.clone();
                    let started = started.clone();
                    move |context, _input| {
                        let observed = observed.clone();
                        let started = started.clone();
                        async move {
                            started.notify_one();
                            context.cancelled().await;
                            observed.store(true, Ordering::Relaxed);
                            Err(ToolError::Cancelled)
                        }
                    }
                },
            )
            .unwrap();
        let registry = builder.build();
        let jobs = JobManager::new(store);
        let executor = ToolExecutor::new(
            registry.clone(),
            Arc::new(AllowAll),
            jobs.clone(),
            root.path().to_path_buf(),
        );
        let running = executor
            .execute(
                agent.clone(),
                "cooperative",
                serde_json::json!({"bg": true}),
                None,
            )
            .await
            .unwrap();
        started.notified().await;
        jobs.cancel(running.job).await.unwrap();
        let cancelled =
            tokio::time::timeout(Duration::from_secs(1), jobs.wait(running.job, None, true))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(cancelled.state, JobState::Cancelled);
        assert!(observed.load(Ordering::Relaxed));

        let executor = ToolExecutor::new(
            registry,
            Arc::new(NeverAuthorize),
            jobs.clone(),
            root.path().to_path_buf(),
        );
        let execution = tokio::spawn(async move {
            executor
                .execute(agent, "cooperative", serde_json::json!({}), None)
                .await
        });
        let awaiting = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Some(job) = jobs
                    .list(&AgentId::root(jobs.store().id()))
                    .await
                    .into_iter()
                    .find(|job| job.state == JobState::AwaitingApproval)
                {
                    break job.id;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        jobs.cancel(awaiting).await.unwrap();
        let result = tokio::time::timeout(Duration::from_secs(1), execution)
            .await
            .expect("authorization did not observe cancellation")
            .unwrap();
        assert!(matches!(result, Err(ExecutionError::Failed { .. })));
        assert_eq!(
            jobs.snapshot(awaiting).await.unwrap().state,
            JobState::Cancelled
        );
    }

    #[tokio::test]
    async fn handler_panics_are_supervised_as_failed_jobs() {
        let runtime = TestRuntime::new().await;
        let agent = runtime.agent.clone();
        let mut builder = ToolRegistryBuilder::default();
        builder
            .register::<NoArgs, String, _, _>(
                "panic",
                "panic",
                ToolOptions::new(Vec::new()).background(),
                |_context, _input| async move {
                    panic!("handler panic");
                },
            )
            .unwrap();
        let jobs = runtime.jobs.clone();
        let executor = runtime.executor(builder);
        let running = executor
            .execute(agent, "panic", serde_json::json!({"bg": true}), None)
            .await
            .unwrap();
        let failed =
            tokio::time::timeout(Duration::from_secs(1), jobs.wait(running.job, None, true))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(failed.state, JobState::Failed);
        assert_eq!(failed.error.as_deref(), Some("tool handler panicked"));
    }

    #[tokio::test]
    async fn finalization_failure_wakes_waiters_with_a_volatile_failure() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::create(root.path()).await.unwrap();
        let session_directory = store.directory().to_path_buf();
        let agent = AgentId::root(store.id());
        let release = Arc::new(Semaphore::new(0));
        let mut builder = ToolRegistryBuilder::default();
        builder
            .register::<NoArgs, String, _, _>(
                "blocked",
                "wait before completing",
                ToolOptions::new(Vec::new()).background(),
                {
                    let release = release.clone();
                    move |_context, _input| {
                        let release = release.clone();
                        async move {
                            release.acquire().await.unwrap().forget();
                            Ok("done".to_owned())
                        }
                    }
                },
            )
            .unwrap();
        let jobs = JobManager::new(store);
        let executor = ToolExecutor::new(
            builder.build(),
            Arc::new(AllowAll),
            jobs.clone(),
            root.path().to_path_buf(),
        );
        let running = executor
            .execute(agent, "blocked", serde_json::json!({"bg": true}), None)
            .await
            .unwrap();
        tokio::fs::write(
            session_directory.join("jobs").join(running.job.to_string()),
            b"not a directory",
        )
        .await
        .unwrap();
        release.add_permits(1);

        let failed =
            tokio::time::timeout(Duration::from_secs(2), jobs.wait(running.job, None, true))
                .await
                .expect("persistence failure left the job waiting")
                .unwrap();
        assert_eq!(failed.state, JobState::Failed);
        assert!(
            failed
                .error
                .as_deref()
                .is_some_and(|error| error.contains("could not be persisted"))
        );
    }
}
