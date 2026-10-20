//! Cancellation propagation, graceful shutdown, and forced aborts.

use super::*;

/// How long a cancelled worker may take to stop before it is aborted.
pub(crate) const CANCELLATION_GRACE: Duration = Duration::from_millis(250);

/// Which jobs a cancellation ends.
#[derive(Clone, Copy)]
pub(crate) enum CancelScope {
    /// Any whose outcome it changes: running work stops, and an interruption
    /// is no longer resumable.
    Outcome,
    /// Only work still running. Closing a session leaves an interruption as it
    /// is, resumable when the session reopens; ending it would be new activity.
    Running,
}

impl CancelScope {
    fn covers(self, entry: &JobFacts) -> bool {
        match self {
            Self::Outcome => entry.cancellable(),
            Self::Running => entry.end().is_none(),
        }
    }
}

impl JobManager {
    /// Request cancellation of this job and its descendants. The returned
    /// metadata is a snapshot, not proof that cancellation has completed; a
    /// previously published terminal result is preserved, and the request is
    /// recorded of each finished job it stops restarting. A turn an interrupt
    /// left held on the job runs on with its result.
    pub async fn cancel(&self, id: JobId) -> Result<JobEnvelope, JobError> {
        let manager = self.clone();
        let cancel = move |state: &CoreState| {
            let entry = state.job(id)?;
            let metadata = entry.metadata(id);
            let held = entry.child().is_some()
                && entry.live()
                && !views::effectively_background(&state.jobs, id);
            let holder = held.then(|| entry.agent.clone());
            let jobs = &state.jobs;
            // Recording a cancel drops the job's handles, so its token, which a
            // child's running loop holds, is taken here.
            let (recorded, tokens): (Vec<_>, Vec<_>) = (jobs.trees(vec![id]).into_iter())
                .filter_map(|job| Some((job, jobs.get(job)?)))
                .filter(|(_, entry)| entry.records_cancel())
                .map(|(job, entry)| {
                    let event = SessionEvent::JobCancelRequested { job };
                    let token =
                        (entry.handles.as_ref()).map(|handles| handles.cancellation.clone());
                    ((entry.agent.clone(), event), token)
                })
                .unzip();
            Ok(Plan::new(recorded, move |state, _| {
                manager.cancel_trees(state, vec![id], CancelScope::Outcome);
                state.defer(move || tokens.iter().flatten().for_each(CancellationToken::cancel));
                if let Some(holder) = holder {
                    state.agents.run_on(&holder);
                }
                metadata
            }))
        };
        self.inner
            .store
            .commit_on([id], Command::Journal, cancel)
            .await
    }

    /// Cancel `roots` and all their descendants, arming the forced-abort watchdog
    /// of the invocation each `scope` covers runs. The watchdog settles whatever
    /// outcome its cancellation still changes, an interruption the cancellation
    /// caused too. Tokens are cancelled once the change applies, in order.
    pub(crate) fn cancel_trees(
        &self,
        state: &mut CoreState,
        roots: Vec<JobId>,
        scope: CancelScope,
    ) {
        let jobs = &mut state.jobs;
        let (mut tokens, mut watched) = (Vec::new(), Vec::new());
        for job in jobs.trees(roots) {
            let cancelled = jobs.modify(job, |entry, _| {
                let watch = scope.covers(entry) && !entry.cancelling();
                entry.cancel = match entry.cancel {
                    CancelState::Recorded => CancelState::Recorded,
                    CancelState::Requested { watched } if !watch => {
                        CancelState::Requested { watched }
                    }
                    _ => CancelState::Requested {
                        watched: watch.then_some(entry.invocation),
                    },
                };
                let token = entry
                    .handles
                    .as_ref()
                    .map(|handles| handles.cancellation.clone());
                // It cancels the invocation running now, not one a later input starts.
                (token, watch.then_some(Run(job, entry.invocation)))
            });
            let Some((token, watch)) = cancelled else {
                continue;
            };
            tokens.extend(token);
            watched.extend(watch);
        }
        let manager = self.clone();
        state.defer(move || {
            tokens.iter().for_each(CancellationToken::cancel);
            for run in watched {
                let jobs = manager.clone();
                // Scheduling the watchdog must not extend the admitted grace period.
                let grace = tokio::time::Instant::now() + CANCELLATION_GRACE;
                // It ends with the cancellation it watches, so a drain does not
                // outwait its grace.
                let settled = move |state: &CoreState| {
                    let entry = state.jobs.get(run.0);
                    (!entry.is_some_and(JobFacts::cancelling)).then_some(())
                };
                manager.inner.supervisor.spawn(async move {
                    let settled = jobs.inner.store.until(Wait::on([Key::Job(run.0)], settled));
                    if tokio::time::timeout_at(grace, settled).await.is_err() {
                        jobs.force_cancel(run).await;
                    }
                });
            }
        });
    }

