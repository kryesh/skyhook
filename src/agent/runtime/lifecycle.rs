//! Session initialization and root-agent lifecycle.

use super::*;

impl SessionRuntime {
    pub(super) async fn build(
        harness: Arc<HarnessInner>,
        store: SessionStore,
        jobs: JobManager,
        prior_records: &[EventRecord],
    ) -> Result<Arc<Self>, HarnessError> {
        // A session never holds more than it started with: its journaled ceiling
        // narrows the live one for as long as it is open.
        let started = prior_records.iter().find_map(|record| match &record.event {
            SessionEvent::SessionStarted { capabilities, .. } => {
                Some(&harness.capabilities & &capabilities.iter().copied().collect())
            }
            _ => None,
        });
        // A new session journals its start alongside its root.
        let new = || harness.capabilities.clone();
        let capabilities = (started.or_else(|| prior_records.is_empty().then(new)))
            .ok_or(HarnessError::MissingSessionStart)?;
        let mut modes = harness.modes.clone();
        let pinned = crate::session::pinned_modes(prior_records);
        modes.extend(pinned.map(|(name, mode)| (name.clone(), mode.clone())));
        let authorization =
            crate::tool::authorization::AuthorizationCoordinator::new(harness.policy.clone())
                .with_targets(store.clone());
        let remote = RemoteManager::new(
            harness.shim_catalog.clone(),
            harness.sensitive_prompts.clone(),
            store.root().child_token(),
        );
        let router = crate::target::TargetRouter::new(store.clone(), remote, authorization.clone());
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
        tools::register(&mut builder, runtime_slot.clone(), &harness.models, &modes)?;
        builder.extend(&harness.extra_tools)?;
        let (mcp, startup_warnings) =
            tools::connect_mcp(&mut builder, &harness, &capabilities, &store).await;
        let executor = ToolExecutor::with_authorization(
            builder.build(),
            authorization,
            jobs.clone(),
            harness.workspace.clone(),
        )
        .with_target_router(router.clone());
        // Both slots are new, so neither set can find a value.
        let _ = executor_slot.set(executor.clone());
        let questions = Arc::new(questions::QuestionCoordinator::new(
            jobs.clone(),
            harness.questions.clone(),
        ));
        let runtime = Arc::new(Self {
            instance: RuntimeInstance::next(),
            harness,
            capabilities,
            modes,
            store: store.clone(),
            jobs: jobs.clone(),
            executor,
            _executor_slot: executor_slot,
            mcp,
            startup_warnings,
            router,
            questions,
        });
        let _ = runtime_slot.set(Arc::downgrade(&runtime));
        let retained = runtime.clone();
        let install = move |state: &mut CoreState| {
            let children = retained.jobs.retained_children(state);
            retained.install_retained(state, children);
        };
        runtime.store.live(install).await;
        Ok(runtime)
    }

    /// Close what a stopped process left open, in one transaction, before any agent
    /// resumes: jobs still running, attempts without an outcome, requests still
    /// waiting for an attempt (their first, or the retry after a failure), and
    /// committed calls without a result, each answered as interrupted, which
    /// reads the outcome of its job if that was cut short. Every turn it leaves
    /// open ends interrupted. Nothing holds a job across a restart, so a retained
    /// child, and every other outcome nobody read, whoever called the job, go on
    /// in the background.
    /// What this closes is dated when the stopped process last did something.
    /// The same commit `resume`s the root, once it has settled them.
    pub(super) async fn activate(
        &self,
        resume: impl FnOnce(&mut CoreState) + Send + 'static,
    ) -> Result<(), HarnessError> {
        let (mut events, outputs) = self.jobs.interrupt_running().await?;
        let activate = move |state: &crate::session::CoreState| {
            let ledger = &state.ledger;
            for (request, record) in ledger.iter() {
                if let crate::session::RequestPhase::Open { attempt, .. } = record.phase {
                    let attempt = crate::session::AttemptRef { request, attempt };
                    let interrupted = SessionEvent::ModelAttemptInterrupted(attempt);
                    events.push((record.agent.clone(), interrupted));
                }
            }
            let error = "interrupted while the session was not running";
            for (agent, call_id, name) in ledger.unanswered() {
                let (call_id, name) = (call_id.to_owned(), name.to_owned());
                let result = dispatch::unrun_tool_result(call_id, name, error.into());
                let message = Message::Tool(vec![result]);
                events.push((agent.clone(), SessionEvent::MessageCommitted { message }));
            }
            let open = ledger.open_turns().into_iter().cloned();
            events.extend(open.map(|agent| (agent, SessionEvent::AgentInterrupted)));
            events.extend(state.jobs.reopened());
            // A reopened session waits for `continue` or a prompt, as an interrupted
            // one does: what it stopped, and the deliveries it closed with.
            let paused = move |state: &mut CoreState| {
                state.agents.flow = Flow::Paused;
                resume(state);
            };
            Ok::<_, SessionError>(Plan::journal(events).saving(outputs).then(paused))
        };
        Ok(self.store.commit(Command::Activate, activate).await?)
    }

    /// A new session's root: the default model, in the configured mode.
    pub(super) fn new_root(&self) -> Result<AgentLaunch, HarnessError> {
        let mode = self.harness.mode.clone();
        let capabilities = match &mode {
            Some(mode) => self.mode_capabilities(mode)?,
            None => self.capabilities.clone(),
        };
        Ok(AgentLaunch::New {
            model: self.harness.default_model.clone(),
            todos: None,
            available_depth: self.harness.max_child_depth,
            location: crate::execution::ExecutionLocation::root(self.harness.workspace.clone()),
            mode,
            capabilities,
        })
    }

    pub(super) async fn start_root(
        self: &Arc<Self>,
        launch: AgentLaunch,
        open: Arc<Open>,
    ) -> Result<SessionHandle, HarnessError> {
        let root = AgentId::root(self.store.id());
        self.spawn_agent(root.clone(), None, launch).await?;
        Ok(SessionHandle {
            runtime: self.clone(),
            root,
            _open: open,
        })
    }
}

