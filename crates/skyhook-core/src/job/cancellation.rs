//! Cancellation propagation, graceful shutdown, and forced aborts.

use super::*;

const CANCELLATION_GRACE: Duration = Duration::from_millis(250);

impl JobManager {
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
                if (!entry.state.is_terminal() || entry.state == JobState::Interrupted)
                    && !entry.cancellation_watchdog_started
                {
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
            .filter_map(|(id, entry)| {
                (&entry.agent == owner
                    && (!entry.state.is_terminal() || entry.state == JobState::Interrupted))
                    .then_some(*id)
            })
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
                .filter_map(|(id, entry)| {
                    (!entry.state.is_terminal() || entry.state == JobState::Interrupted)
                        .then_some(*id)
                })
                .collect::<Vec<_>>();
            if ids.is_empty() {
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
            if entry.state.is_terminal() && entry.state != JobState::Interrupted {
                return;
            }
            entry.task_abort.clone()
        };
        if let Some(task_abort) = task_abort {
            task_abort.abort();
        }
        if let Err(error) = self.finish(id, JobOutcome::Cancelled).await
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
        executor::{ExecutionError, ToolExecutor},
        policy::{AllowAll, AuthorizationRequest, Policy, PolicyFuture},
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

    #[tokio::test]
    async fn cancellation_drain_waits_for_terminal_questions_and_descendants() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::create(root.path()).await.unwrap();
        let agent = AgentId::root(store.id());
        let jobs = JobManager::new(store.clone());
        let parent = jobs
            .test_lease(JobSpec::test(agent.clone(), "script"))
            .await;
        jobs.transition(parent.id, JobState::Running).await.unwrap();
        let question = jobs
            .test_lease(JobSpec {
                parent: Some(parent.id),
                accepts_input: true,
                background: true,
                ..JobSpec::test(agent, "ask")
            })
            .await;
        jobs.transition(question.id, JobState::Running)
            .await
            .unwrap();
        jobs.request_input(question.id, serde_json::json!({"prompt": "pending"}))
            .await
            .unwrap();
        assert_eq!(
            jobs.snapshot(question.id).await.unwrap().state,
            JobState::WaitingInput
        );
        tokio::time::timeout(Duration::from_secs(5), jobs.cancel_and_drain())
            .await
            .unwrap()
            .unwrap();
        for id in [parent.id, question.id] {
            assert_eq!(jobs.snapshot(id).await.unwrap().state, JobState::Cancelled);
            assert!(
                store.records().await.iter().any(|record| {
                    matches!(&record.event, SessionEvent::JobFinished { job, .. } if *job == id)
                }),
                "terminal cancellation must be journaled before shutdown returns"
            );
        }
    }

    #[tokio::test]
    async fn cancelling_completed_parent_cancels_descendants_and_late_starts() {
        let (_root, jobs, agent) = crate::job::tests::runtime().await;
        let parent = jobs
            .test_lease(JobSpec::test(agent.clone(), "script"))
            .await;
        let child = jobs
            .test_lease(JobSpec {
                parent: Some(parent.id),
                ..JobSpec::test(agent.child(1), "agent")
            })
            .await;
        let grandchild = jobs
            .test_lease(JobSpec {
                parent: Some(child.id),
                ..JobSpec::test(agent.child(1), "shell")
            })
            .await;
        jobs.test_finish(parent.id, serde_json::Value::Null).await;
        jobs.cancel(parent.id).await.unwrap();
        assert!(child.cancellation.is_cancelled());
        assert!(grandchild.cancellation.is_cancelled());
        let late = jobs
            .test_lease(JobSpec {
                parent: Some(grandchild.id),
                ..JobSpec::test(agent, "late")
            })
            .await;
        assert!(late.cancellation.is_cancelled());
        let terminal = jobs
            .wait(late.id, Some(Duration::from_secs(2)), true)
            .await
            .unwrap();
        assert_eq!(terminal.state, JobState::Cancelled);
    }

    #[tokio::test]
    async fn uncooperative_handlers_are_aborted_after_cancellation_grace() {
        let runtime = TestRuntime::new().await;
        let agent = runtime.agent.clone();
        let mut builder = ToolRegistryBuilder::default();
        builder
            .register::<NoArgs, String, _, _>(
                "stubborn",
                "never completes",
                ToolOptions::new(Vec::new()).background(),
                |_context, _input| async move {
                    std::future::pending::<Result<String, ToolError>>().await
                },
            )
            .unwrap();
        let jobs = runtime.jobs.clone();
        let executor = runtime.executor(builder);
        let running = executor
            .execute(agent, "stubborn", serde_json::json!({"bg": true}), None)
            .await
            .unwrap();

        jobs.cancel(running.job).await.unwrap();
        let cancelled =
            tokio::time::timeout(Duration::from_secs(2), jobs.wait(running.job, None, true))
                .await
                .expect("forced cancellation did not terminate the job")
                .unwrap();
        assert_eq!(cancelled.state, JobState::Cancelled);
    }

    #[tokio::test]
    async fn cooperative_handlers_and_pending_authorization_observe_cancellation() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::create(root.path()).await.unwrap();
        let agent = AgentId::root(store.id());
        let observed = Arc::new(AtomicBool::new(false));
        let started = Arc::new(Notify::new());
        let mut builder = ToolRegistryBuilder::default();
        builder
            .register::<NoArgs, String, _, _>(
                "cooperative",
                "wait for cancellation",
                ToolOptions::new(vec![Capability::Exec]).background(),
                {
                    let observed = observed.clone();
                    let started = started.clone();
                    move |context, _input| {
                        let observed = observed.clone();
                        let started = started.clone();
                        async move {
                            started.notify_one();
                            context.cancelled().await;
                            observed.store(true, Ordering::Relaxed);
                            Err(ToolError::Cancelled)
                        }
                    }
                },
            )
            .unwrap();
        let registry = builder.build();
        let jobs = JobManager::new(store);
        let executor = ToolExecutor::new(
            registry.clone(),
            Arc::new(AllowAll),
            jobs.clone(),
            root.path().to_path_buf(),
        );
        let running = executor
            .execute(
                agent.clone(),
                "cooperative",
                serde_json::json!({"bg": true}),
                None,
            )
            .await
            .unwrap();
        started.notified().await;
        jobs.cancel(running.job).await.unwrap();
        let cancelled =
            tokio::time::timeout(Duration::from_secs(1), jobs.wait(running.job, None, true))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(cancelled.state, JobState::Cancelled);
        assert!(observed.load(Ordering::Relaxed));

        let executor = ToolExecutor::new(
            registry,
            Arc::new(NeverAuthorize),
            jobs.clone(),
            root.path().to_path_buf(),
        );
        let execution = tokio::spawn(async move {
            executor
                .execute(agent, "cooperative", serde_json::json!({}), None)
                .await
        });
        let awaiting = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Some(job) = jobs
                    .list(&AgentId::root(jobs.store().id()))
                    .await
                    .into_iter()
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
        let result = tokio::time::timeout(Duration::from_secs(1), execution)
            .await
            .expect("authorization did not observe cancellation")
            .unwrap();
        assert!(matches!(result, Err(ExecutionError::Failed { .. })));
        assert_eq!(
            jobs.snapshot(awaiting).await.unwrap().state,
            JobState::Cancelled
        );
    }
}
