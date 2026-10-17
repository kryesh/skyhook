//! Shared session state and journal observation for provider-neutral agent runtimes.
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock, Weak, atomic::Ordering},
};

use futures_util::StreamExt as _;
use serde_json::json;
use tokio::{fs, sync::oneshot};

use crate::{
    identity::{AgentId, JobId, SessionId},
    job::{CancelScope, CancellationToken, JobManager},
    mcp::{McpServerConfig, manager::McpManager},
    media::{MAX_IMAGE_BYTES, MAX_IMAGE_BYTES_PER_SUBMISSION, MAX_IMAGES_PER_SUBMISSION},
    provider::profile::{ModelName, ModelProfile, ModelRef, ProviderName},
    provider::protocol::{
        CutReason, LiveResponse, Outcome, Step as LiveStep, SystemSegment, ToolCall, ToolResult,
        Usage, visible_text,
    },
    provider::{Provider, ProviderError},
    remote::{EmbeddedShimCatalog, RejectSensitivePrompts, RemoteManager, SensitivePromptHandler},
    session::{
        Command, CoreState, EventRecord, Message, MessageSeq, Plan, RecordSeq, SessionError,
        SessionEvent, SessionStore, UserPart,
    },
    target::{TargetDefinition, TargetRegistry, TargetsConfig},
    tool::builtins::{HostSkills, install_script_tool, register_coding_tools},
    tool::policy::{AllowAll, Policy},
    tool::policy::{Capability, CapabilitySet, Mode, ModeName},
    tool::{ToolRegistry, ToolRegistryBuilder, executor::ToolExecutor},
};

use super::TodoItem;
pub use super::error::{Failure, HarnessError, TurnFailure};
use super::interaction::{QuestionHandler, RuntimeEvent};
use super::{Observation, Settlement};

#[cfg(test)]
use crate::provider::protocol::ModelRequest;

mod agents;
#[cfg(test)]
pub(crate) use agents::stopped_turn;
use agents::{Activation, AppliedMode, Flow, Invocation, RecordedContract, Turn, Was};
pub(crate) use agents::{Agents, activities};
use modes::Selected;
mod compact;
mod context;
mod modes;
use context::AgentContext;
mod prompt;
mod questions;
mod queue;
pub use queue::{
    QueuedPrompt, QueuedPromptCancellation, QueuedPromptCommitted, QueuedPromptReceipt,
};
mod builder;
mod dispatch;
mod tools;
mod wait;
use dispatch::CreatedCall;
mod driver;
mod lifecycle;
mod recovery;
mod reopen;
mod session;
mod state;
mod turn;
pub use builder::HarnessBuilder;
pub(crate) use builder::ModelEntry;

#[derive(Clone)]
pub struct Harness {
    inner: Arc<HarnessInner>,
}

struct HarnessInner {
    workspace: PathBuf,
    session_root: PathBuf,
    models: indexmap::IndexMap<ModelRef, ModelEntry>,
    default_model: ModelRef,
    policy: Arc<dyn Policy>,
    questions: Option<Arc<dyn QuestionHandler>>,
    extra_tools: ToolRegistry,
    mcp: BTreeMap<String, McpServerConfig>,
    instructions: Vec<String>,
    skills: HostSkills,
    /// Model configuration, instruction, and skill diagnostics.
    warnings: Vec<String>,
    max_child_depth: usize,
    coalescing: wait::Coalescing,
    /// The most a new session can hold.
    capabilities: CapabilitySet,
    modes: indexmap::IndexMap<ModeName, Mode>,
    /// The mode a new session starts in; None when no modes are configured.
    mode: Option<ModeName>,
    target_definitions: Vec<TargetDefinition>,
    shim_catalog: EmbeddedShimCatalog,
    sensitive_prompts: Arc<dyn SensitivePromptHandler>,
}

impl SessionRuntime {
    /// What `mode` grants under the ceiling. Interaction follows the host, not the mode.
    fn mode_capabilities(&self, name: &ModeName) -> Result<CapabilitySet, HarnessError> {
        let mode = self.modes.get(name);
        let mode = mode.ok_or_else(|| HarnessError::UnknownMode(name.clone()))?;
        Ok(self.granted_by(mode))
    }

    /// A declared mode's grant: its capabilities within the session ceiling, and
    /// interactivity wherever the session has it.
    fn granted_by(&self, mode: &Mode) -> CapabilitySet {
        let mut capabilities = &mode.capabilities.iter().copied().collect() & &self.capabilities;
        if self.capabilities.contains(Capability::Interactive) {
            capabilities.insert(Capability::Interactive);
        }
        capabilities
    }
}

#[derive(Clone)]
pub struct SessionHandle {
    runtime: Arc<SessionRuntime>,
    root: AgentId,
    _open: Arc<Open>,
}

/// What every handle holds: once the last is gone, the session closes as a crash
/// would leave it, and everything it runs stops.
struct Open(SessionStore, crate::job::Supervisor);

impl Drop for Open {
    fn drop(&mut self) {
        self.0.close_soon();
        self.1.stop();
    }
}

/// What a continue attempt actually did. A child-only continue leaves a live or
/// waiting root untouched, so a caller cannot assume the root turn ran.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ContinueOutcome {
    /// Answer produced when the root turn itself was continued.
    pub answer: Option<String>,
    /// Retained child agents restarted by this call.
    pub children_resumed: usize,
    /// Whether a requested model or mode change was applied. Only the continued root
    /// turn can adopt one; resumed children keep their own.
    pub selection_applied: bool,
}

impl ContinueOutcome {
    /// True when there was nothing to continue.
    pub fn is_empty(&self) -> bool {
        self.answer.is_none() && self.children_resumed == 0
    }
}

