//! Provider-neutral session runtime and agent loop.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, OnceLock, RwLock as StdRwLock, Weak},
};

use base64::Engine as _;
use futures_util::{future::join_all, future::poll_fn};
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
        AssistantContent, Message, ModelRequest, ResponseChunk, SystemSegment, ToolCall,
        ToolResult, Usage, UserContent,
    },
    remote::{EmbeddedShimCatalog, RejectSensitivePrompts, RemoteManager, SensitivePromptHandler},
    session::{EventRecord, SessionError, SessionEvent, SessionStore},
    target::{TargetDefinition, TargetRegistry, TargetsConfig, import_ssh_targets},
    tool::builtins::{HostSkills, install_script_tool_weak, register_coding_tools},
    tool::policy::CapabilitySet,
    tool::policy::{AllowAll, Policy},
    tool::{ToolRegistry, ToolRegistryBuilder, executor::ToolExecutor},
};

pub use super::error::HarnessError;
use super::interaction::{QuestionHandler, RuntimeEvent};
use super::{TodoItem, TodoSnapshot, todo::TodoStore};

mod prompt;
mod questions;
mod tools;

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
        runtime.start_root(Vec::new()).await
    }

    pub async fn resume_session(&self, id: SessionId) -> Result<SessionHandle, HarnessError> {
        let (store, records) = SessionStore::open(&self.inner.session_root, id).await?;
        let root = AgentId::root(id);
        let history = records
            .iter()
            .filter(|record| record.agent == root)
            .filter_map(|record| match &record.event {
                SessionEvent::MessageCommitted { message } => {
                    prompt::without_legacy_state(message.clone())
                }
                _ => None,
            })
            .collect();
        let runtime = SessionRuntime::build(self.inner.clone(), store, records).await?;
        runtime.start_root(history).await
    }
}

#[derive(Clone)]
pub struct SessionHandle {
    runtime: Arc<SessionRuntime>,
    root: AgentId,
    root_tx: mpsc::Sender<AgentCommand>,
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