impl SessionRuntime {
    /// Interrupt current turns without cancelling their owner jobs, in one step,
    /// and pause the session if that stopped anything. Each agent's part depends
    /// only on its own turn and work. A busy turn holding foreground child
    /// invocations is held on them, and any other is stopped. A child invocation
    /// yet to begin is stopped before it does: the input that begins it runs as
    /// an interrupted turn. An agent between turns gives up the input it had yet
    /// to take, as a stopped turn gives up what was queued for it, and leaves its
    /// deliveries pending. Each of those counts once.
    ///
    /// Child agents are spared and restarted by `continue`; a holder keeps
    /// waiting for them, shown interrupted, until input redirects it (`redirect`).
    /// Foreground non-agent jobs have no resume point and are cancelled: the tool
    /// drain never observes the agent token, and job tokens descend from parent
    /// jobs, so the turn cannot unwind otherwise. Cancelling a script cancels what
    /// it launched.
    pub(super) fn stop_turns(&self, state: &mut CoreState, root: &AgentId) -> usize {
        let (mut stopped, mut blocking) = (Vec::new(), Vec::new());
        let mut count = 0;
        for agent in state.agents.within(root) {
            count += state.jobs.preempt(&agent);
            let work = state.jobs.live_work(&agent);
            let takes = driver::takes_deliveries(state, &agent);
            let facts = state.agents.get_mut(&agent).expect("listed agents run");
            match facts.turn {
                Turn::Busy if work.children => drop(facts.set_turn(Turn::Held)),
                Turn::Busy => {
                    facts.set_turn(Turn::Parked(TurnFailure::Interrupted));
                    // Input behind it has no turn left to join or follow; what
                    // arrives from here on is new input.
                    state.agents.hand_back(&agent, &state.jobs);
                    // Order is load-bearing: cancel the turn before its jobs. A job
                    // cancelled first would let the drain finish and reach its
                    // request boundary before the turn knows it was stopped.
                    let (supervisor, stopped) = (self.jobs.supervisor().clone(), agent.clone());
                    state.defer(move || supervisor.cancel_turn(&stopped));
                }
                Turn::Held => continue,
                Turn::Idle | Turn::Parked(_) => {
                    let waits = takes && facts.turn == Turn::Idle;
                    // What begins an invocation yet to begin stays: its owner counted it.
                    let rejected = state.agents.hand_back(&agent, &state.jobs);
                    count += usize::from(rejected || waits);
                    continue;
                }
            }
            // Blocking work is cancelled either way, each committing an error result
            // that `continue` resumes from.
            blocking.extend(work.blocking);
            stopped.push(agent);
        }
        (self.jobs).cancel_trees(state, blocking, CancelScope::Outcome);
        count += stopped.len();
        if count > 0 {
            state.agents.flow = Flow::Paused;
        }
        count
    }

    /// Wait for the jobs of agents an interrupt stopped to publish their outcome:
    /// an interrupt cancels model futures before their owning job has finished
    /// journaling. Each such turn's token is cancelled and nothing admits it
    /// further, so it always ends. A held turn is not among them: it unwinds
    /// only once `continue` or new input moves it on.
    pub(super) async fn settle_interrupts(&self) {
        let published = |state: &mut CoreState| {
            let interrupted = (state.agents).turned(&Turn::Parked(TurnFailure::Interrupted));
            state
                .jobs
                .interruptions_published(&interrupted)
                .then_some(())
        };
        let _ = self.store.until(published).await;
    }

    /// Queue `command` for the root, in one commit with the release of every
    /// link new input breaks: the foreground child invocations held turns wait
    /// on, at every depth, and the root's suspended ones. They go on in the
    /// background and the waits return. A held descendant has no input to join,
    /// so its turn ends interrupted. A released link ends as the interrupt left
    /// it: one whose child it held, or stopped before it began, ends interrupted
    /// in the same commit, so its call reads so. Queued input joins the root's
    /// turn at its next request; a direct prompt ends a turn that held what was
    /// released, to start the next. Refused as the session refuses input.
    pub(super) async fn redirect(
        self: &Arc<Self>,
        root: &AgentId,
        command: AgentCommand,
    ) -> Result<(), HarnessError> {
        self.settle_interrupts().await;
        let (root, jobs) = (root.clone(), self.jobs.clone());
        let runtime = self.clone();
        let queue = move |state: &CoreState| {
            let admitted = state.phase.admits(Command::Submit);
            let Some(facts) = state.agents.get(&root).filter(|_| admitted.is_ok()) else {
                let refusal = || refused_input(&state.phase);
                command.refuse(refusal);
                return Err(refusal());
            };
            let held = state.agents.turned(&Turn::Held);
            let links = state.jobs.releases(&root, &held);
            let joins = matches!(command, AgentCommand::QueuedInputs(_));
            let ends =
                !joins && facts.turn == Turn::Busy && links.iter().any(|(_, owner)| owner == &root);
            let released = crate::job::release_records(&links);
            Ok(Plan::new(released, move |state, _| {
                state.agents.flow = Flow::Open;
                let mut stopped = Vec::new();
                for agent in held.into_iter().chain(ends.then(|| root.clone())) {
                    let facts = state.agents.get_mut(&agent).expect("agents stay");
                    if agent == root && joins {
                        facts.set_turn(Turn::Busy);
                        continue;
                    }
                    // Cancelled before its wait returns: the released wait must not
                    // reach its request boundary as a running turn.
                    facts.set_turn(Turn::Parked(TurnFailure::Interrupted));
                    let (supervisor, cancelled) = (jobs.supervisor().clone(), agent.clone());
                    state.defer(move || supervisor.cancel_turn(&cancelled));
                    stopped.push(agent);
                }
                // Each link the interrupt stopped ends interrupted now, so its
                // released wait returns that, and `continue` resumes it.
                let unaccepted = jobs.conclude(state, crate::job::Concluding::Released(&links));
                runtime.install_retained(state, unaccepted);
                state.agents.send(&root, command);
            }))
        };
        self.store.commit(Command::Journal, queue).await
    }

