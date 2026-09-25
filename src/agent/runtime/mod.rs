//! Shared session state and journal observation for provider-neutral agent runtimes.
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock, RwLock as StdRwLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
};

use futures_util::StreamExt as _;
use serde_json::json;
use tokio::{
    fs,
    sync::{Mutex, RwLock, broadcast, mpsc, oneshot},
};

use crate::{
    identity::{AgentId, JobId, SessionId},
    job::{CancellationToken, JobManager},
    mcp::{McpServerConfig, manager::McpManager},
    media::{MAX_IMAGE_BYTES, MAX_IMAGE_BYTES_PER_SUBMISSION, MAX_IMAGES_PER_SUBMISSION},
    provider::profile::ModelProfile,
    provider::protocol::{
        CutReason, LiveResponse, ModelRequest, Outcome, Step as LiveStep, SystemSegment, ToolCall,
        ToolResult, Usage, visible_text,
    },
    provider::{Provider, ProviderError},
    remote::{EmbeddedShimCatalog, RejectSensitivePrompts, RemoteManager, SensitivePromptHandler},
    session::{
        EventRecord, Message, MessageSeq, RecordSeq, RequestSeq, SessionError, SessionEvent,
        SessionStore, UserPart,
    },
    target::{TargetDefinition, TargetRegistry, TargetsConfig},
    tool::builtins::{HostSkills, install_script_tool, register_coding_tools},
    tool::policy::{AllowAll, Policy},
    tool::policy::{Capability, CapabilitySet, Mode},
    tool::{ToolRegistry, ToolRegistryBuilder, executor::ToolExecutor},
};

pub use super::error::{HarnessError, TurnFailure};
use super::interaction::{QuestionHandler, RuntimeEvent};
use super::observation::RuntimeEvents;
use super::{AgentActivity, Observation, Settlement};
use super::{TodoItem, todo::TodoStore};

mod compact;
mod compaction;
mod context;
use context::AgentContext;
pub(super) use context::recorded_context;
mod prompt;
mod questions;
mod queue;
pub use queue::{QueuedPrompt, QueuedPromptCancellation, QueuedPromptReceipt};
mod tools;
mod wait;
use wait::AgentSender;

const AGENT_CHANNEL_CAPACITY: usize = 64;
/// Initial generation plus two reconnects. Compaction has its own additive budget.
mod builder;
mod dispatch;
use dispatch::CreatedCall;
mod driver;
mod lifecycle;
mod recovery;
mod session;
mod state;
mod turn;
pub(crate) use builder::Catalog;
pub use builder::HarnessBuilder;

#[derive(Clone)]
pub struct Harness {
    inner: Arc<HarnessInner>,
}

struct HarnessInner {
    workspace: PathBuf,
    session_root: PathBuf,
    providers: BTreeMap<String, Arc<dyn Provider>>,
    model_profiles: BTreeMap<String, ModelProfile>,
    default_model_profile: String,
    policy: Arc<dyn Policy>,
    questions: Option<Arc<dyn QuestionHandler>>,
    extra_tools: ToolRegistry,
    mcp: BTreeMap<String, McpServerConfig>,
    instructions: Vec<String>,
    skills: HostSkills,
    /// Instruction and skill discovery diagnostics.
    discovery_warnings: Vec<String>,
    max_child_depth: usize,
    /// The most a new session can hold.
    capabilities: CapabilitySet,
    modes: indexmap::IndexMap<String, Mode>,
    /// The mode a new session starts in; None when no modes are configured.
    mode: Option<String>,
    target_definitions: Vec<TargetDefinition>,
    shim_catalog: EmbeddedShimCatalog,
    sensitive_prompts: Arc<dyn SensitivePromptHandler>,
}

impl SessionRuntime {
    /// What `mode` grants under the ceiling. Interaction follows the host, not the mode.
    fn mode_capabilities(&self, name: &str) -> Result<CapabilitySet, HarnessError> {
        let mode = self.modes.get(name);
        let mode = mode.ok_or_else(|| HarnessError::UnknownMode(name.to_owned()))?;
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
    root_tx: AgentSender,
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
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.answer.is_none() && self.children_resumed == 0
    }
}

