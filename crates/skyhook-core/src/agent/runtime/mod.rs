//! Shared session state and journal observation for provider-neutral agent runtimes.

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock, RwLock as StdRwLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
};

use futures_util::{StreamExt as _, future::join_all};
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
    provider::Provider,
    provider::profile::ModelProfile,
    provider::protocol::{
        AssistantContent, BlockContent, Message, ModelRequest, ResponseAssembler, ResponseChunk,
        SystemSegment, ToolCall, ToolResult, Usage, UserContent,
    },
    remote::{EmbeddedShimCatalog, RejectSensitivePrompts, RemoteManager, SensitivePromptHandler},
    session::{EventRecord, SessionError, SessionEvent, SessionStore},
    target::{TargetDefinition, TargetRegistry, TargetsConfig, import_ssh_targets},
    tool::builtins::{HostSkills, install_script_tool, register_coding_tools},
    tool::policy::{AllowAll, Policy},
    tool::policy::{Capability, CapabilitySet},
    tool::{ToolRegistry, ToolRegistryBuilder, executor::ToolExecutor},
};

pub use super::error::HarnessError;
use super::interaction::{QuestionHandler, RuntimeEvent};
use super::observation::RuntimeEvents;
use super::{AgentActivity, Observation};
use super::{TodoItem, TodoSnapshot, todo::TodoStore};

mod compact;
mod compaction;
mod context;
use context::AgentContext;
pub(super) use context::recorded_context;
mod prompt;
mod questions;
mod queue;
pub use queue::{
    PreparedQueuedPrompt, QueueConflict, QueuedPrompt, QueuedPromptCancellation,
    QueuedPromptCommit, QueuedPromptError, QueuedPromptIdentity, QueuedPromptRecovery,
    QueuedPromptToken, RecoveredQueuedPrompt, RecoveredQueuedPromptState,
};
mod tools;
mod wait;
use wait::AgentSender;

const AGENT_CHANNEL_CAPACITY: usize = 64;
/// Initial generation plus two reconnects. Compaction has its own additive budget.
mod builder;
mod dispatch;
mod driver;
#[cfg(test)]
mod durable_queue_tests;
mod lifecycle;
mod recovery;
mod session;
mod turn;
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
    max_child_depth: usize,
    capabilities: CapabilitySet,
    target_definitions: Vec<TargetDefinition>,
    shim_catalog: EmbeddedShimCatalog,
    sensitive_prompts: Arc<dyn SensitivePromptHandler>,
}

#[derive(Clone)]
pub struct SessionHandle {
    runtime: Arc<SessionRuntime>,
    root: AgentId,
    root_tx: AgentSender,
}

/// Options captured when a user submits a message, including queued messages.
#[derive(Clone, Debug, Default)]
pub struct PromptOptions {
    /// Configured model profile when this input is consumed. Omitted retains the
    /// agent's active model; later explicit queued selections can change it.
    pub model: Option<String>,
}

struct SessionRuntime {
    harness: Arc<HarnessInner>,
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
    queue_state: queue::QueueRuntimeState,
    child_counters: RwLock<HashMap<AgentId, u32>>,
    questions: Arc<questions::QuestionCoordinator>,
    usage: Mutex<Usage>,
    events: RuntimeEvents,
    // Fully replayed journal prefix, not the highest (possibly out-of-order) live event.
    caught_up_sequence: Mutex<u64>,
    #[cfg(test)]
    store_forwarding_gate: Arc<Mutex<()>>,
    shutting_down: std::sync::atomic::AtomicBool,
}

