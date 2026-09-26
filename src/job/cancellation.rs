//! Cancellation propagation, graceful shutdown, and forced aborts.

use super::*;

use crate::tool::invocation::CANCELLATION_GRACE;

impl JobManager {
    /// Request cancellation of this job and its descendants. The returned
    /// metadata is a snapshot, not proof that cancellation has completed; a
    /// previously published terminal result is preserved.
    pub async fn cancel(&self, id: JobId) -> Result<JobEnvelope, JobError> {
        let watchdogs = {
            let mut jobs = self.inner.jobs.lock().await;
            if !jobs.contains_key(&id) {
                return Err(JobError::Unknown(id));
            }
            let mut descendants = std::collections::HashSet::from([id]);
            loop {
                let before = descendants.len();
                for (child, entry) in jobs.iter() {
                    if entry
                        .parent
                        .is_some_and(|parent| descendants.contains(&parent))
                    {
                        descendants.insert(*child);
                    }
                }
                if descendants.len() == before {
                    break;
                }
            }
            let mut watchdogs = Vec::new();
            for job in descendants {
                let entry = jobs.get_mut(&job).expect("known descendant");
                entry.cancellation.cancel();
                if entry.cancellable() && !entry.cancellation_watchdog_started {
                    entry.cancellation_watchdog_started = true;
                    watchdogs.push(job);
                }
            }
            watchdogs
        };
        for job in watchdogs {
            let jobs = self.clone();
            tokio::spawn(async move {
                tokio::time::sleep(CANCELLATION_GRACE).await;
                jobs.force_cancel(job).await;
            });
        }
        let jobs = self.inner.jobs.lock().await;
        Ok(jobs.get(&id).ok_or(JobError::Unknown(id))?.metadata(id))
    }

    pub async fn cancel_all(&self, owner: &AgentId) -> usize {
        let ids = self
            .inner
            .jobs
            .lock()
            .await
            .iter()
            .filter_map(|(id, entry)| (&entry.agent == owner && entry.cancellable()).then_some(*id))
            .collect::<Vec<_>>();
        for id in &ids {
            let _ = self.cancel(*id).await;
        }
        ids.len()
    }

    /// Cancel every remaining job and await its persisted terminal outcome.
    /// Unlike an ordinary wait, a pending question is not a completion. Repeat
    /// the snapshot to include descendants created while cancellation propagates.
    pub(crate) async fn cancel_and_drain(&self) -> Result<(), JobError> {
        loop {
            let ids = self
                .inner
                .jobs
                .lock()
                .await
                .iter()
                .filter_map(|(id, entry)| entry.cancellable().then_some(*id))
                .collect::<Vec<_>>();
            if ids.is_empty() {
                // Terminal publication may precede supervisor cleanup.
                self.drain_supervisors().await;
                return Ok(());
            }
            for id in &ids {
                self.cancel(*id).await?;
            }
            for id in ids {
                self.wait_inner(id, None, WaitMode::Terminal).await?;
            }
        }
    }

