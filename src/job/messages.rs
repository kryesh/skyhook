//! Child replies, derived from their source history records independently of
//! lifecycle delivery.
use super::delivery::MESSAGE_BATCH_BYTES;
use super::*;
use crate::session::Message;

const MESSAGE_BATCH_COUNT: usize = 128;

/// What a child reply does beyond being recorded. Only a background child's
/// owner is delivered to.
#[derive(Clone, Copy)]
pub(super) enum Publish {
    /// A foreground child's replies are read through its result.
    Record,
    /// Queue it now and wake the owner.
    Wake,
    /// Queue it when the invocation resolves.
    Withhold,
}

/// The child reply this record projects; empty for a whitespace-only turn.
pub(super) fn visible_text(message: &Message) -> Option<String> {
    let Message::Assistant(items) = message else {
        return None;
    };
    Some(crate::provider::protocol::visible_text(items))
}

impl JobEntry {
    pub(super) fn has_pending(&self) -> bool {
        self.pending_since(0)
    }

    /// Whether anything still pending became pending after stamp `floor`.
    pub(super) fn pending_since(&self, floor: u64) -> bool {
        self.child()
            .is_some_and(|child| !child.messages.is_empty() && child.message_stamp > floor)
            || (self.background
                && self.deliverable()
                && self.delivery == DeliveryState::Pending
                && self.delivery_stamp > floor)
    }

    /// Record a child reply; reports whether it was queued for delivery. A blank
    /// turn is not a reply.
    pub(super) fn publish_message(
        &mut self,
        id: JobId,
        sequence: MessageSeq,
        text: String,
        publish: Publish,
    ) -> bool {
        let name = self.name.clone().map(String::from);
        let finished = self.end().is_some();
        let Some(child) = self.child_mut() else {
            return false;
        };
        if text.is_empty() {
            return false;
        }
        child.last_message = Some(sequence);
        let message = AgentMessage {
            id,
            name,
            message: sequence,
            text,
        };
        match publish {
            Publish::Record => false,
            // A finished job has already released what it withheld; a reply that
            // lands after its end is delivered on its own.
            Publish::Withhold if !finished => {
                // Replies deliver oldest first: an earlier withheld reply goes ahead.
                child.release();
                child.withheld = Some(message);
                false
            }
            Publish::Wake | Publish::Withhold => {
                child.queue(message);
                true
            }
        }
    }

    /// Drop the queued reply committed at `source`: its owner has it.
    pub(super) fn deliver_message(&mut self, source: MessageSeq) {
        if let Some(child) = self.child_mut() {
            child.messages.retain(|message| message.message != source);
        }
    }
}

impl JobManager {
    /// Commit a child's assistant history and publish its visible text as one
    /// cancellation-shielded operation. The source sequence is the delivery ID.
    /// Empty visible text is committed to history but produces no delivery/wake.
    ///
    /// `wake_owner` decides when the reply is delivered. A terminal reply passes
    /// `false`: it is withheld until the job's completion (or
    /// [`JobManager::notify_owner`]) presents it in the same delivery batch as the
    /// completion envelope, so no request boundary in between shows it alone. A
    /// reply committed after the job already finished wakes the owner at once.
    pub(crate) async fn commit_child_message(
        &self,
        child: &AgentId,
        job: JobId,
        message: Message,
        text: String,
        wake_owner: bool,
        follow: impl FnOnce(RecordSeq) -> Vec<(AgentId, SessionEvent)> + Send + 'static,
    ) -> Result<MessageSeq, JobError> {
        // Refuse a projection that replay could not reproduce (including reasoning).
        if visible_text(&message).as_deref() != Some(text.as_str()) {
            return Err(JobError::ChildTextMismatch);
        }
        let manager = self.clone();
        let child = child.clone();
        tokio::spawn(async move {
            let _delivery = manager.inner.delivery_operation.lock().await;
            let (owner, associated) = {
                let jobs = manager.inner.jobs.lock().await;
                let entry = jobs.get(&job).ok_or(JobError::Unknown(job))?;
                let launched = entry.child().ok_or(JobError::NoChild(job))?;
                (entry.agent.clone(), launched.agent.clone())
            };
            if child.parent().as_ref() != Some(&owner) {
                return Err(JobError::ChildOwnerMismatch(job));
            }
            let valid = if let Some(associated) = associated {
                associated == child
            } else {
                let mut valid = false;
                manager
                    .inner
                    .store
                    .visit_records_after(RecordSeq::default(), |records| {
                        valid = records.iter().any(|record| {
                            record.agent == child
                                && matches!(&record.event, SessionEvent::AgentStarted {
                                owner_job: Some(owner_job), ..
                            } if *owner_job == job)
                        });
                    })
                    .await;
                valid
            };
            if !valid {
                return Err(JobError::ChildOwnerMismatch(job));
            }
            let record = manager
                .inner
                .store
                .append_then(
                    child.clone(),
                    SessionEvent::MessageCommitted { message },
                    follow,
                )
                .await?
                .swap_remove(0);
            let mut jobs = manager.inner.jobs.lock().await;
            let publish = if !views::effectively_background(&jobs, job) {
                Publish::Record
            } else if wake_owner {
                Publish::Wake
            } else {
                Publish::Withhold
            };
            let entry = jobs.get_mut(&job).ok_or(JobError::Unknown(job))?;
            if let Some(launched) = entry.child_mut() {
                launched.agent = Some(child);
            }
            let sequence = record.sequence.message();
            let published = entry.publish_message(job, sequence, text, publish);
            if published {
                let _ = manager
                    .inner
                    .completions
                    .send(JobCompletion { agent: owner, job });
            }
            Ok(sequence)
        })
        .await
        .map_err(|error| JobError::OwnerLost {
            owner: "child message delivery",
            reason: error.to_string(),
        })?
    }