    pub(super) async fn interrupt_tree(&self, root: &AgentId) -> usize {
        let (root, jobs) = (root.clone(), self.jobs.clone());
        let interrupt = move |state: &mut CoreState| {
            let agents = state.agents.within(&root);
            let (supervisor, turns) = (jobs.supervisor().clone(), agents.clone());
            state.defer(move || turns.iter().for_each(|agent| supervisor.cancel_turn(agent)));
            jobs.cancel_owned(state, &agents, CancelScope::Outcome)
        };
        self.store.live(interrupt).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::runtime::tests::*;
    use crate::job::JobState;

    /// Results omit an absent field rather than sending null, so no result schema
    /// has a nullable property: a required one is an `Option` serialized as null,
    /// and an optional one a skipped `Option` whose schema is not declared, or an
    /// `Option<Option<T>>`.
    #[tokio::test]
    async fn result_schemas_never_declare_a_nullable_property() {
        use serde_json::Value;
        fn nullable(schema: &Value) -> bool {
            let null = Value::from("null");
            schema["type"] == null
                || schema["type"]
                    .as_array()
                    .is_some_and(|types| types.contains(&null))
                || schema["enum"]
                    .as_array()
                    .is_some_and(|values| values.contains(&Value::Null))
                || ["anyOf", "oneOf"].iter().any(|key| {
                    (schema[*key].as_array()).is_some_and(|variants| variants.iter().any(nullable))
                })
        }
        fn violations(schema: &Value, at: String, found: &mut Vec<String>) {
            match schema {
                Value::Object(object) => {
                    let properties = object.get("properties").and_then(Value::as_object);
                    for (name, _) in properties
                        .into_iter()
                        .flatten()
                        .filter(|(_, property)| nullable(property))
                    {
                        found.push(format!("{at}/properties/{name}"));
                    }
                    for (key, child) in object {
                        violations(child, format!("{at}/{key}"), found);
                    }
                }
                Value::Array(items) => {
                    for (index, child) in items.iter().enumerate() {
                        violations(child, format!("{at}/{index}"), found);
                    }
                }
                _ => {}
            }
        }
        // `skill` is offered only when the host has skills.
        let root = tempfile::tempdir().unwrap();
        let skill = root.path().join(".agents/skills/demo");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), "Demo instructions.").unwrap();
        let provider = scripted_provider(&Requests::default(), []);
        let harness = test_harness(root.path(), &root.path().join("sessions"), provider).await;
        let session = ephemeral_session(&harness).await;
        let capabilities = crate::tool::policy::Capability::ALL.into_iter().collect();
        let registry = session.runtime.executor.registry();
        let surface = registry.surface_for_agent(&capabilities, &session.root);
        let views = &crate::job::JOB_VIEW_SCHEMAS;
        let mut found = Vec::new();
        let mut checked = Vec::new();
        for (tool, schema) in surface
            .result_schemas()
            .chain([("JobView", &views.one), ("JobView[]", &views.many)])
        {
            checked.push(tool);
            violations(schema, tool.to_owned(), &mut found);
        }
        assert!(checked.contains(&"skill"), "{checked:?}");
        assert!(found.is_empty(), "nullable properties: {found:#?}");
    }

    /// Root → child → grandchild, interrupted while the grandchild streams beside
    /// the child's blocking `exec`: the exec is cancelled, the grandchild retained,
    /// and the child's and root's turns stay held on it. `after` scripts the
    /// requests that follow the interrupted three.
    async fn held_chain(
        after: impl IntoIterator<Item = Step>,
    ) -> (
        tempfile::TempDir,
        Harness,
        Arc<Script>,
        SessionHandle,
        tokio::task::JoinHandle<Result<String, HarnessError>>,
    ) {
        let delegate = |id: &str, depth: u32| {
            let arguments = json!({"prompt": format!("{id} task"), "depth": depth});
            tool_call(0, id, "agent", arguments)
        };
        let sleep = tool_call(1, "sleep", "exec", json!({"command": "sleep 30"}));
        let steps = [
            Step::new(response(vec![delegate("child", 1)])),
            Step::new(response(vec![delegate("grandchild", 0), sleep])),
            Step::new(Vec::new()).midstream(),
        ];
        let requests = Requests::default();
        let provider = Script::new(steps.into_iter().chain(after), &requests);
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let harness = test_harness(root.path(), &sessions, provider.clone()).await;
        let session = harness.new_session().await.unwrap();
        let parent = spawn_prompt(&session, "delegate");
        provider.held(2).await;
        let child = session.root.child(1);
        let running =
            |job: &crate::job::JobEnvelope| job.tool == "exec" && job.state == JobState::Running;
        let exec = job_found(&session, &child, running).await;
        assert_eq!(session.interrupt().await, 3);
        assert_eq!(turn(&session, &child), Turn::Held);
        assert_eq!(turn(&session, &session.root), Turn::Held);
        until(&session, exec, |job| job.state == JobState::Cancelled).await;
        (root, harness, provider, session, parent)
    }

    /// A failed journal stops the session as a crash does. The call that met the
    /// failure, the call it cut short and every later call report it; no agent
    /// loop runs, every job is stopped, nothing more is journaled, and observers
    /// read the session stopped. Shutdown and closing report it and free the
    /// session, and reopening settles it as after a crash: the held child is
    /// released, interrupted and resumable.
    #[tokio::test]
    async fn a_journal_failure_stops_the_session_as_a_crash_does() {
        let recovered = (0..8).map(|_| Step::new(answer("recovered")));
        let (_root, harness, _provider, session, parent) = held_chain(recovered).await;
        let (id, store, jobs) = (session.id(), &session.runtime.store, &session.runtime.jobs);
        let launch = json!({"command": "sleep 30", "bg": true});
        let executor = &session.runtime.executor;
        let exec = executor.execute(session.root.clone(), "exec", launch, None);
        let exec = exec.await.unwrap().job;
        until(&session, exec, |job| job.state == JobState::Running).await;
        // Nothing commits once the next commit fails.
        let _refused = crate::session::tests::refuse(store, "BEFORE INSERT ON entry");
        let records = store.records();
        let failed = |result: Result<String, HarnessError>| {
            matches!(
                result,
                Err(HarnessError::Session(SessionError::JournalFailed(_)))
            )
        };
        // The redirect's release meets the failure, which its receipt reports.
        let queued = QueuedPrompt {
            text: "new direction".into(),
            ..QueuedPrompt::default()
        };
        let receipts = bounded(session.enqueue_prompts(vec![queued])).await;
        for receipt in receipts {
            assert!(failed(
                bounded(receipt.into_future()).await.map(|()| String::new())
            ));
        }
        assert!(failed(bounded(session.prompt("new direction")).await));
        assert!(failed(bounded(parent).await.unwrap()));
        assert!(failed(session.continue_turn().await));
        assert!(session.runtime.agents(|agents| agents.is_empty()));
        assert!(jobs.cancellation_token(exec).unwrap().is_cancelled());
        assert_eq!(store.records(), records);
        let viewed = session.observe().snapshot;
        assert!(viewed.phase.failure().is_some());
        let interrupted = crate::agent::AgentActivity::Stopped(TurnFailure::Interrupted);
        assert_eq!(viewed.activity[&session.root].state, interrupted);
        assert_eq!(viewed.jobs[&exec].state, JobState::Interrupted);
        // Remote startups stop with the rest of the session's work.
        let remote = session.runtime.router.environment().await;
        assert!(matches!(remote, Err(crate::remote::RemoteError::Cancelled)));
        assert!(failed(
            bounded(session.shutdown()).await.map(|()| String::new())
        ));
        assert!(failed(session.close().await.map(|()| String::new())));
        let resumed = harness.resume_session(id).await.unwrap();
        resumed.running().await.unwrap();
        let records = resumed.runtime.store.records();
        let released = events!(&records, SessionEvent::JobReleased { job } => *job);
        let held = resumed.runtime.jobs.list(&resumed.root);
        let held = held.into_iter().find(|job| job.tool == "agent").unwrap();
        assert!(released.contains(&held.id));
        assert_eq!(held.state, JobState::Interrupted);
        bounded(resumed.continue_turn()).await.unwrap();
        assert_eq!(terminal(&resumed, held.id).await.state, JobState::Completed);
        shutdown_session(resumed).await;
    }

    /// Continuing a held chain restarts the grandchild, and the whole tree answers
    /// through the preserved waits.
    #[tokio::test]
    async fn continue_resumes_a_turn_held_on_a_retained_child_beside_cancelled_work() {
        let (_root, _harness, provider, session, parent) = held_chain([
            Step::new(answer("grandchild recovered")),
            Step::new(answer("child done")),
            Step::new(answer("parent done")),
        ])
        .await;
        assert_eq!(bounded(session.continue_turn()).await.unwrap(), "");
        assert_eq!(bounded(parent).await.unwrap().unwrap(), "parent done");
        let resumed = rendered(&provider.request(4).await);
        assert!(resumed.contains("grandchild recovered"), "{resumed}");
        session.shutdown().await.unwrap();
    }

    /// Closing leaves every link of a held chain interrupted, as it leaves any
    /// interruption: reopened, `continue` restarts the child and grandchild.
    #[tokio::test]
    async fn shutdown_leaves_a_held_chain_resumable() {
        let recovered = (0..8).map(|_| Step::new(answer("recovered")));
        let (_root, harness, _provider, session, parent) = held_chain(recovered).await;
        let id = session.id();
        shutdown_session(session).await;
        assert!(bounded(parent).await.unwrap().is_err());
        let resumed = harness.resume_session(id).await.unwrap();
        let child = resumed.root.child(1);
        let delegated = async |owner: &AgentId| {
            let jobs = resumed.runtime.jobs.list(owner);
            jobs.into_iter().find(|job| job.tool == "agent").unwrap()
        };
        for owner in [&resumed.root, &child] {
            let state = delegated(owner).await.state;
            assert_eq!(state, JobState::Interrupted, "{owner}");
        }
        bounded(resumed.continue_turn()).await.unwrap();
        for owner in [&resumed.root, &child] {
            let job = delegated(owner).await.id;
            let state = terminal(&resumed, job).await.state;
            assert_eq!(state, JobState::Completed, "{owner}");
        }
        shutdown_session(resumed).await;
    }

    /// A new prompt breaks the link at every depth: the held child ends its turn
    /// interrupted rather than asking its model, so each link is retained
    /// interrupted and the prompt answers.
    #[tokio::test]
    async fn prompt_redirects_a_chain_held_on_a_retained_grandchild() {
        let (_root, _harness, provider, session, parent) =
            held_chain([Step::new(answer("redirected"))]).await;
        assert_eq!(
            bounded(session.prompt("new direction")).await.unwrap(),
            "redirected"
        );
        assert!(bounded(parent).await.unwrap().is_err());
        let child = session.root.child(1);
        for owner in [&session.root, &child] {
            let jobs = session.runtime.jobs.list(owner);
            let agent = jobs.iter().find(|job| job.tool == "agent").unwrap();
            until(&session, agent.id, |job| job.state == JobState::Interrupted).await;
        }
        assert_eq!(turn(&session, &child), INTERRUPTED);
        let records = session.runtime.store.records();
        let released = records.iter().filter(|record| record.agent == child);
        let released = count!(released, SessionEvent::MessageCommitted { message: Message::Tool(results) }
            if results.iter().any(|result| result.call_id == "grandchild"));
        assert_eq!(released, 1);
        let redirected = rendered(&provider.request(3).await);
        assert!(
            redirected.contains("interrupted") && redirected.contains("new direction"),
            "{redirected}"
        );
        session.shutdown().await.unwrap();
    }

    /// A child launched by a foreground script dies with it, so the interrupt
    /// holds nothing: the root's turn ends interrupted instead of taking the
    /// script's cancellation to its model.
    #[tokio::test]
    async fn interrupt_ends_a_turn_whose_child_dies_with_its_script() {
        let source = json!({"source": "return await tool.agent({prompt: 'work', depth: 0});"});
        let steps = [
            Step::new(response(vec![tool_call(0, "script", "script", source)])),
            Step::new(Vec::new()).midstream(),
            Step::new(answer("unprompted")),
        ];
        let provider = Script::new(steps, &Requests::default());
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let harness = test_harness(root.path(), &sessions, provider.clone()).await;
        let session = harness.new_session().await.unwrap();
        let parent = spawn_prompt(&session, "delegate");
        provider.request(1).await;
        assert_eq!(session.interrupt().await, 2);
        let ended = bounded(parent).await.unwrap();
        assert!(matches!(ended, Err(HarnessError::Interrupted)), "{ended:?}");
        assert!(!provider.requested_from(2));
        session.shutdown().await.unwrap();
    }

    /// A holder whose held child is cancelled on request runs on, making calls
    /// again, and a second interrupt stops it.
    #[tokio::test]
    async fn cancelling_a_held_child_lets_its_holder_run_on() {
        let script = json!({"source": "return 1;"});
        let after = [
            Step::new(response(vec![tool_call(0, "script", "script", script)])),
            Step::new(Vec::new()).midstream(),
        ];
        let root = tempfile::tempdir().unwrap();
        let (_harness, session, provider, _, parent, job) =
            held_on_interrupted_child(root.path(), after).await;
        session.runtime.jobs.cancel(job).await.unwrap();
        provider.request(3).await;
        let jobs = session.runtime.jobs.list(&session.root);
        assert!(jobs.iter().any(|job| job.tool == "script"), "{jobs:?}");
        assert_eq!(session.interrupt().await, 1);
        let ended = bounded(parent).await.unwrap();
        assert!(matches!(ended, Err(HarnessError::Interrupted)), "{ended:?}");
        session.shutdown().await.unwrap();
    }

    /// A held turn whose wait ends without `continue` or a cancellation parks
    /// interrupted, its results committed: it asks its model only once continued.
    #[tokio::test]
    async fn a_held_turn_whose_wait_ends_parks_interrupted() {
        // A launch its tool refuses, after the interrupt held the root on it.
        let refused = json!({"prompt": "too deep", "depth": 9});
        let steps = [
            Step::new(response(vec![tool_call(0, "child", "agent", refused)])),
            Step::new(answer("root done")),
        ];
        let provider = Script::new(steps, &Requests::default());
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let harness = test_harness(root.path(), &sessions, provider.clone()).await;
        let session = harness.new_session().await.unwrap();
        let store = session.runtime.store.clone();
        let delegated: fn(&SessionEvent) -> bool =
            |event| matches!(event, SessionEvent::JobCreated { .. });
        let (reached, resume) = store.pause_committed(delegated).await;
        let parent = spawn_prompt(&session, "delegate");
        bounded(reached).await.unwrap();
        assert_eq!(interrupt_behind(&session, resume).await, 2);
        let ended = bounded(parent).await.unwrap();
        assert!(matches!(ended, Err(HarnessError::Interrupted)), "{ended:?}");
        assert_eq!(turn(&session, &session.root), INTERRUPTED);
        assert!(!provider.requested_from(1), "the held turn ran on");
        assert_eq!(bounded(session.continue_turn()).await.unwrap(), "root done");
        assert!(rendered(&provider.request(1).await).contains("available depth"));
        session.shutdown().await.unwrap();
    }

    /// Its owner is held on a child that answered while work of its own keeps
    /// its invocation open, though no turn of the child's runs. What that work
    /// then delivers waits with the paused session, and `continue` lets the
    /// child take it, answer and complete through the preserved wait.
    #[tokio::test]
    async fn interrupt_holds_the_owner_of_an_answered_child_still_open() {
        let script = json!({"source": "return await receive();", "bg": true});
        let (_root, requests, session) = scripted_session([
            response(vec![tool_call(
                0,
                "agent",
                "agent",
                json!({"prompt": "work"}),
            )]),
            response(vec![tool_call(0, "script", "script", script)]),
            answer("premature child answer"),
            answer("child done"),
            answer("root done"),
        ])
        .await;
        let child = session.root.child(1);
        let mut events = session.runtime.store.observe().updates;
        let parent = spawn_prompt(&session, "delegate");
        bounded(async {
            while !matches!(events.recv().await.unwrap().event,
                RuntimeEvent::TurnCompleted { agent } if agent == child)
            {}
        })
        .await;
        assert_eq!(session.interrupt().await, 1);
        assert_eq!(turn(&session, &session.root), Turn::Held);
        let jobs = &session.runtime.jobs;
        let work = jobs.list(&child)[0].id;
        jobs.send(work, json!("released")).await.unwrap();
        until_pending(&session, &child).await;
        assert_eq!(turn(&session, &child), Turn::Idle, "it took a delivery");
        assert_eq!(requests.lock().unwrap().len(), 3);
        assert_eq!(bounded(session.continue_turn()).await.unwrap(), "");
        assert_eq!(bounded(parent).await.unwrap().unwrap(), "root done");
        session.shutdown().await.unwrap();
    }

    /// An interrupt that lands before a child begins its invocation, launched or
    /// not, stops it: its first take parks it without a request, its parent is
    /// held and makes no more calls, and `continue` resumes the child from the
    /// prompt its launch committed.
    #[tokio::test]
    async fn interrupt_stops_a_child_that_has_yet_to_begin() {
        type Landing = fn(&SessionEvent) -> bool;
        let delegated: Landing =
            |event| matches!(event, SessionEvent::JobCreated { tool, .. } if tool == "agent");
        let launched: Landing = |event| matches!(event, SessionEvent::AgentStarted { .. });
        let child = json!({"prompt": "child task", "depth": 0});
        let script =
            |position, id| tool_call(position, id, "script", json!({"source": "return 1;"}));
        let alone = || vec![tool_call(0, "child", "agent", child.clone())];
        let beside = || [alone(), vec![script(1, "first"), script(2, "second")]].concat();
        // The calls made before the interrupt lands, which alone exist, and what
        // it stops: an installed child also gives up its owner's input.
        for (calls, landing, created, stopped) in [
            (alone(), delegated, 1, 2),
            (beside(), delegated, 1, 2),
            (alone(), launched, 1, 3),
            (beside(), launched, 3, 3),
        ] {
            let steps = [
                Step::new(response(calls)),
                Step::new(answer("child done")),
                Step::new(answer("root done")),
            ];
            let provider = Script::new(steps, &Requests::default());
            let root = tempfile::tempdir().unwrap();
            let sessions = root.path().join("sessions");
            let harness = test_harness(root.path(), &sessions, provider.clone()).await;
            let session = harness.new_session().await.unwrap();
            let store = session.runtime.store.clone();
            // Past COMMIT the pause holds no connection a running script may need.
            let (reached, resume) = store.pause_committed(landing).await;
            let parent = spawn_prompt(&session, "delegate");
            bounded(reached).await.unwrap();
            // Owner input behind the launch is handed back; the input that begins
            // the child is not.
            let (committed, owner_input) = oneshot::channel();
            let input = queue::QueuedInput {
                content: Vec::new(),
                options: Selection::default(),
                cancellation: Default::default(),
                committed,
            };
            let (child, queued) = (
                session.root.child(1),
                AgentCommand::QueuedInputs(vec![input]),
            );
            store.enqueue(move |state| {
                state.agents.send(&child, queued);
            });
            // Queued behind the landing commit, ahead of the child's first take.
            assert_eq!(interrupt_behind(&session, resume).await, stopped);
            assert!(!matches!(bounded(owner_input).await, Ok(Ok(()))));
            assert_eq!(turn(&session, &session.root), Turn::Held);
            let jobs = session.runtime.jobs.list(&session.root);
            let delegation = jobs.iter().find(|job| job.tool == "agent").unwrap().id;
            until(&session, delegation, |job| {
                job.state == JobState::Interrupted
            })
            .await;
            // A response's calls all exist before any runs.
            let jobs = session.runtime.jobs.list(&session.root);
            assert_eq!(jobs.len(), created, "calls made after the interrupt");
            assert!(!provider.requested_from(1), "the child requested");
            assert_eq!(turn(&session, &session.root.child(1)), INTERRUPTED);
            assert_eq!(bounded(session.continue_turn()).await.unwrap(), "");
            assert_eq!(bounded(parent).await.unwrap().unwrap(), "root done");
            assert!(rendered(&provider.request(1).await).contains("child task"));
            session.shutdown().await.unwrap();
        }
    }

    /// Chains two and three deep, stopped as the last link's launch is created
    /// or installs its child. An interrupt holds every link above it, so a
    /// redirect resolves them all. A shutdown alone, as a crash would, leaves
    /// every delegation interrupted: reopened, `continue` completes those whose
    /// child was installed, the last from the prompt its launch committed, and a
    /// call that installed none, never accepted, asks again and then launches.
    #[tokio::test]
    async fn every_link_above_a_child_yet_to_begin_stops_and_resumes() {
        type Landing = fn(&SessionEvent) -> bool;
        let created: Landing = |event| matches!(event, SessionEvent::JobCreated { arguments, .. } if arguments["prompt"] == "last");
        let installed: Landing = |event| {
            matches!(event, SessionEvent::MessageCommitted { message: Message::User(parts) }
            if matches!(parts.as_slice(), [UserPart::Text { text }] if text == "last"))
        };
        // Whether the last launch installed its child, and what follows.
        let cases = [2, 3].into_iter().flat_map(|depth| {
            [
                (depth, false, true),
                (depth, false, false),
                (depth, true, false),
            ]
        });
        for (depth, accepted, redirect) in cases {
            let landing = if accepted { installed } else { created };
            let delegate = |link: u32| {
                let prompt = match link + 1 == depth {
                    true => "last".to_owned(),
                    false => format!("link {link}"),
                };
                let arguments = json!({"prompt": prompt, "depth": depth - 1 - link});
                Step::new(response(vec![tool_call(0, "delegate", "agent", arguments)]))
            };
            let done = (0..4 * depth + 4).map(|_| Step::new(answer("done")));
            let provider = Script::new((0..depth).map(delegate).chain(done), &Requests::default());
            let root = tempfile::tempdir().unwrap();
            let sessions = root.path().join("sessions");
            let harness = test_harness(root.path(), &sessions, provider.clone()).await;
            let session = harness.new_session().await.unwrap();
            let links: Vec<_> =
                std::iter::successors(Some(session.root.clone()), |link| Some(link.child(1)))
                    .take(depth as usize)
                    .collect();
            let store = session.runtime.store.clone();
            let (reached, resume) = store.pause_committed(landing).await;
            let parent = spawn_prompt(&session, "delegate");
            bounded(reached).await.unwrap();
            if redirect {
                // Queued behind the last delegation, ahead of its launch.
                assert_eq!(interrupt_behind(&session, resume).await, depth as usize + 1);
                for link in &links {
                    assert_eq!(turn(&session, link), Turn::Held, "{link}");
                }
                assert_eq!(bounded(session.prompt("redirect")).await.unwrap(), "done");
                assert!(bounded(parent).await.unwrap().is_err());
                session.shutdown().await.unwrap();
                continue;
            }
            let mut shutdown = Box::pin(session.shutdown());
            assert!(futures_util::poll!(&mut shutdown).is_pending());
            resume.send(()).unwrap();
            bounded(shutdown).await.unwrap();
            assert!(bounded(parent).await.unwrap().is_err());
            let id = session.id();
            session.close().await.unwrap();
            drop(session);
            let resumed = harness.resume_session(id).await.unwrap();
            let delegation = |owner: &AgentId| {
                let jobs = resumed.runtime.jobs.list(owner).into_iter();
                jobs.into_iter().find(|job| job.tool == "agent").unwrap()
            };
            for link in &links {
                let delegated = delegation(link);
                assert_eq!(delegated.state, JobState::Interrupted, "{link}");
                assert!(delegated.resumable, "{link}");
            }
            let served = provider.requests.lock().unwrap().len();
            bounded(resumed.continue_turn()).await.unwrap();
            let (last, above) = links.split_last().unwrap();
            for link in above {
                let state = terminal(&resumed, delegation(link).id).await.state;
                assert_eq!(state, JobState::Completed, "{link}");
            }
            let state = terminal(&resumed, delegation(last).id).await.state;
            assert_eq!(state, JobState::Completed, "{last}");
            let records = resumed.runtime.store.records();
            let started = count!(&records, SessionEvent::AgentStarted { .. });
            assert_eq!(started, depth as usize + 1, "every link launched once");
            let child = ContextId::from(&last.child(1));
            let first = (provider.requests.lock().unwrap()[served..].iter())
                .find(|served| served.context == child)
                .map(|served| rendered(&served.request));
            assert!(first.unwrap().contains("last"));
            shutdown_session(resumed).await;
        }
    }

    /// An interrupt hands back the input an agent had yet to take. Between turns
    /// that is a prompt and a queued batch, sent in the interrupt's own step, as
    /// when the agent's loop has yet to come back for them; behind a turn the
    /// interrupt stops, a prompt that would have followed it.
    #[tokio::test]
    async fn interrupt_hands_back_untaken_input() {
        let steps = [
            Step::new(Vec::new()).midstream(),
            Step::new(answer("unprompted")),
        ];
        let provider = Script::new(steps, &Requests::default());
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let harness = test_harness(root.path(), &sessions, provider.clone()).await;
        let session = harness.new_session().await.unwrap();
        let (runtime, root) = (session.runtime.clone(), session.root.clone());
        let (done, prompted) = oneshot::channel();
        let (committed, queued) = oneshot::channel();
        let interrupt = move |state: &mut CoreState| {
            let prompt = AgentCommand::Input {
                options: Selection::default(),
                content: Vec::new(),
                done: Some(RequestCompletion::Root(done)),
            };
            let batch = queue::QueuedInput {
                content: Vec::new(),
                options: Selection::default(),
                cancellation: Default::default(),
                committed,
            };
            state.agents.send(&root, prompt);
            (state.agents).send(&root, AgentCommand::QueuedInputs(vec![batch]));
            runtime.stop_turns(state, &root)
        };
        assert_eq!(session.runtime.store.live(interrupt).await, 1);
        let interrupted = |result| matches!(result, Ok(Err(HarnessError::Interrupted)));
        assert!(interrupted(
            bounded(prompted).await.map(|answer| answer.map(drop))
        ));
        assert!(interrupted(bounded(queued).await));

        let first = spawn_prompt(&session, "first");
        provider.held(0).await;
        let second = spawn_prompt(&session, "second");
        until_queued(&session, &session.root).await;
        assert_eq!(session.interrupt().await, 1);
        for stopped in [first, second] {
            let ended = bounded(stopped).await.unwrap();
            assert!(matches!(ended, Err(HarnessError::Interrupted)), "{ended:?}");
        }
        assert_eq!(turn(&session, &session.root), INTERRUPTED);
        assert_eq!(inbox(&session, &session.root), 0);
        assert!(!provider.requested_from(1), "the prompt behind it ran");
        session.shutdown().await.unwrap();
    }

    /// A reopened session asks no model for what was delivered as it closed: the
    /// delivery stays pending, the root reads as interrupted, and continuing
    /// carries the delivery in its request.
    #[tokio::test]
    async fn reopening_parks_the_root_on_a_pending_delivery_for_continue() {
        let requests = Requests::default();
        let provider = scripted_provider(&requests, [answer("first"), answer("second")]);
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let harness = test_harness(root.path(), &sessions, provider).await;
        let session = harness.new_session().await.unwrap();
        assert_eq!(session.prompt("hello").await.unwrap(), "first");
        let arguments = json!({"source": "return await receive();", "bg": true});
        let executor = &session.runtime.executor;
        let launched = executor.execute(session.root.clone(), "script", arguments, None);
        let job = launched.await.unwrap().job;
        until(&session, job, |job| job.state == JobState::Running).await;
        let id = session.id();
        shutdown_session(session).await;
        let resumed = harness.resume_session(id).await.unwrap();
        resumed.running().await.unwrap();
        assert!(pending(&resumed, &resumed.root));
        assert_eq!(turn(&resumed, &resumed.root), INTERRUPTED);
        let activity = resumed.observe().snapshot.activity;
        assert!(activity[&resumed.root].state.is_retryable());
        assert_eq!(requests.lock().unwrap().len(), 1);
        assert_eq!(resumed.continue_turn().await.unwrap(), "second");
        let carried = rendered(&requests.lock().unwrap()[1].request);
        assert!(carried.contains(&job.to_string()), "{carried}");
        shutdown_session(resumed).await;
    }

    /// A root whose turn holds an embedder's agent call that is still preparing
    /// its child until the returned gate has a permit; the root's prompt.
    async fn preparing_delegate() -> (
        tempfile::TempDir,
        SessionHandle,
        Arc<tokio::sync::Semaphore>,
        tokio::task::JoinHandle<Result<String, HarnessError>>,
    ) {
        let root = tempfile::tempdir().unwrap();
        let delegate = tool_call(0, "delegate", "delegate", json!({}));
        let steps = [
            response(vec![delegate]),
            answer("redirected"),
            answer("delivered"),
        ];
        let provider = scripted_provider(&Requests::default(), steps);
        let mut tools = ToolRegistryBuilder::default();
        let options = crate::tool::ToolOptions::default()
            .job_role(crate::job::JobRole::Agent)
            .input();
        let schema = json!({"type": "object", "properties": {}});
        let prepared = Arc::new(tokio::sync::Semaphore::new(0));
        let gate = prepared.clone();
        let preparing = move |_, _| {
            let gate = gate.clone();
            async move {
                gate.acquire().await.unwrap().forget();
                Ok(crate::tool::ToolOutput::default())
            }
        };
        let registered =
            tools.register_dynamic("delegate", "Delegate.", schema, options, preparing);
        registered.unwrap();
        let sessions = root.path().join("sessions");
        let builder = test_builder(root.path(), &sessions, provider, false);
        let session = ephemeral_session(&builder.tools(tools.build()).build().await.unwrap()).await;
        let parent = spawn_prompt(&session, "delegate");
        let running = |job: &crate::job::JobEnvelope| {
            job.tool == "delegate" && job.state == JobState::Running
        };
        job_found(&session, &session.root, running).await;
        (root, session, prepared, parent)
    }

    fn delegated(session: &SessionHandle) -> Option<crate::job::JobEnvelope> {
        let jobs = session.inspect_jobs(&session.root).into_iter();
        jobs.into_iter().find(|job| job.tool == "delegate")
    }

    /// A redirect ends a link an interrupt stopped before its child began, here
    /// an embedder's agent call still preparing it, interrupted in the commit
    /// that releases it: its caller reads it so. The call is stopped as a crash
    /// stops it, and `continue` asks for it again, as a reopen would.
    #[tokio::test]
    async fn a_redirect_ends_a_link_stopped_before_its_child_began() {
        let (_root, session, prepared, parent) = preparing_delegate().await;
        // The held root and the call yet to begin.
        assert_eq!(session.interrupt().await, 2);
        // Queued, the redirect joins the root's turn, which runs on.
        let redirect = QueuedPrompt {
            text: "redirect".into(),
            ..Default::default()
        };
        let receipts = bounded(enqueue_prompts(&session, vec![redirect])).await;
        assert!(receipts[0].is_ok());
        assert_eq!(bounded(parent).await.unwrap().unwrap(), "redirected");
        let job = delegated(&session).unwrap();
        assert_eq!(job.state, JobState::Interrupted);
        let continued = bounded(session.continue_turn_with(Selection::default())).await;
        assert_eq!(continued.unwrap().children_resumed, 1);
        // Only the call asked for again still waits for the permit.
        prepared.add_permits(1);
        assert_eq!(terminal(&session, job.id).await.state, JobState::Completed);
        session.shutdown().await.unwrap();
    }

    /// `continue` lets an invocation an interrupt stopped before it began begin,
    /// counting it, so the next interrupt stops it again.
    #[tokio::test]
    async fn continue_lets_a_link_stopped_before_its_child_began_begin() {
        let (_root, session, prepared, parent) = preparing_delegate().await;
        assert_eq!(session.interrupt().await, 2);
        let continued = bounded(session.continue_turn_with(Selection::default())).await;
        assert_eq!(continued.unwrap().children_resumed, 1);
        assert_eq!(session.interrupt().await, 2);
        prepared.add_permits(1);
        assert!(bounded(parent).await.unwrap().is_err());
        session.shutdown().await.unwrap();
    }

    /// A background child that `continue` restarts is stopped by an interrupt
    /// landing before it begins, as it would be once begun, and stays resumable.
    #[tokio::test]
    async fn interrupt_stops_a_restarted_background_child_yet_to_begin() {
        let steps = [
            Step::new(Vec::new()).midstream(),
            Step::new(answer("child done")),
        ];
        let provider = Script::new(steps, &Requests::default());
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        // No turn is scripted for the completion: the held window keeps it.
        let builder = test_builder(root.path(), &sessions, provider.clone(), false);
        let harness = builder.coalescing(wait::Coalescing::held().0).build();
        let harness = harness.await.unwrap();
        let session = harness.new_session().await.unwrap();
        let arguments = json!({"prompt": "child task", "bg": true});
        let executor = &session.runtime.executor;
        let launched = executor.execute(session.root.clone(), "agent", arguments, None);
        let job = launched.await.unwrap().job;
        provider.held(0).await;
        assert_eq!(session.interrupt().await, 1);
        until(&session, job, |job| job.state == JobState::Interrupted).await;
        let restarted: fn(&SessionEvent) -> bool =
            |event| matches!(event, SessionEvent::JobStateChanged { .. });
        let store = session.runtime.store.clone();
        let (reached, resume) = store.pause_committed(restarted).await;
        let continued = tokio::spawn({
            let session = session.clone();
            async move { session.continue_turn().await }
        });
        bounded(reached).await.unwrap();
        // Queued behind the restart, ahead of the child taking its input.
        assert_eq!(interrupt_behind(&session, resume).await, 1);
        bounded(continued).await.unwrap().unwrap();
        until(&session, job, |job| job.state == JobState::Interrupted).await;
        assert!(!provider.requested_from(1), "the child requested");
        bounded(session.continue_turn()).await.unwrap();
        assert_eq!(terminal(&session, job).await.state, JobState::Completed);
        session.shutdown().await.unwrap();
    }
}
