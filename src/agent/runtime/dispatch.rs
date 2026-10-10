//! Child-agent launch, tool dispatch, and model context selection.

use super::*;

// One owner binds the resolved provider context, admitted capabilities, loop
// controls and initial journal publication to the same launch identity. There
// is no config lookup or reconstruction after preparation.
struct PreparedAgentLaunch {
    runtime: Arc<SessionRuntime>,
    agent_loop: AgentLoop,
    available_depth: usize,
    start: Start,
}

/// Whether a prepared agent is journaled as it starts, with its initial todos.
enum Start {
    New(Option<Vec<TodoItem>>),
    /// Journaled before: resume under that contract without starting it again.
    Resume(Resumed),
}

/// The agent call a new child answers: its job, the prompt that begins the
/// child's first invocation, and what restarts the call once it ends.
pub(super) struct Call {
    pub(super) job: JobId,
    pub(super) prompt: String,
    pub(super) handler: crate::job::ResumeHandler,
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
        let (contract, meter, start) = match launch {
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
                (contract, meter, Start::New(todos))
            }
            AgentLaunch::Resume(resumed) => {
                let (contract, meter) = runtime.store.read(|state| {
                    let meter = super::compact::TokenMeter::restore(&state.records, &id);
                    (runtime.resumed_contract(&state.agents, &id), meter)
                });
                let contract = contract.ok_or_else(|| HarnessError::NoRecordedModel(id.clone()))?;
                (contract, meter, Start::Resume(resumed))
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
        let mut context = runtime.open_agent_context(&id, profile, system, &capabilities, tools)?;
        context.meter = meter;
        Ok(Self {
            runtime,
            agent_loop: AgentLoop {
                id,
                owner_job,
                context,
                location,
                settings: AgentSettings { mode, capabilities },
            },
            available_depth,
            start,
        })
    }

    /// Resume as `resumed` instead.
    fn resumed(self, resumed: Resumed) -> Self {
        let start = Start::Resume(resumed);
        Self { start, ..self }
    }

    /// Journal the agent's start, if new, and run its loop from the same commit.
    /// For `call`, that commit accepts the call: unless its job ended or its name
    /// belongs to another installed child of its owner, it installs the child
    /// with its prompt and the call's resume handler, so the start record alone
    /// marks a call that can run again. Returns where the child's answer arrives.
    async fn install(self, call: Option<Call>) -> Result<Option<ChildAnswer>, HarnessError> {
        let started = self.started(call.as_ref());
        let store = self.runtime.store.clone();
        let launch = move |state: &CoreState| {
            if let Some(call) = &call {
                state.jobs.accepts(call.job)?;
            }
            let run = move |state: &mut CoreState, _: &[EventRecord]| self.run(state, call);
            Ok::<_, HarnessError>(Plan::new(started, run))
        };
        store.commit(Command::LaunchAgent, launch).await
    }

    /// What journals a new agent's start, with its seeded todos and the prompt
    /// of its `call`; nothing for one that resumes.
    fn started(&self, call: Option<&Call>) -> Vec<(AgentId, SessionEvent)> {
        let Start::New(todos) = &self.start else {
            return Vec::new();
        };
        let (runtime, agent_loop) = (&self.runtime, &self.agent_loop);
        let started = SessionEvent::AgentStarted {
            owner_job: agent_loop.owner_job,
            profile: Some(agent_loop.context.profile.clone()),
            available_depth: u32::try_from(self.available_depth).unwrap_or(u32::MAX),
            mode: (agent_loop.settings.mode.as_ref()).map(|mode| runtime.mode_selection(mode)),
            capabilities: agent_loop.settings.capabilities.iter().collect(),
            location: agent_loop.location.clone(),
        };
        let id = &agent_loop.id;
        // Every entry references an agent, so a new session starts with its
        // root, the only agent that starts rather than resumes without an owner.
        let session = id.parent().is_none().then(|| SessionEvent::SessionStarted {
            targets: runtime.harness.target_definitions.clone(),
            capabilities: runtime.capabilities.iter().collect(),
        });
        // The seed lands with the start, so the child never shows an empty list.
        let seeded = (todos.clone()).map(|items| SessionEvent::TodosReplaced { items });
        let prompted = call.map(|call| {
            let text = call.prompt.clone();
            let message = Message::User(vec![UserPart::Text { text }]);
            SessionEvent::MessageCommitted { message }
        });
        let events = (session.into_iter().chain([started]).chain(seeded)).chain(prompted);
        events.map(|event| (id.clone(), event)).collect()
    }

