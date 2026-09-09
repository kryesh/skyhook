//! Provider-neutral session runtime and agent loop.

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, OnceLock, RwLock as StdRwLock, Weak},
};

use futures_util::{StreamExt as _, future::join_all};
use serde_json::json;
use tokio::{
    fs,
    sync::{Mutex, RwLock, broadcast, mpsc, oneshot},
};

use crate::{
    agent::AgentProfile,
    identity::{AgentId, JobId, SessionId},
    job::{CancellationToken, JobManager},
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
    tool::policy::CapabilitySet,
    tool::policy::{AllowAll, Policy},
    tool::{ToolRegistry, ToolRegistryBuilder, executor::ToolExecutor},
};

pub use super::error::HarnessError;
use super::interaction::{QuestionHandler, RuntimeEvent};
use super::observation::RuntimeEvents;
use super::{AgentActivity, Observation};
use super::{TodoItem, TodoSnapshot, todo::TodoStore};

mod compact;
#[cfg(test)]
mod compact_tests;
mod compaction;
mod context;
#[cfg(test)]
mod context_tests;
use context::AgentContext;
pub(super) use context::recorded_context;
mod prompt;
mod questions;
mod queue;
pub use queue::{QueuedPrompt, QueuedPromptToken};
#[cfg(test)]
mod queue_tests;
mod tools;
mod wait;
#[cfg(test)]
mod wait_tests;
use wait::AgentSender;

const AGENT_CHANNEL_CAPACITY: usize = 64;

pub struct HarnessBuilder {
    workspace: PathBuf,
    session_root: Option<PathBuf>,
    providers: BTreeMap<String, Arc<dyn Provider>>,
    model_profiles: BTreeMap<String, ModelProfile>,
    agent_profiles: BTreeMap<String, AgentProfile>,
    default_model_profile: Option<String>,
    default_agent_profile: Option<String>,
    policy: Arc<dyn Policy>,
    questions: Option<Arc<dyn QuestionHandler>>,
    extra_tools: ToolRegistry,
    instructions: Vec<String>,
    max_child_depth: usize,
    capabilities: CapabilitySet,
    targets: TargetsConfig,
    shim_catalog: EmbeddedShimCatalog,
    sensitive_prompts: Arc<dyn SensitivePromptHandler>,
}

impl HarnessBuilder {
    #[must_use]
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
            session_root: None,
            providers: BTreeMap::new(),
            model_profiles: BTreeMap::new(),
            agent_profiles: BTreeMap::new(),
            default_model_profile: None,
            default_agent_profile: None,
            policy: Arc::new(AllowAll),
            questions: None,
            extra_tools: ToolRegistry::default(),
            instructions: Vec::new(),
            max_child_depth: 4,
            capabilities: CapabilitySet::default(),
            targets: TargetsConfig::default(),
            shim_catalog: EmbeddedShimCatalog::default(),
            sensitive_prompts: Arc::new(RejectSensitivePrompts),
        }
    }

    #[must_use]
    pub fn session_root(mut self, path: impl Into<PathBuf>) -> Self {
        self.session_root = Some(path.into());
        self
    }

    #[must_use]
    pub fn provider(mut self, name: impl Into<String>, provider: Arc<dyn Provider>) -> Self {
        self.providers.insert(name.into(), provider);
        self
    }

    #[must_use]
    pub fn model_profile(mut self, name: impl Into<String>, profile: ModelProfile) -> Self {
        self.model_profiles.insert(name.into(), profile);
        self
    }

    #[must_use]
    pub fn agent_profile(mut self, name: impl Into<String>, profile: AgentProfile) -> Self {
        self.agent_profiles.insert(name.into(), profile);
        self
    }

    #[must_use]
    pub fn default_model_profile(mut self, name: impl Into<String>) -> Self {
        self.default_model_profile = Some(name.into());
        self
    }

    #[must_use]
    pub fn default_agent_profile(mut self, name: impl Into<String>) -> Self {
        self.default_agent_profile = Some(name.into());
        self
    }

    #[must_use]
    pub fn policy(mut self, policy: Arc<dyn Policy>) -> Self {
        self.policy = policy;
        self
    }

    #[must_use]
    pub fn question_handler(mut self, handler: Arc<dyn QuestionHandler>) -> Self {
        self.questions = Some(handler);
        self
    }

    #[must_use]
    pub fn tools(mut self, tools: ToolRegistry) -> Self {
        self.extra_tools = tools;
        self
    }

    #[must_use]
    pub fn instructions(mut self, instructions: impl Into<String>) -> Self {
        self.instructions.push(instructions.into());
        self
    }

    #[must_use]
    pub const fn max_child_depth(mut self, depth: usize) -> Self {
        self.max_child_depth = depth;
        self
    }

    #[must_use]
    pub fn capabilities(mut self, capabilities: CapabilitySet) -> Self {
        self.capabilities = capabilities;
        self
    }

    #[must_use]
    pub fn targets_config(mut self, targets: TargetsConfig) -> Self {
        self.targets = targets;
        self
    }

    /// Supplies the platform shims used for SSH-backed tools and agents.
    #[must_use]
    pub fn shim_catalog(mut self, catalog: EmbeddedShimCatalog) -> Self {
        self.shim_catalog = catalog;
        self
    }

    #[must_use]
    pub fn sensitive_prompt_handler(mut self, handler: Arc<dyn SensitivePromptHandler>) -> Self {
        self.sensitive_prompts = handler;
        self
    }

    pub async fn build(self) -> Result<Harness, HarnessError> {
        let workspace = fs::canonicalize(&self.workspace).await?;
        let default_model_profile = self
            .default_model_profile
            .ok_or(HarnessError::MissingDefaultModelProfile)?;
        validate_profiles(
            &self.providers,
            &self.model_profiles,
            &self.agent_profiles,
            &default_model_profile,
            self.default_agent_profile.as_deref(),
        )?;
        let session_root = self
            .session_root
            .unwrap_or_else(|| workspace.join(".skyhook/sessions"));
        let mut instructions = load_agent_instructions(&workspace).await?;
        let skills = HostSkills::discover(&workspace).await;
        let imported = if self.targets.import_ssh_config {
            import_ssh_targets().await?
        } else {
            Vec::new()
        };
        let target_definitions = imported
            .into_iter()
            .chain(self.targets.definitions()?)
            .map(|definition| (definition.name.clone(), definition))
            .collect::<BTreeMap<_, _>>()
            .into_values()
            .collect::<Vec<_>>();
        let target_definitions = crate::target::normalize::normalize(
            target_definitions,
            Vec::new(),
            Arc::new(crate::target::normalize::LocalResolver),
        )
        .await?;
        TargetRegistry::from_definitions(target_definitions.clone())?;
        instructions.extend(self.instructions);
        Ok(Harness {
            inner: Arc::new(HarnessInner {
                workspace,
                session_root,
                providers: self.providers,
                model_profiles: self.model_profiles,
                agent_profiles: self.agent_profiles,
                default_model_profile,
                default_agent_profile: self.default_agent_profile,
                policy: self.policy,
                questions: self.questions,
                extra_tools: self.extra_tools,
                instructions,
                skills,
                max_child_depth: self.max_child_depth,
                capabilities: self.capabilities,
                target_definitions,
                shim_catalog: self.shim_catalog,
                sensitive_prompts: self.sensitive_prompts,
            }),
        })
    }
}

#[derive(Clone)]
pub struct Harness {
    inner: Arc<HarnessInner>,
}

struct HarnessInner {
    workspace: PathBuf,
    session_root: PathBuf,
    providers: BTreeMap<String, Arc<dyn Provider>>,
    model_profiles: BTreeMap<String, ModelProfile>,
    agent_profiles: BTreeMap<String, AgentProfile>,
    default_model_profile: String,
    default_agent_profile: Option<String>,
    policy: Arc<dyn Policy>,
    questions: Option<Arc<dyn QuestionHandler>>,
    extra_tools: ToolRegistry,
    instructions: Vec<String>,
    skills: HostSkills,
    max_child_depth: usize,
    capabilities: CapabilitySet,
    target_definitions: Vec<TargetDefinition>,
    shim_catalog: EmbeddedShimCatalog,
    sensitive_prompts: Arc<dyn SensitivePromptHandler>,
}

impl Harness {
    pub async fn new_session(&self) -> Result<SessionHandle, HarnessError> {
        let store = SessionStore::create(&self.inner.session_root).await?;
        let root = AgentId::root(store.id());
        let started = store
            .append(
                root.clone(),
                SessionEvent::SessionStarted {
                    targets: self.inner.target_definitions.clone(),
                },
            )
            .await?;
        let runtime = SessionRuntime::build(self.inner.clone(), store, vec![started]).await?;
        runtime.start_root(None).await
    }

    pub async fn resume_session(&self, id: SessionId) -> Result<SessionHandle, HarnessError> {
        let (store, records) = SessionStore::open(&self.inner.session_root, id).await?;
        let root = AgentId::root(id);
        let selection = crate::session::agent_selection(&records, &root);
        let runtime = SessionRuntime::build(self.inner.clone(), store, records).await?;
        runtime.start_root(selection).await
    }
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

impl SessionHandle {
    #[must_use]
    pub fn id(&self) -> SessionId {
        self.root.session()
    }

    #[must_use]
    pub fn root_agent(&self) -> &AgentId {
        &self.root
    }

    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
        self.runtime.events.subscribe()
    }

    /// Observe without a gap between the initial snapshot and subsequent updates.
    pub async fn observe(&self) -> Observation {
        self.runtime.catch_up_store_events().await;
        self.runtime.events.observe()
    }

    /// Host diagnostics that must not write directly to a terminal.
    pub fn warnings(&self) -> &[String] {
        self.runtime.harness.skills.warnings()
    }

    pub fn directory(&self) -> &Path {
        self.runtime.store.directory()
    }

    /// Persist a host-facing status without adding it to the agent's model context.
    pub async fn record_status(&self, agent: AgentId, message: String) -> Result<(), HarnessError> {
        self.runtime
            .store
            .append(agent, SessionEvent::Status { message })
            .await?;
        Ok(())
    }

    pub async fn inspect_jobs(&self, agent: &AgentId) -> Vec<crate::job::JobEnvelope> {
        self.runtime.jobs.list(agent).await
    }

    pub async fn inspect_output(
        &self,
        query: crate::job::JobOutputQuery,
    ) -> Result<serde_json::Value, crate::tool::ToolError> {
        self.runtime
            .jobs
            .inspect_output(query, &self.runtime.harness.capabilities)
            .await
    }

    pub async fn cancel_job(
        &self,
        job: JobId,
    ) -> Result<crate::job::JobEnvelope, crate::job::JobError> {
        self.runtime.jobs.cancel(job).await
    }

    pub async fn usage(&self) -> Usage {
        *self.runtime.usage.lock().await
    }

    /// Current todo lists for all session agents, including historical children.
    /// Subscribe before reading this snapshot to observe subsequent replacements.
    pub async fn todos(&self) -> Vec<TodoSnapshot> {
        self.runtime.todos.snapshots().await
    }

    #[must_use]
    pub fn tools(&self) -> &ToolRegistry {
        self.runtime.executor.registry()
    }

    pub async fn prompt(&self, text: impl Into<String>) -> Result<String, HarnessError> {
        self.prompt_with_options(text, &[], PromptOptions::default())
            .await
    }

    /// Continue retained history after a failed/interrupted turn, without duplicating its input.
    pub async fn continue_turn(&self) -> Result<String, HarnessError> {
        self.submit(Vec::new(), None).await
    }

    /// Execute a JavaScript workflow through the session's registered `script` tool.
    pub async fn run_script(
        &self,
        source: impl Into<String>,
    ) -> Result<crate::tool::ToolOutput, HarnessError> {
        let result = self
            .runtime
            .executor
            .clone()
            .with_capabilities(
                self.runtime
                    .harness
                    .capabilities
                    .for_agent(self.runtime.harness.max_child_depth),
            )
            .execute(
                self.root.clone(),
                "script",
                json!({"source": source.into()}),
                None,
            )
            .await?;
        Ok(result.output)
    }

    pub async fn prompt_with_images(
        &self,
        text: impl Into<String>,
        paths: &[PathBuf],
    ) -> Result<String, HarnessError> {
        self.prompt_with_options(text, paths, PromptOptions::default())
            .await
    }

