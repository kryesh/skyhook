//! Serialized lifecycle delivery, claims, and wait semantics.

use super::*;

/// Aggregate child-reply bytes one notification batches; a single oversized
/// reply is still admitted.
pub(super) const MESSAGE_BATCH_BYTES: usize = 4 * output::PAGE_BYTES;

/// Aggregate lifecycle-envelope bytes one notification batches, budgeted
/// separately so reply content cannot starve completion metadata.
pub(super) const LIFECYCLE_BATCH_BYTES: usize = 4 * output::PAGE_BYTES;

/// A pending batch holding the delivery gate. Dropping it uncommitted leaves its
/// messages and jobs pending; presentation must not claim jobs while it is held.
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
    /// Shielded from caller cancellation once admitted.
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
                                let injected = acknowledged
                                    .into_iter()
                                    .map(|job| SessionEvent::JobInjected { job });
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
                    // A bounded snapshot may leave more work: wake the owner for it.
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

    /// Block until `id` is no longer live, without claiming or hydrating output.
    pub(crate) async fn wait_settled(&self, id: JobId) -> Result<(), JobError> {
        self.wait_inner(id, None, WaitMode::Terminal)
            .await
            .map(drop)
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
                self.persist_claim(id, claimed_agent, delivery).await?;
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

    /// Append the claim, then install it live; a no-op without `agent`.
    async fn persist_claim(
        &self,
        id: JobId,
        agent: Option<AgentId>,
        gate: OwnedMutexGuard<()>,
    ) -> Result<(), JobError> {
        let Some(agent) = agent else {
            return Ok(());
        };
        self.spawn_owned(gate, "delivery publication", move |manager| async move {
            let claimed = SessionEvent::JobClaimed { job: id };
            manager.inner.store.append(agent, claimed).await?;
            let mut jobs = manager.inner.jobs.lock().await;
            jobs.get_mut(&id).ok_or(JobError::Unknown(id))?.delivery = DeliveryState::Claimed;
            Ok(())
        })
        .await
    }

    pub async fn claim(&self, id: JobId) -> Result<(), JobError> {
        let delivery = self.inner.delivery_operation.clone().lock_owned().await;
        let agent = {
            let mut jobs = self.inner.jobs.lock().await;
            let entry = jobs.get_mut(&id).ok_or(JobError::Unknown(id))?;
            // A suspended job's delivery waits for its restart; reading its state
            // acknowledges nothing.
            if entry.suspended() {
                return Ok(());
            }
            if !entry.deliverable() {
                return Err(JobError::NotTerminal(id));
            }
            (entry.delivery == DeliveryState::Pending).then(|| entry.agent.clone())
        };
        self.persist_claim(id, agent, delivery).await
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
        let envelopes = self
            .pending_ids(&jobs, owner, &messages)
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

    /// Lifecycle envelopes to present alongside `messages`, within their own
    /// budget. A completion never overtakes the replies of its own job.
    pub(super) fn pending_ids(
        &self,
        jobs: &HashMap<JobId, JobEntry>,
        owner: &AgentId,
        messages: &[AgentMessage],
    ) -> Vec<JobId> {
        let messages_through = messages.last().map_or(0, |message| message.message);
        let mut remaining = LIFECYCLE_BATCH_BYTES;
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
            let metadata = serde_json::to_vec(&entry.metadata(id))
                .map_or(output::PAGE_BYTES, |bytes| bytes.len());
            // A completed child agent presents its result by reference to its reply.
            let referenced = entry.role == JobRole::Agent
                && entry.state == JobState::Completed
                && entry.last_agent_message.is_some();
            let cost = if referenced {
                metadata.saturating_add(128)
            } else {
                output::presentation_size(&self.output(id))
                    .saturating_add(metadata)
                    .saturating_add(128)
            };
            // A reply in this batch pins the completion that references it.
            let pinned = referenced && messages.iter().any(|message| message.id == id);
            if !pinned && cost > remaining && !pending.is_empty() {
                // A later completion may still fit; an oversized first one is admitted.
                continue;
            }
            pending.push(id);
            remaining = remaining.saturating_sub(cost);
        }
        pending
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

    fn notification(job: JobId, state: JobState) -> Message {
        crate::job::tests::job_events(serde_json::json!([{"id":job,"state":state}]))
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
                    SessionEvent::JobClaimed { .. } => Some(false),
                    SessionEvent::JobInjected { .. } => Some(true),
                    _ => None,
                })
                .collect();
            assert_eq!(acks, [sequence.is_some()]);
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
        assert!(!manager.has_pending(&agent).await);
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
        let receipt = manager.pending_delivery(&agent).await.unwrap();
        assert_eq!(receipt.envelopes().len(), 1);
        let output = receipt.envelopes()[0].output.as_ref().unwrap();
        assert_eq!(output["question_id"], "q-3");
        let message = notification(lease.id(), JobState::WaitingInput);
        receipt.commit(message).await.unwrap();
        assert!(!manager.has_pending(&agent).await);
    }
}
