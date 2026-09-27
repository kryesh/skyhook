//! Session initialization and root-agent lifecycle.

use super::*;

impl SessionRuntime {
    pub(super) async fn build(
        harness: Arc<HarnessInner>,
        store: SessionStore,
        prior_records: &[EventRecord],
    ) -> Result<Arc<Self>, HarnessError> {
        // A session never holds more than it started with: its journaled ceiling
        // narrows the live one for as long as it is open.
        let started = prior_records.iter().find_map(|record| match &record.event {
            SessionEvent::SessionStarted {
                capabilities,
                targets,
            } => {
                let capabilities = capabilities.iter().copied().collect();
                Some((&harness.capabilities & &capabilities, targets.clone()))
            }
            _ => None,
        });
        // A new session journals its start, with these targets, alongside its root.
        let new = || {
            (
                harness.capabilities.clone(),
                harness.target_definitions.clone(),
            )
        };
        let (capabilities, definitions) = (started.or_else(|| prior_records.is_empty().then(new)))
            .ok_or(HarnessError::MissingSessionStart)?;
        let mut modes = harness.modes.clone();
        let pinned = crate::session::pinned_modes(prior_records);
        modes.extend(pinned.map(|(name, mode)| (name.clone(), mode.clone())));
        let jobs = JobManager::restore(store.clone(), prior_records).await?;
        let targets = TargetRegistry::from_definitions(definitions)?;
        for record in prior_records {
            if let SessionEvent::TargetsUpserted { targets: restored } = &record.event {
                targets.upsert_many(restored.clone()).await?;
            }
        }
        let authorization =
            crate::tool::authorization::AuthorizationCoordinator::new(harness.policy.clone())
                .journaled(store.clone(), prior_records)
                .await;
        let remote = RemoteManager::new(
            harness.shim_catalog.clone(),
            harness.sensitive_prompts.clone(),
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
        let caught_up_sequence = Mutex::new(
            prior_records
                .last()
                .map_or(RecordSeq::default(), |record| record.sequence),
        );
        let events = RuntimeEvents::new(prior_records);
        let mut child_counters = HashMap::new();
        for record in prior_records {
            if let SessionEvent::AgentStarted { .. } = &record.event
                && let Some(parent) = record.agent.parent()
                && let Some(segment) = record.agent.path().last()
            {
                let counter = child_counters.entry(parent).or_insert(0_u32);
                *counter = (*counter).max(*segment);
            }
        }
        let questions = Arc::new(questions::QuestionCoordinator::new(
            jobs.clone(),
            harness.questions.clone(),
        ));
        let runtime = Arc::new(Self {
            instance: RuntimeInstance::next(),
            shutting_down: std::sync::atomic::AtomicBool::new(false),
            todos: TodoStore::restore(store.clone(), prior_records),
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
            agents: StdRwLock::new(HashMap::new()),
            child_counters: RwLock::new(child_counters),
            questions,
            events,
            caught_up_sequence,
            #[cfg(test)]
            forwarding_gate: Arc::new(Mutex::new(())),
        });
        let _ = runtime_slot.set(Arc::downgrade(&runtime));
        runtime.install_retained_children(prior_records).await;
        runtime.forward_store_events();
        runtime.forward_job_completions();
        Ok(runtime)
    }

    /// Why the agent's journaled turn stopped short of its answer, so `continue` has
    /// something to resume: its turn failed, or it was cut short while its
    /// conversation still awaited the model or its latest request of any purpose
    /// had not completed.
    pub(super) fn stopped_turn(
        &self,
        records: &[EventRecord],
        agent: &AgentId,
    ) -> Option<TurnFailure> {
        let mut stopped = None;
        for record in records.iter().filter(|record| &record.agent == agent) {
            match &record.event {
                SessionEvent::MessageCommitted { message } => {
                    let awaiting = match message {
                        Message::Assistant(items) => items.iter().any(|item| item.call().is_some()),
                        Message::User(_) | Message::Tool(_) => true,
                    };
                    stopped = awaiting.then_some(TurnFailure::Interrupted);
                }
                SessionEvent::AgentFailed { failure } => {
                    stopped = Some(TurnFailure::Failed(failure.clone()));
                }
                _ => {}
            }
        }
        stopped.or_else(|| {
            self.events.ledger(|ledger| {
                let latest = ledger
                    .latest(agent)
                    .and_then(|request| ledger.get(request))?;
                let completed =
                    matches!(latest.phase, crate::session::RequestPhase::Completed { .. });
                (!completed).then_some(TurnFailure::Interrupted)
            })
        })
    }

    /// Close what a stopped process left open, in one transaction, before any agent
    /// resumes: attempts without an outcome, requests still waiting for an attempt
    /// (their first, or the retry after a failure), and committed calls without a
    /// result. Their agents' turns end interrupted. A settled call to a retained
    /// child releases it: nothing waits on it any more.
    pub(super) async fn settle_interrupted_work(
        &self,
        records: &[EventRecord],
    ) -> Result<(), HarnessError> {
        let mut calls = indexmap::IndexMap::new();
        for record in records {
            match &record.event {
                SessionEvent::MessageCommitted {
                    message: Message::Assistant(items),
                } => {
                    let made = items.iter().filter_map(|item| item.call());
                    calls.extend(made.map(|call| {
                        let key = (record.agent.clone(), call.id().to_owned());
                        (key, call.name().to_owned())
                    }));
                }
                SessionEvent::MessageCommitted {
                    message: Message::Tool(results),
                } => {
                    for result in results {
                        calls.shift_remove(&(record.agent.clone(), result.call_id.clone()));
                    }
                }
                _ => {}
            }
        }
        let (mut events, mut agents) = (Vec::new(), Vec::new());
        self.events.ledger(|ledger| {
            for (request, record) in ledger
                .iter()
                .filter(|(_, record)| record.phase.settled_at().is_none())
            {
                if let crate::session::RequestPhase::Open { attempt, .. } = record.phase {
                    let attempt = crate::session::AttemptRef { request, attempt };
                    let interrupted = SessionEvent::ModelAttemptInterrupted(attempt);
                    events.push((record.agent.clone(), interrupted));
                }
                agents.push(record.agent.clone());
            }
        });
        for ((agent, call_id), name) in calls {
            let error = "interrupted while the session was not running".to_owned();
            let result = dispatch::unrun_tool_result(call_id, name, error);
            let message = Message::Tool(vec![result]);
            events.push((agent.clone(), SessionEvent::MessageCommitted { message }));
            agents.push(agent);
        }
        if agents.is_empty() {
            return Ok(());
        }
        agents.sort();
        agents.dedup();
        let interrupted = agents.iter().cloned();
        events.extend(interrupted.map(|agent| (agent, SessionEvent::AgentInterrupted)));
        self.store.append_all(events).await?;
        for agent in &agents {
            self.jobs.release_held(agent).await;
        }
        Ok(())
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
    ) -> Result<SessionHandle, HarnessError> {
        let root = AgentId::root(self.store.id());
        let root_tx = self.spawn_agent(root.clone(), None, launch).await?;
        Ok(SessionHandle {
            runtime: self.clone(),
            root,
            root_tx,
        })
    }
}

/// The turn states an interrupt stops.
const RUNNING: &[TurnState] = &[TurnState::Busy, TurnState::Held];

impl SessionRuntime {
    /// Interrupt current turns without cancelling their owner jobs. This is the
    /// retryable session-interrupt path; explicit job/tree cancellation remains in
    /// `interrupt_tree` below.
    ///
    /// Retained child agents are spared and restarted by `continue`; a parent
    /// holding one keeps waiting for it, shown interrupted, until input redirects
    /// it (`redirect`). Foreground non-agent jobs have no resume point and are
    /// cancelled: the tool drain never observes the agent token, and job tokens
    /// descend from parent jobs, so the turn cannot unwind otherwise. Cancelling a
    /// script cancels what it launched.
    pub(super) async fn interrupt_turns(&self, root: &AgentId) -> usize {
        let mut targets = self
            .agents()
            .keys()
            .filter(|agent| agent.is_within(root))
            .cloned()
            .collect::<Vec<_>>();
        targets.sort_by_key(AgentId::depth);
        let (mut holders, mut cancelled) = (Vec::new(), Vec::new());
        for agent in &targets {
            let control = self.agents().get(agent).map(|live| live.control.clone());
            // A held turn whose wait ended without `continue` runs on, so it is
            // judged by its live work like a busy one.
            let running = |control: &AgentControl| RUNNING.contains(&control.turn());
            let Some(control) = control.filter(running) else {
                continue;
            };
            let work = self.jobs.live_work(agent).await;
            // Preserve a genuine wait on retained children, which `continue`
            // restarts, and leave the background work beside them running.
            if work.children && work.blocking.is_empty() {
                if work.holding.is_some() {
                    holders.push((agent.clone(), control));
                }
                continue;
            }
            // Read the turn's token only now: the agent may have begun a new turn
            // while the awaits above ran, and a stale token would leave that turn
            // live while its jobs are cancelled below.
            let Some(cancellation) = self
                .agents()
                .get(agent)
                .map(|live| live.cancellation.clone())
            else {
                continue;
            };
            // A turn that ended while the awaits above ran has nothing to interrupt.
            if !self.stop(agent, &control, RUNNING, TurnState::Parked) {
                continue;
            }
            // Order is load-bearing: mark and cancel the turn before its jobs. A job
            // cancelled first would let the drain finish and reach an uncancelled
            // request boundary, starting a fresh model request.
            control.retryable_interrupt.store(true, Ordering::Release);
            cancellation.cancel();
            for job in work.blocking {
                // Commits an error result, which `continue` then resumes from.
                let _ = self.jobs.cancel(job).await;
            }
            cancelled.push(agent.clone());
        }
        // A held child is retained too; its holder shows interrupted meanwhile.
        let mut interrupted = cancelled.len();
        for (holder, control) in holders {
            let descendant_cancelled = cancelled
                .iter()
                .any(|agent| agent != &holder && agent.is_within(&holder));
            if descendant_cancelled
                && self.stop(&holder, &control, &[TurnState::Busy], TurnState::Held)
            {
                interrupted += 1;
            }
        }
        interrupted
    }

