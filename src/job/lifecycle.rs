//! Job creation, state transitions, and terminal result supervision.

use super::*;

/// What a journaled change hands back: the job's agent and whether its
/// completions wake an owner.
pub(super) struct Applied {
    pub(super) agent: AgentId,
    pub(super) background: bool,
}

impl JobManager {
    /// After stopping creation producers, wait for admitted creation owners to
    /// finish map publication and any abandoned-lease finalization.
    pub(crate) async fn drain_creations(&self) {
        let _operation = self.inner.creation_operation.write().await;
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
        // Cancelling the caller before admission has no effect; after it, the
        // detached owner still publishes, and the abandoned lease is cancelled.
        let operation = self.inner.creation_operation.clone().read_owned().await;
        self.spawn_owned(operation, "creation", move |manager| async move {
            manager.create_owned(spec).await
        })
        .await
    }

    async fn create_owned(&self, mut spec: JobSpec) -> Result<JobLease, JobError> {
        // Do not hold the map lock across append: cancellation must still be
        // able to reach the parent token.
        if let Some(parent) = spec.parent
            && !self.inner.jobs.lock().await.contains_key(&parent)
        {
            return Err(JobError::Unknown(parent));
        }
        let raw = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let id = JobId::new(raw).map_err(|_| JobError::InvalidId)?;
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

    /// Run publication work on a detached owner holding `held` (its gates and
    /// permits), so caller cancellation cannot abandon it.
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
        .map_err(|error| JobError::OwnerLost {
            owner,
            reason: error.to_string(),
        })?
    }

    /// Journal one change to a job and apply it, in the order every change keeps
    /// under the job's operation lock: refused before the append when the phase
    /// does not admit its step, applied and announced after. `event` is what the
    /// journal records; a volatile change records nothing. `settle` runs on the
    /// entry with the change applied.
    pub(super) async fn journal_change<T>(
        &self,
        id: JobId,
        change: JobChange,
        event: Option<SessionEvent>,
        settle: impl FnOnce(&mut JobEntry) -> T,
    ) -> Result<(Applied, T), JobError> {
        let agent = {
            let jobs = self.inner.jobs.lock().await;
            let entry = jobs.get(&id).ok_or(JobError::Unknown(id))?;
            if !entry.admits(change.step()) {
                return Err(JobError::AlreadyTerminal(id));
            }
            entry.agent.clone()
        };
        if let Some(event) = event {
            self.inner.store.append(agent.clone(), event).await?;
        }
        let (notify, background, settled) = {
            let mut jobs = self.inner.jobs.lock().await;
            let entry = jobs.get_mut(&id).ok_or(JobError::Unknown(id))?;
            entry
                .apply(change)
                .map_err(|rejected| rejected.journaled(id))?;
            (entry.notify.clone(), entry.background, settle(entry))
        };
        notify.notify_waiters();
        Ok((Applied { agent, background }, settled))
    }

    /// A lease's startup transition. Its stage guarantees the order, so the only
    /// phase that refuses is a job finished underneath it.
    pub(super) async fn advance(&self, id: JobId, state: JobTransition) -> Result<(), JobError> {
        let operation = self.operation(id).await?.lock_owned().await;
        self.spawn_owned(
            operation,
            "transition publication",
            move |manager| async move {
                let event = SessionEvent::JobStateChanged { job: id, state };
                manager
                    .journal_change(id, JobChange::Advance(state), Some(event), |_| ())
                    .await?;
                Ok(())
            },
        )
        .await
    }

