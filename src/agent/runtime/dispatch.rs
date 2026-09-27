//! Child-agent launch, tool dispatch, and model context selection.

use super::*;

// One owner binds the resolved provider context, admitted capabilities, loop
// controls and initial journal publication to the same launch identity. There
// is no config lookup or reconstruction after preparation.
struct PreparedAgentLaunch {
    runtime: Arc<SessionRuntime>,
    agent_loop: AgentLoop,
    sender: AgentSender,
    available_depth: usize,
    start: Start,
}

/// Whether a prepared agent is journaled as it starts, with its initial todos.
enum Start {
    New(Option<Vec<TodoItem>>),
    /// Journaled before: resume under that contract without starting it again.
    Resume(Resumed),
}

impl PreparedAgentLaunch {
    async fn prepare(
        runtime: Arc<SessionRuntime>,
        id: AgentId,
        owner_job: Option<JobId>,
        launch: AgentLaunch,
    ) -> Result<Self, HarnessError> {
        if id.depth() > runtime.harness.max_child_depth {
            return Err(HarnessError::ChildDepth);
        }
        let remaining_depth = runtime.harness.max_child_depth.saturating_sub(id.depth());
        let (contract, meter, start, turn) = match launch {
            AgentLaunch::New {
                model,
                todos,
                available_depth,
                location,
                mode,
                capabilities,
            } => {
                if available_depth > remaining_depth {
                    return Err(HarnessError::ChildDepth);
                }
                let contract = RecordedContract {
                    profile: crate::session::ProfileSnapshot {
                        profile: runtime.model_entry(&model)?.profile,
                        name: model,
                    },
                    available_depth,
                    mode,
                    capabilities: capabilities.for_agent(available_depth),
                    location,
                    system: None,
                    tools: None,
                };
                let meter = super::compact::TokenMeter::default();
                (contract, meter, Start::New(todos), TurnState::Idle)
            }
            AgentLaunch::Resume(resumed) => {
                let (contract, meter) = (runtime.store)
                    .visit_records_after(RecordSeq::default(), |records| {
                        let meter = super::compact::TokenMeter::restore(records, &id);
                        (runtime.resumed_contract(records, &id), meter)
                    })
                    .await;
                let contract = contract.ok_or_else(|| HarnessError::NoRecordedModel(id.clone()))?;
                let turn = match resumed {
                    Resumed::Idle => TurnState::Idle,
                    Resumed::Parked(_) => TurnState::Parked,
                };
                (contract, meter, Start::Resume(resumed), turn)
            }
        };
        let RecordedContract {
            profile,
            available_depth,
            mode,
            capabilities,
            location,
            system,
            tools,
        } = contract;
        // A journaled profile is applied as recorded; the live catalog supplies only
        // the provider that serves it.
        let system = match system {
            Some(system) => system,
            None => {
                runtime
                    .system_prompt(
                        &id,
                        &location,
                        available_depth,
                        mode.as_ref(),
                        &capabilities,
                    )
                    .await?
            }
        };
        let mut context = runtime
            .open_agent_context(&id, profile, system, &capabilities, tools)
            .await?;
        context.meter = meter;
        let (tx, rx) = mpsc::channel(AGENT_CHANNEL_CAPACITY);
        let sender = AgentSender::new(tx);
        Ok(Self {
            runtime,
            agent_loop: AgentLoop {
                control: AgentControl::new(turn),
                id,
                owner_job,
                context,
                location,
                settings: AgentSettings { mode, capabilities },
                rx,
            },
            sender,
            available_depth,
            start,
        })
    }

