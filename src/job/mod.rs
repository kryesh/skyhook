//! Supervised jobs and durable saved output.
use std::{
    collections::HashMap,
    sync::{Arc, PoisonError},
    time::Duration,
};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::{sync::mpsc, task::AbortHandle};
pub(crate) use tokio_util::sync::CancellationToken;

use crate::{
    agent::QuestionOutput,
    execution::ExecutionLocation,
    identity::{AgentId, JobId},
    media::ImageRef,
    named_enum::named_enum,
    session::{
        Command, CoreState, Key, MessageSeq, Plan, Prepared, RecordSeq, SessionError, SessionEvent,
        SessionStore, Wait,
    },
    tool::{
        ToolError, ToolOutput,
        diagnostic::{Diagnostic, PartialDiagnostic},
        policy::CapabilitySet,
        registry::JobName,
    },
};

pub(crate) mod output;
pub use output::{
    CaptureDescriptor, CaptureKind, Continuation, ElementPage, FieldPointer, LinePage, Match,
    MatchPage, MemberPage, OutputArgs as JobOutputQuery, OutputFields, OutputPreview,
    OutputSelection, OutputTruncation, PageLimit, PresentedOutput, omit_null_fields,
};
mod cancellation;
pub(crate) use cancellation::{CANCELLATION_GRACE, CancelScope};
mod delivery;
mod entry;
mod error;
mod input;
mod lifecycle;
mod messages;
mod pages;
mod process;
pub(super) mod views;

pub(crate) use delivery::{Delivery, Notification};
use entry::{
    Answer, CancelState, Child, DeliveryState, Finished, JobChange, JobFacts, JobHandles, Phase,
    Queued, Rejected, RoleState, Stamps, Start, addressed,
};
pub(crate) use entry::{AnswerKind, Run, WaitFloor};
pub use entry::{Invocation, RunTiming};
pub use error::{InputUnavailableReason, JobError};
pub(crate) use input::Concluding;
pub(crate) use lifecycle::woken;
pub use pages::JobScope;
pub(crate) use process::{JobProcess, ProcessOwner};
pub(crate) use progress::AgentProgress;
pub(crate) use views::{JOB_VIEW_SCHEMAS, JobListing, ViewKind, Waiting, release_records};
pub use views::{JobEnvelope, JobView, Notice, Presentation};
mod index;
use index::Lookup;
pub(crate) use index::{JobSeeds, Jobs};
#[cfg(debug_assertions)]
pub(crate) use persistence::assert_folded;
pub(crate) use persistence::{ChildStatus, JobStatus, Settled};
mod persistence;
mod progress;
mod supervisor;
#[cfg(test)]
pub(crate) use supervisor::JobWorker;
pub(crate) use supervisor::Supervisor;
pub use supervisor::{JobLease, stage};

const JOB_INPUT_CAPACITY: usize = 32;

/// Whether a job shows under its parent, as one its owner's script issued: a job a
/// model call launched names the calling agent's own job as its parent, so it is a
/// root of the calling agent's view, as is one without a parent.
pub fn nests(parent: Option<JobId>, origin: &Option<crate::session::ModelCallOrigin>) -> bool {
    parent.is_some() && origin.is_none()
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

/// An agent job that restarts: its owner, where it runs, the token its
/// invocations run under, and what runs it.
#[derive(Clone)]
pub(crate) struct Restarting {
    pub job: JobId,
    pub owner: AgentId,
    pub parent: Option<JobId>,
    pub location: ExecutionLocation,
    pub cancellation: CancellationToken,
    pub restart: Restart,
}

/// What runs an agent job again, as its own facts decide.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Restart {
    /// The child its call installed, under its owner's authority.
    Child,
    /// The call itself, asked for again.
    Call,
}

/// Builds what runs a restarting agent job, as it restarts: the session's agent
/// runtime, which runs it under its owner's journaled contract.
pub(crate) trait Restarter: Send + Sync {
    /// What runs `restarting`, if anything can.
    fn resume(self: Arc<Self>, restarting: Restarting) -> Option<Resume>;
}

