//! Mailbox delivery, retained resumption, and interactive questions.

use super::*;

impl JobManager {
    #[cfg(test)]
    pub(crate) async fn set_resume_handler(
        &self,
        id: JobId,
        resume: impl Into<Resume>,
    ) -> Result<(), JobError> {
        let resume = resume.into();
        (self.inner.store)
            .live(move |state| state.jobs.set_resume(id, resume))
            .await
    }

    /// Without a handler an interrupted job is no longer suspended but settled,
    /// which a wait on it must see even when nothing releases it. A session
    /// shutting down leaves it suspended.
    pub(crate) async fn clear_resume_handler(&self, id: JobId) {
        let clear = move |_: &CoreState| {
            Ok::<_, SessionError>(Plan::new(Vec::new(), move |state, _| {
                let entry = state.jobs.entries.get_mut(&id);
                if let Some(entry) = entry.filter(|entry| entry.child().is_some()) {
                    entry.resume = None;
                }
            }))
        };
        let _ = self.inner.store.commit(Command::AgentExited, clear).await;
    }

    /// The child agent job `id` installed, if any.
    pub(crate) fn child_agent(&self, id: JobId) -> Option<AgentId> {
        self.entry(id, |entry| entry.child()?.agent.clone())
            .ok()
            .flatten()
    }

    /// Restored agent jobs that can run again but have no live handler yet: those
    /// whose call installed a child, and calls a stopped process interrupted
    /// before accepting them.
    pub(crate) fn retained_children(&self, state: &CoreState) -> Vec<RetainedChild> {
        let retained = state.jobs.entries.iter().filter(|(_, entry)| {
            let Some(child) = entry.child() else {
                return false;
            };
            let ended = match entry.end() {
                Some(JobEnd::Cancelled) => false,
                Some(JobEnd::Interrupted) | None => true,
                Some(JobEnd::Completed | JobEnd::Failed) => child.agent.is_some(),
            };
            let restarts = entry.accepts_input && entry.resume.is_none();
            restarts && ended && !entry.cancel_requested()
        });
        retained
            .map(|(id, entry)| self.retained(*id, entry))
            .collect()
    }

