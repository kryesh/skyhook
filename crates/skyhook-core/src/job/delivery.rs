//! Serialized lifecycle delivery, claims, and wait semantics.

use super::*;

pub(super) const DELIVERY_BATCH_BYTES: usize = 8192;

/// A non-destructive snapshot serialized against publication, claims and resumption.
/// Dropping a receipt before committing leaves its messages and jobs pending.
/// Presentation must not claim jobs while this receipt holds the delivery gate.
pub(crate) struct PendingDelivery {
    manager: JobManager,
    pub(super) owner: AgentId,
    envelopes: Vec<JobEnvelope>,
    messages: Vec<AgentMessage>,
    _delivery: OwnedMutexGuard<()>,
}

impl PendingDelivery {
    pub(crate) fn messages(&self) -> &[AgentMessage] {
        &self.messages
    }

    pub(crate) fn envelopes(&self) -> &[JobEnvelope] {
        &self.envelopes
    }

    /// Commit the parent notification before acknowledging the selected jobs.
    ///
    /// The caller must shield this operation, together with any other notification
    /// acknowledgements, from cancellation: SessionStore::append performs async I/O.
    /// Replay recognizes the committed runtime envelopes, closing the crash window
    /// between this append and the in-memory acknowledgement without a second event.
    /// The receipt retains the delivery gate after commit; drop it only after all
    /// other notification acknowledgements belonging to this message are complete.
    pub(crate) async fn commit(&self, message: Message) -> Result<u64, JobError> {
        let record = self
            .manager
            .inner
            .store
            .append(
                self.owner.clone(),
                SessionEvent::MessageCommitted { message },
            )
            .await?;
        let mut jobs = self.manager.inner.jobs.lock().await;
        if let SessionEvent::MessageCommitted { message } = &record.event {
            persistence::acknowledge_message(&mut jobs, &self.owner, message);
        }
        // A bounded snapshot may leave more work. Wake after durable acknowledgement
        // so the next parent turn cannot sleep with a pending suffix.
        for (&job, entry) in jobs.iter() {
            if entry.agent == self.owner && entry.has_pending() {
                let _ = self.manager.inner.completions.send(JobCompletion {
                    agent: self.owner.clone(),
                    job,
                });
                break;
            }
        }
        Ok(record.sequence)
    }
}

impl JobManager {
    pub async fn wait(
        &self,
        id: JobId,
        timeout: Option<Duration>,
        claim: bool,
    ) -> Result<JobEnvelope, JobError> {
        let mut envelope = self
            .wait_inner(id, timeout, WaitMode::Explicit { claim })
            .await?;
        self.hydrate_envelope(&mut envelope).await?;
        Ok(envelope)
    }

    pub(crate) async fn wait_foreground(&self, id: JobId) -> Result<JobEnvelope, JobError> {
        self.wait_inner(id, None, WaitMode::Foreground).await
    }

    pub(super) async fn wait_inner(
        &self,
        id: JobId,
        timeout: Option<Duration>,
        mode: WaitMode,
    ) -> Result<JobEnvelope, JobError> {
        let deadline = timeout.map(|duration| tokio::time::Instant::now() + duration);
        loop {
            let delivery = self.inner.delivery_operation.lock().await;
            let (snapshot, notified, ready, claimed_agent) = {
                let mut jobs = self.inner.jobs.lock().await;
                let entry = jobs.get_mut(&id).ok_or(JobError::Unknown(id))?;
                let notified = entry.notify.clone().notified_owned();
                let pending_question = entry.state == JobState::WaitingInput
                    && entry.delivery == DeliveryState::Pending;
                let (ready, claim) = match mode {
                    WaitMode::Foreground => (
                        entry.deliverable() || entry.background,
                        entry.state == JobState::WaitingInput,
                    ),
                    WaitMode::Explicit { claim } => {
                        (entry.state.is_terminal() || pending_question, claim)
                    }
                    WaitMode::Terminal => (entry.state.is_terminal(), false),
                };
                let claimed_agent = if ready && claim {
                    entry.reserve_delivery(DeliveryState::Claimed)
                } else {
                    None
                };
                let mut snapshot = entry.envelope(id);
                if entry.state == JobState::WaitingInput && !ready {
                    snapshot.output = None;
                }
                (snapshot, notified, ready, claimed_agent)
            };
            if ready {
                self.persist_delivery(id, claimed_agent, DeliveryState::Claimed)
                    .await?;
                return Ok(snapshot);
            }
            drop(delivery);
            if let Some(deadline) = deadline {
                if tokio::time::timeout_at(deadline, notified).await.is_err() {
                    return Ok(snapshot);
                }
            } else {
                notified.await;
            }
        }
    }