    /// Returns cumulative model usage for the session, including usage restored on resume.
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
        self.submit(vec![UserContent::Text { text: text.into() }])
            .await
    }

    /// Execute a JavaScript workflow through the session's registered `script` tool.
    pub async fn run_script(
        &self,
        source: impl Into<String>,
    ) -> Result<crate::tool::ToolOutput, HarnessError> {
        let result = self
            .runtime
            .executor
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
        if paths.len() > MAX_IMAGES_PER_SUBMISSION {
            return Err(HarnessError::ImageLimit);
        }
        let mut content = vec![UserContent::Text { text: text.into() }];
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
        self.submit(content).await
    }

    async fn submit(&self, content: Vec<UserContent>) -> Result<String, HarnessError> {
        let (done_tx, done_rx) = oneshot::channel();
        self.root_tx
            .send(AgentCommand::Input {
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
        self.root_tx
            .send(AgentCommand::Shutdown)
            .await
            .map_err(|_| HarnessError::AgentStopped)
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
    events: broadcast::Sender<RuntimeEvent>,
}

struct LiveAgent {
    sender: mpsc::Sender<AgentCommand>,
    cancellation: CancellationToken,
    available_depth: usize,
}

enum AgentCommand {
    Input {
        content: Vec<UserContent>,
        done: Option<oneshot::Sender<Result<String, String>>>,
    },
    JobsReady,
    Shutdown,
}

struct AgentLaunch {
    id: AgentId,
    parent: Option<AgentId>,
    owner_job: Option<JobId>,
    model_profile: String,
    agent_profile: Option<String>,
    history: Vec<Message>,
    todos: Option<Vec<TodoItem>>,
    one_shot: bool,
    available_depth: usize,
    location: crate::execution::ExecutionLocation,
}

struct AgentLoop {
    id: AgentId,
    owner_job: Option<JobId>,
    profile: ModelProfile,
    system: Vec<SystemSegment>,
    history: Vec<Message>,
    one_shot: bool,
    location: crate::execution::ExecutionLocation,
    capabilities: CapabilitySet,
    rx: mpsc::Receiver<AgentCommand>,
}

struct TurnContext<'a> {
    agent: &'a AgentId,
    profile: &'a ModelProfile,
    system: &'a [SystemSegment],
    owner_job: Option<JobId>,
    cancellation: &'a CancellationToken,
    location: &'a crate::execution::ExecutionLocation,
    capabilities: &'a CapabilitySet,
}

impl SessionRuntime {
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
            if let SessionEvent::TargetUpserted { target } = &record.event {
                targets.upsert(target.clone()).await?;
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
        install_script_tool_weak(&mut builder, Arc::downgrade(&executor_slot))?;
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
        let (events, _) = broadcast::channel(1024);
        let mut usage = Usage::default();
        for record in &prior_records {
            if let SessionEvent::Usage { usage: value } = &record.event {
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
        history: Vec<Message>,
    ) -> Result<SessionHandle, HarnessError> {
        let root = AgentId::root(self.store.id());
        let root_tx = self
            .spawn_agent(AgentLaunch {
                id: root.clone(),
                parent: None,
                owner_job: None,
                model_profile: self.harness.default_model_profile.clone(),
                agent_profile: self.harness.default_agent_profile.clone(),
                history,
                todos: None,
                one_shot: false,
                available_depth: self.harness.max_child_depth,
                location: crate::execution::ExecutionLocation::root(self.harness.workspace.clone()),
            })
            .await?;
        Ok(SessionHandle {
            runtime: self.clone(),
            root,
            root_tx,
        })
    }

    fn forward_store_events(self: &Arc<Self>) {
        let mut source = self.store.subscribe();
        let events = self.events.clone();
        tokio::spawn(async move {
            loop {
                match source.recv().await {
                    Ok(record) => {
                        let _ = events.send(RuntimeEvent::Record(record));
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
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
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                let Some(runtime) = runtime.upgrade() else {
                    break;
                };
                if let Some(sender) = runtime.agent_sender(&completion.agent) {
                    let _ = sender.send(AgentCommand::JobsReady).await;
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
            cancelled += self.jobs.cancel_all(&agent).await;
        }
        cancelled
    }

    async fn spawn_agent(
        self: &Arc<Self>,
        launch: AgentLaunch,
    ) -> Result<mpsc::Sender<AgentCommand>, HarnessError> {
        let AgentLaunch {
            id,
            parent,
            owner_job,
            model_profile,
            agent_profile,
            history,
            todos,
            one_shot,
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
        let (profile, system) = self.resolve_agent(
            &model_profile,
            agent_profile.as_deref(),
            &id,
            &location,
            available_depth,
            &capabilities,
        )?;
        self.store
            .append(
                id.clone(),
                SessionEvent::AgentStarted {
                    parent,
                    owner_job,
                    model_profile: model_profile.clone(),
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
        self.agents
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                id.clone(),
                LiveAgent {
                    sender: tx.clone(),
                    cancellation: CancellationToken::new(),
                    available_depth,
                },
            );
        let runtime = self.clone();
        tokio::spawn(async move {
            runtime
                .run_agent(AgentLoop {
                    id,
                    owner_job,
                    profile,
                    system,
                    history,
                    one_shot,
                    location,
                    capabilities,
                    rx,
                })
                .await;
        });
        Ok(tx)
    }

    async fn resolve_location(
        &self,
        target: &str,
        workspace: Option<PathBuf>,
    ) -> Result<crate::execution::ExecutionLocation, HarnessError> {
        if target == crate::target::ROOT_TARGET {
            return Ok(crate::execution::ExecutionLocation::root(
                workspace.unwrap_or_else(|| self.harness.workspace.clone()),
            ));
        }
        let definition = self.router.targets().get(target).await?;
        Ok(crate::execution::ExecutionLocation::named(
            target,
            workspace.unwrap_or(definition.workspace),
        ))
    }

    async fn execute_call(
        &self,
        agent: &AgentId,
        parent: Option<JobId>,
        call: &ToolCall,
        location: &crate::execution::ExecutionLocation,
        capabilities: &CapabilitySet,
    ) -> ToolResult {
        let call = call.clone();
        let result = self
            .executor
            .clone()
            .with_location(location.clone())
            .with_capabilities(capabilities.clone())
            .execute_model(agent.clone(), &call.name, call.arguments.clone(), parent)
            .await;
        match result {
            Ok(result) => ToolResult {
                call_id: call.id,
                name: call.name,
                result: result.output.value,
                console_output: result.output.console_output,
                images: result.output.images,
                is_error: false,
            },
            Err(error) => {
                let failure = error.into_failure();
                let mut result = json!({"error": failure.message});
                if let Some(denial) = failure.denial {
                    result["code"] = json!(denial.code);
                    result["executed"] = json!(denial.executed);
                }
                let mut console_output = String::new();
                let images = if let Some(output) = failure.output {
                    console_output = output.console_output;
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
                    console_output,
                }
            }
        }
    }

    fn resolve_agent(
        &self,
        model_profile: &str,
        agent_profile: Option<&str>,
        agent: &AgentId,
        location: &crate::execution::ExecutionLocation,
        available_depth: usize,
        capabilities: &CapabilitySet,
    ) -> Result<(ModelProfile, Vec<SystemSegment>), HarnessError> {
        let mut selected_model = model_profile.to_owned();
        let profile_instructions = if let Some(name) = agent_profile {
            let profile = self
                .harness
                .agent_profiles
                .get(name)
                .ok_or_else(|| HarnessError::UnknownAgentProfile(name.to_owned()))?;
            if let Some(name) = &profile.model_profile {
                selected_model.clone_from(name);
            }
            Some(profile.instructions.as_str())
        } else {
            None
        };
        let profile = self
            .harness
            .model_profiles
            .get(&selected_model)
            .cloned()
            .ok_or(HarnessError::UnknownModelProfile(selected_model))?;
        let system = vec![prompt::system_segment(
            &self.harness.instructions,
            profile_instructions,
            agent,
            location,
            available_depth,
            capabilities,
        )];
        Ok((profile, system))
    }

    async fn run_agent(self: Arc<Self>, agent_loop: AgentLoop) {
        let AgentLoop {
            id,
            owner_job,
            profile,
            system,
            mut history,
            one_shot,
            location,
            capabilities,
            mut rx,
        } = agent_loop;
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
        let mut child_done: Option<oneshot::Sender<Result<String, String>>> = None;
        let mut child_answer = None;
        loop {
            let command = tokio::select! {
                biased;
                () = owner_cancellation.cancelled() => {
                    if let Some(done) = child_done.take() { let _ = done.send(Err("child agent cancelled".to_owned())); }
                    let _ = self.store.append(id.clone(), SessionEvent::AgentInterrupted).await;
                    break;
                }
                command = rx.recv() => match command { Some(command) => command, None => break },
            };
            let (content, done) = match command {
                AgentCommand::Shutdown => {
                    let _ = self
                        .store
                        .append(id.clone(), SessionEvent::AgentInterrupted)
                        .await;
                    break;
                }
                AgentCommand::Input { content, done } => (content, done),
                AgentCommand::JobsReady => {
                    let pending = match self.jobs.take_pending(&id).await {
                        Ok(pending) if !pending.is_empty() => pending,
                        Ok(_)
                            if one_shot
                                && child_answer.is_some()
                                && !self.jobs.has_running(&id).await =>
                        {
                            if let Some(done) = child_done.take() {
                                let _ =
                                    done.send(Ok(child_answer.take().expect("child has answered")));
                            }
                            let _ = self
                                .store
                                .append(id.clone(), SessionEvent::AgentCompleted)
                                .await;
                            break;
                        }
                        _ => continue,
                    };
                    let presented = pending
                        .iter()
                        .map(|job| job.presented(&capabilities))
                        .collect::<Result<Vec<_>, _>>()
                        .unwrap_or_default();
                    let content = vec![UserContent::Runtime {
                        text: format!(
                            "<skyhook_job_events>\n{}\n</skyhook_job_events>",
                            serde_json::to_string(&presented).unwrap_or_else(|_| "[]".to_owned())
                        ),
                    }];
                    (content, None)
                }
            };
            let done = if one_shot {
                if done.is_some() {
                    child_done = done;
                }
                None
            } else {
                done
            };
            let cancellation = self.begin_turn(&id);
            let message = Message::User(content);
            if let Err(error) = self.commit(&id, message.clone()).await {
                if let Some(done) = done.or_else(|| child_done.take()) {
                    let _ = done.send(Err(error.to_string()));
                }
                if one_shot {
                    self.interrupt_tree(&id).await;
                    break;
                }
                continue;
            }
            history.push(message);
            let result = tokio::select! {
                biased;
                () = owner_cancellation.cancelled() => Err(HarnessError::Interrupted),
                result = self.run_turn(
                    TurnContext {
                        agent: &id,
                        profile: &profile,
                        system: &system,
                        owner_job,
                        cancellation: &cancellation,
                        location: &location,
                        capabilities: &capabilities,
                    },
                    &mut history,
                ) => result,
            };
            if let Some(done) = done {
                let _ = done.send(
                    result
                        .as_ref()
                        .map(Clone::clone)
                        .map_err(ToString::to_string),
                );
            }
            if one_shot && result.is_err() {
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
            if one_shot {
                child_answer = result.as_ref().ok().cloned();
            }
            if one_shot && !self.jobs.has_running(&id).await {
                if let Some(done) = child_done.take() {
                    let _ = done.send(
                        result
                            .as_ref()
                            .map(Clone::clone)
                            .map_err(ToString::to_string),
                    );
                }
                let terminal = if result.is_ok() {
                    SessionEvent::AgentCompleted
                } else {
                    SessionEvent::AgentInterrupted
                };
                let _ = self.store.append(id.clone(), terminal).await;
                break;
            }
        }
        self.agents
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
    }

    async fn run_turn(
        &self,
        turn: TurnContext<'_>,
        history: &mut Vec<Message>,
    ) -> Result<String, HarnessError> {
        let TurnContext {
            agent,
            profile,
            system,
            owner_job,
            cancellation,
            location,
            capabilities,
        } = turn;
        if !profile.supports_images && contains_images(history) {
            return Err(HarnessError::ImagesUnsupported(profile.model.clone()));
        }
        let provider = self
            .harness
            .providers
            .get(&profile.provider)
            .cloned()
            .ok_or_else(|| HarnessError::UnknownProvider(profile.provider.clone()))?;
        let mut final_text = String::new();
        loop {
            if cancellation.is_cancelled() {
                return Err(HarnessError::Interrupted);
            }
            let mut request_messages = history.clone();
            request_messages.push(Message::User(vec![
                prompt::runtime_state_content(&self.jobs, &self.todos, agent, capabilities).await,
            ]));
            self.hydrate_images(&mut request_messages).await?;
            let request = ModelRequest {
                model: profile.model.clone(),
                system: system.to_vec(),
                messages: request_messages,
                tools: self
                    .executor
                    .clone()
                    .with_capabilities(capabilities.clone())
                    .surface()
                    .definitions(),
                reasoning: profile.reasoning.clone(),
                max_output_tokens: profile.max_output_tokens,
                correlation: Some(agent.to_string()),
            };
            let mut response = tokio::select! {
                response = provider.invoke(request) => response?,
                () = cancellation.cancelled() => return Err(HarnessError::Interrupted),
            };
            let mut blocks = Vec::new();
            let mut streamed_text = String::new();
            let mut usage = Usage::default();
            loop {
                let chunk = tokio::select! {
                    chunk = poll_fn(|context| response.as_mut().poll_chunk(context)) => chunk,
                    () = cancellation.cancelled() => return Err(HarnessError::Interrupted),
                };
                let Some(chunk) = chunk else {
                    break;
                };
                match chunk? {
                    ResponseChunk::TextDelta { text } => {
                        let _ = self.events.send(RuntimeEvent::TextDelta {
                            agent: agent.clone(),
                            text: text.clone(),
                        });
                        streamed_text.push_str(&text);
                    }
                    ResponseChunk::ReasoningDelta { text } => {
                        let _ = self.events.send(RuntimeEvent::ReasoningDelta {
                            agent: agent.clone(),
                            text,
                        });
                    }
                    ResponseChunk::Block { block } => blocks.push(block),
                    ResponseChunk::Usage { usage: value } => usage = value,
                }
            }
            let response = finish_response(blocks, streamed_text, usage)?;
            final_text.push_str(&response.text);
            let assistant = Message::Assistant(response.blocks);
            self.commit(agent, assistant.clone()).await?;
            history.push(assistant);
            self.store
                .append(
                    agent.clone(),
                    SessionEvent::Usage {
                        usage: response.usage,
                    },
                )
                .await?;
            self.usage.lock().await.accumulate(response.usage);
            if response.calls.is_empty() {
                let _ = self.events.send(RuntimeEvent::TurnCompleted {
                    agent: agent.clone(),
                    text: final_text.clone(),
                });
                return Ok(final_text);
            }
            self.questions
                .prepare_question_batch(agent, &response.calls)
                .await;
            let results = join_all(
                response
                    .calls
                    .iter()
                    .map(|call| self.execute_call(agent, owner_job, call, location, capabilities)),
            )
            .await;
            let tools = Message::Tool(results);
            self.commit(agent, tools.clone()).await?;
            history.push(tools);
        }
    }

    async fn commit(&self, agent: &AgentId, message: Message) -> Result<(), SessionError> {
        self.store
            .append(agent.clone(), SessionEvent::MessageCommitted { message })
            .await?;
        Ok(())
    }

    async fn hydrate_images(&self, messages: &mut [Message]) -> Result<(), HarnessError> {
        for message in messages {
            match message {
                Message::User(content) => {
                    for item in content {
                        if let UserContent::Image { image } = item {
                            hydrate_image(&self.store, image).await?;
                        }
                    }
                }
                Message::Tool(results) => {
                    for result in results {
                        for image in &mut result.images {
                            hydrate_image(&self.store, image).await?;
                        }
                    }
                }
                Message::Assistant(_) => {}
            }
        }
        Ok(())
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

    fn agent_sender(&self, id: &AgentId) -> Option<mpsc::Sender<AgentCommand>> {
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
    blocks: Vec<AssistantContent>,
    usage: Usage,
    calls: Vec<ToolCall>,
    text: String,
}

fn finish_response(
    mut blocks: Vec<AssistantContent>,
    streamed_text: String,
    usage: Usage,
) -> Result<FoldedResponse, HarnessError> {
    if !streamed_text.is_empty()
        && !blocks
            .iter()
            .any(|block| matches!(block, AssistantContent::Text { .. }))
    {
        blocks.insert(
            0,
            AssistantContent::Text {
                text: streamed_text,
            },
        );
    }
    if blocks.is_empty() {
        return Err(HarnessError::EmptyResponse);
    }
    let text = blocks
        .iter()
        .filter_map(|block| match block {
            AssistantContent::Text { text } => Some(text.as_str()),
            AssistantContent::Reasoning { .. } | AssistantContent::ToolCall(_) => None,
        })
        .collect::<String>();
    let calls = blocks
        .iter()
        .filter_map(|block| match block {
            AssistantContent::ToolCall(call) => Some(call.clone()),
            AssistantContent::Text { .. } | AssistantContent::Reasoning { .. } => None,
        })
        .collect();
    Ok(FoldedResponse {
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

async fn hydrate_image(
    store: &SessionStore,
    image: &mut crate::media::ImageReference,
) -> Result<(), HarnessError> {
    if image.data_base64.is_none() {
        let bytes = store.read_blob(image).await?;
        image.data_base64 = Some(base64::engine::general_purpose::STANDARD.encode(bytes));
    }
    Ok(())
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
        provider::protocol::ToolCall,
        provider::{ProviderFuture, ResponseHandle},
    };

    struct ScriptedProvider {
        responses: StdMutex<VecDeque<Vec<ResponseChunk>>>,
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
    }

    #[derive(Default)]
    struct HangingProvider {
        invocations: Option<Arc<AtomicUsize>>,
    }

    struct BlockingFirstProvider {
        calls: AtomicUsize,
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
        release: Arc<tokio::sync::Semaphore>,
    }

    struct RecordingQuestions {
        batches: Arc<StdMutex<Vec<Vec<Question>>>>,
        answer: serde_json::Value,
    }

    struct GatedResponse {
        release: Pin<Box<dyn Future<Output = ()> + Send>>,
        emitted: bool,
    }

    impl ResponseHandle for GatedResponse {
        fn poll_chunk(
            mut self: Pin<&mut Self>,
            context: &mut std::task::Context<'_>,
        ) -> Poll<Option<Result<ResponseChunk, crate::provider::ProviderError>>> {
            if self.emitted {
                return Poll::Ready(None);
            }
            if self.release.as_mut().poll(context).is_pending() {
                return Poll::Pending;
            }
            self.emitted = true;
            Poll::Ready(Some(Ok(ResponseChunk::TextDelta {
                text: "initial".to_owned(),
            })))
        }
    }

    impl Provider for HangingProvider {
        fn invoke(&self, _request: ModelRequest) -> ProviderFuture {
            if let Some(invocations) = &self.invocations {
                invocations.fetch_add(1, Ordering::SeqCst);
            }
            Box::pin(async { Ok(Box::pin(stream::pending()) as Pin<Box<dyn ResponseHandle>>) })
        }
    }

    impl Provider for ScriptedProvider {
        fn invoke(&self, request: ModelRequest) -> ProviderFuture {
            self.requests.lock().unwrap().push(request);
            let chunks = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted provider response");
            Box::pin(async move {
                Ok(Box::pin(stream::iter(chunks.into_iter().map(Ok)))
                    as Pin<Box<dyn ResponseHandle>>)
            })
        }
    }

    impl Provider for BlockingFirstProvider {
        fn invoke(&self, request: ModelRequest) -> ProviderFuture {
            self.requests.lock().unwrap().push(request);
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let release = self.release.clone();
            Box::pin(async move {
                let response: Pin<Box<dyn ResponseHandle>> = if call == 0 {
                    Box::pin(GatedResponse {
                        release: Box::pin(async move {
                            let permit = release.acquire_owned().await.unwrap();
                            permit.forget();
                        }),
                        emitted: false,
                    })
                } else {
                    Box::pin(stream::iter(vec![Ok(ResponseChunk::TextDelta {
                        text: "jobs handled".to_owned(),
                    })]))
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

    fn request_has_tool(request: &ModelRequest, name: &str) -> bool {
        request.tools.iter().any(|tool| tool.name == name)
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
                    max_output_tokens: None,
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

    #[test]
    fn response_folding_keeps_only_the_current_assistant_turn() {
        let call = ToolCall {
            id: "call-1".to_owned(),
            name: "read".to_owned(),
            arguments: json!({"path":"README.md"}),
        };
        let response = finish_response(
            vec![AssistantContent::ToolCall(call.clone())],
            "working".to_owned(),
            Usage {
                input_tokens: 10,
                output_tokens: 2,
                ..Usage::default()
            },
        )
        .unwrap();
        assert_eq!(response.text, "working");
        assert_eq!(response.calls, vec![call]);
        assert_eq!(response.blocks.len(), 2);
        assert_eq!(response.usage.input_tokens, 10);
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
            Arc::new(ScriptedProvider {
                responses: StdMutex::new(VecDeque::from([
                    vec![
                        ResponseChunk::Block {
                            block: AssistantContent::ToolCall(ToolCall {
                                id: "read-1".to_owned(),
                                name: "read".to_owned(),
                                arguments: json!({"path": "note.txt"}),
                            }),
                        },
                        ResponseChunk::Block {
                            block: AssistantContent::ToolCall(ToolCall {
                                id: "read-2".to_owned(),
                                name: "read".to_owned(),
                                arguments: json!({"path": "note.txt"}),
                            }),
                        },
                    ],
                    vec![ResponseChunk::TextDelta {
                        text: "finished".to_owned(),
                    }],
                ])),
                requests: requests.clone(),
            }),
        )
        .await;
        let session = harness.new_session().await.unwrap();
        assert_eq!(session.prompt("read the note").await.unwrap(), "finished");
        let script_output = session
            .run_script(r#"return tool.read().path("note.txt");"#)
            .await
            .unwrap();
        assert_eq!(script_output.value["content"], "hello");
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
        assert!(session.tools().get("script").is_some());
        assert!(session.tools().get("jobs").is_some());
        assert_eq!(
            session.tools().get("read").unwrap().placement(),
            crate::tool::ToolPlacement::TargetedWorkspace
        );
        assert_eq!(
            session.tools().get("jobs").unwrap().placement(),
            crate::tool::ToolPlacement::Host
        );
        let targeted = session
            .tools()
            .tools()
            .filter(|tool| tool.placement() == crate::tool::ToolPlacement::TargetedWorkspace)
            .map(|tool| tool.name())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(targeted, ["exec", "glob", "read", "search", "shell"].into());
        let mut capabilities = crate::tool::policy::CapabilitySet::default();
        capabilities.insert(crate::tool::policy::Capability::Targets);
        let surface = session.tools().surface(&capabilities);
        assert!(targeted.iter().all(|name| {
            surface.get(name).unwrap().input_schema["properties"]["target"].is_object()
        }));
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
        let read = requests[0]
            .tools
            .iter()
            .find(|tool| tool.name == "read")
            .unwrap();
        assert!(read.input_schema["properties"].get("target").is_none());
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
    async fn remote_children_use_the_shared_host_agent_loop() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let provider = Arc::new(ScriptedProvider {
            responses: StdMutex::new(VecDeque::from([
                vec![ResponseChunk::Block {
                    block: AssistantContent::ToolCall(ToolCall {
                        id: "agent-1".to_owned(),
                        name: "agent".to_owned(),
                        arguments: json!({"prompt":"inspect", "target":"build"}),
                    }),
                }],
                vec![ResponseChunk::TextDelta {
                    text: "child done".to_owned(),
                }],
                vec![ResponseChunk::TextDelta {
                    text: "root done".to_owned(),
                }],
            ])),
            requests: requests.clone(),
        });
        let mut targets = TargetsConfig::default();
        targets.entries.insert(
            "build".to_owned(),
            crate::target::TargetConfig {
                host: "build.example.com".to_owned(),
                user: None,
                port: None,
                workspace: PathBuf::from("/srv/project"),
                via: None,
                auth: crate::target::TargetAuth::Openssh,
            },
        );
        let harness = test_builder(workspace.path(), sessions.path(), provider)
            .capabilities({
                let mut capabilities = CapabilitySet::default();
                capabilities.insert(crate::tool::policy::Capability::Targets);
                capabilities
            })
            .targets_config(targets)
            .build()
            .await
            .unwrap();

        let session = harness.new_session().await.unwrap();
        assert_eq!(session.prompt("delegate").await.unwrap(), "root done");

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        let tools_with_target = requests[0]
            .tools
            .iter()
            .filter(|tool| tool.input_schema["properties"]["target"].is_object())
            .map(|tool| tool.name.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            tools_with_target,
            ["agent", "exec", "glob", "read", "search", "shell"].into()
        );
        assert!(request_has_tool(&requests[0], "targets"));
        assert!(request_has_tool(&requests[0], "target_add"));
        assert!(requests[0].system[0].text.contains(prompt::TARGET_PROMPT));
        assert!(requests[1].system[0].text.contains("\"name\":\"build\""));
        assert!(requests[1].system[0].text.contains("\"kind\":\"ssh\""));
        assert!(requests[1].system[0].text.contains("/srv/project"));
        assert!(requests[1].system[0].text.starts_with(prompt::CHILD_PROMPT));
        assert!(!requests[1].system[0].text.contains("orchestration"));
        assert!(!requests[1].system[0].text.contains("available_depth"));
        assert!(!request_has_tool(&requests[1], "agent"));
        let Message::Tool(results) = request_history(&requests[2]).last().unwrap() else {
            panic!("root must receive the child result");
        };
        assert_eq!(results[0].result["target"], "build");
        assert_eq!(results[0].result["result"], "child done");
    }

    #[tokio::test]
    async fn child_completion_waits_for_background_work_and_returns_its_updated_answer() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let call = |name: &str, arguments| {
            vec![ResponseChunk::Block {
                block: AssistantContent::ToolCall(ToolCall {
                    id: name.to_owned(),
                    name: name.to_owned(),
                    arguments,
                }),
            }]
        };
        let text = |text: &str| {
            vec![ResponseChunk::TextDelta {
                text: text.to_owned(),
            }]
        };
        let harness = test_harness(
            workspace.path(),
            sessions.path(),
            Arc::new(ScriptedProvider {
                responses: StdMutex::new(VecDeque::from([
                    call("agent", json!({"prompt":"work"})),
                    call(
                        "script",
                        json!({"source":"return await receive();", "bg":true}),
                    ),
                    text("premature child answer"),
                    text("child work completed"),
                    text("root done"),
                ])),
                requests: requests.clone(),
            }),
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
        assert_eq!(results[0].result["result"], "child work completed");
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
                calls: AtomicUsize::new(0),
                requests: Arc::new(StdMutex::new(Vec::new())),
                release: release.clone(),
            }),
        )
        .await;
        let session = harness.new_session().await.unwrap();
        let child = session.root.child(1);
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
                parent: Some(session.root.clone()),
                owner_job: None,
                model_profile: "test".to_owned(),
                agent_profile: None,
                history: Vec::new(),
                todos: None,
                one_shot: true,
                available_depth: 0,
                location: crate::execution::ExecutionLocation::root(workspace.path().to_path_buf()),
            })
            .await
            .unwrap();
        let (done, received) = oneshot::channel();
        let mut events = session.runtime.events.subscribe();
        sender
            .send(AgentCommand::Input {
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
            .finish(job, Ok(crate::tool::ToolOutput::default()), None)
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
    }

    #[tokio::test]
    async fn child_relative_workspaces_and_explicit_root_follow_the_shared_rules() {
        for (target, expected) in [(None, "nested"), (Some("root"), "")] {
            let workspace = tempfile::tempdir().unwrap();
            std::fs::create_dir(workspace.path().join("nested")).unwrap();
            let sessions = tempfile::tempdir().unwrap();
            let requests = Arc::new(StdMutex::new(Vec::new()));
            let call = |arguments| {
                vec![ResponseChunk::Block {
                    block: AssistantContent::ToolCall(ToolCall {
                        id: "agent".to_owned(),
                        name: "agent".to_owned(),
                        arguments,
                    }),
                }]
            };
            let text = || {
                vec![ResponseChunk::TextDelta {
                    text: "done".to_owned(),
                }]
            };
            let harness = test_builder(
                workspace.path(),
                sessions.path(),
                Arc::new(ScriptedProvider {
                    responses: StdMutex::new(VecDeque::from([
                        call(json!({"prompt":"child", "depth":1, "workspace":"nested"})),
                        call(json!({"prompt":"grandchild", "target":target})),
                        text(),
                        text(),
                        text(),
                    ])),
                    requests: requests.clone(),
                }),
            )
            .capabilities({
                let mut set = CapabilitySet::default();
                set.insert(crate::tool::policy::Capability::Targets);
                set
            })
            .build()
            .await
            .unwrap();
            let session = harness.new_session().await.unwrap();
            session.prompt("delegate").await.unwrap();
            let requests = requests.lock().unwrap();
            let expected_path = std::fs::canonicalize(workspace.path().join(expected)).unwrap();
            assert!(
                requests[2].system[0]
                    .text
                    .contains(&format!("\"workspace\":{}", json!(expected_path)))
            );
            assert!(!requests[2].messages.iter().any(|message| matches!(message, Message::User(content) if content.iter().any(|item| matches!(item, UserContent::Text {text} if text == "delegate")))));
        }
    }

    #[tokio::test]
    async fn local_children_honor_absolute_workspace_overrides() {
        let workspace = tempfile::tempdir().unwrap();
        let child_workspace = tempfile::tempdir().unwrap();
        std::fs::write(
            child_workspace.path().join("note.txt"),
            "from child workspace",
        )
        .unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let child_path = std::fs::canonicalize(child_workspace.path()).unwrap();
        let provider = Arc::new(ScriptedProvider {
            responses: StdMutex::new(VecDeque::from([
                vec![ResponseChunk::Block {
                    block: AssistantContent::ToolCall(ToolCall {
                        id: "agent-local".to_owned(),
                        name: "agent".to_owned(),
                        arguments: json!({"prompt":"inspect", "workspace": child_path}),
                    }),
                }],
                vec![ResponseChunk::Block {
                    block: AssistantContent::ToolCall(ToolCall {
                        id: "read-child".to_owned(),
                        name: "read".to_owned(),
                        arguments: json!({"path":"note.txt"}),
                    }),
                }],
                vec![ResponseChunk::TextDelta {
                    text: "child done".to_owned(),
                }],
                vec![ResponseChunk::TextDelta {
                    text: "root done".to_owned(),
                }],
            ])),
            requests: requests.clone(),
        });
        let harness = test_harness(workspace.path(), sessions.path(), provider).await;
        let session = harness.new_session().await.unwrap();
        assert_eq!(session.prompt("delegate").await.unwrap(), "root done");

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 4);
        assert!(
            requests[1].system[0]
                .text
                .contains(&child_path.to_string_lossy().into_owned())
        );
        let Message::Tool(results) = request_history(&requests[2]).last().unwrap() else {
            panic!("child must receive its read result");
        };
        assert_eq!(results[0].result["content"], "from child workspace");
    }

    #[tokio::test]
    async fn delegated_depth_controls_child_agent_visibility() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let harness = test_harness(
            workspace.path(),
            sessions.path(),
            Arc::new(ScriptedProvider {
                responses: StdMutex::new(VecDeque::from([
                    vec![ResponseChunk::Block {
                        block: AssistantContent::ToolCall(ToolCall {
                            id: "root-agent".to_owned(),
                            name: "agent".to_owned(),
                            arguments: json!({"prompt":"delegate once", "depth":1}),
                        }),
                    }],
                    vec![ResponseChunk::Block {
                        block: AssistantContent::ToolCall(ToolCall {
                            id: "child-agent".to_owned(),
                            name: "agent".to_owned(),
                            arguments: json!({"prompt":"inspect directly", "todos":["Inspect directly"]}),
                        }),
                    }],
                    vec![ResponseChunk::TextDelta {
                        text: "leaf done".to_owned(),
                    }],
                    vec![ResponseChunk::TextDelta {
                        text: "child done".to_owned(),
                    }],
                    vec![ResponseChunk::TextDelta {
                        text: "root done".to_owned(),
                    }],
                ])),
                requests: requests.clone(),
            }),
        )
        .await;

        let session = harness.new_session().await.unwrap();
        assert_eq!(session.prompt("delegate").await.unwrap(), "root done");

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 5);
        assert!(request_has_tool(&requests[0], "agent"));
        assert!(requests[0].system[0].text.contains("\"available_depth\":4"));
        assert!(requests[1].system[0].text.starts_with(prompt::CHILD_PROMPT));
        assert!(requests[1].system[0].text.contains("\"available_depth\":1"));
        assert!(request_has_tool(&requests[1], "agent"));
        assert!(requests[2].system[0].text.starts_with(prompt::CHILD_PROMPT));
        assert!(!requests[2].system[0].text.contains("available_depth"));
        assert!(!request_has_tool(&requests[2], "agent"));
        let Message::User(content) = requests[2].messages.last().unwrap() else {
            panic!("grandchild runtime state");
        };
        assert!(
            matches!(&content[0], UserContent::Runtime { text } if text.contains(r#""todos":[{"text":"Inspect directly","status":"pending"}]"#))
        );
    }

    #[tokio::test]
    async fn delegated_depth_cannot_exceed_the_callers_budget() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let harness = test_harness(
            workspace.path(),
            sessions.path(),
            Arc::new(ScriptedProvider {
                responses: StdMutex::new(VecDeque::from([
                    vec![ResponseChunk::Block {
                        block: AssistantContent::ToolCall(ToolCall {
                            id: "agent-too-deep".to_owned(),
                            name: "agent".to_owned(),
                            arguments: json!({"prompt":"too deep", "depth":4}),
                        }),
                    }],
                    vec![ResponseChunk::TextDelta {
                        text: "root done".to_owned(),
                    }],
                ])),
                requests: requests.clone(),
            }),
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
            Arc::new(ScriptedProvider {
                responses: StdMutex::new(VecDeque::from([
                    vec![ResponseChunk::Block {
                        block: AssistantContent::ToolCall(ToolCall {
                            id: "root-agent".to_owned(),
                            name: "agent".to_owned(),
                            arguments: json!({"prompt":"try hidden delegation"}),
                        }),
                    }],
                    vec![
                        ResponseChunk::Block {
                            block: AssistantContent::ToolCall(ToolCall {
                                id: "hidden-agent".to_owned(),
                                name: "agent".to_owned(),
                                arguments: json!({"prompt":"escape"}),
                            }),
                        },
                        ResponseChunk::Block {
                            block: AssistantContent::ToolCall(ToolCall {
                                id: "script-agent".to_owned(),
                                name: "script".to_owned(),
                                arguments: json!({
                                    "source":"return tool.agent({prompt: 'escape'});"
                                }),
                            }),
                        },
                    ],
                    vec![ResponseChunk::TextDelta {
                        text: "child done".to_owned(),
                    }],
                    vec![ResponseChunk::TextDelta {
                        text: "root done".to_owned(),
                    }],
                ])),
                requests: requests.clone(),
            }),
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
            Arc::new(ScriptedProvider {
                responses: StdMutex::new(VecDeque::from([
                    vec![
                        ResponseChunk::Block {
                            block: AssistantContent::ToolCall(ToolCall {
                                id: "ask-1".to_owned(),
                                name: "ask".to_owned(),
                                arguments: json!({"id":"first", "prompt":"First?", "options":[]}),
                            }),
                        },
                        ResponseChunk::Block {
                            block: AssistantContent::ToolCall(ToolCall {
                                id: "ask-2".to_owned(),
                                name: "ask".to_owned(),
                                arguments: json!({"id":"second", "prompt":"Second?", "options":[]}),
                            }),
                        },
                    ],
                    vec![ResponseChunk::TextDelta {
                        text: "done".to_owned(),
                    }],
                ])),
                requests: requests.clone(),
            }),
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
        assert_eq!(results[0].result, "yes");
        assert_eq!(results[1].result, json!({"value":2}));

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
    async fn interrupt_stops_a_pending_provider_turn() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let invocations = Arc::new(AtomicUsize::new(0));
        let harness = test_harness(
            workspace.path(),
            sessions.path(),
            Arc::new(HangingProvider {
                invocations: Some(invocations.clone()),
            }),
        )
        .await;
        let session = harness.new_session().await.unwrap();
        let pending = tokio::spawn({
            let session = session.clone();
            async move { session.prompt("wait forever").await }
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while invocations.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        session.interrupt().await;
        let error = tokio::time::timeout(Duration::from_secs(1), pending)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("interrupted"));
    }

    #[tokio::test]
    async fn hidden_job_controls_are_documented_and_execute_only_through_scripts() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let harness = test_harness(
            workspace.path(),
            sessions.path(),
            Arc::new(HangingProvider::default()),
        )
        .await;
        let session = harness.new_session().await.unwrap();
        let definitions = session
            .tools()
            .surface(&CapabilitySet::default())
            .definitions();
        let names = definitions
            .iter()
            .map(|definition| definition.name.as_str())
            .collect::<Vec<_>>();
        assert!(names.contains(&"jobs"));
        assert!(names.contains(&"wait"));
        for hidden in ["job_inspect", "job_send", "job_cancel", "job_events"] {
            assert!(!names.contains(&hidden));
        }
        let description = &definitions
            .iter()
            .find(|definition| definition.name == "script")
            .unwrap()
            .description;
        for signature in [
            "tool.job(id).inspect()",
            "tool.job(id).wait({timeout?",
            "tool.job(id).send({value: JSON})",
            "tool.job(id).cancel()",
            "tool.job(id).events({after?",
        ] {
            assert!(
                description.contains(signature),
                "missing {signature}: {description}"
            );
        }
        assert!(!description.contains("$schema"));

        let direct = session
            .runtime
            .executor
            .execute_model(session.root.clone(), "job_inspect", json!({"job": 1}), None)
            .await
            .err()
            .unwrap();
        assert!(direct.to_string().contains("not exposed"));

        let running = session
            .runtime
            .executor
            .execute(
                session.root.clone(),
                "script",
                json!({"source":"return 7;", "bg":true}),
                None,
            )
            .await
            .unwrap();
        let inspected = session
            .run_script(format!(
                "await tool.job({}).wait({{timeout:2}}); return tool.job({}).inspect();",
                running.job, running.job
            ))
            .await
            .unwrap();
        assert_eq!(inspected.value["id"], running.job.get());
    }

    #[tokio::test]
    async fn resumed_sessions_do_not_append_empty_state_or_rewrite_history() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let harness = test_harness(
            workspace.path(),
            sessions.path(),
            Arc::new(ScriptedProvider {
                responses: StdMutex::new(VecDeque::from([
                    vec![
                        ResponseChunk::TextDelta {
                            text: "done".to_owned(),
                        },
                        ResponseChunk::Usage {
                            usage: Usage {
                                input_tokens: 10,
                                cached_input_tokens: 2,
                                output_tokens: 3,
                            },
                        },
                    ],
                    vec![
                        ResponseChunk::TextDelta {
                            text: "done".to_owned(),
                        },
                        ResponseChunk::Usage {
                            usage: Usage {
                                input_tokens: 20,
                                cached_input_tokens: 4,
                                output_tokens: 5,
                            },
                        },
                    ],
                ])),
                requests: requests.clone(),
            }),
        )
        .await;
        let session = harness.new_session().await.unwrap();
        let session_id = session.id();
        assert_eq!(session.prompt("first").await.unwrap(), "done");
        assert_eq!(
            session.usage().await,
            Usage {
                input_tokens: 10,
                cached_input_tokens: 2,
                output_tokens: 3,
            }
        );
        session.shutdown().await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while session.runtime.agent_sender(&session.root).is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drop(session);

        let resumed = harness.resume_session(session_id).await.unwrap();
        assert_eq!(
            resumed.usage().await,
            Usage {
                input_tokens: 10,
                cached_input_tokens: 2,
                output_tokens: 3,
            }
        );
        assert_eq!(resumed.prompt("second").await.unwrap(), "done");
        assert_eq!(
            resumed.usage().await,
            Usage {
                input_tokens: 30,
                cached_input_tokens: 6,
                output_tokens: 8,
            }
        );
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(request_history(&requests[1]).starts_with(request_history(&requests[0])));
        assert_eq!(runtime_state_count(&requests[0].messages), 1);
        assert_eq!(runtime_state_count(&requests[1].messages), 1);
    }

    #[tokio::test]
    async fn background_completions_batch_events_with_one_state_snapshot() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let harness = test_harness(
            workspace.path(),
            sessions.path(),
            Arc::new(BlockingFirstProvider {
                calls: AtomicUsize::new(0),
                requests: requests.clone(),
                release: release.clone(),
            }),
        )
        .await;
        let session = harness.new_session().await.unwrap();
        let prompt = tokio::spawn({
            let session = session.clone();
            async move { session.prompt("start").await }
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while requests.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        for value in ["first", "second"] {
            let lease = session
                .runtime
                .jobs
                .create(crate::job::JobSpec {
                    background: true,
                    name: Some(value.to_owned()),
                    ..crate::job::JobSpec::test(session.root.clone(), "test")
                })
                .await
                .unwrap();
            session
                .runtime
                .jobs
                .transition(lease.id, crate::job::JobState::Running)
                .await
                .unwrap();
            session
                .runtime
                .jobs
                .finish(
                    lease.id,
                    Ok(crate::tool::ToolOutput::new(json!({"value": value}))),
                    None,
                )
                .await
                .unwrap();
        }
        tokio::task::yield_now().await;
        release.add_permits(1);
        assert_eq!(prompt.await.unwrap().unwrap(), "initial");
        tokio::time::timeout(Duration::from_secs(1), async {
            while requests.lock().unwrap().len() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        session.root_tx.send(AgentCommand::JobsReady).await.unwrap();
        assert_eq!(session.prompt("barrier").await.unwrap(), "jobs handled");

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(request_history(&requests[1]).starts_with(request_history(&requests[0])));
        let Message::User(content) = request_history(&requests[1]).last().unwrap() else {
            panic!("job wakeup must append one runtime user message");
        };
        assert_eq!(content.len(), 1);
        let UserContent::Runtime { text: events } = &content[0] else {
            panic!("job events must be runtime content");
        };
        assert_eq!(events.matches("\"id\"").count(), 2);
        assert!(events.contains(r#""name":"first""#));
        assert!(events.contains(r#""name":"second""#));
        assert_eq!(runtime_state_count(&requests[1].messages), 1);
        let event_messages = requests[2]
            .messages
            .iter()
            .filter_map(|message| match message {
                Message::User(content) => Some(content),
                Message::Assistant(_) | Message::Tool(_) => None,
            })
            .flatten()
            .filter(|content| {
                matches!(
                    content,
                    UserContent::Runtime { text } if text.contains("<skyhook_job_events>")
                )
            })
            .count();
        assert_eq!(event_messages, 1, "redundant wakeups must commit nothing");
        assert_eq!(runtime_state_count(&requests[2].messages), 1);
    }

    #[tokio::test]
    async fn child_questions_route_through_the_stable_agent_job() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let harness = test_harness(
            workspace.path(),
            sessions.path(),
            Arc::new(HangingProvider::default()),
        )
        .await;
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
        assert_eq!(ask.input.recv().await.unwrap(), json!("yes"));
        assert_eq!(ask_two.input.recv().await.unwrap(), json!(2));
        session
            .runtime
            .questions
            .resolve_child_question(owner)
            .await
            .unwrap();
        assert_eq!(
            session.runtime.jobs.snapshot(owner).await.unwrap().state,
            crate::job::JobState::Running
        );
    }

    #[tokio::test]
    async fn script_receive_requires_background_and_accepts_job_input() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let harness = test_harness(
            workspace.path(),
            sessions.path(),
            Arc::new(HangingProvider::default()),
        )
        .await;
        let session = harness.new_session().await.unwrap();
        let error = session
            .run_script("return await receive();")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("requires the script tool"));

        let running = session
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
        session
            .runtime
            .jobs
            .send(running.job, json!("hello"))
            .await
            .unwrap();
        let completed = session
            .runtime
            .jobs
            .wait(running.job, Some(Duration::from_secs(2)), true)
            .await
            .unwrap();
        assert_eq!(completed.output, Some(json!("hello")));
    }

    #[tokio::test]
    async fn script_console_reaches_the_agent_on_success_and_failure() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("note.txt"), "hello").unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let scripts = [
            (
                "success",
                r#"console.log("Processed", 12, {files: true}); return {changed:3};"#,
            ),
            (
                "failure",
                r#"console.log("before failure"); throw new Error("boom");"#,
            ),
            ("silent", "return 42;"),
            (
                "serialization",
                "console.log('before serialization'); return {bad: undefined};",
            ),
            (
                "pool",
                "const inputs=['missing-a','note.txt','missing-b','note.txt']; const results=[]; for await(const {index,value} of new WorkPool(2).map(inputs, path=>tool.read({path}))) results.push({index,content:value.content}); return results.sort((a,b)=>a.index-b.index);",
            ),
        ];
        let harness = test_harness(
            workspace.path(),
            sessions.path(),
            Arc::new(ScriptedProvider {
                responses: StdMutex::new(VecDeque::from([
                    scripts
                        .into_iter()
                        .map(|(id, source)| ResponseChunk::Block {
                            block: AssistantContent::ToolCall(ToolCall {
                                id: id.to_owned(),
                                name: "script".to_owned(),
                                arguments: json!({"source": source}),
                            }),
                        })
                        .collect(),
                    vec![ResponseChunk::TextDelta {
                        text: "done".to_owned(),
                    }],
                ])),
                requests: requests.clone(),
            }),
        )
        .await;
        let session = harness.new_session().await.unwrap();
        assert_eq!(session.prompt("run scripts").await.unwrap(), "done");
        let requests = requests.lock().unwrap();
        let results = requests[1]
            .messages
            .iter()
            .filter_map(|message| {
                if let Message::Tool(results) = message {
                    Some(results)
                } else {
                    None
                }
            })
            .flatten()
            .collect::<Vec<_>>();
        let result = |id: &str| *results.iter().find(|result| result.call_id == id).unwrap();
        assert_eq!(result("success").result, json!({"changed":3}));
        assert_eq!(
            result("success").console_output,
            "Processed 12 {\"files\":true}\n"
        );
        assert!(!result("success").is_error);
        assert!(result("failure").is_error);
        assert_eq!(result("failure").console_output, "before failure\n");
        assert_eq!(
            result("failure").result["output"]["failure"]["message"],
            "boom"
        );
        assert_eq!(result("silent").result, json!(42));
        assert!(
            serde_json::to_value(result("silent"))
                .unwrap()
                .get("console_output")
                .is_none()
        );
        assert!(result("serialization").is_error);
        assert_eq!(
            result("serialization").console_output,
            "before serialization\n"
        );
        assert!(!result("pool").is_error);
        assert_eq!(
            result("pool").result,
            json!([
                {"index":1,"content":"hello"}, {"index":3,"content":"hello"}
            ])
        );
        assert!(
            result("pool")
                .console_output
                .contains("WorkPool item 0 failed:")
        );
        assert!(
            result("pool")
                .console_output
                .contains("WorkPool item 2 failed:")
        );
    }

    #[tokio::test]
    async fn background_script_console_survives_wait_and_replay() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let harness = test_harness(
            workspace.path(),
            sessions.path(),
            Arc::new(HangingProvider::default()),
        )
        .await;
        let session = harness.new_session().await.unwrap();
        for source in [
            "console.log('background'); return 7;",
            "console.log('background'); throw new Error('failed');",
        ] {
            let running = session
                .runtime
                .executor
                .execute(
                    session.root.clone(),
                    "script",
                    json!({"source":source, "bg":true}),
                    None,
                )
                .await
                .unwrap();
            let completed = session
                .run_script(format!(
                    "return tool.job({}).wait({{timeout:5}});",
                    running.job.get()
                ))
                .await
                .unwrap();
            assert_eq!(completed.value["console_output"], "background\n");
            if source.contains("return") {
                assert_eq!(completed.value["output"], 7);
            } else {
                assert_eq!(completed.value["state"], "failed");
            }
        }
        let store = &session.runtime.store;
        store.close().await.unwrap();
        let (store, records) = SessionStore::open(sessions.path(), store.id())
            .await
            .unwrap();
        let restored = JobManager::restore(store, &records).await.unwrap();
        let scripts = restored
            .list(&session.root)
            .await
            .into_iter()
            .filter(|job| job.console_output == "background\n")
            .collect::<Vec<_>>();
        assert_eq!(scripts.len(), 2);
    }

    #[tokio::test]
    async fn script_messages_preserve_order_across_calls_and_cancel_waiting_receivers() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let harness = test_harness(
            workspace.path(),
            sessions.path(),
            Arc::new(HangingProvider::default()),
        )
        .await;
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
const pending = await tool.job({id}).wait({{timeout:1}});
return {{accepted, state:pending.state}};
"#
            ))
            .await
            .unwrap();
        assert_eq!(
            queued.value["accepted"],
            json!(vec![json!({"accepted":true}); 3])
        );
        assert_eq!(queued.value["state"], "running");
        let completed = session
            .run_script(format!(
                r#"
await tool.job({id}).send({{value:"last"}});
return tool.job({id}).wait({{timeout:5}});
"#
            ))
            .await
            .unwrap();
        assert_eq!(completed.value["state"], "completed");
        assert_eq!(
            completed.value["output"],
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
        let cancelled = session
            .run_script(format!(
                r#"
await tool.job({id}).wait({{timeout:1}});
await tool.job({id}).cancel();
return tool.job({id}).wait({{timeout:5}});
"#
            ))
            .await
            .unwrap();
        assert_eq!(cancelled.value["state"], "cancelled");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn script_command_events_are_live_replayable_and_paginated() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let harness = test_harness(
            workspace.path(),
            sessions.path(),
            Arc::new(HangingProvider::default()),
        )
        .await;
        let session = harness.new_session().await.unwrap();
        let output = tokio::time::timeout(Duration::from_secs(15), session.run_script(r#"
const job = await tool.shell({
  command: "printf 'out-before\\n'; printf 'err-before\\n' >&2; while [ ! -f release ]; do sleep 0.01; done; printf 'out-after\\n'; printf 'err-after\\n' >&2",
  timeout:10, bg:true
});
let live;
do { live = await tool.job(job.id).events({limit:1}); } while (!live.events.length);
const pending = await tool.job(job.id).inspect();
await tool.write({path:"release", content:"go"});
const completed = await tool.job(job.id).wait({timeout:10});
const first = await tool.job(job.id).events({after:0, limit:1});
const replay = await tool.job(job.id).events({after:0, limit:1});
const events = [];
let cursor = 0, page;
do {
  page = await tool.job(job.id).events({after:cursor, limit:1});
  if (page.events.length) {
    if (page.events.length !== 1 || page.next <= cursor) throw new Error("invalid event cursor");
    events.push(...page.events);
  }
  cursor = page.next;
} while (page.events.length);
return {live, pending:pending.state, completed, first, replay, events, tail:page};
"#)).await.unwrap().unwrap();
        let value = output.value;
        assert_eq!(value["pending"], "running");
        assert_eq!(value["live"]["state"], "running");
        assert_eq!(value["completed"]["state"], "completed");
        assert_eq!(value["completed"]["output"]["exit_code"], 0);
        assert_eq!(value["first"], value["replay"]);
        let events = value["events"].as_array().unwrap();
        assert!(events.len() >= 2);
        for (index, event) in events.iter().enumerate() {
            assert_eq!(event["sequence"], (index + 1) as u64);
        }
        for (kind, expected) in [
            ("stdout", "out-before\nout-after\n"),
            ("stderr", "err-before\nerr-after\n"),
        ] {
            let text = events
                .iter()
                .filter(|event| event["kind"] == kind)
                .map(|event| event["data"]["text"].as_str().unwrap())
                .collect::<String>();
            assert_eq!(text, expected);
            assert_eq!(value["completed"]["output"][kind], expected);
        }
        assert_eq!(value["tail"]["events"], json!([]));
        assert_eq!(value["tail"]["next"], events.last().unwrap()["sequence"]);
        assert_eq!(value["tail"]["state"], "completed");
    }
}
