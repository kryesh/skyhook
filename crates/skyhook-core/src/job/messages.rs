//! Child replies are derived from their source history records, independently of
//! lifecycle delivery. There is deliberately no second message-publication event.
use super::delivery::DELIVERY_BATCH_BYTES;
use super::*;
use crate::provider::protocol::BlockContent;

const MESSAGE_BATCH_COUNT: usize = 128;

pub(super) fn visible_text(message: &Message) -> Option<String> {
    let Message::Assistant(items) = message else {
        return None;
    };
    Some(
        items
            .iter()
            .flat_map(|item| &item.blocks)
            .filter_map(|block| match &block.content {
                BlockContent::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect(),
    )
}

impl JobEntry {
    pub(super) fn has_pending(&self) -> bool {
        !self.messages.is_empty()
            || (self.background && self.deliverable() && self.delivery == DeliveryState::Pending)
    }

    pub(super) fn publish_message(&mut self, id: JobId, sequence: u64, text: String) {
        if text.is_empty() {
            return;
        }
        self.last_agent_message = Some(sequence);
        self.messages.push(AgentMessage {
            id,
            name: self.name.clone(),
            message: sequence,
            text,
        });
    }
}

impl JobManager {
    /// Commit a child's assistant history and publish its visible text as one
    /// cancellation-shielded operation. The source sequence is the delivery ID.
    /// Empty visible text is committed to history but produces no delivery/wake.
    ///
    /// `wake_owner` decides only *when* the owner is woken, never what is
    /// published: the durable message record is identical either way. A
    /// non-terminal reply (the child keeps working) wakes the owner immediately; a
    /// terminal reply is published silently so the invocation's resolution point —
    /// the owning job's own completion broadcast, or [`JobManager::notify_owner`]
    /// where that invocation does not finish — presents it in the same delivery
    /// batch as the completion envelope.
    pub(crate) async fn commit_child_message(
        &self,
        child: &AgentId,
        job: JobId,
        message: Message,
        text: String,
        wake_owner: bool,
    ) -> Result<u64, JobError> {
        // Refuse a projection that replay could not reproduce (including reasoning).
        if visible_text(&message).as_deref() != Some(text.as_str()) {
            return Err(JobError::Internal(
                "child message text does not match committed assistant text".into(),
            ));
        }
        let manager = self.clone();
        let child = child.clone();
        tokio::spawn(async move {
            let _delivery = manager.inner.delivery_operation.lock().await;
            let (owner, associated) = {
                let jobs = manager.inner.jobs.lock().await;
                let entry = jobs.get(&job).ok_or(JobError::Unknown(job))?;
                (entry.agent.clone(), entry.child.clone())
            };
            if child.parent().as_ref() != Some(&owner) {
                return Err(JobError::Internal("child job owner mismatch".into()));
            }
            let valid = if let Some(associated) = associated {
                associated == child
            } else {
                let mut valid = false;
                manager
                    .inner
                    .store
                    .visit_records_after(0, |records| {
                        valid = records.iter().any(|record| {
                            record.agent == child
                                && matches!(&record.event, SessionEvent::AgentStarted {
                                parent: Some(parent), owner_job: Some(owner_job), ..
                            } if parent == &owner && *owner_job == job)
                        });
                    })
                    .await;
                valid
            };
            if !valid {
                return Err(JobError::Internal(
                    "child job association missing or mismatched".into(),
                ));
            }
            let record = manager
                .inner
                .store
                .append(child.clone(), SessionEvent::MessageCommitted { message })
                .await?;
            let mut jobs = manager.inner.jobs.lock().await;
            let entry = jobs.get_mut(&job).ok_or(JobError::Unknown(job))?;
            entry.child = Some(child);
            let visible = !text.is_empty();
            entry.publish_message(job, record.sequence, text);
            if visible && wake_owner {
                // Foreground child replies are just as deliverable as background ones.
                let _ = manager
                    .inner
                    .completions
                    .send(JobCompletion { agent: owner, job });
            }
            Ok(record.sequence)
        })
        .await
        .map_err(|error| JobError::Internal(error.to_string()))?
    }

    /// Wake a child job's owner for replies that are already durably published.
    /// Nothing is published here, so delivery batching, acknowledgement, dedup and
    /// replay are untouched; this only replaces the wake that
    /// `commit_child_message(.., wake_owner: false)` deliberately withheld. Callers
    /// are the invocation resolution paths that do *not* finish the owning job,
    /// whose completion broadcast would otherwise be the wake.
    pub(crate) async fn notify_owner(&self, job: JobId) {
        let jobs = self.inner.jobs.lock().await;
        let Some(entry) = jobs.get(&job) else {
            return;
        };
        // Messages stay in the entry until acknowledged, so an empty list means the
        // owner has nothing to collect and must not be woken with an empty snapshot.
        if entry.messages.is_empty() {
            return;
        }
        let _ = self.inner.completions.send(JobCompletion {
            agent: entry.agent.clone(),
            job,
        });
    }

    /// Last committed *visible* child message, even after acknowledgement/resume.
    #[cfg(test)]
    pub(crate) async fn last_agent_message(&self, job: JobId) -> Result<Option<u64>, JobError> {
        let jobs = self.inner.jobs.lock().await;
        Ok(jobs
            .get(&job)
            .ok_or(JobError::Unknown(job))?
            .last_agent_message)
    }
}

fn message_size(message: &AgentMessage) -> usize {
    // Include the runtime kind discriminator and JSON array separator.
    serde_json::to_vec(message).map_or(DELIVERY_BATCH_BYTES, |bytes| bytes.len().saturating_add(18))
}

pub(super) fn batch_size(messages: &[AgentMessage]) -> usize {
    messages.iter().fold(0_usize, |bytes, message| {
        bytes.saturating_add(message_size(message))
    })
}

pub(super) fn pending_messages(
    jobs: &HashMap<JobId, JobEntry>,
    owner: &AgentId,
) -> Vec<AgentMessage> {
    let mut messages: Vec<_> = jobs
        .values()
        .filter(|entry| &entry.agent == owner)
        .flat_map(|entry| &entry.messages)
        .collect();
    messages.sort_by_key(|message| message.message);
    let mut budget: usize = 0;
    let mut pending = Vec::new();
    for message in messages.into_iter().take(MESSAGE_BATCH_COUNT) {
        let cost = message_size(message);
        if !pending.is_empty() && budget.saturating_add(cost) > DELIVERY_BATCH_BYTES {
            break;
        }
        // Like lifecycle output, always allow one oversized item to make progress.
        budget = budget.saturating_add(cost);
        pending.push(message.clone());
    }
    pending
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::protocol::{AssistantContent, BlockContent, Message, UserContent};

    async fn child_job(
        background: bool,
    ) -> (tempfile::TempDir, JobManager, AgentId, AgentId, JobId) {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::create(root.path()).await.unwrap();
        let owner = AgentId::root(store.id());
        let child = owner.child(1);
        let manager = JobManager::new(store);
        let spec = JobSpec {
            background,
            ..JobSpec::test(owner.clone(), "agent")
        };
        let job = manager.test_create(spec).await;
        let started = SessionEvent::AgentStarted {
            parent: Some(owner.clone()),
            owner_job: Some(job),
            model_profile: "test".into(),
            max_context: None,
            location: ExecutionLocation::root(".".into()),
        };
        manager.test_append(child.clone(), started).await;
        manager.transition(job, JobState::Running).await.unwrap();
        (root, manager, owner, child, job)
    }

    fn assistant(text: &str) -> Message {
        Message::Assistant(vec![AssistantContent::text("text", 0, text)])
    }

    fn runtime_text(text: String) -> Message {
        Message::User(vec![UserContent::Runtime { text }])
    }

    fn job_events(items: impl serde::Serialize) -> Message {
        let items = serde_json::to_string(&items).unwrap();
        runtime_text(format!(
            "<skyhook_job_events>\n{items}\n</skyhook_job_events>"
        ))
    }

    async fn commit(manager: &JobManager, child: &AgentId, job: JobId, text: &str) -> u64 {
        let message = assistant(text);
        manager
            .commit_child_message(child, job, message, text.into(), true)
            .await
            .unwrap()
    }

    fn notification(messages: &[AgentMessage], envelopes: &[JobEnvelope]) -> Message {
        let messages = messages.iter().map(|message| {
            let mut value = serde_json::to_value(message).unwrap();
            value["kind"] = serde_json::json!("message");
            value
        });
        let envelopes = envelopes
            .iter()
            .map(|envelope| serde_json::to_value(envelope).unwrap());
        job_events(messages.chain(envelopes).collect::<Vec<_>>())
    }

    /// Acknowledge everything the receipt presents.
    async fn ack(receipt: PendingDelivery) {
        let message = notification(receipt.messages(), receipt.envelopes());
        receipt.commit(message).await.unwrap();
    }

    async fn finish(manager: &JobManager, job: JobId) {
        manager
            .test_finish(job, serde_json::json!("saved result"))
            .await;
    }

    fn sequences(receipt: &PendingDelivery) -> Vec<u64> {
        receipt
            .messages()
            .iter()
            .map(|message| message.message)
            .collect()
    }

    async fn first_pending(manager: &JobManager, owner: &AgentId) -> AgentMessage {
        manager.pending_delivery(owner).await.unwrap().messages()[0].clone()
    }

    #[tokio::test]
    async fn messages_are_independent_of_claims_and_foreground_lifecycle() {
        let (_root, manager, owner, child, job) = child_job(false).await;
        let mut wakes = manager.subscribe_completions();
        let first = commit(&manager, &child, job, "first").await;
        assert_eq!(wakes.recv().await.unwrap().agent, owner);
        let second = commit(&manager, &child, job, "second").await;
        assert_eq!(wakes.recv().await.unwrap().job, job);
        finish(&manager, job).await;
        manager.claim(job).await.unwrap();
        assert!(manager.has_pending(&owner).await);
        let receipt = manager.pending_delivery(&owner).await.unwrap();
        assert!(receipt.envelopes().is_empty());
        assert_eq!(sequences(&receipt), vec![first, second]);
        let acknowledgement = notification(&receipt.messages()[..1], &[]);
        receipt.commit(acknowledgement).await.unwrap();
        let restored = manager.test_replay().await;
        let receipt = restored.pending_delivery(&owner).await.unwrap();
        assert_eq!(sequences(&receipt), [second]);
        ack(receipt).await;
        assert!(!restored.has_pending(&owner).await);
        assert_eq!(
            restored.last_agent_message(job).await.unwrap(),
            Some(second)
        );
        let output = restored.snapshot(job).await.unwrap().output;
        assert_eq!(output, Some(serde_json::json!("saved result")));
        assert!(!restored.test_replay().await.has_pending(&owner).await);
    }

    #[tokio::test]
    async fn replay_derives_publication_and_ack_from_source_history_only() {
        let (_root, manager, owner, child, job) = child_job(false).await;
        let mut item = AssistantContent::text("visible", 0, "visible");
        item.blocks.push(crate::provider::protocol::AssistantBlock {
            id: "secret".into(),
            position: 1,
            content: BlockContent::Reasoning {
                text: "private reasoning".into(),
            },
        });
        // Simulate a crash after committing history but before in-memory publication.
        let message = Message::Assistant(vec![item]);
        let source = manager
            .test_append(child.clone(), SessionEvent::MessageCommitted { message })
            .await;
        commit(&manager, &child, job, "").await;
        finish(&manager, job).await;
        let restored = manager.test_replay().await;
        let receipt = restored.pending_delivery(&owner).await.unwrap();
        let expected = AgentMessage {
            id: job,
            name: None,
            message: source,
            text: "visible".into(),
        };
        assert_eq!(receipt.messages(), &[expected]);
        let message = notification(receipt.messages(), &[]);
        drop(receipt);
        // Simulate a crash after parent history append but before in-memory ACK.
        manager
            .test_append(owner.clone(), SessionEvent::MessageCommitted { message })
            .await;
        let restored = manager.test_replay().await;
        assert!(!restored.has_pending(&owner).await);
        assert_eq!(
            restored.last_agent_message(job).await.unwrap(),
            Some(source)
        );
    }

    #[tokio::test]
    async fn dropped_receipt_and_failed_parent_commit_keep_messages_and_lifecycle_pending() {
        let (_root, manager, owner, child, job) = child_job(true).await;
        let sequence = commit(&manager, &child, job, "reply").await;
        finish(&manager, job).await;
        drop(manager.pending_delivery(&owner).await.unwrap());
        let mut receipt = manager.pending_delivery(&owner).await.unwrap();
        let message = notification(receipt.messages(), receipt.envelopes());
        receipt.owner = AgentId::root(crate::identity::SessionId::generate().unwrap());
        assert!(receipt.commit(message).await.is_err());
        for state in [manager.clone(), manager.test_replay().await] {
            let receipt = state.pending_delivery(&owner).await.unwrap();
            assert_eq!(sequences(&receipt), [sequence]);
            assert_eq!(receipt.envelopes()[0].id, job);
        }
    }

    /// Message and lifecycle batches share one byte budget; later batches rewake
    /// the owner and completion never overtakes undelivered messages.
    #[tokio::test]
    async fn bounded_batches_rewake_and_do_not_overtake_messages_with_completion() {
        // (message sizes, expected (messages, envelopes) per batch)
        let cases = [
            (vec![5000; 3], vec![(1, 0), (1, 0), (1, 1)]),
            (vec![9000], vec![(1, 0), (0, 1)]),
        ];
        for (sizes, batches) in cases {
            let (_root, manager, owner, child, job) = child_job(true).await;
            let mut pending = Vec::new();
            for size in sizes {
                pending.push(commit(&manager, &child, job, &"x".repeat(size)).await);
            }
            finish(&manager, job).await;
            let mut wakes = manager.subscribe_completions();
            for (index, (messages, envelopes)) in batches.iter().enumerate() {
                let receipt = manager.pending_delivery(&owner).await.unwrap();
                assert_eq!(
                    sequences(&receipt),
                    pending.drain(..*messages).collect::<Vec<_>>()
                );
                assert_eq!(receipt.envelopes().len(), *envelopes);
                ack(receipt).await;
                if index + 1 < batches.len() {
                    assert_eq!(wakes.try_recv().unwrap().job, job);
                }
            }
            assert!(!manager.has_pending(&owner).await);
            assert!(wakes.try_recv().is_err());
            assert!(!manager.test_replay().await.has_pending(&owner).await);
        }
    }

    /// A terminal child reply is published durably but silently, so the job's own
    /// completion is the single wake that presents both items in one batch.
    #[tokio::test]
    async fn deferred_reply_waits_for_its_completion_and_notify_owner_covers_the_rest() {
        let (_root, manager, owner, child, job) = child_job(true).await;
        let mut wakes = manager.subscribe_completions();
        let message = assistant("terminal answer");
        let sequence = manager
            .commit_child_message(&child, job, message, "terminal answer".into(), false)
            .await
            .unwrap();
        // Durable and deliverable, but the owner is not woken for it on its own.
        assert!(manager.has_pending(&owner).await);
        assert!(wakes.try_recv().is_err());
        assert_eq!(
            manager.last_agent_message(job).await.unwrap(),
            Some(sequence)
        );

        // An invocation that does not finish wakes explicitly instead.
        manager.notify_owner(job).await;
        assert_eq!(wakes.try_recv().unwrap().job, job);
        manager.notify_owner(job).await;
        assert_eq!(wakes.try_recv().unwrap().agent, owner);

        // The completion wake presents reply and envelope as one receipt.
        finish(&manager, job).await;
        assert_eq!(wakes.try_recv().unwrap().job, job);
        let receipt = manager.pending_delivery(&owner).await.unwrap();
        assert_eq!(sequences(&receipt), [sequence]);
        assert_eq!(receipt.envelopes().len(), 1);
        ack(receipt).await;
        // Nothing pending: a wake with nothing to present is never sent.
        assert!(!manager.has_pending(&owner).await);
        while wakes.try_recv().is_ok() {}
        manager.notify_owner(job).await;
        assert!(wakes.try_recv().is_err());
        assert!(!manager.test_replay().await.has_pending(&owner).await);
    }

    #[tokio::test]
    async fn message_ack_does_not_claim_lifecycle_and_resume_does_not_claim_messages() {
        let (_root, manager, owner, child, job) = child_job(true).await;
        let first = commit(&manager, &child, job, "before question").await;
        let question = serde_json::json!({"question":"continue?"});
        manager.request_input(job, question).await.unwrap();
        let receipt = manager.pending_delivery(&owner).await.unwrap();
        let malicious_state = job_events(serde_json::json!([{
            "kind":"message", "id":job, "message":first, "text":"before question", "state":"waiting_input"
        }]));
        receipt.commit(malicious_state).await.unwrap();
        assert_eq!(
            manager
                .pending_delivery(&owner)
                .await
                .unwrap()
                .envelopes()
                .len(),
            1
        );
        let second = commit(&manager, &child, job, "after question").await;
        manager.resume_input(job).await.unwrap();
        let receipt = manager.pending_delivery(&owner).await.unwrap();
        assert!(receipt.envelopes().is_empty());
        assert_eq!(receipt.messages()[0].message, second);
        drop(receipt);
        finish(&manager, job).await;
        let restored = manager.test_replay().await;
        assert_eq!(
            restored.last_agent_message(job).await.unwrap(),
            Some(second)
        );
        assert_eq!(first_pending(&restored, &owner).await.message, second);
    }

    #[tokio::test]
    async fn child_commit_is_shielded_from_cancellation_and_serialized_with_receipts() {
        let (_root, manager, owner, child, job) = child_job(false).await;
        let receipt = manager.pending_delivery(&owner).await.unwrap();
        let mut wakes = manager.subscribe_completions();
        let message = assistant("survives");
        let mut pending =
            Box::pin(manager.commit_child_message(&child, job, message, "survives".into(), true));
        // Poll through the internal spawn, then cancel the caller while the spawned
        // transaction is waiting on the receipt's delivery gate.
        assert!(
            tokio::time::timeout(Duration::from_millis(20), pending.as_mut())
                .await
                .is_err()
        );
        drop(pending);
        assert!(wakes.try_recv().is_err());
        drop(receipt);
        tokio::time::timeout(Duration::from_secs(2), wakes.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first_pending(&manager, &owner).await.text, "survives");
        finish(&manager, job).await;
        assert_eq!(
            first_pending(&manager.test_replay().await, &owner)
                .await
                .text,
            "survives"
        );
    }

    #[tokio::test]
    async fn invalid_association_or_projection_never_commits_and_empty_text_never_wakes() {
        let (_root, manager, owner, child, job) = child_job(false).await;
        let before = manager.store().records().await.len();
        for (author, projection) in [
            (child.clone(), "private"),
            (owner.child(2), "visible"),
            (owner.clone(), "visible"),
        ] {
            let message = assistant("visible");
            let result =
                manager.commit_child_message(&author, job, message, projection.into(), true);
            assert!(result.await.is_err());
        }
        assert_eq!(manager.store().records().await.len(), before);
        let mut wakes = manager.subscribe_completions();
        commit(&manager, &child, job, "").await;
        assert_eq!(manager.store().records().await.len(), before + 1);
        assert_eq!(manager.last_agent_message(job).await.unwrap(), None);
        assert!(!manager.has_pending(&owner).await);
        assert!(wakes.try_recv().is_err());
    }

    #[tokio::test]
    async fn legacy_runtime_ack_supported_but_user_text_and_wrong_owner_ignored() {
        let (_root, manager, owner, child, job) = child_job(false).await;
        commit(&manager, &child, job, "reply").await;
        finish(&manager, job).await;
        let receipt = manager.pending_delivery(&owner).await.unwrap();
        let messages = serde_json::to_string(receipt.messages()).unwrap();
        let text = format!("<skyhook_agent_messages>\n{messages}\n</skyhook_agent_messages>");
        let user_text = Message::User(vec![UserContent::Text { text: text.clone() }]);
        receipt.commit(user_text).await.unwrap();
        let message = runtime_text(text.clone());
        manager
            .test_append(child, SessionEvent::MessageCommitted { message })
            .await;
        assert!(manager.test_replay().await.has_pending(&owner).await);
        let receipt = manager.pending_delivery(&owner).await.unwrap();
        receipt.commit(runtime_text(text)).await.unwrap();
        assert!(!manager.test_replay().await.has_pending(&owner).await);
    }

    #[tokio::test]
    async fn blocked_low_id_child_does_not_budget_out_unrelated_completion() {
        let (_root, manager, owner, child, job) = child_job(true).await;
        for _ in 0..3 {
            commit(&manager, &child, job, &"report".repeat(500)).await;
        }
        manager
            .test_finish(job, serde_json::json!("large".repeat(10000)))
            .await;
        let spec = JobSpec {
            background: true,
            ..JobSpec::test(owner.clone(), "ordinary")
        };
        let ordinary = manager.test_create(spec).await;
        finish(&manager, ordinary).await;
        assert!(job < ordinary);
        let receipt = manager.pending_delivery(&owner).await.unwrap();
        assert_eq!(receipt.messages().len(), 2);
        let envelopes: Vec<_> = receipt
            .envelopes()
            .iter()
            .map(|envelope| envelope.id)
            .collect();
        assert_eq!(envelopes, [ordinary]);
        assert!(messages::batch_size(receipt.messages()) < DELIVERY_BATCH_BYTES);
        ack(receipt).await;
        assert!(manager.has_pending(&owner).await);
    }

    #[tokio::test]
    async fn legacy_completed_result_acknowledges_only_exact_last_source_reply() {
        for result in ["final reply", "final rep"] {
            let (_root, manager, owner, child, job) = child_job(true).await;
            let first = commit(&manager, &child, job, "earlier undelivered report").await;
            let last = commit(&manager, &child, job, "final reply").await;
            manager
                .test_finish(job, serde_json::json!("final reply"))
                .await;
            // Old final replies were delivered as lifecycle `result`, without a
            // message discriminator/source sequence or the new last_message field.
            let message =
                job_events(serde_json::json!([{"id":job, "state":"completed", "result":result}]));
            manager
                .test_append(owner.clone(), SessionEvent::MessageCommitted { message })
                .await;
            let restored = manager.test_replay().await;
            let receipt = restored.pending_delivery(&owner).await.unwrap();
            assert!(receipt.envelopes().is_empty());
            let expected = if result == "final reply" {
                vec![first]
            } else {
                vec![first, last]
            };
            assert_eq!(sequences(&receipt), expected);
            assert_eq!(restored.last_agent_message(job).await.unwrap(), Some(last));
        }
    }

    #[tokio::test]
    async fn claims_without_parent_delivery_evidence_never_acknowledge_last_reply() {
        for background in [false, true] {
            let (_root, manager, owner, child, job) = child_job(background).await;
            let last = commit(&manager, &child, job, "final reply").await;
            manager
                .test_finish(job, serde_json::json!("final reply"))
                .await;
            manager.claim(job).await.unwrap();
            assert_eq!(
                first_pending(&manager.test_replay().await, &owner)
                    .await
                    .message,
                last
            );
        }
    }
}
