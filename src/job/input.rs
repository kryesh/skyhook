//! Mailbox delivery, retained resumption, and interactive questions.

use super::*;

impl JobManager {
    /// The child agent job `id` installed, if any.
    pub(crate) async fn child_agent(&self, id: JobId) -> Option<AgentId> {
        self.entry(id, |entry| entry.child()?.agent.clone())
            .await
            .ok()
            .flatten()
    }

    /// Give agent job `job`, which a child's loop answers through, what it runs
    /// with, if nothing has run it since it was held.
    pub(crate) fn hold_handles(&self, state: &mut CoreState, job: JobId) {
        let Some(entry) = state.jobs.get(job) else {
            return;
        };
        let cancellation = self.token_of(&state.jobs, entry);
        state.jobs.modify(job, |entry, _| {
            (entry.handles).get_or_insert_with(|| JobHandles::new(cancellation).0);
        });
    }

    /// The token job `entry` runs under: its own, or a fresh one under its
    /// parent's while nothing runs it.
    fn token_of(&self, jobs: &Jobs, entry: &JobFacts) -> CancellationToken {
        match &entry.handles {
            Some(handles) => handles.cancellation.clone(),
            None => self.token_under(jobs, entry.parent),
        }
    }

    /// What runs job `id`, which `entry` holds, again, as the session's restarter
    /// builds it, with the token it runs under.
    fn resume_of(
        &self,
        state: &CoreState,
        id: JobId,
        entry: &JobFacts,
    ) -> Option<(Resume, CancellationToken)> {
        let restart = entry.restart()?;
        let restarter = self.inner.restarter.get()?.upgrade()?;
        let cancellation = self.token_of(&state.jobs, entry);
        let restarting = Restarting {
            job: id,
            owner: entry.agent.clone(),
            parent: entry.parent,
            location: entry.location.clone(),
            cancellation: cancellation.clone(),
            restart,
        };
        Some((restarter.resume(restarting)?, cancellation))
    }

    /// Accept input into the current mailbox or start a retained resumption.
    /// Success does not confirm that the worker has consumed or acted on it.
    pub async fn send(&self, id: JobId, mut value: Value) -> Result<(), JobError> {
        loop {
            let resumable = self.entry(id, |entry| {
                if !entry.accepts_input {
                    return Err(JobError::InputUnsupported(id));
                }
                if !entry.resumable_end()
                    && !matches!(entry.phase, Phase::Running | Phase::WaitingInput(_))
                {
                    return Err(JobError::InputUnavailable {
                        job: id,
                        reason: InputUnavailableReason::State(entry.state()),
                    });
                }
                Ok(entry.resumable_end())
            });
            let resumable = resumable.await??;
            if resumable {
                match self.resume(id, value).await? {
                    Resumption::Started => return Ok(()),
                    Resumption::Running(pending) => value = pending,
                }
                continue;
            }
            let sender = self.entry(id, |entry| Some(entry.handles.as_ref()?.input.clone()));
            let sender = sender.await?;
            let sender = sender.ok_or(JobError::InputClosed(id))?;
            // Accepted under the core's read lock, so the handler's close, a core
            // live command, orders with it and its drain sees every accepted input.
            match self.facts(|_| sender.try_send(value)) {
                Ok(()) => return Ok(()),
                Err(mpsc::error::TrySendError::Full(pending)) => {
                    value = pending;
                    // Wait for room, carrying no permit across the close boundary.
                    let _ = sender.reserve().await;
                }
                Err(mpsc::error::TrySendError::Closed(pending)) => {
                    value = pending;
                    // Child request handlers close their mailbox before finalizing.
                    // A rejected send retries against the next lifecycle, never losing input.
                    let moved = move |state: &CoreState| {
                        let entry = match state.jobs.lookup(id) {
                            Lookup::Held(entry) => entry,
                            // Let go since the wait last decided, so as it was then.
                            Lookup::Unheld(()) => return None,
                            Lookup::Unknown => return Some(Err(JobError::Unknown(id))),
                        };
                        let current = entry.handles.as_ref().map(|handles| &handles.input);
                        let moved = current.is_none_or(|current| !current.same_channel(&sender));
                        match entry.restart() {
                            None => Some(Err(JobError::InputClosed(id))),
                            Some(_) if entry.end().is_some() || moved => Some(Ok(())),
                            Some(_) => None,
                        }
                    };
                    let moved = Wait::on([Key::Job(id)], moved);
                    self.inner.store.until(moved).await??;
                }
            }
        }
    }

