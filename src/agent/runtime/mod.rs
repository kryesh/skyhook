//! Provider-neutral session runtime and agent loop.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use base64::Engine as _;
use futures_util::{future::join_all, future::poll_fn};
use serde_json::json;
use tokio::{
    fs,
    sync::{Mutex, RwLock, broadcast, mpsc, oneshot},
};

use crate::{
    agent::profile::AgentProfile,
    identity::{AgentId, JobId, SessionId},
    job::JobManager,
    media::{MAX_IMAGE_BYTES, MAX_IMAGE_BYTES_PER_SUBMISSION, MAX_IMAGES_PER_SUBMISSION},
    provider::Provider,
    provider::profile::ModelProfile,
    provider::protocol::{
        AssistantContent, Message, ModelRequest, ResponseChunk, SystemSegment, ToolCall,
        ToolDefinition, ToolResult, Usage, UserContent,
    },
    remote::protocol::{RemoteAgentSpec, RemoteAgentStep},
    remote::{RejectSensitivePrompts, RemoteManager, SensitivePromptHandler},
    session::{EventRecord, SessionError, SessionEvent, SessionStore},
    target::{TargetDefinition, TargetRegistry, TargetsConfig, import_ssh_targets},
    tool::builtins::{HostSkills, install_script_tool_weak, register_coding_tools},
    tool::policy::{AllowAll, Policy},
    tool::{ToolRegistry, ToolRegistryBuilder, executor::ToolExecutor},
};

pub use super::error::HarnessError;
use super::interaction::{QuestionHandler, RuntimeEvent};

mod prompt;
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
    targets: TargetsConfig,
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
            targets: TargetsConfig::default(),
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
    pub fn targets_config(mut self, targets: TargetsConfig) -> Self {
        self.targets = targets;
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
        let mut target_definitions = if self.targets.import_ssh_config {
            import_ssh_targets().await?
        } else {
            Vec::new()
        };
        for definition in self.targets.definitions()? {
            if let Some(existing) = target_definitions
                .iter_mut()
                .find(|target| target.name == definition.name)
            {
                *existing = definition;
            } else {
                target_definitions.push(definition);
            }
        }
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
                target_definitions,
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
    target_definitions: Vec<TargetDefinition>,
    sensitive_prompts: Arc<dyn SensitivePromptHandler>,
}

impl Harness {
    pub async fn new_session(&self) -> Result<SessionHandle, HarnessError> {
        let store = SessionStore::create(&self.inner.session_root).await?;
        let root = AgentId::root(store.id());
        store
            .append(
                root.clone(),
                SessionEvent::SessionStarted {
                    workspace: self.inner.workspace.clone(),
                    root_model_profile: self.inner.default_model_profile.clone(),
                    root_agent_profile: self.inner.default_agent_profile.clone(),
                },
            )
            .await?;
        store
            .append(
                root.clone(),
                SessionEvent::TargetsSnapshot {
                    targets: self.inner.target_definitions.clone(),
                },
            )
            .await?;
        let runtime = SessionRuntime::build(self.inner.clone(), store, Vec::new()).await?;
        runtime.start_root(Vec::new()).await
    }

