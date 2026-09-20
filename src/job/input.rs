//! Mailbox delivery, retained resumption, and interactive questions.

use super::*;

impl JobManager {
    pub(crate) async fn set_resume_handler(
        &self,
        id: JobId,
        handler: ResumeHandler,
    ) -> Result<(), JobError> {
        let mut jobs = self.inner.jobs.lock().await;
        let entry = jobs.get_mut(&id).ok_or(JobError::Unknown(id))?;
        entry.resume = Some(handler);
        Ok(())
    }

    pub(crate) async fn clear_resume_handler(&self, id: JobId) {
        if let Some(entry) = self.inner.jobs.lock().await.get_mut(&id) {
            entry.resume = None;
        }
    }

    /// Restored agent jobs whose child can resume but has no live handler yet.
    pub(crate) async fn retained_children(&self) -> Vec<RetainedChild> {
        self.inner
            .jobs
            .lock()
            .await
            .iter()
            .filter_map(|(id, entry)| {
                let retained = entry.role == crate::job::JobRole::Agent
                    && entry.accepts_input
                    && entry.resume.is_none()
                    && !entry.cancellation.is_cancelled()
                    && entry.state != JobState::Cancelled;
                retained.then_some(())?;
                Some(RetainedChild {
                    job: *id,
                    owner: entry.agent.clone(),
                    parent: entry.parent,
                    scope: entry.authorization_scope,
                    child: entry.child.clone()?,
                    location: entry.location.clone(),
                    cancellation: entry.cancellation.clone(),
                })
            })
            .collect()
    }

    /// Find a caller-owned child (or a launch still in progress) using this name.
    /// Installed children retain their names after completion because follow-ups
    /// address their existing job. A failed admission with no child identity must
    /// not reserve the name permanently. The current invocation excludes itself.
    pub(crate) async fn child_name_owner(
        &self,
        owner: &AgentId,
        name: &str,
        current: JobId,
    ) -> Option<JobId> {
        self.inner.jobs.lock().await.iter().find_map(|(id, entry)| {
            (*id != current
                && &entry.agent == owner
                && entry.role == crate::job::JobRole::Agent
                && entry.name.as_deref() == Some(name)
                // Earlier pending launches reserve the name. Without ordering,
                // simultaneous invocations could both reject one another before
                // either has installed a child.
                && (entry.child.is_some() || (*id < current && !entry.state.is_terminal())))
            .then_some(*id)
        })
    }

    /// Record the agent identity as soon as it is launched, including turns that
    /// fail before producing visible assistant text.
    pub(crate) async fn set_child_agent(&self, id: JobId, child: AgentId) -> Result<(), JobError> {
        let mut jobs = self.inner.jobs.lock().await;
        let entry = jobs.get_mut(&id).ok_or(JobError::Unknown(id))?;
        entry.child = Some(child);
        Ok(())
    }

    pub async fn send(&self, id: JobId, mut value: Value) -> Result<(), JobError> {
        loop {
            // Serialize resumption against finishing and other senders.
            let operation = self.operation(id).await?;
            let guard = operation.lock_owned().await;
            let sender = {
                let jobs = self.inner.jobs.lock().await;
                let entry = jobs.get(&id).ok_or(JobError::Unknown(id))?;
                if !entry.accepts_input {
                    return Err(JobError::InputUnsupported(id));
                }
                if matches!(
                    entry.state,
                    JobState::Completed | JobState::Failed | JobState::Interrupted
                ) {
                    drop(jobs);
                    return self.resume_locked(id, Some(value), guard).await;
                }
                if !matches!(entry.state, JobState::Running | JobState::WaitingInput) {
                    return Err(JobError::NotRunning(id));
                }
                entry.input.clone()
            };
            // Enqueue under the same lock used to drain/close the mailbox.
            // Never hold that lock while waiting for capacity.
            let sent = sender.try_send(value);
            drop(guard);
            match sent {
                Ok(()) => return Ok(()),
                Err(mpsc::error::TrySendError::Full(pending)) => {
                    value = pending;
                    // Do not carry a reserved permit across the close boundary.
                    if let Ok(permit) = sender.reserve().await {
                        drop(permit);
                    }
                }
                Err(mpsc::error::TrySendError::Closed(pending)) => {
                    value = pending;
                    // Child request handlers close their mailbox before finalizing.
                    // A rejected send retries against the next lifecycle, never losing input.
                    loop {
                        let notified = {
                            let jobs = self.inner.jobs.lock().await;
                            let entry = jobs.get(&id).ok_or(JobError::Unknown(id))?;
                            if entry.resume.is_none() {
                                return Err(JobError::InputClosed(id));
                            }
                            if entry.state.is_terminal() || !entry.input.same_channel(&sender) {
                                break;
                            }
                            entry.notify.clone().notified_owned()
                        };
                        notified.await;
                    }
                }
            }
        }
    }

