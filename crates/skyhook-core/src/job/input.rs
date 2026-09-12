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
            // Serialize resumption against finishing and other senders. Once resumed,
            // subsequent sends use the new invocation's normal input mailbox.
            let operation = self.operation(id).await?;
            let guard = operation.lock().await;
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
            let guard = operation.lock().await;
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

    /// The operation guard is deliberately held across the state journal and the
    /// handler launch, so a parent that is waiting sees the job as running rather
    /// than receiving a stale terminal delivery.
    async fn resume_locked(
        &self,
        id: JobId,
        value: Option<Value>,
        _guard: tokio::sync::MutexGuard<'_, ()>,
    ) -> Result<(), JobError> {
        let (agent, handler, suspended) = {
            let jobs = self.inner.jobs.lock().await;
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
        let _delivery = self.inner.delivery_operation.lock().await;
        self.inner
            .store
            .append(
                agent,
                SessionEvent::JobStateChanged {
                    job: id,
                    state: JobState::Running,
                },
            )
            .await?;
        // Old saved results must not masquerade as the new invocation's output.
        let directory = self.output_directory(id);
        output::save(&directory, &serde_json::json!({"result":null}))
            .map_err(SessionError::from)?;
        let mut files = tokio::fs::read_dir(&directory)
            .await
            .map_err(SessionError::from)?;
        while let Some(file) = files.next_entry().await.map_err(SessionError::from)? {
            if file.file_name().to_string_lossy().starts_with("field-") {
                tokio::fs::remove_file(file.path())
                    .await
                    .map_err(SessionError::from)?;
            }
        }
        let (input, receiver) = mpsc::channel(JOB_INPUT_CAPACITY);
        let notify = {
            let mut jobs = self.inner.jobs.lock().await;
            let entry = jobs.get_mut(&id).ok_or(JobError::Unknown(id))?;
            entry.state = JobState::Running;
            entry.input = input;
            entry.output = None;
            entry.images.clear();
            entry.error = None;
            entry.denial = None;
            entry.delivery = DeliveryState::Pending;
            // An interrupted foreground invocation still has its original waiter.
            // Do not turn it into a background result merely because it resumed.
            if !suspended {
                entry.background = true;
            }
            entry.task_abort = None;
            entry.notify.clone()
        };
        let worker = tokio::spawn(async move { handler(value, receiver).await });
        self.attach_task(id, worker.abort_handle()).await?;
        let jobs = self.clone();
        tokio::spawn(async move {
            let outcome = match worker.await {
                Ok(Ok(output)) => JobOutcome::Completed(output),
                Ok(Err(error)) => error.into(),
                Err(error) if error.is_cancelled() => JobOutcome::Cancelled,
                Err(_) => {
                    crate::tool::ToolError::Failed("agent resume handler panicked".to_owned())
                        .into()
                }
            };
            crate::tool::executor::persist_completion(&jobs, id, outcome).await;
        });
        notify.notify_waiters();
        Ok(())
    }

    /// Suspend a running job until its caller supplies input. The payload is the
    /// externally visible question envelope. Already waiting jobs may refresh
    /// that envelope when their outstanding question set changes.
    pub async fn request_input(&self, id: JobId, output: Value) -> Result<(), JobError> {
        let operation = self.operation(id).await?;
        let _operation = operation.lock().await;
        let _delivery = self.inner.delivery_operation.lock().await;
        let agent = {
            let jobs = self.inner.jobs.lock().await;
            let entry = jobs.get(&id).ok_or(JobError::Unknown(id))?;
            if !matches!(entry.state, JobState::Running | JobState::WaitingInput) {
                return Err(JobError::InvalidTransition);
            }
            entry.agent.clone()
        };
        self.inner
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
            let mut jobs = self.inner.jobs.lock().await;
            let entry = jobs.get_mut(&id).ok_or(JobError::Unknown(id))?;
            entry.state = JobState::WaitingInput;
            entry.output = Some(output);
            entry.error = None;
            entry.delivery = DeliveryState::Pending;
            entry.background = true;
            entry.notify.clone()
        };
        notify.notify_waiters();
        let _ = self
            .inner
            .completions
            .send(JobCompletion { agent, job: id });
        Ok(())
    }

    pub async fn resume_input(&self, id: JobId) -> Result<(), JobError> {
        let operation = self.operation(id).await?;
        let _operation = operation.lock().await;
        let _delivery = self.inner.delivery_operation.lock().await;
        let agent = {
            let jobs = self.inner.jobs.lock().await;
            let entry = jobs.get(&id).ok_or(JobError::Unknown(id))?;
            if entry.state != JobState::WaitingInput {
                return Err(JobError::InvalidTransition);
            }
            entry.agent.clone()
        };
        self.inner
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
            let mut jobs = self.inner.jobs.lock().await;
            let entry = jobs.get_mut(&id).ok_or(JobError::Unknown(id))?;
            entry.state = JobState::Running;
            entry.output = None;
            entry.delivery = DeliveryState::Pending;
            entry.notify.clone()
        };
        notify.notify_waiters();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn session_retry_skips_children_changed_after_its_snapshot() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        for state in [JobState::Running, JobState::Completed, JobState::Cancelled] {
            let (_root, jobs, owner) = super::super::tests::runtime().await;
            let mut candidates = Vec::new();
            // Distinct depths guarantee snapshot order A, B, C.
            for depth in (1..=3).rev() {
                let lease = jobs
                    .test_lease(JobSpec {
                        accepts_input: true,
                        ..JobSpec::test(owner.clone(), "agent")
                    })
                    .await;
                let mut child = owner.clone();
                for _ in 0..depth {
                    child = child.child(1);
                }
                jobs.set_child_agent(lease.id, child).await.unwrap();
                let calls = Arc::new(AtomicUsize::new(0));
                let invoked = calls.clone();
                jobs.set_resume_handler(
                    lease.id,
                    Arc::new(move |_, _| {
                        invoked.fetch_add(1, Ordering::SeqCst);
                        Box::pin(async { Ok(ToolOutput::new(serde_json::json!("resumed"))) })
                    }),
                )
                .await
                .unwrap();
                jobs.finish(
                    lease.id,
                    crate::tool::ToolError::Failed("fixture failure".into()).into(),
                )
                .await
                .unwrap();
                candidates.push((lease.id, calls));
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
                    assert_eq!(
                        jobs.wait(*id, Some(Duration::from_secs(2)), true)
                            .await
                            .unwrap()
                            .state,
                        JobState::Completed
                    );
                    assert_eq!(calls.load(Ordering::SeqCst), 1);
                }
            }
        }
    }

    #[tokio::test]
    async fn cancelling_a_suspended_child_releases_waiters_and_prevents_resumption() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::create(root.path()).await.unwrap();
        let jobs = JobManager::new(store.clone());
        let lease = jobs
            .test_lease(JobSpec {
                accepts_input: true,
                ..JobSpec::test(AgentId::root(store.id()), "agent")
            })
            .await;
        jobs.transition(lease.id, JobState::Running).await.unwrap();
        jobs.set_resume_handler(
            lease.id,
            Arc::new(|_, _| Box::pin(async { panic!("cancelled child must never resume") })),
        )
        .await
        .unwrap();
        jobs.finish(lease.id, JobOutcome::Interrupted)
            .await
            .unwrap();
        jobs.cancel(lease.id).await.unwrap();
        let result = jobs
            .wait(lease.id, Some(Duration::from_secs(2)), true)
            .await
            .unwrap();
        assert_eq!(result.state, JobState::Cancelled);
        assert!(matches!(
            jobs.send(lease.id, serde_json::json!("resume")).await,
            Err(JobError::NotRunning(_))
        ));
    }

    #[tokio::test]
    async fn interrupted_retained_child_resumes_on_send() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::create(root.path()).await.unwrap();
        let jobs = JobManager::new(store.clone());
        let lease = jobs
            .test_lease(JobSpec {
                accepts_input: true,
                ..JobSpec::test(AgentId::root(store.id()), "agent")
            })
            .await;
        jobs.transition(lease.id, JobState::Running).await.unwrap();
        jobs.set_resume_handler(
            lease.id,
            Arc::new(|value, _| {
                Box::pin(async move {
                    Ok(ToolOutput::new(
                        value.expect("job_send supplies parent input"),
                    ))
                })
            }),
        )
        .await
        .unwrap();
        jobs.finish(lease.id, JobOutcome::Interrupted)
            .await
            .unwrap();
        jobs.send(lease.id, serde_json::json!("retry"))
            .await
            .unwrap();
        let result = jobs
            .wait(lease.id, Some(Duration::from_secs(2)), true)
            .await
            .unwrap();
        assert_eq!(result.state, JobState::Completed);
        assert_eq!(result.output, Some(serde_json::json!("retry")));
    }

    #[tokio::test]
    async fn send_retries_when_child_mailbox_closes_before_completion() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::create(root.path()).await.unwrap();
        let jobs = JobManager::new(store.clone());
        let mut agent = jobs
            .test_lease(JobSpec {
                accepts_input: true,
                ..JobSpec::test(AgentId::root(store.id()), "agent")
            })
            .await;
        jobs.transition(agent.id, JobState::Running).await.unwrap();
        jobs.set_resume_handler(
            agent.id,
            Arc::new(|value, _| {
                Box::pin(async move { Ok(ToolOutput::new(value.expect("send supplies a value"))) })
            }),
        )
        .await
        .unwrap();
        agent.input.close();
        let send = jobs.send(agent.id, serde_json::json!("not lost"));
        tokio::pin!(send);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut send)
                .await
                .is_err()
        );
        jobs.test_finish(agent.id, serde_json::Value::Null).await;
        tokio::time::timeout(Duration::from_secs(2), send)
            .await
            .unwrap()
            .unwrap();
        let result = jobs
            .wait(agent.id, Some(Duration::from_secs(2)), true)
            .await
            .unwrap();
        assert_eq!(result.state, JobState::Completed);
        assert_eq!(result.output, Some(serde_json::json!("not lost")));
    }
}