    pub(crate) async fn finish(&self, id: JobId, outcome: JobOutcome) -> Result<(), JobError> {
        let operation = self.operation(id).await?.lock_owned().await;
        self.spawn_owned(
            operation,
            "finalization publication",
            move |manager| async move {
                let _delivery = manager.inner.delivery_operation.lock().await;
                let end = outcome.end();
                let (output, mut diagnostic) = match outcome {
                    JobOutcome::Completed(output) => (Some(output), None),
                    JobOutcome::Failed { diagnostic, output } => (output, Some(diagnostic)),
                };
                let capture_complete = output
                    .as_ref()
                    .is_some_and(|output| output.streams == crate::tool::StreamEnd::Finished);
                let (mut output, images, captures, output_diagnostic) =
                    output.map_or((None, Vec::new(), Vec::new(), None), |mut output| {
                        let captures = output.take_captures();
                        (
                            Some(output.value),
                            output.images,
                            captures,
                            output.diagnostic,
                        )
                    });
                // Diagnostic facts, not an early capability-specific rendering, are
                // authoritative. Saved documents keep only the registered slot.
                if output_diagnostic.is_some()
                    && let Some(message) = output
                        .as_mut()
                        .and_then(|output| output.pointer_mut("/error/message"))
                {
                    *message = Value::Null;
                }
                let mut finished = Box::new(Finished {
                    end,
                    images,
                    diagnostic: None,
                    output_diagnostic: output_diagnostic.map(PartialDiagnostic::resolve),
                });
                let previous = {
                    let jobs = manager.inner.jobs.lock().await;
                    let entry = jobs.get(&id).ok_or(JobError::Unknown(id))?;
                    if !entry.admits(JobStep::Finish(end)) {
                        return Err(JobError::AlreadyTerminal(id));
                    }
                    entry.finished().cloned()
                };
                if let Some(previous) = previous {
                    // Cancelling an interruption ends resumability, not its published
                    // output. Leave the saved document and capture references untouched;
                    // hydrating and resaving them would lose completeness and may be huge.
                    if let Some(interrupted) = previous.diagnostic {
                        diagnostic = diagnostic.map(|d| d.or(interrupted.context.into()));
                    }
                    finished.images = previous.images;
                    finished.output_diagnostic = previous.output_diagnostic;
                } else {
                    let saved = manager.output(id);
                    // Captures have independent pointer/type metadata. A failed tool need not
                    // produce a result, and unfinished JSON captures are not valid result trees.
                    output::blocking(move || {
                        output::save_completed(&saved, output, capture_complete, captures)
                    })
                    .await
                    .map_err(|error| JobError::Output(Box::new(error)))?;
                }
                finished.diagnostic = diagnostic.map(PartialDiagnostic::resolve);
                let event = SessionEvent::JobFinished {
                    job: id,
                    state: end,
                    diagnostic: finished.diagnostic.clone(),
                    output_diagnostic: finished.output_diagnostic.clone(),
                    images: finished.images.clone(),
                };
                let (applied, ()) = manager
                    .journal_change(id, JobChange::Finish(finished), Some(event), |_| ())
                    .await?;
                if applied.background {
                    let _ = manager.inner.completions.send(JobCompletion {
                        agent: applied.agent,
                        job: id,
                    });
                }
                Ok(())
            },
        )
        .await
    }

    pub(crate) async fn operation(&self, id: JobId) -> Result<Arc<Mutex<()>>, JobError> {
        self.entry(id, |entry| entry.operation.clone()).await
    }

    pub(crate) async fn attach_task(
        &self,
        id: JobId,
        task_abort: AbortHandle,
    ) -> Result<(), JobError> {
        let mut jobs = self.inner.jobs.lock().await;
        let entry = jobs.get_mut(&id).ok_or(JobError::Unknown(id))?;
        if entry.end().is_some() {
            task_abort.abort();
            return Err(JobError::AlreadyTerminal(id));
        }
        entry.task_abort = Some(task_abort);
        Ok(())
    }

