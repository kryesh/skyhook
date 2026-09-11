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

    pub async fn send(&self, id: JobId, mut value: Value) -> Result<(), JobError> {
        loop {
            // Serialize resumption against finishing and other senders. Once resumed,
            // subsequent sends use the new invocation's normal input mailbox.
            let operation = self.operation(id).await?;
            let guard = operation.lock().await;
            let (sender, resume) = {
                let jobs = self.inner.jobs.lock().await;
                let entry = jobs.get(&id).ok_or(JobError::Unknown(id))?;
                if !entry.accepts_input {
                    return Err(JobError::InputUnsupported(id));
                }
                if entry.state == JobState::Completed && !entry.cancellation.is_cancelled() {
                    let handler = entry.resume.clone().ok_or(JobError::NotRunning(id))?;
                    (entry.input.clone(), Some((entry.agent.clone(), handler)))
                } else if matches!(entry.state, JobState::Running | JobState::WaitingInput) {
                    (entry.input.clone(), None)
                } else {
                    return Err(JobError::NotRunning(id));
                }
            };
            if let Some((agent, handler)) = resume {
                // Delivery reservation and its journal event are one ordered unit
                // relative to resumption, including batched background injection.
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
                // Atomically replace the referenced document before removing stale
                // field files; replay must always find every JobFinished artifact.
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
                    entry.background = true;
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
                        Err(_) => crate::tool::ToolError::Failed(
                            "agent resume handler panicked".to_owned(),
                        )
                        .into(),
                    };
                    crate::tool::executor::persist_completion(&jobs, id, outcome).await;
                });
                notify.notify_waiters();
                return Ok(());
            }
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
            Arc::new(|value, _| Box::pin(async move { Ok(ToolOutput::new(value)) })),
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