    /// End the running agent invocations `scope` selects interrupted now, rather
    /// than once their turns unwind: resumable where the child is installed, as
    /// a crash would leave them. Each finish is claimed here, so nothing its
    /// worker reports meanwhile ends it otherwise; one already finishing keeps
    /// its own outcome. An invocation yet to begin gives up the input that would
    /// begin it. What a script launched ends with the script instead.
    /// A call that installed no child is stopped as a crash stops it, so input
    /// asks for it again.
    pub(crate) fn conclude(&self, state: &mut CoreState, scope: Concluding<'_>) {
        let (jobs, agents) = (&state.jobs, &state.agents);
        let candidates: Vec<_> = match scope {
            Concluding::Every => jobs.running().map(|(id, _)| id).collect(),
            Concluding::Released(links) => links.iter().map(|(link, _)| *link).collect(),
        };
        let concluded = |child: &Child| match scope {
            Concluding::Every => true,
            Concluding::Released(_) => {
                let stopped = |agent: &AgentId| agents.stopped(agent);
                child.start == Start::Preempted || child.agent.as_ref().is_some_and(stopped)
            }
        };
        let running = candidates.into_iter().filter_map(|id| {
            let entry = jobs.get(id)?;
            let concludes = entry.end().is_none()
                && entry.child().is_some_and(concluded)
                && !views::launched_by_blocking(jobs, entry);
            concludes.then_some(Run(id, entry.invocation))
        });
        let runs: Vec<_> = running.collect();
        for run in runs {
            let Ok(claim) = super::lifecycle::claim(state, run, JobEnd::Interrupted) else {
                continue;
            };
            let entry = state.jobs.get(run.0).expect("a claimed job is held");
            let unbegun = entry.child().filter(|child| child.start != Start::Begun);
            match (unbegun.map(|child| child.agent.clone()), entry.parent) {
                (Some(Some(child)), _) => drop(state.agents.hand_back(&child, &state.jobs)),
                (Some(None), parent) => self.abandon(state, run, parent),
                (None, _) => {}
            }
            let manager = self.clone();
            state.defer(move || {
                let published = manager.clone();
                let publish = async move {
                    published
                        .publish_claimed(claim, ToolError::interrupted().into())
                        .await
                };
                manager.inner.supervisor.spawn(publish);
            });
        }
    }

    /// Stop what still works for invocation `run`: its worker, and what runs
    /// under its token, which later invocations no longer share.
    fn abandon(&self, state: &mut CoreState, Run(id, invocation): Run, parent: Option<JobId>) {
        let fresh = self.token_under(&state.jobs, parent);
        let stopped = state.jobs.modify(id, |entry, _| {
            let handles = entry.handles.as_mut()?;
            let worker = handles.task_abort.take_if(|(run, _)| *run == invocation);
            Some((std::mem::replace(&mut handles.cancellation, fresh), worker))
        });
        let Some((stopped, worker)) = stopped.flatten() else {
            return;
        };
        state.defer(move || {
            stopped.cancel();
            worker.into_iter().for_each(|(_, worker)| worker.abort());
        });
    }

    /// Restart every session-retryable child, deepest descendants first, in one
    /// transaction: what it journals, and the change that then starts them and
    /// reports how many invocations it resumed.
    pub(crate) fn continue_resumable(
        &self,
        state: &CoreState,
    ) -> (
        Vec<(AgentId, SessionEvent)>,
        impl FnOnce(&mut CoreState) -> usize + use<>,
    ) {
        let mut jobs = (state.jobs.select(JobFacts::retryable).into_iter())
            .filter_map(|(id, entry)| {
                Some((id, entry.agent.clone(), self.resume_of(state, id, entry)?))
            })
            .collect::<Vec<_>>();
        jobs.sort_by_key(|(_, owner, _)| std::cmp::Reverse(owner.depth()));
        let running = (jobs.iter())
            .map(|(id, owner, _)| (owner.clone(), running(*id)))
            .collect();
        let manager = self.clone();
        let restart = move |state: &mut CoreState| {
            // An invocation an interrupt stopped before it began begins after all.
            let resumed = jobs.len() + state.jobs.resume_preempted();
            for (id, _, resume) in jobs {
                manager.restart(state, id, resume, None);
            }
            resumed
        };
        (running, restart)
    }