    /// Submit a user message and wait for the resulting turn to finish.
    /// Explicit request-boundary enqueues may change the model during that turn.
    pub async fn prompt_with_options(
        &self,
        text: impl Into<String>,
        paths: &[PathBuf],
        options: PromptOptions,
    ) -> Result<String, HarnessError> {
        let content = self.prepare_prompt(text.into(), paths, &options).await?;
        self.submit(content, options.model).await
    }

    /// Queue a message for the next safe model-request boundary, including while a
    /// request or tool is running. Images and the selected model are captured at
    /// submission. Returns after history commit, NOT after the model finishes.
    ///
    /// Retain a clone of `token` to cancel an unclaimed submission. A successful
    /// cancellation guarantees that its message will not enter history. Cancelled
    /// submissions return [`HarnessError::Interrupted`]. Once claimed, await this
    /// receipt before editing or retrying the message. A dropped receipt does not
    /// cancel the input. Errors mean the user message was not committed.
    ///
    /// Concurrent preparations are serialized in polling order. All available
    /// messages are committed FIFO before compaction and the next provider request;
    /// the last explicit model selection governs that request. Ordinary `prompt`
    /// calls retain their turn-completion semantics.
    pub async fn enqueue_prompt_with_options(
        &self,
        text: impl Into<String>,
        paths: &[PathBuf],
        options: PromptOptions,
        token: QueuedPromptToken,
    ) -> Result<(), HarnessError> {
        self.enqueue_prompts_with_options(vec![QueuedPrompt {
            text: text.into(),
            paths: paths.to_vec(),
            options,
            token,
        }])
        .await
        .pop()
        .expect("one queued prompt has one receipt")
    }

    /// Prepare and enqueue a group atomically at a model-request boundary.
    /// Results correspond to input order and acknowledge history commits, not
    /// turn completion. Invalid or cancelled items do not prevent the remaining
    /// items from committing FIFO. The whole group is prepared before it becomes
    /// visible to the runtime, even when preparing attachments yields.
    pub async fn enqueue_prompts_with_options(
        &self,
        inputs: Vec<QueuedPrompt>,
    ) -> Vec<Result<(), HarnessError>> {
        let preparation = self.enqueue_preparation.lock().await;
        let mut batch = Vec::with_capacity(inputs.len());
        let mut receipts = Vec::with_capacity(inputs.len());
        for input in inputs {
            if input.token.is_cancelled() {
                receipts.push(Err(HarnessError::Interrupted));
                continue;
            }
            match self
                .prepare_prompt(input.text, &input.paths, &input.options)
                .await
            {
                Ok(content) => {
                    let (committed, receipt) = oneshot::channel();
                    batch.push(queue::QueuedInput {
                        content,
                        model: input.options.model,
                        token: input.token,
                        committed,
                    });
                    receipts.push(Ok(receipt));
                }
                Err(error) => receipts.push(Err(error)),
            }
        }
        if !batch.is_empty()
            && !self
                .runtime
                .shutting_down
                .load(std::sync::atomic::Ordering::Acquire)
        {
            // A failed send drops every sender, resolving each receipt as stopped.
            let _ = self.root_tx.send(AgentCommand::QueuedInputs(batch)).await;
        } else {
            drop(batch);
        }
        drop(preparation);
        let mut results = Vec::with_capacity(receipts.len());
        for receipt in receipts {
            results.push(match receipt {
                Ok(receipt) => receipt.await.unwrap_or(Err(HarnessError::AgentStopped)),
                Err(error) => Err(error),
            });
        }
        results
    }

    async fn prepare_prompt(
        &self,
        text: String,
        paths: &[PathBuf],
        options: &PromptOptions,
    ) -> Result<Vec<UserContent>, HarnessError> {
        if let Some(model) = &options.model
            && !self.runtime.harness.model_profiles.contains_key(model)
        {
            return Err(HarnessError::UnknownModelProfile(model.clone()));
        }
        if paths.len() > MAX_IMAGES_PER_SUBMISSION {
            return Err(HarnessError::ImageLimit);
        }
        let mut content = vec![UserContent::Text { text }];
        let mut total = 0_u64;
        for path in paths {
            let absolute = contained_path(&self.runtime.harness.workspace, path).await?;
            let bytes = fs::read(&absolute).await?;
            let length = u64::try_from(bytes.len()).map_err(|_| HarnessError::ImageLimit)?;
            if length > MAX_IMAGE_BYTES {
                return Err(HarnessError::ImageLimit);
            }
            total = total.saturating_add(length);
            if total > MAX_IMAGE_BYTES_PER_SUBMISSION {
                return Err(HarnessError::ImageLimit);
            }
            let media_type = image_media_type(&absolute).ok_or(HarnessError::UnsupportedImage)?;
            let image = self
                .runtime
                .store
                .import_blob(
                    &bytes,
                    absolute
                        .file_name()
                        .map_or_else(|| "image".to_owned(), |name| name.to_string_lossy().into()),
                    media_type.to_owned(),
                )
                .await?;
            content.push(UserContent::Image { image });
        }
        Ok(content)
    }

    async fn submit(
        &self,
        content: Vec<UserContent>,
        model: Option<String>,
    ) -> Result<String, HarnessError> {
        let (done_tx, done_rx) = oneshot::channel();
        self.root_tx
            .send(AgentCommand::Input {
                model,
                content,
                done: Some(done_tx),
            })
            .await
            .map_err(|_| HarnessError::AgentStopped)?;
        done_rx
            .await
            .map_err(|_| HarnessError::AgentStopped)?
            .map_err(HarnessError::Agent)
    }

    pub async fn shutdown(&self) -> Result<(), HarnessError> {
        self.runtime
            .shutting_down
            .store(true, std::sync::atomic::Ordering::Release);
        self.runtime.interrupt_tree(&self.root).await;
        self.runtime.router.shutdown().await;
        // Completed children retain idle loops for resumption, and must also stop.
        let senders = self
            .runtime
            .agents
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .map(|agent| agent.sender.clone())
            .collect::<Vec<_>>();
        for sender in senders {
            let _ = sender.send(AgentCommand::Shutdown).await;
        }
        Ok(())
    }

    pub async fn interrupt(&self) -> usize {
        self.runtime.interrupt_tree(&self.root).await
    }
}

struct SessionRuntime {
    harness: Arc<HarnessInner>,
    store: SessionStore,
    jobs: JobManager,
    todos: TodoStore,
    executor: ToolExecutor,
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
    agent_profile: Option<String>,
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
    fn activity(&self, agent: &AgentId, activity: AgentActivity) {
        self.events.send(RuntimeEvent::Activity {
            agent: agent.clone(),
            activity,
        });
    }

    async fn build(
        harness: Arc<HarnessInner>,
        store: SessionStore,
        prior_records: Vec<EventRecord>,
    ) -> Result<Arc<Self>, HarnessError> {
        let jobs = JobManager::restore(store.clone(), &prior_records).await?;
        let definitions = prior_records
            .iter()
            .find_map(|record| match &record.event {
                SessionEvent::SessionStarted { targets } => Some(targets.clone()),
                _ => None,
            })
            .ok_or_else(|| {
                HarnessError::Initialization("session start event is missing".to_owned())
            })?;
        let targets = TargetRegistry::from_definitions(definitions)?;
        for record in &prior_records {
            if let SessionEvent::TargetsUpserted { targets: restored } = &record.event {
                targets.upsert_many(restored.clone()).await?;
            }
        }
        let authorization =
            crate::tool::authorization::AuthorizationCoordinator::new(harness.policy.clone());
        let remote = RemoteManager::new(
            harness.shim_catalog.clone(),
            harness.sensitive_prompts.clone(),
            authorization.clone(),
        );
        let router =
            crate::target::TargetRouter::new(targets.clone(), remote, authorization.clone());
        let executor_slot = Arc::new(OnceLock::new());
        let runtime_slot = Arc::new(OnceLock::<Weak<Self>>::new());
        let mut builder = ToolRegistryBuilder::default();
        register_coding_tools(
            &mut builder,
            store.clone(),
            jobs.clone(),
            harness.skills.clone(),
            router.clone(),
        )?;
        install_script_tool(&mut builder, Arc::downgrade(&executor_slot))?;
        tools::register(&mut builder, runtime_slot.clone())?;
        builder.extend(&harness.extra_tools)?;
        let executor = ToolExecutor::with_authorization(
            builder.build(),
            authorization,
            jobs.clone(),
            harness.workspace.clone(),
        )
        .with_target_router(router.clone());
        executor_slot
            .set(executor.clone())
            .map_err(|_| HarnessError::Initialization("executor already set".to_owned()))?;
        let records = store.records().await;
        let caught_up_sequence = Mutex::new(records.last().map_or(0, |record| record.sequence));
        let events = RuntimeEvents::new(&records);
        let mut usage = Usage::default();
        for record in &prior_records {
            if let SessionEvent::Usage { usage: value, .. } = &record.event {
                usage.accumulate(*value);
            }
        }
        let mut child_counters = HashMap::new();
        for record in &prior_records {
            if let SessionEvent::AgentStarted {
                parent: Some(parent),
                ..
            } = &record.event
                && let Some(segment) = record.agent.path().last()
            {
                let counter = child_counters.entry(parent.clone()).or_insert(0_u32);
                *counter = (*counter).max(*segment);
            }
        }
        let questions = Arc::new(questions::QuestionCoordinator::new(
            jobs.clone(),
            harness.questions.clone(),
        ));
        let runtime = Arc::new(Self {
            shutting_down: std::sync::atomic::AtomicBool::new(false),
            todos: TodoStore::restore(store.clone(), &prior_records),
            harness,
            store: store.clone(),
            jobs: jobs.clone(),
            executor,
            _executor_slot: executor_slot,
            router,
            agents: StdRwLock::new(HashMap::new()),
            child_counters: RwLock::new(child_counters),
            questions,
            usage: Mutex::new(usage),
            events,
            caught_up_sequence,
        });
        runtime_slot
            .set(Arc::downgrade(&runtime))
            .map_err(|_| HarnessError::Initialization("runtime already set".to_owned()))?;
        runtime.forward_store_events();
        runtime.forward_job_completions();
        Ok(runtime)
    }

    async fn start_root(
        self: &Arc<Self>,
        selection: Option<(String, Option<String>)>,
    ) -> Result<SessionHandle, HarnessError> {
        let root = AgentId::root(self.store.id());
        let (model_profile, agent_profile) = selection.unwrap_or_else(|| {
            (
                self.harness.default_model_profile.clone(),
                self.harness.default_agent_profile.clone(),
            )
        });
        let root_tx = self
            .spawn_agent(AgentLaunch {
                id: root.clone(),
                owner_job: None,
                model_profile,
                agent_profile,
                todos: None,
                available_depth: self.harness.max_child_depth,
                location: crate::execution::ExecutionLocation::root(self.harness.workspace.clone()),
            })
            .await?;
        Ok(SessionHandle {
            runtime: self.clone(),
            root,
            root_tx,
            enqueue_preparation: Arc::new(Mutex::new(())),
        })
    }