/// What restarts a finished agent job; ordinary input-capable tools cannot restart.
#[derive(Clone)]
pub(crate) enum Resume {
    /// Runs the installed child again, given its owner's input, if any.
    Child(ResumeHandler),
    /// Asks again for a call interrupted before it installed its child, then
    /// runs it, from its approval on.
    Call(Relaunch),
}

/// Starts a call's restarted invocation, from the lease that runs it.
pub(crate) type Relaunch = Arc<
    dyn Fn(JobLease<stage::Running>) -> futures_util::future::BoxFuture<'static, ()> + Send + Sync,
>;

/// Runs an installed child's restarted invocation as its worker.
pub(crate) type ResumeHandler = Arc<
    dyn Fn(
            Option<Value>,
            mpsc::Receiver<Value>,
        )
            -> futures_util::future::BoxFuture<'static, Result<ToolOutput, crate::tool::ToolError>>
        + Send
        + Sync,
>;

struct JobManagerInner {
    store: SessionStore,
    /// Set once the session's agent runtime is built: without it, nothing restarts.
    restarter: std::sync::OnceLock<std::sync::Weak<dyn Restarter>>,
    /// The job each attached process belongs to.
    processes: std::sync::Mutex<HashMap<u32, JobId>>,
    supervisor: Supervisor,
}

/// A call from its issue until its job is published or refused.
pub(crate) struct Issuing(SessionStore, JobId);

impl Drop for Issuing {
    fn drop(&mut self) {
        let parent = self.1;
        self.0.enqueue(move |state| state.jobs.issued(parent));
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
        Self {
            inner: Arc::new(JobManagerInner {
                supervisor: Supervisor::new(store.root()),
                store,
                restarter: std::sync::OnceLock::new(),
                processes: std::sync::Mutex::default(),
            }),
        }
    }

    /// Let `restarter` build what runs agent jobs that restart.
    pub(crate) fn restart_with(&self, restarter: std::sync::Weak<dyn Restarter>) {
        let _ = self.inner.restarter.set(restarter);
    }

    /// Read the jobs as the core last applied them.
    fn facts<T>(&self, read: impl FnOnce(&Jobs) -> T) -> T {
        self.inner.store.read(|state| read(&state.jobs))
    }

    /// Read job `id`'s facts, held or read back from its status row once no longer held.
    async fn entry<T>(&self, id: JobId, read: impl FnOnce(&JobFacts) -> T) -> Result<T, JobError> {
        let lookup = self.facts(move |jobs| match jobs.lookup(id) {
            Lookup::Held(entry) => Lookup::Held(read(entry)),
            Lookup::Unheld(()) => Lookup::Unheld(read),
            Lookup::Unknown => Lookup::Unknown,
        });
        match lookup {
            Lookup::Held(value) => Ok(value),
            Lookup::Unheld(read) => {
                let journal = self.store().journal();
                let settled = move || Settled::read(&journal, &[id]);
                let mut settled = self.store().reads().hold(settled).await?;
                Ok(read(&settled.take(id)?))
            }
            Lookup::Unknown => Err(JobError::Unknown(id)),
        }
    }

    /// Count a call `parent` issued, synchronously, until its job is published:
    /// the count is queued ahead of every command its caller issues after it.
    pub(crate) fn issue(&self, parent: JobId) -> Issuing {
        let store = self.inner.store.clone();
        store.enqueue(move |state| state.jobs.issue(parent));
        Issuing(store, parent)
    }

    /// Wait until every call `parent` has issued so far has its job published.
    pub(crate) async fn issued(&self, parent: JobId) {
        let published = move |state: &CoreState| state.jobs.published(parent);
        let _ = (self.inner.store)
            .until(Wait::on([Key::Job(parent)], published))
            .await;
    }

    /// Record that a call of `agent`'s response committed at `response` ended
    /// without a job.
    pub(crate) fn settled_jobless(&self, agent: &AgentId, response: MessageSeq) {
        let agent = agent.clone();
        let settled = move |state: &mut CoreState| state.jobs.settled_jobless(agent, response);
        self.inner.store.enqueue(settled);
    }

    pub(crate) fn supervisor(&self) -> &Supervisor {
        &self.inner.supervisor
    }

