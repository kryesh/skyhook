//! Child-agent launch, tool dispatch, and model context selection.

use super::*;

/// An agent's available depth and capabilities.
type Grants = (usize, CapabilitySet);

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
    /// Journaled before: resume as the journal, read beforehand, leaves it,
    /// without starting it again, read after how many of its loops had ended.
    Resume(Resumed, Box<agents::Journaled>, u64),
}

impl Start {
    /// Whether it resumes on `profile` in place of its journaled model, which its
    /// start journals and the images its history holds must suit.
    fn moves_to(&self, profile: &crate::session::ProfileSnapshot) -> bool {
        matches!(self, Self::Resume(_, read, _) if read.contract.profile.name != profile.name)
    }
}

/// The agent call a new child answers: its job, and the prompt that begins the
/// child's first invocation.
pub(super) struct Call {
    pub(super) job: JobId,
    pub(super) prompt: String,
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
        let (contract, (available_depth, capabilities), start) = match launch {
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
                    mode,
                    location,
                    context: None,
                };
                let grants = (available_depth, capabilities.for_agent(available_depth));
                (contract, grants, Start::New(todos))
            }
            // Resumed as its journal leaves its last turn.
            AgentLaunch::Resume { on } => {
                let ended = runtime.store.read(|state| state.agents.ended(&id));
                let (journaled, grants, stopped) = runtime.journaled(&id).await?;
                let resumed = stopped.map_or(Resumed::Idle, Resumed::Parked);
                let mut contract = journaled.contract.clone();
                // One whose own model is gone moves to the root's, which a root on a
                // gone model keeps until a selection replaces it.
                let on = on.or_else(|| {
                    let gone = !runtime.harness.models.contains_key(&contract.profile.name);
                    gone.then(|| runtime.root_model())
                });
                if let Some(name) = on.filter(|name| *name != contract.profile.name) {
                    let profile = runtime.model_entry(&name)?.profile;
                    contract.profile = crate::session::ProfileSnapshot { name, profile };
                }
                let start = Start::Resume(resumed, journaled.into(), ended);
                (contract, grants, start)
            }
        };
        let RecordedContract {
            profile,
            mode,
            location,
            context,
        } = contract;
        let (system, tools) = context
            .map(|(_, context)| (context.system, context.tools))
            .unzip();
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
        let context =
            (runtime.open_agent_context(&id, profile, system, &capabilities, tools)).await?;
        // A journaled profile ran the history it resumes; another must suit it.
        if start.moves_to(&context.model.profile) {
            context.admits_images()?;
        }
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

    /// Whether it resumes as its journal was read once `ended` of its loops had ended.
    fn read_after(&self, ended: u64) -> bool {
        matches!(self.start, Start::Resume(.., at) if at == ended)
    }

    /// Parked on `held`, the deliveries its idle resumption would rest on.
    fn resting_on(mut self, held: Option<TurnFailure>) -> Self {
        if let (Start::Resume(at @ Resumed::Idle, ..), Some(failure)) = (&mut self.start, held) {
            *at = Resumed::Parked(failure);
        }
        self
    }

    /// Journal the agent's start, if new, and run its loop from the same commit.
    /// For `call`, that commit accepts the call: unless its job ended or its name
    /// belongs to another installed child of its owner, it installs the child
    /// with its prompt, so the start record alone marks a call that can run
    /// again. Returns where the child's answer arrives.
    async fn install(self, call: Option<Call>) -> Result<Option<ChildAnswer>, HarnessError> {
        let started = self.started(call.as_ref());
        let store = self.runtime.store.clone();
        let launch = move |state: &CoreState| {
            if let Some(call) = &call {
                state.jobs.accepts(call.job)?;
            }
            let run = move |state: &mut CoreState, _: &[Arc<EventRecord>]| self.run(state, call);
            Ok::<_, HarnessError>(Plan::new(started, run))
        };
        store.commit(Command::LaunchAgent, launch).await
    }

    /// What journals a new agent's start, with its seeded todos and the prompt
    /// of its `call`; for one that resumes, the model it moves to, if any.
    fn started(&self, call: Option<&Call>) -> Vec<(AgentId, SessionEvent)> {
        let (runtime, agent_loop) = (&self.runtime, &self.agent_loop);
        let Start::New(todos) = &self.start else {
            let profile = &agent_loop.context.model.profile;
            let moved = (self.start.moves_to(profile)).then(|| {
                let profile = profile.clone();
                (
                    agent_loop.id.clone(),
                    SessionEvent::ModelChanged { profile },
                )
            });
            return moved.into_iter().collect();
        };
        let started = SessionEvent::AgentStarted {
            owner_job: agent_loop.owner_job,
            profile: agent_loop.context.model.profile.clone(),
            available_depth: u32::try_from(self.available_depth).unwrap_or(u32::MAX),
            mode: (agent_loop.settings.mode.as_ref()).map(AppliedMode::selection),
            capabilities: agent_loop.settings.capabilities.iter().collect(),
            location: agent_loop.location.clone(),
        };
        let id = &agent_loop.id;
        // Every entry references an agent, so a new session starts with its
        // root, the only agent that starts rather than resumes without an owner.
        let session = id.parent().is_none().then(|| SessionEvent::SessionStarted {
            targets: runtime.harness.target_definitions.clone(),
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
        // A resumed agent's last invocation resolved: only input opens the next.
        let (turn, invocation, read) = match start {
            Start::New(_) => (Turn::Idle, Invocation::Open, None),
            Start::Resume(Resumed::Idle, read, ..) => {
                (Turn::Idle, Invocation::Resolved, Some(read))
            }
            Start::Resume(Resumed::Parked(failure), read, ..) => {
                (Turn::Parked(failure), Invocation::Resolved, Some(read))
            }
        };
        // As of its start, which journals the model it moves to.
        let read = read.map(|mut read| {
            read.contract.profile = agent_loop.context.model.profile.clone();
            *read
        });
        let id = agent_loop.id.clone();
        let facts = agents::AgentFacts {
            turn,
            since: chrono::Utc::now().timestamp_millis(),
            inbox: Default::default(),
            window: Default::default(),
            invocation,
            turn_token: runtime.jobs.supervisor().token(),
            held: None,
        };
        state.agents.insert(id.clone(), facts, read);
        // The prompt is committed: the input that begins the turn carries none.
        let answer = call.map(|_| {
            let (done, answer) = oneshot::channel();
            let input = AgentCommand::Input {
                options: Selection::default(),
                content: Vec::new(),
                done: Some(RequestCompletion::Child(done)),
            };
            state.agents.send(&id, input);
            answer
        });
        if let Some(job) = agent_loop.owner_job {
            runtime.jobs.hold_handles(state, job);
        }
        state.defer(move || {
            let supervisor = runtime.jobs.supervisor().clone();
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
    /// stopped, or on the deliveries the settlement leaves it, as the live turn
    /// would rest.
    pub(super) async fn activate_root(
        self: Arc<Self>,
        on: Option<ModelRef>,
    ) -> Result<(), HarnessError> {
        // Settlement closes only what preparing already read as stopped.
        let root = AgentId::root(self.store.id());
        let resume = AgentLaunch::Resume { on };
        let prepared = PreparedAgentLaunch::prepare(self.clone(), root, None, resume).await?;
        let started = prepared.started(None);
        let resume = move |state: &mut CoreState| {
            let held = agents::held_deliveries(state, &prepared.agent_loop.id);
            prepared.resting_on(held).run(state, None);
        };
        self.activate(started, resume).await
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

    /// Plan a response's calls concurrently, then publish their jobs in call
    /// order in one commit before any runs, so a `wait` among them sees its
    /// siblings as outstanding work from the start. A call that fails planning
    /// settles alone.
    pub(super) async fn create_calls(
        &self,
        agent: &AgentId,
        parent: Option<JobId>,
        calls: &[ToolCall],
        origin: MessageSeq,
        location: &crate::execution::ExecutionLocation,
        capabilities: &CapabilitySet,
    ) -> Vec<CreatedCall> {
        let executors: Vec<_> = (calls.iter())
            .map(|call| {
                (self.executor.clone())
                    .with_location(location.clone())
                    .with_capabilities(capabilities.clone())
                    .with_model_origin(crate::session::ModelCallOrigin {
                        message: origin,
                        call_id: call.id().to_owned(),
                    })
            })
            .collect();
        let plans = calls.iter().zip(&executors).map(|(call, executor)| {
            let arguments = serde_json::Value::Object(call.arguments().clone());
            let kind = crate::tool::executor::InvocationKind::Model;
            executor.plan(kind, agent.clone(), call.name(), arguments, parent)
        });
        let planned = futures_util::future::join_all(plans).await;
        let created = self.executor.publish(planned).await;
        (created.into_iter().zip(executors).zip(calls))
            .map(|((created, executor), call)| match created {
                Ok(created) => CreatedCall::Created {
                    executor,
                    call: call.clone(),
                    created: Box::new(created),
                },
                Err(error) => CreatedCall::Settled(failed_result(call, &executor, error)),
            })
            .collect()
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

/// One `error` result per call, in call order, closing each exchange: the
/// failure view of a call that never ran as a job.
pub(super) fn unrun_results<'a>(
    calls: impl Iterator<Item = (&'a AgentId, &'a str, &'a str)>,
    error: &str,
) -> Vec<(AgentId, SessionEvent)> {
    calls
        .map(|(agent, call_id, name)| {
            let failure = crate::job::JobView::failure(error.into(), None, false);
            let output = crate::tool::ToolOutput::new(failure.into_value());
            let message = Message::Tool(failed_tool_result(call_id.into(), name.into(), output));
            (agent.clone(), SessionEvent::MessageCommitted { message })
        })
        .collect()
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

    /// The model the root has committed to.
    pub(super) fn root_model(&self) -> ModelRef {
        let root = AgentId::root(self.store.id());
        let committed = |state: &CoreState| {
            let held = state.agents.journaled(&root);
            held.map(|held| held.contract.profile.name.clone())
        };
        let committed = self.store.read(committed);
        committed.expect("the root's journal is held from its start")
    }

    /// The edition of `name` the configuration defines now.
    pub(super) fn applied(&self, name: &ModeName) -> Result<AppliedMode, HarnessError> {
        let definition = self.modes.get(name);
        let definition = definition.ok_or_else(|| HarnessError::UnknownMode(name.clone()))?;
        Ok(AppliedMode::configured(name, definition))
    }

    pub(super) async fn system_prompt(
        &self,
        agent: &AgentId,
        location: &crate::execution::ExecutionLocation,
        available_depth: usize,
        mode: Option<&AppliedMode>,
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
            mode: mode.map(|mode| (mode.name.as_str(), &*mode.definition)),
            capabilities,
        })];
        Ok(system)
    }

    /// Open a context for `profile` on the live provider of its model, offering the
    /// live tool surface, or `pinned` tools journaled earlier. A pinned tool runs
    /// against its live definition, which validates the arguments; one the live
    /// surface no longer offers stays visible to the model but fails when called.
    /// While the configuration no longer has the model, only the root opens, with
    /// no provider, until a selection replaces it: a child refused here starts
    /// again on the root's model once it has one.
    pub(super) async fn open_agent_context(
        &self,
        agent: &AgentId,
        profile: crate::session::ProfileSnapshot,
        system: Vec<SystemSegment>,
        capabilities: &CapabilitySet,
        pinned: Option<Vec<crate::provider::protocol::ToolDefinition>>,
    ) -> Result<AgentContext, HarnessError> {
        let id = crate::provider::protocol::ContextId::from(agent);
        let provider = match self.harness.models.get(&profile.name) {
            Some(entry) => Some(entry.provider.open_context(id)?),
            None if agent.parent().is_none() => None,
            None => return Err(HarnessError::UnknownModel(profile.name)),
        };
        let tools = pinned.unwrap_or_else(|| {
            let executor = self
                .executor
                .clone()
                .with_capabilities(capabilities.clone());
            executor.surface_for_agent(agent).definitions()
        });
        let model = crate::session::ModelContext {
            purpose: crate::session::ModelPurpose::Agent,
            profile,
            system,
            tools,
            response_schema: None,
        };
        Ok(AgentContext::open(agent, model, provider, &self.store).await?)
    }
}

impl crate::job::Restarter for SessionRuntime {
    /// Under its owner's journaled contract, as the owner would resume: a child
    /// runs again with its owner's input, and a call stopped before it was
    /// accepted is asked for again, once its owner's loop runs to take what it
    /// delivers.
    fn resume(self: Arc<Self>, restarting: crate::job::Restarting) -> Option<crate::job::Resume> {
        let runtime = Arc::downgrade(&self);
        Some(match restarting.restart {
            crate::job::Restart::Child => {
                crate::job::Resume::Child(tools::child_resume_handler(runtime, restarting))
            }
            crate::job::Restart::Call => crate::job::Resume::Call(Arc::new(move |lease| {
                let (runtime, owner) = (runtime.upgrade(), restarting.owner.clone());
                let job = restarting.job;
                Box::pin(async move {
                    let Some(runtime) = runtime else {
                        return;
                    };
                    let owned = |_: &CoreState| Ok(Plan::journal(Vec::new()));
                    let started = match runtime.with_loops(&owner, owned).await {
                        Ok(()) => runtime.contract(&owner).await,
                        Err(error) => Err(error),
                    };
                    match started {
                        Ok((location, capabilities)) => {
                            let relaunch = (runtime.executor.clone())
                                .with_location(location)
                                .with_capabilities(capabilities)
                                .relaunch();
                            relaunch(lease).await;
                        }
                        Err(error) => lease.fail(tools::not_restarted(job, error).into()).await,
                    }
                })
            })),
        })
    }
}

/// What a change that needs loops found.
enum Looped<T, P> {
    Ran(T),
    /// These were parked without a loop prepared for them, which the change hands
    /// back with those it was given and the plan it did not make.
    Parked(Vec<(AgentId, JobId)>, Vec<PreparedAgentLaunch>, P),
}

impl SessionRuntime {
    /// Commit what `plan` makes, which needs `agent`'s loop and that of each
    /// child above it, whose owner takes what it delivers. The change that
    /// commits it starts each of them that is parked, prepared beforehand, so
    /// none runs two loops or parks between the check and the change. A child
    /// whose job can no longer run it is stopped; the root's loop is its own.
    pub(super) fn with_loops<'a, T, P>(
        self: &'a Arc<Self>,
        agent: &'a AgentId,
        plan: P,
    ) -> futures_util::future::BoxFuture<'a, Result<T, HarnessError>>
    where
        T: Send + 'static,
        P: FnOnce(&CoreState) -> Result<Plan<T>, HarnessError> + Send + 'static,
    {
        Box::pin(async move {
            let (mut prepared, mut plan) = (Vec::new(), plan);
            loop {
                let agent = agent.clone();
                let mut launches: Vec<PreparedAgentLaunch> = std::mem::take(&mut prepared);
                let looped = move |state: &CoreState| {
                    let (mut parked, mut starts) = (Vec::new(), Vec::new());
                    let chain = std::iter::successors(Some(agent.clone()), AgentId::parent);
                    for child in chain.filter(|child| child.parent().is_some()) {
                        match agents::child_loop(state, &child) {
                            agents::ChildLoop::Running => {}
                            agents::ChildLoop::Parked(job, ended) => {
                                let launch = launches.iter().position(|l| l.agent_loop.id == child);
                                // One read before its last loop ended is stale.
                                match launch.map(|at| launches.swap_remove(at)) {
                                    Some(launch) if launch.read_after(ended) => starts.push(launch),
                                    _ => parked.push((child, job)),
                                }
                            }
                            agents::ChildLoop::Stopped if child == agent => {
                                return Err(HarnessError::AgentStopped);
                            }
                            agents::ChildLoop::Stopped => {}
                        }
                    }
                    if !parked.is_empty() {
                        launches.append(&mut starts);
                        let parked = Looped::Parked(parked, launches, plan);
                        return Ok(Plan::new(Vec::new(), move |_, _| parked));
                    }
                    if !starts.is_empty() {
                        state.phase().admits(Command::LaunchAgent)?;
                    }
                    let moved = starts
                        .iter()
                        .flat_map(|start| start.started(None))
                        .collect();
                    let start = move |state: &mut CoreState| {
                        starts
                            .into_iter()
                            .for_each(|start| drop(start.run(state, None)));
                    };
                    Ok(plan(state)?.after(moved, start, Looped::Ran))
                };
                match self.store.commit(Command::Journal, looped).await? {
                    Looped::Ran(value) => return Ok(value),
                    Looped::Parked(parked, launches, unplanned) => {
                        (prepared, plan) = (launches, unplanned);
                        let prepare = parked.into_iter().map(|(child, job)| {
                            let resume = AgentLaunch::Resume { on: None };
                            PreparedAgentLaunch::prepare(self.clone(), child, Some(job), resume)
                        });
                        for launch in futures_util::future::join_all(prepare).await {
                            prepared.push(launch?);
                        }
                    }
                }
            }
        })
    }

    /// What `agent`'s journal leaves of its contract, its grants, and why its last
    /// turn stopped short of an answer, if it did: the contract is held while it
    /// runs, as the root's always is, and otherwise read from the records its state
    /// names.
    async fn journaled(
        &self,
        agent: &AgentId,
    ) -> Result<(agents::Journaled, Grants, Option<TurnFailure>), HarnessError> {
        let id = agent.clone();
        let read = self.store.read(move |state| {
            let started = state.agent_states.get(&id)?;
            let held = state.agents.journaled(&id).cloned().ok_or_else(|| {
                let context = (state.requests).context(&id, crate::session::ModelPurpose::Agent);
                ([started.profiled, started.granted], context)
            });
            let stopped = started.stopped(&id, &state.requests);
            Some((held, self.granted(state, &id)?, stopped))
        });
        let unrecorded = || HarnessError::NoRecordedModel(agent.clone());
        let (held, grants, stopped) = read.ok_or_else(unrecorded)?;
        let held = match held {
            Ok(held) => held,
            Err((entries, context)) => {
                let (journal, id) = (self.store.journal(), agent.clone());
                let read = move || {
                    let read = journal.agent(&id, entries, context)?;
                    let read = read.and_then(|read| agents::Journaled::read(&id, &read));
                    Ok::<_, crate::session::SessionError>(read)
                };
                let held = self.store.reads().hold(read).await?;
                held.ok_or_else(unrecorded)?
            }
        };
        Ok((held, grants, stopped))
    }

    /// Where `agent` runs, and what it holds.
    pub(super) async fn contract(
        &self,
        agent: &AgentId,
    ) -> Result<(crate::execution::ExecutionLocation, CapabilitySet), HarnessError> {
        let (journaled, (_, capabilities), _) = self.journaled(agent).await?;
        Ok((journaled.contract.location, capabilities))
    }

    /// The depth and capabilities `state` journaled for `agent`, under the live
    /// configuration.
    pub(super) fn granted(&self, state: &CoreState, agent: &AgentId) -> Option<Grants> {
        let started = state.agent_states.get(agent)?;
        Some(self.resumed_grants(agent, (started.available_depth, &started.capabilities)))
    }

    /// Journaled grants under the live configuration, which may narrow them but
    /// never widen them: a lowered depth limit narrows rather than refuses them.
    pub(super) fn resumed_grants(
        &self,
        agent: &AgentId,
        (available_depth, capabilities): (usize, &CapabilitySet),
    ) -> Grants {
        let remaining = self.harness.max_child_depth.saturating_sub(agent.depth());
        let depth = available_depth.min(remaining);
        let capabilities = capabilities & &self.capabilities.for_agent(depth);
        (depth, capabilities)
    }

    /// What `pick` reads of `agent`'s journaled contract, given its grants, while
    /// its journal is held.
    pub(super) fn held<T>(
        &self,
        agent: &AgentId,
        pick: impl FnOnce(&RecordedContract, usize, CapabilitySet) -> T,
    ) -> Option<T> {
        self.store.read(|state| {
            let contract = &state.agents.journaled(agent)?.contract;
            let (depth, capabilities) = self.granted(state, agent)?;
            Some(pick(contract, depth, capabilities))
        })
    }
}

impl SessionRuntime {
    /// Reserve running `parent`'s next child id, before the launch that journals
    /// it.
    pub(super) async fn next_child(&self, parent: &AgentId) -> Option<AgentId> {
        let parent = parent.clone();
        self.store
            .live(move |state| state.agents.next_child(&parent))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::runtime::tests::*;

    fn last_tool_results(request: &ModelRequest) -> &[ToolResult] {
        let Some(Sent::Tool(results)) = request.messages().last() else {
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

    /// A response's calls are created in one commit: a call that fails planning
    /// leaves its siblings created in call order with contiguous ids, and a
    /// refused commit leaves none of them, never the ones before the refusal.
    #[tokio::test]
    async fn a_responses_jobs_are_created_together_in_call_order() {
        for commits in [true, false] {
            let root = tempfile::tempdir().unwrap();
            let sessions = root.path().join("sessions");
            let requests = Requests::default();
            let exec = json!({"command": "true"});
            let calls = vec![
                tool_call(0, "first", "exec", exec.clone()),
                tool_call(1, "unplanned", "missing", json!({})),
                tool_call(2, "third", "exec", exec),
            ];
            let provider = scripted_provider(&requests, [response(calls), answer("done")]);
            let harness = test_harness(root.path(), &sessions, provider).await;
            let session = harness.new_session().await.unwrap();
            let refused = (!commits).then(|| {
                let rule = "BEFORE INSERT ON job WHEN NEW.id = 2";
                crate::session::tests::refuse(&session.runtime.store, rule)
            });
            assert_eq!(session.prompt("run").await.is_ok(), commits);
            drop(refused);
            let created = |jobs: Vec<crate::job::JobEnvelope>| {
                (jobs.into_iter())
                    .map(|job| (job.id.get(), job.origin.unwrap().call_id))
                    .collect::<Vec<_>>()
            };
            if commits {
                let jobs = session.runtime.jobs.list(&session.root).await.unwrap();
                let expected = [(1, "first".to_owned()), (2, "third".to_owned())];
                assert_eq!(created(jobs), expected);
                shutdown_session(session).await;
                continue;
            }
            let id = session.id();
            assert!(bounded(session.shutdown()).await.is_err());
            assert!(session.close().await.is_err());
            let resumed = harness.resume_session(id).await.unwrap();
            let jobs = resumed.runtime.jobs.list(&resumed.root).await.unwrap();
            assert_eq!(created(jobs), []);
            shutdown_session(resumed).await;
        }
    }

    #[tokio::test]
    async fn committed_tool_history_preserves_null_fields() {
        let source = "return {error: null, nested: {absent: null, ok: false}, array: [null, 0]};";
        let call = tool_call(0, "script-call", "script", json!({ "source": source }));
        let (_root, requests, session) =
            scripted_session([response(vec![call]), answer("done")]).await;
        assert_eq!(session.prompt("run").await.unwrap(), "done");
        let records = session.runtime.store.records();
        let results = events!(&records, SessionEvent::MessageCommitted { message: Message::Tool(result) } => result);
        let value = &results[0].result["result"]["value"];
        assert_eq!(
            value,
            &json!({
                "error": null,
                "nested": {"absent": null, "ok": false},
                "array": [null, 0]
            })
        );
        assert_eq!(
            last_tool_results(&requests.lock().unwrap()[1]),
            [results[0].clone()]
        );
    }

    /// A call's launch is one commit: a fault refusing it leaves no child, seed or
    /// prompt, never part of them; one failing to acknowledge it leaves what it
    /// committed, as reopening shows it, and starts no agent.
    #[tokio::test]
    async fn launch_faults_never_install_a_live_agent() {
        for committed in [false, true] {
            let (_root, _, session) = scripted_session([]).await;
            let runtime = &session.runtime;
            let todos = Some(vec![todo("seed", crate::agent::TodoStatus::Pending)]);
            let id = runtime.next_child(&session.root).await.unwrap();
            let job = owner(&session, false).await;
            let launch = child_launch(&session, todos);
            let launch = PreparedAgentLaunch::prepare(runtime.clone(), id, Some(job), launch)
                .await
                .unwrap();
            let agent = launch.agent_loop.id.clone();
            let call = Call {
                job,
                prompt: "child task".into(),
            };
            let refuse = |store| crate::session::tests::refuse(store, "BEFORE INSERT ON entry");
            let refused = (!committed).then(|| refuse(&runtime.store));
            if committed {
                runtime.store.fail_committed().await;
            }
            assert!(bounded(launch.install(Some(call))).await.is_err());
            drop(refused);
            assert!(runtime.agents(|agents| agents.get(&agent).is_none()));
            if !committed {
                assert!(runtime.jobs.child_agent(job).await.is_none());
                let seeded = runtime.store.inspect_todos(&agent, None).unwrap();
                assert!(seeded.is_empty());
            }
            let visible = runtime.store.observe().snapshot;
            assert_eq!(visible.views.entries.contains_key(&agent), committed);
            let _ = bounded(session.shutdown()).await;
        }
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
        let arguments = json!({"prompt": "work", "todos": todos.clone()});
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
        assert!(state.contains("\ntodos:\n"), "{state}");
        let seeded: Vec<crate::agent::TodoItem> = serde_json::from_value(todos).unwrap();
        let child = session.root.child(1);
        let journaled = session.runtime.store.inspect_todos(&child, None).unwrap();
        assert_eq!(journaled, seeded);
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

    /// Resumed grants are the exact journaled set, never the defaults, narrowed by
    /// the live ceiling and depth limit but never widened: a depth reduced to zero
    /// drops only the ability to delegate.
    #[tokio::test]
    async fn resumed_grants_narrow_exact_journaled_capabilities() {
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
            let child = session.root.child(1);
            assert_eq!(
                session.runtime.resumed_grants(&child, (depth, &journaled)),
                (available_depth, capabilities),
                "{journaled:?} at depth {depth} under {limit}"
            );
            shutdown_session(session).await;
        }
    }
}