/// Model and mode selections carried by a submitted or queued message, or by a
/// continued turn. Issued by [`SessionHandle::selection`], so it only ever names a
/// `provider/model` and a mode the issuing runtime instance has.
#[derive(Clone, Debug, Default)]
pub struct Selection {
    /// Model when this input is consumed. Omitted retains the agent's
    /// active model, which for a refusal would deterministically refuse again;
    /// later explicit queued selections can change it.
    pub model: Option<SessionModel>,
    /// Mode when this input is consumed: the root agent's capabilities, tools and
    /// mode instructions from then on. Omitted retains the active mode.
    pub mode: Option<SessionMode>,
}

/// A model one runtime instance admitted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionModel {
    runtime: RuntimeInstance,
    name: ModelRef,
}

/// A mode one runtime instance admitted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionMode {
    runtime: RuntimeInstance,
    name: ModeName,
}

/// Process-local identity of one runtime instance and its immutable catalog. A
/// resumed or reloaded session is a new instance, whatever its durable id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RuntimeInstance(u64);

impl RuntimeInstance {
    fn next() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        Self(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
    }
}

impl SessionModel {
    pub fn name(&self) -> &ModelRef {
        &self.name
    }
}

struct SessionRuntime {
    instance: RuntimeInstance,
    harness: Arc<HarnessInner>,
    /// The most any agent can hold: the live harness ceiling, whatever the session
    /// held when it last ran. A mode grants the root agent a subset.
    capabilities: CapabilitySet,
    /// The harness modes. Editions the session recorded earlier are only journaled.
    modes: indexmap::IndexMap<ModeName, Mode>,
    store: SessionStore,
    jobs: JobManager,
    executor: ToolExecutor,
    mcp: Arc<McpManager>,
    startup_warnings: Vec<String>,
    // Keeps the script tool's weak executor lookup alive without an executor/registry cycle.
    _executor_slot: Arc<OnceLock<ToolExecutor>>,
    router: crate::target::TargetRouter,
    questions: Arc<questions::QuestionCoordinator>,
}

type ChildCompletion = oneshot::Sender<Result<String, TurnFailure>>;

/// Who waits for an input's answer: the session's own prompt, which receives the
/// full error, or an owning job's waiter.
enum RequestCompletion {
    Root(oneshot::Sender<Result<String, HarnessError>>),
    Child(ChildCompletion),
}

impl RequestCompletion {
    fn settle(self, result: Result<String, HarnessError>) {
        match self {
            Self::Root(done) => drop(done.send(result)),
            Self::Child(done) => drop(done.send(result.map_err(|error| (&error).into()))),
        }
    }
}

enum AgentCommand {
    QueuedInputs(Vec<queue::QueuedInput>),
    Input {
        options: Selection,
        content: Vec<UserPart>,
        done: Option<RequestCompletion>,
    },
    /// Adopt `options` without input or a turn, as work that runs the root but
    /// commits no message, such as a host script, must before it starts.
    Select {
        options: Selection,
        done: oneshot::Sender<Result<(), HarnessError>>,
    },
    /// Try again, as `continue` asks, the batch a failed turn held, claimed
    /// with the selection that commits it.
    Retry {
        batch: queue::Batch,
        done: RequestCompletion,
    },
}

impl AgentCommand {
    /// Settle whoever waits on this command with `error`.
    fn refuse(self, error: impl Fn() -> HarnessError) {
        match self {
            Self::QueuedInputs(inputs) => {
                inputs.into_iter().for_each(|input| input.reject(error()))
            }
            Self::Input {
                done: Some(done), ..
            } => done.settle(Err(error())),
            Self::Input { done: None, .. } => {}
            Self::Select { done, .. } => {
                let _ = done.send(Err(error()));
            }
            Self::Retry { batch, done } => {
                batch.hand_back();
                done.settle(Err(error()));
            }
        }
    }
}

#[derive(Clone)]
enum AgentLaunch {
    /// An agent journaled as it starts.
    New {
        model: ModelRef,
        todos: Option<Vec<TodoItem>>,
        available_depth: usize,
        location: crate::execution::ExecutionLocation,
        /// The agent's mode; a child has one only when its parent chose it.
        mode: Option<AppliedMode>,
        /// The most this agent may hold: its mode's set, or its parent's current set.
        capabilities: CapabilitySet,
    },
    /// An agent that started before, under its journaled contract narrowed to the
    /// live configuration.
    Resume(Resumed),
}

/// The turn state an agent resumes in.
#[derive(Clone)]
enum Resumed {
    Idle,
    /// Its journaled turn stopped short of an answer, for this reason.
    Parked(TurnFailure),
}

struct AgentLoop {
    id: AgentId,
    owner_job: Option<JobId>,
    context: AgentContext,
    location: crate::execution::ExecutionLocation,
    settings: AgentSettings,
}

/// What an agent currently runs under; a consumed input may change any of it.
struct AgentSettings {
    /// The agent's mode, when it runs in one. Only the root's can change.
    mode: Option<AppliedMode>,
    capabilities: CapabilitySet,
}

struct TurnContext<'a> {
    agent: &'a AgentId,
    owner_job: Option<JobId>,
    cancellation: &'a CancellationToken,
    location: &'a crate::execution::ExecutionLocation,
    /// The agent's set as of this request; a consumed mode change replaces it.
    capabilities: CapabilitySet,
}

impl TurnContext<'_> {
    fn diagnostic_viewer(&self) -> crate::tool::diagnostic::DiagnosticViewer<'_> {
        crate::tool::diagnostic::DiagnosticViewer::new(&self.capabilities, self.location)
    }
}