/// Model and mode selections carried by a submitted or queued message, or by a
/// continued turn. Issued by [`SessionHandle::selection`], so it only ever names a
/// model profile and a mode the issuing runtime instance has.
#[derive(Clone, Debug, Default)]
pub struct Selection {
    /// Model profile when this input is consumed. Omitted retains the agent's
    /// active model, which for a refusal would deterministically refuse again;
    /// later explicit queued selections can change it.
    pub model: Option<SessionModel>,
    /// Mode when this input is consumed: the root agent's capabilities, tools and
    /// mode instructions from then on. Omitted retains the active mode.
    pub mode: Option<SessionMode>,
}

/// A model profile one runtime instance admitted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionModel {
    runtime: RuntimeInstance,
    name: String,
}

/// A mode one runtime instance admitted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionMode {
    runtime: RuntimeInstance,
    name: String,
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
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl SessionMode {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

struct SessionRuntime {
    instance: RuntimeInstance,
    harness: Arc<HarnessInner>,
    /// The most any agent can hold: the harness ceiling, never more than the session
    /// started with. A mode grants the root agent a subset.
    capabilities: CapabilitySet,
    /// The harness modes, except that a mode the session has used keeps the
    /// definition it was used under.
    modes: indexmap::IndexMap<String, Mode>,
    store: SessionStore,
    jobs: JobManager,
    todos: TodoStore,
    executor: ToolExecutor,
    mcp: Arc<McpManager>,
    startup_warnings: Vec<String>,
    // Keeps the script tool's weak executor lookup alive without an executor/registry cycle.
    _executor_slot: Arc<OnceLock<ToolExecutor>>,
    router: crate::target::TargetRouter,
    agents: StdRwLock<HashMap<AgentId, LiveAgent>>,
    child_counters: RwLock<HashMap<AgentId, u32>>,
    questions: Arc<questions::QuestionCoordinator>,
    usage: Mutex<Usage>,
    events: RuntimeEvents,
    // Fully replayed journal prefix, not the highest (possibly out-of-order) live event.
    caught_up_sequence: Mutex<RecordSeq>,
    #[cfg(test)]
    store_forwarding_gate: Arc<Mutex<()>>,
    shutting_down: std::sync::atomic::AtomicBool,
}

struct LiveAgent {
    model_profile: String,
    capabilities: CapabilitySet,
    sender: AgentSender,
    cancellation: CancellationToken,
    control: AgentControl,
    available_depth: usize,
}

// Constructed before registration and handed directly to both the map entry
// and the loop. Running a loop never rediscovers its own control authority by ID.
#[derive(Clone)]
struct AgentControl {
    retryable_interrupt: Arc<AtomicBool>,
    completion_gate: Arc<Mutex<bool>>,
}

impl AgentControl {
    fn new() -> Self {
        Self {
            retryable_interrupt: Arc::new(AtomicBool::new(false)),
            completion_gate: Arc::new(Mutex::new(true)),
        }
    }
}

type RequestCompletion = oneshot::Sender<Result<String, TurnFailure>>;

enum AgentCommand {
    QueuedInputs(Vec<queue::QueuedInput>),
    Input {
        options: Selection,
        content: Vec<UserPart>,
        done: Option<RequestCompletion>,
    },
    JobsReady,
    Shutdown,
}

struct AgentLaunch {
    id: AgentId,
    owner_job: Option<JobId>,
    model_profile: String,
    todos: Option<Vec<TodoItem>>,
    available_depth: usize,
    location: crate::execution::ExecutionLocation,
    /// The agent's mode; a child has one only when its parent chose it.
    mode: Option<String>,
    /// The most this agent may hold: its mode's set, or its parent's current set.
    capabilities: CapabilitySet,
}

struct AgentLoop {
    control: AgentControl,
    id: AgentId,
    owner_job: Option<JobId>,
    context: AgentContext,
    location: crate::execution::ExecutionLocation,
    settings: AgentSettings,
    rx: mpsc::Receiver<AgentCommand>,
}

/// What an agent currently runs under; a consumed input may change any of it.
struct AgentSettings {
    model_profile: String,
    /// The agent's mode, when it runs in one. Only the root's can change.
    mode: Option<String>,
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

type LiveAgents = HashMap<AgentId, LiveAgent>;

impl SessionRuntime {
    /// The live agents; a poisoned lock still holds a consistent map.
    fn agents(&self) -> std::sync::RwLockReadGuard<'_, LiveAgents> {
        let agents = self.agents.read();
        agents.unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn agents_mut(&self) -> std::sync::RwLockWriteGuard<'_, LiveAgents> {
        let agents = self.agents.write();
        agents.unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(super) fn activity(&self, agent: &AgentId, activity: AgentActivity) {
        self.events.send(RuntimeEvent::Activity {
            agent: agent.clone(),
            activity,
        });
    }

    pub(super) async fn catch_up_store_events(&self) {
        // Serialize catchups so the cursor advances only after the entire prefix is
        // published. Live forwarding may race ahead; RuntimeEvents deduplicates it.
        let mut sequence = self.caught_up_sequence.lock().await;
        self.store
            .visit_records_after(*sequence, |records| {
                for record in records {
                    self.events
                        .send(RuntimeEvent::Record(Box::new(record.clone())));
                    *sequence = record.sequence;
                }
            })
            .await;
    }

    pub(super) fn forward_store_events(self: &Arc<Self>) {
        let mut source = self.store.subscribe();
        #[cfg(test)]
        let forwarding_gate = self.store_forwarding_gate.clone();
        let events = self.events.clone();
        let runtime = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                let received = source.recv().await;
                #[cfg(test)]
                let _forwarding = forwarding_gate.lock().await;
                match received {
                    Ok(record) => {
                        events.send(RuntimeEvent::Record(Box::new(record)));
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let Some(runtime) = runtime.upgrade() else {
                            break;
                        };
                        runtime.catch_up_store_events().await;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    pub(super) fn forward_job_completions(self: &Arc<Self>) {
        let mut completions = self.jobs.subscribe_completions();
        let runtime = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                let completion = match completions.recv().await {
                    Ok(completion) => completion,
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let Some(runtime) = runtime.upgrade() else {
                            break;
                        };
                        let agents = runtime
                            .agents()
                            .iter()
                            .map(|(id, agent)| (id.clone(), agent.sender.clone()))
                            .collect::<Vec<_>>();
                        for (id, sender) in agents {
                            if runtime.jobs.has_pending(&id).await {
                                sender.jobs_ready();
                            }
                        }
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                let Some(runtime) = runtime.upgrade() else {
                    break;
                };
                if let Some(sender) = runtime.agent_sender(&completion.agent) {
                    sender.jobs_ready();
                }
            }
        });
    }

    pub(super) async fn commit(
        &self,
        agent: &AgentId,
        message: Message,
    ) -> Result<MessageSeq, SessionError> {
        let record = self
            .store
            .append(agent.clone(), SessionEvent::MessageCommitted { message })
            .await?;
        Ok(record.sequence.message())
    }
}
fn contains_images(messages: &[Message]) -> bool {
    messages.iter().any(|message| match message {
        Message::User(content) => content.iter().any(UserPart::is_image),
        Message::Tool(results) => results.iter().any(|result| !result.images.is_empty()),
        Message::Assistant(_) => false,
    })
}

#[cfg(test)]
mod tests {
    /// What a provider received, as distinct from what the session journals.
    pub(crate) use crate::provider::protocol::{Message as Sent, UserContent as SentPart};
    pub(super) use std::{
        future::Future,
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
        provider::protocol::{
            AssistantItem, Binding, BlockId, BlockRef, ContextId, ItemId, ItemKind, Provenance,
            Replay, ResponseEvent, Scope, ToolCall,
        },
        provider::{ProviderContext, ResponseStream},
    };

    /// A request as the scripted context that received it saw it.
    #[derive(Clone, Debug, PartialEq)]
    pub(super) struct Served {
        pub(super) context: ContextId,
        pub(super) request: ModelRequest,
    }

    impl std::ops::Deref for Served {
        type Target = ModelRequest;
        fn deref(&self) -> &ModelRequest {
            &self.request
        }
    }

    pub(super) type Requests = Arc<StdMutex<Vec<Served>>>;

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
        served: StdMutex<Vec<(usize, ModelRequest)>>,
        changed: tokio::sync::Notify,
        pub(super) requests: Requests,
        pub(super) opened: AtomicUsize,
        this: Weak<Script>,
    }

    impl Script {
        pub(super) fn new(steps: impl IntoIterator<Item = Step>, requests: &Requests) -> Arc<Self> {
            Arc::new_cyclic(|this| Self {
                steps: steps.into_iter().collect(),
                served: StdMutex::default(),
                changed: tokio::sync::Notify::new(),
                requests: requests.clone(),
                opened: AtomicUsize::new(0),
                this: this.clone(),
            })
        }

        /// The request that `step` served, once it arrives.
        pub(super) async fn request(&self, step: usize) -> ModelRequest {
            bounded(async {
                loop {
                    let notified = self.changed.notified();
                    // Clone only the match, in a scope that ends the guard before awaiting.
                    let found = {
                        let served = self.served.lock().unwrap();
                        served.iter().find(|(id, _)| *id == step).cloned()
                    };
                    if let Some((_, request)) = found {
                        return request;
                    }
                    notified.await;
                }
            })
            .await
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
            served.iter().any(|(seen, _)| *seen >= step)
        }

        pub(super) fn remaining(&self) -> usize {
            self.steps.len() - self.served.lock().unwrap().len()
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

    impl ProviderContext for ScriptContext {
        fn invoke(&mut self, request: ModelRequest) -> ResponseStream {
            let script = self.0.clone();
            let step = {
                let mut served = script.served.lock().unwrap();
                let free = |(index, step): &(usize, &Step)| {
                    step.model.is_none_or(|model| model == request.model)
                        && !served.iter().any(|(seen, _)| seen == index)
                };
                let (step, _) = script
                    .steps
                    .iter()
                    .enumerate()
                    .find(free)
                    .expect("scripted response");
                script.requests.lock().unwrap().push(Served {
                    context: self.1.clone(),
                    request: request.clone(),
                });
                served.push((step, request));
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
        let runtime = SessionRuntime::build(harness.inner.clone(), store, Vec::new());
        runtime.await.unwrap().start_root(None).await.unwrap()
    }

    #[derive(Clone)]
    pub(super) struct HangingProvider;

    #[derive(Default)]
    pub(super) struct RecordingQuestions {
        pub(super) batches: Arc<StdMutex<Vec<Vec<Question>>>>,
        pub(super) backgrounds: Arc<StdMutex<Vec<bool>>>,
        pub(super) answer: serde_json::Value,
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
                self.batches.lock().unwrap().push(questions);
                Ok(self.answer.clone())
            };
            Box::pin(async move { answer })
        }
    }

    /// Every message of a request, as the JSON text assertions search.
    pub(super) fn rendered(request: &ModelRequest) -> String {
        serde_json::to_string(&request.messages().collect::<Vec<_>>()).unwrap()
    }

    /// A leaf child of the root on the default test profile, in the session workspace.
    pub(super) fn child_launch(
        session: &SessionHandle,
        id: AgentId,
        owner_job: Option<JobId>,
    ) -> AgentLaunch {
        let workspace = session.runtime.harness.workspace.clone();
        AgentLaunch {
            id,
            owner_job,
            model_profile: session.runtime.harness.default_model_profile.clone(),
            todos: None,
            available_depth: 0,
            location: crate::execution::ExecutionLocation::root(workspace),
            mode: None,
            capabilities: session.runtime.capabilities.clone(),
        }
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
        let [crate::provider::protocol::Message::User(content)] = request.tail.as_slice() else {
            panic!("expected transient runtime state at the end of the request");
        };
        let state = content.iter().find_map(|content| match content {
            crate::provider::protocol::UserContent::Runtime { text } => Some(text.clone()),
            _ => None,
        });
        let state = state.expect("request has a runtime state block");
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
        let profile = ModelProfile::new("test", "test", None, 128_000, 16_384, supports_images);
        HarnessBuilder::new(workspace)
            .session_root(sessions)
            .provider("test", provider)
            .model_profile("test", profile)
            .default_model_profile("test")
    }

    #[track_caller]
    pub(super) fn bounded<T>(future: impl Future<Output = T>) -> impl Future<Output = T> {
        let caller = std::panic::Location::caller();
        async move {
            tokio::time::timeout(Duration::from_secs(10), future)
                .await
                .unwrap_or_else(|_| panic!("test synchronization timed out at {caller}"))
        }
    }

    /// One step of a polling loop. A timer rather than a yield, because paused
    /// time only advances while every task is idle.
    pub(super) async fn poll() {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    pub(super) async fn until(
        session: &SessionHandle,
        job: JobId,
        predicate: impl Fn(&crate::job::JobEnvelope) -> bool,
    ) -> crate::job::JobEnvelope {
        bounded(async {
            loop {
                let snapshot = session.runtime.jobs.snapshot(job).await.unwrap();
                if predicate(&snapshot) {
                    return snapshot;
                }
                poll().await;
            }
        })
        .await
    }

    pub(super) async fn terminal(session: &SessionHandle, job: JobId) -> crate::job::JobEnvelope {
        until(session, job, |job| job.state.is_terminal()).await;
        session.runtime.jobs.wait(job, None, true).await.unwrap()
    }

    /// Journal the start of the root's child `index`, as the agent tool would.
    pub(super) async fn start_child(
        session: &SessionHandle,
        index: u32,
        owner: Option<JobId>,
    ) -> AgentId {
        let workspace = session.runtime.harness.workspace.clone();
        let store = &session.runtime.store;
        crate::session::fixture::start_child(store, &session.root, index, owner, &workspace).await
    }

    /// A running Agent-role job owning retained children and their questions.
    pub(super) async fn owner(session: &SessionHandle) -> JobId {
        let jobs = &session.runtime.jobs;
        let spec = crate::job::JobSpec {
            accepts_input: true,
            role: crate::job::JobRole::Agent,
            ..crate::job::JobSpec::test(session.root.clone(), "agent")
        };
        jobs.test_running(spec).await.into_test_id()
    }

    // A separate parent inbox keeps the idle root from consuming a child's responses.
    pub(super) struct QuietRoot<'a> {
        session: &'a SessionHandle,
        _rx: mpsc::Receiver<AgentCommand>,
    }

    impl Drop for QuietRoot<'_> {
        fn drop(&mut self) {
            set_root_sender(self.session, self.session.root_tx.clone());
        }
    }

    fn set_root_sender(session: &SessionHandle, sender: AgentSender) {
        let mut agents = session.runtime.agents.write().unwrap();
        agents.get_mut(&session.root).unwrap().sender = sender;
    }

    pub(super) fn quiet_root(session: &SessionHandle) -> QuietRoot<'_> {
        let (tx, rx) = mpsc::channel(AGENT_CHANNEL_CAPACITY);
        set_root_sender(session, AgentSender::new(tx));
        QuietRoot { session, _rx: rx }
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
        let runtime = Arc::downgrade(&session.runtime);
        session.shutdown().await.unwrap();
        drop(session);
        // The journal lock is released with the runtime; a resume needs it.
        bounded(async {
            while runtime.strong_count() != 0 {
                poll().await;
            }
        })
        .await;
    }

    pub(super) fn usage(input_tokens: u64, cached_input_tokens: u64, output_tokens: u64) -> Usage {
        Usage {
            input_tokens,
            cached_input_tokens,
            output_tokens,
        }
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

    /// A provisional text delta for block `block` of item `item`.
    pub(super) fn delta(item: &str, block: &str, kind: ItemKind, text: &str) -> ResponseEvent {
        ResponseEvent::Delta {
            block: BlockRef {
                item: ItemId::try_from(item.to_owned()).unwrap(),
                block: BlockId::try_from(block.to_owned()).unwrap(),
            },
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
            text: text.to_owned(),
            status,
        }
    }

    /// Enqueue one batch and await every receipt; results keep input order.
    pub(super) async fn enqueue_prompts(
        session: &SessionHandle,
        prompts: Vec<QueuedPrompt>,
    ) -> Vec<Result<(), HarnessError>> {
        let mut results = Vec::new();
        for receipt in session.enqueue_prompts(prompts).await {
            results.push(receipt.await.unwrap_or(Err(HarnessError::AgentStopped)));
        }
        results
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

    async fn observation_session(root: &Path) -> SessionHandle {
        let harness = test_harness(root, &root.join("sessions"), Arc::new(HangingProvider)).await;
        ephemeral_session(&harness).await
    }

    #[tokio::test]
    async fn observation_catchup_does_not_skip_gaps_before_live_records() {
        let root = tempfile::tempdir().unwrap();
        let session = observation_session(root.path()).await;
        session.observe().await;
        // Pause forwarding rather than relying on scheduler luck.
        let forwarding = session.runtime.store_forwarding_gate.lock().await;
        let store = &session.runtime.store;
        let first = store.append(session.root.clone(), SessionEvent::AgentInterrupted);
        let first = first.await.unwrap();
        let second = store.append(session.root.clone(), SessionEvent::AgentCompleted);
        let second = second.await.unwrap();
        let events = &session.runtime.events;
        events.send(RuntimeEvent::Record(Box::new(second.clone())));
        let snapshot = session.runtime.events.observe().snapshot;
        assert!(!snapshot.records.contains_key(&first.sequence));
        assert_eq!(snapshot.records.get(&second.sequence), Some(&second));
        drop(forwarding);
        let (left, right) = tokio::join!(session.observe(), session.observe());
        for snapshot in [&left.snapshot, &right.snapshot] {
            assert_eq!(snapshot.records.get(&first.sequence), Some(&first));
            assert_eq!(snapshot.records.get(&second.sequence), Some(&second));
        }
        assert_eq!(left.snapshot.revision, right.snapshot.revision);
        let caught_up = *session.runtime.caught_up_sequence.lock().await;
        assert_eq!(caught_up, second.sequence);
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn observation_forwarder_catches_up_after_store_lag() {
        let root = tempfile::tempdir().unwrap();
        let session = observation_session(root.path()).await;
        let mut observation = session.observe().await;
        let initial_count = observation.snapshot.records.len();
        // 600 gated appends overflow the 512-slot channel, forcing the Lagged catch-up.
        let forwarding = session.runtime.store_forwarding_gate.lock().await;
        let mut last = RecordSeq::default();
        for _ in 0..600 {
            let record = session
                .runtime
                .store
                .append(session.root.clone(), SessionEvent::AgentInterrupted);
            last = record.await.unwrap().sequence;
        }
        drop(forwarding);
        bounded(async {
            while observation.snapshot.records.len() < initial_count + 600 {
                let update = observation.updates.recv().await.unwrap();
                observation.snapshot.apply(update);
            }
        })
        .await;
        assert_eq!(*session.runtime.caught_up_sequence.lock().await, last);
        let records = observation
            .snapshot
            .records
            .into_values()
            .collect::<Vec<_>>();
        assert_eq!(records, session.runtime.store.records().await);
        session.shutdown().await.unwrap();
    }
}