    /// Wait for requested child interruptions to finish publishing their outcome.
    /// This does not claim or deliver the interrupted result to the parent.
    pub(crate) async fn settle_interrupted_agents(&self, agents: &[AgentId]) {
        loop {
            let pending = {
                let jobs = self.inner.jobs.lock().await;
                jobs.values()
                    .find(|entry| {
                        !entry.state.is_terminal()
                            && entry
                                .child
                                .as_ref()
                                .is_some_and(|child| agents.contains(child))
                    })
                    .map(|entry| entry.notify.clone().notified_owned())
            };
            let Some(pending) = pending else {
                return;
            };
            pending.await;
        }
    }

    /// Restart every failed or interrupted retained child, deepest descendants first.
    ///
    /// Callers use this for a session-wide retry. Jobs with a cancelled owner or
    /// no live handler are deliberately excluded.
    pub(crate) async fn continue_resumable_children(&self) -> Result<usize, JobError> {
        let mut jobs = {
            let entries = self.inner.jobs.lock().await;
            entries
                .iter()
                .filter_map(|(id, entry)| {
                    (matches!(entry.state, JobState::Failed | JobState::Interrupted)
                        && !entry.cancellation.is_cancelled()
                        && entry.resume.is_some())
                    .then(|| entry.child.clone().map(|child| (*id, child)))
                    .flatten()
                })
                .collect::<Vec<_>>()
        };
        jobs.sort_by_key(|(_, child)| std::cmp::Reverse(child.depth()));
        let mut resumed = 0;
        for (id, _) in jobs {
            let operation = match self.operation(id).await {
                Ok(operation) => operation,
                Err(JobError::Unknown(_)) => continue,
                Err(error) => return Err(error),
            };
            let guard = operation.lock_owned().await;
            // The snapshot can go stale while waiting on another child's operation.
            // Unlike an explicit send, a session retry must never restart Completed.
            let eligible = self.inner.jobs.lock().await.get(&id).is_some_and(|entry| {
                matches!(entry.state, JobState::Failed | JobState::Interrupted)
                    && entry.resume.is_some()
                    && !entry.cancellation.is_cancelled()
            });
            if !eligible {
                continue;
            }
            match self.resume_locked(id, None, guard).await {
                Ok(()) => resumed += 1,
                // Cancellation can invalidate eligibility without the operation lock.
                Err(JobError::NotRunning(_) | JobError::Unknown(_)) => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(resumed)
    }

    /// Holds the operation guard through the handler launch, so a waiting parent
    /// sees the job running rather than a stale terminal delivery.
    async fn resume_locked(
        &self,
        id: JobId,
        value: Option<Value>,
        guard: OwnedMutexGuard<()>,
    ) -> Result<(), JobError> {
        // Accepted resumption owns its operation gate through mailbox/output
        // publication and worker handoff even when the sender disappears.
        let held = (guard, self.inner.supervision.enter());
        self.spawn_owned(held, "job resumption", move |manager| async move {
            let (agent, handler, suspended) = {
                let jobs = manager.inner.jobs.lock().await;
                let entry = jobs.get(&id).ok_or(JobError::Unknown(id))?;
                if !entry.accepts_input {
                    return Err(JobError::InputUnsupported(id));
                }
                if !matches!(
                    entry.state,
                    JobState::Completed | JobState::Failed | JobState::Interrupted
                ) || entry.cancellation.is_cancelled()
                {
                    return Err(JobError::NotRunning(id));
                }
                (
                    entry.agent.clone(),
                    entry.resume.clone().ok_or(JobError::NotRunning(id))?,
                    entry.suspended(),
                )
            };
            // Delivery reservation and its journal event are one ordered unit relative
            // to resumption, including batched background injection.
            let _delivery = manager.inner.delivery_operation.lock().await;
            manager
                .inner
                .store
                .append(
                    agent,
                    SessionEvent::JobStateChanged {
                        job: id,
                        state: JobState::Running,
                    },
                )
                .await?;
            // The running transition starts a new output generation, so old saved
            // results cannot masquerade as the new invocation's output.
            let (input, receiver) = mpsc::channel(JOB_INPUT_CAPACITY);
            let (notify, cancellation) = {
                let mut jobs = manager.inner.jobs.lock().await;
                let entry = jobs.get_mut(&id).ok_or(JobError::Unknown(id))?;
                entry.state = JobState::Running;
                entry.input = input;
                entry.clear_invocation_output();
                entry.pend_delivery();
                // An interrupted foreground invocation still has its original waiter.
                // Do not turn it into a background result merely because it resumed.
                if !suspended {
                    entry.background = true;
                }
                entry.task_abort = None;
                (entry.notify.clone(), entry.cancellation.clone())
            };
            let completion =
                super::supervisor::CompletionPermit::new(manager.clone(), id, cancellation);
            notify.notify_waiters();

            completion
                .start(
                    async move { handler(value, receiver).await },
                    "agent resume handler panicked",
                )
                .await?;
            notify.notify_waiters();
            Ok(())
        })
        .await
    }

    /// Suspend a running job until its caller supplies input. The payload is the
    /// externally visible question envelope. Already waiting jobs may refresh
    /// that envelope when their outstanding question set changes.
    pub async fn request_input(&self, id: JobId, output: Value) -> Result<(), JobError> {
        let operation = self.operation(id).await?.lock_owned().await;
        let held = (operation, self.inner.supervision.enter());
        self.spawn_owned(held, "job input publication", move |manager| async move {
            let _delivery = manager.inner.delivery_operation.lock().await;
            let agent = {
                let jobs = manager.inner.jobs.lock().await;
                let entry = jobs.get(&id).ok_or(JobError::Unknown(id))?;
                if !matches!(entry.state, JobState::Running | JobState::WaitingInput) {
                    return Err(JobError::InvalidTransition);
                }
                entry.agent.clone()
            };
            manager
                .inner
                .store
                .append(
                    agent.clone(),
                    SessionEvent::JobStateChanged {
                        job: id,
                        state: JobState::WaitingInput,
                    },
                )
                .await?;
            let notify = {
                let mut jobs = manager.inner.jobs.lock().await;
                let entry = jobs.get_mut(&id).ok_or(JobError::Unknown(id))?;
                entry.state = JobState::WaitingInput;
                entry.output = Some(output);
                entry.error = None;
                entry.pend_delivery();
                entry.background = true;
                entry.notify.clone()
            };
            notify.notify_waiters();
            let _ = manager
                .inner
                .completions
                .send(JobCompletion { agent, job: id });
            Ok(())
        })
        .await
    }

    pub async fn resume_input(&self, id: JobId) -> Result<(), JobError> {
        let operation = self.operation(id).await?.lock_owned().await;
        let held = (operation, self.inner.supervision.enter());
        self.spawn_owned(held, "job input publication", move |manager| async move {
            let _delivery = manager.inner.delivery_operation.lock().await;
            let agent = {
                let jobs = manager.inner.jobs.lock().await;
                let entry = jobs.get(&id).ok_or(JobError::Unknown(id))?;
                if entry.state != JobState::WaitingInput {
                    return Err(JobError::InvalidTransition);
                }
                entry.agent.clone()
            };
            manager
                .inner
                .store
                .append(
                    agent,
                    SessionEvent::JobStateChanged {
                        job: id,
                        state: JobState::Running,
                    },
                )
                .await?;
            let notify = {
                let mut jobs = manager.inner.jobs.lock().await;
                let entry = jobs.get_mut(&id).ok_or(JobError::Unknown(id))?;
                entry.state = JobState::Running;
                entry.output = None;
                entry.pend_delivery();
                entry.notify.clone()
            };
            notify.notify_waiters();
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn pending_child_names_reserve_in_order_without_creating_terminal_ghosts() {
        let runtime = crate::tests::TestRuntime::new().await;
        let jobs = &runtime.jobs;
        let launch = async |owner: &AgentId| {
            jobs.create(JobSpec {
                role: JobRole::Agent,
                name: Some("worker".into()),
                ..JobSpec::test(owner.clone(), "agent")
            })
            .await
            .unwrap()
            .into_test_id()
        };
        let first = launch(&runtime.agent).await;
        let second = launch(&runtime.agent).await;
        assert_eq!(
            jobs.child_name_owner(&runtime.agent, "worker", first).await,
            None
        );
        assert_eq!(
            jobs.child_name_owner(&runtime.agent, "worker", second)
                .await,
            Some(first)
        );

        jobs.finish(
            first,
            JobOutcome::Failed {
                message: "launch failed before installing a child".into(),
                output: None,
                denial: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            jobs.child_name_owner(&runtime.agent, "worker", second)
                .await,
            None
        );
        jobs.set_child_agent(second, runtime.agent.child(1))
            .await
            .unwrap();
        jobs.finish(
            second,
            JobOutcome::Completed(crate::tool::ToolOutput::new(Value::Null)),
        )
        .await
        .unwrap();
        let third = launch(&runtime.agent).await;
        assert_eq!(
            jobs.child_name_owner(&runtime.agent, "worker", third).await,
            Some(second)
        );
        let other = crate::session::fixture::start_child(
            &runtime.store,
            &runtime.agent,
            2,
            None,
            runtime.root.path(),
        )
        .await;
        let independent = launch(&other).await;
        assert_eq!(
            jobs.child_name_owner(&other, "worker", independent).await,
            None
        );
    }

    /// A running input-capable job whose resume handler echoes and counts calls.
    async fn retained(jobs: &JobManager, agent: &AgentId) -> (JobLease, Arc<AtomicUsize>) {
        let spec = JobSpec {
            accepts_input: true,
            ..JobSpec::test(agent.clone(), "agent")
        };
        let lease = jobs.test_lease(spec).await;
        jobs.transition(lease.id(), JobState::Running)
            .await
            .unwrap();
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

    async fn settled(jobs: &JobManager, id: JobId) -> JobEnvelope {
        jobs.wait(id, Some(Duration::from_secs(2)), true)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn session_retry_skips_children_changed_after_its_snapshot() {
        for state in [JobState::Running, JobState::Completed, JobState::Cancelled] {
            let (_root, jobs, owner) = super::super::tests::runtime().await;
            let mut candidates = Vec::new();
            // Distinct depths guarantee snapshot order A, B, C.
            for depth in (1..=3).rev() {
                let (lease, calls) = retained(&jobs, &owner).await;
                let child = (0..depth).fold(owner.clone(), |agent, _| agent.child(1));
                jobs.set_child_agent(lease.id(), child).await.unwrap();
                let failure = crate::tool::ToolError::Failed("fixture failure".into());
                jobs.finish(lease.id(), failure.into()).await.unwrap();
                candidates.push((lease.id(), calls));
            }
            let a = jobs.operation(candidates[0].0).await.unwrap();
            let guard = a.lock().await;
            let sweep = jobs.continue_resumable_children();
            tokio::pin!(sweep);
            // All other locks are free: one poll takes the snapshot and blocks at A.
            assert!(futures_util::poll!(&mut sweep).is_pending());
            let b = jobs.operation(candidates[1].0).await.unwrap();
            {
                let _guard = b.lock().await;
                let mut entries = jobs.inner.jobs.lock().await;
                let entry = entries.get_mut(&candidates[1].0).unwrap();
                entry.state = state;
                if state == JobState::Cancelled {
                    entry.cancellation.cancel();
                }
            }
            drop(guard);
            assert_eq!(sweep.await.unwrap(), 2, "{state:?}");
            for (index, (id, calls)) in candidates.iter().enumerate() {
                if index == 1 {
                    assert_eq!(jobs.snapshot(*id).await.unwrap().state, state);
                    assert_eq!(calls.load(Ordering::SeqCst), 0, "{state:?}");
                } else {
                    assert_eq!(settled(&jobs, *id).await.state, JobState::Completed);
                    assert_eq!(calls.load(Ordering::SeqCst), 1);
                }
            }
        }
    }

    /// An interrupted retained child resumes on send unless it was cancelled,
    /// which releases waiters and prevents resumption.
    #[tokio::test]
    async fn interrupted_retained_child_resumes_on_send_unless_cancelled() {
        for cancel in [false, true] {
            let (_root, jobs, agent) = super::super::tests::runtime().await;
            let (lease, calls) = retained(&jobs, &agent).await;
            jobs.finish(lease.id(), JobOutcome::Interrupted)
                .await
                .unwrap();
            let sent = if cancel {
                jobs.cancel(lease.id()).await.unwrap();
                assert_eq!(settled(&jobs, lease.id()).await.state, JobState::Cancelled);
                jobs.send(lease.id(), serde_json::json!("retry")).await
            } else {
                jobs.send(lease.id(), serde_json::json!("retry")).await
            };
            if cancel {
                assert!(matches!(sent, Err(JobError::NotRunning(_))));
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
        let (mut lease, _calls) = retained(&jobs, &agent).await;
        lease.take_input().close();
        let send = jobs.send(lease.id(), serde_json::json!("not lost"));
        tokio::pin!(send);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut send)
                .await
                .is_err()
        );
        jobs.test_finish(lease.id(), serde_json::Value::Null).await;
        tokio::time::timeout(Duration::from_secs(2), send)
            .await
            .unwrap()
            .unwrap();
        let result = settled(&jobs, lease.id()).await;
        assert_eq!(result.state, JobState::Completed);
        assert_eq!(result.output, Some(serde_json::json!("not lost")));
    }
}