    /// Restart job `id` with `value` if it ended resumably, journaling it running
    /// and starting its handler in one step.
    async fn resume(&self, id: JobId, value: Value) -> Result<Resumption, JobError> {
        let manager = self.clone();
        let resume = move |state: &CoreState| {
            let entry = state.job(id)?;
            if !entry.resumable_end() {
                let running = Resumption::Running(value);
                return Ok(Plan::new(Vec::new(), move |_, _| running));
            }
            let reason = if entry.cancel_requested() {
                InputUnavailableReason::CancellationRequested
            } else if let Some(resume) = manager.resume_of(state, id, entry) {
                let running = vec![(entry.agent.clone(), running(id))];
                return Ok(Plan::new(running, move |state, _| {
                    manager.restart(state, id, resume, Some(value));
                    Resumption::Started
                }));
            } else {
                InputUnavailableReason::ResumeUnavailable
            };
            Err(JobError::InputUnavailable { job: id, reason })
        };
        self.inner
            .store
            .commit_on([id], Command::Send, resume)
            .await
    }

    /// Start a journaled restart's handler on a fresh mailbox, under the token
    /// it was built with if nothing ran the job yet: the running transition
    /// starts a new output generation, so old saved results cannot masquerade
    /// as the new invocation's output. A relaunched call finds the input that
    /// restarted it waiting in its mailbox.
    fn restart(
        &self,
        state: &mut CoreState,
        id: JobId,
        (resume, cancellation): (Resume, CancellationToken),
        value: Option<Value>,
    ) {
        let (input, receiver) = mpsc::channel(JOB_INPUT_CAPACITY);
        let started = state.jobs.modify(id, |entry, _| {
            let handles = (entry.handles).get_or_insert_with(|| JobHandles::new(cancellation).0);
            handles.input = input.clone();
            (handles.cancellation.clone(), Run(id, entry.invocation))
        });
        let Some((cancellation, run)) = started else {
            return;
        };
        let manager = self.clone();
        match resume {
            Resume::Child(handler) => {
                let worker = super::supervisor::JobWorker::new(manager, run, cancellation);
                let resumed = async move { handler(value, receiver).await };
                state.defer(move || worker.launch(resumed, "agent resume handler panicked"));
            }
            Resume::Call(relaunch) => {
                if let Some(value) = value {
                    let _ = input.try_send(value);
                }
                let lease = JobLease::new(manager, run, cancellation, receiver);
                let supervisor = self.inner.supervisor.clone();
                state.defer(move || drop(supervisor.spawn(relaunch(lease))));
            }
        }
    }

    /// Suspend a running job until its caller supplies input. Already waiting
    /// jobs may refresh their question when the outstanding set changes.
    pub(crate) async fn request_input(
        &self,
        id: JobId,
        question: QuestionOutput,
    ) -> Result<(), JobError> {
        let ask = JobChange::Ask(question);
        (self.journal_change(id, ask, Rejected::input)).await
    }

    /// Resume a waiting job whose question was answered.
    pub async fn resume_input(&self, id: JobId) -> Result<(), JobError> {
        (self.journal_change(id, JobChange::Answer, Rejected::input)).await
    }
}

/// Which running agent invocations a stop ends interrupted.
#[derive(Clone, Copy)]
pub(crate) enum Concluding<'a> {
    /// Every one, as closing leaves them.
    Every,
    /// Of the links a redirect released, those an interrupt stopped: whose
    /// child's turn it stopped, or which had yet to begin.
    Released(&'a [(JobId, AgentId)]),
}

/// What a send that found its job ended resumably did.
enum Resumption {
    Started,
    /// Another resumption won: the input joins the invocation it started.
    Running(Value),
}