    #[cfg(test)]
    pub(crate) async fn cancel_all(&self, owner: &AgentId, scope: CancelScope) -> usize {
        let (manager, owner) = (self.clone(), owner.clone());
        let cancel = move |state: &mut CoreState| manager.cancel_owned(state, &[owner], scope);
        self.inner.store.live(cancel).await
    }

    /// Cancel the jobs of `owners` that `scope` covers, with their descendants.
    pub(crate) fn cancel_owned(
        &self,
        state: &mut CoreState,
        owners: &[AgentId],
        scope: CancelScope,
    ) -> usize {
        let jobs = &state.jobs;
        let ids = (owners.iter())
            .flat_map(|owner| jobs.cancellable(owner))
            .filter(|(_, entry)| scope.covers(entry))
            .map(|(id, _)| id)
            .collect::<Vec<_>>();
        let count = ids.len();
        self.cancel_trees(state, ids, scope);
        count
    }

    /// Cancel every job still running but child invocations, which closing leaves
    /// resumable, with its descendants: what a script launched ends with it.
    pub(crate) fn cancel_running(&self, state: &mut CoreState) {
        let running = (state.jobs.running())
            .filter(|(_, entry)| entry.child().is_none())
            .map(|(id, _)| id)
            .collect();
        self.cancel_trees(state, running, CancelScope::Running);
    }

    /// Cancel what `cancel_running` does and await its persisted terminal outcome,
    /// and that of every cancellation already in flight. Unlike an ordinary
    /// wait, a pending question is not a completion. Repeat to include
    /// descendants created while cancellation propagates.
    pub(crate) async fn cancel_and_drain(&self) -> Result<(), JobError> {
        loop {
            let manager = self.clone();
            let cancelling = self.inner.store.live(move |state| {
                manager.cancel_running(state);
                let jobs = &state.jobs;
                let cancellable = jobs.owners().flat_map(|owner| jobs.cancellable(owner));
                let cancelling = cancellable.filter(|(_, entry)| entry.cancelling());
                cancelling.map(|(id, _)| id).collect::<Vec<_>>()
            });
            let cancelling = cancelling.await;
            if cancelling.is_empty() {
                // Terminal publication may precede supervisor cleanup.
                self.inner.supervisor.drain().await;
                return Ok(());
            }
            let keys: Vec<_> = cancelling.iter().copied().map(Key::Job).collect();
            let settled = move |state: &CoreState| {
                let still = |job: &JobId| state.jobs.get(*job).is_some_and(JobFacts::cancelling);
                (!cancelling.iter().any(still)).then_some(())
            };
            self.inner.store.until(Wait::on(keys, settled)).await?;
        }
    }