    async fn catch_up_store_events(&self) {
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

    fn forward_store_events(self: &Arc<Self>) {
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

    fn forward_job_completions(self: &Arc<Self>) {
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

    async fn interrupt_tree(&self, root: &AgentId) -> usize {
        let targets = self
            .agents
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|(agent, _)| {
                agent.session() == root.session() && agent.path().starts_with(root.path())
            })
            .map(|(id, agent)| (id.clone(), agent.cancellation.clone()))
            .collect::<Vec<_>>();
        let mut cancelled = 0;
        for (agent, cancellation) in targets {
            cancellation.cancel();
            self.activity(&agent, AgentActivity::Interrupted);
            cancelled += self.jobs.cancel_all(&agent).await;
        }
        cancelled
    }

    async fn spawn_agent(
        self: &Arc<Self>,
        launch: AgentLaunch,
    ) -> Result<AgentSender, HarnessError> {
        let AgentLaunch {
            id,
            owner_job,
            model_profile,
            agent_profile,
            todos,
            available_depth,
            location,
        } = launch;
        if id.depth() > self.harness.max_child_depth {
            return Err(HarnessError::ChildDepth);
        }
        let remaining_depth = self.harness.max_child_depth.saturating_sub(id.depth());
        if available_depth > remaining_depth {
            return Err(HarnessError::ChildDepth);
        }
        let capabilities = self.harness.capabilities.for_agent(available_depth);
        let (profile, system) = self
            .resolve_agent(
                &model_profile,
                agent_profile.as_deref(),
                &id,
                &location,
                available_depth,
                &capabilities,
            )
            .await?;
        let context = self
            .open_agent_context(&id, profile, system, &capabilities, true)
            .await?;
        self.store
            .append(
                id.clone(),
                SessionEvent::AgentStarted {
                    parent: id.parent(),
                    owner_job,
                    model_profile: model_profile.clone(),
                    max_context: Some(context.profile.max_context),
                    agent_profile: agent_profile.clone(),
                    location: location.clone(),
                },
            )
            .await?;
        if let Some(job) = owner_job {
            self.jobs.set_agent_location(job, location.clone()).await?;
        }
        self.todos.register(id.clone(), owner_job, todos).await?;
        let (tx, rx) = mpsc::channel(AGENT_CHANNEL_CAPACITY);
        let tx = AgentSender::new(tx);
        self.agents
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                id.clone(),
                LiveAgent {
                    model_profile: model_profile.clone(),
                    sender: tx.clone(),
                    cancellation: CancellationToken::new(),
                    available_depth,
                    completion_gate: Arc::new(Mutex::new(true)),
                },
            );
        self.activity(&id, AgentActivity::Idle);
        let runtime = self.clone();
        tokio::spawn(async move {
            runtime
                .run_agent(AgentLoop {
                    id,
                    owner_job,
                    context,
                    model_profile,
                    location,
                    capabilities,
                    rx,
                })
                .await;
        });
        Ok(tx)
    }

    async fn execute_call(
        &self,
        agent: &AgentId,
        parent: Option<JobId>,
        call: &ToolCall,
        origin: u64,
        location: &crate::execution::ExecutionLocation,
        capabilities: &CapabilitySet,
    ) -> ToolResult {
        let call = call.clone();
        let result = self
            .executor
            .clone()
            .with_location(location.clone())
            .with_capabilities(capabilities.clone())
            .with_model_origin(crate::session::ModelCallOrigin {
                message: origin,
                call_id: call.id.clone(),
            })
            .execute_model(agent.clone(), &call.name, call.arguments.clone(), parent)
            .await;
        match result {
            Ok(result) => {
                let is_error = call.name != "job_output"
                    && result
                        .output
                        .value
                        .get("state")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|state| {
                            matches!(state, "failed" | "cancelled" | "interrupted")
                        });
                ToolResult {
                    call_id: call.id,
                    name: call.name,
                    result: result.output.value,
                    images: result.output.images,
                    is_error,
                }
            }
            Err(error) => {
                let failure = error.into_failure();
                let mut result = json!({"error": failure.message});
                if let Some(denial) = failure.denial {
                    result["code"] = json!(denial.code);
                    result["executed"] = json!(denial.executed);
                }
                let images = if let Some(output) = failure.output {
                    result["output"] = output.value;
                    output.images
                } else {
                    Vec::new()
                };
                ToolResult {
                    call_id: call.id,
                    name: call.name,
                    result,
                    images,
                    is_error: true,
                }
            }
        }
    }

    async fn resolve_agent(
        &self,
        model_profile: &str,
        agent_profile: Option<&str>,
        agent: &AgentId,
        location: &crate::execution::ExecutionLocation,
        available_depth: usize,
        capabilities: &CapabilitySet,
    ) -> Result<(ModelProfile, Vec<SystemSegment>), HarnessError> {
        let profile_instructions = if let Some(name) = agent_profile {
            let profile = self
                .harness
                .agent_profiles
                .get(name)
                .ok_or_else(|| HarnessError::UnknownAgentProfile(name.to_owned()))?;
            Some(profile.instructions.as_str())
        } else {
            None
        };
        let profile = self
            .harness
            .model_profiles
            .get(model_profile)
            .cloned()
            .ok_or_else(|| HarnessError::UnknownModelProfile(model_profile.to_owned()))?;
        let target = if location.is_root() {
            None
        } else {
            Some(self.router.targets().get(&location.target).await?)
        };
        let system = vec![prompt::system_segment(
            &self.harness.instructions,
            profile_instructions,
            agent,
            location,
            target.as_ref(),
            available_depth,
            capabilities,
        )];
        Ok((profile, system))
    }

    async fn open_agent_context(
        &self,
        agent: &AgentId,
        profile: ModelProfile,
        system: Vec<SystemSegment>,
        capabilities: &CapabilitySet,
        restore_meter: bool,
    ) -> Result<AgentContext, HarnessError> {
        let factory = self
            .harness
            .providers
            .get(&profile.provider)
            .ok_or_else(|| HarnessError::UnknownProvider(profile.provider.clone()))?;
        let template = ModelRequest {
            model: profile.model.clone(),
            system,
            messages: Vec::new(),
            tools: self
                .executor
                .clone()
                .with_capabilities(capabilities.clone())
                .surface()
                .definitions(),
            reasoning: profile.reasoning.clone(),
            response_schema: None,
            max_output_tokens: Some(profile.max_output),
            correlation: Some(agent.to_string()),
        };
        AgentContext::open(
            agent,
            profile,
            template,
            factory.as_ref(),
            &self.store.records().await,
            restore_meter,
        )
    }