/// Why the root takes no input: the session's refusal once its phase refuses
/// input, which is what ends the root's loop, or else its agent stopped.
fn refused_input(phase: &crate::session::SessionPhase) -> HarnessError {
    let refused = phase.admits(Command::Submit);
    refused.map_or_else(Into::into, |_| HarnessError::AgentStopped)
}

/// Why input whose reply was dropped was not taken.
async fn dropped_input(store: &SessionStore) -> HarnessError {
    store.behind(|state| refused_input(&state.phase)).await
}

impl SessionRuntime {
    /// Read the running agents as the core last left them.
    fn agents<T>(&self, read: impl FnOnce(&Agents) -> T) -> T {
        self.store.read(|state| read(&state.agents))
    }

    /// Journal the `events` of `agent`'s turn as `command`, given the state they
    /// commit to: a model request, an attempt at one, or the state it carries.
    /// Only a busy turn does: one an interrupt stopped, held or cancelled is
    /// refused, and ends interrupted. The last event's record.
    pub(super) async fn journal_turn(
        &self,
        agent: &AgentId,
        command: Command,
        events: impl FnOnce(&CoreState) -> Vec<SessionEvent> + Send + 'static,
    ) -> Result<RecordSeq, HarnessError> {
        let agent = agent.clone();
        let journal = move |state: &CoreState| {
            if !state.agents.busy(&agent) {
                return Err(HarnessError::Interrupted);
            }
            let entries = events(state)
                .into_iter()
                .map(|event| (agent.clone(), event));
            let journaled = |_: &mut CoreState, records: &[EventRecord]| {
                records.last().expect("a turn journals events").sequence
            };
            Ok(Plan::new(entries.collect(), journaled))
        };
        self.store.commit(command, journal).await
    }

    /// Journal `agent`'s model request, made under `context`, with a record of
    /// that context unless it is the agent's latest, in one commit: requests
    /// sharing a record keep their reported prompts comparable.
    pub(super) async fn journal_request(
        &self,
        agent: &AgentId,
        context: crate::session::ModelContext,
        requested: impl FnOnce(RecordSeq) -> SessionEvent + Send + 'static,
    ) -> Result<crate::session::RequestSeq, HarnessError> {
        let author = agent.clone();
        let events = move |state: &CoreState| {
            let latest = (state.agents.contract(&author))
                .and_then(|contract| contract.context.as_ref())
                .filter(|(_, latest)| *latest == context);
            match latest {
                Some((recorded, _)) => vec![requested(*recorded)],
                None => {
                    let recorded = state.next_sequence();
                    vec![SessionEvent::ModelContext { context }, requested(recorded)]
                }
            }
        };
        let requested = self.journal_turn(agent, Command::Request, events).await?;
        Ok(requested.request())
    }

    /// Commit a call's result with the acknowledgement of the delivery it
    /// reports, in one transaction: a result that never commits leaves it pending.
    async fn commit_result(
        &self,
        agent: &AgentId,
        result: crate::provider::protocol::ToolResult,
        acknowledges: Option<crate::job::Delivery>,
    ) -> Result<(), HarnessError> {
        let message = Message::Tool(result);
        let mut entries = vec![(agent.clone(), SessionEvent::MessageCommitted { message })];
        let commit = move |state: &CoreState| {
            let claim = acknowledges
                .map(|delivery| delivery.claim(state))
                .transpose()?;
            entries.extend(claim.flatten());
            Ok::<_, crate::job::JobError>(Plan::journal(entries))
        };
        Ok(self.store.commit(Command::Journal, commit).await?)
    }
}

#[cfg(test)]
mod tests {
    /// What a provider received, as distinct from what the session journals.
    pub(crate) use crate::provider::protocol::{Message as Sent, UserContent as SentPart};