    /// Abort `run`'s worker and settle its cancellation, if that changes its outcome.
    pub(super) async fn force_cancel(&self, run: Run) {
        let Run(job, invocation) = run;
        let worker = self.entry(job, |entry| entry.handles.as_ref()?.task_abort.clone());
        let worker = worker.await.ok().flatten();
        if let Some((running, task_abort)) = worker
            && running == invocation
        {
            task_abort.abort();
        }
        let cancelled = async { ToolError::cancelled().into() };
        let _ = super::supervisor::spawn_completion(self.clone(), run, cancelled).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{TestRuntime, bounded};
    use crate::tool::{
        ToolError, ToolOptions, ToolRegistryBuilder,
        policy::{Approval, AuthorizationRequest, Capability, Policy},
    };
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use tokio::sync::Notify;

    #[derive(Deserialize, JsonSchema)]
    struct NoArgs {}

    /// Reports each job it is asked about, and never decides.
    struct NeverAuthorize(tokio::sync::mpsc::UnboundedSender<JobId>);

    impl Policy for NeverAuthorize {
        fn authorize(&self, request: AuthorizationRequest) -> Approval<'_> {
            Approval::Ask(Box::pin(async move {
                self.0.send(request.job).unwrap();
                std::future::pending().await
            }))
        }
    }

    fn child(parent: JobId, agent: AgentId, tool: &str) -> JobSpec {
        JobSpec {
            parent: Some(parent),
            ..JobSpec::test(agent, tool)
        }
    }

    async fn cancelled(jobs: &JobManager, id: JobId) -> JobState {
        bounded(jobs.wait(id, true)).await.unwrap().state
    }

    #[tokio::test]
    async fn cancellation_preserves_published_results_and_denials() {
        use crate::{
            job::output::PendingCapture,
            media::ImageFormat,
            tool::{
                StreamEnd,
                diagnostic::{
                    Cause, FailureSite, Operation, PartialContext, RequestEffects, Subject,
                },
            },
        };
        use std::io::Write as _;

        let (_root, jobs, agent) = crate::job::tests::runtime().await;
        let context = PartialContext::new(Operation::Wait, Subject::Process)
            .at(FailureSite::Execution(ExecutionLocation::named(
                "worker".parse().unwrap(),
                "/workspace".into(),
            )))
            .request(RequestEffects::MayHaveExecuted);
        let image = ImageRef {
            file: Some("partial.png".into()),
            format: ImageFormat::Png,
            blob: jobs.store().store_blob(b"image").await.unwrap(),
        };
        let text = "partial output\n".repeat(4096);
        for (error, streams) in [
            (None, StreamEnd::Finished),
            (
                Some(ToolError::denied("permission denied after partial work")),
                StreamEnd::Finished,
            ),
            (Some(ToolError::cancelled()), StreamEnd::Finished),
            (Some(ToolError::interrupted()), StreamEnd::Finished),
            (Some(ToolError::interrupted()), StreamEnd::Cut),
        ] {
            let id = jobs
                .test_create(JobSpec::test(agent.clone(), "finished"))
                .await;
            let saved = jobs.output(id).await;
            let stdout = "/result/stdout".parse().unwrap();
            let mut writer = PendingCapture::create(&saved, &stdout, CaptureKind::Text)
                .unwrap()
                .open();
            writer.write_all(text.as_bytes()).unwrap();
            let mut output = ToolOutput::new(serde_json::json!({"partial": true}))
                .with_captures(vec![writer.finish().unwrap()])
                .with_images(vec![image.clone()]);
            output.streams = streams;
            let outcome = match error {
                Some(error) => error.context(context.clone()).with_result(output).into(),
                None => JobOutcome::Completed(output),
            };
            jobs.finish(id, outcome).await.unwrap();
            let mut expected = jobs.snapshot(id).await.unwrap();
            let document = saved.test_document();
            let fields = saved.test_fields();
            let view = jobs
                .present_output(JobOutputQuery::new(id), &CapabilitySet::default())
                .await
                .unwrap();
            let metadata = jobs.cancel(id).await.unwrap();
            assert_eq!(metadata.state, expected.state);
            // Cancellation ends an interruption's resumability, but only its
            // terminal cause changes; prior context and output remain authoritative.
            if expected.state == JobState::Interrupted {
                expected.state = JobState::Cancelled;
                expected.diagnostic.as_mut().unwrap().cause = Cause::Cancelled;
            }
            // Even a late watchdog must not overwrite output or denial metadata.
            jobs.force_cancel(jobs.run(id).await).await;
            // Replayed after the live reads, which can themselves journal.
            for replay in [false, true] {
                let manager = if replay {
                    jobs.test_replay()
                } else {
                    jobs.clone()
                };
                assert_eq!(manager.snapshot(id).await.unwrap(), expected);
                assert_eq!(
                    manager.snapshot(id).await.unwrap().images,
                    vec![image.clone()]
                );
                let saved = manager.output(id).await;
                assert_eq!(saved.test_document(), document);
                assert_eq!(saved.test_fields(), fields);
                assert_eq!(saved.test_bytes("/result/stdout").unwrap(), text.as_bytes());
                let after = manager
                    .present_output(JobOutputQuery::new(id), &CapabilitySet::default())
                    .await
                    .unwrap();
                assert_eq!(
                    after["presentation"]["captures"],
                    view["presentation"]["captures"]
                );
            }
        }
    }