    /// Install the agent in `state`, begun on `call` if it answers one, and run
    /// its loop once the change applies.
    fn run(self, state: &mut CoreState, call: Option<Call>) -> Option<ChildAnswer> {
        let Self {
            runtime,
            agent_loop,
            available_depth: _,
            start,
        } = self;
        let turn = match start {
            Start::Resume(Resumed::Parked(failure)) => Turn::Parked(failure),
            Start::New(_) | Start::Resume(Resumed::Idle) => Turn::Idle,
        };
        let id = agent_loop.id.clone();
        let facts = agents::AgentFacts {
            turn,
            since: chrono::Utc::now().timestamp_millis(),
            inbox: Default::default(),
            window: Default::default(),
            invocation: Invocation::Open,
        };
        state.agents.insert(id.clone(), facts);
        // The prompt is committed: the input that begins the turn carries none.
        let answer = call.map(|call| {
            let _ = runtime.jobs.install(state, call.job, call.handler.into());
            let (done, answer) = oneshot::channel();
            let input = AgentCommand::Input {
                options: Selection::default(),
                content: Vec::new(),
                done: Some(RequestCompletion::Child(done)),
            };
            state.agents.send(&id, input);
            answer
        });
        state.defer(move || {
            let supervisor = runtime.jobs.supervisor().clone();
            let window = runtime.harness.coalescing.clone();
            supervisor.spawn(wait::coalesce(runtime.store.clone(), id, window));
            supervisor.spawn(async move { runtime.run_agent(agent_loop).await });
        });
        answer
    }
}

/// Where a child's answer to the invocation an input began arrives.
pub(super) type ChildAnswer = oneshot::Receiver<Result<String, TurnFailure>>;

impl SessionRuntime {
    pub(super) async fn spawn_agent(
        self: &Arc<Self>,
        id: AgentId,
        owner_job: Option<JobId>,
        launch: AgentLaunch,
    ) -> Result<(), HarnessError> {
        let prepared = PreparedAgentLaunch::prepare(self.clone(), id, owner_job, launch);
        prepared.await?.install(None).await.map(drop)
    }

    /// Activate the viewed session as its root resumes, then let a later first
    /// call try again if it could not. The root is prepared first, so one that
    /// cannot be journals nothing; then one commit settles what the process that
    /// last ran the session left open and starts the root, parked as it read
    /// stopped, or interrupted once the settlement leaves it deliveries.
    pub(super) async fn activate_root(self: Arc<Self>) -> Result<(), HarnessError> {
        // Settlement closes only what this already reads as stopped.
        let root = AgentId::root(self.store.id());
        let stopped = self.store.read(|state| agents::stopped_turn(state, &root));
        let idle = AgentLaunch::Resume(Resumed::Idle);
        let prepared = PreparedAgentLaunch::prepare(self.clone(), root, None, idle).await?;
        let resume = move |state: &mut CoreState| {
            let resumed = match stopped {
                Some(failure) => Resumed::Parked(failure),
                None if state.jobs.has_pending(&prepared.agent_loop.id) => {
                    Resumed::Parked(TurnFailure::Interrupted)
                }
                None => Resumed::Idle,
            };
            prepared.resumed(resumed).run(state, None);
        };
        self.activate(resume).await
    }

