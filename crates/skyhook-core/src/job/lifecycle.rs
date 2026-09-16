//! Job creation, state transitions, and terminal result supervision.

use super::*;

impl JobManager {
    /// After stopping creation producers, wait for admitted creation owners to
    /// finish map publication and any abandoned-lease finalization. The session
    /// writer drain alone only covers append, not this manager's publication.
    pub(crate) async fn drain_creations(&self) {
        let _operation = self.inner.creation_operation.write().await;
        // Creation publication is complete; every admitted per-job owner now
        // retains its operation gate through durable append AND map publication.
        let operations = self
            .inner
            .jobs
            .lock()
            .await
            .values()
            .map(|entry| entry.operation.clone())
            .collect::<Vec<_>>();
        for operation in operations {
            let _settled = operation.lock().await;
        }
        let _deliveries = self.inner.delivery_operation.lock().await;
    }

    pub async fn create(&self, spec: JobSpec) -> Result<JobLease, JobError> {
        if spec.agent.session() != self.inner.store.id() {
            return Err(SessionError::WrongSession.into());
        }
        // Cancellation while waiting for admission has no effect. Once the
        // operation owns this gate, it owns accepted append through publication.
        // A lease abandoned with the detached owner's result is cancelled and
        // finalized by its completion permit's Drop.
        let operation = self.inner.creation_operation.clone().read_owned().await;
        self.spawn_owned(operation, "creation", move |manager| async move {
            manager.create_owned(spec).await
        })
        .await
    }

    async fn create_owned(&self, mut spec: JobSpec) -> Result<JobLease, JobError> {
        // Pruning takes the same operation gate. Do not hold the map lock across
        // append: cancellation must still be able to reach the parent token.
        if let Some(parent) = spec.parent
            && !self.inner.jobs.lock().await.contains_key(&parent)
        {
            return Err(JobError::Unknown(parent));
        }
        let raw = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let id = JobId::new(raw).map_err(|error| JobError::Internal(error.to_string()))?;
        let created = self
            .inner
            .store
            .accept_append(
                spec.agent.clone(),
                SessionEvent::JobCreated {
                    origin: spec.origin.clone(),
                    job: id,
                    parent: spec.parent,
                    tool: spec.tool.clone(),
                    role: spec.role,
                    name: spec.name.clone(),
                    arguments: std::mem::take(&mut spec.arguments),
                    output_schema: spec.output_schema.clone(),
                    accepts_input: spec.accepts_input,
                    background: spec.background,
                    authorization_scope: spec.authorization_scope,
                    location: spec.location.clone(),
                },
            )
            .await?
            .committed()
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
        Ok(JobLease::new(self.clone(), id, cancellation, input))
    }

    /// Run accepted publication work on a detached owner holding a manager
    /// clone and `held` (its gates/permits), so caller cancellation cannot
    /// abandon it. The clone is released before `held`: a gate is also the
    /// drain receipt, and a drained session must be closable afterwards.
    pub(super) async fn spawn_owned<T, F, Fut>(
        &self,
        held: impl Send + 'static,
        owner: &'static str,
        work: F,
    ) -> Result<T, JobError>
    where
        T: Send + 'static,
        F: FnOnce(JobManager) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<T, JobError>> + Send + 'static,
    {
        let manager = self.clone();
        tokio::spawn(async move {
            let result = work(manager).await;
            drop(held);
            result
        })
        .await
        .map_err(|error| JobError::Internal(format!("{owner} owner lost: {error}")))?
    }