    async fn run_agent(self: Arc<Self>, agent_loop: AgentLoop) {
        let AgentLoop {
            id,
            owner_job,
            mut context,
            mut model_profile,
            location,
            capabilities,
            mut rx,
        } = agent_loop;
        let is_child = owner_job.is_some();
        let owner_cancellation = match owner_job {
            Some(job) => match self.jobs.cancellation_token(job).await {
                Ok(token) => token,
                Err(_) => {
                    self.agents
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .remove(&id);
                    return;
                }
            },
            None => CancellationToken::new(),
        };
        let completion_gate = self
            .agents
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&id)
            .expect("registered live agent")
            .completion_gate
            .clone();
        let mut child_done: Option<oneshot::Sender<Result<String, String>>> = None;
        let mut child_answer = None;
        let mut deferred = VecDeque::new();
        loop {
            let command = if let Some(command) = deferred.pop_front() {
                command
            } else {
                tokio::select! {
                    biased;
                    () = owner_cancellation.cancelled() => {
                        if let Some(done) = child_done.take() { let _ = done.send(Err("child agent cancelled".to_owned())); }
                        let _ = self.store.append(id.clone(), SessionEvent::AgentInterrupted).await;
                        break;
                    }
                    command = rx.recv() => match command { Some(command) => command, None => break },
                }
            };
            // Register before claiming/persisting a queued message: interrupt must
            // not be lost while a commit is in flight.
            let cancellation = self.begin_turn(&id);
            if let Some(sender) = self.agent_sender(&id) {
                // Every input/notification path shares the same delivery gate.
                let _ = sender.flush_events(&cancellation).await;
            }
            let mut pending_events = None;
            let (content, done, selected_model) = match command {
                AgentCommand::QueuedInputs(inputs) => {
                    if !self
                        .consume_queued_batch(
                            &id,
                            &mut context,
                            &mut model_profile,
                            &capabilities,
                            &cancellation,
                            inputs,
                        )
                        .await
                    {
                        continue;
                    }
                    (Vec::new(), None, None)
                }
                AgentCommand::Shutdown => {
                    let _ = self
                        .store
                        .append(id.clone(), SessionEvent::AgentInterrupted)
                        .await;
                    break;
                }
                AgentCommand::Input {
                    content,
                    done,
                    model,
                } => (content, done, model),
                AgentCommand::JobsReady => {
                    let content = match self
                        .pending_event_content(&id, &capabilities, &location)
                        .await
                    {
                        Ok((content, messages)) if !content.is_empty() => {
                            pending_events = Some(messages);
                            content
                        }
                        Ok(_)
                            if is_child
                                && child_answer.is_some()
                                && !self.jobs.has_running(&id).await =>
                        {
                            let mut completing = completion_gate.lock().await;
                            while let Ok(command) = rx.try_recv() {
                                deferred.push_back(command);
                            }
                            if deferred
                                .iter()
                                .any(|command| matches!(command, AgentCommand::QueuedInputs(_)))
                                || self
                                    .agent_sender(&id)
                                    .is_some_and(|sender| sender.has_child_messages())
                            {
                                continue;
                            }
                            *completing = false;
                            if let Some(done) = child_done.take() {
                                let _ =
                                    done.send(Ok(child_answer.take().expect("child has answered")));
                            }
                            let _ = self
                                .store
                                .append(id.clone(), SessionEvent::AgentCompleted)
                                .await;
                            continue;
                        }
                        _ => continue,
                    };
                    (content, None, None)
                }
            };
            if let Some(model) = selected_model.filter(|model| model != &model_profile)
                && let Err(error) = self
                    .select_model(&id, &mut context, &mut model_profile, &capabilities, model)
                    .await
            {
                if let Some(done) = done {
                    let _ = done.send(Err(error.to_string()));
                }
                continue;
            }
            let done = if is_child {
                if done.is_some() {
                    child_done = done;
                }
                None
            } else {
                done
            };
            self.activity(&id, AgentActivity::Working);
            if !content.is_empty() {
                let message = Message::User(content);
                let committed = match pending_events {
                    Some(messages) => messages.commit(&self, &id, message.clone()).await,
                    None => self
                        .commit(&id, message.clone())
                        .await
                        .map_err(HarnessError::from),
                };
                if let Err(error) = &committed {
                    if let Some(done) = done.or_else(|| child_done.take()) {
                        let _ = done.send(Err(error.to_string()));
                    }
                    if is_child {
                        self.interrupt_tree(&id).await;
                        break;
                    }
                    continue;
                }
                context
                    .projected
                    .push((committed.expect("commit succeeded"), message));
            }
            let result = tokio::select! {
                biased;
                () = owner_cancellation.cancelled() => Err(HarnessError::Interrupted),
                result = self.run_turn(
                    TurnContext {
                        agent: &id,
                        owner_job,
                        cancellation: &cancellation,
                        location: &location,
                        capabilities: &capabilities,
                    },
                    &mut context,
                    &mut model_profile,
                    &mut rx,
                    &mut deferred,
                ) => result,
            };
            if result.is_err() {
                // Unclaimed queue entries remain caller-owned after an interrupt;
                // do not silently start a new turn for them.
                queue::reject_pending(&mut rx, &mut deferred);
            }
            // Serialize the final mailbox check with owner forwarding. An accepted
            // update either joins this request cycle or starts a new retained turn.
            let mut completing = completion_gate.lock().await;
            if is_child && result.is_ok() {
                while let Ok(command) = rx.try_recv() {
                    deferred.push_back(command);
                }
                if deferred
                    .iter()
                    .any(|command| matches!(command, AgentCommand::QueuedInputs(_)))
                    || self
                        .agent_sender(&id)
                        .is_some_and(|sender| sender.has_child_messages())
                {
                    continue;
                }
            }
            self.activity(
                &id,
                match &result {
                    Err(HarnessError::Interrupted) => AgentActivity::Interrupted,
                    Err(error) => AgentActivity::Failed(error.to_string()),
                    Ok(_) if is_child && self.jobs.has_running(&id).await => {
                        AgentActivity::WaitingChildren
                    }
                    Ok(_) => AgentActivity::Idle,
                },
            );
            if let Some(done) = done {
                let _ = done.send(
                    result
                        .as_ref()
                        .map(Clone::clone)
                        .map_err(ToString::to_string),
                );
            }
            if is_child && result.is_err() {
                self.interrupt_tree(&id).await;
                // A cancelled child must not keep its command loop alive.
                if let Some(done) = child_done.take() {
                    let _ = done.send(
                        result
                            .as_ref()
                            .map(Clone::clone)
                            .map_err(ToString::to_string),
                    );
                }
                let _ = self
                    .store
                    .append(id.clone(), SessionEvent::AgentInterrupted)
                    .await;
                break;
            }
            if is_child {
                child_answer = result.as_ref().ok().cloned();
            }
            if is_child && !self.jobs.has_running(&id).await {
                // A descendant may have published a reply and then finished
                // during the awaits above. Check after observing no live jobs.
                if self
                    .agent_sender(&id)
                    .is_some_and(|sender| sender.has_child_messages())
                {
                    continue;
                }
                *completing = false;
                if let Some(done) = child_done.take() {
                    let _ = done.send(
                        result
                            .as_ref()
                            .map(Clone::clone)
                            .map_err(ToString::to_string),
                    );
                }
                let _ = self
                    .store
                    .append(id.clone(), SessionEvent::AgentCompleted)
                    .await;
                // Retain the provider session and full projected history while idle.
                // A fresh owner request resumes this same child, never a new agent.
                child_answer = None;
                continue;
            }
        }
        if let Some(job) = owner_job {
            self.jobs.clear_resume_handler(job).await;
        }
        self.agents
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
    }

    async fn run_turn(
        &self,
        turn: TurnContext<'_>,
        agent_context: &mut AgentContext,
        model_profile: &mut String,
        rx: &mut mpsc::Receiver<AgentCommand>,
        deferred: &mut VecDeque<AgentCommand>,
    ) -> Result<String, HarnessError> {
        let TurnContext {
            agent,
            owner_job,
            cancellation,
            location,
            capabilities,
        } = turn;
        let mut context_sequence = None;
        let mut final_text = String::new();
        let mut force_compaction = false;
        let mut provider_attempt = 0u8;
        let mut compaction_checked = false;
        'requests: loop {
            if let Some(sender) = self.agent_sender(agent) {
                sender.flush_events(cancellation).await?;
                sender.begin_request();
            }
            if cancellation.is_cancelled() {
                return Err(HarnessError::Interrupted);
            }
            if self
                .consume_queued_inputs(&turn, agent_context, model_profile, rx, deferred)
                .await
            {
                // A model change can replace both the template and its token meter.
                context_sequence = None;
                compaction_checked = false;
                provider_attempt = 0;
            }
            if cancellation.is_cancelled() {
                return Err(HarnessError::Interrupted);
            }
            let (content, messages) = self
                .pending_event_content(agent, capabilities, location)
                .await?;
            if !content.is_empty() {
                messages.commit(self, agent, Message::User(content)).await?;
            }
            let profile = agent_context.profile.clone();
            if !profile.supports_images && agent_context.contains_images() {
                return Err(HarnessError::ImagesUnsupported(profile.model.clone()));
            }
            let template = agent_context.template.clone();
            let context = match context_sequence {
                Some(sequence) => sequence,
                None => {
                    let record = self
                        .store
                        .append(
                            agent.clone(),
                            SessionEvent::ModelContext {
                                provider: profile.provider.clone(),
                                template: template.clone(),
                            },
                        )
                        .await?;
                    context_sequence = Some(record.sequence);
                    record.sequence
                }
            };
            agent_context.refresh(&self.store.records().await, agent)?;
            let runtime =
                prompt::runtime_state_content(&self.jobs, &self.todos, agent, capabilities).await;
            let mut request = agent_context.request(runtime);
            if force_compaction || (!compaction_checked && agent_context.needs_compaction(&request))
            {
                self.compact_history(
                    &turn,
                    agent_context.provider.as_mut(),
                    context,
                    &request,
                    profile.max_context,
                )
                .await?;
                force_compaction = false;
                compaction_checked = true;
                // Consume input received during compaction before starting the
                // normal request, rather than delaying it by another request.
                continue 'requests;
            }
            let mut messages = compact::context_sources(&agent_context.projected);
            messages.push(crate::session::ContextMessage::Inline {
                message: request
                    .messages
                    .last()
                    .expect("runtime state is present")
                    .clone(),
            });
            let requested = self
                .store
                .append(
                    agent.clone(),
                    SessionEvent::ModelRequested {
                        context,
                        messages,
                        purpose: crate::session::ModelPurpose::Agent,
                    },
                )
                .await?;
            self.activity(agent, AgentActivity::Working);
            self.events.send(RuntimeEvent::Context {
                agent: agent.clone(),
                tokens: agent_context.meter.estimate(&request),
                capacity: profile.max_context,
            });
            let input_estimate = compaction::estimate_request(&request);
            self.store.hydrate_model_request(&mut request).await?;
            provider_attempt += 1;
            let invoked = tokio::select! {
                response = agent_context.provider.invoke(request) => response,
                () = cancellation.cancelled() => return Err(HarnessError::Interrupted),
            };
            let mut response = match invoked {
                Ok(response) => response,
                Err(error) => {
                    self.record_model_failure(
                        agent,
                        requested.sequence,
                        provider_attempt,
                        Usage::default(),
                        error.to_string(),
                    )
                    .await?;
                    // Transport owns retries. A context rejection may recover only by
                    // changing the request through bounded compaction, never blind replay.
                    if provider_attempt < compact::MAX_PROVIDER_ATTEMPTS
                        && error.kind == crate::provider::ProviderErrorKind::ContextWindowExceeded
                    {
                        force_compaction = true;
                        continue;
                    }
                    return Err(error.into());
                }
            };
            let mut assembler = ResponseAssembler::default();
            let mut usage = Usage::default();
            let mut saw_content = false;
            loop {
                let chunk = tokio::select! {
                    chunk = response.next() => chunk,
                    () = cancellation.cancelled() => {
                        self.record_model_usage(agent, requested.sequence, usage).await?;
                        return Err(HarnessError::Interrupted);
                    },
                };
                let Some(chunk) = chunk else {
                    break;
                };
                let chunk = match chunk.and_then(|chunk| {
                    assembler.push(&chunk)?;
                    Ok(chunk)
                }) {
                    Ok(chunk) => chunk,
                    Err(error) => {
                        self.record_model_failure(
                            agent,
                            requested.sequence,
                            provider_attempt,
                            usage,
                            error.to_string(),
                        )
                        .await?;
                        if !saw_content
                            && provider_attempt < compact::MAX_PROVIDER_ATTEMPTS
                            && error.kind
                                == crate::provider::ProviderErrorKind::ContextWindowExceeded
                        {
                            force_compaction = true;
                            continue 'requests;
                        }
                        return Err(error.into());
                    }
                };
                saw_content |= !matches!(&chunk, ResponseChunk::UsageUpdated { .. });
                if let ResponseChunk::UsageUpdated { usage: value } = &chunk {
                    usage = *value;
                }
                self.events.send(RuntimeEvent::ResponseEvent {
                    agent: agent.clone(),
                    request: requested.sequence,
                    event: chunk,
                });
            }
            let response = match finish_response(assembler, usage) {
                Ok(response) => response,
                Err(error) => {
                    self.record_model_failure(
                        agent,
                        requested.sequence,
                        provider_attempt,
                        usage,
                        error.to_string(),
                    )
                    .await?;
                    return Err(error);
                }
            };
            let assistant = Message::Assistant(response.blocks);
            let origin = self.commit(agent, assistant.clone()).await?;
            agent_context.projected.push((origin, assistant));
            if response.stop_reason == crate::provider::protocol::StopReason::Aborted {
                // Preserve completed visible/replay content, but never turn a
                // provider cancellation into a successful agent turn. The failure
                // helper records observed usage exactly once before returning.
                let error = "provider aborted response".to_owned();
                self.record_model_failure(
                    agent,
                    requested.sequence,
                    provider_attempt,
                    response.usage,
                    error.clone(),
                )
                .await?;
                self.events.send(RuntimeEvent::ResponseSettled {
                    agent: agent.clone(),
                    request: requested.sequence,
                    message: Some(origin),
                    error: Some(error),
                });
                return Err(HarnessError::Interrupted);
            }
            if let Some(job) = owner_job
                && !response.calls.is_empty()
                && !response.text.trim().is_empty()
                && let Some(parent) = agent.parent().and_then(|parent| self.agent_sender(&parent))
            {
                let metadata = self.jobs.metadata(job).await?;
                parent.child_message(wait::ChildMessage {
                    id: job,
                    name: metadata.name,
                    message: origin,
                    text: response.text.clone(),
                });
            } else {
                // Child progress is delivered separately, not concatenated into
                // the eventual final result (nor repeated at completion).
                final_text.push_str(&response.text);
            }
            self.events.send(RuntimeEvent::ResponseSettled {
                agent: agent.clone(),
                request: requested.sequence,
                message: Some(origin),
                error: None,
            });
            self.record_model_usage(agent, requested.sequence, response.usage)
                .await?;
            agent_context.meter.observe(input_estimate, response.usage);
            let runtime =
                prompt::runtime_state_content(&self.jobs, &self.todos, agent, capabilities).await;
            let current = agent_context.request(runtime);
            self.events.send(RuntimeEvent::Context {
                agent: agent.clone(),
                tokens: agent_context.meter.estimate(&current),
                capacity: profile.max_context,
            });
            provider_attempt = 0;
            compaction_checked = false;
            if response.calls.is_empty() {
                // Notifications arriving during this provider request must be
                // processed before returning an answer based on earlier context.
                // Use the same snapshot/commit/ack boundary for child progress
                // and terminal/question events, including a shared completion batch.
                let (content, messages) = self
                    .pending_event_content(agent, capabilities, location)
                    .await?;
                if !content.is_empty() {
                    messages.commit(self, agent, Message::User(content)).await?;
                    final_text.clear();
                    continue 'requests;
                }
                // A response without tools is still a request boundary. Consume
                // prompts received in flight before completing a one-shot child
                // (or returning a stale final answer to the root caller).
                if self
                    .consume_queued_inputs(&turn, agent_context, model_profile, rx, deferred)
                    .await
                {
                    context_sequence = None;
                    final_text.clear();
                    continue 'requests;
                }
                self.events.send(RuntimeEvent::TurnCompleted {
                    agent: agent.clone(),
                    text: final_text.clone(),
                });
                return Ok(final_text);
            }
            self.questions
                .prepare_question_batch(agent, &response.calls)
                .await;
            self.activity(agent, AgentActivity::Tools);
            let results = join_all(response.calls.iter().map(|call| {
                self.execute_call(agent, owner_job, call, origin, location, capabilities)
            }))
            .await;
            let tools = Message::Tool(results);
            let sequence = self.commit(agent, tools.clone()).await?;
            agent_context.projected.push((sequence, tools));
        }
    }

    async fn record_model_failure(
        &self,
        agent: &AgentId,
        request: u64,
        attempt: u8,
        usage: Usage,
        error: String,
    ) -> Result<(), HarnessError> {
        if usage != Usage::default() {
            self.record_model_usage(agent, request, usage).await?;
        }
        self.store
            .append(
                agent.clone(),
                SessionEvent::ModelFailed {
                    request,
                    attempt,
                    error,
                },
            )
            .await?;
        Ok(())
    }

    async fn record_model_usage(
        &self,
        agent: &AgentId,
        request: u64,
        usage: Usage,
    ) -> Result<(), HarnessError> {
        self.store
            .append(
                agent.clone(),
                SessionEvent::Usage {
                    request: Some(request),
                    usage,
                },
            )
            .await?;
        self.usage.lock().await.accumulate(usage);
        Ok(())
    }

    async fn commit(&self, agent: &AgentId, message: Message) -> Result<u64, SessionError> {
        let record = self
            .store
            .append(agent.clone(), SessionEvent::MessageCommitted { message })
            .await?;
        Ok(record.sequence)
    }

    async fn next_child(&self, parent: &AgentId) -> AgentId {
        let mut counters = self.child_counters.write().await;
        let counter = counters.entry(parent.clone()).or_insert(0);
        *counter = counter.saturating_add(1);
        parent.child(*counter)
    }

    fn available_depth(&self, agent: &AgentId) -> usize {
        self.agents
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(agent)
            .map_or(0, |agent| agent.available_depth)
    }

    fn agent_sender(&self, id: &AgentId) -> Option<AgentSender> {
        self.agents
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .map(|agent| agent.sender.clone())
    }

    fn begin_turn(&self, id: &AgentId) -> CancellationToken {
        let cancellation = CancellationToken::new();
        self.agents
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(id)
            .expect("running agents are registered")
            .cancellation = cancellation.clone();
        cancellation
    }
}

struct FoldedResponse {
    stop_reason: crate::provider::protocol::StopReason,
    blocks: Vec<AssistantContent>,
    usage: Usage,
    calls: Vec<ToolCall>,
    text: String,
}