    /// Deliver the reply `commit_child_message(.., wake_owner: false)` withheld and
    /// wake the owner for it, for invocations that resolve without finishing the
    /// owning job.
    pub(crate) async fn notify_owner(&self, job: JobId) {
        let mut jobs = self.inner.jobs.lock().await;
        let Some(entry) = jobs.get_mut(&job) else {
            return;
        };
        // Never wake the owner with nothing to collect.
        let Some(child) = entry.child_mut() else {
            return;
        };
        child.release();
        if child.messages.is_empty() {
            return;
        }
        let _ = self.inner.completions.send(JobCompletion {
            agent: entry.agent.clone(),
            job,
        });
    }

    /// Last committed *visible* child message, even after acknowledgement/resume.
    #[cfg(test)]
    pub(crate) async fn last_agent_message(
        &self,
        job: JobId,
    ) -> Result<Option<MessageSeq>, JobError> {
        self.entry(job, |entry| entry.child()?.last_message).await
    }
}

fn message_size(message: &AgentMessage) -> usize {
    // Include the runtime kind discriminator and JSON array separator.
    serde_json::to_vec(message).map_or(MESSAGE_BATCH_BYTES, |bytes| bytes.len().saturating_add(18))
}

/// Presented size of a whole reply batch.
#[cfg(test)]
pub(super) fn batch_size(messages: &[AgentMessage]) -> usize {
    messages.iter().fold(0_usize, |bytes, message| {
        bytes.saturating_add(message_size(message))
    })
}