    /// Preserve a live, observable terminal result when durable finalization is
    /// unavailable. Ordered after any finalization in flight, so it never ends a
    /// job the journal has just ended differently.
    pub(crate) async fn fail_volatile(&self, id: JobId, error: String) {
        let Ok(operation) = self.operation(id).await else {
            return;
        };
        let _operation = operation.lock_owned().await;
        let _delivery = self.inner.delivery_operation.lock().await;
        let failed = Box::new(Finished {
            end: JobEnd::Failed,
            images: Vec::new(),
            diagnostic: Some(
                crate::tool::ToolError::failed(error)
                    .operation(
                        crate::tool::diagnostic::Operation::Save,
                        crate::tool::diagnostic::Subject::Job(id),
                    )
                    .at(crate::tool::diagnostic::FailureSite::Host)
                    .diagnostic(),
            ),
            output_diagnostic: None,
        });
        let Ok((applied, ())) = self
            .journal_change(id, JobChange::Finish(failed), None, |_| ())
            .await
        else {
            return;
        };
        if applied.background {
            let _ = self.inner.completions.send(JobCompletion {
                agent: applied.agent,
                job: id,
            });
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
            let field = "/result/stdout".parse().unwrap();
            let mut writer = PendingCapture::create(&output, &field, CaptureKind::Text)
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
                    SessionEvent::JobFinished { job, state: JobEnd::Completed, .. } if job == id
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
                    .rendered_error(&CapabilitySet::default())
                    .is_some_and(|error| error.contains(expected)),
                "{tool}"
            );
        }
    }

    // Creation ownership and admission tests.

    use crate::job::tests::terminal;
    use crate::session::AppendBoundary;

    async fn count(jobs: &JobManager, matches: impl Fn(&SessionEvent) -> bool) -> usize {
        let records = jobs.store().records().await;
        records
            .iter()
            .filter(|record| matches(&record.event))
            .count()
    }

    async fn creation_count(jobs: &JobManager) -> usize {
        count(jobs, |event| {
            matches!(event, SessionEvent::JobCreated { .. })
        })
        .await
    }

    #[tokio::test]
    async fn cancellation_before_creation_gate_has_no_admitted_work() {
        let (_root, jobs, agent) = crate::job::tests::runtime().await;
        let operation = jobs.inner.creation_operation.write().await;
        let creating = tokio::spawn({
            let (jobs, agent) = (jobs.clone(), agent.clone());
            async move { jobs.create(JobSpec::test(agent, "not-admitted")).await }
        });
        tokio::task::yield_now().await;
        creating.abort();
        assert!(matches!(creating.await, Err(error) if error.is_cancelled()));
        drop(operation);
        assert_eq!(creation_count(&jobs).await, 0);
        assert_eq!(
            jobs.test_create(JobSpec::test(agent, "first")).await.get(),
            1
        );
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Op {
        Create,
        Transition,
        Finish,
        Claim,
        Send,
        RequestInput,
        ResumeInput,
    }

    /// Creates job 1 (except for `Create`) in the state each operation requires,
    /// holding its lease while it is live.
    async fn owned_job(op: Op) -> (tempfile::TempDir, JobManager, AgentId, JobId, Box<dyn Send>) {
        let (root, jobs, agent) = crate::job::tests::runtime().await;
        let id = JobId::new(1).unwrap();
        let spec = JobSpec {
            accepts_input: true,
            ..JobSpec::test(agent.clone(), "owned")
        };
        let lease: Box<dyn Send> = match op {
            Op::Create => Box::new(()),
            Op::Transition => {
                let lease = jobs.test_approving(spec).await;
                assert_eq!(lease.id(), id);
                Box::new(lease)
            }
            Op::Claim | Op::Send => {
                assert_eq!(jobs.test_create(spec).await, id);
                let handler: ResumeHandler = Arc::new(|value, _| {
                    Box::pin(async move { Ok(ToolOutput::new(value.unwrap())) })
                });
                jobs.set_resume_handler(id, handler).await.unwrap();
                jobs.test_finish(id, serde_json::json!("previous")).await;
                Box::new(())
            }
            Op::RequestInput | Op::ResumeInput => {
                let lease = jobs.test_running(spec).await;
                assert_eq!(lease.id(), id);
                if op == Op::ResumeInput {
                    let question = crate::job::tests::question("before");
                    jobs.request_input(id, question).await.unwrap();
                }
                Box::new(lease)
            }
            Op::Finish => {
                assert_eq!(jobs.test_create(spec).await, id);
                Box::new(())
            }
        };
        (root, jobs, agent, id, lease)
    }

    async fn run(op: Op, jobs: JobManager, agent: AgentId, id: JobId) -> Result<(), JobError> {
        match op {
            Op::Create => jobs
                .create(JobSpec::test(agent, "abandoned"))
                .await
                .map(drop),
            Op::Transition => jobs.advance(id, JobTransition::Running).await,
            Op::Finish => {
                let output = ToolOutput::new(serde_json::json!({"text":"done"}));
                jobs.finish(id, JobOutcome::Completed(output)).await
            }
            Op::Claim => jobs.claim(id).await,
            Op::Send => jobs.send(id, serde_json::json!("resumed")).await,
            Op::RequestInput => {
                let question = crate::job::tests::question("after");
                jobs.request_input(id, question).await
            }
            Op::ResumeInput => jobs.resume_input(id).await,
        }
    }

    /// Every accepted append keeps its ownership gate through a caller abort and
    /// publishes both durably and live once the writer resumes.
    #[tokio::test]
    async fn cancelled_callers_keep_accepted_publication_owned_at_append_boundaries() {
        use Op::*;
        let ops = [
            Create,
            Transition,
            Finish,
            Claim,
            Send,
            RequestInput,
            ResumeInput,
        ];
        for op in ops {
            let case = format!("{op:?}");
            let (_root, jobs, agent, id, _lease) = owned_job(op).await;
            let state = async |jobs: &JobManager| jobs.metadata(id).await.ok().map(|m| m.state);
            let delivery = async |jobs: &JobManager| jobs.inner.jobs.lock().await[&id].delivery;
            let before = state(&jobs).await;
            // Either boundary parks the owner on the same receipt; the session's own
            // tests cover what differs between them.
            let (reached, resume) = jobs.store().pause_append_at(AppendBoundary::Write).await;
            let caller = tokio::spawn(run(op, jobs.clone(), agent, id));
            reached.await.unwrap();
            caller.abort();
            assert!(caller.await.unwrap_err().is_cancelled());
            // Nothing is published live before the durable append completes, and
            // the cancelled caller must not release its publication gate.
            assert_eq!(state(&jobs).await, before, "{case}");
            let gate_held = match op {
                Create => jobs.inner.creation_operation.try_write().is_err(),
                Claim => {
                    assert!(delivery(&jobs).await == DeliveryState::Pending);
                    jobs.inner.delivery_operation.try_lock().is_err()
                }
                _ => jobs.operation(id).await.unwrap().try_lock().is_err(),
            };
            assert!(gate_held, "{case}");
            let drain = tokio::spawn({
                let jobs = jobs.clone();
                async move {
                    jobs.drain_creations().await;
                    jobs.drain_supervisors().await;
                }
            });
            tokio::task::yield_now().await;
            assert!(!drain.is_finished(), "{case}");
            resume.send(()).unwrap();
            tokio::time::timeout(Duration::from_secs(3), drain)
                .await
                .unwrap()
                .unwrap();
            let expected = match op {
                Create => JobState::Cancelled,
                Transition | ResumeInput => JobState::Running,
                Finish | Claim | Send => JobState::Completed,
                RequestInput => JobState::WaitingInput,
            };
            assert_eq!(state(&jobs).await, Some(expected), "{case}");
            let replay = jobs.test_replay().await;
            // Replay deliberately interrupts unfinished work; terminal results,
            // delivery, and durable transition records still agree with the owner.
            let durable = match op {
            Create | Finish | Claim | Send => state(&replay).await == Some(expected),
            Transition | RequestInput | ResumeInput => {
                count(&jobs, |event| {
                    matches!(event, SessionEvent::JobStateChanged { job, state } if *job == id && JobState::from(*state) == expected)
                })
                .await
                    > 0
            }
        };
            assert!(durable, "{case}");
            if op == Claim {
                assert!(delivery(&jobs).await == DeliveryState::Claimed);
                assert!(delivery(&replay).await == DeliveryState::Claimed);
            }
            if op == Create {
                assert_eq!(creation_count(&jobs).await, 1);
                let finished = count(
                    &jobs,
                    |event| matches!(event, SessionEvent::JobFinished { job, .. } if *job == id),
                );
                assert_eq!(finished.await, 1);
            }
        }
    }

    /// An indeterminate append publishes nothing live, and the poisoned writer
    /// refuses the retry instead of appending again.
    #[tokio::test]
    async fn indeterminate_appends_do_not_publish_live_success_or_retry() {
        for op in [Op::Create, Op::Transition, Op::Finish, Op::Claim] {
            let (_root, jobs, agent, id, _lease) = owned_job(op).await;
            let sequence = jobs.store().records().await.len() as u64 + 1;
            jobs.store()
                .fail_append_at(AppendBoundary::Publication)
                .await;
            let error = run(op, jobs.clone(), agent.clone(), id).await.unwrap_err();
            let JobError::Session(SessionError::AppendIndeterminate(recovery)) = error else {
                panic!("{op:?}: expected recovery-required failure: {error}");
            };
            assert_eq!(
                recovery.identity.sequence,
                RecordSeq::from(sequence),
                "{op:?}"
            );
            let retried = run(op, jobs.clone(), agent, id).await;
            assert!(
                matches!(retried, Err(JobError::Session(SessionError::AppendUnavailable(ref later))) if later == &recovery),
                "{op:?}"
            );
            match op {
                Op::Create => {
                    assert!(jobs.inner.jobs.lock().await.is_empty());
                    assert_eq!(creation_count(&jobs).await, 0);
                }
                Op::Claim => {
                    assert!(jobs.inner.jobs.lock().await[&id].delivery == DeliveryState::Pending);
                }
                _ => {
                    let expected = if op == Op::Transition {
                        JobState::AwaitingApproval
                    } else {
                        JobState::Queued
                    };
                    assert_eq!(jobs.metadata(id).await.unwrap().state, expected);
                    assert!(jobs.operation(id).await.unwrap().try_lock().is_ok());
                }
            }
        }
    }

    #[tokio::test]
    async fn creation_inherits_concurrent_parent_cancellation() {
        let (_root, jobs, agent) = crate::job::tests::runtime().await;
        let parent = jobs
            .test_create(JobSpec::test(agent.clone(), "parent"))
            .await;
        jobs.test_finish(parent, serde_json::json!("done")).await;
        let (reached, resume) = jobs
            .store()
            .pause_append_at(AppendBoundary::Publication)
            .await;
        let creating = tokio::spawn({
            let jobs = jobs.clone();
            let spec = JobSpec {
                parent: Some(parent),
                ..JobSpec::test(agent, "child")
            };
            async move { jobs.create(spec).await }
        });
        tokio::time::timeout(Duration::from_secs(3), reached)
            .await
            .unwrap()
            .unwrap();
        // Cancellation does not need the creation gate or the session writer lock.
        let cancel = tokio::time::timeout(Duration::from_secs(1), jobs.cancel(parent));
        cancel.await.unwrap().unwrap();
        resume.send(()).unwrap();
        let child = creating.await.unwrap().unwrap();
        assert!(child.cancellation_token().is_cancelled());
        assert_eq!(terminal(&jobs, child.id()).await.state, JobState::Cancelled);
        assert_eq!(creation_count(&jobs).await, 2);
    }
}
