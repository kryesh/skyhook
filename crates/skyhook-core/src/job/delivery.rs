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

    /// Commit the parent notification with an acknowledgement row for each delivered
    /// job outcome and child reply, in one transaction, then acknowledge them live.
    ///
    /// This consuming receipt shields the commit and all selected acknowledgements.
    /// Cancelling the caller after admission cannot release the delivery gate, which
    /// is released only after durable and live acknowledgement; no reusable receipt
    /// survives a successful or failed commit.
    pub(crate) async fn commit(self, message: Message) -> Result<u64, JobError> {
        let Self {
            manager,
            owner,
            envelopes,
            messages,
            _delivery: gate,
        } = self;
        manager
            .spawn_owned(
                gate,
                "notification publication",
                move |manager| async move {
                    // The gate serializes outcomes with delivery, so this selection holds.
                    let delivered: Vec<_> = {
                        let jobs = manager.inner.jobs.lock().await;
                        envelopes
                            .iter()
                            .filter(|envelope| {
                                jobs.get(&envelope.id).is_some_and(|entry| {
                                    entry.agent == owner
                                        && entry.background
                                        && entry.state == envelope.state
                                        && entry.deliverable()
                                        && entry.delivery == DeliveryState::Pending
                                })
                            })
                            .map(|envelope| envelope.id)
                            .collect()
                    };
                    let (acknowledged, replies) = (delivered.clone(), messages.clone());
                    let author = owner.clone();
                    let records = manager
                        .inner
                        .store
                        .append_then(
                            owner.clone(),
                            SessionEvent::MessageCommitted { message },
                            move |notification| {
                                let injected = acknowledged.into_iter().map(|job| {
                                    let notification = Some(notification);
                                    SessionEvent::JobInjected { job, notification }
                                });
                                let replies = replies.into_iter().map(|reply| {
                                    SessionEvent::JobMessageDelivered {
                                        job: reply.id,
                                        source: reply.message,
                                        notification,
                                    }
                                });
                                injected
                                    .chain(replies)
                                    .map(|event| (author.clone(), event))
                                    .collect()
                            },
                        )
                        .await?;
                    let record = &records[0];
                    let mut jobs = manager.inner.jobs.lock().await;
                    for reply in &messages {
                        if let Some(entry) = jobs.get_mut(&reply.id) {
                            entry
                                .messages
                                .retain(|message| message.message != reply.message);
                        }
                    }
                    for job in delivered {
                        if let Some(entry) = jobs.get_mut(&job) {
                            entry.reserve_delivery(DeliveryState::Injected);
                        }
                    }
                    // A bounded snapshot may leave more work. Wake after durable
                    // acknowledgement so the next parent turn cannot sleep with a
                    // pending suffix.
                    for (&job, entry) in jobs.iter() {
                        if entry.agent == owner && entry.has_pending() {
                            let _ = manager.inner.completions.send(JobCompletion {
                                agent: owner.clone(),
                                job,
                            });
                            break;
                        }
                    }
                    Ok(record.sequence)
                },
            )
            .await
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

    /// Transfer observes the same readiness as foreground execution without
    /// acknowledging either completion or question delivery.
    pub(crate) async fn wait_foreground_for_transfer(
        &self,
        id: JobId,
    ) -> Result<JobEnvelope, JobError> {
        self.wait_inner(id, None, WaitMode::Transfer).await
    }

    pub(super) async fn wait_inner(
        &self,
        id: JobId,
        timeout: Option<Duration>,
        mode: WaitMode,
    ) -> Result<JobEnvelope, JobError> {
        let deadline = timeout.map(|duration| tokio::time::Instant::now() + duration);
        loop {
            let delivery = self.inner.delivery_operation.clone().lock_owned().await;
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
                    WaitMode::Transfer => (entry.deliverable() || entry.background, false),
                    WaitMode::Explicit { claim } => (
                        !entry.suspended() && (entry.state.is_terminal() || pending_question),
                        claim,
                    ),
                    WaitMode::Terminal => (entry.state.is_terminal() && !entry.suspended(), false),
                };
                let claimed_agent = if ready && claim {
                    (entry.delivery == DeliveryState::Pending).then(|| entry.agent.clone())
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
                self.persist_delivery(id, claimed_agent, DeliveryState::Claimed, delivery)
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
        gate: OwnedMutexGuard<()>,
    ) -> Result<(), JobError> {
        let Some(agent) = agent else {
            return Ok(());
        };
        self.spawn_owned(gate, "delivery publication", move |manager| async move {
            manager.publish_delivery(id, agent, delivery).await
        })
        .await
    }

    /// Append the delivery event, then install the same state live.
    async fn publish_delivery(
        &self,
        id: JobId,
        agent: AgentId,
        delivery: DeliveryState,
    ) -> Result<(), JobError> {
        self.inner.store.append(agent, delivery.event(id)).await?;
        let mut jobs = self.inner.jobs.lock().await;
        jobs.get_mut(&id).ok_or(JobError::Unknown(id))?.delivery = delivery;
        Ok(())
    }

    pub async fn claim(&self, id: JobId) -> Result<(), JobError> {
        let delivery = self.inner.delivery_operation.clone().lock_owned().await;
        let agent = {
            let mut jobs = self.inner.jobs.lock().await;
            let entry = jobs.get_mut(&id).ok_or(JobError::Unknown(id))?;
            if !entry.deliverable() {
                return Err(JobError::NotTerminal(id));
            }
            (entry.delivery == DeliveryState::Pending).then(|| entry.agent.clone())
        };
        self.persist_delivery(id, agent, DeliveryState::Claimed, delivery)
            .await
    }

    pub(crate) async fn prune_claimed(&self) -> Result<usize, JobError> {
        let creation = self.inner.creation_operation.clone().write_owned().await;
        self.spawn_owned(creation, "prune", move |manager| async move {
            // Pin candidates against accepted finalization/reset publication before
            // taking the delivery gate, preserving operation -> delivery lock order.
            let candidates = manager
                .inner
                .jobs
                .lock()
                .await
                .iter()
                .filter(|(_, entry)| {
                    entry.state.is_terminal()
                        && entry.delivery == DeliveryState::Claimed
                        && entry.resume.is_none()
                })
                .map(|(&id, entry)| (id, entry.operation.clone()))
                .collect::<Vec<_>>();
            let mut pinned = Vec::with_capacity(candidates.len());
            for (id, operation) in candidates {
                pinned.push((id, operation.lock_owned().await));
            }
            let _delivery = manager.inner.delivery_operation.lock().await;
            let removed = {
                let mut jobs = manager.inner.jobs.lock().await;
                let removed = pinned
                    .iter()
                    .filter_map(|(id, _)| {
                        jobs.get(id)
                            .filter(|entry| {
                                entry.state.is_terminal()
                                    && entry.delivery == DeliveryState::Claimed
                                    && entry.resume.is_none()
                            })
                            .map(|_| *id)
                    })
                    .collect::<Vec<_>>();
                for id in &removed {
                    jobs.remove(id);
                }
                removed
            };
            // The owner retains creation and candidate gates through cleanup even
            // if its caller disappears after membership publication.
            for id in &removed {
                manager.inner.store.remove_job_artifacts(*id).await?;
            }
            Ok(removed.len())
        })
        .await
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
            let estimate = output::presentation_size(&self.output(id));
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

    /// Legacy eager delivery for callers that do not commit parent history.
    /// Each accepted event is installed live before releasing the shared gate.
    pub async fn take_pending(&self, owner: &AgentId) -> Result<Vec<JobEnvelope>, JobError> {
        let gate = self.inner.delivery_operation.clone().lock_owned().await;
        let owner = owner.clone();
        self.spawn_owned(gate, "pending delivery", move |manager| async move {
            let pending = {
                let jobs = manager.inner.jobs.lock().await;
                manager
                    .pending_ids(&jobs, &owner, 0, DELIVERY_BATCH_BYTES, false)
                    .into_iter()
                    .map(|id| (id, jobs[&id].agent.clone(), jobs[&id].envelope(id)))
                    .collect::<Vec<_>>()
            };
            for (id, agent, _) in &pending {
                let injected = DeliveryState::Injected;
                manager
                    .publish_delivery(*id, agent.clone(), injected)
                    .await?;
            }
            Ok(pending
                .into_iter()
                .map(|(_, _, envelope)| envelope)
                .collect())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::protocol::{Message, UserContent};

    async fn completed_job() -> (tempfile::TempDir, JobManager, AgentId, JobId) {
        let (root, manager, owner) = crate::job::tests::runtime().await;
        let spec = JobSpec {
            background: true,
            ..JobSpec::test(owner.clone(), "test")
        };
        let lease = manager.test_lease(spec).await;
        manager
            .test_finish(lease.id(), serde_json::json!("answer"))
            .await;
        (root, manager, owner, lease.id())
    }

    async fn question_job() -> (tempfile::TempDir, JobManager, AgentId, JobId) {
        let (root, manager, owner) = crate::job::tests::runtime().await;
        let job = manager
            .test_create(JobSpec::test(owner.clone(), "agent"))
            .await;
        manager.transition(job, JobState::Running).await.unwrap();
        let question = serde_json::json!({"question":"choose"});
        manager.request_input(job, question).await.unwrap();
        (root, manager, owner, job)
    }

    fn notification_text(job: JobId, state: JobState) -> String {
        let events = serde_json::json!([{"id":job,"state":state}]);
        format!("<skyhook_job_events>\n{events}\n</skyhook_job_events>")
    }

    fn notification(job: JobId, state: JobState) -> Message {
        Message::User(vec![UserContent::Runtime {
            text: notification_text(job, state),
        }])
    }

    async fn commit_pending(manager: &JobManager, owner: &AgentId, message: Message) -> u64 {
        let receipt = manager.pending_delivery(owner).await.unwrap();
        receipt.commit(message).await.unwrap()
    }

    /// Asserts the owner has (or has no) pending delivery both live and after replay.
    async fn assert_pending(manager: &JobManager, owner: &AgentId, pending: bool, case: &str) {
        assert_eq!(manager.has_pending(owner).await, pending, "live {case}");
        assert_eq!(
            manager.test_replay().await.has_pending(owner).await,
            pending,
            "replay {case}"
        );
    }

    #[tokio::test]
    async fn foreground_transfer_does_not_claim_questions() {
        let (_root, manager, owner, job) = question_job().await;
        let transferred = manager.wait_foreground_for_transfer(job).await.unwrap();
        assert_eq!(transferred.state, JobState::WaitingInput);
        assert!(transferred.output.is_some());
        assert!(manager.has_pending(&owner).await);
        assert_eq!(manager.wait_foreground(job).await.unwrap(), transferred);
        assert!(!manager.has_pending(&owner).await);
    }

    /// An open receipt serializes an explicit claim; either the claim wins after
    /// the receipt drops, or a commit wins without a duplicate acknowledgement.
    #[tokio::test]
    async fn delivery_receipt_serializes_explicit_claim_until_drop_or_commit() {
        for commit in [false, true] {
            let (_root, manager, owner, job) = completed_job().await;
            let receipt = manager.pending_delivery(&owner).await.unwrap();
            assert_eq!(receipt.envelopes()[0].id, job);
            let claim = manager.claim(job);
            tokio::pin!(claim);
            assert!(
                tokio::time::timeout(Duration::from_millis(20), claim.as_mut())
                    .await
                    .is_err()
            );
            let sequence = if commit {
                // Consuming commit owns the notification append and releases its gate
                // only after publication; the competing claim can now complete.
                Some(
                    receipt
                        .commit(notification(job, JobState::Completed))
                        .await
                        .unwrap(),
                )
            } else {
                drop(receipt);
                assert!(manager.has_pending(&owner).await);
                None
            };
            claim.await.unwrap();
            assert_pending(&manager, &owner, false, "claimed").await;
            let records = manager.store().records().await;
            // Exactly one acknowledgement: the notification's, or the later claim.
            let acks: Vec<_> = records
                .iter()
                .filter_map(|record| match record.event {
                    SessionEvent::JobClaimed { .. } => Some(None),
                    SessionEvent::JobInjected { notification, .. } => Some(notification),
                    _ => None,
                })
                .collect();
            assert_eq!(acks, [sequence]);
            // Claiming affects delivery, not later explicit output access.
            let output = manager
                .test_replay()
                .await
                .snapshot(job)
                .await
                .unwrap()
                .output;
            assert_eq!(output, Some(serde_json::json!("answer")));
        }
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

    /// Replay acknowledges delivery from the rows committed with a notification,
    /// never by reading notification text.
    #[tokio::test]
    async fn delivery_replay_acknowledges_only_committed_delivery_rows() {
        for receipt in [true, false] {
            let (_root, manager, owner, job) = completed_job().await;
            let message = notification(job, JobState::Completed);
            if receipt {
                commit_pending(&manager, &owner, message).await;
            } else {
                // Text that merely looks like a notification acknowledges nothing.
                let event = SessionEvent::MessageCommitted { message };
                manager.test_append(owner.clone(), event).await;
            }
            assert_eq!(manager.has_pending(&owner).await, !receipt);
            let restored = manager.test_replay().await;
            assert_eq!(restored.has_pending(&owner).await, !receipt, "{receipt}");
        }
    }

    #[tokio::test]
    async fn delivery_replay_old_notification_does_not_acknowledge_resumed_completion() {
        let (_root, manager, owner, job) = completed_job().await;
        commit_pending(&manager, &owner, notification(job, JobState::Completed)).await;
        let running = SessionEvent::JobStateChanged {
            job,
            state: JobState::Running,
        };
        manager.test_append(owner.clone(), running).await;
        let finished = SessionEvent::JobFinished {
            job,
            state: JobState::Completed,
            error: None,
            images: Vec::new(),
            denial: None,
        };
        manager.test_append(owner.clone(), finished).await;
        assert!(manager.test_replay().await.has_pending(&owner).await);
    }

    /// A delivered question never suppresses the job's later terminal delivery.
    #[tokio::test]
    async fn delivery_question_ack_does_not_suppress_later_terminal_result() {
        for cancelled in [false, true] {
            let (_root, manager, owner, job) = question_job().await;
            commit_pending(&manager, &owner, notification(job, JobState::WaitingInput)).await;
            if cancelled {
                manager.finish(job, JobOutcome::Cancelled).await.unwrap();
            } else {
                manager.resume_input(job).await.unwrap();
                manager.test_finish(job, serde_json::json!("done")).await;
            }
            assert_pending(&manager, &owner, true, "terminal").await;
            let restored = manager.test_replay().await;
            let state = restored.pending_delivery(&owner).await.unwrap().envelopes()[0].state;
            let expected = if cancelled {
                JobState::Cancelled
            } else {
                JobState::Completed
            };
            assert_eq!(state, expected);
        }
    }

    #[tokio::test]
    async fn dropped_and_empty_receipts_release_the_delivery_gate() {
        let (_root, manager, owner, job) = completed_job().await;
        // A dropped receipt acknowledges nothing.
        drop(manager.pending_delivery(&owner).await.unwrap());
        assert_pending(&manager, &owner, true, "dropped receipt").await;
        // An empty receipt still releases the gate after consuming its commit.
        manager.claim(job).await.unwrap();
        let receipt = manager.pending_delivery(&owner).await.unwrap();
        assert!(receipt.envelopes().is_empty());
        let text = "<skyhook_child_messages>\n[]\n</skyhook_child_messages>".into();
        receipt
            .commit(Message::User(vec![UserContent::Runtime { text }]))
            .await
            .unwrap();
        assert!(
            manager
                .pending_delivery(&owner)
                .await
                .unwrap()
                .envelopes()
                .is_empty()
        );
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
            .get(&lease.id())
            .unwrap()
            .notify
            .clone();
        let waiter = tokio::spawn({
            let (jobs, id) = (jobs.clone(), lease.id());
            async move {
                jobs.wait(id, Some(Duration::from_secs(10)), true)
                    .await
                    .unwrap()
            }
        });
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
        assert!(!lease.cancellation_token().is_cancelled());
    }

    #[tokio::test]
    async fn waiting_input_is_claimed_or_injected_exactly_once() {
        let (_root, manager, agent) = crate::job::tests::runtime().await;
        let spec = JobSpec {
            accepts_input: true,
            ..JobSpec::test(agent.clone(), "agent")
        };
        let lease = manager.test_lease(spec).await;
        manager
            .transition(lease.id(), JobState::Running)
            .await
            .unwrap();
        let question = |id: &str| serde_json::json!({"kind":"questions","question_id":id});
        manager
            .request_input(lease.id(), question("q-2"))
            .await
            .unwrap();
        let question_view = manager.wait(lease.id(), None, true).await.unwrap();
        assert_eq!(question_view.state, JobState::WaitingInput);
        assert_eq!(question_view.output.unwrap()["question_id"], "q-2");
        assert!(manager.take_pending(&agent).await.unwrap().is_empty());
        let repeated = manager
            .wait(lease.id(), Some(Duration::from_millis(1)), true)
            .await
            .unwrap();
        assert_eq!(
            (repeated.state, repeated.output),
            (JobState::WaitingInput, None)
        );
        manager.resume_input(lease.id()).await.unwrap();
        manager
            .request_input(lease.id(), question("q-3"))
            .await
            .unwrap();
        let injected = manager.take_pending(&agent).await.unwrap();
        assert_eq!(injected.len(), 1);
        assert_eq!(injected[0].output.as_ref().unwrap()["question_id"], "q-3");
        assert!(manager.take_pending(&agent).await.unwrap().is_empty());
    }
}