struct LiveAgent {
    model_profile: String,
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

/// Why an agent request ended without an answer. An interrupt stays typed so an
/// owner can recognise it without comparing rendered messages.
#[derive(Clone, Debug, PartialEq, Eq)]
enum RequestFailure {
    Interrupted,
    Failed(String),
}

impl From<&HarnessError> for RequestFailure {
    fn from(error: &HarnessError) -> Self {
        match error {
            HarnessError::Interrupted => Self::Interrupted,
            error => Self::Failed(error.to_string()),
        }
    }
}

impl std::fmt::Display for RequestFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Interrupted => HarnessError::Interrupted.fmt(f),
            Self::Failed(message) => f.write_str(message),
        }
    }
}

type RequestCompletion = oneshot::Sender<Result<String, RequestFailure>>;

enum AgentCommand {
    QueuedInputs(Vec<queue::QueuedInput>),
    Input {
        model: Option<String>,
        content: Vec<UserContent>,
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
}

struct AgentLoop {
    control: AgentControl,
    id: AgentId,
    owner_job: Option<JobId>,
    context: AgentContext,
    model_profile: String,
    location: crate::execution::ExecutionLocation,
    capabilities: CapabilitySet,
    rx: mpsc::Receiver<AgentCommand>,
}

struct TurnContext<'a> {
    agent: &'a AgentId,
    owner_job: Option<JobId>,
    cancellation: &'a CancellationToken,
    location: &'a crate::execution::ExecutionLocation,
    capabilities: &'a CapabilitySet,
}

impl SessionRuntime {
    pub(super) fn activity(&self, agent: &AgentId, activity: AgentActivity) {
        self.events.send(RuntimeEvent::Activity {
            agent: agent.clone(),
            activity,
        });
    }
}
impl SessionRuntime {
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
                            .agents
                            .read()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
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
}
impl SessionRuntime {
    pub(super) async fn commit(
        &self,
        agent: &AgentId,
        message: Message,
    ) -> Result<u64, SessionError> {
        let record = self
            .store
            .append(agent.clone(), SessionEvent::MessageCommitted { message })
            .await?;
        Ok(record.sequence)
    }
}
fn contains_images(messages: &[Message]) -> bool {
    messages.iter().any(|message| match message {
        Message::User(content) => content.iter().any(UserContent::is_image),
        Message::Tool(results) => results.iter().any(|result| !result.images.is_empty()),
        Message::Assistant(_) => false,
    })
}

#[cfg(test)]
mod tests {
    pub(super) use std::{
        collections::VecDeque,
        future::Future,
        pin::Pin,
        sync::{
            Mutex as StdMutex,
            atomic::{AtomicUsize, Ordering},
        },
        task::Poll,
        time::Duration,
    };

    pub(super) use futures_util::stream;

    pub(super) use super::*;
    pub(super) use crate::{
        agent::Question,
        provider::protocol::{ItemKind, ReplayEnvelope, StopReason, ToolCall, events_for_content},
        provider::{ProviderContext, ProviderError, ProviderFuture, ResponseStream},
    };

