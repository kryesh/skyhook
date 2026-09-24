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
        policy::{Capability, CapabilitySet},
        registry::JobName,
    },
};

pub(crate) mod output;
pub use output::{
    CaptureKind, FieldPointer, OutputArgs as JobOutputQuery, OutputSelection, PresentedOutput,
    omit_null_fields,
};
mod cancellation;
mod delivery;
mod input;
mod lifecycle;
mod messages;
pub(super) mod views;

pub(crate) use delivery::PendingDelivery;
pub(crate) use progress::AgentProgress;
pub use views::JobEnvelope;
#[cfg(test)]
pub(crate) use views::Presentation;
pub(crate) use views::{JobMetadata, JobView, WaitCaller, presented_job_schema};
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
    #[derive(Clone, Copy, Debug, Default, Deserialize, JsonSchema, Serialize, PartialEq, Eq)]
    pub enum JobRole {
        #[default]
        Tool = "tool",
        Agent = "agent",
        Script = "script",
        Question = "question",
    }
}

/// Automatic completion delivery may reference an already-visible child reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OutputPresentation {
    Full,
    Automatic,
}

named_enum! {
    #[derive(Clone, Copy, Debug, Deserialize, JsonSchema, Serialize, PartialEq, Eq)]
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
    #[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
    pub enum JobTransition {
        AwaitingApproval = "awaiting_approval",
        Running = "running",
        WaitingInput = "waiting_input",
    }
}

named_enum! {
    /// How a job's invocation ended.
    #[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
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
    /// The cause decides whether the job failed, was cancelled, or was interrupted.
    Failed {
        diagnostic: PartialDiagnostic,
        output: Option<ToolOutput>,
    },
}