    pub(super) async fn persist_delivery(
        &self,
        id: JobId,
        agent: Option<AgentId>,
        delivery: DeliveryState,
    ) -> Result<(), JobError> {
        if let Some(agent) = agent {
            self.inner.store.append(agent, delivery.event(id)).await?;
        }
        Ok(())
    }

    pub async fn claim(&self, id: JobId) -> Result<(), JobError> {
        let _delivery = self.inner.delivery_operation.lock().await;
        let agent = {
            let mut jobs = self.inner.jobs.lock().await;
            let entry = jobs.get_mut(&id).ok_or(JobError::Unknown(id))?;
            if !entry.deliverable() {
                return Err(JobError::NotTerminal(id));
            }
            entry.reserve_delivery(DeliveryState::Claimed)
        };
        self.persist_delivery(id, agent, DeliveryState::Claimed)
            .await
    }

    pub(crate) async fn prune_claimed(&self) -> Result<usize, JobError> {
        let removed = {
            let mut jobs = self.inner.jobs.lock().await;
            let removed = jobs
                .iter()
                .filter_map(|(id, entry)| {
                    (entry.state.is_terminal()
                        && entry.delivery == DeliveryState::Claimed
                        && entry.resume.is_none())
                    .then_some(*id)
                })
                .collect::<Vec<_>>();
            for id in &removed {
                jobs.remove(id);
            }
            removed
        };
        for id in &removed {
            self.inner.store.remove_job_artifacts(*id).await?;
        }
        Ok(removed.len())
    }

    /// Snapshot a bounded pending batch without acknowledging it. The receipt
    /// holds the delivery gate through presentation and parent history commit.
    pub(crate) async fn pending_delivery(
        &self,
        owner: &AgentId,
    ) -> Result<PendingDelivery, JobError> {
        let delivery = self.inner.delivery_operation.clone().lock_owned().await;
        let jobs = self.inner.jobs.lock().await;
        let messages = messages::pending_messages(&jobs, owner);
        let through = messages.last().map_or(0, |message| message.message);
        let remaining = DELIVERY_BATCH_BYTES.saturating_sub(messages::batch_size(&messages));
        let envelopes = self
            .pending_ids(&jobs, owner, through, remaining, !messages.is_empty())
            .into_iter()
            .map(|id| jobs[&id].envelope(id))
            .collect();
        Ok(PendingDelivery {
            manager: self.clone(),
            owner: owner.clone(),
            envelopes,
            messages,
            _delivery: delivery,
        })
    }

    pub(super) fn pending_ids(
        &self,
        jobs: &HashMap<JobId, JobEntry>,
        owner: &AgentId,
        messages_through: u64,
        mut remaining: usize,
        has_messages: bool,
    ) -> Vec<JobId> {
        let mut ids = jobs
            .iter()
            .filter(|(_, entry)| {
                &entry.agent == owner
                    && entry.background
                    && entry.deliverable()
                    && entry.delivery == DeliveryState::Pending
                    // Filter before budgeting: a blocked low-ID child must not
                    // consume the lifecycle budget of an unrelated completion.
                    && entry.messages.iter().all(|message| message.message <= messages_through)
            })
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        ids.sort();
        let mut pending = Vec::new();
        for id in ids {
            let entry = &jobs[&id];
            let metadata =
                serde_json::to_vec(&entry.metadata(id)).map_or(8192, |bytes| bytes.len());
            let estimate = output::presentation_size(&self.output_directory(id));
            let cost = if entry.state == JobState::Completed && estimate <= output::CONTENT_BYTES {
                estimate
                    .saturating_add(metadata)
                    .saturating_add(128)
                    .min(8192)
            } else {
                8192
            };
            if cost > remaining && (has_messages || !pending.is_empty()) {
                // A smaller later completion may fit the shared remaining budget.
                continue;
            }
            pending.push(id);
            remaining = remaining.saturating_sub(cost);
        }
        pending
    }

