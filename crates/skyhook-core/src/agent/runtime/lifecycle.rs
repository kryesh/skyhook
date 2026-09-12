//! Session initialization and root-agent lifecycle.

use super::*;

impl SessionRuntime {
    pub(super) async fn build(
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
        let (mcp, startup_warnings) = tools::connect_mcp(&mut builder, &harness, &store).await;
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
            mcp,
            startup_warnings,
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

    pub(super) async fn start_root(
        self: &Arc<Self>,
        selection: Option<String>,
    ) -> Result<SessionHandle, HarnessError> {
        let root = AgentId::root(self.store.id());
        let model_profile = selection.unwrap_or_else(|| self.harness.default_model_profile.clone());
        let root_tx = self
            .spawn_agent(AgentLaunch {
                id: root.clone(),
                owner_job: None,
                model_profile,
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
}
impl SessionRuntime {
    /// Interrupt current turns without cancelling their owner jobs. This is the
    /// retryable session-interrupt path; explicit job/tree cancellation remains in
    /// `interrupt_tree` below.
    pub(super) async fn interrupt_turns(&self, root: &AgentId) -> usize {
        let targets = self
            .agents
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|(agent, _)| {
                agent.session() == root.session() && agent.path().starts_with(root.path())
            })
            .map(|(id, agent)| {
                (
                    id.clone(),
                    agent.cancellation.clone(),
                    agent.retryable_interrupt.clone(),
                )
            })
            .collect::<Vec<_>>();
        let activity = self.events.observe().snapshot.activity;
        let mut interrupted = 0;
        for (agent, cancellation, retryable) in &targets {
            match activity.get(agent) {
                // Preserve actual waits, not every agent with background jobs:
                // an agent may be making a model request while its jobs run.
                Some(AgentActivity::Tools | AgentActivity::WaitingChildren)
                    if self.jobs.has_running(agent).await =>
                {
                    continue;
                }
                None
                | Some(
                    AgentActivity::Idle | AgentActivity::Failed(_) | AgentActivity::Interrupted,
                ) => continue,
                _ => {}
            }
            retryable.store(true, Ordering::Release);
            cancellation.cancel();
            self.activity(agent, AgentActivity::Interrupted);
            interrupted += 1;
        }
        interrupted
    }

    pub(super) async fn interrupt_tree(&self, root: &AgentId) -> usize {
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
}
