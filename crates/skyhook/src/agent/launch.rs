//! Launching agents: preparing an agent's contract and context, installing its loop,
//! and restarting the child a job runs again.

use super::{
    AgentLaunch, AgentLoop, AgentSettings, HarnessError, Invocation, Resumed, SessionRuntime, Turn,
    agents, child,
};
use crate::identity::AgentId;
use crate::session::{
    AppliedMode, Command, Contract, CoreState, EventRecord, Journaled, Message, Plan, SessionEvent,
    TodoItem, TurnFailure, UserPart,
};
use crate::tool::policy::CapabilitySet;
use skyhook_worker::job::JobId;
use std::sync::Arc;
use tokio::sync::oneshot;

/// An agent's available depth and capabilities.
type Grants = (usize, CapabilitySet);

/// Journaled grants under the live ceiling and the depth the agent has left,
/// which may narrow them but never widen them: a lowered depth limit narrows
/// rather than refuses them.
fn resumed_grants(
    ceiling: &CapabilitySet,
    remaining: usize,
    (available_depth, capabilities): (usize, &CapabilitySet),
) -> Grants {
    let depth = available_depth.min(remaining);
    (depth, capabilities & &ceiling.for_agent(depth))
}

// One owner binds the resolved provider context, admitted capabilities, loop
// controls and initial journal publication to the same launch identity.
pub(super) struct PreparedAgentLaunch {
    runtime: Arc<SessionRuntime>,
    pub(super) agent_loop: AgentLoop,
    available_depth: usize,
    start: Start,
}