    /// Legacy eager reservation for callers that do not commit parent history.
    pub async fn take_pending(&self, owner: &AgentId) -> Result<Vec<JobEnvelope>, JobError> {
        let _delivery = self.inner.delivery_operation.lock().await;
        let pending = {
            let mut jobs = self.inner.jobs.lock().await;
            let mut pending = Vec::new();
            for id in self.pending_ids(&jobs, owner, 0, DELIVERY_BATCH_BYTES, false) {
                let entry = jobs.get_mut(&id).expect("selected job");
                if let Some(agent) = entry.reserve_delivery(DeliveryState::Injected) {
                    pending.push((id, agent, entry.envelope(id)));
                }
            }
            pending
        };
        for (job, agent, _) in &pending {
            self.persist_delivery(*job, Some(agent.clone()), DeliveryState::Injected)
                .await?;
        }
        Ok(pending
            .into_iter()
            .map(|(_, _, envelope)| envelope)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::protocol::{Message, UserContent};

    async fn completed_job() -> (tempfile::TempDir, JobManager, AgentId, JobId) {
        let (root, manager, owner) = crate::job::tests::runtime().await;
        let lease = manager
            .test_lease(JobSpec {
                background: true,
                ..JobSpec::test(owner.clone(), "test")
            })
            .await;
        manager
            .test_finish(lease.id, serde_json::json!("answer"))
            .await;
        (root, manager, owner, lease.id)
    }

    async fn question_job() -> (tempfile::TempDir, JobManager, AgentId, JobId) {
        let (root, manager, owner) = crate::job::tests::runtime().await;
        let job = manager
            .test_create(JobSpec::test(owner.clone(), "agent"))
            .await;
        manager.transition(job, JobState::Running).await.unwrap();
        manager
            .request_input(job, serde_json::json!({"question":"choose"}))
            .await
            .unwrap();
        (root, manager, owner, job)
    }

    fn notification(job: JobId, state: JobState) -> Message {
        Message::User(vec![UserContent::Runtime {
            text: format!(
                "<skyhook_job_events>\n{}\n</skyhook_job_events>",
                serde_json::json!([{"id":job,"state":state}]),
            ),
        }])
    }

    #[tokio::test]
    async fn delivery_snapshot_serializes_explicit_claim_until_drop() {
        let (_root, manager, owner, job) = completed_job().await;
        let receipt = manager.pending_delivery(&owner).await.unwrap();
        assert_eq!(receipt.envelopes()[0].id, job);
        assert!(manager.has_pending(&owner).await);
        let claim = manager.claim(job);
        tokio::pin!(claim);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), claim.as_mut())
                .await
                .is_err()
        );
        drop(receipt);
        assert!(manager.has_pending(&owner).await);
        claim.await.unwrap();
        assert!(!manager.has_pending(&owner).await);
        // Claiming affects delivery, not later explicit output access.
        assert_eq!(
            manager.snapshot(job).await.unwrap().output,
            Some(serde_json::json!("answer"))
        );
    }

    #[tokio::test]
    async fn delivery_commit_wins_over_concurrent_explicit_claim_without_duplicate_ack() {
        let (_root, manager, owner, job) = completed_job().await;
        let receipt = manager.pending_delivery(&owner).await.unwrap();
        let claim = manager.claim(job);
        tokio::pin!(claim);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), claim.as_mut())
                .await
                .is_err()
        );
        let sequence = receipt
            .commit(notification(job, JobState::Completed))
            .await
            .unwrap();
        // The outer transaction can acknowledge child progress before releasing the gate.
        assert!(
            tokio::time::timeout(Duration::from_millis(20), claim.as_mut())
                .await
                .is_err()
        );
        drop(receipt);
        claim.await.unwrap();
        assert!(!manager.has_pending(&owner).await);
        let records = manager.store().records().await;
        assert_eq!(records.last().unwrap().sequence, sequence);
        assert!(!records.iter().any(|record| matches!(
            record.event,
            SessionEvent::JobClaimed { .. } | SessionEvent::JobInjected { .. }
        )));
        let restored = JobManager::restore(manager.store().clone(), &records)
            .await
            .unwrap();
        assert!(!restored.has_pending(&owner).await);
        assert_eq!(
            restored.snapshot(job).await.unwrap().output,
            Some(serde_json::json!("answer"))
        );
    }

    #[tokio::test]
    async fn delivery_cancel_before_commit_releases_gate_and_keeps_batch_pending() {
        let (_root, manager, owner, job) = completed_job().await;
        let receipt = manager.pending_delivery(&owner).await.unwrap();
        let task = tokio::spawn(async move {
            std::future::pending::<()>().await;
            receipt.commit(notification(job, JobState::Completed)).await
        });
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let receipt = manager.pending_delivery(&owner).await.unwrap();
        assert_eq!(receipt.envelopes()[0].id, job);
    }

    #[tokio::test]
    async fn delivery_replay_closes_crash_between_parent_commit_and_memory_ack() {
        let (_root, manager, owner, job) = completed_job().await;
        manager
            .test_append(
                owner.clone(),
                SessionEvent::MessageCommitted {
                    message: notification(job, JobState::Completed),
                },
            )
            .await;
        // Simulate a host crash: only the parent message reached the journal.
        assert!(manager.has_pending(&owner).await);
        let restored = manager.test_replay().await;
        assert!(!restored.has_pending(&owner).await);
    }

    #[tokio::test]
    async fn delivery_replay_does_not_acknowledge_user_text_wrong_owner_or_wrong_state() {
        for mode in ["text", "parent_input", "wrong_owner", "wrong_state"] {
            let (_root, manager, owner, job) = completed_job().await;
            let Message::User(mut content) = notification(job, JobState::Completed) else {
                unreachable!()
            };
            let UserContent::Runtime { text } = content.remove(0) else {
                unreachable!()
            };
            let message = match mode {
                "text" => Message::User(vec![UserContent::Text { text }]),
                "parent_input" => Message::User(vec![UserContent::ParentInput { text }]),
                "wrong_state" => notification(job, JobState::WaitingInput),
                _ => notification(job, JobState::Completed),
            };
            let author = if mode == "wrong_owner" {
                owner.child(1)
            } else {
                owner.clone()
            };
            manager
                .test_append(author, SessionEvent::MessageCommitted { message })
                .await;
            let restored = manager.test_replay().await;
            assert!(restored.has_pending(&owner).await, "{mode}");
        }
    }

    #[tokio::test]
    async fn delivery_replay_old_notification_does_not_acknowledge_resumed_completion() {
        let (_root, manager, owner, job) = completed_job().await;
        manager
            .pending_delivery(&owner)
            .await
            .unwrap()
            .commit(notification(job, JobState::Completed))
            .await
            .unwrap();
        manager
            .test_append(
                owner.clone(),
                SessionEvent::JobStateChanged {
                    job,
                    state: JobState::Running,
                },
            )
            .await;
        manager
            .test_append(
                owner.clone(),
                SessionEvent::JobFinished {
                    job,
                    state: JobState::Completed,
                    output_path: Some(
                        std::path::PathBuf::from("jobs")
                            .join(job.to_string())
                            .join("document.json"),
                    ),
                    error: None,
                    images: Vec::new(),
                    denial: None,
                },
            )
            .await;
        let restored = manager.test_replay().await;
        assert!(restored.has_pending(&owner).await);
    }

    #[tokio::test]
    async fn delivery_question_ack_does_not_suppress_later_terminal_result_after_replay() {
        let (_root, manager, owner, job) = question_job().await;
        manager
            .pending_delivery(&owner)
            .await
            .unwrap()
            .commit(notification(job, JobState::WaitingInput))
            .await
            .unwrap();
        manager.resume_input(job).await.unwrap();
        manager.test_finish(job, serde_json::json!("done")).await;
        assert!(manager.has_pending(&owner).await);
        let restored = manager.test_replay().await;
        assert!(restored.has_pending(&owner).await);
    }

    #[tokio::test]
    async fn delivery_cancelled_question_gets_a_new_terminal_delivery() {
        let (_root, manager, owner, job) = question_job().await;
        let receipt = manager.pending_delivery(&owner).await.unwrap();
        receipt
            .commit(notification(job, JobState::WaitingInput))
            .await
            .unwrap();
        let finish = manager.finish(job, JobOutcome::Cancelled);
        tokio::pin!(finish);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), finish.as_mut())
                .await
                .is_err()
        );
        drop(receipt);
        finish.await.unwrap();
        assert!(manager.has_pending(&owner).await);
        let restored = manager.test_replay().await;
        let receipt = restored.pending_delivery(&owner).await.unwrap();
        assert_eq!(receipt.envelopes()[0].state, JobState::Cancelled);
    }

    #[tokio::test]
    async fn delivery_empty_receipt_keeps_gate_through_other_notification_acknowledgements() {
        let (_root, manager, owner, job) = completed_job().await;
        manager.claim(job).await.unwrap();
        let receipt = manager.pending_delivery(&owner).await.unwrap();
        assert!(receipt.envelopes().is_empty());
        receipt
            .commit(Message::User(vec![UserContent::Runtime {
                text: "<skyhook_child_messages>\n[]\n</skyhook_child_messages>".into(),
            }]))
            .await
            .unwrap();
        let next = manager.pending_delivery(&owner);
        tokio::pin!(next);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), next.as_mut())
                .await
                .is_err()
        );
        drop(receipt);
        assert!(next.await.unwrap().envelopes().is_empty());
    }

    #[tokio::test]
    async fn delivery_missing_notification_in_committed_message_stays_pending_live_and_replay() {
        let (_root, manager, owner, _job) = completed_job().await;
        let receipt = manager.pending_delivery(&owner).await.unwrap();
        receipt
            .commit(Message::User(vec![UserContent::Text {
                text: "new user input".into(),
            }]))
            .await
            .unwrap();
        drop(receipt);
        assert!(manager.has_pending(&owner).await);
        let restored = manager.test_replay().await;
        assert!(restored.has_pending(&owner).await);
    }

    #[tokio::test(start_paused = true)]
    async fn wait_deadline_does_not_restart_on_notifications() {
        let (_root, jobs, agent) = crate::job::tests::runtime().await;
        let lease = jobs.test_lease(JobSpec::test(agent, "pending")).await;
        let notify = jobs
            .inner
            .jobs
            .lock()
            .await
            .get(&lease.id)
            .unwrap()
            .notify
            .clone();
        let waiter = {
            let jobs = jobs.clone();
            tokio::spawn(async move {
                jobs.wait(lease.id, Some(Duration::from_secs(10)), true)
                    .await
                    .unwrap()
            })
        };
        tokio::task::yield_now().await;
        for _ in 0..4 {
            tokio::time::advance(Duration::from_secs(2)).await;
            notify.notify_waiters();
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;
        assert!(waiter.is_finished());
        assert_eq!(waiter.await.unwrap().state, JobState::Queued);
        assert!(!lease.cancellation.is_cancelled());
    }

    #[tokio::test]
    async fn waiting_input_is_claimed_or_injected_exactly_once() {
        let (_root, manager, agent) = crate::job::tests::runtime().await;
        let lease = manager
            .test_lease(JobSpec {
                accepts_input: true,
                ..JobSpec::test(agent.clone(), "agent")
            })
            .await;
        manager
            .transition(lease.id, JobState::Running)
            .await
            .unwrap();
        manager
            .request_input(
                lease.id,
                serde_json::json!({"kind":"questions","question_id":"q-2"}),
            )
            .await
            .unwrap();

        let question = manager.wait(lease.id, None, true).await.unwrap();
        assert_eq!(question.state, JobState::WaitingInput);
        assert_eq!(question.output.unwrap()["question_id"], "q-2");
        assert_eq!(
            manager.take_pending(&agent).await.unwrap(),
            Vec::<JobEnvelope>::new()
        );
        let repeated = manager
            .wait(lease.id, Some(Duration::from_millis(1)), true)
            .await
            .unwrap();
        assert_eq!(repeated.state, JobState::WaitingInput);
        assert_eq!(repeated.output, None);

        manager.resume_input(lease.id).await.unwrap();
        manager
            .request_input(
                lease.id,
                serde_json::json!({"kind":"questions","question_id":"q-3"}),
            )
            .await
            .unwrap();
        let injected = manager.take_pending(&agent).await.unwrap();
        assert_eq!(injected.len(), 1);
        assert_eq!(injected[0].output.as_ref().unwrap()["question_id"], "q-3");
        assert_eq!(
            manager.take_pending(&agent).await.unwrap(),
            Vec::<JobEnvelope>::new()
        );
    }
}