    async fn install(self) -> Result<AgentSender, HarnessError> {
        let Self {
            runtime,
            agent_loop,
            sender,
            available_depth,
            start,
        } = self;
        let activity = match &start {
            Start::Resume(Resumed::Parked(failure)) => AgentActivity::Stopped(failure.clone()),
            Start::New(_) | Start::Resume(Resumed::Idle) => AgentActivity::Idle,
        };
        let todos = match start {
            Start::Resume(_) => None,
            Start::New(todos) => {
                let started = SessionEvent::AgentStarted {
                    owner_job: agent_loop.owner_job,
                    profile: Some(agent_loop.context.profile.clone()),
                    available_depth: u32::try_from(available_depth).unwrap_or(u32::MAX),
                    mode: (agent_loop.settings.mode.as_ref())
                        .map(|mode| runtime.mode_selection(mode)),
                    capabilities: agent_loop.settings.capabilities.iter().collect(),
                    location: agent_loop.location.clone(),
                };
                // Every entry references an agent, so a new session starts with its
                // root, the only agent that starts rather than resumes without an owner.
                let id = &agent_loop.id;
                let session = id.parent().is_none().then(|| SessionEvent::SessionStarted {
                    targets: runtime.harness.target_definitions.clone(),
                    capabilities: runtime.capabilities.iter().collect(),
                });
                let events = session.into_iter().chain([started]);
                runtime
                    .store
                    .append_all(events.map(|event| (id.clone(), event)).collect())
                    .await?;
                todos
            }
        };
        runtime
            .todos
            .register(agent_loop.id.clone(), agent_loop.owner_job, todos)
            .await?;
        {
            let mut agents = runtime.agents_mut();
            // Serialize this check with shutdown's agent snapshot. A child whose
            // provider initialization raced shutdown must not leave an idle loop.
            if runtime.shutting_down.load(Ordering::Acquire) {
                return Err(HarnessError::Interrupted);
            }
            agents.insert(
                agent_loop.id.clone(),
                LiveAgent {
                    model: agent_loop.context.profile.name.clone(),
                    capabilities: agent_loop.settings.capabilities.clone(),
                    sender: sender.clone(),
                    cancellation: CancellationToken::new(),
                    control: agent_loop.control.clone(),
                    available_depth,
                },
            );
        }
        runtime.activity(&agent_loop.id, activity);
        tokio::spawn(async move { runtime.run_agent(agent_loop).await });
        Ok(sender)
    }
}

impl SessionRuntime {
    pub(super) async fn spawn_agent(
        self: &Arc<Self>,
        id: AgentId,
        owner_job: Option<JobId>,
        launch: AgentLaunch,
    ) -> Result<AgentSender, HarnessError> {
        PreparedAgentLaunch::prepare(self.clone(), id, owner_job, launch)
            .await?
            .install()
            .await
    }

    /// Plan a model call and publish its job. A response's calls are created in
    /// order before any runs, so a `wait` among them sees its siblings as
    /// outstanding work from the start.
    pub(super) async fn create_call(
        &self,
        agent: &AgentId,
        parent: Option<JobId>,
        call: &ToolCall,
        origin: MessageSeq,
        location: &crate::execution::ExecutionLocation,
        capabilities: &CapabilitySet,
    ) -> CreatedCall {
        let call = call.clone();
        let executor = self
            .executor
            .clone()
            .with_location(location.clone())
            .with_capabilities(capabilities.clone())
            .with_model_origin(crate::session::ModelCallOrigin {
                message: origin,
                call_id: call.id().to_owned(),
            });
        let created = executor
            .create_model(
                agent.clone(),
                call.name(),
                serde_json::Value::Object(call.arguments().clone()),
                parent,
            )
            .await;
        match created {
            Ok(created) => CreatedCall::Created {
                executor,
                call,
                created: Box::new(created),
            },
            Err(error) => CreatedCall::Settled(failed_result(&call, &executor, error)),
        }
    }
}

/// A model call between its job's publication and its result.
pub(super) enum CreatedCall {
    Created {
        executor: ToolExecutor,
        call: ToolCall,
        created: Box<crate::tool::executor::CreatedInvocation>,
    },
    /// Settled without a job: planning failed or the tool is unavailable.
    Settled(ToolResult),
}