    pub async fn transition(&self, id: JobId, state: JobState) -> Result<(), JobError> {
        if state.is_terminal() {
            return Err(JobError::InvalidTransition);
        }
        let operation = self.operation(id).await?.lock_owned().await;
        self.spawn_owned(
            operation,
            "transition publication",
            move |manager| async move {
                let agent = {
                    let jobs = manager.inner.jobs.lock().await;
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
                manager
                    .inner
                    .store
                    .append(agent, SessionEvent::JobStateChanged { job: id, state })
                    .await?;
                let notify = {
                    let mut jobs = manager.inner.jobs.lock().await;
                    let entry = jobs.get_mut(&id).ok_or(JobError::Unknown(id))?;
                    entry.state = state;
                    entry.notify.clone()
                };
                notify.notify_waiters();
                Ok(())
            },
        )
        .await
    }

    pub(crate) async fn finish(&self, id: JobId, outcome: JobOutcome) -> Result<(), JobError> {
        let operation = self.operation(id).await?.lock_owned().await;
        self.spawn_owned(operation, "finalization publication", move |manager| async move {
            let _delivery = manager.inner.delivery_operation.lock().await;
            let agent = {
                let jobs = manager.inner.jobs.lock().await;
                let entry = jobs.get(&id).ok_or(JobError::Unknown(id))?;
                if entry.state.is_terminal()
                    && !(entry.state == JobState::Interrupted
                        && matches!(outcome, JobOutcome::Cancelled))
                {
                    return Err(JobError::AlreadyTerminal(id));
                }
                entry.agent.clone()
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
            let capture_complete = output
                .as_ref()
                .is_some_and(|output| output.streams == crate::tool::StreamEnd::Finished);
            let (output, images, captures) =
                output.map_or((None, Vec::new(), Vec::new()), |mut output| {
                    let captures = output.take_captures();
                    (Some(output.value), output.images, captures)
                });
            let saved = manager.output(id);
            // Captures have independent pointer/type metadata. A failed tool need not
            // produce a result, and unfinished JSON captures are not valid result trees.
            let document = serde_json::json!({"capture_complete":capture_complete, "result":output, "error":error});
            tokio::task::spawn_blocking(move || {
                output::save_completed(&saved, &document, captures)
            })
            .await
            .map_err(|e| JobError::Internal(e.to_string()))?
            .map_err(|e| JobError::Internal(e.to_string()))?;
            manager.inner
                .store
                .append(
                    agent.clone(),
                    SessionEvent::JobFinished {
                        job: id,
                        state,
                        error: error.clone(),
                        images: images.clone(),
                        denial: denial.clone(),
                    },
                )
                .await?;
            let (notify, background) = {
                let mut jobs = manager.inner.jobs.lock().await;
                let entry = jobs.get_mut(&id).ok_or(JobError::Unknown(id))?;
                if entry.state.is_terminal()
                    && !(entry.state == JobState::Interrupted && state == JobState::Cancelled)
                {
                    return Err(JobError::AlreadyTerminal(id));
                }
                entry.apply_finished(state, images, error, denial);
                (entry.notify.clone(), entry.background)
            };
            notify.notify_waiters();
            if background {
                let _ = manager
                    .inner
                    .completions
                    .send(JobCompletion { agent, job: id });
            }
            Ok(())
        })
        .await
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
            // Preserve the current denial metadata on this volatile fallback,
            // just as before; persistence failure does not reclassify authority.
            entry.apply_finished(
                JobState::Failed,
                Vec::new(),
                Some(error),
                entry.denial.clone(),
            );
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
    use crate::tool::{ToolOptions, ToolRegistryBuilder};
    use tokio::sync::Semaphore;

    #[derive(Deserialize, JsonSchema)]
    struct NoArgs {}

    #[tokio::test]
    async fn invalid_create_preflight_does_not_append_or_allocate() {
        let (_root, jobs, agent) = super::super::tests::runtime().await;
        let before = jobs.store().records().await.len();
        let unknown = JobId::new(99).unwrap();
        let spec = JobSpec {
            parent: Some(unknown),
            ..JobSpec::test(agent.clone(), "invalid-parent")
        };
        assert!(matches!(jobs.create(spec).await, Err(JobError::Unknown(id)) if id == unknown));
        let other = AgentId::root(crate::identity::SessionId::generate().unwrap());
        assert!(matches!(
            jobs.create(JobSpec::test(other, "wrong-session")).await,
            Err(JobError::Session(SessionError::WrongSession))
        ));
        assert_eq!(jobs.store().records().await.len(), before);
        assert!(jobs.inner.jobs.lock().await.is_empty());
        assert_eq!(
            jobs.test_create(JobSpec::test(agent, "valid")).await.get(),
            1
        );
    }

    /// A capture that cannot be admitted (invalid UTF-8 or unreadable) is
    /// published as incomplete; it never turns a completed job into a volatile,
    /// unjournaled failure.
    #[tokio::test]
    async fn invalid_or_unreadable_text_captures_still_journal_completion() {
        use crate::job::output::{CaptureKind, PendingCapture};
        use std::io::Write as _;
        for unreadable in [false, true] {
            let (_root, jobs, agent) = super::super::tests::runtime().await;
            let id = jobs.test_create(JobSpec::test(agent, "capture")).await;
            let output = jobs.output(id);
            let field = "/result/stdout";
            let mut writer = PendingCapture::create(&output, field, CaptureKind::Text)
                .unwrap()
                .open();
            writer.write_all(b"partial \xff output").unwrap();
            let capture = writer.finish().unwrap();
            if unreadable {
                output.test_delete_capture(&capture);
            }
            let product =
                ToolOutput::new(serde_json::json!({"exit_code": 0})).with_captures(vec![capture]);
            jobs.finish(id, JobOutcome::Completed(product))
                .await
                .unwrap();
            assert_eq!(jobs.snapshot(id).await.unwrap().state, JobState::Completed);
            let journaled = jobs.store().records().await.into_iter().any(|record| {
                matches!(
                    record.event,
                    SessionEvent::JobFinished { job, state: JobState::Completed, .. } if job == id
                )
            });
            assert!(journaled, "unreadable: {unreadable}");
            let replay = jobs.test_replay().await;
            let replayed = replay.snapshot(id).await.unwrap();
            assert_eq!(replayed.state, JobState::Completed);
            assert_eq!(replayed.output, Some(serde_json::json!({"exit_code": 0})));
        }
    }

    /// Handler panics and finalization persistence failures both wake waiters
    /// with failed jobs rather than leaving them running.
    #[tokio::test]
    async fn handler_panics_and_finalization_failures_are_supervised_as_failed_jobs() {
        let runtime = TestRuntime::new().await;
        let release = Arc::new(Semaphore::new(0));
        let mut builder = ToolRegistryBuilder::default();
        let options = ToolOptions::new(Vec::new()).background();
        builder
            .register::<NoArgs, String, _, _>(
                "panic",
                "panic",
                options.clone(),
                |_, _| async move {
                    panic!("handler panic");
                },
            )
            .unwrap()
            .register::<NoArgs, String, _, _>("blocked", "wait before completing", options, {
                let release = release.clone();
                move |_context, _input| {
                    let release = release.clone();
                    async move {
                        release.acquire().await.unwrap().forget();
                        Ok("done".to_owned())
                    }
                }
            })
            .unwrap();
        let (jobs, executor) = (runtime.jobs.clone(), runtime.executor(builder));
        let background = serde_json::json!({"bg": true});
        for (tool, expected) in [
            ("panic", "tool handler panicked"),
            ("blocked", "could not be persisted"),
        ] {
            let running = executor
                .run_host(&runtime.agent, tool, background.clone())
                .await
                .unwrap();
            if tool == "blocked" {
                jobs.store().outputs().test_batch(
                    "CREATE TEMP TRIGGER reject_output BEFORE INSERT ON job_output \
                     BEGIN SELECT RAISE(ABORT, 'injected output fault'); END;",
                );
                release.add_permits(1);
            }
            let wait =
                tokio::time::timeout(Duration::from_secs(2), jobs.wait(running.job, None, true));
            let failed = wait.await.expect("failure left the job waiting").unwrap();
            assert_eq!(failed.state, JobState::Failed);
            assert!(
                failed
                    .error
                    .as_deref()
                    .is_some_and(|error| error.contains(expected)),
                "{tool}"
            );
        }
    }
}