/// Whether a prepared agent is journaled as it starts, with its initial todos.
enum Start {
    New(Option<Vec<TodoItem>>),
    /// Journaled before: resume as the journal, read beforehand, leaves it,
    /// without starting it again, read after how many of its loops had ended.
    Resume(Resumed, Box<Journaled>, u64),
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
    pub(super) async fn prepare(
        runtime: Arc<SessionRuntime>,
        id: AgentId,
        owner_job: Option<JobId>,
        launch: AgentLaunch,
    ) -> Result<Self, HarnessError> {
        if id.depth() > runtime.harness.max_child_depth {
            return Err(HarnessError::ChildDepth);
        }
        let (contract, (available_depth, capabilities), start, facts) = match launch {
            AgentLaunch::New {
                model,
                todos,
                available_depth,
                location,
                mode,
                capabilities,
                facts,
            } => {
                let contract = Contract {
                    profile: crate::session::ProfileSnapshot {
                        profile: runtime.model_entry(&model)?.profile,
                        name: model,
                    },
                    mode,
                    location,
                    context: None,
                };
                let grants = (available_depth, capabilities.for_agent(available_depth));
                let facts = facts.map(|facts| *facts);
                (contract, grants, Start::New(todos), Some(facts))
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
                (contract, grants, start, None)
            }
        };
        let Contract {
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
                // One resumed without a context has no call that approved a connection,
                // so a remote target's facts are known only from the launch that brought it.
                let facts = facts.unwrap_or_else(|| match location.target {
                    skyhook_tools::target::TargetRef::Root => Some(runtime.executor.root_facts()),
                    skyhook_tools::target::TargetRef::Named(_) => None,
                });
                let mode = mode.as_ref();
                runtime.system_prompt(
                    &id,
                    &location,
                    available_depth,
                    mode,
                    &capabilities,
                    facts,
                )?
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
    pub(super) fn resting_on(mut self, held: Option<TurnFailure>) -> Self {
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
        store.commit(Command::Start, launch).await
    }

    /// What journals a new agent's start, with its seeded todos and the prompt
    /// of its `call`; for one that resumes, the model it moves to, if any.
    pub(super) fn started(&self, call: Option<&Call>) -> Vec<(AgentId, SessionEvent)> {
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
            capabilities: agent_loop.settings.capabilities.clone(),
            location: agent_loop.location.clone(),
        };
        let id = &agent_loop.id;
        // Every entry references an agent, so a new session starts with its
        // root, the only agent that starts rather than resumes without an owner.
        let session = id.parent().is_none().then(|| SessionEvent::SessionStarted {
            host: runtime.store.host().clone(),
            targets: runtime.harness.targets.definitions(),
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
    pub(super) fn run(self, state: &mut CoreState, call: Option<Call>) -> Option<ChildAnswer> {
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
        state.contracts.hold(&id, read);
        state.agents.insert(id.clone(), facts);
        let answer = call.map(|_| {
            let (done, answer) = oneshot::channel();
            state.agents.begin(&id, done);
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
                crate::job::Resume::Child(child::child_resume_handler(runtime, restarting))
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
                        Ok(()) => runtime.journaled(&owner).await,
                        Err(error) => Err(error),
                    };
                    match started {
                        Ok((journaled, (_, capabilities), _)) => {
                            let relaunch = (runtime.executor.clone())
                                .with_location(journaled.contract.location)
                                .with_capabilities(capabilities)
                                .relaunch();
                            relaunch(lease).await;
                        }
                        Err(error) => lease.fail(child::not_restarted(job, error).into()).await,
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
                        state.phase().admits(Command::Start)?;
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
    pub(super) async fn journaled(
        &self,
        agent: &AgentId,
    ) -> Result<(Journaled, Grants, Option<TurnFailure>), HarnessError> {
        let recorded = self.store.contract(agent).await?;
        let recorded = recorded.ok_or_else(|| HarnessError::NoRecordedModel(agent.clone()))?;
        let journaled = (recorded.available_depth, &recorded.capabilities);
        let grants = self.narrowed(agent, journaled);
        Ok((recorded.journaled, grants, recorded.stopped))
    }

    /// The depth and capabilities `state` journaled for `agent`, under the live
    /// configuration.
    pub(super) fn granted(&self, state: &CoreState, agent: &AgentId) -> Option<Grants> {
        let started = state.agent_states.get(agent)?;
        Some(self.narrowed(agent, (started.available_depth, &started.capabilities)))
    }

    /// `agent`'s `journaled` grants under the live configuration.
    fn narrowed(&self, agent: &AgentId, journaled: (usize, &CapabilitySet)) -> Grants {
        let remaining = self.harness.max_child_depth.saturating_sub(agent.depth());
        resumed_grants(&self.capabilities, remaining, journaled)
    }

    /// What `pick` reads of `agent`'s journaled contract, given its grants, while
    /// its journal is held.
    pub(super) fn held<T>(
        &self,
        agent: &AgentId,
        pick: impl FnOnce(&Contract, usize, CapabilitySet) -> T,
    ) -> Option<T> {
        self.store.read(|state| {
            let contract = state.contracts.get(agent)?;
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
            .live(move |state| state.contracts.next_child(&parent))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::queue::tests::{count, parent_inputs, start, stop};
    use crate::agent::tests::*;

    /// A call's launch is one commit: a fault refusing it leaves no child, seed or
    /// prompt, never part of them; one failing to acknowledge it leaves what it
    /// committed, as reopening shows it, and starts no agent.
    #[tokio::test]
    async fn launch_faults_never_install_a_live_agent() {
        for committed in [false, true] {
            let (_root, _, session) = scripted_session([]).await;
            let runtime = &session.runtime;
            let todos = Some(vec![todo("seed", crate::session::TodoStatus::Pending)]);
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
        let state = request_runtime_state(child_request);
        assert!(
            state.contains("\nin_progress:\n  Make the change\n"),
            "{state}"
        );
        let seeded: Vec<crate::session::TodoItem> = serde_json::from_value(todos).unwrap();
        let child = session.root.child(1);
        let journaled = session.runtime.store.inspect_todos(&child, None).unwrap();
        assert_eq!(journaled, seeded);
    }

    /// A delegation deeper than the caller's budget, and any from a leaf child,
    /// directly or from a script, is refused without starting an agent.
    #[tokio::test]
    async fn leaf_children_cannot_invoke_agent_directly_or_from_scripts() {
        let too_deep = json!({"prompt":"too deep", "depth":4});
        let script = json!({"source":"return tool.agent({prompt: 'escape'});"});
        let hidden = json!({"prompt":"try hidden delegation"});
        let (_root, requests, session) = scripted_session([
            response(vec![tool_call(0, "agent-too-deep", "agent", too_deep)]),
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
        assert_eq!(
            requests.len(),
            5,
            "refused delegations must not start agents"
        );
        let results = last_tool_results(&requests[1]);
        assert!(results[0].is_error);
        let error = results[0].result["error"].as_str().unwrap();
        assert!(error.contains("available depth of 4"));
        let results = last_tool_results(&requests[3]);
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|result| result.is_error));
        let error = results[0].result["error"].as_str().unwrap();
        assert!(error.contains("unavailable"));
    }

    /// Resumed grants are the exact journaled set, never the defaults, narrowed by
    /// the live ceiling and depth limit but never widened: a depth reduced to zero
    /// drops only the ability to delegate.
    #[test]
    fn resumed_grants_narrow_exact_journaled_capabilities() {
        use Capability::{Agents, Interactive, Mcp, Read};
        let set = |capabilities: &[Capability]| -> CapabilitySet {
            capabilities.iter().cloned().collect()
        };
        let (all, defaults) = (set(&Capability::ALL), CapabilitySet::default());
        let delegating = set(&[Read, Agents, Mcp, Interactive]);
        let delegated = set(&[Read, Mcp, Interactive]);
        let delegator = set(&[Read, Agents]);
        // (journaled, journaled depth, live ceiling, depth left) -> contract
        let cases = [
            (set(&[]), 1, defaults.clone(), 1, set(&[]), 1),
            (set(&[Read]), 1, all.clone(), 1, set(&[Read]), 1),
            (defaults, 1, delegator.clone(), 1, delegator, 1),
            (delegating.clone(), 1, all.clone(), 0, delegated, 0),
            (delegating.clone(), 1, all, 2, delegating, 1),
        ];
        for (journaled, depth, ceiling, left, capabilities, available_depth) in cases {
            assert_eq!(
                resumed_grants(&ceiling, left, (depth, &journaled)),
                (available_depth, capabilities),
                "{journaled:?} at depth {depth} with {left} left"
            );
        }
    }

    #[tokio::test]
    async fn child_names_are_owner_scoped_and_survive_completion_but_not_failed_launches() {
        let (_root, tracking, session) = start(false).await;
        let runtime = &session.runtime;
        let root_inbox = quiet_root(&session);
        // Each invocation is a separate script, but the caller owns its children.
        let call = async |owner: &AgentId, source: &str| {
            bounded(runtime.executor.execute(
                owner.clone(),
                "script",
                json!({"source":source}),
                None,
            ))
            .await
            .unwrap()
            .output
            .value["value"]
                .clone()
        };
        let launch = |name| {
            format!("return await tool.agent({{prompt:'test:{name}',name:'{name}',bg:true}});")
        };
        let id =
            |view: &serde_json::Value| serde_json::from_value::<JobId>(view["id"].clone()).unwrap();
        let refused = |view: &serde_json::Value, existing: JobId| {
            assert_eq!(view["state"], "failed");
            let error = view["error"].as_str().unwrap();
            assert!(error.contains("already exists"), "{error}");
            assert!(
                error.contains(&format!("control({{job: {existing}, send: ...}})")),
                "{error}"
            );
        };

        // An admitted tool job that never installs a child must not manufacture
        // a permanent name reservation, even though it has role Agent and a name.
        let invalid = call(
            &session.root,
            "return await tool.agent({prompt:'bad-depth',name:'worker',depth:999});",
        )
        .await;
        assert_eq!(invalid["state"], "failed");
        assert!(invalid["error"].as_str().unwrap().contains("depth"));
        assert!(runtime.jobs.snapshot(id(&invalid)).await.is_ok());
        let first = call(&session.root, &launch("worker")).await;
        assert_ne!(first["state"], "failed", "{first}");
        let first = id(&first);
        tracking.request(0).await;
        let duplicate = "return await tool.agent({prompt:'duplicate',name:'worker'});";
        refused(&call(&session.root, duplicate).await, first);

        // A second caller may install its own worker while the root's worker is
        // still active. Resolve its identity from the actual installation record.
        let manager = id(&call(
            &session.root,
            "return await tool.agent({prompt:'test:manager',name:'manager',depth:1,bg:true});",
        )
        .await);
        tracking.request(1).await;
        let records = runtime.store.records();
        let manager = records
            .iter()
            .find_map(|record| match &record.event {
                SessionEvent::AgentStarted {
                    owner_job: Some(job),
                    ..
                } if *job == manager => Some(record.agent.clone()),
                _ => None,
            })
            .unwrap();
        let second = call(&manager, &launch("worker")).await;
        assert_ne!(second["state"], "failed", "{second}");
        let second = id(&second);
        assert_ne!(first, second);
        tracking.request(2).await;
        let completed = bounded(runtime.jobs.wait(second, true)).await.unwrap();
        assert_eq!(completed.state, crate::job::JobState::Completed);
        refused(&call(&manager, duplicate).await, second);

        tracking.release(0);
        let completed = bounded(runtime.jobs.wait(first, true)).await.unwrap();
        assert_eq!(completed.state, crate::job::JobState::Completed);
        // A parked child keeps its name.
        until_parked(&session, &runtime.jobs.child_agent(first).await.unwrap()).await;
        refused(&call(&session.root, duplicate).await, first);
        // Rejecting a duplicate does not interfere with the intended follow-up
        // path: the original child resumes under its existing ID and history.
        bounded(runtime.jobs.send(first, json!("test:resume")))
            .await
            .unwrap();
        let resumed = tracking.request(3).await;
        assert!(
            parent_inputs(resumed.messages())
                .iter()
                .any(|text| text.contains("test:resume"))
        );
        let completed = bounded(runtime.jobs.wait(first, true)).await.unwrap();
        assert_eq!(completed.output, Some(json!({"reply": "answer-3"})));
        drop(root_inbox);
        stop(&session).await;
        assert_eq!(count(&tracking), 4);
    }
}