    fn retained(&self, job: JobId, entry: &JobFacts) -> RetainedChild {
        let restart = match entry.child().and_then(|child| child.agent.as_ref()) {
            Some(_) => Restart::Child {
                parent: entry.parent,
                location: entry.location.clone(),
                cancellation: self.handles(job, |handles| handles.cancellation.clone()),
            },
            None => Restart::Call,
        };
        let owner = entry.agent.clone();
        RetainedChild {
            job,
            owner,
            restart,
        }
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
            })??;
            if resumable {
                match self.resume(id, value).await? {
                    Resumption::Started => return Ok(()),
                    Resumption::Running(pending) => value = pending,
                }
                continue;
            }
            let sender = self.handles(id, |handles| handles.input.clone());
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
                    let manager = self.clone();
                    let moved = move |state: &mut CoreState| {
                        let Some(entry) = state.jobs.entries.get(&id) else {
                            return Some(Err(JobError::Unknown(id)));
                        };
                        let current = manager.handles(id, |handles| handles.input.clone());
                        match entry.resume {
                            None => Some(Err(JobError::InputClosed(id))),
                            Some(_) if entry.end().is_some() || !current.same_channel(&sender) => {
                                Some(Ok(()))
                            }
                            Some(_) => None,
                        }
                    };
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
    /// A call that installed no child is stopped as a crash stops it, and
    /// returned for a handler that asks for it again, as a reopen gives one.
    pub(crate) fn conclude(
        &self,
        state: &mut CoreState,
        scope: Concluding<'_>,
    ) -> Vec<RetainedChild> {
        let (jobs, agents) = (&state.jobs.entries, &state.agents);
        let concluded = |id: &JobId, child: &Child| match scope {
            Concluding::Every => true,
            Concluding::Released(links) => {
                let stopped = |agent: &AgentId| agents.stopped(agent);
                links.iter().any(|(link, _)| link == id)
                    && (child.start == Start::Preempted
                        || child.agent.as_ref().is_some_and(stopped))
            }
        };
        let running = jobs.iter().filter(|(id, entry)| {
            entry.end().is_none()
                && entry.child().is_some_and(|child| concluded(id, child))
                && !views::launched_by_blocking(jobs, entry)
        });
        let runs: Vec<_> = running
            .map(|(id, entry)| Run(*id, entry.invocation))
            .collect();
        let mut unaccepted = Vec::new();
        for run in runs {
            let Ok(claim) = super::lifecycle::claim(state, run, JobEnd::Interrupted) else {
                continue;
            };
            let entry = &state.jobs.entries[&run.0];
            let unbegun = entry.child().filter(|child| child.start != Start::Begun);
            match (unbegun.map(|child| child.agent.clone()), entry.parent) {
                (Some(Some(child)), _) => drop(state.agents.hand_back(&child, &state.jobs)),
                (Some(None), parent) => {
                    self.abandon(state, run, parent);
                    unaccepted.push(self.retained(run.0, &state.jobs.entries[&run.0]));
                }
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
        unaccepted
    }

    /// Stop what still works for invocation `run`: its worker, and what runs
    /// under its token, which later invocations no longer share.
    fn abandon(&self, state: &mut CoreState, Run(id, invocation): Run, parent: Option<JobId>) {
        let fresh = self.token_under(parent);
        let (stopped, worker) = self.handles(id, |handles| {
            let worker = handles.task_abort.take_if(|(run, _)| *run == invocation);
            (std::mem::replace(&mut handles.cancellation, fresh), worker)
        });
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
        let mut jobs = (state.jobs.entries.iter())
            .filter(|(_, entry)| entry.accepts_input)
            .filter_map(|(id, entry)| Some((*id, entry.agent.clone(), session_retryable(entry)?)))
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
            let entry = state.jobs.entries.get(&id).ok_or(JobError::Unknown(id))?;
            if !entry.resumable_end() {
                let running = Resumption::Running(value);
                return Ok(Plan::new(Vec::new(), move |_, _| running));
            }
            let reason = if entry.cancel_requested() {
                InputUnavailableReason::CancellationRequested
            } else if let Some(resume) = entry.resume.clone() {
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
        self.inner.store.commit(Command::Send, resume).await
    }

    /// Start a journaled restart's handler on a fresh mailbox: the running
    /// transition starts a new output generation, so old saved results cannot
    /// masquerade as the new invocation's output. A relaunched call finds the
    /// input that restarted it waiting in its mailbox.
    fn restart(&self, state: &mut CoreState, id: JobId, resume: Resume, value: Option<Value>) {
        let (input, receiver) = mpsc::channel(JOB_INPUT_CAPACITY);
        let cancellation = self.handles(id, |handles| {
            handles.input = input.clone();
            handles.cancellation.clone()
        });
        let run = Run(id, state.jobs.entries[&id].invocation);
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

/// The handler of a failed or interrupted, uncancelled job: a session-wide retry
/// restarts it with that. Unlike an explicit send, it never restarts Completed.
fn session_retryable(entry: &JobFacts) -> Option<Resume> {
    let retryable = matches!(entry.end(), Some(JobEnd::Failed | JobEnd::Interrupted))
        && !entry.cancel_requested();
    entry.resume.clone().filter(|_| retryable)
}

impl Jobs {
    /// Let `resume` restart agent job `id` once it ends.
    pub(crate) fn set_resume(&mut self, id: JobId, resume: Resume) -> Result<(), JobError> {
        let entry = self.entries.get_mut(&id).ok_or(JobError::Unknown(id))?;
        entry.child().ok_or(JobError::NoChild(id))?;
        entry.resume = Some(resume);
        Ok(())
    }

    /// Whether agent call `id` may install its child: it still runs, and no
    /// installed child of its owner, in any end state, has its name. A call that
    /// installs none holds no name, so of two taking one, the first to install
    /// has it.
    pub(crate) fn accepts(&self, id: JobId) -> Result<(), JobError> {
        let entry = self.entries.get(&id).ok_or(JobError::Unknown(id))?;
        if entry.end().is_some() {
            return Err(JobError::AlreadyTerminal(id));
        }
        let named = self.entries.iter().find(|(_, other)| {
            entry.name.is_some()
                && (other.agent == entry.agent && other.name == entry.name)
                && other.child().is_some_and(|child| child.agent.is_some())
        });
        named.map_or(Ok(()), |(job, other)| {
            let name = other.name.clone().expect("it has the call's name");
            Err(JobError::NameTaken { name, job: *job })
        })
    }

    /// Whether every interruption `agents` accepted has published its outcome:
    /// none still answers a running invocation it had begun.
    pub(crate) fn interruptions_published(&self, agents: &[AgentId]) -> bool {
        !self.entries.values().any(|entry| {
            entry.end().is_none()
                && entry.child().is_some_and(|launched| {
                    launched.start == Start::Begun
                        && (launched.agent.as_ref()).is_some_and(|child| agents.contains(child))
                })
        })
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
        let calls = Arc::new(AtomicUsize::new(0));
        let invoked = calls.clone();
        let handler: ResumeHandler = Arc::new(move |value, _| {
            invoked.fetch_add(1, Ordering::SeqCst);
            let value = value.unwrap_or_else(|| serde_json::json!("resumed"));
            Box::pin(async move { Ok(ToolOutput::new(value)) })
        });
        jobs.set_resume_handler(lease.id(), handler).await.unwrap();
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
        for outcome in [
            JobOutcome::Completed(ToolOutput::new(serde_json::json!({"retained": true}))),
            ToolError::interrupted().into(),
        ] {
            let (lease, calls) = retained(&jobs, &agent).await;
            jobs.finish(lease.id(), outcome).await.unwrap();
            jobs.clear_resume_handler(lease.id()).await;
            let before = jobs.snapshot(lease.id()).unwrap();
            assert!(matches!(
                jobs.send(lease.id(), serde_json::json!("not accepted")).await,
                Err(JobError::InputUnavailable { job, reason: InputUnavailableReason::ResumeUnavailable })
                    if job == lease.id()
            ));
            assert_eq!(jobs.snapshot(lease.id()).unwrap(), before);
            assert_eq!(calls.load(Ordering::SeqCst), 0);
        }
    }

    /// An agent stopping clears its job's handler, settling its interruption. A
    /// wait held on it returns though nothing released it first.
    #[tokio::test]
    async fn removing_the_resume_handler_ends_a_wait_held_on_an_interruption() {
        use futures_util::FutureExt;
        let (_root, jobs, agent) = super::super::tests::runtime().await;
        let (lease, _) = retained(&jobs, &agent).await;
        let id = lease.id();
        jobs.finish(id, ToolError::interrupted().into())
            .await
            .unwrap();
        let wait = jobs.wait_foreground(id);
        tokio::pin!(wait);
        assert!((&mut wait).now_or_never().is_none());
        jobs.clear_resume_handler(id).await;
        let (envelope, _) = crate::tests::bounded(wait).await.unwrap();
        assert_eq!(envelope.state, JobState::Interrupted);
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