impl JobOutcome {
    fn end(&self) -> JobEnd {
        use crate::tool::diagnostic::Cause;
        match self {
            Self::Completed(_) => JobEnd::Completed,
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

/// Where a job is in its lifecycle. A waiting job holds the question it asked;
/// a finished one holds its published outcome.
enum Phase {
    Queued,
    AwaitingApproval,
    Running,
    WaitingInput(QuestionOutput),
    Finished(Box<Finished>),
}

/// A published outcome. Saved results and captures stay in the database.
#[derive(Clone)]
struct Finished {
    end: JobEnd,
    images: Vec<ImageRef>,
    diagnostic: Option<Diagnostic>,
    output_diagnostic: Option<Diagnostic>,
}

/// A phase change: the journaled transitions, plus the question a live job asks.
enum JobChange {
    Advance(JobTransition),
    Ask(QuestionOutput),
    Finish(Box<Finished>),
}

/// The journaled shape of a change, checked before its event is appended.
#[derive(Clone, Copy)]
enum JobStep {
    Advance(JobTransition),
    Finish(JobEnd),
}

impl JobChange {
    fn step(&self) -> JobStep {
        match self {
            Self::Advance(transition) => JobStep::Advance(*transition),
            Self::Ask(_) => JobStep::Advance(JobTransition::WaitingInput),
            Self::Finish(finished) => JobStep::Finish(finished.end),
        }
    }
}

/// A change the current phase does not admit.
struct Rejected;

/// What a role keeps beyond the shared lifecycle.
enum RoleState {
    Tool,
    Question,
    Agent(Child),
    Script {
        /// What a `wait` hosted by this script last reported, so it is not
        /// told twice.
        wait_floor: Option<WaitFloor>,
    },
}

impl RoleState {
    fn new(role: JobRole) -> Self {
        match role {
            JobRole::Tool => Self::Tool,
            JobRole::Question => Self::Question,
            JobRole::Agent => Self::Agent(Child::default()),
            JobRole::Script => Self::Script { wait_floor: None },
        }
    }

    fn role(&self) -> JobRole {
        match self {
            Self::Tool => JobRole::Tool,
            Self::Question => JobRole::Question,
            Self::Agent(_) => JobRole::Agent,
            Self::Script { .. } => JobRole::Script,
        }
    }
}

/// The agent an agent job launched and the replies it has published.
#[derive(Default)]
struct Child {
    /// Installed by the launch; a launch still in progress has none.
    agent: Option<AgentId>,
    /// Replies queued for delivery to the owner.
    messages: Vec<AgentMessage>,
    /// When the newest queued reply was published; replies are delivered
    /// oldest first, so it stays queued while any older one does.
    message_stamp: u64,
    /// A terminal reply, queued only when its invocation resolves: the job's end
    /// or `notify_owner` releases it, so the owner sees it with that resolution
    /// rather than alone at an earlier request boundary.
    withheld: Option<AgentMessage>,
    /// Source sequence of the last visible reply.
    last_message: Option<MessageSeq>,
}

impl Child {
    /// Queue a reply for delivery to the owner.
    fn queue(&mut self, message: AgentMessage) {
        self.message_stamp = next_pending_stamp();
        self.messages.push(message);
    }

    /// Queue the withheld reply, if any.
    fn release(&mut self) {
        if let Some(message) = self.withheld.take() {
            self.queue(message);
        }
    }
}

/// The pending stamp and input revision a script's `wait` last reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WaitFloor {
    pub(crate) stamp: u64,
    pub(crate) input: u64,
}

impl Rejected {
    /// The change was checked before its append, so the phase moved underneath it.
    fn journaled(self, id: JobId) -> JobError {
        JobError::PhaseMoved(id)
    }
}

struct JobEntry {
    origin: Option<crate::session::ModelCallOrigin>,
    output_schema: Option<Value>,
    agent: AgentId,
    parent: Option<JobId>,
    tool: String,
    role: RoleState,
    name: Option<JobName>,
    created_at_millis: i64,
    phase: Phase,
    accepts_input: bool,
    resume: Option<ResumeHandler>,
    input: mpsc::Sender<Value>,
    cancellation: CancellationToken,
    notify: Arc<Notify>,
    operation: Arc<Mutex<()>>,
    task_abort: Option<AbortHandle>,
    cancellation_watchdog_started: bool,
    delivery: DeliveryState,
    /// When the current delivery last became pending.
    delivery_stamp: u64,
    /// A `wait` parked for agent events: not work other waits should defer to.
    awaiting_events: bool,
    background: bool,
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
                role: RoleState::new(spec.role),
                name: spec.name,
                created_at_millis,
                phase: Phase::Queued,
                accepts_input: spec.accepts_input,
                resume: None,
                input,
                cancellation: CancellationToken::new(),
                notify: Arc::new(Notify::new()),
                operation: Arc::new(Mutex::new(())),
                task_abort: None,
                cancellation_watchdog_started: false,
                delivery: DeliveryState::Pending,
                awaiting_events: false,
                delivery_stamp: next_pending_stamp(),
                background: spec.background,
                location: spec.location,
            },
            receiver,
        )
    }

    fn role(&self) -> JobRole {
        self.role.role()
    }

    /// The launched agent of an agent job.
    fn child(&self) -> Option<&Child> {
        match &self.role {
            RoleState::Agent(child) => Some(child),
            _ => None,
        }
    }

    fn child_mut(&mut self) -> Option<&mut Child> {
        match &mut self.role {
            RoleState::Agent(child) => Some(child),
            _ => None,
        }
    }

    /// The journaled projection of the phase.
    fn state(&self) -> JobState {
        match &self.phase {
            Phase::Queued => JobState::Queued,
            Phase::AwaitingApproval => JobState::AwaitingApproval,
            Phase::Running => JobState::Running,
            Phase::WaitingInput(_) => JobState::WaitingInput,
            Phase::Finished(finished) => finished.end.into(),
        }
    }

    fn finished(&self) -> Option<&Finished> {
        match &self.phase {
            Phase::Finished(finished) => Some(finished.as_ref()),
            _ => None,
        }
    }

    fn end(&self) -> Option<JobEnd> {
        self.finished().map(|finished| finished.end)
    }

    fn question(&self) -> Option<&QuestionOutput> {
        match &self.phase {
            Phase::WaitingInput(question) => Some(question),
            _ => None,
        }
    }

    /// Whether the phase admits `step`; `apply` changes nothing otherwise.
    fn admits(&self, step: JobStep) -> bool {
        use JobTransition::{AwaitingApproval, Running, WaitingInput};
        match (&self.phase, step) {
            (Phase::Queued, JobStep::Advance(AwaitingApproval))
            | (Phase::AwaitingApproval, JobStep::Advance(Running))
            | (Phase::Running | Phase::WaitingInput(_), JobStep::Advance(WaitingInput))
            | (Phase::WaitingInput(_), JobStep::Advance(Running)) => true,
            (Phase::Finished(_), JobStep::Advance(Running)) => self.resumable_end(),
            // Cancelling an interruption ends its resumability; nothing else
            // supersedes a published outcome.
            (Phase::Finished(previous), JobStep::Finish(end)) => {
                previous.end == JobEnd::Interrupted && end == JobEnd::Cancelled
            }
            (_, JobStep::Finish(_)) => true,
            (_, JobStep::Advance(_)) => false,
        }
    }

    /// Move to the next phase with every live effect the change implies.
    fn apply(&mut self, change: JobChange) -> Result<(), Rejected> {
        if !self.admits(change.step()) {
            return Err(Rejected);
        }
        match change {
            JobChange::Advance(JobTransition::AwaitingApproval) => {
                self.phase = Phase::AwaitingApproval;
            }
            JobChange::Advance(JobTransition::Running) => {
                match &self.phase {
                    // A new invocation: the previous worker is gone and the new
                    // outcome is a new delivery.
                    Phase::Finished(previous) => {
                        // An interrupted foreground invocation still has its original
                        // waiter. Any other restart reports through delivery.
                        if previous.end != JobEnd::Interrupted {
                            self.background = true;
                        }
                        self.task_abort = None;
                        self.pend_delivery();
                    }
                    Phase::WaitingInput(_) => self.pend_delivery(),
                    Phase::Queued | Phase::AwaitingApproval | Phase::Running => {}
                }
                self.phase = Phase::Running;
            }
            JobChange::Advance(JobTransition::WaitingInput) => self.ask(QuestionOutput::default()),
            JobChange::Ask(question) => self.ask(question),
            JobChange::Finish(finished) => {
                if finished.end == JobEnd::Cancelled {
                    self.resume = None;
                }
                self.task_abort = None;
                self.phase = Phase::Finished(finished);
                // A new delivery even after an earlier question was acknowledged; it
                // carries the reply the invocation withheld for it.
                self.pend_delivery();
                if let Some(child) = self.child_mut() {
                    child.release();
                }
            }
        }
        Ok(())
    }

    /// A question is delivered like an outcome; its answer arrives as a wake.
    fn ask(&mut self, question: QuestionOutput) {
        self.phase = Phase::WaitingInput(question);
        self.pend_delivery();
        self.background = true;
    }

    fn waiting(&self) -> bool {
        self.question().is_some()
    }

    /// Interrupted with a handler that can restart it.
    fn suspended(&self) -> bool {
        self.end() == Some(JobEnd::Interrupted) && self.resume.is_some()
    }

    /// Not finished, or finished but retained for resumption. `active_states`
    /// deliberately differs: it presents a suspended job by its retained state.
    fn live(&self) -> bool {
        self.end().is_none() || self.suspended()
    }

    /// Ended, other than by a retained interruption.
    fn settled(&self) -> bool {
        self.end().is_some() && !self.suspended()
    }

    /// Still running, or interrupted: cancellation changes its outcome.
    fn cancellable(&self) -> bool {
        self.end().is_none_or(|end| end == JobEnd::Interrupted)
    }

    /// Finished in a way input can restart, given a handler.
    fn resumable_end(&self) -> bool {
        matches!(
            self.end(),
            Some(JobEnd::Completed | JobEnd::Failed | JobEnd::Interrupted)
        )
    }

    fn pend_delivery(&mut self) {
        self.delivery = DeliveryState::Pending;
        self.delivery_stamp = next_pending_stamp();
    }

    fn deliverable(&self) -> bool {
        // Keep parent waits pending across a retryable interruption. Snapshots
        // still expose the interrupted state to the user.
        !self.suspended() && (self.end().is_some() || self.waiting())
    }

    fn reserve_delivery(&mut self, delivery: DeliveryState) -> Option<AgentId> {
        if !self.deliverable() || self.delivery != DeliveryState::Pending {
            return None;
        }
        self.delivery = delivery;
        Some(self.agent.clone())
    }

    fn metadata(&self, id: JobId) -> JobEnvelope {
        let finished = self.finished();
        let diagnostic = finished.and_then(|finished| finished.diagnostic.clone());
        JobEnvelope {
            id,
            parent: self.parent,
            tool: self.tool.clone(),
            role: self.role(),
            name: self.name.clone().map(String::from),
            state: self.state(),
            output: None,
            question: None,
            diagnostic,
            output_diagnostic: finished.and_then(|finished| finished.output_diagnostic.clone()),
            location: self.location.clone(),
        }
    }

    fn envelope(&self, id: JobId) -> JobEnvelope {
        JobEnvelope {
            question: self.question().cloned(),
            ..self.metadata(id)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    pub name: Option<String>,
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

/// Why a destination cannot accept input in its current lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputUnavailableReason {
    State(JobState),
    CancellationRequested,
    ResumeUnavailable,
}

impl std::fmt::Display for InputUnavailableReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::State(state) => write!(formatter, "job is {}", state.presented()),
            Self::CancellationRequested => formatter.write_str("cancellation has been requested"),
            Self::ResumeUnavailable => {
                formatter.write_str("no retained resume handler is available")
            }
        }
    }
}

