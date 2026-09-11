//! Job creation, state transitions, and terminal result supervision.

use super::*;

impl JobManager {
    pub async fn create(&self, mut spec: JobSpec) -> Result<JobLease, JobError> {
        let raw = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let id = JobId::new(raw).map_err(|error| JobError::Internal(error.to_string()))?;
        let created = self
            .inner
            .store
            .append(
                spec.agent.clone(),
                SessionEvent::JobCreated {
                    origin: spec.origin.clone(),
                    job: id,
                    parent: spec.parent,
                    tool: spec.tool.clone(),
                    name: spec.name.clone(),
                    arguments: std::mem::take(&mut spec.arguments),
                    output_schema: spec.output_schema.clone(),
                    accepts_input: spec.accepts_input,
                    background: spec.background,
                    location: spec.location.clone(),
                },
            )
            .await?;
        let (mut entry, input) = JobEntry::new(spec, created.timestamp_millis);
        let cancellation = {
            let mut jobs = self.inner.jobs.lock().await;
            if let Some(parent) = entry.parent {
                entry.cancellation = jobs
                    .get(&parent)
                    .ok_or(JobError::Unknown(parent))?
                    .cancellation
                    .child_token();
            }
            let cancellation = entry.cancellation.clone();
            jobs.insert(id, entry);
            cancellation
        };
        if cancellation.is_cancelled() {
            self.cancel(id).await?;
        }
        Ok(JobLease {
            id,
            cancellation,
            input,
        })
    }

    pub async fn transition(&self, id: JobId, state: JobState) -> Result<(), JobError> {
        if state.is_terminal() {
            return Err(JobError::InvalidTransition);
        }
        let operation = self.operation(id).await?;
        let _operation = operation.lock().await;
        let agent = {
            let jobs = self.inner.jobs.lock().await;
            let entry = jobs.get(&id).ok_or(JobError::Unknown(id))?;
            let valid = matches!(
                (entry.state, state),
                (
                    JobState::Queued,
                    JobState::AwaitingApproval | JobState::Running
                ) | (JobState::AwaitingApproval, JobState::Running)
            );
            if !valid {
                return Err(JobError::InvalidTransition);
            }
            entry.agent.clone()
        };
        self.inner
            .store
            .append(agent, SessionEvent::JobStateChanged { job: id, state })
            .await?;
        let notify = {
            let mut jobs = self.inner.jobs.lock().await;
            let entry = jobs.get_mut(&id).ok_or(JobError::Unknown(id))?;
            entry.state = state;
            entry.notify.clone()
        };
        notify.notify_waiters();
        Ok(())
    }

    pub(crate) async fn finish(&self, id: JobId, outcome: JobOutcome) -> Result<(), JobError> {
        let operation = self.operation(id).await?;
        let _operation = operation.lock().await;
        let _delivery = self.inner.delivery_operation.lock().await;
        let (agent, script) = {
            let jobs = self.inner.jobs.lock().await;
            let entry = jobs.get(&id).ok_or(JobError::Unknown(id))?;
            if entry.state.is_terminal() {
                return Err(JobError::AlreadyTerminal(id));
            }
            (entry.agent.clone(), entry.tool == "script")
        };
        let (state, output, error, denial) = match outcome {
            JobOutcome::Completed(output) => (JobState::Completed, Some(output), None, None),
            JobOutcome::Failed {
                message,
                output,
                denial,
            } => (JobState::Failed, output, Some(message), denial),
            JobOutcome::Cancelled => (
                JobState::Cancelled,
                None,
                Some("tool was cancelled".to_owned()),
                None,
            ),
            JobOutcome::Interrupted => (
                JobState::Interrupted,
                None,
                Some("interrupted while the session was not running".to_owned()),
                None,
            ),
        };
        let (mut output, images) = output.map_or((None, Vec::new()), |output| {
            (Some(output.value), output.images)
        });
        let directory = self.output_directory(id);
        let capture_complete = output
            .as_ref()
            .is_some_and(|value| value.get("timed_out") != Some(&Value::Bool(true)));
        if output.is_none() && script {
            output = Some(serde_json::json!({"value":null,"console":""}));
        }
        if output.is_none() {
            let mut partial = serde_json::Map::new();
            for field in ["stdout", "stderr"] {
                if output::field_file(&directory, &format!("/result/{field}")).exists() {
                    partial.insert(field.to_owned(), Value::String(String::new()));
                }
            }
            if !partial.is_empty() {
                output = Some(Value::Object(partial));
            }
        }
        let document = serde_json::json!({"capture_complete":capture_complete, "result":output, "error":error});
        tokio::task::spawn_blocking(move || output::save(&directory, &document))
            .await
            .map_err(|e| JobError::Internal(e.to_string()))?
            .map_err(SessionError::from)?;
        let output_path = Some(
            std::path::PathBuf::from("jobs")
                .join(id.to_string())
                .join("document.json"),
        );
        self.inner
            .store
            .append(
                agent.clone(),
                SessionEvent::JobFinished {
                    job: id,
                    state,
                    output_path,
                    error: error.clone(),
                    images: images.clone(),
                    denial: denial.clone(),
                },
            )
            .await?;
        let (notify, background) = {
            let mut jobs = self.inner.jobs.lock().await;
            let entry = jobs.get_mut(&id).ok_or(JobError::Unknown(id))?;
            if entry.state.is_terminal() {
                return Err(JobError::AlreadyTerminal(id));
            }
            entry.state = state;
            // A terminal outcome is a new delivery even if an earlier question
            // was claimed or injected without returning through resume_input.
            entry.delivery = DeliveryState::Pending;
            if state != JobState::Completed {
                entry.resume = None;
            }
            entry.output = None;
            entry.images = images;
            entry.error = error;
            entry.denial = denial;
            (entry.notify.clone(), entry.background)
        };
        notify.notify_waiters();
        if background {
            let _ = self
                .inner
                .completions
                .send(JobCompletion { agent, job: id });
        }
        Ok(())
    }

