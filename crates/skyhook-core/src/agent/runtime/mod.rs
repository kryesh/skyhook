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
pub use queue::{QueuedPrompt, QueuedPromptToken};
mod tools;
mod wait;
use wait::AgentSender;

const AGENT_CHANNEL_CAPACITY: usize = 64;
/// Initial generation plus two reconnects. Compaction has its own additive budget.
mod builder;
mod dispatch;
mod driver;
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
    enqueue_preparation: Arc<Mutex<()>>,
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
    child_counters: RwLock<HashMap<AgentId, u32>>,
    questions: Arc<questions::QuestionCoordinator>,
    usage: Mutex<Usage>,
    events: RuntimeEvents,
    // Fully replayed journal prefix, not the highest (possibly out-of-order) live event.
    caught_up_sequence: Mutex<u64>,
    shutting_down: std::sync::atomic::AtomicBool,
}

struct LiveAgent {
    model_profile: String,
    sender: AgentSender,
    cancellation: CancellationToken,
    retryable_interrupt: Arc<AtomicBool>,
    available_depth: usize,
    completion_gate: Arc<Mutex<bool>>,
}

enum AgentCommand {
    QueuedInputs(Vec<queue::QueuedInput>),
    Input {
        model: Option<String>,
        content: Vec<UserContent>,
        done: Option<oneshot::Sender<Result<String, String>>>,
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
        let events = self.events.clone();
        let runtime = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                match source.recv().await {
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
        Message::User(content) => content
            .iter()
            .any(|item| matches!(item, UserContent::Image { .. })),
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
        provider::protocol::{
            ContentDelta, ItemKind, ReplayEnvelope, StopReason, ToolCall, events_for_content,
        },
        provider::{ProviderContext, ProviderError, ProviderFuture, ResponseStream},
    };

    #[derive(Clone)]
    pub(super) struct ScriptedProvider {
        pub(super) responses: Arc<StdMutex<VecDeque<Vec<ResponseChunk>>>>,
        pub(super) requests: Arc<StdMutex<Vec<ModelRequest>>>,
    }

    pub(super) fn scripted_provider(
        requests: &Arc<StdMutex<Vec<ModelRequest>>>,
        responses: impl IntoIterator<Item = Vec<ResponseChunk>>,
    ) -> Arc<ScriptedProvider> {
        Arc::new(ScriptedProvider {
            requests: requests.clone(),
            responses: Arc::new(StdMutex::new(responses.into_iter().collect())),
        })
    }

    #[derive(Clone)]
    pub(super) struct HangingProvider;

    #[derive(Clone)]
    pub(super) struct BlockingFirstProvider {
        pub(super) calls: Arc<AtomicUsize>,
        pub(super) requests: Arc<StdMutex<Vec<ModelRequest>>>,
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

    impl Provider for HangingProvider {
        fn open_context(
            &self,
            _correlation: String,
        ) -> Result<Box<dyn ProviderContext>, ProviderError> {
            Ok(Box::new(self.clone()))
        }
    }

    impl ProviderContext for HangingProvider {
        fn invoke(&mut self, _request: ModelRequest) -> ProviderFuture {
            Box::pin(async { Ok(Box::pin(stream::pending()) as ResponseStream) })
        }
    }

    impl Provider for ScriptedProvider {
        fn open_context(
            &self,
            _correlation: String,
        ) -> Result<Box<dyn ProviderContext>, ProviderError> {
            Ok(Box::new(self.clone()))
        }
    }

    impl ProviderContext for ScriptedProvider {
        fn invoke(&mut self, request: ModelRequest) -> ProviderFuture {
            self.requests.lock().unwrap().push(request);
            let chunks = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted provider response");
            Box::pin(async move {
                Ok(Box::pin(stream::iter(chunks.into_iter().map(Ok))) as ResponseStream)
            })
        }
    }

    impl Provider for BlockingFirstProvider {
        fn open_context(
            &self,
            _correlation: String,
        ) -> Result<Box<dyn ProviderContext>, ProviderError> {
            Ok(Box::new(self.clone()))
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
                            let permit = release.acquire_owned().await.unwrap();
                            permit.forget();
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
        let Some(Message::User(content)) = request.messages.last() else {
            panic!("expected transient runtime state at the end of the request");
        };
        content
            .iter()
            .find_map(|content| match content {
                UserContent::Runtime { text } => text
                    .strip_prefix("<skyhook_state>\n")
                    .and_then(|text| text.strip_suffix("\n</skyhook_state>")),
                _ => None,
            })
            .expect("request has a compact runtime state block")
    }

    pub(super) fn request_history(request: &ModelRequest) -> &[Message] {
        let (_, history) = request
            .messages
            .split_last()
            .expect("runtime request has state");
        history
    }

    pub(super) fn test_builder(
        workspace: &Path,
        sessions: &Path,
        provider: Arc<dyn Provider>,
    ) -> HarnessBuilder {
        HarnessBuilder::new(workspace)
            .session_root(sessions)
            .provider("test", provider)
            .model_profile(
                "test",
                ModelProfile {
                    provider: "test".to_owned(),
                    model: "test".to_owned(),
                    reasoning: None,
                    max_context: 128_000,
                    max_output: 16_384,
                    supports_images: false,
                },
            )
            .default_model_profile("test")
    }

    pub(super) async fn test_harness(
        workspace: &Path,
        sessions: &Path,
        provider: Arc<dyn Provider>,
    ) -> Harness {
        test_builder(workspace, sessions, provider)
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

    pub(super) async fn observation_session(workspace: &Path, sessions: &Path) -> SessionHandle {
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let harness = test_harness(workspace, sessions, scripted_provider(&requests, [])).await;
        let store = SessionStore::create_ephemeral(sessions).await.unwrap();
        let started = store
            .append(
                AgentId::root(store.id()),
                SessionEvent::SessionStarted {
                    targets: harness.inner.target_definitions.clone(),
                },
            )
            .await
            .unwrap();
        let runtime = SessionRuntime::build(harness.inner.clone(), store, vec![started])
            .await
            .unwrap();
        runtime.start_root(None).await.unwrap()
    }

    pub(super) async fn question_harness(
        workspace: &Path,
        sessions: &Path,
        provider: Arc<dyn Provider>,
        questions: Arc<dyn QuestionHandler>,
    ) -> Harness {
        test_builder(workspace, sessions, provider)
            .question_handler(questions)
            .build()
            .await
            .unwrap()
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

    pub(super) fn replay(payload: serde_json::Value) -> ReplayEnvelope {
        ReplayEnvelope {
            version: 1,
            protocol: "responses".into(),
            model: "native".into(),
            scope: "reasoning".into(),
            payload,
        }
    }

    #[tokio::test]
    async fn observation_catchup_does_not_skip_gaps_before_live_records() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let session = observation_session(workspace.path(), sessions.path()).await;
        session.observe().await;
        // Keep the forwarder from running until a later live record is projected.
        let (first, second) = tokio::task::unconstrained(async {
            let first = session
                .runtime
                .store
                .append(session.root.clone(), SessionEvent::AgentInterrupted)
                .await
                .unwrap();
            let second = session
                .runtime
                .store
                .append(session.root.clone(), SessionEvent::AgentCompleted)
                .await
                .unwrap();
            session
                .runtime
                .events
                .send(RuntimeEvent::Record(Box::new(second.clone())));
            let snapshot = session.runtime.events.observe().snapshot;
            assert!(!snapshot.records.contains_key(&first.sequence));
            assert_eq!(snapshot.records.get(&second.sequence), Some(&second));
            (first, second)
        })
        .await;
        let (left, right) = tokio::join!(session.observe(), session.observe());
        for snapshot in [&left.snapshot, &right.snapshot] {
            assert_eq!(snapshot.records.get(&first.sequence), Some(&first));
            assert_eq!(snapshot.records.get(&second.sequence), Some(&second));
        }
        assert_eq!(left.snapshot.revision, right.snapshot.revision);
        assert_eq!(
            *session.runtime.caught_up_sequence.lock().await,
            second.sequence
        );
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn observation_forwarder_catches_up_after_store_lag() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let session = observation_session(workspace.path(), sessions.path()).await;
        let mut observation = session.observe().await;
        let initial_count = observation.snapshot.records.len();
        // The ephemeral writer has no I/O suspension. Disable cooperative yields
        // to overflow the store's 512-slot channel before its forwarder can run.
        let last = tokio::task::unconstrained(async {
            let mut last = 0;
            for _ in 0..600 {
                last = session
                    .runtime
                    .store
                    .append(session.root.clone(), SessionEvent::AgentInterrupted)
                    .await
                    .unwrap()
                    .sequence;
            }
            last
        })
        .await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while observation.snapshot.records.len() < initial_count + 600 {
                observation
                    .snapshot
                    .apply(observation.updates.recv().await.unwrap());
            }
        })
        .await
        .unwrap();
        assert_eq!(*session.runtime.caught_up_sequence.lock().await, last);
        assert_eq!(
            observation
                .snapshot
                .records
                .into_values()
                .collect::<Vec<_>>(),
            session.runtime.store.records().await
        );
        session.shutdown().await.unwrap();
    }
}
