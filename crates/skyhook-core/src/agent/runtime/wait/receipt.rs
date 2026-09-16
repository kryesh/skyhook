//! Cancellation-safe commits and acknowledgments for event batches.

/// A one-shot notification: message, recipient and store are bound during
/// preparation. The inner receipt owns delivery acknowledgement and shields
/// accepted append through live publication when its caller is cancelled.
pub(in crate::agent::runtime) struct PendingEventBatch {
    pub(super) jobs: Option<crate::job::PendingDelivery>,
    pub(super) store: crate::session::SessionStore,
    pub(super) agent: crate::identity::AgentId,
    pub(super) message: crate::provider::protocol::Message,
}

impl PendingEventBatch {
    pub(in crate::agent::runtime) async fn commit(
        self,
    ) -> Result<u64, crate::agent::runtime::HarnessError> {
        let Self {
            jobs,
            store,
            agent,
            message,
        } = self;
        match jobs {
            Some(jobs) => Ok(jobs.commit(message).await?),
            None => Ok(store
                .append(
                    agent,
                    crate::session::SessionEvent::MessageCommitted { message },
                )
                .await?
                .sequence),
        }
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
        let spec = JobSpec {
            background: true,
            role: crate::job::JobRole::Agent,
            ..JobSpec::test(session.root.clone(), "agent")
        };
        let id = runtime.jobs.create(spec).await.unwrap().into_test_id();
        let child = session.root.child(123);
        let started = crate::session::fixture::child_started(
            Some(session.root.clone()),
            Some(id),
            crate::execution::ExecutionLocation::root(root.to_path_buf()),
        );
        runtime.store.append(child.clone(), started).await.unwrap();
        runtime
            .jobs
            .transition(id, JobState::Running)
            .await
            .unwrap();
        (id, child)
    }

    pub(in crate::agent::runtime::wait) async fn fixture_message(
        session: &SessionHandle,
        job: JobId,
        child: &AgentId,
        text: &str,
    ) -> u64 {
        let message = Message::Assistant(vec![AssistantContent::text("fixture-reply", 0, text)]);
        let jobs = &session.runtime.jobs;
        // Fixture progress stands in for a non-terminal child reply: it wakes the owner.
        let committed =
            jobs.commit_child_message(child, job, message, text.to_owned(), true, |_| Vec::new());
        committed.await.unwrap()
    }

    async fn pending_content(
        session: &SessionHandle,
        root: &Path,
    ) -> (Vec<UserContent>, wait::PendingEventBatch) {
        let runtime = &session.runtime;
        let location = crate::execution::ExecutionLocation::root(root.to_path_buf());
        let capabilities = &runtime.harness.capabilities;
        let pending = runtime.pending_event_content(session.root_agent(), capabilities, &location);
        pending.await.unwrap()
    }

    /// Snapshotting must not consume the only durable copy of a child's progress.
    #[tokio::test]
    async fn child_progress_snapshot_survives_abandoned_parent_commits() {
        let tracking = Tracking::new(vec![("root", answer()), ("root", answer())]);
        let (root, session) = start(&tracking).await;
        let runtime = &session.runtime;
        let turn = prompt(&session);
        tracking.request(0).await;
        let (job, child) = fixture_child(&session, root.path()).await;
        let first = fixture_message(&session, job, &child, "retained-progress").await;
        let (content, batch) = pending_content(&session, root.path()).await;
        assert_eq!(content.len(), 1);
        drop(batch);
        assert!(runtime.jobs.has_pending(session.root_agent()).await);

        let (retry, batch) = pending_content(&session, root.path()).await;
        // abandoning a receipt must retain its source message
        assert_eq!(retry, content);
        let sequence = batch.commit().await.unwrap();
        let records = runtime.store.records().await;
        let committed = records.iter().find(|record| record.sequence == sequence);
        let committed = committed.unwrap();
        assert_eq!(&committed.agent, session.root_agent());
        assert!(matches!(&committed.event,
            SessionEvent::MessageCommitted { message: Message::User(saved) } if saved == &content));
        assert!(!runtime.jobs.has_pending(session.root_agent()).await);
        // Equal text is a new independent event when its source sequence differs.
        let second = fixture_message(&session, job, &child, "retained-progress").await;
        assert_ne!(first, second);
        let (remaining, batch) = pending_content(&session, root.path()).await;
        let remaining = serde_json::to_string(&remaining).unwrap();
        assert!(remaining.contains("retained-progress"));
        drop(batch);
        tracking.release(0);
        let next = tracking.pass(1).await;
        let delivered = agent_messages(&next);
        assert_eq!(delivered.len(), 2);
        let found = (&delivered[0]["message"], &delivered[1]["message"]);
        assert_eq!(found, (&json!(first), &json!(second)));
        assert!(!runtime.jobs.has_pending(session.root_agent()).await);
        bounded(turn).await.unwrap().unwrap();
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cancelling_a_started_commit_finishes_acknowledgments_and_serializes_the_next_snapshot()
    {
        for with_completion in [true, false] {
            cancelled_notification_commit(with_completion).await;
        }
    }

    async fn cancelled_notification_commit(with_completion: bool) {
        let tracking = Tracking::new(vec![("root", answer()), ("root", answer())]);
        let (root, session) = start(&tracking).await;
        let runtime = &session.runtime;
        let turn = prompt(&session);
        tracking.request(0).await;
        let completion = if with_completion {
            Some(complete_background(&session, &session.root, "committed-completion").await)
        } else {
            None
        };
        let (job, child) = fixture_child(&session, root.path()).await;
        fixture_message(&session, job, &child, "committed-progress").await;
        let (content, batch) = pending_content(&session, root.path()).await;
        // progress and completion share the same envelope
        assert_eq!(content.len(), 1);
        assert!(runtime.jobs.has_pending(session.root_agent()).await);
        let mut records = runtime.store.subscribe();
        {
            let commit = batch.commit();
            tokio::pin!(commit);
            // Unpolled since scheduling: dropping the caller models an interrupt just
            // after this transaction's ownership boundary.
            assert!(futures_util::poll!(commit.as_mut()).is_pending());
        }
        {
            // An immediate resumed request cannot snapshot progress still owned by
            // the interrupted caller's transaction, even without any terminal job.
            let next_batch = pending_content(&session, root.path());
            tokio::pin!(next_batch);
            assert!(futures_util::poll!(next_batch.as_mut()).is_pending());
        }
        bounded(async {
            loop {
                let record = records.recv().await.unwrap();
                if matches!(&record.event, SessionEvent::MessageCommitted { message } if matches!(message, Message::User(_))) {
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
        let next = tracking.pass(1).await;
        assert_eq!(agent_messages(&next).len(), 1);
        let completions = events(&next)
            .iter()
            .map(|event| event["id"].clone())
            .collect::<Vec<_>>();
        let expected = completion.map(|job| json!(job.get()));
        assert_eq!(completions, expected.into_iter().collect::<Vec<_>>());
        bounded(turn).await.unwrap().unwrap();
        session.shutdown().await.unwrap();
    }
}