    pub(crate) async fn operation(&self, id: JobId) -> Result<Arc<Mutex<()>>, JobError> {
        self.inner
            .jobs
            .lock()
            .await
            .get(&id)
            .map(|entry| entry.operation.clone())
            .ok_or(JobError::Unknown(id))
    }

    pub(crate) async fn attach_task(
        &self,
        id: JobId,
        task_abort: AbortHandle,
    ) -> Result<(), JobError> {
        let mut jobs = self.inner.jobs.lock().await;
        let entry = jobs.get_mut(&id).ok_or(JobError::Unknown(id))?;
        if entry.state.is_terminal() {
            task_abort.abort();
            return Err(JobError::AlreadyTerminal(id));
        }
        entry.task_abort = Some(task_abort);
        Ok(())
    }

    /// Preserve a live, observable terminal result when durable finalization is unavailable.
    pub(crate) async fn fail_volatile(&self, id: JobId, error: String) {
        let _delivery = self.inner.delivery_operation.lock().await;
        let terminal = {
            let mut jobs = self.inner.jobs.lock().await;
            let Some(entry) = jobs.get_mut(&id) else {
                return;
            };
            if entry.state.is_terminal() {
                return;
            }
            entry.state = JobState::Failed;
            entry.delivery = DeliveryState::Pending;
            entry.output = None;
            entry.images.clear();
            entry.error = Some(error);
            Some((entry.agent.clone(), entry.notify.clone(), entry.background))
        };
        if let Some((agent, notify, background)) = terminal {
            notify.notify_waiters();
            if background {
                let _ = self
                    .inner
                    .completions
                    .send(JobCompletion { agent, job: id });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::TestRuntime;
    use crate::tool::{ToolOptions, ToolRegistryBuilder, executor::ToolExecutor, policy::AllowAll};
    use tokio::sync::Semaphore;
    #[derive(Deserialize, JsonSchema)]
    struct NoArgs {}

    #[tokio::test]
    async fn handler_panics_are_supervised_as_failed_jobs() {
        let runtime = TestRuntime::new().await;
        let agent = runtime.agent.clone();
        let mut builder = ToolRegistryBuilder::default();
        builder
            .register::<NoArgs, String, _, _>(
                "panic",
                "panic",
                ToolOptions::new(Vec::new()).background(),
                |_context, _input| async move {
                    panic!("handler panic");
                },
            )
            .unwrap();
        let jobs = runtime.jobs.clone();
        let executor = runtime.executor(builder);
        let running = executor
            .execute(agent, "panic", serde_json::json!({"bg": true}), None)
            .await
            .unwrap();
        let failed =
            tokio::time::timeout(Duration::from_secs(1), jobs.wait(running.job, None, true))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(failed.state, JobState::Failed);
        assert_eq!(failed.error.as_deref(), Some("tool handler panicked"));
    }

    #[tokio::test]
    async fn finalization_failure_wakes_waiters_with_a_volatile_failure() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::create(root.path()).await.unwrap();
        let session_directory = store.directory().to_path_buf();
        let agent = AgentId::root(store.id());
        let release = Arc::new(Semaphore::new(0));
        let mut builder = ToolRegistryBuilder::default();
        builder
            .register::<NoArgs, String, _, _>(
                "blocked",
                "wait before completing",
                ToolOptions::new(Vec::new()).background(),
                {
                    let release = release.clone();
                    move |_context, _input| {
                        let release = release.clone();
                        async move {
                            release.acquire().await.unwrap().forget();
                            Ok("done".to_owned())
                        }
                    }
                },
            )
            .unwrap();
        let jobs = JobManager::new(store);
        let executor = ToolExecutor::new(
            builder.build(),
            Arc::new(AllowAll),
            jobs.clone(),
            root.path().to_path_buf(),
        );
        let running = executor
            .execute(agent, "blocked", serde_json::json!({"bg": true}), None)
            .await
            .unwrap();
        tokio::fs::write(
            session_directory.join("jobs").join(running.job.to_string()),
            b"not a directory",
        )
        .await
        .unwrap();
        release.add_permits(1);

        let failed =
            tokio::time::timeout(Duration::from_secs(2), jobs.wait(running.job, None, true))
                .await
                .expect("persistence failure left the job waiting")
                .unwrap();
        assert_eq!(failed.state, JobState::Failed);
        assert!(
            failed
                .error
                .as_deref()
                .is_some_and(|error| error.contains("could not be persisted"))
        );
    }
}