impl CreatedCall {
    /// Tool dispatch owns its execution inputs. Erasing this future separates
    /// the driver's Send proof from the nested supervised executor graph.
    pub(super) fn run(self) -> futures_util::future::BoxFuture<'static, ToolResult> {
        Box::pin(async move {
            let (executor, call, created) = match self {
                Self::Settled(result) => return result,
                Self::Created {
                    executor,
                    call,
                    created,
                } => (executor, call, created),
            };
            match executor.run(*created).await {
                Ok(result) => ToolResult {
                    call_id: call.id().to_owned(),
                    name: call.name().to_owned(),
                    result: result.output.value,
                    images: result.output.images,
                    is_error: result.is_error,
                },
                Err(error) => failed_result(&call, &executor, error),
            }
        })
    }
}

fn failed_result(
    call: &ToolCall,
    executor: &ToolExecutor,
    error: crate::tool::ToolError,
) -> ToolResult {
    let viewer = executor.diagnostic_viewer();
    let output = crate::tool::executor::failure_response(error, viewer);
    failed_tool_result(call.id().to_owned(), call.name().to_owned(), output)
}

/// A call that never ran as a job: its failure view as the call's result.
pub(super) fn unrun_tool_result(call_id: String, name: String, error: String) -> ToolResult {
    let failure = crate::job::JobView::failure(error, None, false);
    let output = crate::tool::ToolOutput::new(failure.into_value());
    failed_tool_result(call_id, name, output)
}

/// A failed call's result, reported by no job.
pub(super) fn failed_tool_result(
    call_id: String,
    name: String,
    output: crate::tool::ToolOutput,
) -> ToolResult {
    ToolResult {
        call_id,
        name,
        result: output.value,
        images: output.images,
        is_error: true,
    }
}

impl SessionRuntime {
    /// A model this runtime's catalog serves. Journaled names are checked here on
    /// resume, since the catalog may have changed since they were recorded.
    pub(super) fn model_entry(&self, model: &ModelRef) -> Result<ModelEntry, HarnessError> {
        self.harness
            .models
            .get(model)
            .cloned()
            .ok_or_else(|| HarnessError::UnknownModel(model.clone()))
    }

    /// The journal form of a mode about to be applied. The session keeps the
    /// definition of a mode's first use only.
    pub(super) fn mode_selection(&self, name: &ModeName) -> crate::session::ModeSelection {
        crate::session::ModeSelection {
            name: name.clone(),
            definition: self.modes.get(name).cloned(),
        }
    }

    pub(super) async fn system_prompt(
        &self,
        agent: &AgentId,
        location: &crate::execution::ExecutionLocation,
        available_depth: usize,
        mode: Option<&ModeName>,
        capabilities: &CapabilitySet,
    ) -> Result<Vec<SystemSegment>, HarnessError> {
        let target = match &location.target {
            crate::target::TargetRef::Root => None,
            crate::target::TargetRef::Named(name) => Some(self.router.targets().get(name).await?),
        };
        let system = vec![prompt::system_segment(&prompt::PromptInputs {
            instructions: &self.harness.instructions,
            agent,
            location,
            target: target.as_ref(),
            available_depth,
            mode: mode.and_then(|name| Some((name.as_str(), self.modes.get(name)?))),
            capabilities,
        })];
        Ok(system)
    }