    pub async fn resume_session(&self, id: SessionId) -> Result<SessionHandle, HarnessError> {
        let (store, records) = SessionStore::open(&self.inner.session_root, id).await?;
        let root = AgentId::root(id);
        let history = records
            .iter()
            .filter(|record| record.agent == root)
            .filter_map(|record| match &record.event {
                SessionEvent::MessageCommitted { message } => Some(message.clone()),
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
    executor: ToolExecutor,
    // Keeps the script tool's weak executor lookup alive without an executor/registry cycle.
    _executor_slot: Arc<OnceLock<ToolExecutor>>,
    tool_definitions: Vec<ToolDefinition>,
    remote: RemoteManager,
    targets: TargetRegistry,
    agents: RwLock<HashMap<AgentId, mpsc::Sender<AgentCommand>>>,
    interrupts: RwLock<HashMap<AgentId, Arc<AtomicBool>>>,
    child_counters: RwLock<HashMap<AgentId, u32>>,
    pending_questions: Mutex<HashMap<JobId, PendingQuestion>>,
    events: broadcast::Sender<RuntimeEvent>,
}

#[derive(Clone)]
struct PendingQuestion {
    ask_job: JobId,
}

enum AgentCommand {
    Input {
        content: Vec<UserContent>,
        done: Option<oneshot::Sender<Result<String, String>>>,
    },
    JobsReady,
    Shutdown,
}

impl SessionRuntime {
    async fn build(
        harness: Arc<HarnessInner>,
        store: SessionStore,
        prior_records: Vec<EventRecord>,
    ) -> Result<Arc<Self>, HarnessError> {
        let jobs = JobManager::restore(store.clone(), &prior_records).await?;
        let mut definitions = prior_records
            .iter()
            .find_map(|record| match &record.event {
                SessionEvent::TargetsSnapshot { targets } => Some(targets.clone()),
                _ => None,
            })
            .unwrap_or_else(|| harness.target_definitions.clone());
        let targets = TargetRegistry::from_definitions(std::mem::take(&mut definitions))?;
        for record in &prior_records {
            if let SessionEvent::TargetUpserted { target } = &record.event {
                targets.upsert(target.clone()).await?;
            }
        }
        let remote = RemoteManager::new(targets.clone())
            .with_prompt_handler(harness.sensitive_prompts.clone());
        let executor_slot = Arc::new(OnceLock::new());
        let runtime_slot = Arc::new(OnceLock::<Weak<Self>>::new());
        let mut builder = ToolRegistryBuilder::default();
        register_coding_tools(
            &mut builder,
            store.clone(),
            jobs.clone(),
            harness.skills.clone(),
            targets.clone(),
            remote.clone(),
        )?;
        install_script_tool_weak(&mut builder, Arc::downgrade(&executor_slot))?;
        tools::register(&mut builder, runtime_slot.clone())?;
        builder.extend(&harness.extra_tools)?;
        let executor = ToolExecutor::new(
            builder.build(),
            harness.policy.clone(),
            jobs.clone(),
            harness.workspace.clone(),
        );
        executor_slot
            .set(executor.clone())
            .map_err(|_| HarnessError::Initialization("executor already set".to_owned()))?;
        let tool_definitions = executor.registry().definitions();
        let (events, _) = broadcast::channel(1024);
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
        let runtime = Arc::new(Self {
            harness,
            store: store.clone(),
            jobs: jobs.clone(),
            executor,
            _executor_slot: executor_slot,
            tool_definitions,
            remote,
            targets,
            agents: RwLock::new(HashMap::new()),
            interrupts: RwLock::new(HashMap::new()),
            child_counters: RwLock::new(child_counters),
            pending_questions: Mutex::new(HashMap::new()),
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
            .spawn_agent(
                root.clone(),
                None,
                None,
                self.harness.default_model_profile.clone(),
                self.harness.default_agent_profile.clone(),
                history,
                false,
            )
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
                if let Some(sender) = runtime.agents.read().await.get(&completion.agent).cloned() {
                    let _ = sender.send(AgentCommand::JobsReady).await;
                }
            }
        });
    }

    async fn interrupt_tree(&self, root: &AgentId) -> usize {
        let targets = self
            .interrupts
            .read()
            .await
            .iter()
            .filter(|(agent, _)| {
                agent.session() == root.session() && agent.path().starts_with(root.path())
            })
            .map(|(agent, interrupted)| (agent.clone(), interrupted.clone()))
            .collect::<Vec<_>>();
        let mut cancelled = 0;
        for (agent, interrupted) in targets {
            interrupted.store(true, Ordering::Relaxed);
            cancelled += self.jobs.cancel_all(&agent).await;
        }
        cancelled
    }

    async fn spawn_agent(
        self: &Arc<Self>,
        id: AgentId,
        parent: Option<AgentId>,
        owner_job: Option<JobId>,
        model_profile: String,
        agent_profile: Option<String>,
        history: Vec<Message>,
        one_shot: bool,
    ) -> Result<mpsc::Sender<AgentCommand>, HarnessError> {
        if id.depth() > self.harness.max_child_depth {
            return Err(HarnessError::ChildDepth);
        }
        let (profile, system) = self.resolve_agent(
            &model_profile,
            agent_profile.as_deref(),
            &id,
            crate::target::ROOT_TARGET,
            "local",
            &self.harness.workspace,
        )?;
        self.store
            .append(
                id.clone(),
                SessionEvent::AgentStarted {
                    parent,
                    model_profile: model_profile.clone(),
                    agent_profile: agent_profile.clone(),
                    target: None,
                    workspace: None,
                },
            )
            .await?;
        let (tx, rx) = mpsc::channel(AGENT_CHANNEL_CAPACITY);
        self.agents.write().await.insert(id.clone(), tx.clone());
        let interrupted = Arc::new(AtomicBool::new(false));
        self.interrupts
            .write()
            .await
            .insert(id.clone(), interrupted.clone());
        let runtime = self.clone();
        tokio::spawn(async move {
            runtime
                .run_agent(
                    id,
                    owner_job,
                    interrupted,
                    profile,
                    system,
                    history,
                    one_shot,
                    rx,
                )
                .await;
        });
        Ok(tx)
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_remote_child(
        &self,
        context: &crate::tool::ToolContext,
        child: AgentId,
        target: String,
        workspace: Option<PathBuf>,
        model_profile: String,
        agent_profile: Option<String>,
        prompt: String,
        todo: Vec<super::TodoItem>,
    ) -> Result<serde_json::Value, HarnessError> {
        if child.depth() > self.harness.max_child_depth {
            return Err(HarnessError::ChildDepth);
        }
        let target_definition = self.targets.get(&target).await?;
        let effective_workspace = workspace
            .clone()
            .unwrap_or_else(|| target_definition.workspace.clone());
        let (profile, system) = self.resolve_agent(
            &model_profile,
            agent_profile.as_deref(),
            &child,
            &target,
            "ssh",
            &effective_workspace,
        )?;
        self.store
            .append(
                child.clone(),
                SessionEvent::AgentStarted {
                    parent: Some(context.agent.clone()),
                    model_profile,
                    agent_profile,
                    target: Some(target.clone()),
                    workspace: workspace.clone(),
                },
            )
            .await?;
        if !todo.is_empty() {
            self.store
                .append(child.clone(), SessionEvent::TodoReplaced { items: todo })
                .await?;
        }
        let spec = RemoteAgentSpec {
            model: profile.model.clone(),
            provider: profile.provider.clone(),
            system,
            tools: self.tool_definitions.clone(),
            reasoning: profile.reasoning.clone(),
            max_output_tokens: profile.max_output_tokens,
            history: Vec::new(),
        };
        let id = child.to_string();
        let result = async {
            let mut step = self
                .remote
                .agent_start(&target, workspace.as_deref(), id.clone(), spec)
                .await
                .map_err(|error| HarnessError::Agent(error.to_string()))?;
            loop {
                step = match step {
                    RemoteAgentStep::Started { mut request, clock } => {
                        let message = Message::User(vec![
                            UserContent::Text {
                                text: prompt.clone(),
                            },
                            prompt::state_content(&self.jobs, &child, Some(clock)).await,
                        ]);
                        self.commit(&child, message.clone()).await?;
                        request.messages.push(message.clone());
                        let chunks = self
                            .invoke_remote_provider(context, &child, &profile, request)
                            .await?;
                        self.remote
                            .agent_provider(
                                &target,
                                workspace.as_deref(),
                                id.clone(),
                                Some(message),
                                chunks,
                            )
                            .await
                            .map_err(|error| HarnessError::Agent(error.to_string()))?
                    }
                    RemoteAgentStep::Provider { request } => {
                        let chunks = self
                            .invoke_remote_provider(context, &child, &profile, request)
                            .await?;
                        self.remote
                            .agent_provider(&target, workspace.as_deref(), id.clone(), None, chunks)
                            .await
                            .map_err(|error| HarnessError::Agent(error.to_string()))?
                    }
                    RemoteAgentStep::Tools {
                        blocks,
                        usage,
                        calls,
                    } => {
                        let assistant = Message::Assistant(blocks);
                        self.commit(&child, assistant).await?;
                        self.store
                            .append(child.clone(), SessionEvent::Usage { usage })
                            .await?;
                        let results = join_all(calls.iter().map(|call| {
                            self.execute_remote_call(
                                &child,
                                Some(context.job),
                                call,
                                &target,
                                workspace.as_deref(),
                            )
                        }))
                        .await;
                        self.commit(&child, Message::Tool(results.clone())).await?;
                        self.remote
                            .agent_tools(&target, workspace.as_deref(), id.clone(), results)
                            .await
                            .map_err(|error| HarnessError::Agent(error.to_string()))?
                    }
                    RemoteAgentStep::Complete {
                        blocks,
                        usage,
                        text,
                    } => {
                        self.commit(&child, Message::Assistant(blocks)).await?;
                        self.store
                            .append(child.clone(), SessionEvent::Usage { usage })
                            .await?;
                        let _ = self.events.send(RuntimeEvent::TurnCompleted {
                            agent: child.clone(),
                            text: text.clone(),
                        });
                        return Ok(json!({"agent": child, "target": target, "result": text}));
                    }
                };
            }
        }
        .await;
        let terminal = if result.is_ok() {
            SessionEvent::AgentCompleted
        } else {
            SessionEvent::AgentInterrupted
        };
        let _ = self.store.append(child, terminal).await;
        result
    }

    async fn invoke_remote_provider(
        &self,
        context: &crate::tool::ToolContext,
        child: &AgentId,
        profile: &ModelProfile,
        mut request: ModelRequest,
    ) -> Result<Vec<ResponseChunk>, HarnessError> {
        self.hydrate_images(&mut request.messages).await?;
        let provider = self
            .harness
            .providers
            .get(&profile.provider)
            .cloned()
            .ok_or_else(|| HarnessError::UnknownProvider(profile.provider.clone()))?;
        let mut response = provider.invoke(request).await?;
        let mut chunks = Vec::new();
        let cancelled = context.cancellation();
        loop {
            let chunk = tokio::select! {
                chunk = poll_fn(|cx| response.as_mut().poll_chunk(cx)) => chunk,
                () = wait_for_interrupt(&cancelled) => return Err(HarnessError::Interrupted),
            };
            let Some(chunk) = chunk else { break };
            let chunk = chunk?;
            match &chunk {
                ResponseChunk::TextDelta { text } => {
                    let _ = self.events.send(RuntimeEvent::TextDelta {
                        agent: child.clone(),
                        text: text.clone(),
                    });
                }
                ResponseChunk::ReasoningDelta { text } => {
                    let _ = self.events.send(RuntimeEvent::ReasoningDelta {
                        agent: child.clone(),
                        text: text.clone(),
                    });
                }
                _ => {}
            }
            chunks.push(chunk);
        }
        Ok(chunks)
    }

    async fn execute_remote_call(
        &self,
        agent: &AgentId,
        parent: Option<JobId>,
        call: &ToolCall,
        target: &str,
        workspace: Option<&Path>,
    ) -> ToolResult {
        let mut call = call.clone();
        let workspace_tool = matches!(
            call.name.as_str(),
            "read"
                | "search"
                | "glob"
                | "exec"
                | "shell"
                | "write"
                | "replace"
                | "patch"
                | "remove"
                | "script"
        );
        let selected_target = call
            .arguments
            .get("target")
            .and_then(serde_json::Value::as_str);
        if workspace_tool && selected_target.is_none_or(|selected| selected == target) {
            if let Some(arguments) = call.arguments.as_object_mut() {
                arguments.remove("target");
            }
            let remote = self.remote.clone();
            let store = self.store.clone();
            let target = target.to_owned();
            let workspace = workspace.map(Path::to_path_buf);
            let name = call.name.clone();
            let result = self
                .executor
                .execute_external(
                    agent.clone(),
                    &call.name,
                    call.arguments.clone(),
                    parent,
                    vec![
                        crate::tool::policy::ToolEffect::RemoteAccess,
                        crate::tool::policy::ToolEffect::Network,
                    ],
                    move |arguments| async move {
                        let mut output = remote
                            .execute_tool(&target, workspace.as_deref(), name, arguments)
                            .await
                            .map_err(|error| crate::tool::ToolError::Failed(error.to_string()))?;
                        let mut imported = Vec::new();
                        for image in output.images {
                            let encoded = image.data_base64.as_deref().ok_or_else(|| {
                                crate::tool::ToolError::Failed(
                                    "remote image payload is missing".to_owned(),
                                )
                            })?;
                            let bytes = base64::engine::general_purpose::STANDARD
                                .decode(encoded)
                                .map_err(|error| {
                                crate::tool::ToolError::Failed(error.to_string())
                            })?;
                            let reference = store
                                .import_blob(&bytes, image.name, image.media_type)
                                .await
                                .map_err(|error| {
                                    crate::tool::ToolError::Failed(error.to_string())
                                })?;
                            if reference.sha256 != image.sha256 {
                                return Err(crate::tool::ToolError::Failed(
                                    "remote image hash did not match its payload".to_owned(),
                                ));
                            }
                            imported.push(reference);
                        }
                        output.images = imported;
                        Ok(output)
                    },
                )
                .await;
            return match result {
                Ok(result) => ToolResult {
                    call_id: call.id,
                    name: call.name,
                    result: result.output.value,
                    images: result.output.images,
                    is_error: false,
                },
                Err(error) => ToolResult {
                    call_id: call.id,
                    name: call.name,
                    result: json!({"error": error.to_string()}),
                    images: Vec::new(),
                    is_error: true,
                },
            };
        }
        if matches!(call.name.as_str(), "exec" | "shell" | "agent")
            && let Some(arguments) = call.arguments.as_object_mut()
        {
            arguments
                .entry("target")
                .or_insert_with(|| serde_json::Value::String(target.to_owned()));
            if call.name == "agent"
                && let Some(workspace) = workspace
            {
                arguments.entry("workspace").or_insert_with(|| {
                    serde_json::Value::String(workspace.to_string_lossy().into_owned())
                });
            }
        }
        self.execute_call(agent, parent, &call).await
    }

    fn resolve_agent(
        &self,
        model_profile: &str,
        agent_profile: Option<&str>,
        agent: &AgentId,
        target: &str,
        target_kind: &str,
        workspace: &Path,
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
            target,
            target_kind,
            workspace,
            self.harness.max_child_depth,
        )];
        Ok((profile, system))
    }

    async fn run_agent(
        self: Arc<Self>,
        id: AgentId,
        owner_job: Option<JobId>,
        interrupted: Arc<AtomicBool>,
        profile: ModelProfile,
        system: Vec<SystemSegment>,
        mut history: Vec<Message>,
        one_shot: bool,
        mut rx: mpsc::Receiver<AgentCommand>,
    ) {
        while let Some(command) = rx.recv().await {
            match command {
                AgentCommand::Shutdown => {
                    let _ = self
                        .store
                        .append(id.clone(), SessionEvent::AgentInterrupted)
                        .await;
                    break;
                }
                AgentCommand::Input { mut content, done } => {
                    interrupted.store(false, Ordering::Relaxed);
                    content.push(prompt::state_content(&self.jobs, &id, None).await);
                    let message = Message::User(content);
                    if let Err(error) = self.commit(&id, message.clone()).await {
                        if let Some(done) = done {
                            let _ = done.send(Err(error.to_string()));
                        }
                        continue;
                    }
                    history.push(message);
                    let result = self
                        .run_turn(
                            &id,
                            &profile,
                            &system,
                            owner_job,
                            &interrupted,
                            &mut history,
                        )
                        .await;
                    if let Some(done) = done {
                        let _ = done.send(
                            result
                                .as_ref()
                                .map(Clone::clone)
                                .map_err(ToString::to_string),
                        );
                    }
                    if one_shot && !self.jobs.has_running(&id).await {
                        let _ = self
                            .store
                            .append(id.clone(), SessionEvent::AgentCompleted)
                            .await;
                        break;
                    }
                }
                AgentCommand::JobsReady => {
                    let pending = match self.jobs.take_pending(&id).await {
                        Ok(pending) if !pending.is_empty() => pending,
                        _ => continue,
                    };
                    interrupted.store(false, Ordering::Relaxed);
                    let message = Message::User(vec![
                        UserContent::Runtime {
                            text: format!(
                                "<skyhook_job_events>\n{}\n</skyhook_job_events>",
                                serde_json::to_string(&pending).unwrap_or_else(|_| "[]".to_owned())
                            ),
                        },
                        prompt::state_content(&self.jobs, &id, None).await,
                    ]);
                    if self.commit(&id, message.clone()).await.is_err() {
                        continue;
                    }
                    history.push(message);
                    let _ = self
                        .run_turn(
                            &id,
                            &profile,
                            &system,
                            owner_job,
                            &interrupted,
                            &mut history,
                        )
                        .await;
                    if one_shot && !self.jobs.has_running(&id).await {
                        let _ = self
                            .store
                            .append(id.clone(), SessionEvent::AgentCompleted)
                            .await;
                        break;
                    }
                }
            }
        }
        self.agents.write().await.remove(&id);
        self.interrupts.write().await.remove(&id);
    }

    async fn run_turn(
        &self,
        agent: &AgentId,
        profile: &ModelProfile,
        system: &[SystemSegment],
        owner_job: Option<JobId>,
        interrupted: &Arc<AtomicBool>,
        history: &mut Vec<Message>,
    ) -> Result<String, HarnessError> {
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
            if interrupted.load(Ordering::Relaxed) {
                return Err(HarnessError::Interrupted);
            }
            let mut request_messages = history.clone();
            self.hydrate_images(&mut request_messages).await?;
            let request = ModelRequest {
                model: profile.model.clone(),
                system: system.to_vec(),
                messages: request_messages,
                tools: self.tool_definitions.clone(),
                reasoning: profile.reasoning.clone(),
                max_output_tokens: profile.max_output_tokens,
                correlation: Some(agent.to_string()),
            };
            let mut response = provider.invoke(request).await?;
            let mut blocks = Vec::new();
            let mut streamed_text = String::new();
            let mut usage = Usage::default();
            loop {
                let chunk = tokio::select! {
                    chunk = poll_fn(|context| response.as_mut().poll_chunk(context)) => chunk,
                    () = wait_for_interrupt(interrupted) => return Err(HarnessError::Interrupted),
                };
                let Some(chunk) = chunk else {
                    break;
                };
                match chunk? {
                    ResponseChunk::TextDelta { text } => {
                        streamed_text.push_str(&text);
                        let _ = self.events.send(RuntimeEvent::TextDelta {
                            agent: agent.clone(),
                            text,
                        });
                    }
                    ResponseChunk::ReasoningDelta { text } => {
                        let _ = self.events.send(RuntimeEvent::ReasoningDelta {
                            agent: agent.clone(),
                            text,
                        });
                    }
                    ResponseChunk::Block { block } => blocks.push(block),
                    ResponseChunk::Usage { usage: value } => usage = value,
                    ResponseChunk::MessageStart { .. }
                    | ResponseChunk::ToolInputDelta { .. }
                    | ResponseChunk::Diagnostic { .. }
                    | ResponseChunk::Done { .. } => {}
                }
            }
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
            for block in &blocks {
                if let AssistantContent::Text { text } = block {
                    final_text.push_str(text);
                }
            }
            let assistant = Message::Assistant(blocks.clone());
            self.commit(agent, assistant.clone()).await?;
            history.push(assistant);
            self.store
                .append(agent.clone(), SessionEvent::Usage { usage })
                .await?;
            let calls = blocks
                .into_iter()
                .filter_map(|block| match block {
                    AssistantContent::ToolCall(call) => Some(call),
                    _ => None,
                })
                .collect::<Vec<_>>();
            if calls.is_empty() {
                let _ = self.events.send(RuntimeEvent::TurnCompleted {
                    agent: agent.clone(),
                    text: final_text.clone(),
                });
                return Ok(final_text);
            }
            let results = join_all(
                calls
                    .iter()
                    .map(|call| self.execute_call(agent, owner_job, call)),
            )
            .await;
            let tools = Message::Tool(results);
            self.commit(agent, tools.clone()).await?;
            history.push(tools);
        }
    }