    /// Move an interrupted agent whose turn is still in `from` to `to`, so
    /// `continue` resumes it without waiting for its turn to unwind, and show it
    /// interrupted.
    fn stop(
        &self,
        agent: &AgentId,
        control: &AgentControl,
        from: &[TurnState],
        to: TurnState,
    ) -> bool {
        let stopped = control.turn_from(from, to);
        if stopped {
            self.activity(agent, AgentActivity::Stopped(TurnFailure::Interrupted));
        }
        stopped
    }

    /// Wait for interrupted agents' jobs to settle: an interrupt cancels model
    /// futures before their owning job has finished journaling.
    pub(super) async fn settle_interrupts(&self) {
        let interrupted = self
            .agents()
            .iter()
            .filter(|(_, agent)| agent.control.retryable_interrupt.load(Ordering::Acquire))
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        self.jobs.settle_interrupted_agents(&interrupted).await;
    }

    /// New input for the root holding a retained job breaks the link: the job goes
    /// on in the background and the held wait returns. Queued input then joins the
    /// turn's next request; a direct prompt ends the turn (`cancel`) to start the next.
    pub(super) async fn redirect(&self, root: &AgentId, cancel: bool) {
        self.settle_interrupts().await;
        // Cancel before releasing: the released wait must not reach an uncancelled
        // request boundary.
        if cancel
            && self.jobs.has_suspended(root).await
            && let Some(live) = self.agents().get(root)
        {
            live.cancellation.cancel();
        }
        self.jobs.release_held(root).await;
    }