/// Replies in sequence order, bounded by count and by their own byte budget.
pub(super) fn pending_messages(
    jobs: &HashMap<JobId, JobEntry>,
    owner: &AgentId,
) -> Vec<AgentMessage> {
    let mut messages: Vec<_> = jobs
        .values()
        .filter(|entry| &entry.agent == owner)
        .filter_map(JobEntry::child)
        .flat_map(|child| &child.messages)
        .collect();
    messages.sort_by_key(|message| message.message);
    let mut budget: usize = 0;
    let mut pending = Vec::new();
    for message in messages.into_iter().take(MESSAGE_BATCH_COUNT) {
        let cost = message_size(message);
        if !pending.is_empty() && budget.saturating_add(cost) > MESSAGE_BATCH_BYTES {
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
    use crate::job::tests::job_events;
    use crate::provider::protocol::AssistantItem;
    use crate::session::JobEvent;

    use crate::job::delivery::LIFECYCLE_BATCH_BYTES;

    async fn child_job(
        background: bool,
    ) -> (tempfile::TempDir, JobManager, AgentId, AgentId, JobId) {
        let session = crate::session::fixture::MemorySession::new().await;
        let owner = session.agent.clone();
        let manager = JobManager::new(session.store.clone());
        let (child, job) = agent_job(&session, &manager, &owner, 1, background).await;
        (session.root, manager, owner, child, job)
    }

    /// Session, manager and owner without any job, so a test controls job IDs.
    async fn owner_session() -> (crate::session::fixture::MemorySession, JobManager, AgentId) {
        let session = crate::session::fixture::MemorySession::new().await;
        let owner = session.agent.clone();
        let manager = JobManager::new(session.store.clone());
        (session, manager, owner)
    }

    /// A running child-agent job and its child agent.
    async fn agent_job(
        session: &crate::session::fixture::MemorySession,
        manager: &JobManager,
        owner: &AgentId,
        index: u32,
        background: bool,
    ) -> (AgentId, JobId) {
        let spec = JobSpec {
            background,
            role: JobRole::Agent,
            ..JobSpec::test(owner.clone(), "agent")
        };
        let job = manager.test_running(spec).await.into_test_id();
        let child = session.start_child(owner, index, Some(job)).await;
        (child, job)
    }

    /// A running background tool job for `owner`.
    async fn tool_job(manager: &JobManager, owner: &AgentId) -> JobId {
        let spec = JobSpec {
            background: true,
            ..JobSpec::test(owner.clone(), "exec")
        };
        manager.test_running(spec).await.into_test_id()
    }

    /// A saved result too large to present inline.
    async fn finish_bulky(manager: &JobManager, job: JobId) {
        let result = serde_json::json!("y".repeat(2 * output::PAGE_BYTES));
        manager.test_finish(job, result).await;
    }

    fn envelope_ids(receipt: &PendingDelivery) -> Vec<JobId> {
        receipt
            .envelopes()
            .iter()
            .map(|envelope| envelope.id)
            .collect()
    }

    fn assistant(text: &str) -> Message {
        Message::Assistant(vec![AssistantItem::text("text", 0, text)])
    }

    async fn commit(manager: &JobManager, child: &AgentId, job: JobId, text: &str) -> MessageSeq {
        let message = assistant(text);
        manager
            .commit_child_message(child, job, message, text.into(), true, |_| Vec::new())
            .await
            .unwrap()
    }

    fn notification(messages: &[AgentMessage], envelopes: &[JobEnvelope]) -> Message {
        let messages = messages.iter().cloned().map(JobEvent::Message);
        let envelopes = envelopes
            .iter()
            .map(|envelope| crate::job::tests::job_view(envelope.id, envelope.state));
        job_events(messages.chain(envelopes).collect())
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

    fn sequences(receipt: &PendingDelivery) -> Vec<MessageSeq> {
        receipt
            .messages()
            .iter()
            .map(|message| message.message)
            .collect()
    }

    async fn first_pending(manager: &JobManager, owner: &AgentId) -> AgentMessage {
        manager.pending_delivery(owner).await.unwrap().messages()[0].clone()
    }

    /// A foreground child is a call: its replies are never delivered as events, and
    /// claiming its result consumes them, live and across replay.
    #[tokio::test]
    async fn foreground_replies_are_consumed_by_the_result() {
        let (_root, manager, owner, child, job) = child_job(false).await;
        commit(&manager, &child, job, "first").await;
        let second = commit(&manager, &child, job, "second").await;
        let receipt = manager.pending_delivery(&owner).await.unwrap();
        assert!(receipt.messages().is_empty() && receipt.envelopes().is_empty());
        drop(receipt);
        finish(&manager, job).await;
        manager.claim(job).await.unwrap();
        for state in [manager.clone(), manager.test_replay().await] {
            assert!(!state.has_pending(&owner).await);
            assert_eq!(state.last_agent_message(job).await.unwrap(), Some(second));
            let output = state.snapshot(job).await.unwrap().output;
            assert_eq!(output, Some(serde_json::json!("saved result")));
        }
    }

    #[tokio::test]
    async fn replay_derives_publication_and_ack_from_source_history_only() {
        let (_root, manager, owner, child, job) = child_job(true).await;
        let items = vec![
            AssistantItem::text("visible", 0, "visible"),
            AssistantItem::reasoning("secret", 1, "private reasoning", None),
        ];
        // Simulate a crash after committing history but before in-memory publication.
        let message = Message::Assistant(items);
        let source = manager
            .test_append(child.clone(), SessionEvent::MessageCommitted { message })
            .await
            .message();
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
        // The notification and its acknowledgement rows commit together.
        let message = notification(receipt.messages(), &[]);
        receipt.commit(message).await.unwrap();
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

    /// Later batches rewake the owner, and a completion arrives with its job's
    /// last reply, even when that reply alone exceeds the reply budget.
    #[tokio::test]
    async fn bounded_batches_rewake_and_do_not_overtake_messages_with_completion() {
        // Sized against the budget so the split points survive retuning it.
        let large = MESSAGE_BATCH_BYTES * 5 / 8;
        // (message sizes, unrelated finished tool job, expected (messages, envelopes) per batch)
        let cases = [
            (vec![large; 3], false, vec![(1, 0), (1, 0), (1, 1)]),
            // Reply bytes never defer lifecycle metadata, not even an unrelated job's.
            (vec![MESSAGE_BATCH_BYTES + 1000], true, vec![(1, 2)]),
        ];
        for (sizes, unrelated, batches) in cases {
            let (_root, manager, owner, child, job) = child_job(true).await;
            let mut pending = Vec::new();
            for size in sizes {
                pending.push(commit(&manager, &child, job, &"x".repeat(size)).await);
            }
            let mut finished = vec![job];
            if unrelated {
                finished.push(tool_job(&manager, &owner).await);
            }
            for job in &finished {
                finish(&manager, *job).await;
            }
            let mut wakes = manager.subscribe_completions();
            for (index, (messages, envelopes)) in batches.iter().enumerate() {
                let receipt = manager.pending_delivery(&owner).await.unwrap();
                assert_eq!(
                    sequences(&receipt),
                    pending.drain(..*messages).collect::<Vec<_>>()
                );
                assert_eq!(receipt.envelopes().len(), *envelopes);
                if *envelopes > 0 {
                    assert_eq!(envelope_ids(&receipt), finished);
                }
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

    /// A completed child agent's envelope is budgeted as the metadata it presents,
    /// not as the saved result it references. The replies are acknowledged first
    /// so that pinning cannot mask the cost of the envelopes.
    #[tokio::test]
    async fn referenced_completions_are_budgeted_as_metadata() {
        let (session, manager, owner) = owner_session().await;
        // More children than whole-page envelopes the lifecycle budget would admit.
        let count = LIFECYCLE_BATCH_BYTES / output::PAGE_BYTES + 4;
        let mut jobs = Vec::new();
        for index in 0..count {
            let (child, job) = agent_job(&session, &manager, &owner, index as u32 + 1, true).await;
            commit(&manager, &child, job, "small reply").await;
            jobs.push(job);
        }
        let receipt = manager.pending_delivery(&owner).await.unwrap();
        assert_eq!(receipt.messages().len(), count);
        assert!(receipt.envelopes().is_empty());
        ack(receipt).await;
        // The reference survives acknowledgement, so the envelopes stay metadata.
        for job in &jobs {
            finish_bulky(&manager, *job).await;
        }
        let receipt = manager.pending_delivery(&owner).await.unwrap();
        assert!(receipt.messages().is_empty());
        assert_eq!(envelope_ids(&receipt), jobs);
        ack(receipt).await;
        assert!(!manager.has_pending(&owner).await);
        assert!(!manager.test_replay().await.has_pending(&owner).await);
    }

    /// The lifecycle budget is denominated in the bytes presentation really emits,
    /// so a fan-in of untruncatable results is bounded instead of concatenated.
    #[tokio::test]
    async fn lifecycle_batch_bounds_presented_bytes() {
        let (_session, manager, owner) = owner_session().await;
        let mut jobs = Vec::new();
        for _ in 0..5 {
            let job = tool_job(&manager, &owner).await;
            // Not schema-annotated, so presentation cannot shorten it.
            let result = serde_json::json!({"value": "z".repeat(2 * output::PAGE_BYTES)});
            manager.test_finish(job, result).await;
            jobs.push(job);
        }
        let receipt = manager.pending_delivery(&owner).await.unwrap();
        let admitted = envelope_ids(&receipt);
        assert!(admitted.len() < jobs.len(), "admitted {admitted:?}");
        let capabilities = crate::tool::policy::CapabilitySet::default();
        let mut presented = 0;
        for job in &admitted {
            let view = manager
                .present_output_with(
                    output::OutputArgs::new(*job),
                    crate::job::CancellationToken::new(),
                    &capabilities,
                    output::OutputOptions::Host {
                        presentation: OutputPresentation::Automatic,
                    },
                )
                .await
                .unwrap();
            presented += serde_json::to_vec(&view.into_view()).unwrap().len();
        }
        // One first envelope may exceed the budget; the rest must fit within it.
        assert!(
            presented <= LIFECYCLE_BATCH_BYTES + 3 * output::PAGE_BYTES,
            "presented {presented} bytes for {} envelopes",
            admitted.len()
        );
    }

    /// A question envelope rides with the same job's reply.
    #[tokio::test]
    async fn question_travels_with_the_reply_of_its_own_job() {
        let (session, manager, owner) = owner_session().await;
        let (child, job) = agent_job(&session, &manager, &owner, 1, true).await;
        let sequence = commit(&manager, &child, job, "progress note").await;
        let question = crate::job::tests::question("continue");
        manager.request_input(job, question).await.unwrap();
        let receipt = manager.pending_delivery(&owner).await.unwrap();
        assert_eq!(self::sequences(&receipt), [sequence]);
        assert_eq!(envelope_ids(&receipt), [job]);
        assert_eq!(receipt.envelopes()[0].state, JobState::WaitingInput);
        ack(receipt).await;
        assert!(!manager.has_pending(&owner).await);
    }

    /// Small replies are bounded by count, which no byte budget would reach.
    #[tokio::test]
    async fn reply_batches_are_bounded_by_count() {
        let (_root, manager, owner, child, job) = child_job(true).await;
        for index in 0..MESSAGE_BATCH_COUNT + 2 {
            commit(&manager, &child, job, &format!("reply {index}")).await;
        }
        let receipt = manager.pending_delivery(&owner).await.unwrap();
        assert_eq!(receipt.messages().len(), MESSAGE_BATCH_COUNT);
        assert!(messages::batch_size(receipt.messages()) < MESSAGE_BATCH_BYTES);
        // The job still has undelivered replies, so its lifecycle stays withheld.
        assert!(receipt.envelopes().is_empty());
        ack(receipt).await;
        assert!(manager.has_pending(&owner).await);
    }

    /// A reply in the batch pins the completion that references it even when
    /// earlier completions have exhausted the lifecycle budget.
    #[tokio::test]
    async fn reply_pins_its_completion_when_the_lifecycle_budget_is_full() {
        let (session, manager, owner) = owner_session().await;
        // One more whole-page envelope than the budget admits, all created before
        // the child job so their lower IDs consume the budget first.
        let fillers = LIFECYCLE_BATCH_BYTES / output::PAGE_BYTES + 1;
        let mut crowd = Vec::new();
        for _ in 0..fillers {
            crowd.push(tool_job(&manager, &owner).await);
        }
        let (child, job) = agent_job(&session, &manager, &owner, 1, true).await;
        let sequence = commit(&manager, &child, job, "small reply").await;
        finish(&manager, job).await;
        for filler in &crowd {
            finish_bulky(&manager, *filler).await;
        }
        let receipt = manager.pending_delivery(&owner).await.unwrap();
        assert_eq!(sequences(&receipt), [sequence]);
        // How many fillers fit depends on the budget, so assert the invariant: the
        // batch is full, yet the completion referencing this batch's reply is in it.
        let admitted = envelope_ids(&receipt);
        assert!(admitted.len() <= crowd.len(), "admitted {admitted:?}");
        assert!(admitted.contains(&job), "admitted {admitted:?}");
        ack(receipt).await;
        // Every crowded-out filler still arrives, in later batches.
        let mut deferred: Vec<_> = crowd
            .iter()
            .copied()
            .filter(|filler| !admitted.contains(filler))
            .collect();
        assert!(!deferred.is_empty());
        while !deferred.is_empty() {
            let receipt = manager.pending_delivery(&owner).await.unwrap();
            assert!(receipt.messages().is_empty());
            let batch = envelope_ids(&receipt);
            assert!(!batch.is_empty(), "no progress with {deferred:?} deferred");
            deferred.retain(|filler| !batch.contains(filler));
            ack(receipt).await;
        }
        assert!(!manager.has_pending(&owner).await);
        assert!(!manager.test_replay().await.has_pending(&owner).await);
    }

    /// A terminal child reply is durable but withheld from delivery, so the job's
    /// own completion is the single wake that presents both items in one batch and
    /// no owner request boundary in between shows the reply alone.
    #[tokio::test]
    async fn deferred_reply_waits_for_its_completion_and_notify_owner_covers_the_rest() {
        let (session, manager, owner) = owner_session().await;
        let (child, job) = agent_job(&session, &manager, &owner, 1, true).await;
        let mut wakes = manager.subscribe_completions();
        let message = assistant("terminal answer");
        let sequence = manager
            .commit_child_message(
                &child,
                job,
                message,
                "terminal answer".into(),
                false,
                |_| Vec::new(),
            )
            .await
            .unwrap();
        // Durable, but neither pending nor a wake until the invocation resolves.
        assert!(!manager.has_pending(&owner).await);
        assert!(wakes.try_recv().is_err());
        assert_eq!(
            manager.last_agent_message(job).await.unwrap(),
            Some(sequence)
        );

        // The completion wake presents reply and envelope as one receipt.
        finish(&manager, job).await;
        assert_eq!(wakes.try_recv().unwrap().job, job);
        let receipt = manager.pending_delivery(&owner).await.unwrap();
        assert_eq!(sequences(&receipt), [sequence]);
        assert_eq!(receipt.envelopes().len(), 1);
        ack(receipt).await;
        assert!(!manager.has_pending(&owner).await);

        // An invocation that does not finish releases the reply explicitly instead.
        let (child, job) = agent_job(&session, &manager, &owner, 2, true).await;
        let message = assistant("kept going");
        let sequence = manager
            .commit_child_message(&child, job, message, "kept going".into(), false, |_| {
                Vec::new()
            })
            .await
            .unwrap();
        assert!(!manager.has_pending(&owner).await);
        manager.notify_owner(job).await;
        assert_eq!(wakes.try_recv().unwrap().job, job);
        assert_eq!(
            sequences(&manager.pending_delivery(&owner).await.unwrap()),
            [sequence]
        );
        // Delivered once: a second notify wakes again for what is still queued only.
        manager.notify_owner(job).await;
        assert_eq!(wakes.try_recv().unwrap().agent, owner);
        ack(manager.pending_delivery(&owner).await.unwrap()).await;
        // Nothing pending: a wake with nothing to present is never sent.
        assert!(!manager.has_pending(&owner).await);
        while wakes.try_recv().is_ok() {}
        manager.notify_owner(job).await;
        assert!(wakes.try_recv().is_err());
        finish(&manager, job).await;
        ack(manager.pending_delivery(&owner).await.unwrap()).await;
        assert!(!manager.has_pending(&owner).await);

        // A shielded commit that lands after the job finished is delivered on its
        // own: nothing later would release a withheld reply.
        let (child, job) = agent_job(&session, &manager, &owner, 3, true).await;
        finish(&manager, job).await;
        ack(manager.pending_delivery(&owner).await.unwrap()).await;
        while wakes.try_recv().is_ok() {}
        let message = assistant("late answer");
        let sequence = manager
            .commit_child_message(&child, job, message, "late answer".into(), false, |_| {
                Vec::new()
            })
            .await
            .unwrap();
        assert_eq!(wakes.try_recv().unwrap().job, job);
        assert_eq!(
            sequences(&manager.pending_delivery(&owner).await.unwrap()),
            [sequence]
        );
        ack(manager.pending_delivery(&owner).await.unwrap()).await;
        assert!(!manager.has_pending(&owner).await);
        assert!(!manager.test_replay().await.has_pending(&owner).await);
    }

    #[tokio::test]
    async fn message_ack_does_not_claim_lifecycle_and_resume_does_not_claim_messages() {
        let (_root, manager, owner, child, job) = child_job(true).await;
        // Replies too large to share a batch give a reply-only first receipt
        // while the question is pending.
        let bulky = "x".repeat(MESSAGE_BATCH_BYTES * 5 / 8);
        let first = commit(&manager, &child, job, &bulky).await;
        let second = commit(&manager, &child, job, &bulky).await;
        let question = crate::job::tests::question("continue");
        manager.request_input(job, question).await.unwrap();
        let receipt = manager.pending_delivery(&owner).await.unwrap();
        assert_eq!(sequences(&receipt), [first]);
        assert!(receipt.envelopes().is_empty());
        let reply = job_events(vec![JobEvent::Message(AgentMessage {
            id: job,
            name: None,
            message: first,
            text: "before question".into(),
        })]);
        receipt.commit(reply).await.unwrap();
        // Acknowledging replies never claims the question the same job is holding.
        assert_eq!(
            manager
                .pending_delivery(&owner)
                .await
                .unwrap()
                .envelopes()
                .len(),
            1
        );
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
        let (_root, manager, owner, child, job) = child_job(true).await;
        let receipt = manager.pending_delivery(&owner).await.unwrap();
        let mut wakes = manager.subscribe_completions();
        let message = assistant("survives");
        let mut pending = Box::pin(manager.commit_child_message(
            &child,
            job,
            message,
            "survives".into(),
            true,
            |_| Vec::new(),
        ));
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

    /// Private reasoning plus a blank text separator is kept in history, but
    /// publishes no reply, wakes nobody, and leaves nothing for replay to deliver.
    #[tokio::test]
    async fn whitespace_only_child_reply_commits_history_without_publishing_or_waking() {
        for blank in ["", " \t\n"] {
            let (_root, manager, owner, child, job) = child_job(true).await;
            let before = manager.store().records().await.len();
            let mut wakes = manager.subscribe_completions();
            let items = vec![
                AssistantItem::reasoning("thought", 0, "private reasoning", None),
                AssistantItem::text("blank", 1, blank),
            ];
            // A turn publishes its normalized projection, which a blank turn empties.
            let projection = crate::provider::protocol::visible_text(&items);
            assert_eq!(projection, "", "blank text {blank:?}");
            manager
                .commit_child_message(
                    &child,
                    job,
                    Message::Assistant(items),
                    projection,
                    true,
                    |_| Vec::new(),
                )
                .await
                .unwrap();
            // Committed to history, but not as a reply.
            assert_eq!(manager.store().records().await.len(), before + 1);
            assert_eq!(manager.last_agent_message(job).await.unwrap(), None);
            assert!(!manager.has_pending(&owner).await);
            assert!(wakes.try_recv().is_err(), "blank reply woke the owner");
            assert!(
                manager
                    .pending_delivery(&owner)
                    .await
                    .unwrap()
                    .messages()
                    .is_empty()
            );
            // Replay agrees. (It delivers the job as interrupted, so assert on replies.)
            let replayed = manager.test_replay().await;
            assert_eq!(replayed.last_agent_message(job).await.unwrap(), None);
            assert!(
                replayed
                    .pending_delivery(&owner)
                    .await
                    .unwrap()
                    .messages()
                    .is_empty()
            );
        }
    }

    #[tokio::test]
    async fn invalid_association_or_projection_never_commits() {
        let (_root, manager, owner, child, job) = child_job(false).await;
        let before = manager.store().records().await.len();
        for (author, text, projection, expected) in [
            (
                child.clone(),
                "visible",
                "private",
                JobError::ChildTextMismatch,
            ),
            // Raw blank text is not the normalized projection.
            (child.clone(), "\n\n", "\n\n", JobError::ChildTextMismatch),
            (
                owner.child(2),
                "visible",
                "visible",
                JobError::ChildOwnerMismatch(job),
            ),
            (
                owner.clone(),
                "visible",
                "visible",
                JobError::ChildOwnerMismatch(job),
            ),
        ] {
            let message = assistant(text);
            let result = manager.commit_child_message(
                &author,
                job,
                message,
                projection.into(),
                true,
                |_| Vec::new(),
            );
            let failure = result.await.unwrap_err();
            assert_eq!(
                std::mem::discriminant(&failure),
                std::mem::discriminant(&expected),
                "unexpected: {failure}"
            );
        }
        assert_eq!(manager.store().records().await.len(), before);
        assert!(!manager.has_pending(&owner).await);
    }

    #[tokio::test]
    async fn blocked_low_id_child_does_not_withhold_unrelated_completion() {
        let (_root, manager, owner, child, job) = child_job(true).await;
        // Sized so one reply fills the batch: the child keeps undelivered replies,
        // which is what withholds its own completion.
        for _ in 0..3 {
            commit(
                &manager,
                &child,
                job,
                &"x".repeat(MESSAGE_BATCH_BYTES * 5 / 8),
            )
            .await;
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
        assert_eq!(receipt.messages().len(), 1);
        assert_eq!(envelope_ids(&receipt), [ordinary]);
        assert!(messages::batch_size(receipt.messages()) < MESSAGE_BATCH_BYTES);
        ack(receipt).await;
        assert!(manager.has_pending(&owner).await);
    }

    #[tokio::test]
    async fn claims_without_parent_delivery_evidence_never_acknowledge_last_reply() {
        let (_root, manager, owner, child, job) = child_job(true).await;
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