    async fn execute_call(
        &self,
        agent: &AgentId,
        parent: Option<JobId>,
        call: &ToolCall,
    ) -> ToolResult {
        match self
            .executor
            .execute(agent.clone(), &call.name, call.arguments.clone(), parent)
            .await
        {
            Ok(result) => ToolResult {
                call_id: call.id.clone(),
                name: call.name.clone(),
                result: result.output.value,
                images: result.output.images,
                is_error: false,
            },
            Err(error) => ToolResult {
                call_id: call.id.clone(),
                name: call.name.clone(),
                result: json!({"error": error.to_string()}),
                images: Vec::new(),
                is_error: true,
            },
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

    async fn owning_agent_job(&self, mut job: JobId) -> Result<JobId, HarnessError> {
        loop {
            let envelope = self.jobs.snapshot(job).await?;
            if envelope.tool == "agent" {
                return Ok(job);
            }
            job = envelope.parent_job.ok_or_else(|| {
                HarnessError::Agent("child question has no owning agent job".to_owned())
            })?;
        }
    }

    async fn open_child_question(
        &self,
        ask_job: JobId,
        output: serde_json::Value,
    ) -> Result<JobId, HarnessError> {
        let owner_job = self.owning_agent_job(ask_job).await?;
        {
            let mut pending = self.pending_questions.lock().await;
            if pending.contains_key(&owner_job) {
                return Err(HarnessError::Agent(
                    "an agent job may only have one outstanding question batch".to_owned(),
                ));
            }
            pending.insert(owner_job, PendingQuestion { ask_job });
        }
        if let Err(error) = self.jobs.request_input(owner_job, output).await {
            self.pending_questions.lock().await.remove(&owner_job);
            return Err(error.into());
        }
        Ok(owner_job)
    }

    async fn resolve_child_question(&self, owner_job: JobId) -> Result<(), HarnessError> {
        self.pending_questions.lock().await.remove(&owner_job);
        self.jobs.resume_input(owner_job).await?;
        Ok(())
    }

    async fn answer_child_question(
        &self,
        owner_job: JobId,
        value: serde_json::Value,
    ) -> Result<bool, HarnessError> {
        let pending = self.pending_questions.lock().await.get(&owner_job).cloned();
        let Some(pending) = pending else {
            return Ok(false);
        };
        self.jobs.send(pending.ask_job, value).await?;
        Ok(true)
    }

    async fn cancel_child_question(&self, owner_job: JobId) {
        let pending = self.pending_questions.lock().await.remove(&owner_job);
        if let Some(pending) = pending {
            let _ = self.jobs.cancel(pending.ask_job).await;
        }
    }
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

async fn wait_for_interrupt(interrupted: &AtomicBool) {
    while !interrupted.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        pin::Pin,
        sync::{
            Mutex as StdMutex,
            atomic::{AtomicUsize, Ordering},
        },
        task::Poll,
    };

    use futures_util::stream;

    use super::*;
    use crate::{
        provider::protocol::{StopReason, ToolCall},
        provider::{ProviderFuture, ResponseHandle},
    };

    struct ToolCallingProvider {
        calls: AtomicUsize,
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
    }

    struct HangingProvider;

    struct CapturingProvider {
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
    }

    struct BlockingFirstProvider {
        calls: AtomicUsize,
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
        release: Arc<tokio::sync::Semaphore>,
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
            Box::pin(async { Ok(Box::pin(stream::pending()) as Pin<Box<dyn ResponseHandle>>) })
        }
    }

    impl Provider for CapturingProvider {
        fn invoke(&self, request: ModelRequest) -> ProviderFuture {
            self.requests.lock().unwrap().push(request);
            Box::pin(async {
                Ok(Box::pin(stream::iter(vec![
                    Ok(ResponseChunk::TextDelta {
                        text: "done".to_owned(),
                    }),
                    Ok(ResponseChunk::Done {
                        stop_reason: Some(StopReason::Complete),
                    }),
                ])) as Pin<Box<dyn ResponseHandle>>)
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

    impl Provider for ToolCallingProvider {
        fn invoke(&self, request: ModelRequest) -> ProviderFuture {
            self.requests.lock().unwrap().push(request);
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                let chunks = if call == 0 {
                    vec![
                        Ok(ResponseChunk::Block {
                            block: AssistantContent::ToolCall(ToolCall {
                                id: "read-1".to_owned(),
                                name: "read".to_owned(),
                                arguments: json!({"path": "note.txt"}),
                            }),
                        }),
                        Ok(ResponseChunk::Block {
                            block: AssistantContent::ToolCall(ToolCall {
                                id: "read-2".to_owned(),
                                name: "read".to_owned(),
                                arguments: json!({"path": "note.txt"}),
                            }),
                        }),
                        Ok(ResponseChunk::Done {
                            stop_reason: Some(StopReason::ToolUse),
                        }),
                    ]
                } else {
                    vec![
                        Ok(ResponseChunk::TextDelta {
                            text: "finished".to_owned(),
                        }),
                        Ok(ResponseChunk::Done {
                            stop_reason: Some(StopReason::Complete),
                        }),
                    ]
                };
                Ok(Box::pin(stream::iter(chunks)) as Pin<Box<dyn ResponseHandle>>)
            })
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

    #[tokio::test]
    async fn provider_tool_loop_runs_through_the_shared_registry() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("note.txt"), "hello").unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let harness = HarnessBuilder::new(workspace.path())
            .session_root(sessions.path())
            .provider(
                "test",
                Arc::new(ToolCallingProvider {
                    calls: AtomicUsize::new(0),
                    requests: requests.clone(),
                }),
            )
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
            .build()
            .await
            .unwrap();
        let session = harness.new_session().await.unwrap();
        assert_eq!(session.prompt("read the note").await.unwrap(), "finished");
        assert!(session.tools().get("script").is_some());
        assert!(session.tools().get("jobs").is_some());
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].system, requests[1].system);
        assert_eq!(requests[0].tools, requests[1].tools);
        assert!(requests[1].messages.starts_with(&requests[0].messages));
        assert_eq!(requests[1].messages.len(), requests[0].messages.len() + 2);
        let Message::Tool(results) = requests[1].messages.last().unwrap() else {
            panic!("parallel calls must be committed as one tool-result message");
        };
        assert_eq!(results.len(), 2);
        assert_eq!(runtime_state_count(&requests[1].messages), 1);
    }

    #[tokio::test]
    async fn interrupt_stops_a_pending_provider_turn() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let harness = HarnessBuilder::new(workspace.path())
            .session_root(sessions.path())
            .provider("test", Arc::new(HangingProvider))
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
            .build()
            .await
            .unwrap();
        let session = harness.new_session().await.unwrap();
        let pending = tokio::spawn({
            let session = session.clone();
            async move { session.prompt("wait forever").await }
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        session.interrupt().await;
        let error = tokio::time::timeout(Duration::from_secs(1), pending)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("interrupted"));
    }

    #[tokio::test]
    async fn host_scripts_use_the_registered_script_tool() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let harness = HarnessBuilder::new(workspace.path())
            .session_root(sessions.path())
            .provider("test", Arc::new(HangingProvider))
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
            .build()
            .await
            .unwrap();
        let session = harness.new_session().await.unwrap();
        let output = session.run_script("return {value: 40 + 2};").await.unwrap();
        assert_eq!(output.value, json!({"value": 42}));
    }

