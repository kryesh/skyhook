//! Cancellation-safe commits and acknowledgments for event batches.

use super::SessionRuntime;

/// Messages and lifecycle notifications share one durable delivery receipt.
/// Preparing/presenting a batch is cancellable; its parent-history commit and
/// acknowledgement run to completion even if the caller is interrupted.
pub(in crate::agent::runtime) struct PendingEventBatch {
    pub(super) jobs: Option<crate::job::PendingDelivery>,
}

impl PendingEventBatch {
    pub(in crate::agent::runtime) async fn commit(
        self,
        runtime: &SessionRuntime,
        agent: &crate::identity::AgentId,
        message: crate::provider::protocol::Message,
    ) -> Result<u64, crate::agent::runtime::HarnessError> {
        let store = runtime.store.clone();
        let agent = agent.clone();
        tokio::spawn(async move {
            match self.jobs {
                Some(jobs) => jobs
                    .commit(message)
                    .await
                    .map_err(crate::agent::runtime::HarnessError::from),
                None => Ok(store
                    .append(
                        agent,
                        crate::session::SessionEvent::MessageCommitted { message },
                    )
                    .await?
                    .sequence),
            }
        })
        .await
        .map_err(|error| crate::session::SessionError::Io(std::io::Error::other(error)))?
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::super::tests::*;
    use crate::agent::runtime::*;
    use crate::job::{JobSpec, JobState};
    /// Publish through the same durable source-history path as a real child, without
    /// invoking another provider. The owner stays gated while a test prepares receipts.
    pub(in crate::agent::runtime::wait) async fn fixture_child(
        session: &SessionHandle,
        root: &Path,
    ) -> (JobId, AgentId) {
        let runtime = &session.runtime;
        let lease = runtime
            .jobs
            .create(JobSpec {
                background: true,
                ..JobSpec::test(session.root.clone(), "agent")
            })
            .await
            .unwrap();
        let child = session.root.child(123);
        runtime
            .store
            .append(
                child.clone(),
                SessionEvent::AgentStarted {
                    parent: Some(session.root.clone()),
                    owner_job: Some(lease.id),
                    model_profile: "child".into(),
                    max_context: None,
                    location: crate::execution::ExecutionLocation::root(root.to_path_buf()),
                },
            )
            .await
            .unwrap();
        runtime
            .jobs
            .transition(lease.id, JobState::Running)
            .await
            .unwrap();
        (lease.id, child)
    }

    pub(in crate::agent::runtime::wait) async fn fixture_message(
        session: &SessionHandle,
        job: JobId,
        child: &AgentId,
        text: &str,
    ) -> u64 {
        let runtime = &session.runtime;
        runtime
            .jobs
            .commit_child_message(
                child,
                job,
                Message::Assistant(vec![AssistantContent::text("fixture-reply", 0, text)]),
                text.to_owned(),
            )
            .await
            .unwrap()
    }

    async fn pending_content(
        session: &SessionHandle,
        root: &Path,
    ) -> (Vec<UserContent>, wait::PendingEventBatch) {
        let runtime = &session.runtime;
        runtime
            .pending_event_content(
                session.root_agent(),
                &runtime.harness.capabilities,
                &crate::execution::ExecutionLocation::root(root.to_path_buf()),
            )
            .await
            .unwrap()
    }

    /// Snapshotting must not consume the only durable copy of a child's progress.
    #[tokio::test]
    async fn child_progress_snapshot_survives_abandoned_parent_commits() {
        let workspace = tempfile::tempdir().unwrap();
        let tracking = Tracking::new(vec![("root", answer()), ("root", answer())]);
        let harness = harness(workspace.path(), tracking.clone()).await;
        let session = Arc::new(harness.new_session().await.unwrap());
        let runtime = &session.runtime;
        let turn = prompt(&session);
        tracking.request(0).await;
        let (job, child) = fixture_child(&session, workspace.path()).await;
        let first = fixture_message(&session, job, &child, "retained-progress").await;
        let (content, batch) = pending_content(&session, workspace.path()).await;
        assert_eq!(content.len(), 1);
        drop(batch);
        assert!(runtime.jobs.has_pending(session.root_agent()).await);

        let (retry, batch) = pending_content(&session, workspace.path()).await;
        assert_eq!(
            retry, content,
            "abandoning a receipt must retain its source message"
        );
        batch
            .commit(runtime, session.root_agent(), Message::User(retry))
            .await
            .unwrap();
        assert!(!runtime.jobs.has_pending(session.root_agent()).await);
        // Equal text is a new independent event when its source sequence differs.
        let second = fixture_message(&session, job, &child, "retained-progress").await;
        assert_ne!(first, second);
        let (remaining, batch) = pending_content(&session, workspace.path()).await;
        assert!(
            serde_json::to_string(&remaining)
                .unwrap()
                .contains("retained-progress")
        );
        drop(batch);
        tracking.release(0);
        let next = tracking.request(1).await;
        let delivered = agent_messages(&next);
        assert_eq!(delivered.len(), 2);
        assert_eq!(delivered[0]["message"], first);
        assert_eq!(delivered[1]["message"], second);
        assert!(!runtime.jobs.has_pending(session.root_agent()).await);
        tracking.release(1);
        bounded(turn).await.unwrap().unwrap();
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cancelling_a_started_notification_commit_finishes_both_acknowledgments() {
        cancelled_notification_commit(true).await;
    }

    #[tokio::test]
    async fn cancelling_a_started_child_only_commit_serializes_the_next_snapshot() {
        cancelled_notification_commit(false).await;
    }

    async fn cancelled_notification_commit(with_completion: bool) {
        let workspace = tempfile::tempdir().unwrap();
        let tracking = Tracking::new(vec![("root", answer()), ("root", answer())]);
        let harness = harness(workspace.path(), tracking.clone()).await;
        let session = Arc::new(harness.new_session().await.unwrap());
        let runtime = &session.runtime;
        let turn = prompt(&session);
        tracking.request(0).await;
        let completion = if with_completion {
            Some(complete_background(&session, "committed-completion").await)
        } else {
            None
        };
        let (job, child) = fixture_child(&session, workspace.path()).await;
        fixture_message(&session, job, &child, "committed-progress").await;
        let location = crate::execution::ExecutionLocation::root(workspace.path().to_path_buf());
        let (content, batch) = runtime
            .pending_event_content(
                session.root_agent(),
                &runtime.harness.capabilities,
                &location,
            )
            .await
            .unwrap();
        assert_eq!(
            content.len(),
            1,
            "progress and completion share the same envelope"
        );
        assert!(runtime.jobs.has_pending(session.root_agent()).await);
        let mut records = runtime.store.subscribe();
        {
            let commit = batch.commit(runtime, session.root_agent(), Message::User(content));
            tokio::pin!(commit);
            // This current-thread test has not yielded: the owned commit task has
            // been scheduled but cannot have run yet. Dropping its caller models an
            // interrupt exactly after this transaction's ownership boundary.
            assert!(futures_util::poll!(commit.as_mut()).is_pending());
        }
        {
            // An immediate resumed request cannot snapshot progress still owned by
            // the interrupted caller's transaction, even without any terminal job.
            let next_batch = runtime.pending_event_content(
                session.root_agent(),
                &runtime.harness.capabilities,
                &location,
            );
            tokio::pin!(next_batch);
            assert!(futures_util::poll!(next_batch.as_mut()).is_pending());
        }
        bounded(async {
            loop {
                let record = records.recv().await.unwrap();
                if matches!(
                    record.event,
                    SessionEvent::MessageCommitted {
                        message: Message::User(_)
                    }
                ) {
                    break;
                }
            }
            while runtime.jobs.has_pending(session.root_agent()).await {
                tokio::task::yield_now().await;
            }
        })
        .await;
        // Duplicate wake commands do not reconstruct already acknowledged output.
        session.root_tx.jobs_ready();
        session.root_tx.jobs_ready();
        tracking.release(0);
        bounded(turn).await.unwrap().unwrap();
        let turn = prompt(&session);
        let next = tracking.request(1).await;
        assert_eq!(agent_messages(&next).len(), 1);
        assert_eq!(events(&next).len(), usize::from(with_completion));
        if with_completion {
            assert_eq!(events(&next)[0]["id"], completion.unwrap().get());
        }
        tracking.release(1);
        bounded(turn).await.unwrap().unwrap();
        session.shutdown().await.unwrap();
    }
}