    /// A committed conversation message as the provider receives it.
    pub(crate) fn sent(message: &Message) -> Sent {
        use crate::provider::protocol::{Entry, StateLifetime};
        let Entry::Message(sent) = message.entry(StateLifetime::Persisted) else {
            panic!("a conversation message")
        };
        sent
    }
    pub(super) use std::{
        sync::{
            Mutex as StdMutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    pub(super) use futures_util::{TryStreamExt, stream};

    pub(super) use super::*;
    pub(super) use crate::{
        agent::Question,
        provider::codec::common::tests::envelope,
        provider::protocol::{
            AssistantItem, Binding, BlockRef, ContextId, ItemKind, ReplayFormat, ResponseEvent,
            ToolCall,
        },
        provider::{ProviderContext, ResponseStream},
        session::tests::{on_commit, usage},
        tests::bounded,
    };

    /// A request as the scripted context that received it saw it.
    #[derive(Debug, PartialEq)]
    pub(super) struct Served {
        pub(super) context_id: ContextId,
        pub(super) request: ModelRequest,
    }

    impl std::ops::Deref for Served {
        type Target = ModelRequest;
        fn deref(&self) -> &ModelRequest {
            &self.request
        }
    }

    pub(super) type Requests = Arc<StdMutex<Vec<Arc<Served>>>>;

    /// Counts records (or record references) whose event matches a pattern.
    macro_rules! count {
        ($records:expr, $pattern:pat $(if $guard:expr)?) => {
            IntoIterator::into_iter($records)
                .filter(|record| matches!(&record.event, $pattern $(if $guard)?))
                .count()
        };
    }
    /// Collects a value bound by a pattern from each matching record event.
    macro_rules! events {
        ($records:expr, $pattern:pat $(if $guard:expr)? => $value:expr) => {
            IntoIterator::into_iter($records)
                .filter_map(|record| match &record.event {
                    $pattern $(if $guard)? => Some($value),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
    }
    /// Implements `Provider` for cloneable fixture contexts.
    macro_rules! cloned_provider {
        ($($provider:ty),+) => {$(
            impl crate::provider::Provider for $provider {
                fn open_context(
                    &self,
                    _: crate::provider::protocol::ContextId,
                ) -> Result<Box<dyn crate::provider::ProviderContext>, crate::provider::ProviderError> {
                    Ok(Box::new(self.clone()))
                }
            }
        )+};
    }
    pub(crate) use {count, events};

    type Events = Vec<Result<ResponseEvent, ProviderError>>;

    /// One scripted response, served to the first unserved request it matches.
    pub(super) struct Step {
        model: Option<&'static str>,
        response: StdMutex<Option<Result<Events, ProviderError>>>,
        gate: tokio::sync::Semaphore,
        midstream: bool,
    }

    impl Step {
        pub(super) fn stream(events: Events) -> Self {
            Self {
                model: None,
                response: StdMutex::new(Some(Ok(events))),
                gate: tokio::sync::Semaphore::new(1),
                midstream: false,
            }
        }

        pub(super) fn new(events: Vec<ResponseEvent>) -> Self {
            Self::stream(events.into_iter().map(Ok).collect())
        }

        /// The invocation itself fails, before any stream exists.
        pub(super) fn fail(error: ProviderError) -> Self {
            let step = Self::stream(Vec::new());
            *step.response.lock().unwrap() = Some(Err(error));
            step
        }

        /// Serve only requests for this model.
        pub(super) fn model(mut self, model: &'static str) -> Self {
            self.model = Some(model);
            self
        }

        /// Hold the response until `Script::release`.
        pub(super) fn gated(mut self) -> Self {
            self.gate = tokio::sync::Semaphore::new(0);
            self
        }

        /// Stream every event but the terminal one, then hold it until `Script::release`.
        pub(super) fn midstream(mut self) -> Self {
            self.midstream = true;
            self.gated()
        }
    }

    /// A provider answering from `steps`, shared by every context it opens.
    pub(super) struct Script {
        steps: Vec<Step>,
        served: StdMutex<Vec<Option<ModelRequest>>>,
        /// Midstream steps whose stream reached its held terminal event.
        held: StdMutex<Vec<usize>>,
        changed: tokio::sync::Notify,
        pub(super) requests: Requests,
        pub(super) opened: AtomicUsize,
        dropped: StdMutex<Vec<ContextId>>,
        this: Weak<Script>,
    }

    impl Script {
        pub(super) fn new(steps: impl IntoIterator<Item = Step>, requests: &Requests) -> Arc<Self> {
            let steps = steps.into_iter().collect::<Vec<_>>();
            let served = vec![None; steps.len()];
            Arc::new_cyclic(|this| Self {
                steps,
                served: StdMutex::new(served),
                held: StdMutex::default(),
                changed: tokio::sync::Notify::new(),
                requests: requests.clone(),
                opened: AtomicUsize::new(0),
                dropped: StdMutex::default(),
                this: this.clone(),
            })
        }

        /// What `found` yields, once it yields.
        async fn when<T>(&self, found: impl Fn(&Self) -> Option<T>) -> T {
            bounded(async {
                loop {
                    let notified = self.changed.notified();
                    if let Some(found) = found(self) {
                        return found;
                    }
                    notified.await;
                }
            })
            .await
        }

        /// The request that `step` served, once it arrives.
        pub(super) async fn request(&self, step: usize) -> ModelRequest {
            let served = |script: &Self| script.served.lock().unwrap()[step].clone();
            self.when(served).await
        }

        /// Once midstream `step` has streamed all but its held terminal event.
        pub(super) async fn held(&self, step: usize) {
            let held = |script: &Self| script.held.lock().unwrap().contains(&step).then_some(());
            self.when(held).await;
        }

        /// Replace the response of `step`, before it is served.
        pub(super) fn respond(&self, step: usize, events: Vec<ResponseEvent>) {
            let events = events.into_iter().map(Ok).collect();
            *self.steps[step].response.lock().unwrap() = Some(Ok(events));
        }

        pub(super) fn release(&self, step: usize) {
            self.steps[step].gate.add_permits(1);
        }

        /// Waits for a step's request, then lets its response through.
        pub(super) async fn pass(&self, step: usize) -> ModelRequest {
            let request = self.request(step).await;
            self.release(step);
            request
        }

        /// Whether any step at or after `step` has been requested.
        pub(super) fn requested_from(&self, step: usize) -> bool {
            let served = self.served.lock().unwrap();
            served.iter().skip(step).any(Option::is_some)
        }

        pub(super) fn remaining(&self) -> usize {
            self.served
                .lock()
                .unwrap()
                .iter()
                .filter(|request| request.is_none())
                .count()
        }

        pub(super) fn dropped(&self, context: &ContextId) -> bool {
            self.dropped.lock().unwrap().contains(context)
        }

        /// Once a context opened as `context` has been dropped.
        pub(super) async fn released(&self, context: &ContextId) {
            self.when(|script| script.dropped(context).then_some(()))
                .await;
        }
    }

    impl Provider for Script {
        fn open_context(
            &self,
            context: ContextId,
        ) -> Result<Box<dyn ProviderContext>, ProviderError> {
            self.opened.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(ScriptContext(
                self.this.upgrade().unwrap(),
                context,
            )))
        }
    }

    struct ScriptContext(Arc<Script>, ContextId);

    impl Drop for ScriptContext {
        fn drop(&mut self) {
            self.0.dropped.lock().unwrap().push(self.1.clone());
            self.0.changed.notify_waiters();
        }
    }

    impl ProviderContext for ScriptContext {
        fn invoke(&mut self, request: ModelRequest) -> ResponseStream {
            let script = self.0.clone();
            let step = {
                let mut served = script.served.lock().unwrap();
                let free = |(index, step): &(usize, &Step)| {
                    step.model
                        .is_none_or(|model| request.model.as_str() == model)
                        && served[*index].is_none()
                };
                let (step, _) = script
                    .steps
                    .iter()
                    .enumerate()
                    .find(free)
                    .expect("scripted response");
                script.requests.lock().unwrap().push(Arc::new(Served {
                    context_id: self.1.clone(),
                    request: request.clone(),
                }));
                served[step] = Some(request);
                step
            };
            script.changed.notify_waiters();
            let started = async move {
                let gate = async move |script: Arc<Script>| {
                    script.steps[step].gate.acquire().await.unwrap().forget();
                };
                let midstream = script.steps[step].midstream;
                if !midstream {
                    gate(script.clone()).await;
                }
                let mut events = script.steps[step]
                    .response
                    .lock()
                    .unwrap()
                    .take()
                    .unwrap()?;
                let split = if midstream {
                    events.len().saturating_sub(1)
                } else {
                    0
                };
                let rest = events.split_off(split);
                let held = stream::once(async move {
                    if midstream {
                        script.held.lock().unwrap().push(step);
                        script.changed.notify_waiters();
                        gate(script).await;
                    }
                    stream::iter(rest)
                });
                Ok::<_, ProviderError>(
                    Box::pin(stream::iter(events).chain(held.flatten())) as ResponseStream
                )
            };
            Box::pin(stream::once(started).try_flatten())
        }
    }

    pub(super) fn scripted_provider(
        requests: &Requests,
        responses: impl IntoIterator<Item = Vec<ResponseEvent>>,
    ) -> Arc<Script> {
        Script::new(responses.into_iter().map(Step::new), requests)
    }

    /// A session over `root` (sessions in `root/sessions`) answering from a script.
    pub(super) async fn scripted_session(
        responses: impl IntoIterator<Item = Vec<ResponseEvent>>,
    ) -> (tempfile::TempDir, Requests, SessionHandle) {
        let root = tempfile::tempdir().unwrap();
        let requests = Requests::default();
        let provider = scripted_provider(&requests, responses);
        let harness = test_harness(root.path(), &root.path().join("sessions"), provider).await;
        (root, requests, ephemeral_session(&harness).await)
    }

    /// A session whose journal is never reopened, so it skips durability.
    pub(super) async fn ephemeral_session(harness: &Harness) -> SessionHandle {
        let store = SessionStore::create_ephemeral(&harness.inner.session_root);
        let store = store.await.unwrap();
        let jobs = JobManager::new(store.clone());
        let open = Arc::new(Open(store.clone(), jobs.supervisor().clone()));
        let runtime = SessionRuntime::build(harness.inner.clone(), store, jobs);
        let runtime = runtime.await.unwrap();
        runtime
            .start_root(runtime.new_root().unwrap(), open)
            .await
            .unwrap()
    }

    #[derive(Clone)]
    pub(super) struct HangingProvider;

    #[derive(Default)]
    pub(super) struct RecordingQuestions {
        pub(super) batches: Arc<StdMutex<Vec<Vec<Question>>>>,
        pub(super) backgrounds: Arc<StdMutex<Vec<bool>>>,
        /// Every question's answer.
        pub(super) answer: String,
        /// Fail every batch, as a host dismissing it would.
        pub(super) cancel: bool,
    }

    cloned_provider!(HangingProvider);

    impl ProviderContext for HangingProvider {
        fn invoke(&mut self, _request: ModelRequest) -> ResponseStream {
            Box::pin(stream::pending())
        }
    }

    impl QuestionHandler for RecordingQuestions {
        fn ask(
            &self,
            _agent: AgentId,
            questions: Vec<Question>,
            background: bool,
        ) -> crate::agent::QuestionFuture {
            self.backgrounds.lock().unwrap().push(background);
            let answer = if self.cancel {
                Err(crate::agent::QuestionError::Failed(
                    "question cancelled".into(),
                ))
            } else {
                let answer = crate::agent::QuestionAnswer::Text(self.answer.clone());
                let reply = questions
                    .iter()
                    .map(|q| (q.id.clone(), answer.clone()))
                    .collect();
                self.batches.lock().unwrap().push(questions);
                Ok(reply)
            };
            Box::pin(async move { answer })
        }
    }

    /// Every message of a request, as the JSON text assertions search.
    pub(super) fn rendered(request: &ModelRequest) -> String {
        serde_json::to_string(&request.context).unwrap()
    }

    /// A leaf child of the root on the default test profile, in the session workspace.
    pub(super) fn child_launch(
        session: &SessionHandle,
        todos: Option<Vec<TodoItem>>,
    ) -> AgentLaunch {
        let workspace = session.runtime.harness.workspace.clone();
        AgentLaunch::New {
            model: session.runtime.harness.default_model.clone(),
            todos,
            available_depth: 0,
            location: crate::execution::ExecutionLocation::root(workspace),
            mode: None,
            capabilities: session.runtime.capabilities.clone(),
        }
    }

    /// Poll `future`, which stays pending throughout, until `ready` holds.
    pub(super) async fn pending_until<F: Future + ?Sized>(
        future: &mut std::pin::Pin<Box<F>>,
        mut ready: impl AsyncFnMut() -> bool,
    ) {
        bounded(async {
            loop {
                assert!(futures_util::poll!(future.as_mut()).is_pending());
                if ready().await {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
    }

    /// Every usage report the session journaled, as its requests' ledger folds it.
    pub(super) async fn journaled_usage(session: &SessionHandle) -> Usage {
        let ledger = session.observe().snapshot.ledger;
        let mut usage = Usage::default();
        for (_, request) in ledger.iter() {
            usage.accumulate(request.usage);
        }
        usage
    }

    pub(super) fn turn(session: &SessionHandle, agent: &AgentId) -> Turn {
        session
            .runtime
            .agents(|agents| agents.get(agent).unwrap().turn.clone())
    }

    pub(super) use super::agents::INTERRUPTED;

    /// Queue `command` for `agent`, as an owner or the host would.
    pub(super) async fn send(session: &SessionHandle, agent: &AgentId, command: AgentCommand) {
        let agent = agent.clone();
        let send = move |state: &mut CoreState| state.agents.send(&agent, command);
        assert!(session.runtime.store.live(send).await, "the agent runs");
    }

    impl SessionRuntime {
        /// Journal `message` as `agent`'s, whatever its turn.
        pub(super) async fn commit(
            &self,
            agent: &AgentId,
            message: Message,
        ) -> Result<MessageSeq, SessionError> {
            let event = SessionEvent::MessageCommitted { message };
            let committed = self.store.append(agent.clone(), event).await?;
            Ok(committed.sequence.message())
        }
    }

    /// How many commands `agent` has yet to take.
    pub(super) fn inbox(session: &SessionHandle, agent: &AgentId) -> usize {
        session
            .runtime
            .agents(|agents| agents.get(agent).unwrap().inbox.len())
    }

    /// Whether `agent` has deliveries pending.
    pub(super) fn pending(session: &SessionHandle, agent: &AgentId) -> bool {
        session
            .runtime
            .store
            .read(|state| state.jobs.has_pending(agent))
    }

    /// Wait until `agent` has deliveries pending.
    pub(super) async fn until_pending(session: &SessionHandle, agent: &AgentId) {
        let agent = agent.clone();
        found(session, move |state| {
            state.jobs.has_pending(&agent).then_some(())
        })
        .await;
    }

    /// Wait until input waits in `agent`'s inbox.
    pub(super) async fn until_queued(session: &SessionHandle, agent: &AgentId) {
        let agent = agent.clone();
        found(session, move |state| {
            (!state.agents.get(&agent)?.inbox.is_empty()).then_some(())
        })
        .await;
    }

    pub(super) fn assistant_commits(records: &[EventRecord]) -> usize {
        count!(
            records,
            SessionEvent::MessageCommitted {
                message: Message::Assistant(_)
            }
        )
    }

    /// The rendered runtime state at the end of the request, without its tags.
    pub(super) fn request_runtime_state(request: &ModelRequest) -> String {
        use crate::provider::protocol::{Entry, State, StateLifetime};
        let Some(Entry::State(State {
            text: state,
            lifetime: StateLifetime::Current,
        })) = request.context.last()
        else {
            panic!("expected the request's own state at its end");
        };
        let (prefix, suffix) = ("<skyhook_state>\n", "\n</skyhook_state>");
        state
            .strip_prefix(prefix)
            .and_then(|state| state.strip_suffix(suffix))
            .expect("rendered state is tagged")
            .to_owned()
    }

    pub(super) fn test_builder(
        workspace: &Path,
        sessions: &Path,
        provider: Arc<dyn Provider>,
        supports_images: bool,
    ) -> HarnessBuilder {
        let profile = ModelProfile {
            max_output: crate::tests::limit(16_384),
            ..crate::tests::profile("test", supports_images)
        };
        HarnessBuilder::new(workspace)
            .session_root(sessions)
            .coalescing(wait::Coalescing::NONE)
            .provider(provider_name("test"), provider, models([("test", profile)]))
            .default_model(model_ref("test"))
    }

    /// A test harness builder whose `test` provider also serves `entries`.
    pub(super) fn serving<const N: usize>(
        root: &Path,
        provider: Arc<dyn Provider>,
        entries: [(&str, ModelProfile); N],
    ) -> HarnessBuilder {
        test_builder(root, &root.join("sessions"), provider.clone(), false).provider(
            provider_name("test"),
            provider,
            models(entries),
        )
    }

    /// A session whose root is held on a child it interrupted mid-response, and
    /// the root's prompt; `after` scripts the requests that follow.
    pub(super) async fn held_on_interrupted_child(
        root: &Path,
        after: impl IntoIterator<Item = Step>,
    ) -> (
        Harness,
        SessionHandle,
        Arc<Script>,
        Requests,
        tokio::task::JoinHandle<Result<String, HarnessError>>,
        JobId,
    ) {
        let child = json!({"prompt": "child task", "depth": 0});
        let delegate = Step::new(response(vec![tool_call(0, "agent-0", "agent", child)]));
        let steps = [delegate, Step::new(Vec::new()).midstream()];
        let requests = Requests::default();
        let provider = Script::new(steps.into_iter().chain(after), &requests);
        let harness = test_harness(root, &root.join("sessions"), provider.clone()).await;
        let session = harness.new_session().await.unwrap();
        let parent = spawn_prompt(&session, "delegate");
        provider.request(1).await;
        root_waiting(&session).await;
        assert_eq!(session.interrupt().await, 2);
        assert_eq!(turn(&session, &session.root), Turn::Held);
        let job = session.runtime.jobs.list(&session.root)[0].id;
        (harness, session, provider, requests, parent, job)
    }

    /// Interrupt right behind the commit of the first record from here on that
    /// `matches` accepts; how many it stopped.
    pub(super) async fn interrupt_on(
        session: &SessionHandle,
        matches: fn(&SessionEvent) -> bool,
    ) -> impl Future<Output = usize> + use<> {
        let (interrupting, store) = (session.clone(), &session.runtime.store);
        on_commit(store, matches, move || async move {
            interrupting.interrupt().await
        })
        .await
    }

    /// The root's child 1, spawned under a running owner job.
    pub(super) async fn spawned_child(session: &SessionHandle) -> (AgentId, JobId) {
        let child = session.root.child(1);
        let owner = owner(session, false).await;
        let launch = child_launch(session, None);
        let spawned = session
            .runtime
            .spawn_agent(child.clone(), Some(owner), launch);
        spawned.await.unwrap();
        (child, owner)
    }

    pub(super) fn input(
        text: &str,
        options: Selection,
        done: Option<ChildCompletion>,
    ) -> AgentCommand {
        let content = vec![UserPart::Text { text: text.into() }];
        AgentCommand::Input {
            options,
            content,
            done: done.map(RequestCompletion::Child),
        }
    }

    pub(super) async fn child_input(
        session: &SessionHandle,
        child: &AgentId,
        done: Option<ChildCompletion>,
    ) {
        send(session, child, input("task", Selection::default(), done)).await;
    }

    /// Prompt the root from a task.
    pub(super) fn spawn_prompt(
        session: &SessionHandle,
        text: &'static str,
    ) -> tokio::task::JoinHandle<Result<String, HarnessError>> {
        let session = session.clone();
        tokio::spawn(async move { session.prompt(text).await })
    }

    /// Wait until `find` finds what it looks for in the session's core state.
    pub(super) async fn found<T: Send + 'static>(
        session: &SessionHandle,
        find: impl FnMut(&mut CoreState) -> Option<T> + Send + Sync + 'static,
    ) -> T {
        bounded(session.runtime.store.until(find)).await.unwrap()
    }

    /// Wait until `agent`'s activity is one `wanted` accepts.
    pub(super) async fn activity_is(
        session: &SessionHandle,
        agent: &AgentId,
        wanted: fn(&crate::agent::AgentActivity) -> bool,
    ) {
        let agent = agent.clone();
        let activity = move |state: &mut CoreState| {
            let activity = crate::agent::activities(state).remove(&agent)?;
            wanted(&activity.state).then_some(())
        };
        found(session, activity).await;
    }

    /// Wait until the root runs tools or waits on children.
    pub(super) async fn root_waiting(session: &SessionHandle) {
        use crate::agent::AgentActivity::{Tools, WaitingChildren};
        let waiting = |state: &_| matches!(state, Tools | WaitingChildren);
        activity_is(session, &session.root, waiting).await;
    }

    /// Job `job` once `predicate` accepts it.
    pub(super) async fn until(
        session: &SessionHandle,
        job: JobId,
        predicate: impl Fn(&crate::job::JobEnvelope) -> bool + Send + Sync + 'static,
    ) -> crate::job::JobEnvelope {
        found(session, move |state| {
            state.jobs.snapshot(job).filter(&predicate)
        })
        .await
    }

    /// The first of `owner`'s jobs that `find` accepts, once there is one.
    pub(super) async fn job_found(
        session: &SessionHandle,
        owner: &AgentId,
        find: impl Fn(&crate::job::JobEnvelope) -> bool + Send + Sync + 'static,
    ) -> JobId {
        let owner = owner.clone();
        let first = move |state: &mut CoreState| {
            let mut jobs = state.jobs.views().into_values();
            jobs.find(|job| job.agent == owner && find(job))
                .map(|job| job.id)
        };
        found(session, first).await
    }

    pub(super) async fn terminal(
        session: &SessionHandle,
        job: JobId,
    ) -> crate::job::tests::Hydrated {
        until(session, job, |job| job.state.is_terminal()).await;
        session.runtime.jobs.wait(job, true).await.unwrap()
    }

    /// Journal the start of the root's child `index`, as the agent tool would.
    pub(super) async fn start_child(
        session: &SessionHandle,
        index: u32,
        owner: Option<JobId>,
    ) -> AgentId {
        let workspace = session.runtime.harness.workspace.clone();
        let store = &session.runtime.store;
        crate::session::tests::start_child(store, &session.root, index, owner, &workspace).await
    }

    /// A running Agent-role job owning retained children and their questions.
    pub(super) async fn owner(session: &SessionHandle, background: bool) -> JobId {
        let jobs = &session.runtime.jobs;
        let spec = crate::job::JobSpec {
            accepts_input: true,
            background,
            role: crate::job::JobRole::Agent,
            ..crate::job::JobSpec::test(session.root.clone(), "agent")
        };
        jobs.test_running(spec).await.into_test_id()
    }

    /// Parks the root, so while it lives the idle root leaves a child's responses
    /// pending rather than consuming them.
    pub(super) struct QuietRoot<'a>(&'a SessionHandle);

    impl Drop for QuietRoot<'_> {
        fn drop(&mut self) {
            set_root_turn(self.0, Turn::Idle);
        }
    }

    pub(super) fn set_root_turn(session: &SessionHandle, turn: Turn) {
        let root = session.root.clone();
        let set = move |state: &mut CoreState| {
            // A failed session runs no agent.
            if let Some(facts) = state.agents.get_mut(&root) {
                facts.set_turn(turn);
            }
        };
        session.runtime.store.enqueue(set);
    }

    pub(super) fn quiet_root(session: &SessionHandle) -> QuietRoot<'_> {
        set_root_turn(session, Turn::Parked(TurnFailure::Cancelled));
        QuietRoot(session)
    }

    pub(super) async fn test_harness(
        workspace: &Path,
        sessions: &Path,
        provider: Arc<dyn Provider>,
    ) -> Harness {
        test_builder(workspace, sessions, provider, false)
            .build()
            .await
            .unwrap()
    }

    pub(super) async fn shutdown_session(session: SessionHandle) {
        bounded(session.shutdown()).await.unwrap();
        session.close().await.unwrap();
    }

    pub(super) fn mode_name(name: &str) -> ModeName {
        name.parse().unwrap()
    }

    /// A model under the `test` provider.
    pub(super) fn model_ref(name: &str) -> ModelRef {
        format!("test/{name}").parse().unwrap()
    }

    pub(super) fn provider_name(name: &str) -> ProviderName {
        name.parse().unwrap()
    }

    pub(super) fn models<const N: usize>(
        entries: [(&str, ModelProfile); N],
    ) -> [(ModelName, ModelProfile); N] {
        entries.map(|(name, profile)| (name.parse().unwrap(), profile))
    }

    /// A normal finish: tool use when any item is a call, otherwise an answer.
    pub(super) fn response(items: Vec<AssistantItem>) -> Vec<ResponseEvent> {
        vec![ResponseEvent::End(
            crate::provider::protocol::Completion::finished(items).unwrap(),
        )]
    }

    /// An abnormal end keeping `items`, which must not include calls.
    pub(super) fn cut(items: Vec<AssistantItem>, reason: CutReason) -> Vec<ResponseEvent> {
        vec![ResponseEvent::End(
            crate::provider::protocol::Completion::cut(items, reason).unwrap(),
        )]
    }

    pub(super) fn answer(text: impl Into<String>) -> Vec<ResponseEvent> {
        response(vec![AssistantItem::text("answer", 0, text)])
    }

    pub(super) async fn next_recovery(
        events: &mut tokio::sync::broadcast::Receiver<crate::agent::ObservedEvent>,
    ) -> crate::session::EventRecord {
        bounded(async {
            loop {
                if let RuntimeEvent::Record(record) = events.recv().await.unwrap().event
                    && matches!(
                        record.event,
                        SessionEvent::ModelFailed {
                            failure: crate::session::AttemptFailure::Retrying { .. },
                            ..
                        }
                    )
                {
                    return *record;
                }
            }
        })
        .await
    }

    /// A provisional delta into the single block of item `item`.
    pub(super) fn delta(item: &str, kind: ItemKind, text: &str) -> ResponseEvent {
        ResponseEvent::Delta {
            block: BlockRef::single(item),
            kind,
            text: text.into(),
        }
    }

    /// A tool call item `tool-{position}` invoking `name` with call id `id`.
    pub(super) fn tool_call(
        position: u32,
        id: &str,
        name: &str,
        arguments: serde_json::Value,
    ) -> AssistantItem {
        let call = ToolCall::new(id, name, arguments).unwrap();
        AssistantItem::tool_call(format!("tool-{position}"), position, call)
    }

    pub(super) fn todo(text: &str, status: crate::agent::TodoStatus) -> TodoItem {
        TodoItem {
            text: text.parse().unwrap(),
            status,
        }
    }

    /// Enqueue one batch and await every receipt; results keep input order.
    pub(super) async fn enqueue_prompts(
        session: &SessionHandle,
        prompts: Vec<QueuedPrompt>,
    ) -> Vec<Result<queue::QueuedPromptCommitted, HarnessError>> {
        let mut results = Vec::new();
        for receipt in session.enqueue_prompts(prompts).await {
            results.push(receipt.await);
        }
        results
    }

    pub(super) fn recoverable() -> ProviderError {
        crate::provider::ProviderErrorKind::Transport.error("scripted connection lost")
    }

    /// Run `operation`, letting each retried failure's backoff elapse once it is
    /// journaled: what a retry does is under test, not how long it waits.
    pub(super) async fn retrying<T>(
        session: &SessionHandle,
        operation: impl Future<Output = T>,
    ) -> T {
        let mut updates = session.observe().updates;
        let mut operation = std::pin::pin!(operation);
        loop {
            tokio::select! {
                output = &mut operation => return output,
                update = updates.recv() => {
                    if let RuntimeEvent::Record(record) = update.expect("the observer keeps up").event {
                        elapse_backoff(&record).await;
                    }
                }
            }
        }
    }

    /// Let the backoff a retried failure `record` waits out elapse.
    pub(super) async fn elapse_backoff(record: &EventRecord) {
        let backoff = match &record.event {
            SessionEvent::ModelFailed {
                failure: crate::session::AttemptFailure::Retrying { delay, .. },
                ..
            } => Duration::from_millis(delay.millis()),
            // The record does not say which attempt failed: the longest backoff.
            SessionEvent::CompactionFailed { error, .. } if error.retryable() => {
                compact::RETRY_BACKOFF * u32::from(compact::MAX_COMPACTION_ATTEMPTS)
            }
            _ => return,
        };
        crate::tests::elapse(backoff).await;
    }

    /// A valid, otherwise empty compaction continuation.
    pub(super) fn summary_json() -> serde_json::Value {
        json!({
            "objective": "Continue the user's task.", "user_instructions": [],
            "session_rules": [], "plan": [], "resumption_point": "Continue the user's task.",
            "completed_work": [], "findings": [], "decisions": [], "open_issues": [],
            "next_actions": [], "running_work": [], "recovery_details": [], "jobs": [],
            "additional_context": [], "todo_reconciliation": [], "todos": []
        })
    }
}
