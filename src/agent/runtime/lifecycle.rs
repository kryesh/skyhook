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
                SessionEvent::SessionStarted { targets, .. } => Some(targets.clone()),
                _ => None,
            })
            // A new session journals its start, with these targets, alongside its root.
            .or_else(|| {
                prior_records
                    .is_empty()
                    .then(|| harness.target_definitions.clone())
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
            crate::tool::authorization::AuthorizationCoordinator::new(harness.policy.clone())
                .journaled(store.clone())
                .await;
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
            #[cfg(test)]
            store_forwarding_gate: Arc::new(Mutex::new(())),
        });
        runtime_slot
            .set(Arc::downgrade(&runtime))
            .map_err(|_| HarnessError::Initialization("runtime already set".to_owned()))?;
        runtime.install_retained_children().await;
        runtime.forward_store_events();
        runtime.forward_job_completions();
        Ok(runtime)
    }

    /// Close what a stopped process left open, in one transaction, before any agent
    /// resumes: attempts without an outcome, and committed calls without a result.
    /// Their agents' turns end interrupted; returns those agents. A settled call to
    /// a retained child releases it: nothing waits on it any more.
    pub(super) async fn settle_interrupted_work(&self) -> Result<Vec<AgentId>, HarnessError> {
        let work = self.store.interrupted_work().await?;
        if work.attempts.is_empty() && work.calls.is_empty() {
            return Ok(Vec::new());
        }
        let attempts = work.attempts.into_iter().map(|(agent, request, attempt)| {
            (
                agent,
                SessionEvent::ModelAttemptInterrupted { request, attempt },
            )
        });
        let mut events: Vec<_> = attempts.collect();
        let mut agents: Vec<_> = events.iter().map(|(agent, _)| agent.clone()).collect();
        agents.extend(work.calls.iter().map(|(agent, ..)| agent.clone()));
        agents.sort();
        agents.dedup();
        events.extend(work.calls.into_iter().map(|(agent, call_id, name)| {
            let result = ToolResult {
                call_id,
                name,
                result: json!({"error": "interrupted while the session was not running"}),
                images: Vec::new(),
                is_error: true,
            };
            let message = Message::Tool(vec![result]);
            (agent, SessionEvent::MessageCommitted { message })
        }));
        let interrupted = agents.iter().cloned();
        events.extend(interrupted.map(|agent| (agent, SessionEvent::AgentInterrupted)));
        self.store.append_all(events).await?;
        for agent in &agents {
            self.jobs.release_held(agent).await;
        }
        Ok(agents)
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
        })
    }
}
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
            .filter(|agent| {
                agent.session() == root.session() && agent.path().starts_with(root.path())
            })
            .cloned()
            .collect::<Vec<_>>();
        targets.sort_by_key(AgentId::depth);
        let activity = self.events.observe().snapshot.activity;
        let (mut holders, mut cancelled) = (Vec::new(), Vec::new());
        for agent in &targets {
            let work = self.jobs.live_work(agent).await;
            match activity.get(agent) {
                // Preserve a genuine wait on work an interrupt keeps: retained
                // children, which `continue` restarts, or background jobs.
                Some(AgentActivity::Tools | AgentActivity::WaitingChildren)
                    if work.any && work.blocking.is_empty() =>
                {
                    if work.holding.is_some() {
                        holders.push(agent.clone());
                    }
                    continue;
                }
                None
                | Some(
                    AgentActivity::Idle | AgentActivity::Failed(_) | AgentActivity::Interrupted,
                ) => continue,
                _ => {}
            }
            // Read the turn's token only now: the agent may have begun a new turn
            // while the awaits above ran, and a stale token would leave that turn
            // live while its jobs are cancelled below.
            let Some((cancellation, retryable)) = self.agents().get(agent).map(|live| {
                (
                    live.cancellation.clone(),
                    live.control.retryable_interrupt.clone(),
                )
            }) else {
                continue;
            };
            // Order is load-bearing: mark and cancel the turn before its jobs. A job
            // cancelled first would let the drain finish and reach an uncancelled
            // request boundary, starting a fresh model request.
            retryable.store(true, Ordering::Release);
            cancellation.cancel();
            for job in work.blocking {
                // Commits an error result, which `continue` then resumes from.
                let _ = self.jobs.cancel(job).await;
            }
            self.activity(agent, AgentActivity::Interrupted);
            cancelled.push(agent.clone());
        }
        // A held child is retained too; its holder shows interrupted meanwhile.
        let mut interrupted = cancelled.len();
        for holder in holders {
            let descendant_cancelled = cancelled
                .iter()
                .any(|agent| agent != &holder && agent.path().starts_with(holder.path()));
            if descendant_cancelled {
                self.activity(&holder, AgentActivity::Interrupted);
                interrupted += 1;
            }
        }
        interrupted
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

    /// New input for an agent holding a retained job breaks the link: the job goes
    /// on in the background and the held wait returns. Queued input then joins the
    /// turn's next request; a direct prompt ends the turn (`cancel`) to start the next.
    pub(super) async fn redirect(&self, agent: &AgentId, cancel: bool) {
        self.settle_interrupts().await;
        // Cancel before releasing: the released wait must not reach an uncancelled
        // request boundary.
        if cancel
            && self.jobs.has_suspended(agent).await
            && let Some(live) = self.agents().get(agent)
        {
            live.control
                .retryable_interrupt
                .store(true, Ordering::Release);
            live.cancellation.cancel();
        }
        self.jobs.release_held(agent).await;
    }

    pub(super) async fn interrupt_tree(&self, root: &AgentId) -> usize {
        let targets = self
            .agents()
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