    pub fn store(&self) -> &SessionStore {
        &self.inner.store
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::session::Message;

    pub(super) async fn runtime() -> (tempfile::TempDir, JobManager, AgentId) {
        let runtime = crate::tests::TestRuntime::new().await;
        (runtime.root, runtime.jobs, runtime.agent)
    }

    /// What tests run as their agent jobs' children, by session and job: the
    /// restarter of every job manager a test gives one.
    static HANDLERS: std::sync::LazyLock<Arc<Handlers>> = std::sync::LazyLock::new(Arc::default);

    #[derive(Default)]
    struct Handlers(std::sync::Mutex<HashMap<(crate::identity::SessionId, JobId), ResumeHandler>>);

    impl Restarter for Handlers {
        fn resume(self: Arc<Self>, restarting: Restarting) -> Option<Resume> {
            let key = (restarting.owner.session(), restarting.job);
            let handlers = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            handlers.get(&key).cloned().map(Resume::Child)
        }
    }

    /// Hold, or let go, the finish of the job `child` answers through, as a
    /// finish in flight holds it: its own publication waits until let go.
    pub(crate) fn hold_finish(state: &mut CoreState, child: &AgentId, held: bool) {
        let job = state.jobs.launched(child).expect("a launched child");
        state.jobs.modify(job, |entry, _| entry.finishing = held);
    }

    /// Drain owners, drop the manager, and replay the durable journal of the
    /// runtime at `root`.
    pub(crate) async fn reopen(jobs: JobManager, root: &std::path::Path) -> JobManager {
        let session = jobs.store().id();
        jobs.supervisor().drain().await;
        drop(jobs);
        let store = SessionStore::open(&root.join("sessions"), session)
            .await
            .unwrap();
        reopened(store).await
    }

    /// The jobs of a reopened session, what its last process left running settled.
    pub(crate) async fn reopened(store: SessionStore) -> JobManager {
        let jobs = JobManager::new(store.clone());
        let (interrupted, outputs) = jobs.interrupt_running().await.unwrap();
        let settle =
            |_: &CoreState| Ok::<_, SessionError>(Plan::journal(interrupted).saving(outputs));
        store.commit(Command::Activate, settle).await.unwrap();
        jobs
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

    /// Progress: a turn with a tool call beside its text.
    pub(super) fn progress(text: &str) -> Message {
        use crate::provider::protocol::{AssistantItem, ToolCall};
        let call = ToolCall::new("call", "todo", serde_json::json!({}));
        Message::Assistant(vec![
            AssistantItem::text("text", 0, text),
            AssistantItem::tool_call("call", 1, call.unwrap()),
        ])
    }

    /// Commit `child`'s progress reply for `job`.
    pub(super) async fn commit(
        manager: &JobManager,
        child: &AgentId,
        job: JobId,
        text: &str,
    ) -> MessageSeq {
        manager
            .commit_child_message(child, job, progress(text), text.into(), |_| Vec::new())
            .await
            .unwrap()
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

    /// Every phase change a job admits or rejects, with the effects the journaled
    /// change carries when it is applied.
    #[test]
    fn changes_admit_only_lifecycle_order_and_cancel_interruptions() {
        use JobChange::{Advance, Answer, Ask, Finish};
        use JobTransition::{AwaitingApproval, Running, WaitingInput};
        let finished = |end| {
            Box::new(Finished {
                end,
                images: Vec::new(),
                diagnostic: None,
            })
        };
        let agent = AgentId::root(crate::identity::SessionId::generate().unwrap());
        let id = JobId::new(1).unwrap();
        let created = crate::session::tests::job_created(id, JobRole::Agent);
        // Applies a change as the journal does, when the phase admits it.
        let apply = |entry: &mut JobFacts, stamps: &Stamps, change: &JobChange| {
            entry.admits(change.step()).ok()?;
            let (_, journaled) = JobChange::of(&change.event(id)).unwrap();
            entry.enter(journaled, stamps);
            Some(())
        };
        let at = |changes: &[JobChange]| {
            let mut jobs = Jobs::default();
            (jobs.apply(&crate::session::tests::record(&agent, 1, created.clone()))).unwrap();
            jobs.modify(id, |entry, stamps| {
                for change in changes {
                    apply(entry, stamps, change).expect("fixture prefix is legal");
                }
            });
            jobs
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
                Some(JobState::AwaitingApproval),
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
            at(prefix).modify(id, |entry, stamps| {
                let before = entry.state();
                let applied = apply(entry, stamps, &change);
                assert_eq!(applied.is_some(), expected.is_some(), "{case}");
                assert_eq!(entry.state(), expected.unwrap_or(before), "{case}");
            });
        }
        // A restart reports through delivery unless the interruption kept its waiter;
        // a question always does.
        for (case, prefix, change, background) in [
            ("completed", &completed[..], Advance(Running), true),
            ("interrupted", &interrupted[..], Advance(Running), false),
            ("asked", &running[..], Ask(question("q")), true),
        ] {
            at(prefix).modify(id, |entry, stamps| {
                entry.background = false;
                entry.acknowledge();
                apply(entry, stamps, &change).unwrap();
                assert_eq!(entry.background, background, "{case}");
                assert!(entry.unacknowledged(), "{case}");
            });
        }
    }

    /// A job's envelope and the result its run saved.
    pub(crate) struct Hydrated {
        pub(crate) envelope: JobEnvelope,
        pub(crate) output: Option<Value>,
    }

    impl std::ops::Deref for Hydrated {
        type Target = JobEnvelope;
        fn deref(&self) -> &JobEnvelope {
            &self.envelope
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

        /// The job's envelope once it ends, or asks while unacknowledged; with
        /// `claim`, its delivery is acknowledged.
        pub(crate) async fn wait(&self, id: JobId, claim: bool) -> Result<Hydrated, JobError> {
            let ready = |entry: &JobFacts| {
                !entry.suspended()
                    && (entry.end().is_some() || entry.asks_owner() && entry.unacknowledged())
            };
            let (envelope, presented) = self.until_job(id, ready).await?;
            if claim {
                self.acknowledge(presented).await?;
            }
            self.hydrated(envelope).await
        }

        /// `envelope` with the result its run saved.
        pub(crate) async fn hydrated(&self, envelope: JobEnvelope) -> Result<Hydrated, JobError> {
            let output = self.saved_result(&envelope).await?;
            Ok(Hydrated { envelope, output })
        }

        /// Job `id`'s envelope, [`hydrated`](Self::hydrated).
        pub(crate) async fn test_snapshot(&self, id: JobId) -> Result<Hydrated, JobError> {
            self.hydrated(self.snapshot(id).await?).await
        }

        /// Commit a child's assistant history and publish its visible text in one
        /// step: see [`Jobs::message_plan`].
        pub(crate) async fn commit_child_message(
            &self,
            child: &AgentId,
            job: JobId,
            message: crate::session::Message,
            text: String,
            follow: impl FnOnce(RecordSeq) -> Vec<(AgentId, SessionEvent)> + Send + 'static,
        ) -> Result<MessageSeq, JobError> {
            let child = child.clone();
            let commit = move |state: &CoreState| {
                Jobs::message_plan(state, child, message, Some((job, &text)), follow, |_| {})
            };
            self.inner
                .store
                .commit_on([job], Command::Journal, commit)
                .await
        }

        /// Release `owner`'s suspended foreground jobs, as a redirect does.
        pub(crate) async fn test_release(&self, owner: &AgentId) {
            let owner = owner.clone();
            let release = move |state: &CoreState| {
                let released = state.jobs.releases(&owner, &[]);
                Ok::<_, SessionError>(Plan::journal(release_records(&released)))
            };
            self.store()
                .commit(Command::Journal, release)
                .await
                .unwrap();
        }

        pub(crate) fn has_pending(&self, owner: &AgentId) -> bool {
            self.facts(|jobs| jobs.has_pending(owner))
        }

        /// Acknowledge the job's current delivery, as a read presenting it does.
        pub(crate) async fn test_claim(&self, job: JobId) -> Result<(), JobError> {
            let delivery = self.entry(job, |entry| entry.delivery(job)).await?;
            self.acknowledge(delivery).await
        }

        /// A running agent job, called `tool`, whose resume handler answers `answer`.
        pub(super) async fn test_resumable(
            &self,
            agent: AgentId,
            tool: &str,
            answer: Value,
        ) -> JobId {
            let spec = JobSpec {
                accepts_input: true,
                role: JobRole::Agent,
                ..JobSpec::test(agent, tool)
            };
            let id = self.test_running(spec).await.into_test_id();
            self.test_install(id).await;
            let handler: ResumeHandler = Arc::new(move |_, _| {
                let answer = answer.clone();
                Box::pin(async move { Ok(ToolOutput::new(answer)) })
            });
            self.test_resume_with(id, handler);
            id
        }

        /// Install a child in running agent job `id`, as its call would.
        pub(crate) async fn test_install(&self, id: JobId) -> AgentId {
            let owner = self.metadata(id).await.unwrap().agent;
            let index = u32::try_from(id.get()).unwrap();
            let workspace = std::path::Path::new(".");
            crate::session::tests::start_child(self.store(), &owner, index, Some(id), workspace)
                .await
        }

        /// Run `handler` as the installed child of agent job `id` whenever the
        /// job restarts, as the agent runtime would run it.
        pub(crate) fn test_resume_with(&self, id: JobId, handler: ResumeHandler) {
            let handlers: Arc<dyn Restarter> = HANDLERS.clone();
            self.restart_with(Arc::downgrade(&handlers));
            let mut registered = HANDLERS.0.lock().unwrap_or_else(PoisonError::into_inner);
            registered.insert((self.store().id(), id), handler);
        }

        pub(super) async fn test_finish(&self, job: JobId, output: Value) {
            self.test_complete(job, ToolOutput::new(output)).await;
        }

        pub(super) async fn test_complete(&self, job: JobId, output: ToolOutput) {
            self.finish(job, JobOutcome::Completed(output))
                .await
                .unwrap();
        }

        pub(super) async fn test_append(&self, agent: AgentId, event: SessionEvent) -> RecordSeq {
            self.store().append(agent, event).await.unwrap().sequence
        }

        /// The jobs as a reopen opens them, before it settles what was running.
        /// Live commits and opening share one fold, so what the journal records of
        /// every job, delivery stamps included, is the same both ways.
        pub(crate) fn test_replay(&self) -> Self {
            let replay = Self::new(self.store().replica());
            let live = self.facts(journaled);
            assert_eq!(live, replay.facts(journaled), "live and replay disagree");
            replay
        }
    }

    /// What a host inspecting held job `id` is shown. A wait keyed on the job reads it
    /// held whenever it changes, and once unheld it changes no more.
    pub(crate) fn job_snapshot(state: &CoreState, id: JobId) -> Option<JobEnvelope> {
        Some(state.jobs.get(id)?.envelope(id))
    }

    /// What a host inspecting each job `owner` created is shown, for a wait keyed on
    /// `owner`, which reads no journal: as the journal left each job when it
    /// registered, then as each change to it, which journals under `owner`, leaves it.
    pub(crate) struct Shown {
        owner: AgentId,
        jobs: std::sync::Mutex<std::collections::BTreeMap<JobId, JobEnvelope>>,
    }

    impl Shown {
        pub(crate) fn read(journal: &crate::session::Journal, owner: AgentId) -> Self {
            let created = journal.created_by(&owner).expect("the journal reads");
            let settled = Settled::read(journal, &created).expect("the journal reads");
            let jobs = settled
                .into_iter()
                .map(|(id, entry)| (id, entry.envelope(id)));
            let jobs = std::sync::Mutex::new(jobs.collect());
            Self { owner, jobs }
        }

        /// The jobs in id order.
        pub(crate) fn jobs(&self, state: &CoreState) -> Vec<JobEnvelope> {
            let mut jobs = self.jobs.lock().unwrap();
            let held = state
                .jobs
                .iter()
                .filter(|(_, entry)| entry.agent == self.owner);
            jobs.extend(held.map(|(id, entry)| (id, entry.envelope(id))));
            jobs.values().cloned().collect()
        }
    }

    /// What the journal records of each job.
    fn journaled(jobs: &Jobs) -> Vec<String> {
        let mut facts = (jobs.iter())
            .map(|(id, entry)| entry.journaled(id))
            .collect::<Vec<_>>();
        facts.sort();
        facts
    }
}