    /// Open a context for `profile` on the live provider of its model, offering the
    /// live tool surface, or `pinned` tools journaled earlier. A pinned tool whose
    /// live definition differs or is gone stays visible to the model but is
    /// unavailable to call.
    pub(super) async fn open_agent_context(
        &self,
        agent: &AgentId,
        profile: crate::session::ProfileSnapshot,
        system: Vec<SystemSegment>,
        capabilities: &CapabilitySet,
        pinned: Option<Vec<crate::provider::protocol::ToolDefinition>>,
    ) -> Result<AgentContext, HarnessError> {
        let provider = self.model_entry(&profile.name)?.provider;
        let live = self
            .executor
            .clone()
            .with_capabilities(capabilities.clone())
            .surface_for_agent(agent)
            .definitions();
        let (tools, unavailable) = match pinned {
            // A pinned tool the live surface no longer offers cannot run. One it still
            // offers runs against its live definition, which validates the arguments.
            Some(pinned) => {
                let unavailable = pinned
                    .iter()
                    .filter(|tool| !live.iter().any(|live| live.name == tool.name))
                    .map(|tool| tool.name.clone())
                    .collect();
                (pinned, unavailable)
            }
            None => (live, Default::default()),
        };
        let template = ModelRequest {
            model: profile.profile.model.clone(),
            system,
            history: Vec::new(),
            tail: Vec::new(),
            history_lifetime: Default::default(),
            tools,
            reasoning: profile.profile.reasoning.clone(),
            response_schema: None,
            max_output_tokens: Some(profile.profile.max_output),
            blobs: Default::default(),
        };
        let template = template.try_into()?;
        let provider = provider.open_context(crate::provider::protocol::ContextId::from(agent))?;
        let open = |records: &[EventRecord]| {
            AgentContext::open(agent, profile, template, provider, records)
        };
        let mut context = self
            .store
            .visit_records_after(RecordSeq::default(), open)
            .await;
        context.unavailable_tools = Arc::new(unavailable);
        Ok(context)
    }
}

impl SessionRuntime {
    /// Give restored agent jobs a handler that resumes their child, starting it
    /// again under its journaled contract when its loop is gone.
    pub(super) async fn install_retained_children(self: &Arc<Self>, records: &[EventRecord]) {
        for retained in self.jobs.retained_children().await {
            // The owner holds what it would if it resumed itself.
            let Some(owner) = self.resumed_contract(records, &retained.owner) else {
                continue;
            };
            let authorization = crate::tool::authorization::AuthorizationSubject {
                agent: retained.owner,
                job: retained.job,
                parent: retained.parent,
                capabilities: owner.capabilities,
                cancellation: retained.cancellation,
            };
            let handler = super::tools::child_resume_handler(
                Arc::downgrade(self),
                retained.child,
                authorization,
                retained.location,
                owner.location,
            );
            let _ = self.jobs.set_resume_handler(retained.job, handler).await;
        }
    }

    /// `agent`'s journaled contract under the live configuration, which may narrow
    /// it but never widen it: a lowered depth limit narrows rather than refuses it.
    fn resumed_contract(
        &self,
        records: &[EventRecord],
        agent: &AgentId,
    ) -> Option<RecordedContract> {
        let mut contract = recorded_contract(records, agent)?;
        let remaining = self.harness.max_child_depth.saturating_sub(agent.depth());
        contract.available_depth = contract.available_depth.min(remaining);
        let allowed = self.capabilities.for_agent(contract.available_depth);
        contract.capabilities = &contract.capabilities & &allowed;
        Some(contract)
    }
}

/// The contract an agent runs under. A journaled one is its start, latest applied
/// profile, and the prompt and tools of its latest agent model context.
struct RecordedContract {
    profile: crate::session::ProfileSnapshot,
    available_depth: usize,
    mode: Option<ModeName>,
    capabilities: CapabilitySet,
    location: crate::execution::ExecutionLocation,
    system: Option<Vec<SystemSegment>>,
    tools: Option<Vec<crate::provider::protocol::ToolDefinition>>,
}