    /// Start child `id` for `call`, accepting the call as its child is installed.
    pub(super) async fn launch_child(
        self: &Arc<Self>,
        id: AgentId,
        launch: AgentLaunch,
        call: Call,
    ) -> Result<ChildAnswer, HarnessError> {
        let prepared = PreparedAgentLaunch::prepare(self.clone(), id, Some(call.job), launch);
        let answer = prepared.await?.install(Some(call)).await?;
        Ok(answer.expect("a call's child answers it"))
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
    /// The call's result, with the delivery it reports for its commit to
    /// acknowledge. Tool dispatch owns its execution inputs. Erasing this future
    /// separates the driver's Send proof from the nested supervised executor graph.
    pub(super) fn run(
        self,
    ) -> futures_util::future::BoxFuture<'static, (ToolResult, Option<crate::job::Delivery>)> {
        Box::pin(async move {
            let (executor, call, created) = match self {
                Self::Settled(result) => return (result, None),
                Self::Created {
                    executor,
                    call,
                    created,
                } => (executor, call, created),
            };
            let reported = executor.run_reported(*created).await;
            let result = match reported.outcome {
                Ok(result) => ToolResult {
                    call_id: call.id().to_owned(),
                    name: call.name().to_owned(),
                    result: result.output.value,
                    images: result.output.images,
                    is_error: result.is_error,
                },
                Err(error) => failed_result(&call, &executor, error),
            };
            (result, reported.acknowledges)
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
            crate::target::TargetRef::Named(name) => {
                Some(self.router.targets(|targets| targets.get(name))?)
            }
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
    pub(super) fn open_agent_context(
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
        let mut context = self.store.read(|state| open(&state.records));
        context.unavailable_tools = Arc::new(unavailable);
        Ok(context)
    }
}

impl SessionRuntime {
    /// Give each agent job in `retained` a handler under its owner's journaled
    /// contract: one that resumes the child its call installed, or one that asks
    /// again for a call stopped before it was accepted.
    pub(super) fn install_retained(
        self: &Arc<Self>,
        state: &mut CoreState,
        retained: Vec<crate::job::RetainedChild>,
    ) {
        for retained in retained {
            // The owner holds what it would if it resumed itself.
            let Some(owner) = self.resumed_contract(&state.agents, &retained.owner) else {
                continue;
            };
            let resume = match retained.restart {
                crate::job::Restart::Child {
                    parent,
                    location,
                    cancellation,
                } => {
                    let authorization = crate::tool::authorization::AuthorizationSubject {
                        agent: retained.owner,
                        job: retained.job,
                        parent,
                        capabilities: owner.capabilities,
                        cancellation,
                    };
                    let handler = super::tools::child_resume_handler(
                        Arc::downgrade(self),
                        authorization,
                        location,
                        owner.location,
                    );
                    crate::job::Resume::Child(handler)
                }
                crate::job::Restart::Call => {
                    let executor = (self.executor.clone())
                        .with_location(owner.location)
                        .with_capabilities(owner.capabilities);
                    crate::job::Resume::Call(executor.relaunch())
                }
            };
            let _ = self.jobs.install(state, retained.job, resume);
        }
    }

    /// `agent`'s journaled contract under the live configuration, which may narrow
    /// it but never widen it: a lowered depth limit narrows rather than refuses it.
    pub(super) fn resumed_contract(
        &self,
        agents: &Agents,
        agent: &AgentId,
    ) -> Option<RecordedContract> {
        let mut contract = agents.contract(agent)?.clone();
        let remaining = self.harness.max_child_depth.saturating_sub(agent.depth());
        contract.available_depth = contract.available_depth.min(remaining);
        let allowed = self.capabilities.for_agent(contract.available_depth);
        contract.capabilities = &contract.capabilities & &allowed;
        Some(contract)
    }
}

impl SessionRuntime {
    /// Reserve `parent`'s next child id, before the launch that journals it.
    pub(super) async fn next_child(&self, parent: &AgentId) -> AgentId {
        let parent = parent.clone();
        self.store
            .live(move |state| state.agents.next_child(&parent))
            .await
    }

    /// The model `agent` delegates with and how deep it may delegate, whether it
    /// runs or not yet: a call relaunched after a reopen may run before its owner
    /// resumes.
    pub(super) fn delegation(&self, agent: &AgentId) -> Option<(ModelRef, usize)> {
        let contract = self.agents(|agents| self.resumed_contract(agents, agent))?;
        Some((contract.profile.name, contract.available_depth))
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
        let records = session.runtime.store.records();
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

    /// A call's launch is one append: a fault leaves no child, seed, prompt or
    /// resumable call, never part of them.
    #[tokio::test]
    async fn launch_faults_never_install_a_live_agent() {
        for committed in [false, true] {
            let (_root, _, session) = scripted_session([]).await;
            let runtime = &session.runtime;
            let todos = Some(vec![todo("seed", crate::agent::TodoStatus::Pending)]);
            let id = runtime.next_child(&session.root).await;
            let job = owner(&session, false).await;
            let launch = child_launch(&session, todos);
            let launch = PreparedAgentLaunch::prepare(runtime.clone(), id, Some(job), launch)
                .await
                .unwrap();
            let agent = launch.agent_loop.id.clone();
            let call = Call {
                job,
                prompt: "child task".into(),
                handler: Arc::new(|_, _| Box::pin(async { unreachable!("never resumed") })),
            };
            let refuse = |store| crate::session::tests::refuse(store, "BEFORE INSERT ON entry");
            let refused = (!committed).then(|| refuse(&runtime.store));
            if committed {
                runtime.store.fail_committed().await;
            }
            assert!(bounded(launch.install(Some(call))).await.is_err());
            drop(refused);
            assert!(runtime.agents(|agents| agents.get(&agent).is_none()));
            let launched = runtime.jobs.metadata(job).unwrap();
            assert!(!launched.resumable && runtime.jobs.child_agent(job).is_none());
            let seeded = runtime.store.inspect_todos(&agent, None).unwrap();
            assert!(seeded.is_empty());
            let visible = runtime.store.records();
            assert!(visible.iter().all(|record| record.agent != agent));
            let _ = bounded(session.shutdown()).await;
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
            let mut records = runtime.store.records();
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
            let mut agents = super::Agents::default();
            agents.observe(&records[0]);
            let contract = runtime.resumed_contract(&agents, &child).unwrap();
            assert_eq!(
                (contract.capabilities, contract.available_depth),
                (capabilities, available_depth),
                "{journaled:?} at depth {depth} under {limit}"
            );
            shutdown_session(session).await;
        }
    }
}