    pub(super) async fn force_cancel(&self, id: JobId) {
        let task_abort = {
            let jobs = self.inner.jobs.lock().await;
            let Some(entry) = jobs.get(&id) else {
                return;
            };
            if !entry.cancellable() {
                return;
            }
            entry.task_abort.clone()
        };
        if let Some(task_abort) = task_abort {
            task_abort.abort();
        }
        if let Err(error) = self.finish(id, ToolError::cancelled().into()).await
            && !matches!(error, JobError::AlreadyTerminal(_))
        {
            self.fail_volatile(
                id,
                format!("job cancellation could not be persisted: {error}"),
            )
            .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::TestRuntime;
    use crate::tool::{
        ToolError, ToolOptions, ToolRegistryBuilder,
        policy::{AuthorizationRequest, Capability, Policy, PolicyFuture},
    };
    use std::sync::atomic::AtomicBool;

    #[derive(Deserialize, JsonSchema)]
    struct NoArgs {}

    struct NeverAuthorize;

    impl Policy for NeverAuthorize {
        fn authorize(&self, _request: AuthorizationRequest) -> PolicyFuture<'_> {
            Box::pin(std::future::pending())
        }
    }

    fn child(parent: JobId, agent: AgentId, tool: &str) -> JobSpec {
        JobSpec {
            parent: Some(parent),
            ..JobSpec::test(agent, tool)
        }
    }

    async fn cancelled_within(jobs: &JobManager, id: JobId, seconds: u64) -> JobState {
        let wait = tokio::time::timeout(Duration::from_secs(seconds), jobs.wait(id, None, true));
        wait.await
            .expect("cancellation did not terminate the job")
            .unwrap()
            .state
    }

    #[tokio::test]
    async fn cancellation_preserves_published_results_and_denials() {
        use crate::{
            job::output::PendingCapture,
            media::ImageFormat,
            tool::{
                StreamEnd,
                diagnostic::{
                    Cause, Effects, FailureSite, Operation, PartialContext, PartialDiagnostic,
                    Subject,
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
            .effects(Effects::MayHaveExecuted);
        let output_diagnostic = PartialDiagnostic::new(
            PartialContext::new(Operation::ReadCapture, Subject::Process)
                .effects(Effects::OutputIncomplete),
            Cause::Message("partial output".into()),
        );
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
            let saved = jobs.output(id);
            let stdout = "/result/stdout".parse().unwrap();
            let mut writer = PendingCapture::create(&saved, &stdout, CaptureKind::Text)
                .unwrap()
                .open();
            writer.write_all(text.as_bytes()).unwrap();
            let mut output = ToolOutput::new(serde_json::json!({
                "partial": true, "error": {"message": null},
            }))
            .with_captures(vec![writer.finish().unwrap()])
            .with_images(vec![image.clone()])
            .with_diagnostic(output_diagnostic.clone());
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
            jobs.force_cancel(id).await;
            for manager in [jobs.clone(), jobs.test_replay().await] {
                assert_eq!(manager.snapshot(id).await.unwrap(), expected);
                assert_eq!(manager.images(id).await.unwrap(), vec![image.clone()]);
                let saved = manager.output(id);
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

    #[tokio::test]
    async fn cancellation_drain_waits_for_terminal_questions_and_descendants() {
        let (_root, jobs, agent) = crate::job::tests::runtime().await;
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
        let drain = tokio::time::timeout(Duration::from_secs(5), jobs.cancel_and_drain());
        drain.await.unwrap().unwrap();
        let records = jobs.store().records().await;
        for id in [parent.id(), question.id()] {
            assert_eq!(jobs.snapshot(id).await.unwrap().state, JobState::Cancelled);
            assert!(
                records.iter().any(|record| {
                    matches!(&record.event, SessionEvent::JobFinished { job, .. } if *job == id)
                }),
                "terminal cancellation must be journaled before shutdown returns"
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
        assert_eq!(
            cancelled_within(&jobs, late.id(), 2).await,
            JobState::Cancelled
        );
    }

    #[tokio::test]
    async fn uncooperative_and_cooperative_handlers_and_pending_authorization_observe_cancellation()
    {
        let runtime = TestRuntime::new().await;
        let (agent, jobs) = (&runtime.agent, &runtime.jobs);
        let observed = Arc::new(AtomicBool::new(false));
        let started = Arc::new(Notify::new());
        let mut builder = ToolRegistryBuilder::default();
        let options = ToolOptions::new(vec![Capability::Exec]).background();
        builder
            .register::<NoArgs, String, _, _>(
                "stubborn",
                "never completes",
                options.clone(),
                |_, _| std::future::pending::<Result<String, ToolError>>(),
            )
            .unwrap()
            .register::<NoArgs, String, _, _>("cooperative", "wait for cancellation", options, {
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
        let background = serde_json::json!({"bg": true});
        let stubborn = executor
            .run_host(agent, "stubborn", background.clone())
            .await
            .unwrap();
        jobs.cancel(stubborn.job).await.unwrap();
        assert_eq!(
            cancelled_within(jobs, stubborn.job, 2).await,
            JobState::Cancelled
        );
        let cooperative = executor
            .run_host(agent, "cooperative", background)
            .await
            .unwrap();
        started.notified().await;
        jobs.cancel(cooperative.job).await.unwrap();
        assert_eq!(
            cancelled_within(jobs, cooperative.job, 1).await,
            JobState::Cancelled
        );
        assert!(observed.load(Ordering::Relaxed));

        let executor = crate::tool::executor::ToolExecutor::new(
            registry,
            Arc::new(NeverAuthorize),
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
        let awaiting = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let listed = jobs.list(agent).await;
                if let Some(job) = listed
                    .iter()
                    .find(|job| job.state == JobState::AwaitingApproval)
                {
                    break job.id;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        jobs.cancel(awaiting).await.unwrap();
        let result = tokio::time::timeout(Duration::from_secs(1), execution);
        let result = result
            .await
            .expect("authorization did not observe cancellation")
            .unwrap();
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