    pub(super) async fn interrupt_tree(&self, root: &AgentId) -> usize {
        let targets = self
            .agents()
            .iter()
            .filter(|(agent, _)| agent.is_within(root))
            .map(|(id, agent)| (id.clone(), agent.cancellation.clone()))
            .collect::<Vec<_>>();
        let mut cancelled = 0;
        for (agent, cancellation) in targets {
            cancellation.cancel();
            self.activity(&agent, AgentActivity::Stopped(TurnFailure::Interrupted));
            cancelled += self.jobs.cancel_all(&agent).await;
        }
        cancelled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::runtime::tests::*;

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

    /// Interrupt stops only a running turn: one that ended while the interrupt
    /// awaited keeps its state, so an answered child still takes `JobsReady`.
    #[tokio::test]
    async fn interrupt_stops_only_a_running_turn() {
        let (_root, _, session) = scripted_session([]).await;
        let control = session.runtime.agents()[&session.root].control.clone();
        for (turn, stopped) in [(TurnState::Idle, false), (TurnState::Busy, true)] {
            control.set_turn(turn);
            (session.runtime).stop(&session.root, &control, RUNNING, TurnState::Parked);
            assert_eq!(control.turn() == TurnState::Parked, stopped, "{turn:?}");
        }
        shutdown_session(session).await;
    }

    /// A holder whose held child ended without `continue` runs on, still marked
    /// held, and a second interrupt stops it.
    #[tokio::test(start_paused = true)]
    async fn interrupt_stops_a_holder_whose_held_child_ended() {
        let child = json!({"prompt":"child task", "depth":0});
        let steps = [
            Step::new(response(vec![tool_call(0, "agent-0", "agent", child)])),
            Step::new(Vec::new()).midstream(),
            Step::new(Vec::new()).midstream(),
        ];
        let requests = Requests::default();
        let provider = Script::new(steps, &requests);
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let harness = test_harness(root.path(), &sessions, provider.clone()).await;
        let session = harness.new_session().await.unwrap();
        let parent_session = session.clone();
        let parent = tokio::spawn(async move { parent_session.prompt("delegate").await });
        provider.request(1).await;
        root_waiting(&session).await;
        assert_eq!(session.interrupt().await, 2);
        assert_eq!(turn(&session, &session.root), TurnState::Held);
        let job = session.runtime.jobs.list(&session.root).await[0].id;
        session.runtime.jobs.cancel(job).await.unwrap();
        provider.request(2).await;
        assert_eq!(session.interrupt().await, 1);
        let ended = bounded(parent).await.unwrap();
        assert!(matches!(ended, Err(HarnessError::Interrupted)), "{ended:?}");
        session.shutdown().await.unwrap();
    }
}