    pub(super) type Requests = Arc<StdMutex<Vec<ModelRequest>>>;

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
                    _: String,
                ) -> Result<Box<dyn crate::provider::ProviderContext>, crate::provider::ProviderError> {
                    Ok(Box::new(self.clone()))
                }
            }
        )+};
    }
    pub(crate) use {cloned_provider, count, events};

    #[derive(Clone)]
    pub(super) struct ScriptedProvider {
        pub(super) responses: Arc<StdMutex<VecDeque<Vec<ResponseChunk>>>>,
        pub(super) requests: Requests,
    }

    pub(super) fn scripted_provider(
        requests: &Requests,
        responses: impl IntoIterator<Item = Vec<ResponseChunk>>,
    ) -> Arc<ScriptedProvider> {
        Arc::new(ScriptedProvider {
            requests: requests.clone(),
            responses: Arc::new(StdMutex::new(responses.into_iter().collect())),
        })
    }

    /// A session over `root` (sessions in `root/sessions`) answering from a script.
    pub(super) async fn scripted_session(
        responses: impl IntoIterator<Item = Vec<ResponseChunk>>,
    ) -> (tempfile::TempDir, Requests, SessionHandle) {
        let root = tempfile::tempdir().unwrap();
        let requests = Requests::default();
        let provider = scripted_provider(&requests, responses);
        let harness = test_harness(root.path(), &root.path().join("sessions"), provider).await;
        let session = harness.new_session().await.unwrap();
        (root, requests, session)
    }

    #[derive(Clone)]
    pub(super) struct HangingProvider;

    #[derive(Clone)]
    pub(super) struct BlockingFirstProvider {
        pub(super) calls: Arc<AtomicUsize>,
        pub(super) requests: Requests,
        pub(super) release: Arc<tokio::sync::Semaphore>,
    }

    pub(super) struct RecordingQuestions {
        pub(super) batches: Arc<StdMutex<Vec<Vec<Question>>>>,
        pub(super) answer: serde_json::Value,
    }

    pub(super) struct GatedResponse {
        pub(super) release: Pin<Box<dyn Future<Output = ()> + Send>>,
        pub(super) released: bool,
        pub(super) events: VecDeque<ResponseChunk>,
    }

    impl futures_util::Stream for GatedResponse {
        type Item = Result<ResponseChunk, crate::provider::ProviderError>;

        fn poll_next(
            mut self: Pin<&mut Self>,
            context: &mut std::task::Context<'_>,
        ) -> Poll<Option<Result<ResponseChunk, crate::provider::ProviderError>>> {
            if self.events.is_empty() {
                return Poll::Ready(None);
            }
            if !self.released {
                if self.release.as_mut().poll(context).is_pending() {
                    return Poll::Pending;
                }
                self.released = true;
            }
            Poll::Ready(self.events.pop_front().map(Ok))
        }
    }

    cloned_provider!(HangingProvider, ScriptedProvider, BlockingFirstProvider);

    impl ProviderContext for HangingProvider {
        fn invoke(&mut self, _request: ModelRequest) -> ProviderFuture {
            Box::pin(async { Ok(Box::pin(stream::pending()) as ResponseStream) })
        }
    }

    impl ProviderContext for ScriptedProvider {
        fn invoke(&mut self, request: ModelRequest) -> ProviderFuture {
            self.requests.lock().unwrap().push(request);
            let chunks = self.responses.lock().unwrap().pop_front();
            let chunks = chunks.expect("scripted provider response");
            Box::pin(async move {
                Ok(Box::pin(stream::iter(chunks.into_iter().map(Ok))) as ResponseStream)
            })
        }
    }

    impl ProviderContext for BlockingFirstProvider {
        fn invoke(&mut self, request: ModelRequest) -> ProviderFuture {
            self.requests.lock().unwrap().push(request);
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let release = self.release.clone();
            Box::pin(async move {
                let response: ResponseStream = if call == 0 {
                    Box::pin(GatedResponse {
                        release: Box::pin(async move {
                            release.acquire_owned().await.unwrap().forget();
                        }),
                        released: false,
                        events: answer("initial").into(),
                    })
                } else {
                    Box::pin(stream::iter(answer("jobs handled").into_iter().map(Ok)))
                };
                Ok(response)
            })
        }
    }

    impl QuestionHandler for RecordingQuestions {
        fn ask(&self, _agent: AgentId, questions: Vec<Question>) -> crate::agent::QuestionFuture {
            self.batches.lock().unwrap().push(questions);
            let answer = self.answer.clone();
            Box::pin(async move { Ok(answer) })
        }
    }

    pub(super) fn request_runtime_state(request: &ModelRequest) -> &str {
        let [Message::User(content)] = request.tail.as_slice() else {
            panic!("expected transient runtime state at the end of the request");
        };
        let (prefix, suffix) = ("<skyhook_state>\n", "\n</skyhook_state>");
        let state = content.iter().find_map(|content| match content {
            UserContent::Runtime { text } => text.strip_prefix(prefix)?.strip_suffix(suffix),
            _ => None,
        });
        state.expect("request has a compact runtime state block")
    }

    pub(super) fn request_history(request: &ModelRequest) -> &[Message] {
        &request.history
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

    pub(super) async fn bounded<T>(future: impl Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(10), future)
            .await
            .expect("test synchronization timed out")
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
                tokio::task::yield_now().await;
            }
        })
        .await
    }

    pub(super) async fn terminal(session: &SessionHandle, job: JobId) -> crate::job::JobEnvelope {
        until(session, job, |job| job.state.is_terminal()).await;
        session.runtime.jobs.wait(job, None, true).await.unwrap()
    }

    /// A running Agent-role job owning retained children and their questions.
    pub(super) async fn owner(session: &SessionHandle) -> JobId {
        let jobs = &session.runtime.jobs;
        let spec = crate::job::JobSpec {
            accepts_input: true,
            role: crate::job::JobRole::Agent,
            ..crate::job::JobSpec::test(session.root.clone(), "agent")
        };
        let job = jobs.create(spec).await.unwrap().into_test_id();
        let running = crate::job::JobState::Running;
        jobs.transition(job, running).await.unwrap();
        job
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
        tokio::time::timeout(Duration::from_secs(5), async {
            while runtime.strong_count() != 0 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("session runtime released after shutdown");
    }

    pub(super) fn response(items: Vec<AssistantContent>) -> Vec<ResponseChunk> {
        let stop_reason = if items.iter().any(|item| item.kind == ItemKind::ToolCall) {
            StopReason::ToolUse
        } else {
            StopReason::EndTurn
        };
        let mut events = events_for_content(&items);
        events.push(ResponseChunk::ResponseEnded { stop_reason });
        events
    }

    pub(super) fn answer(text: impl Into<String>) -> Vec<ResponseChunk> {
        response(vec![AssistantContent::text("answer", 0, text)])
    }

    /// A tool call item `tool-{position}` invoking `name` with call id `id`.
    pub(super) fn tool_call(
        position: usize,
        id: &str,
        name: &str,
        arguments: serde_json::Value,
    ) -> AssistantContent {
        let call = ToolCall::new(id, name, arguments).unwrap();
        AssistantContent::tool_call(format!("tool-{position}"), position, call)
    }

    pub(super) fn todo(text: &str, status: crate::agent::TodoStatus) -> TodoItem {
        TodoItem {
            text: text.to_owned(),
            status,
        }
    }

    /// Prepare each prompt, then enqueue every prepared one as one batch;
    /// results keep input order.
    pub(super) async fn enqueue_prompts(
        session: &SessionHandle,
        prompts: Vec<QueuedPrompt>,
    ) -> Vec<Result<QueuedPromptCommit, QueuedPromptError>> {
        let (mut prepared, mut results) = (Vec::new(), Vec::new());
        for prompt in prompts {
            results.push(match session.prepare_queued_prompt(prompt).await {
                Ok(permit) => {
                    prepared.push(permit);
                    None
                }
                Err(error) => Some(Err(error)),
            });
        }
        let mut receipts = session
            .enqueue_prepared_queued_prompts(prepared)
            .await
            .into_iter();
        let next = |result: Option<_>| result.unwrap_or_else(|| receipts.next().unwrap());
        results.into_iter().map(next).collect()
    }

    pub(super) fn model(profile: &str) -> PromptOptions {
        PromptOptions {
            model: Some(profile.to_owned()),
        }
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
        let store = SessionStore::create_ephemeral(&root.join("sessions"))
            .await
            .unwrap();
        let targets = harness.inner.target_definitions.clone();
        let started = SessionEvent::SessionStarted { targets };
        let started = store.append(AgentId::root(store.id()), started).await;
        let runtime = SessionRuntime::build(harness.inner.clone(), store, vec![started.unwrap()])
            .await
            .unwrap();
        runtime.start_root(None).await.unwrap()
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
        let mut last = 0;
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