fn recorded_contract(records: &[EventRecord], agent: &AgentId) -> Option<RecordedContract> {
    let mut contract: Option<RecordedContract> = None;
    for record in records.iter().filter(|record| &record.agent == agent) {
        match &record.event {
            SessionEvent::AgentStarted {
                profile: Some(profile),
                available_depth,
                mode,
                capabilities,
                location,
                ..
            } => {
                contract = Some(RecordedContract {
                    profile: profile.clone(),
                    available_depth: *available_depth as usize,
                    mode: mode.as_ref().map(|mode| mode.name.clone()),
                    capabilities: capabilities.iter().copied().collect(),
                    location: location.clone(),
                    system: None,
                    tools: None,
                });
            }
            SessionEvent::ModelChanged { profile } => {
                if let Some(contract) = &mut contract {
                    contract.profile = profile.clone();
                }
            }
            SessionEvent::ModeChanged { mode, capabilities } => {
                if let Some(contract) = &mut contract {
                    contract.mode = Some(mode.name.clone());
                    contract.capabilities = capabilities.iter().copied().collect();
                    // The mode's prompt and tools are pinned by its next model context.
                    (contract.system, contract.tools) = (None, None);
                }
            }
            SessionEvent::ModelContext { context }
                if context.purpose == crate::session::ModelPurpose::Agent =>
            {
                if let Some(contract) = &mut contract {
                    contract.system = Some(context.system.clone());
                    contract.tools = Some(context.tools.clone());
                }
            }
            _ => {}
        }
    }
    contract
}
impl SessionRuntime {
    pub(super) async fn next_child(&self, parent: &AgentId) -> AgentId {
        let mut counters = self.child_counters.write().await;
        let counter = counters.entry(parent.clone()).or_insert(0);
        *counter = counter.saturating_add(1);
        parent.child(*counter)
    }

    pub(super) fn available_depth(&self, agent: &AgentId) -> usize {
        self.agents()
            .get(agent)
            .map_or(0, |agent| agent.available_depth)
    }

    pub(super) fn agent_sender(&self, id: &AgentId) -> Option<AgentSender> {
        self.agents().get(id).map(|agent| agent.sender.clone())
    }