    #[tokio::test]
    async fn target_tools_use_generated_object_and_builder_apis() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let harness = HarnessBuilder::new(workspace.path())
            .session_root(sessions.path())
            .provider("test", Arc::new(HangingProvider))
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
            .build()
            .await
            .unwrap();
        let session = harness.new_session().await.unwrap();
        let added = session
            .run_script(
                r#"return await tool.target_add()
                    .name("build")
                    .host("build.example.com")
                    .via(null);"#,
            )
            .await
            .unwrap();
        assert_eq!(added.value["name"], "build");
        let listed = session
            .run_script("return await tool.targets({});")
            .await
            .unwrap();
        assert_eq!(listed.value["targets"][0]["name"], "root");
        assert!(
            listed.value["targets"]
                .as_array()
                .unwrap()
                .iter()
                .any(|target| target["name"] == "build")
        );
    }

    #[tokio::test]
    async fn requests_have_one_cached_system_and_durable_runtime_state() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let harness = HarnessBuilder::new(workspace.path())
            .session_root(sessions.path())
            .provider(
                "test",
                Arc::new(CapturingProvider {
                    requests: requests.clone(),
                }),
            )
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
            .build()
            .await
            .unwrap();
        let session = harness.new_session().await.unwrap();
        assert_eq!(session.prompt("hello").await.unwrap(), "done");
        let requests = requests.lock().unwrap();
        let request = &requests[0];
        assert_eq!(request.system.len(), 1);
        assert!(request.system[0].cache);
        assert!(request.system[0].text.starts_with(prompt::BASE_PROMPT));
        assert!(request.system[0].text.contains("<skyhook_context>"));
        let Message::User(content) = request.messages.last().unwrap() else {
            panic!("external input and runtime state must share a user message");
        };
        assert!(matches!(&content[0], UserContent::Text { text } if text == "hello"));
        let UserContent::Runtime { text } = &content[1] else {
            panic!("runtime state must follow external input as runtime content");
        };
        assert!(text.contains("<skyhook_state>"));
        assert!(text.contains("\"active_jobs\":[]"));
        assert_eq!(runtime_state_count(&request.messages), 1);
    }

    #[tokio::test]
    async fn resumed_sessions_append_state_without_rewriting_history() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let harness = HarnessBuilder::new(workspace.path())
            .session_root(sessions.path())
            .provider(
                "test",
                Arc::new(CapturingProvider {
                    requests: requests.clone(),
                }),
            )
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
            .build()
            .await
            .unwrap();
        let session = harness.new_session().await.unwrap();
        let session_id = session.id();
        assert_eq!(session.prompt("first").await.unwrap(), "done");
        session.shutdown().await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while session
                .runtime
                .agents
                .read()
                .await
                .contains_key(&session.root)
                || session
                    .runtime
                    .interrupts
                    .read()
                    .await
                    .contains_key(&session.root)
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drop(session);
        tokio::time::sleep(Duration::from_millis(10)).await;

        let resumed = harness.resume_session(session_id).await.unwrap();
        assert_eq!(resumed.prompt("second").await.unwrap(), "done");
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].messages.starts_with(&requests[0].messages));
        assert_eq!(runtime_state_count(&requests[0].messages), 1);
        assert_eq!(runtime_state_count(&requests[1].messages), 2);
    }

    #[tokio::test]
    async fn background_completions_batch_events_with_one_state_snapshot() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let harness = HarnessBuilder::new(workspace.path())
            .session_root(sessions.path())
            .provider(
                "test",
                Arc::new(BlockingFirstProvider {
                    calls: AtomicUsize::new(0),
                    requests: requests.clone(),
                    release: release.clone(),
                }),
            )
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
            .build()
            .await
            .unwrap();
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
                .create(
                    session.root.clone(),
                    None,
                    "test".to_owned(),
                    json!({}),
                    false,
                    true,
                )
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
        tokio::time::sleep(Duration::from_millis(10)).await;

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2, "redundant wakeups must commit nothing");
        assert!(requests[1].messages.starts_with(&requests[0].messages));
        let Message::User(content) = requests[1].messages.last().unwrap() else {
            panic!("job wakeup must append one runtime user message");
        };
        assert_eq!(content.len(), 2);
        let UserContent::Runtime { text: events } = &content[0] else {
            panic!("job events must be runtime content");
        };
        assert_eq!(events.matches("\"job_id\"").count(), 2);
        assert!(matches!(
            &content[1],
            UserContent::Runtime { text } if text.contains("<skyhook_state>")
        ));
        assert_eq!(runtime_state_count(&requests[1].messages), 2);
    }

    #[tokio::test]
    async fn child_questions_route_through_the_stable_agent_job() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let harness = HarnessBuilder::new(workspace.path())
            .session_root(sessions.path())
            .provider("test", Arc::new(HangingProvider))
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
            .build()
            .await
            .unwrap();
        let session = harness.new_session().await.unwrap();
        let root = session.root.clone();
        let child = root.child(1);
        let agent = session
            .runtime
            .jobs
            .create(root, None, "agent".to_owned(), json!({}), true, false)
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
            .create(
                child.clone(),
                Some(agent.id),
                "ask".to_owned(),
                json!({}),
                true,
                false,
            )
            .await
            .unwrap();
        session
            .runtime
            .jobs
            .transition(ask.id, crate::job::JobState::Running)
            .await
            .unwrap();
        let owner = session
            .runtime
            .open_child_question(
                ask.id,
                json!({"kind":"questions","question_id":"q-test","questions":[]}),
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
                .answer_child_question(owner, json!({"answer": "yes"}))
                .await
                .unwrap()
        );
        assert_eq!(ask.input.recv().await.unwrap(), json!({"answer": "yes"}));
        session.runtime.resolve_child_question(owner).await.unwrap();
        assert_eq!(
            session.runtime.jobs.snapshot(owner).await.unwrap().state,
            crate::job::JobState::Running
        );
    }

    #[tokio::test]
    async fn script_receive_requires_background_and_accepts_job_input() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let harness = HarnessBuilder::new(workspace.path())
            .session_root(sessions.path())
            .provider("test", Arc::new(HangingProvider))
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
            .build()
            .await
            .unwrap();
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
}