fn running(job: JobId) -> SessionEvent {
    let state = JobTransition::Running;
    SessionEvent::JobStateChanged { job, state }
}

impl Jobs {
    /// Whether agent call `id` may install its child: it still runs, and no
    /// installed child of its owner, in any end state, has its name. A call that
    /// installs none holds no name, so of two taking one, the first to install
    /// has it.
    pub(crate) fn accepts(&self, id: JobId) -> Result<(), JobError> {
        let entry = self.get(id).ok_or(JobError::Unknown(id))?;
        if entry.end().is_some() {
            return Err(JobError::AlreadyTerminal(id));
        }
        let Some(name) = &entry.name else {
            return Ok(());
        };
        match self.named(&entry.agent, name) {
            Some(job) => Err(JobError::NameTaken {
                name: name.clone(),
                job,
            }),
            None => Ok(()),
        }
    }

    /// Whether every interruption `agents` accepted, and every launch in
    /// `preempted` that has since begun, has published its outcome: none still
    /// answers a running invocation it had begun.
    pub(crate) fn interruptions_published(&self, agents: &[AgentId], preempted: &[JobId]) -> bool {
        let unpublished = |job: JobId| {
            self.get(job).is_some_and(|entry| {
                let begun = entry
                    .child()
                    .is_some_and(|launched| launched.start == Start::Begun);
                entry.end().is_none() && begun
            })
        };
        let launched = agents.iter().filter_map(|agent| self.launched(agent));
        !launched.chain(preempted.iter().copied()).any(unpublished)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A child's name belongs to its owner's installed children, whatever their
    /// end: a call that installed none holds no name, so the first of two to
    /// install has it, and another owner's children share none.
    #[tokio::test]
    async fn a_name_belongs_to_the_installed_children_of_its_owner() {
        let runtime = crate::tests::TestRuntime::new().await;
        let jobs = &runtime.jobs;
        let worker: JobName = "worker".parse().unwrap();
        let launch = async |owner: &AgentId| {
            jobs.create(JobSpec {
                role: JobRole::Agent,
                name: Some(worker.clone()),
                ..JobSpec::test(owner.clone(), "agent")
            })
            .await
            .unwrap()
            .into_test_id()
        };
        let accepts = |job| jobs.facts(|jobs| jobs.accepts(job));
        let (first, second) = (launch(&runtime.agent).await, launch(&runtime.agent).await);
        assert!(accepts(first).is_ok() && accepts(second).is_ok());
        let root = runtime.root.path();
        crate::session::tests::start_child(&runtime.store, &runtime.agent, 1, Some(second), root)
            .await;
        let taken =
            |refused| matches!(refused, Err(JobError::NameTaken { job, .. }) if job == second);
        assert!(taken(accepts(first)));
        let failed = crate::tool::ToolError::failed("refused before installing a child");
        jobs.finish(first, failed.into()).await.unwrap();
        let completed = JobOutcome::Completed(crate::tool::ToolOutput::new(Value::Null));
        jobs.finish(second, completed).await.unwrap();
        assert!(taken(accepts(launch(&runtime.agent).await)));
        let other =
            crate::session::tests::start_child(&runtime.store, &runtime.agent, 2, None, root).await;
        assert!(accepts(launch(&other).await).is_ok());
    }

    /// A running retained agent job whose resume handler echoes and counts calls.
    async fn retained(
        jobs: &JobManager,
        agent: &AgentId,
    ) -> (JobLease<stage::Running>, Arc<AtomicUsize>) {
        let spec = JobSpec {
            accepts_input: true,
            role: JobRole::Agent,
            ..JobSpec::test(agent.clone(), "agent")
        };
        let lease = jobs.test_running(spec).await;
        jobs.test_install(lease.id()).await;
        let calls = Arc::new(AtomicUsize::new(0));
        let invoked = calls.clone();
        let handler: ResumeHandler = Arc::new(move |value, _| {
            invoked.fetch_add(1, Ordering::SeqCst);
            let value = value.unwrap_or_else(|| serde_json::json!("resumed"));
            Box::pin(async move { Ok(ToolOutput::new(value)) })
        });
        jobs.test_resume_with(lease.id(), handler);
        (lease, calls)
    }

    async fn settled(jobs: &JobManager, id: JobId) -> crate::job::tests::Hydrated {
        crate::tests::bounded(jobs.wait(id, true)).await.unwrap()
    }

    #[tokio::test]
    async fn send_reports_lifecycle_state_or_missing_resume_without_replacing_output() {
        let (_root, jobs, agent) = super::super::tests::runtime().await;
        for state in [JobState::Queued, JobState::AwaitingApproval] {
            let spec = JobSpec {
                accepts_input: true,
                ..JobSpec::test(agent.clone(), "pending")
            };
            let id = if state == JobState::AwaitingApproval {
                jobs.test_approving(spec).await.into_test_id()
            } else {
                jobs.test_create(spec).await
            };
            let error = jobs
                .send(id, serde_json::json!("not accepted"))
                .await
                .unwrap_err();
            assert!(matches!(
                error,
                JobError::InputUnavailable { job, reason: InputUnavailableReason::State(actual) }
                    if job == id && actual == state
            ));
            assert!(error.to_string().contains("queued"));
            assert!(!error.to_string().contains("approval"));
        }
        // A call that installed no child, once it ends otherwise than interrupted.
        let spec = JobSpec {
            accepts_input: true,
            role: JobRole::Agent,
            ..JobSpec::test(agent.clone(), "agent")
        };
        let lease = jobs.test_running(spec).await;
        let completed = ToolOutput::new(serde_json::json!({"retained": true}));
        jobs.test_complete(lease.id(), completed).await;
        let before = jobs.snapshot(lease.id()).await.unwrap();
        assert!(matches!(
            jobs.send(lease.id(), serde_json::json!("not accepted")).await,
            Err(JobError::InputUnavailable { job, reason: InputUnavailableReason::ResumeUnavailable })
                if job == lease.id()
        ));
        assert_eq!(jobs.snapshot(lease.id()).await.unwrap(), before);
    }

    /// An interrupted retained child resumes on send unless it was cancelled,
    /// which releases waiters and prevents resumption.
    #[tokio::test]
    async fn interrupted_retained_child_resumes_on_send_unless_cancelled() {
        let (_root, jobs, agent) = super::super::tests::runtime().await;
        for cancel in [false, true] {
            let (lease, calls) = retained(&jobs, &agent).await;
            jobs.finish(lease.id(), ToolError::interrupted().into())
                .await
                .unwrap();
            if cancel {
                let (cancelled, _) = crate::tests::expire(
                    jobs.wait(lease.id(), true),
                    async { jobs.cancel(lease.id()).await.unwrap() },
                    super::super::cancellation::CANCELLATION_GRACE,
                )
                .await;
                assert_eq!(cancelled.unwrap().state, JobState::Cancelled);
            }
            let sent = jobs.send(lease.id(), serde_json::json!("retry")).await;
            if cancel {
                assert!(matches!(
                    sent,
                    Err(JobError::InputUnavailable {
                        reason: InputUnavailableReason::State(JobState::Cancelled),
                        ..
                    })
                ));
                assert_eq!(calls.load(Ordering::SeqCst), 0);
            } else {
                sent.unwrap();
                let result = settled(&jobs, lease.id()).await;
                assert_eq!(result.state, JobState::Completed);
                assert_eq!(result.output, Some(serde_json::json!("retry")));
            }
        }
    }

    #[tokio::test]
    async fn send_retries_when_child_mailbox_closes_before_completion() {
        let (_root, jobs, agent) = super::super::tests::runtime().await;
        let (lease, _calls) = retained(&jobs, &agent).await;
        let id = lease.id();
        let (mut input, _worker) = lease.split();
        input.close();
        let send = jobs.send(id, serde_json::json!("not lost"));
        tokio::pin!(send);
        // Poll through the closed-mailbox check to its lifecycle notification.
        assert!(futures_util::poll!(&mut send).is_pending());
        jobs.test_finish(id, serde_json::Value::Null).await;
        crate::tests::bounded(send).await.unwrap();
        let result = settled(&jobs, id).await;
        assert_eq!(result.state, JobState::Completed);
        assert_eq!(result.output, Some(serde_json::json!("not lost")));
    }
}