impl<O> From<JobError> for crate::tool::invocation::OperationError<O> {
    fn from(error: JobError) -> Self {
        match error {
            JobError::Session(error) => error.into(),
            JobError::InputClosed(_) => Self::input_closed(),
            JobError::Output(error) => Self::from_facts(error.into_facts().0, None),
            error => Self::failed(error),
        }
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
    #[error("job {job} input unavailable: {reason}")]
    InputUnavailable {
        job: JobId,
        reason: InputUnavailableReason,
    },
    #[error("job {0} does not accept input")]
    InputUnsupported(JobId),
    #[error("job {0} input channel is closed")]
    InputClosed(JobId),
    #[error("job identifier space is exhausted")]
    InvalidId,
    /// The journaled change was admitted, but the live phase moved underneath it.
    #[error("job {0} changed phase while its change was journaled")]
    PhaseMoved(JobId),
    #[error("journal entry {sequence} changes a job's phase illegally")]
    IllegalTransition { sequence: u64 },
    #[error("job {0} did not launch an agent")]
    NoChild(JobId),
    #[error("child agent does not belong to job {0}")]
    ChildOwnerMismatch(JobId),
    #[error("child message text does not match the committed assistant text")]
    ChildTextMismatch,
    /// The detached task publishing `owner` stopped before reporting.
    #[error("{owner} owner lost: {reason}")]
    OwnerLost { owner: &'static str, reason: String },
    /// Saving or reading the job's output failed; the facts name the stage.
    #[error(transparent)]
    Output(Box<crate::tool::ToolError>),
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
            state,
            has_result: false,
            result: Value::Null,
            error: None,
            meta: None,
            presentation: None,
        }))
    }

    /// Poll until a published job settles into a terminal state.
    pub(super) async fn terminal(jobs: &JobManager, id: JobId) -> JobEnvelope {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                match jobs.metadata(id).await {
                    Ok(envelope) if envelope.state.is_terminal() => return envelope,
                    Ok(_) | Err(JobError::Unknown(_)) => tokio::task::yield_now().await,
                    Err(error) => panic!("unexpected job lookup failure: {error}"),
                }
            }
        })
        .await
        .expect("creation owner must settle its published job")
    }

    /// Every phase change `apply` admits or rejects, with the live effects a
    /// legal one carries.
    #[tokio::test]
    async fn apply_admits_only_lifecycle_order_and_cancels_interruptions() {
        use JobChange::{Advance, Ask, Finish};
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
        let entry = || JobEntry::new(JobSpec::test(agent.clone(), "phase"), 0).0;
        let at = |changes: &[JobChange]| {
            let mut entry = entry();
            for change in changes {
                let change = match change {
                    Advance(transition) => Advance(*transition),
                    Ask(question) => Ask(question.clone()),
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
            entry.resume = Some(handler.clone());
            let before = entry.state();
            let applied = entry.apply(change);
            assert_eq!(applied.is_ok(), expected.is_some(), "{case}");
            assert_eq!(entry.state(), expected.unwrap_or(before), "{case}");
            assert_eq!(
                entry.resume.is_none(),
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
            entry.delivery = DeliveryState::Claimed;
            entry.apply(change).ok().unwrap();
            assert_eq!(entry.background, background, "{case}");
            assert_eq!(entry.delivery, DeliveryState::Pending, "{case}");
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