fn finish_response(
    assembler: ResponseAssembler,
    usage: Usage,
) -> Result<FoldedResponse, HarnessError> {
    let (mut blocks, final_usage, stop_reason) = assembler.finish()?;
    // A terminal limit/filter/abort does not authorize execution, even if a
    // backend completed valid arguments before learning the final stop reason.
    if matches!(
        stop_reason,
        crate::provider::protocol::StopReason::MaxTokens
            | crate::provider::protocol::StopReason::ContentFilter
            | crate::provider::protocol::StopReason::Aborted
    ) {
        blocks.retain(|item| {
            !item
                .blocks
                .iter()
                .any(|block| matches!(block.content, BlockContent::ToolCall(_)))
        });
    }
    debug_assert_eq!(usage, final_usage);
    let text = blocks
        .iter()
        .flat_map(|item| &item.blocks)
        .filter_map(|block| match &block.content {
            BlockContent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    let calls = blocks
        .iter()
        .flat_map(|item| &item.blocks)
        .filter_map(|block| match &block.content {
            BlockContent::ToolCall(call) => Some(call.clone()),
            _ => None,
        })
        .collect();
    Ok(FoldedResponse {
        stop_reason,
        blocks,
        usage,
        calls,
        text,
    })
}

fn validate_profiles(
    providers: &BTreeMap<String, Arc<dyn Provider>>,
    models: &BTreeMap<String, ModelProfile>,
    agents: &BTreeMap<String, AgentProfile>,
    default_model: &str,
    default_agent: Option<&str>,
) -> Result<(), HarnessError> {
    if !models.contains_key(default_model) {
        return Err(HarnessError::UnknownModelProfile(default_model.to_owned()));
    }
    if let Some(name) = default_agent
        && !agents.contains_key(name)
    {
        return Err(HarnessError::UnknownAgentProfile(name.to_owned()));
    }
    for (name, profile) in models {
        profile.validate_limits().map_err(|error| {
            HarnessError::InvalidProfile(format!("model profile `{name}`: {error}"))
        })?;
        if !providers.contains_key(&profile.provider) {
            return Err(HarnessError::InvalidProfile(format!(
                "model profile `{name}` uses unknown provider `{}`",
                profile.provider
            )));
        }
    }
    for (name, profile) in agents {
        if let Some(model) = &profile.model_profile
            && !models.contains_key(model)
        {
            return Err(HarnessError::InvalidProfile(format!(
                "agent profile `{name}` uses unknown model profile `{model}`"
            )));
        }
    }
    Ok(())
}

async fn load_agent_instructions(workspace: &Path) -> Result<Vec<String>, std::io::Error> {
    let mut directories = workspace.ancestors().collect::<Vec<_>>();
    directories.reverse();
    let mut output = Vec::new();
    for directory in directories {
        let path = directory.join("AGENTS.md");
        match fs::read_to_string(path).await {
            Ok(text) if !text.trim().is_empty() => output.push(text),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(output)
}

async fn contained_path(workspace: &Path, requested: &Path) -> Result<PathBuf, HarnessError> {
    let candidate = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        workspace.join(requested)
    };
    let canonical = fs::canonicalize(candidate).await?;
    if !canonical.starts_with(workspace) {
        return Err(HarnessError::OutsideWorkspace);
    }
    Ok(canonical)
}

fn image_media_type(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
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
    use std::{
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

    use futures_util::stream;

    use super::*;
    use crate::{
        agent::Question,
        provider::protocol::{
            ContentDelta, ItemKind, ReplayEnvelope, StopReason, ToolCall, events_for_content,
        },
        provider::{ProviderContext, ProviderError, ProviderFuture, ResponseStream},
    };

    #[derive(Clone)]
    struct ScriptedProvider {
        responses: Arc<StdMutex<VecDeque<Vec<ResponseChunk>>>>,
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
    }

    fn scripted_provider(
        requests: &Arc<StdMutex<Vec<ModelRequest>>>,
        responses: impl IntoIterator<Item = Vec<ResponseChunk>>,
    ) -> Arc<ScriptedProvider> {
        Arc::new(ScriptedProvider {
            requests: requests.clone(),
            responses: Arc::new(StdMutex::new(responses.into_iter().collect())),
        })
    }

    #[derive(Clone)]
    struct HangingProvider;

    #[derive(Clone)]
    struct BlockingFirstProvider {
        calls: Arc<AtomicUsize>,
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
        release: Arc<tokio::sync::Semaphore>,
    }

    struct RecordingQuestions {
        batches: Arc<StdMutex<Vec<Vec<Question>>>>,
        answer: serde_json::Value,
    }

    struct GatedResponse {
        release: Pin<Box<dyn Future<Output = ()> + Send>>,
        released: bool,
        events: VecDeque<ResponseChunk>,
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

    fn runtime_state_count(messages: &[Message]) -> usize {
        messages
            .iter()
            .filter_map(|message| match message {
                Message::User(content) => Some(content),
                Message::Assistant(_) | Message::Tool(_) => None,
            })
            .flatten()
            .filter(|content| {
                matches!(
                    content,
                    UserContent::Runtime { text } if text.contains("<skyhook_state>")
                )
            })
            .count()
    }

    fn request_history(request: &ModelRequest) -> &[Message] {
        assert_eq!(runtime_state_count(&request.messages), 1);
        let (state, history) = request.messages.split_last().unwrap();
        assert_eq!(runtime_state_count(std::slice::from_ref(state)), 1);
        history
    }

    async fn assert_request_journal(
        store: &SessionStore,
        captured: &Arc<StdMutex<Vec<ModelRequest>>>,
    ) {
        let journal = fs::read_to_string(store.directory().join("events.jsonl"))
            .await
            .unwrap();
        let records = journal
            .lines()
            .map(|line| serde_json::from_str::<EventRecord>(line).unwrap())
            .collect::<Vec<_>>();
        let mut reconstructed = BTreeMap::<_, Vec<_>>::new();
        for record in &records {
            if matches!(record.event, SessionEvent::ModelRequested { .. }) {
                let (provider, mut request) =
                    crate::session::reconstruct_model_request(&records, record.sequence).unwrap();
                assert_eq!(provider, "test");
                assert_eq!(
                    request.correlation.as_deref(),
                    Some(record.agent.to_string().as_str())
                );
                store.hydrate_model_request(&mut request).await.unwrap();
                reconstructed
                    .entry(request.correlation.clone())
                    .or_default()
                    .push(request);
            }
        }
        let mut expected = BTreeMap::<_, Vec<_>>::new();
        for request in captured.lock().unwrap().iter() {
            expected
                .entry(request.correlation.clone())
                .or_default()
                .push(request.clone());
        }
        assert!(!expected.is_empty());
        assert_eq!(reconstructed, expected);
    }

    fn request_location(request: &ModelRequest) -> serde_json::Value {
        let text = &request.system[0].text;
        assert!(!text.contains("<target_context>"));
        let (_, context) = text.split_once("<skyhook_context>\n").unwrap();
        let (context, _) = context.split_once("\n</skyhook_context>").unwrap();
        serde_json::from_str(context).unwrap()
    }

    fn test_builder(
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

    async fn test_harness(
        workspace: &Path,
        sessions: &Path,
        provider: Arc<dyn Provider>,
    ) -> Harness {
        test_builder(workspace, sessions, provider)
            .build()
            .await
            .unwrap()
    }

    // Wait for owned agent loops to release the runtime before reopening its journal.
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

    async fn observation_session(workspace: &Path, sessions: &Path) -> SessionHandle {
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

    async fn question_harness(
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

    fn response(items: Vec<AssistantContent>) -> Vec<ResponseChunk> {
        let stop_reason = if items.iter().any(|item| item.kind == ItemKind::ToolCall) {
            StopReason::ToolUse
        } else {
            StopReason::EndTurn
        };
        let mut events = events_for_content(&items);
        events.push(ResponseChunk::ResponseEnded { stop_reason });
        events
    }

    fn answer(text: impl Into<String>) -> Vec<ResponseChunk> {
        response(vec![AssistantContent::text("answer", 0, text)])
    }

    fn replay(payload: serde_json::Value) -> ReplayEnvelope {
        ReplayEnvelope {
            version: 1,
            protocol: "responses".into(),
            model: "native".into(),
            scope: "reasoning".into(),
            payload,
        }
    }

    #[tokio::test]
    async fn switching_models_preserves_image_history_and_reports_unsupported_images() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let image = workspace.path().join("sample.png");
        fs::write(&image, b"image fixture").await.unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let provider = scripted_provider(
            &requests,
            [
                response(vec![AssistantContent::text("answer", 0, "Image received")]),
                response(vec![AssistantContent::text(
                    "answer",
                    0,
                    "Image still present",
                )]),
            ],
        );
        let harness = test_builder(workspace.path(), sessions.path(), provider)
            .model_profile(
                "vision",
                ModelProfile {
                    provider: "test".into(),
                    model: "vision-model".into(),
                    reasoning: None,
                    max_context: 128_000,
                    max_output: 4096,
                    supports_images: true,
                },
            )
            .build()
            .await
            .unwrap();
        let session = harness.new_session().await.unwrap();
        let before = session.runtime.store.records().await.len();
        assert!(
            session
                .prompt_with_options(
                    "Missing image",
                    &[workspace.path().join("missing.png")],
                    PromptOptions {
                        model: Some("vision".into())
                    }
                )
                .await
                .is_err()
        );
        assert_eq!(session.runtime.store.records().await.len(), before);
        session
            .prompt_with_options(
                "Look at this",
                &[image],
                PromptOptions {
                    model: Some("vision".into()),
                },
            )
            .await
            .unwrap();
        let error = session
            .prompt_with_options(
                "Keep going",
                &[],
                PromptOptions {
                    model: Some("test".into()),
                },
            )
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("does not support image"),
            "{error}"
        );
        assert_eq!(requests.lock().unwrap().len(), 1);
        session
            .prompt_with_options(
                "Use vision again",
                &[],
                PromptOptions {
                    model: Some("vision".into()),
                },
            )
            .await
            .unwrap();
        {
            let requests = requests.lock().unwrap();
            assert!(contains_images(&requests[1].messages));
        }
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn incomplete_native_response_is_not_retried_or_committed() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let harness = test_harness(
            workspace.path(),
            sessions.path(),
            Arc::new(ScriptedProvider {
                requests: Arc::new(StdMutex::new(Vec::new())),
                responses: StdMutex::new(VecDeque::from([
                    vec![
                        ResponseChunk::ItemStarted {
                            id: "answer".into(),
                            position: 0,
                            kind: ItemKind::Text,
                        },
                        ResponseChunk::BlockStarted {
                            item: "answer".into(),
                            id: "answer:0".into(),
                            position: 0,
                            kind: crate::provider::protocol::BlockKind::Text,
                        },
                        ResponseChunk::BlockDelta {
                            item: "answer".into(),
                            block: "answer:0".into(),
                            delta: ContentDelta::Text("must not persist".into()),
                        },
                    ],
                    answer("Recovered"),
                ]))
                .into(),
            }),
        )
        .await;
        let session = harness.new_session().await.unwrap();
        assert!(session.prompt("Question").await.is_err());
        session.shutdown().await.unwrap();
        let records = SessionStore::read_records(sessions.path(), session.id())
            .await
            .unwrap();
        assert!(records.iter().any(|record| matches!(&record.event,
            SessionEvent::ModelFailed { error, .. } if error.contains("response ended before all items ended"))));
        let assistants: Vec<_> = records
            .iter()
            .filter_map(|record| match &record.event {
                SessionEvent::MessageCommitted {
                    message: Message::Assistant(blocks),
                } => Some(blocks),
                _ => None,
            })
            .collect();
        assert!(assistants.is_empty());
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(record.event, SessionEvent::ModelRequested { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn abnormal_termination_never_executes_even_completed_tool_calls() {
        for reason in [
            StopReason::MaxTokens,
            StopReason::ContentFilter,
            StopReason::Aborted,
        ] {
            let mut assembler = ResponseAssembler::default();
            let items = vec![
                AssistantContent::text("answer", 0, "Visible response"),
                AssistantContent::tool_call(
                    "tool",
                    1,
                    ToolCall {
                        id: "call".into(),
                        name: "shell".into(),
                        arguments: json!({"command":"unsafe"}),
                    },
                ),
            ];
            for event in crate::provider::protocol::events_for_content(&items) {
                assembler.push(&event).unwrap();
            }
            assembler
                .push(&ResponseChunk::ResponseEnded {
                    stop_reason: reason,
                })
                .unwrap();
            let folded = finish_response(assembler, Usage::default()).unwrap();
            assert_eq!(folded.text, "Visible response");
            assert!(folded.calls.is_empty());
            assert_eq!(folded.blocks.len(), 1);
        }
    }

    #[tokio::test]
    async fn aborted_response_preserves_visible_content_and_usage_but_settles_as_interrupted() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let retained = vec![
            AssistantContent::reasoning(
                "reason",
                0,
                "completed reasoning",
                Some(replay(json!({"encrypted_content":"retained"}))),
            ),
            AssistantContent::text("answer", 1, "partial visible answer"),
        ];
        let mut items = retained.clone();
        items.push(AssistantContent::tool_call(
            "tool",
            2,
            ToolCall {
                id: "call".into(),
                name: "write".into(),
                arguments: json!({"path":"must-not-exist", "content":"unsafe"}),
            },
        ));
        let observed = Usage {
            input_tokens: 11,
            cached_input_tokens: 7,
            output_tokens: 3,
        };
        let mut chunks = events_for_content(&items);
        chunks.push(ResponseChunk::UsageUpdated { usage: observed });
        chunks.push(ResponseChunk::ResponseEnded {
            stop_reason: StopReason::Aborted,
        });
        let harness = test_harness(
            workspace.path(),
            sessions.path(),
            scripted_provider(&requests, [chunks]),
        )
        .await;
        let session = harness.new_session().await.unwrap();
        let mut events = session.runtime.events.subscribe();
        let error = session.prompt("Abort this turn.").await.unwrap_err();
        assert!(error.to_string().contains("interrupted"));
        assert_eq!(requests.lock().unwrap().len(), 1);
        assert_eq!(session.usage().await, observed);
        let records = session.runtime.store.records().await;
        let assistants: Vec<_> = records
            .iter()
            .filter_map(|record| match &record.event {
                SessionEvent::MessageCommitted {
                    message: Message::Assistant(items),
                } => Some(items.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(assistants, vec![retained]);
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(record.event, SessionEvent::Usage { .. }))
                .count(),
            1
        );
        assert!(records.iter().any(|record| matches!(&record.event, SessionEvent::ModelFailed { error, .. } if error == "provider aborted response")));
        assert!(
            !records
                .iter()
                .any(|record| matches!(record.event, SessionEvent::JobCreated { .. }))
        );
        assert!(!workspace.path().join("must-not-exist").exists());
        let mut settled = false;
        while let Ok(event) = events.try_recv() {
            match event {
                RuntimeEvent::ResponseSettled { message, error, .. } => {
                    assert!(message.is_some());
                    assert_eq!(error.as_deref(), Some("provider aborted response"));
                    settled = true;
                }
                RuntimeEvent::TurnCompleted { .. } => {
                    panic!("aborted turn cannot complete successfully")
                }
                _ => {}
            }
        }
        assert!(settled);
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn native_blocks_keep_order_and_opaque_state_after_journal_reload() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        // Adjacent reasoning items retain separate native identities and replay envelopes.
        let blocks = vec![
            AssistantContent::reasoning(
                "one",
                0,
                "First",
                Some(replay(json!({
                    "type":"reasoning","id":"one","encrypted_content":"secret-one"
                }))),
            ),
            AssistantContent::reasoning(
                "two",
                1,
                "Second",
                Some(replay(json!({
                    "type":"reasoning","id":"two","encrypted_content":"secret-two"
                }))),
            ),
            AssistantContent::text("answer", 2, "Answer"),
            AssistantContent::text("punctuation", 3, "!"),
        ];
        let mut chunks = Vec::new();
        // Arrival order is deliberately different from provider item order.
        for index in [1, 0, 2, 3] {
            let item = &blocks[index];
            let block = &item.blocks[0];
            chunks.push(ResponseChunk::ItemStarted {
                id: item.id.clone(),
                position: item.position,
                kind: item.kind,
            });
            chunks.push(ResponseChunk::BlockStarted {
                item: item.id.clone(),
                id: block.id.clone(),
                position: block.position,
                kind: block.content.kind(),
            });
            if index != 3 {
                chunks.push(ResponseChunk::BlockDelta {
                    item: item.id.clone(),
                    block: block.id.clone(),
                    delta: ContentDelta::Text("provisional content".into()),
                });
            }
        }
        for index in [1, 2, 3, 0] {
            let item = &blocks[index];
            let block = &item.blocks[0];
            chunks.push(ResponseChunk::BlockEnded {
                item: item.id.clone(),
                block: block.id.clone(),
                content: block.content.clone(),
            });
            chunks.push(ResponseChunk::ItemEnded {
                id: item.id.clone(),
                replay: item.replay.clone(),
            });
        }
        chunks.push(ResponseChunk::ResponseEnded {
            stop_reason: StopReason::EndTurn,
        });
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let harness = test_harness(
            workspace.path(),
            sessions.path(),
            Arc::new(ScriptedProvider {
                requests: requests.clone(),
                responses: StdMutex::new(VecDeque::from([
                    chunks,
                    answer("Next"),
                    answer("Resumed"),
                ]))
                .into(),
            }),
        )
        .await;
        let session = harness.new_session().await.unwrap();
        assert_eq!(session.prompt("Question").await.unwrap(), "Answer!");
        session.prompt("Follow up").await.unwrap();
        assert!(
            requests.lock().unwrap()[1]
                .messages
                .contains(&Message::Assistant(blocks.clone()))
        );
        session.shutdown().await.unwrap();
        let records = SessionStore::read_records(sessions.path(), session.id())
            .await
            .unwrap();
        assert!(records.iter().any(|record| matches!(&record.event,
            SessionEvent::MessageCommitted { message: Message::Assistant(actual) } if actual == &blocks)));
        let id = session.id();
        shutdown_session(session).await;
        let resumed = harness.resume_session(id).await.unwrap();
        assert_eq!(resumed.prompt("After reload").await.unwrap(), "Resumed");
        assert!(
            requests.lock().unwrap()[2]
                .messages
                .contains(&Message::Assistant(blocks))
        );
        resumed.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn continuing_interrupted_turn_retains_input_once_and_shutdown_is_idempotent() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let harness = test_harness(
            workspace.path(),
            sessions.path(),
            Arc::new(BlockingFirstProvider {
                calls: Arc::new(AtomicUsize::new(0)),
                requests: requests.clone(),
                release: Arc::new(tokio::sync::Semaphore::new(0)),
            }),
        )
        .await;
        let session = harness.new_session().await.unwrap();
        let task = tokio::spawn({
            let session = session.clone();
            async move { session.prompt("retained input").await }
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while requests.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        session.interrupt().await;
        assert!(task.await.unwrap().is_err());
        session
            .record_status(session.root.clone(), "Interrupted".into())
            .await
            .unwrap();
        assert_eq!(session.continue_turn().await.unwrap(), "jobs handled");
        let records = session.runtime.store.records().await;
        assert_eq!(records.iter().filter(|record| matches!(&record.event,
            SessionEvent::MessageCommitted { message: Message::User(blocks) }
                if blocks.iter().any(|block| matches!(block, UserContent::Text { text } if text == "retained input"))
        )).count(), 1);
        assert!(records.iter().any(|record| matches!(&record.event, SessionEvent::Status { message } if message == "Interrupted")));
        let captured = requests.lock().unwrap().clone();
        assert_eq!(captured.len(), 2);
        assert_eq!(captured[0].messages, captured[1].messages);
        session.shutdown().await.unwrap();
        tokio::task::yield_now().await;
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn failed_provider_call_is_journaled_before_invocation() {
        #[derive(Clone)]
        struct FailingProvider {
            session_root: PathBuf,
        }
        impl Provider for FailingProvider {
            fn open_context(
                &self,
                _correlation: String,
            ) -> Result<Box<dyn ProviderContext>, ProviderError> {
                Ok(Box::new(self.clone()))
            }
        }

        impl ProviderContext for FailingProvider {
            fn invoke(&mut self, request: ModelRequest) -> ProviderFuture {
                let session = request
                    .correlation
                    .as_ref()
                    .unwrap()
                    .split(':')
                    .next()
                    .unwrap();
                let path = self.session_root.join(session).join("events.jsonl");
                Box::pin(async move {
                    let journal = fs::read_to_string(path).await.unwrap();
                    let records = journal
                        .lines()
                        .map(|line| serde_json::from_str::<EventRecord>(line).unwrap())
                        .collect::<Vec<_>>();
                    let call = records.last().unwrap();
                    let (provider, restored) =
                        crate::session::reconstruct_model_request(&records, call.sequence).unwrap();
                    assert_eq!(provider, "test");
                    assert_eq!(restored, request);
                    Err(crate::provider::ProviderError::protocol(
                        "intentional provider failure",
                    ))
                })
            }
        }
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let harness = test_harness(
            workspace.path(),
            sessions.path(),
            Arc::new(FailingProvider {
                session_root: sessions.path().to_owned(),
            }),
        )
        .await;
        let session = harness.new_session().await.unwrap();
        let error = session.prompt("test failure").await.unwrap_err();
        assert!(error.to_string().contains("intentional provider failure"));
    }

    #[tokio::test]
    async fn provider_tool_loop_runs_through_the_shared_registry() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("note.txt"), "hello").unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let harness = test_harness(
            workspace.path(),
            sessions.path(),
            scripted_provider(
                &requests,
                [
                    response(vec![
                        AssistantContent::tool_call(
                            "tool-0",
                            0,
                            ToolCall {
                                id: "read-1".to_owned(),
                                name: "read".to_owned(),
                                arguments: json!({"path": "note.txt"}),
                            },
                        ),
                        AssistantContent::tool_call(
                            "tool-1",
                            1,
                            ToolCall {
                                id: "read-2".to_owned(),
                                name: "read".to_owned(),
                                arguments: json!({"path": "note.txt"}),
                            },
                        ),
                    ]),
                    response(vec![AssistantContent::text(
                        "answer",
                        0,
                        "finished".to_owned(),
                    )]),
                ],
            ),
        )
        .await;
        let session = harness.new_session().await.unwrap();
        assert_eq!(session.prompt("read the note").await.unwrap(), "finished");
        let script_output = session
            .run_script(r#"return tool.read().path("note.txt");"#)
            .await
            .unwrap();
        assert_eq!(script_output.value["value"]["content"], "hello");
        let background = session
            .runtime
            .executor
            .execute_model(
                session.root.clone(),
                "script",
                json!({"source": "return null;", "bg": true}),
                None,
            )
            .await
            .unwrap();
        assert!(background.output.value["location"].get("target").is_none());
        assert_request_journal(&session.runtime.store, &requests).await;
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].system, requests[1].system);
        assert_eq!(requests[0].tools, requests[1].tools);
        assert_eq!(requests[0].system.len(), 1);
        assert!(requests[0].system[0].cache);
        assert!(requests[0].system[0].text.starts_with(prompt::ROOT_PROMPT));
        assert!(requests[0].system[0].text.contains("<skyhook_context>"));
        assert!(!requests[0].system[0].text.contains("\"date\":"));
        assert!(requests[0].system[0].text.contains("\"available_depth\":4"));
        assert!(!requests[0].system[0].text.contains(prompt::TARGET_PROMPT));
        assert_eq!(
            request_location(&requests[0]),
            json!({"workspace": workspace.path()})
        );
        assert!(requests[0].tools.iter().all(|tool| {
            tool.input_schema["properties"].get("target").is_none()
                && !matches!(tool.name.as_str(), "targets" | "target_add")
        }));
        let script = requests[0]
            .tools
            .iter()
            .find(|tool| tool.name == "script")
            .unwrap();
        assert!(!script.description.contains(".target("));
        let agent = requests[0]
            .tools
            .iter()
            .find(|tool| tool.name == "agent")
            .unwrap();
        assert_eq!(agent.input_schema["properties"]["depth"]["default"], 0);
        let Message::User(content) = request_history(&requests[0]).last().unwrap() else {
            panic!("external input must remain a user message");
        };
        assert!(matches!(&content[0], UserContent::Text { text } if text == "read the note"));
        assert_eq!(
            content.len(),
            1,
            "runtime state must not be committed with input"
        );
        assert!(request_history(&requests[1]).starts_with(request_history(&requests[0])));
        assert_eq!(requests[1].messages.len(), requests[0].messages.len() + 2);
        let Message::Tool(results) = request_history(&requests[1]).last().unwrap() else {
            panic!("parallel calls must be committed as one tool-result message");
        };
        assert_eq!(results.len(), 2);
        assert_eq!(runtime_state_count(&requests[1].messages), 1);
    }

    #[tokio::test]
    async fn child_first_request_includes_parent_supplied_todos() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let todos = json!([
            {"text": "Inspect the implementation", "status": "completed"},
            {"text": "Make the change", "status": "in_progress"},
            {"text": "Run relevant checks", "status": "pending"}
        ]);
        let harness = test_harness(
            workspace.path(),
            sessions.path(),
            scripted_provider(
                &requests,
                [
                    response(vec![AssistantContent::tool_call(
                        "tool-0",
                        0,
                        ToolCall {
                            id: "delegate".to_owned(),
                            name: "agent".to_owned(),
                            arguments: json!({"prompt": "work", "todos": todos}),
                        },
                    )]),
                    response(vec![AssistantContent::text(
                        "answer",
                        0,
                        "child done".to_owned(),
                    )]),
                    response(vec![AssistantContent::text(
                        "answer",
                        0,
                        "root done".to_owned(),
                    )]),
                ],
            ),
        )
        .await;
        let session = harness.new_session().await.unwrap();
        assert_eq!(session.prompt("delegate").await.unwrap(), "root done");
        assert_request_journal(&session.runtime.store, &requests).await;
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        let child_request = &requests[1];
        assert!(
            child_request.system[0]
                .text
                .starts_with(prompt::CHILD_PROMPT)
        );
        assert_eq!(runtime_state_count(&child_request.messages), 1);
        let Some(Message::User(content)) = child_request.messages.last() else {
            panic!("expected transient runtime state at the end of the first child request");
        };
        let state = content
            .iter()
            .find_map(|content| match content {
                UserContent::Runtime { text } => text
                    .strip_prefix("<skyhook_state>\n")
                    .and_then(|text| text.strip_suffix("\n</skyhook_state>")),
                _ => None,
            })
            .expect("first child request has a runtime state block");
        let state: serde_json::Value = serde_json::from_str(state).unwrap();
        assert_eq!(state["todos"], todos);
    }

    #[tokio::test]
    async fn child_completion_waits_for_background_work_and_returns_its_updated_answer() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let call = |name: &str, arguments| {
            response(vec![AssistantContent::tool_call(
                "tool-0",
                0,
                ToolCall {
                    id: name.to_owned(),
                    name: name.to_owned(),
                    arguments,
                },
            )])
        };
        let text =
            |text: &str| response(vec![AssistantContent::text("answer", 0, text.to_owned())]);
        let answer = "child work completed\n".repeat(500);
        let harness = test_harness(
            workspace.path(),
            sessions.path(),
            scripted_provider(
                &requests,
                [
                    call("agent", json!({"prompt":"work"})),
                    call(
                        "script",
                        json!({"source":"return await receive();", "bg":true}),
                    ),
                    text("premature child answer"),
                    text(&answer),
                    text("root done"),
                ],
            ),
        )
        .await;
        let session = harness.new_session().await.unwrap();
        let mut events = session.runtime.events.subscribe();
        let prompt = session.prompt("delegate");
        tokio::pin!(prompt);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                tokio::select! {
                    result = &mut prompt => panic!("parent returned before child work completed: {result:?}"),
                    event = events.recv() => if matches!(event.unwrap(), RuntimeEvent::TurnCompleted { text, .. } if text == "premature child answer") { break; },
                }
            }
        }).await.unwrap();
        let agent_job = session
            .runtime
            .jobs
            .list(&session.root)
            .await
            .into_iter()
            .find(|job| job.tool == "agent")
            .unwrap();
        assert!(!agent_job.state.is_terminal());
        let child = session.root.child(1);
        let script = session
            .runtime
            .jobs
            .list(&child)
            .await
            .into_iter()
            .find(|job| job.tool == "script")
            .unwrap();
        session
            .runtime
            .jobs
            .send(script.id, json!("released"))
            .await
            .unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), prompt)
                .await
                .unwrap()
                .unwrap(),
            "root done"
        );
        let requests = requests.lock().unwrap();
        let Message::Tool(results) = request_history(requests.last().unwrap()).last().unwrap()
        else {
            panic!("child result expected")
        };
        assert_eq!(results[0].result["result"], answer);
        assert!(results[0].result.get("truncated").is_none());
        for request in requests.iter() {
            let system = &request.system[0].text;
            assert!(!system.contains("compaction"));
            assert!(system.contains("Use job_output to retrieve truncated results."));
            assert!(!system.contains("Continue with the returned"));
        }
    }

    #[tokio::test]
    async fn completed_child_resumes_same_history_and_job_repeatedly() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let harness = test_harness(
            workspace.path(),
            sessions.path(),
            scripted_provider(
                &requests,
                [
                    response(vec![AssistantContent::text("answer", 0, "first answer")]),
                    response(vec![AssistantContent::text("answer", 0, "second answer")]),
                    response(vec![AssistantContent::text("answer", 0, "third answer")]),
                ],
            ),
        )
        .await;
        let session = harness.new_session().await.unwrap();
        // This test drives tools directly; suppress autonomous parent wakeups.
        let (quiet_sender, _quiet_receiver) = mpsc::channel(AGENT_CHANNEL_CAPACITY);
        session
            .runtime
            .agents
            .write()
            .unwrap()
            .get_mut(&session.root)
            .unwrap()
            .sender = AgentSender::new(quiet_sender);
        let first = session
            .runtime
            .executor
            .execute(
                session.root.clone(),
                "agent",
                json!({"prompt":"remember the initial task", "depth":0}),
                None,
            )
            .await
            .unwrap();
        assert_eq!(first.output.value, "first answer");
        let job = first.job;
        let child = session.root.child(1);
        assert_eq!(session.runtime.jobs.prune_claimed().await.unwrap(), 0);
        for (instruction, answer) in [
            ("follow-up one", "second answer"),
            ("follow-up two", "third answer"),
        ] {
            session
                .run_script(format!(
                    "return tool.job({job}).send({{value:{}}});",
                    json!(instruction)
                ))
                .await
                .unwrap();
            session.runtime.jobs.wait(job, None, true).await.unwrap();
            let sent = session
                .run_script(format!("return tool.job({job}).output();"))
                .await
                .unwrap();
            assert_eq!(sent.value["value"]["id"], job.get());
            assert_eq!(sent.value["value"]["state"], "completed");
            assert_eq!(sent.value["value"]["result"], answer);
            assert_eq!(
                session.runtime.jobs.metadata(job).await.unwrap().state,
                crate::job::JobState::Completed
            );
        }
        {
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 3);
            assert!(
                requests
                    .iter()
                    .all(|request| request.correlation.as_deref()
                        == Some(child.to_string().as_str()))
            );
            let history = request_history(&requests[2]);
            let text = serde_json::to_string(history).unwrap();
            for expected in [
                "remember the initial task",
                "first answer",
                "follow-up one",
                "second answer",
                "follow-up two",
            ] {
                assert!(text.contains(expected), "missing {expected}: {text}");
            }
            assert!(
                matches!(history.last(), Some(Message::User(content)) if matches!(&content[0], UserContent::ParentInput {text} if text.contains("follow-up two")))
            );
        }
        assert_eq!(session.runtime.store.records().await.iter().filter(|record| matches!(&record.event, SessionEvent::AgentStarted {owner_job:Some(id), ..} if *id == job)).count(), 1);
        let restored = JobManager::restore(
            session.runtime.store.clone(),
            &session.runtime.store.records().await,
        )
        .await
        .unwrap();
        assert_eq!(
            restored.snapshot(job).await.unwrap().output,
            Some(json!("third answer"))
        );
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn child_finishes_when_another_waiter_claims_its_last_background_result() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let harness = test_harness(
            workspace.path(),
            sessions.path(),
            Arc::new(BlockingFirstProvider {
                calls: Arc::new(AtomicUsize::new(0)),
                requests: Arc::new(StdMutex::new(Vec::new())),
                release: release.clone(),
            }),
        )
        .await;
        let session = harness.new_session().await.unwrap();
        let child = session.root.child(1);
        let owner_job = session
            .runtime
            .jobs
            .create(crate::job::JobSpec::test(session.root.clone(), "agent"))
            .await
            .unwrap()
            .id;
        let job = session
            .runtime
            .jobs
            .create(crate::job::JobSpec {
                background: true,
                ..crate::job::JobSpec::test(child.clone(), "manual")
            })
            .await
            .unwrap()
            .id;
        let sender = session
            .runtime
            .spawn_agent(AgentLaunch {
                id: child.clone(),
                owner_job: Some(owner_job),
                model_profile: "test".to_owned(),
                agent_profile: None,
                todos: None,
                available_depth: 0,
                location: crate::execution::ExecutionLocation::root(workspace.path().to_path_buf()),
            })
            .await
            .unwrap();
        let (done, received) = oneshot::channel();
        let mut events = session.runtime.events.subscribe();
        sender
            .send(AgentCommand::Input {
                model: None,
                content: vec![UserContent::Text {
                    text: "task".to_owned(),
                }],
                done: Some(done),
            })
            .await
            .unwrap();
        release.add_permits(1);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if matches!(events.recv().await.unwrap(), RuntimeEvent::TurnCompleted {agent, ..} if agent == child) { break; }
            }
        }).await.unwrap();
        session
            .runtime
            .jobs
            .finish(
                job,
                crate::job::JobOutcome::Completed(crate::tool::ToolOutput::default()),
            )
            .await
            .unwrap();
        session.runtime.jobs.claim(job).await.unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), received)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            "initial"
        );
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn delegated_depth_cannot_exceed_the_callers_budget() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let harness = test_harness(
            workspace.path(),
            sessions.path(),
            scripted_provider(
                &requests,
                [
                    response(vec![AssistantContent::tool_call(
                        "tool-0",
                        0,
                        ToolCall {
                            id: "agent-too-deep".to_owned(),
                            name: "agent".to_owned(),
                            arguments: json!({"prompt":"too deep", "depth":4}),
                        },
                    )]),
                    response(vec![AssistantContent::text(
                        "answer",
                        0,
                        "root done".to_owned(),
                    )]),
                ],
            ),
        )
        .await;

        let session = harness.new_session().await.unwrap();
        assert_eq!(session.prompt("overdelegate").await.unwrap(), "root done");

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2, "an over-budget child must not start");
        let Message::Tool(results) = request_history(&requests[1]).last().unwrap() else {
            panic!("root must receive the failed agent result");
        };
        assert!(results[0].is_error);
        assert!(
            results[0].result["error"]
                .as_str()
                .is_some_and(|error| error.contains("available depth of 4"))
        );
    }

    #[tokio::test]
    async fn leaf_children_cannot_invoke_agent_directly_or_from_scripts() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let harness = test_harness(
            workspace.path(),
            sessions.path(),
            scripted_provider(
                &requests,
                [
                    response(vec![AssistantContent::tool_call(
                        "tool-0",
                        0,
                        ToolCall {
                            id: "root-agent".to_owned(),
                            name: "agent".to_owned(),
                            arguments: json!({"prompt":"try hidden delegation"}),
                        },
                    )]),
                    response(vec![
                        AssistantContent::tool_call(
                            "tool-0",
                            0,
                            ToolCall {
                                id: "hidden-agent".to_owned(),
                                name: "agent".to_owned(),
                                arguments: json!({"prompt":"escape"}),
                            },
                        ),
                        AssistantContent::tool_call(
                            "tool-1",
                            1,
                            ToolCall {
                                id: "script-agent".to_owned(),
                                name: "script".to_owned(),
                                arguments: json!({
                                    "source":"return tool.agent({prompt: 'escape'});"
                                }),
                            },
                        ),
                    ]),
                    response(vec![AssistantContent::text(
                        "answer",
                        0,
                        "child done".to_owned(),
                    )]),
                    response(vec![AssistantContent::text(
                        "answer",
                        0,
                        "root done".to_owned(),
                    )]),
                ],
            ),
        )
        .await;

        let session = harness.new_session().await.unwrap();
        assert_eq!(session.prompt("delegate").await.unwrap(), "root done");

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 4, "hidden delegation must not start agents");
        let Message::Tool(results) = request_history(&requests[2]).last().unwrap() else {
            panic!("child must receive both failed tool results");
        };
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|result| result.is_error));
        assert!(
            results[0].result["error"]
                .as_str()
                .is_some_and(|error| error.contains("unavailable"))
        );
    }

    #[tokio::test]
    async fn concurrent_root_questions_are_merged_and_answers_are_split() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let batches = Arc::new(StdMutex::new(Vec::new()));
        let harness = question_harness(
            workspace.path(),
            sessions.path(),
            scripted_provider(
                &requests,
                [
                    response(vec![
                        AssistantContent::tool_call(
                            "tool-0",
                            0,
                            ToolCall {
                                id: "ask-1".to_owned(),
                                name: "ask".to_owned(),
                                arguments: json!({"id":"first", "prompt":"First?", "options":[]}),
                            },
                        ),
                        AssistantContent::tool_call(
                            "tool-1",
                            1,
                            ToolCall {
                                id: "ask-2".to_owned(),
                                name: "ask".to_owned(),
                                arguments: json!({"id":"second", "prompt":"Second?", "options":[]}),
                            },
                        ),
                    ]),
                    response(vec![AssistantContent::text("answer", 0, "done".to_owned())]),
                ],
            ),
            Arc::new(RecordingQuestions {
                batches: batches.clone(),
                answer: json!({"first":"yes", "second":{"value":2}, "extra":true}),
            }),
        )
        .await;
        let session = harness.new_session().await.unwrap();
        let ask = session
            .tools()
            .surface(&CapabilitySet::default())
            .definitions()
            .into_iter()
            .find(|definition| definition.name == "ask")
            .unwrap();
        assert!(ask.input_schema["properties"]["id"].is_object());
        assert!(ask.input_schema["properties"]["prompt"].is_object());
        assert!(ask.input_schema["properties"]["options"].is_object());
        assert!(ask.input_schema["properties"].get("questions").is_none());
        assert_eq!(ask.input_schema["properties"]["bg"]["default"], false);
        assert_eq!(ask.input_schema["properties"]["bg"]["type"], "boolean");
        assert!(
            !ask.input_schema["required"]
                .as_array()
                .unwrap()
                .contains(&json!("bg"))
        );
        assert!(ask.input_schema["properties"].get("background").is_none());

        assert_eq!(session.prompt("ask twice").await.unwrap(), "done");
        let batches = batches.lock().unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(
            batches[0]
                .iter()
                .map(|question| question.id.as_str())
                .collect::<Vec<_>>(),
            ["first", "second"]
        );
        let requests = requests.lock().unwrap();
        let Message::Tool(results) = request_history(&requests[1]).last().unwrap() else {
            panic!("answers must be returned as tool results");
        };
        assert_eq!(results[0].result["result"], "yes");
        assert_eq!(results[1].result["result"], json!({"value":2}));

        let events =
            std::fs::read_to_string(session.runtime.store.directory().join("events.jsonl"))
                .unwrap();
        let opened = events
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|record| record["event"]["type"] == "question_opened")
            .collect::<Vec<_>>();
        assert_eq!(opened.len(), 2);
        assert!(opened.iter().all(|record| {
            record["event"]["question_id"]
                .as_str()
                .is_some_and(|id| id.starts_with("q-"))
                && record["event"]["questions"]
                    .as_array()
                    .is_some_and(|items| items.len() == 1)
        }));
    }

    #[tokio::test]
    async fn background_child_asks_merge_across_turns_and_resolve_independently() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let harness =
            test_harness(workspace.path(), sessions.path(), Arc::new(HangingProvider)).await;
        let session = harness.new_session().await.unwrap();
        let child = session.root.child(1);
        let owner = session
            .runtime
            .jobs
            .create(crate::job::JobSpec {
                accepts_input: true,
                ..crate::job::JobSpec::test(session.root.clone(), "agent")
            })
            .await
            .unwrap();
        session
            .runtime
            .jobs
            .transition(owner.id, crate::job::JobState::Running)
            .await
            .unwrap();
        let first = session
            .runtime
            .executor
            .execute_model(
                child.clone(),
                "ask",
                json!({"id":"first", "prompt":"First?", "bg":true}),
                Some(owner.id),
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while session.runtime.jobs.snapshot(owner.id).await.unwrap().state
                != crate::job::JobState::WaitingInput
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let second = session
            .runtime
            .executor
            .execute_model(
                child.clone(),
                "ask",
                json!({"id":"second", "prompt":"Second?", "bg":true}),
                Some(owner.id),
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let output = session
                    .runtime
                    .jobs
                    .snapshot(owner.id)
                    .await
                    .unwrap()
                    .output
                    .unwrap();
                if output["questions"].as_array().unwrap().len() == 2 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(first.background && second.background);
        // A repeated stable ID must not replace an outstanding question.
        let duplicate = session
            .runtime
            .executor
            .execute_model(
                child,
                "ask",
                json!({"id":"second", "prompt":"Duplicate?", "bg":true}),
                Some(owner.id),
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !session
                .runtime
                .jobs
                .snapshot(duplicate.job)
                .await
                .unwrap()
                .state
                .is_terminal()
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            session
                .runtime
                .jobs
                .snapshot(duplicate.job)
                .await
                .unwrap()
                .state,
            crate::job::JobState::Failed
        );
        // Parents can answer a subset keyed by stable question ID, even when
        // a new background batch arrived after the question was presented.
        assert!(
            session
                .runtime
                .questions
                .answer_child_question(owner.id, json!({"first":"one"}))
                .await
                .unwrap()
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            while !session
                .runtime
                .jobs
                .snapshot(first.job)
                .await
                .unwrap()
                .state
                .is_terminal()
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let remaining = session.runtime.jobs.snapshot(owner.id).await.unwrap();
        assert_eq!(remaining.state, crate::job::JobState::WaitingInput);
        assert_eq!(remaining.output.unwrap()["questions"][0]["id"], "second");
        assert_eq!(
            session
                .runtime
                .jobs
                .wait(first.job, None, true)
                .await
                .unwrap()
                .output,
            Some(json!("one"))
        );
        // Keyed semantics survive shrinking a merged set to one question.
        assert!(
            session
                .runtime
                .questions
                .answer_child_question(owner.id, json!({"second":"two"}))
                .await
                .unwrap()
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            while !session
                .runtime
                .jobs
                .snapshot(second.job)
                .await
                .unwrap()
                .state
                .is_terminal()
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            session
                .runtime
                .jobs
                .wait(second.job, None, true)
                .await
                .unwrap()
                .output,
            Some(json!("two"))
        );
        assert_eq!(
            session.runtime.jobs.snapshot(owner.id).await.unwrap().state,
            crate::job::JobState::Running
        );
        session.runtime.jobs.cancel(owner.id).await.unwrap();
    }

    #[tokio::test]
    async fn child_questions_route_through_the_stable_agent_job() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let harness =
            test_harness(workspace.path(), sessions.path(), Arc::new(HangingProvider)).await;
        let session = harness.new_session().await.unwrap();
        let root = session.root.clone();
        let child = root.child(1);
        let agent = session
            .runtime
            .jobs
            .create(crate::job::JobSpec {
                accepts_input: true,
                ..crate::job::JobSpec::test(root, "agent")
            })
            .await
            .unwrap();
        session
            .runtime
            .jobs
            .transition(agent.id, crate::job::JobState::Running)
            .await
            .unwrap();
        let mut ask = session
            .runtime
            .jobs
            .create(crate::job::JobSpec {
                parent: Some(agent.id),
                accepts_input: true,
                ..crate::job::JobSpec::test(child.clone(), "ask")
            })
            .await
            .unwrap();
        session
            .runtime
            .jobs
            .transition(ask.id, crate::job::JobState::Running)
            .await
            .unwrap();
        let mut ask_two = session
            .runtime
            .jobs
            .create(crate::job::JobSpec {
                parent: Some(agent.id),
                accepts_input: true,
                ..crate::job::JobSpec::test(child.clone(), "ask")
            })
            .await
            .unwrap();
        session
            .runtime
            .jobs
            .transition(ask_two.id, crate::job::JobState::Running)
            .await
            .unwrap();
        let owner = session
            .runtime
            .questions
            .open_child_questions(
                vec![
                    ("first".to_owned(), ask.id),
                    ("second".to_owned(), ask_two.id),
                ],
                json!({"kind":"questions","question_ids":["q-one","q-two"],"questions":[]}),
            )
            .await
            .unwrap();
        assert_eq!(owner, agent.id);
        assert_eq!(
            session.runtime.jobs.snapshot(owner).await.unwrap().state,
            crate::job::JobState::WaitingInput
        );
        assert!(
            session
                .runtime
                .questions
                .answer_child_question(owner, json!({"first": "yes", "second": 2}))
                .await
                .unwrap()
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            for _ in 0..100 {
                assert!(
                    session
                        .runtime
                        .questions
                        .answer_child_question(
                            owner,
                            json!({"first":"duplicate", "second":"duplicate"})
                        )
                        .await
                        .unwrap()
                );
            }
        })
        .await
        .expect("duplicate answers must not block on a full input channel");
        assert_eq!(ask.input.recv().await.unwrap(), json!("yes"));
        assert_eq!(ask_two.input.recv().await.unwrap(), json!(2));
        session
            .runtime
            .questions
            .resolve_child_question(owner, &[ask.id, ask_two.id])
            .await
            .unwrap();
        assert_eq!(
            session.runtime.jobs.snapshot(owner).await.unwrap().state,
            crate::job::JobState::Running
        );
        // Cancellation remains responsive after a burst of duplicate replies.
        session
            .runtime
            .questions
            .open_child_questions(vec![("cancel".to_owned(), ask.id)], json!({"questions":[]}))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            for _ in 0..100 {
                session
                    .runtime
                    .questions
                    .answer_child_question(owner, json!("cancel me"))
                    .await
                    .unwrap();
            }
            session.runtime.questions.cancel_child_question(owner).await;
        })
        .await
        .expect("reply bursts must not block coordinator cancellation");
        assert!(ask.cancellation.is_cancelled());
    }

    #[tokio::test]
    async fn script_messages_preserve_order_across_calls_and_cancel_waiting_receivers() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let harness =
            test_harness(workspace.path(), sessions.path(), Arc::new(HangingProvider)).await;
        let session = harness.new_session().await.unwrap();
        let running = session.runtime.executor.execute(
            session.root.clone(), "script",
            json!({"source": "const messages=[]; for(let i=0;i<4;i++) messages.push(await receive()); return {messages, notifyType:typeof notify};", "bg":true}),
            None,
        ).await.unwrap();
        let id = running.job.get();
        let queued = session
            .run_script(format!(
                r#"
const accepted = [];
for (const value of [{{text:"hello 🌏", nested:[1,true]}}, null, false]) {{
  accepted.push(await tool.job({id}).send({{value}}));
}}
const pending = await tool.job({id}).output();
return {{accepted, state:pending.state}};
"#
            ))
            .await
            .unwrap();
        assert_eq!(
            queued.value["value"]["accepted"],
            json!(vec![json!({"accepted":true}); 3])
        );
        assert_eq!(queued.value["value"]["state"], "running");
        session
            .run_script(format!("return tool.job({id}).send({{value:\"last\"}});"))
            .await
            .unwrap();
        session
            .runtime
            .jobs
            .wait(running.job, None, true)
            .await
            .unwrap();
        let completed = session
            .run_script(format!("return tool.job({id}).output();"))
            .await
            .unwrap();
        assert_eq!(completed.value["value"]["state"], "completed");
        assert_eq!(
            completed.value["value"]["result"]["value"],
            json!({
                "messages":[{"text":"hello 🌏", "nested":[1,true]}, null, false, "last"],
                "notifyType":"undefined",
            })
        );

        let waiting = session
            .runtime
            .executor
            .execute(
                session.root.clone(),
                "script",
                json!({"source":"return await receive();", "bg":true}),
                None,
            )
            .await
            .unwrap();
        let id = waiting.job.get();
        session
            .run_script(format!(
                "await tool.job({id}).output(); return tool.job({id}).cancel();"
            ))
            .await
            .unwrap();
        session
            .runtime
            .jobs
            .wait(waiting.job, None, true)
            .await
            .unwrap();
        let cancelled = session
            .run_script(format!("return tool.job({id}).output();"))
            .await
            .unwrap();
        assert_eq!(cancelled.value["value"]["state"], "cancelled");
    }
}
