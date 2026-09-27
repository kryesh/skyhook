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
    agent::QuestionOutput,
    execution::ExecutionLocation,
    identity::{AgentId, JobId},
    media::ImageRef,
    named_enum::named_enum,
    session::{EventRecord, MessageSeq, RecordSeq, SessionError, SessionEvent, SessionStore},
    tool::{
        ToolError, ToolOutput,
        diagnostic::{Diagnostic, PartialDiagnostic},
        policy::CapabilitySet,
        registry::JobName,
    },
};

pub(crate) mod output;
pub use output::{
    CaptureDescriptor, CaptureKind, FieldPointer, OutputArgs as JobOutputQuery, OutputPreview,
    OutputSelection, PresentedOutput, diagnostic_slot, omit_null_fields,
};
mod cancellation;
pub(crate) use cancellation::CANCELLATION_GRACE;
mod delivery;
mod entry;
mod error;
mod input;
mod lifecycle;
mod messages;
pub(super) mod views;

pub(crate) use delivery::PendingDelivery;
pub(crate) use entry::WaitFloor;
use entry::{Answer, Finished, JobChange, JobEntry, JobStep, Phase, Rejected, RoleState};
pub use error::{InputUnavailableReason, JobError};
pub(crate) use progress::AgentProgress;
pub(crate) use views::{JOB_VIEW_SCHEMAS, WaitCaller};
pub use views::{JobEnvelope, JobView, Notice, Presentation};
mod persistence;
mod progress;
mod supervisor;
#[cfg(test)]
pub(crate) use supervisor::JobWorker;
pub use supervisor::{JobLease, stage};

const JOB_INPUT_CAPACITY: usize = 32;

/// Orders what becomes pending for delivery. Taken under the jobs lock, so a wait
/// floor snapshotted under that lock covers exactly what the wait could see.
static PENDING_STAMP: AtomicU64 = AtomicU64::new(0);

fn next_pending_stamp() -> u64 {
    PENDING_STAMP.fetch_add(1, Ordering::Relaxed) + 1
}

fn current_pending_stamp() -> u64 {
    PENDING_STAMP.load(Ordering::Relaxed)
}

named_enum! {
    /// Semantic execution role, independent of extensible tool names.
    #[derive(Clone, Copy, Debug, Default, JsonSchema, Serialize, PartialEq, Eq)]
    pub enum JobRole {
        #[default]
        Tool = "tool",
        Agent = "agent",
        Script = "script",
        Question = "question",
        /// A `wait`: the agent pauses for events rather than doing work.
        Wait = "wait",
    }
}

/// How a model read presents a job: `Automatic` leaves out the metadata a
/// successful foreground call does not need; `Full` always shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OutputPresentation {
    Full,
    Automatic,
}