    /// Register this agent's turn cancellation, or decline once stopping. Checked
    /// under the `agents` write lock: a token minted before `interrupt_tree`'s
    /// snapshot is cancelled by it, and none can be minted after.
    pub(super) fn begin_turn(&self, id: &AgentId) -> Option<CancellationToken> {
        let cancellation = CancellationToken::new();
        let mut agents = self.agents_mut();
        if self.shutting_down.load(Ordering::Acquire) {
            return None;
        }
        let agent = agents.get_mut(id).expect("running agents are registered");
        agent
            .control
            .retryable_interrupt
            .store(false, Ordering::Release);
        agent.cancellation = cancellation.clone();
        Some(cancellation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::runtime::tests::*;

    fn last_tool_results(request: &ModelRequest) -> &[ToolResult] {
        let Some(Sent::Tool(results)) = request.history.last() else {
            panic!("expected tool results at the end of the history");
        };
        results
    }

    /// A call that fails before its job exists has no handle, and its metadata
    /// would only repeat the call.
    #[tokio::test]
    async fn preadmission_failure_history_is_a_failure_without_a_job() {
        let call = tool_call(0, "missing-call", "missing", json!({"name":"unadmitted"}));
        let (_root, requests, session) =
            scripted_session([response(vec![call]), answer("done")]).await;
        assert_eq!(session.prompt("run").await.unwrap(), "done");
        let requests = requests.lock().unwrap();
        let results = last_tool_results(&requests[1]);
        assert!(results[0].is_error);
        let view = results[0].result.as_object().unwrap();
        assert_eq!(view["state"], "failed");
        assert_eq!(view.keys().collect::<Vec<_>>(), ["state", "error"]);
    }

    #[tokio::test]
    async fn committed_tool_history_preserves_null_fields() {
        let source = "return {error: null, nested: {absent: null, ok: false}, array: [null, 0]};";
        let call = tool_call(0, "script-call", "script", json!({ "source": source }));
        let (_root, requests, session) =
            scripted_session([response(vec![call]), answer("done")]).await;
        assert_eq!(session.prompt("run").await.unwrap(), "done");
        let records = session.runtime.store.records().await;
        let results = events!(&records, SessionEvent::MessageCommitted { message: Message::Tool(results) } => results);
        let value = &results[0][0].result["result"]["value"];
        assert_eq!(
            value,
            &json!({
                "error": null,
                "nested": {"absent": null, "ok": false},
                "array": [null, 0]
            })
        );
        assert_eq!(last_tool_results(&requests.lock().unwrap()[1]), results[0]);
    }

    #[tokio::test]
    async fn child_first_request_includes_parent_supplied_todos() {
        let todos = json!([
            {"text": "Inspect the implementation", "status": "completed"},
            {"text": "Make the change", "status": "in_progress"},
            {"text": "Run relevant checks", "status": "pending"},
            {"text": "Check \"quoted\" text\nand Unicode: café", "status": "pending"},
            {"text": "Keep the original order", "status": "completed"}
        ]);
        let arguments = json!({"prompt": "work", "todos": todos});
        let (_root, requests, session) = scripted_session([
            response(vec![tool_call(0, "delegate", "agent", arguments)]),
            answer("child done"),
            answer("root done"),
        ])
        .await;
        assert_eq!(session.prompt("delegate").await.unwrap(), "root done");
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        let child_request = &requests[1];
        let system = &child_request.system[0].text;
        assert!(system.starts_with(prompt::CHILD_PROMPT));
        let state = request_runtime_state(child_request);
        let (_, sections) = state.split_once('\n').unwrap();
        assert_eq!(
            sections,
            concat!(
                "todos:\n",
                "completed:\n",
                "  \"Inspect the implementation\"\n",
                "in_progress:\n",
                "  \"Make the change\"\n",
                "pending:\n",
                "  \"Run relevant checks\"\n",
                "  \"Check \\\"quoted\\\" text\\nand Unicode: café\"\n",
                "completed:\n",
                "  \"Keep the original order\"",
            )
        );
    }

    #[tokio::test]
    async fn delegated_depth_cannot_exceed_the_callers_budget() {
        let arguments = json!({"prompt":"too deep", "depth":4});
        let (_root, requests, session) = scripted_session([
            response(vec![tool_call(0, "agent-too-deep", "agent", arguments)]),
            answer("root done"),
        ])
        .await;
        assert_eq!(session.prompt("overdelegate").await.unwrap(), "root done");
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2, "an over-budget child must not start");
        let results = last_tool_results(&requests[1]);
        assert!(results[0].is_error);
        let error = results[0].result["error"].as_str().unwrap();
        assert!(error.contains("available depth of 4"));
    }

    #[tokio::test]
    async fn leaf_children_cannot_invoke_agent_directly_or_from_scripts() {
        let script = json!({"source":"return tool.agent({prompt: 'escape'});"});
        let hidden = json!({"prompt":"try hidden delegation"});
        let (_root, requests, session) = scripted_session([
            response(vec![tool_call(0, "root-agent", "agent", hidden)]),
            response(vec![
                tool_call(0, "hidden-agent", "agent", json!({"prompt":"escape"})),
                tool_call(1, "script-agent", "script", script),
            ]),
            answer("child done"),
            answer("root done"),
        ])
        .await;
        assert_eq!(session.prompt("delegate").await.unwrap(), "root done");
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 4, "hidden delegation must not start agents");
        let results = last_tool_results(&requests[2]);
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|result| result.is_error));
        let error = results[0].result["error"].as_str().unwrap();
        assert!(error.contains("unavailable"));
    }

    #[tokio::test]
    async fn launch_faults_never_install_a_live_agent() {
        use crate::session::AppendBoundary;
        for seed_fault in [false, true] {
            for boundary in AppendBoundary::ALL {
                let (_root, _, session) = scripted_session([]).await;
                let runtime = &session.runtime;
                let todos = Some(vec![todo("seed", crate::agent::TodoStatus::Pending)]);
                let id = runtime.next_child(&session.root).await;
                let launch = child_launch(&session, todos);
                let launch = PreparedAgentLaunch::prepare(runtime.clone(), id, None, launch)
                    .await
                    .unwrap();
                let agent = launch.agent_loop.id.clone();
                let sender = launch.sender.clone();
                let worker = if seed_fault {
                    let store = &runtime.store;
                    let (reached, resume) =
                        store.pause_append_at(AppendBoundary::Publication).await;
                    let worker = tokio::spawn(launch.install());
                    bounded(reached).await.unwrap();
                    // Queue the next fault on the writer's FIFO mutex before letting
                    // AgentStarted commit; TodosReplaced is the next launch append.
                    let fault = runtime.store.fail_append_at(boundary);
                    tokio::pin!(fault);
                    assert!(futures_util::poll!(fault.as_mut()).is_pending());
                    resume.send(()).unwrap();
                    bounded(fault).await;
                    worker
                } else {
                    runtime.store.fail_append_at(boundary).await;
                    tokio::spawn(launch.install())
                };
                assert!(bounded(worker).await.unwrap().is_err());
                bounded(sender.closed()).await;
                assert!(!runtime.agents.read().unwrap().contains_key(&agent));
                let seeded = runtime.todos.inspect(&agent, None).await.unwrap();
                assert!(seeded.is_empty());
                // A failed append may leave a replayable prefix, never a live agent.
                let visible = runtime.store.records().await;
                let prefix = visible.iter().filter(|r| r.agent == agent).count();
                assert_eq!(prefix, usize::from(seed_fault));
                let _ = bounded(session.shutdown()).await;
            }
        }
    }

    /// A resumed contract keeps the exact journaled set, never the defaults, and is
    /// narrowed by the live ceiling and depth limit but never widened: a depth
    /// reduced to zero drops only the ability to delegate.
    #[tokio::test]
    async fn resumed_contracts_narrow_exact_journaled_capabilities() {
        use Capability::{Agents, Interactive, Mcp, Read};
        let set = |capabilities: &[Capability]| -> CapabilitySet {
            capabilities.iter().copied().collect()
        };
        let (all, defaults) = (set(&Capability::ALL), CapabilitySet::default());
        let delegating = set(&[Read, Agents, Mcp, Interactive]);
        let delegated = set(&[Read, Mcp, Interactive]);
        let delegator = set(&[Read, Agents]);
        // (journaled, journaled depth, live ceiling, live depth limit) -> contract
        let cases = [
            (set(&[]), 1, defaults.clone(), 2, set(&[]), 1),
            (set(&[Read]), 1, all.clone(), 2, set(&[Read]), 1),
            (defaults, 1, delegator.clone(), 2, delegator, 1),
            (delegating.clone(), 1, all.clone(), 1, delegated, 0),
            (delegating.clone(), 1, all, 3, delegating, 1),
        ];
        for (journaled, depth, ceiling, limit, capabilities, available_depth) in cases {
            let root = tempfile::tempdir().unwrap();
            let provider = scripted_provider(&Requests::default(), []);
            let harness = test_builder(root.path(), &root.path().join("sessions"), provider, false)
                .capabilities(ceiling)
                .max_child_depth(limit)
                .build()
                .await
                .unwrap();
            let session = ephemeral_session(&harness).await;
            let runtime = &session.runtime;
            let child = session.root.child(1);
            let mut records = runtime.store.records().await;
            records.retain(|record| matches!(record.event, SessionEvent::AgentStarted { .. }));
            records[0].agent = child.clone();
            let SessionEvent::AgentStarted {
                available_depth: recorded_depth,
                capabilities: recorded,
                ..
            } = &mut records[0].event
            else {
                unreachable!("retained above");
            };
            (*recorded_depth, *recorded) = (depth, journaled.iter().collect());
            let contract = runtime.resumed_contract(&records, &child).unwrap();
            assert_eq!(
                (contract.capabilities, contract.available_depth),
                (capabilities, available_depth),
                "{journaled:?} at depth {depth} under {limit}"
            );
            shutdown_session(session).await;
        }
    }
}