    /// The drain ends running work and settles cancellations in flight, but
    /// leaves an interruption as it is, journaling nothing for it.
    #[tokio::test]
    async fn cancellation_drain_settles_running_work_and_leaves_interruptions() {
        let (_root, jobs, agent) = crate::job::tests::runtime().await;
        let interrupted = async || {
            let id = jobs.test_create(JobSpec::test(agent.clone(), "exec")).await;
            jobs.finish(id, ToolError::interrupted().into())
                .await
                .unwrap();
            id
        };
        let (kept, cancelling) = (interrupted().await, interrupted().await);
        jobs.cancel(cancelling).await.unwrap();
        let parent = jobs
            .test_running(JobSpec::test(agent.clone(), "script"))
            .await;
        let spec = JobSpec {
            accepts_input: true,
            background: true,
            ..child(parent.id(), agent, "ask")
        };
        let question = jobs.test_running(spec).await;
        let prompt = crate::job::tests::question("pending");
        jobs.request_input(question.id(), prompt).await.unwrap();
        assert_eq!(
            jobs.snapshot(question.id()).await.unwrap().state,
            JobState::WaitingInput
        );
        bounded(jobs.cancel_and_drain()).await.unwrap();
        let records = jobs.store().records();
        let finished = |id| {
            let finished = |record: &&Arc<crate::session::EventRecord>| matches!(&record.event, SessionEvent::JobFinished { job, .. } if *job == id);
            records.iter().filter(finished).count()
        };
        assert_eq!(
            jobs.snapshot(kept).await.unwrap().state,
            JobState::Interrupted
        );
        assert_eq!(finished(kept), 1);
        for id in [parent.id(), question.id(), cancelling] {
            assert_eq!(jobs.snapshot(id).await.unwrap().state, JobState::Cancelled);
            let expected = if id == cancelling { 2 } else { 1 };
            // Journaled before shutdown returns.
            assert_eq!(finished(id), expected, "job {id:?}");
        }
    }

    /// Cancelling a reopened tree reaches the descendants the stopped process
    /// left interrupted, though nothing runs them any more.
    #[tokio::test]
    async fn cancelling_a_reopened_parent_reaches_descendants_nothing_runs() {
        let TestRuntime {
            root, jobs, agent, ..
        } = TestRuntime::on_disk().await;
        let parent = jobs
            .test_running(JobSpec::test(agent.clone(), "script"))
            .await;
        let spec = child(parent.id(), agent.clone(), "script");
        let nested = jobs.test_running(spec).await;
        let leaf = jobs.test_running(child(nested.id(), agent, "exec")).await;
        let tree = [parent.id(), nested.id(), leaf.id()];
        // Gone the way a process exit leaves them: cancelled, never finalized.
        std::thread::spawn(move || drop((parent, nested, leaf)))
            .join()
            .unwrap();
        let restored = crate::job::tests::reopen(jobs, root.path()).await;
        restored.cancel(tree[0]).await.unwrap();
        bounded(restored.cancel_and_drain()).await.unwrap();
        for id in tree {
            assert_eq!(
                restored.snapshot(id).await.unwrap().state,
                JobState::Cancelled
            );
        }
    }