named_enum! {
    #[derive(Clone, Copy, Debug, JsonSchema, Serialize, PartialEq, Eq)]
    pub enum JobState {
        Queued = "queued",
        #[schemars(skip)]
        AwaitingApproval = "awaiting_approval",
        Running = "running",
        WaitingInput = "waiting_input",
        Completed = "completed",
        Failed = "failed",
        Cancelled = "cancelled",
        Interrupted = "interrupted",
    }
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

named_enum! {
    /// A live state a job is journaled entering; `Queued` is only ever created into.
    #[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
    pub enum JobTransition {
        AwaitingApproval = "awaiting_approval",
        Running = "running",
        WaitingInput = "waiting_input",
    }
}

named_enum! {
    /// How a job's invocation ended.
    #[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
    pub enum JobEnd {
        Completed = "completed",
        Failed = "failed",
        Cancelled = "cancelled",
        Interrupted = "interrupted",
    }
}

impl From<JobTransition> for JobState {
    fn from(transition: JobTransition) -> Self {
        match transition {
            JobTransition::AwaitingApproval => Self::AwaitingApproval,
            JobTransition::Running => Self::Running,
            JobTransition::WaitingInput => Self::WaitingInput,
        }
    }
}

impl From<JobEnd> for JobState {
    fn from(end: JobEnd) -> Self {
        match end {
            JobEnd::Completed => Self::Completed,
            JobEnd::Failed => Self::Failed,
            JobEnd::Cancelled => Self::Cancelled,
            JobEnd::Interrupted => Self::Interrupted,
        }
    }
}

pub(crate) enum JobOutcome {
    Completed(ToolOutput),
    /// Completed by a tool whose native result type is unit.
    NoResult,
    /// The cause decides whether the job failed, was cancelled, or was interrupted.
    Failed {
        diagnostic: PartialDiagnostic,
        output: Option<ToolOutput>,
    },
}

impl JobOutcome {
    /// A unit tool's outcome: it has no result, even when cancelled or failed.
    pub(crate) fn without_result(self) -> Self {
        match self {
            Self::Completed(_) | Self::NoResult => Self::NoResult,
            Self::Failed { diagnostic, .. } => Self::Failed {
                diagnostic,
                output: None,
            },
        }
    }

    fn end(&self) -> JobEnd {
        use crate::tool::diagnostic::Cause;
        match self {
            Self::Completed(_) | Self::NoResult => JobEnd::Completed,
            Self::Failed { diagnostic, .. } => match diagnostic.cause {
                Cause::Cancelled => JobEnd::Cancelled,
                Cause::Interrupted => JobEnd::Interrupted,
                _ => JobEnd::Failed,
            },
        }
    }
}

impl From<crate::tool::ToolError> for JobOutcome {
    fn from(error: crate::tool::ToolError) -> Self {
        let (diagnostic, output) = error.into_facts();
        Self::Failed { diagnostic, output }
    }
}

impl From<Result<ToolOutput, crate::tool::ToolError>> for JobOutcome {
    fn from(result: Result<ToolOutput, crate::tool::ToolError>) -> Self {
        result.map_or_else(Into::into, Self::Completed)
    }
}

#[derive(Clone, Debug)]
pub struct JobCompletion {
    pub agent: AgentId,
    pub job: JobId,
}

/// A restored agent job: its owner, child and the authority its invocations ran under.
pub(crate) struct RetainedChild {
    pub job: JobId,
    pub owner: AgentId,
    pub parent: Option<JobId>,
    pub child: AgentId,
    pub location: ExecutionLocation,
    pub cancellation: CancellationToken,
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

#[derive(Clone, Copy)]
enum WaitMode {
    Foreground,
    Explicit { claim: bool },
    Terminal,
}

struct JobManagerInner {
    store: SessionStore,
    jobs: Mutex<HashMap<JobId, JobEntry>>,
    progress: Mutex<progress::Progress>,
    supervision: Arc<supervisor::Supervision>,
    delivery_operation: Arc<Mutex<()>>,
    /// Shared by concurrent creations; exclusive for drain.
    /// Never acquired while holding a jobs, delivery, or per-job operation lock.
    creation_operation: Arc<tokio::sync::RwLock<()>>,
    next_id: AtomicU64,
    completions: broadcast::Sender<JobCompletion>,
    /// Per agent, fires when one of its jobs newly parks in a `wait`, releasing
    /// that agent's waits deferring to it.
    parked: std::sync::Mutex<HashMap<AgentId, Arc<Notify>>>,
    /// Per parent job, its issued calls whose jobs are not published yet.
    issuing: std::sync::Mutex<HashMap<JobId, usize>>,
    issued: Notify,
}

/// A call from its issue until its job is published or refused.
pub(crate) struct Issuing(JobManager, JobId);

impl Drop for Issuing {
    fn drop(&mut self) {
        let inner = &self.0.inner;
        let mut issuing = inner.issuing.lock().expect("issuing poisoned");
        if let Some(count) = issuing.get_mut(&self.1) {
            *count -= 1;
            if *count == 0 {
                issuing.remove(&self.1);
            }
        }
        inner.issued.notify_waiters();
    }
}

#[derive(Clone)]
pub struct JobManager {
    inner: Arc<JobManagerInner>,
}

/// A visible child reply identified by its source MessageCommitted sequence.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct AgentMessage {
    pub id: JobId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<JobName>,
    /// Delivery bookkeeping for the journal and the TUI; the model never sees it.
    #[serde(skip)]
    pub message: MessageSeq,
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct JobSpec {
    pub origin: Option<crate::session::ModelCallOrigin>,
    pub output_schema: Option<Value>,
    pub agent: AgentId,
    pub parent: Option<JobId>,
    pub tool: String,
    pub role: JobRole,
    pub name: Option<JobName>,
    pub arguments: Value,
    pub accepts_input: bool,
    pub background: bool,
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
            role: JobRole::Tool,
            name: None,
            arguments: Value::Object(serde_json::Map::new()),
            accepts_input: false,
            background: false,
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
                supervision: Arc::new(supervisor::Supervision::default()),
                delivery_operation: Arc::new(Mutex::new(())),
                creation_operation: Arc::default(),
                next_id: AtomicU64::new(next_id),
                completions,
                parked: std::sync::Mutex::default(),
                issuing: std::sync::Mutex::default(),
                issued: Notify::new(),
            }),
        }
    }

    pub async fn restore(store: SessionStore, records: &[EventRecord]) -> Result<Self, JobError> {
        persistence::restore(store, records).await
    }

    /// Count a call `parent` issued, synchronously, until its job is published.
    pub(crate) fn issue(&self, parent: JobId) -> Issuing {
        let mut issuing = self.inner.issuing.lock().expect("issuing poisoned");
        *issuing.entry(parent).or_default() += 1;
        Issuing(self.clone(), parent)
    }

    /// Wait until every call `parent` has issued so far has its job published.
    pub(crate) async fn issued(&self, parent: JobId) {
        loop {
            let published = self.inner.issued.notified();
            let pending = (self.inner.issuing.lock())
                .expect("issuing poisoned")
                .contains_key(&parent);
            if !pending {
                return;
            }
            published.await;
        }
    }

    #[must_use]
    pub fn subscribe_completions(&self) -> broadcast::Receiver<JobCompletion> {
        self.inner.completions.subscribe()
    }

    #[must_use]
    pub fn store(&self) -> &SessionStore {
        &self.inner.store
    }

    /// Tell `agent` it has work from `job` to collect.
    fn wake(&self, agent: AgentId, job: JobId) {
        let _ = self.inner.completions.send(JobCompletion { agent, job });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Message;

    pub(super) async fn runtime() -> (tempfile::TempDir, JobManager, AgentId) {
        let runtime = crate::tests::TestRuntime::new().await;
        (runtime.root, runtime.jobs, runtime.agent)
    }

    /// A parent notification presenting `events`.
    pub(super) fn job_events(events: Vec<crate::session::JobEvent>) -> Message {
        Message::User(vec![crate::session::UserPart::JobEvents { events }])
    }

    /// A one-question batch identified by `id`.
    pub(super) fn question(id: &str) -> QuestionOutput {
        QuestionOutput {
            questions: vec![crate::agent::Question {
                id: id.to_owned(),
                prompt: "Question?".to_owned(),
                options: Vec::new(),
            }],
        }
    }

    /// A job's view as a notification presents it, with no result or metadata.
    pub(super) fn job_view(job: JobId, state: JobState) -> crate::session::JobEvent {
        crate::session::JobEvent::Job(Box::new(JobView {
            id: Some(job),
            state: Some(state),
            result: None,
            error: None,
            meta: None,
            presentation: None,
        }))
    }

    /// Every phase change `apply` admits or rejects, with the live effects a
    /// legal one carries.
    #[tokio::test]
    async fn apply_admits_only_lifecycle_order_and_cancels_interruptions() {
        use JobChange::{Advance, Answer, Ask, Finish};
        use JobTransition::{AwaitingApproval, Running, WaitingInput};
        let finished = |end| {
            Box::new(Finished {
                end,
                images: Vec::new(),
                diagnostic: None,
                output_diagnostic: None,
            })
        };
        let handler: ResumeHandler =
            Arc::new(|_, _| Box::pin(async { Ok(ToolOutput::new(Value::Null)) }));
        let agent = AgentId::root(crate::identity::SessionId::generate().unwrap());
        let spec = JobSpec {
            role: JobRole::Agent,
            ..JobSpec::test(agent.clone(), "phase")
        };
        let entry = || JobEntry::new(spec.clone(), 0).0;
        let at = |changes: &[JobChange]| {
            let mut entry = entry();
            for change in changes {
                let change = match change {
                    Advance(transition) => Advance(*transition),
                    Ask(question) => Ask(question.clone()),
                    Answer => Answer,
                    Finish(outcome) => Finish(outcome.clone()),
                };
                entry.apply(change).ok().expect("fixture prefix is legal");
            }
            entry
        };
        let running = [Advance(AwaitingApproval), Advance(Running)];
        let waiting = [
            Advance(AwaitingApproval),
            Advance(Running),
            Ask(question("q")),
        ];
        let interrupted = [
            Advance(AwaitingApproval),
            Advance(Running),
            Finish(finished(JobEnd::Interrupted)),
        ];
        let completed = [
            Advance(AwaitingApproval),
            Advance(Running),
            Finish(finished(JobEnd::Completed)),
        ];
        let cancelled = [
            Advance(AwaitingApproval),
            Advance(Running),
            Finish(finished(JobEnd::Cancelled)),
        ];
        let cases: Vec<(&str, &[JobChange], JobChange, Option<JobState>)> = vec![
            (
                "queued approval",
                &[],
                Advance(AwaitingApproval),
                Some(JobState::AwaitingApproval),
            ),
            ("queued running", &[], Advance(Running), None),
            ("queued question", &[], Ask(question("q")), None),
            (
                "queued finish",
                &[],
                Finish(finished(JobEnd::Failed)),
                Some(JobState::Failed),
            ),
            ("running again", &running, Advance(Running), None),
            (
                "running approval",
                &running,
                Advance(AwaitingApproval),
                None,
            ),
            (
                "running question",
                &running,
                Ask(question("q")),
                Some(JobState::WaitingInput),
            ),
            (
                "waiting refresh",
                &waiting,
                Ask(question("r")),
                Some(JobState::WaitingInput),
            ),
            (
                "waiting replay",
                &waiting,
                Advance(WaitingInput),
                Some(JobState::WaitingInput),
            ),
            (
                "waiting resume",
                &waiting,
                Advance(Running),
                Some(JobState::Running),
            ),
            ("waiting answer", &waiting, Answer, Some(JobState::Running)),
            ("running answer", &running, Answer, None),
            (
                "waiting finish",
                &waiting,
                Finish(finished(JobEnd::Completed)),
                Some(JobState::Completed),
            ),
            (
                "completed restart",
                &completed,
                Advance(Running),
                Some(JobState::Running),
            ),
            (
                "completed finish",
                &completed,
                Finish(finished(JobEnd::Failed)),
                None,
            ),
            (
                "interrupted cancel",
                &interrupted,
                Finish(finished(JobEnd::Cancelled)),
                Some(JobState::Cancelled),
            ),
            (
                "interrupted finish",
                &interrupted,
                Finish(finished(JobEnd::Completed)),
                None,
            ),
            (
                "interrupted restart",
                &interrupted,
                Advance(Running),
                Some(JobState::Running),
            ),
            ("cancelled restart", &cancelled, Advance(Running), None),
            (
                "cancelled finish",
                &cancelled,
                Finish(finished(JobEnd::Interrupted)),
                None,
            ),
        ];
        for (case, prefix, change, expected) in cases {
            let mut entry = at(prefix);
            entry.child_mut().unwrap().resume = Some(handler.clone());
            let before = entry.state();
            let applied = entry.apply(change);
            assert_eq!(applied.is_ok(), expected.is_some(), "{case}");
            assert_eq!(entry.state(), expected.unwrap_or(before), "{case}");
            assert_eq!(
                entry.resume().is_none(),
                expected == Some(JobState::Cancelled),
                "{case}"
            );
        }
        // A restart reports through delivery unless the interruption kept its waiter;
        // a question always does.
        for (case, prefix, change, background) in [
            ("completed", &completed[..], Advance(Running), true),
            ("interrupted", &interrupted[..], Advance(Running), false),
            ("asked", &running[..], Ask(question("q")), true),
        ] {
            let mut entry = at(prefix);
            entry.background = false;
            entry.acknowledge();
            entry.apply(change).ok().unwrap();
            assert_eq!(entry.background, background, "{case}");
            assert!(entry.unacknowledged(), "{case}");
        }
    }

    // Success-only fixture operations. A returned lease is the job's owner:
    // dropping it cancels the job.
    impl JobManager {
        pub(super) async fn test_lease(&self, spec: JobSpec) -> JobLease {
            self.create(spec).await.unwrap()
        }

        pub(crate) async fn test_create(&self, spec: JobSpec) -> JobId {
            self.test_lease(spec).await.into_test_id()
        }

        pub(super) async fn test_approving(&self, spec: JobSpec) -> JobLease<stage::Approving> {
            self.test_lease(spec).await.await_approval().await.unwrap()
        }

        /// A running job with no worker.
        pub(crate) async fn test_running(&self, spec: JobSpec) -> JobLease<stage::Running> {
            self.test_lease(spec).await.test_run().await
        }

        pub(super) async fn test_finish(&self, job: JobId, output: Value) {
            self.finish(job, JobOutcome::Completed(ToolOutput::new(output)))
                .await
                .unwrap();
        }

        pub(super) async fn test_append(&self, agent: AgentId, event: SessionEvent) -> RecordSeq {
            self.store().append(agent, event).await.unwrap().sequence
        }

        pub(super) async fn test_replay(&self) -> Self {
            Self::restore(self.store().clone(), &self.store().records().await)
                .await
                .unwrap()
        }
    }
}