    #[tokio::test]
    async fn cancelling_completed_parent_cancels_descendants_and_late_starts() {
        let (root, jobs, agent) = crate::job::tests::runtime().await;
        let parent = jobs
            .test_lease(JobSpec::test(agent.clone(), "script"))
            .await;
        crate::session::tests::start_child(jobs.store(), &agent, 1, None, root.path()).await;
        let child_lease = jobs
            .test_lease(child(parent.id(), agent.child(1), "agent"))
            .await;
        let grandchild = jobs
            .test_lease(child(child_lease.id(), agent.child(1), "exec"))
            .await;
        jobs.test_finish(parent.id(), serde_json::Value::Null).await;
        jobs.cancel(parent.id()).await.unwrap();
        assert!(child_lease.cancellation_token().is_cancelled());
        assert!(grandchild.cancellation_token().is_cancelled());
        let late = jobs.test_lease(child(grandchild.id(), agent, "late")).await;
        assert!(late.cancellation_token().is_cancelled());
        assert_eq!(cancelled(&jobs, late.id()).await, JobState::Cancelled);
    }

    #[tokio::test]
    async fn uncooperative_and_cooperative_handlers_and_pending_authorization_observe_cancellation()
    {
        let runtime = TestRuntime::new().await;
        let (agent, jobs) = (&runtime.agent, &runtime.jobs);
        let observed = Arc::new(AtomicBool::new(false));
        let started = Arc::new(Notify::new());
        let mut builder = ToolRegistryBuilder::default();
        let options = || ToolOptions::new(vec![Capability::Exec]).background();
        builder
            .register::<NoArgs, String, _, _>("stubborn", "never completes", options(), |_, _| {
                std::future::pending::<Result<String, ToolError>>()
            })
            .unwrap()
            .register::<NoArgs, String, _, _>("cooperative", "wait for cancellation", options(), {
                let (observed, started) = (observed.clone(), started.clone());
                move |context, _input| {
                    let (observed, started) = (observed.clone(), started.clone());
                    async move {
                        started.notify_one();
                        context.cancelled().await;
                        observed.store(true, Ordering::Relaxed);
                        Err(ToolError::cancelled())
                    }
                }
            })
            .unwrap();
        let registry = builder.build();
        let executor = crate::tool::executor::ToolExecutor::new(
            registry.clone(),
            Arc::new(crate::tool::policy::AllowAll),
            jobs.clone(),
            runtime.root.path().to_path_buf(),
        );
        // Uncooperative handlers are aborted after the cancellation grace.
        let background = serde_json::json!({crate::tool::registry::BACKGROUND: true});
        let stubborn = executor
            .run_host(agent, "stubborn", background.clone())
            .await
            .unwrap();
        jobs.cancel(stubborn.job).await.unwrap();
        assert_eq!(cancelled(jobs, stubborn.job).await, JobState::Cancelled);
        let cooperative = executor
            .run_host(agent, "cooperative", background)
            .await
            .unwrap();
        started.notified().await;
        jobs.cancel(cooperative.job).await.unwrap();
        assert_eq!(cancelled(jobs, cooperative.job).await, JobState::Cancelled);
        assert!(observed.load(Ordering::Relaxed));

        let (asked, mut asked_about) = tokio::sync::mpsc::unbounded_channel();
        let executor = crate::tool::executor::ToolExecutor::new(
            registry,
            Arc::new(NeverAuthorize(asked)),
            jobs.clone(),
            runtime.root.path().to_path_buf(),
        );
        let execution = tokio::spawn({
            let agent = agent.clone();
            async move {
                executor
                    .run_host(&agent, "cooperative", serde_json::json!({}))
                    .await
            }
        });
        let awaiting = bounded(asked_about.recv()).await.unwrap();
        assert_eq!(
            jobs.snapshot(awaiting).await.unwrap().state,
            JobState::AwaitingApproval
        );
        jobs.cancel(awaiting).await.unwrap();
        let result = bounded(execution).await.unwrap();
        assert_eq!(
            result.unwrap_err().diagnostic().cause,
            crate::tool::diagnostic::Cause::Cancelled,
        );
        assert_eq!(
            jobs.snapshot(awaiting).await.unwrap().state,
            JobState::Cancelled
        );
    }
}
