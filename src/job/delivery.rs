//! Lifecycle delivery, claims, and wait semantics.

use super::*;
use crate::session::Message;

/// Aggregate child-reply bytes one notification batches; a single oversized
/// reply is still admitted.
pub(super) const MESSAGE_BATCH_BYTES: usize = 4 * output::PAGE_BYTES;

/// Aggregate lifecycle-envelope bytes one notification batches, budgeted
/// separately so reply content cannot starve completion metadata.
pub(super) const LIFECYCLE_BATCH_BYTES: usize = 4 * output::PAGE_BYTES;

/// What a notification adds around each envelope it presents.
const ENVELOPE_OVERHEAD: usize = 128;

/// The pending question or outcome one read presented, which only that read
/// may acknowledge: a later delivery of the same job is not its to acknowledge.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Delivery {
    pub(super) job: JobId,
    pub(super) stamp: u64,
}

impl Delivery {
    /// The claim acknowledging this delivery, unless its job has a newer one since.
    pub(crate) fn claim(
        self,
        state: &CoreState,
    ) -> Result<Option<(AgentId, SessionEvent)>, JobError> {
        let Self { job, stamp } = self;
        let entry = state.jobs.entries.get(&job).ok_or(JobError::Unknown(job))?;
        let current = entry
            .delivery(job)
            .is_some_and(|delivery| delivery.stamp == stamp);
        Ok(current.then(|| (entry.agent.clone(), SessionEvent::JobClaimed { job })))
    }

    /// The claims acknowledging `reads` that are still current: each job's
    /// newest, once.
    pub(crate) fn claims(
        mut reads: Vec<Self>,
        state: &CoreState,
    ) -> impl Iterator<Item = (AgentId, SessionEvent)> {
        reads.sort_by_key(|read| (read.job, std::cmp::Reverse(read.stamp)));
        reads.dedup_by_key(|read| read.job);
        (reads.into_iter()).filter_map(|read| read.claim(state).ok().flatten())
    }
}

/// A pending batch as one read saw it. Committing it delivers exactly what it
/// presents, or nothing once part of it is no longer pending as it was read.
pub(crate) struct Notification {
    manager: JobManager,
    pub(super) owner: AgentId,
    envelopes: Vec<JobEnvelope>,
    /// What each envelope presents.
    deliveries: Vec<Delivery>,
    messages: Vec<AgentMessage>,
}

impl Notification {
    pub(crate) fn messages(&self) -> &[AgentMessage] {
        &self.messages
    }

    pub(crate) fn envelopes(&self) -> &[JobEnvelope] {
        &self.envelopes
    }

    /// Commit the owner's `message` presenting this batch, after the owner's
    /// `preceding` entries that apply to it, with an acknowledgement row for each
    /// job outcome and child reply it delivers, in one transaction. Refused
    /// [`JobError::Interrupted`] once the owner's turn is not busy, and
    /// [`JobError::Stale`] when any of them is no longer pending as it was read, so
    /// the owner presents what is pending now instead.
    pub(crate) async fn commit(
        self,
        preceding: Vec<SessionEvent>,
        message: Message,
    ) -> Result<MessageSeq, JobError> {
        let Self {
            manager,
            owner,
            deliveries,
            messages,
            ..
        } = self;
        let store = manager.inner.store.clone();
        let notify = move |state: &CoreState| {
            if !state.agents.busy(&owner) {
                return Err(JobError::Interrupted);
            }
            let jobs = &state.jobs.entries;
            let current = deliveries.iter().all(|Delivery { job, stamp }| {
                jobs.get(job).is_some_and(|entry| {
                    entry.agent == owner && entry.lifecycle_pending() == Some(*stamp)
                })
            });
            let queued = messages.iter().all(|reply| {
                let child = jobs.get(&reply.id).and_then(JobFacts::child);
                child.is_some_and(|child| child.messages.iter().any(|m| m.message == reply.message))
            });
            if !(current && queued) {
                return Err(JobError::Stale);
            }
            let at = preceding.len();
            let next = state.next_sequence();
            let notification = preceding.iter().fold(next, |seq, _| seq.next()).message();
            let injected = (deliveries.into_iter())
                .map(|Delivery { job, .. }| SessionEvent::JobInjected { job });
            let replies = messages
                .into_iter()
                .map(|reply| SessionEvent::JobMessageDelivered {
                    job: reply.id,
                    source: reply.message,
                    notification,
                });
            let events = (preceding.into_iter())
                .chain([SessionEvent::MessageCommitted { message }])
                .chain(injected)
                .chain(replies);
            let entries = events.map(|event| (owner.clone(), event)).collect();
            Ok(Plan::new(entries, move |_, records| {
                records[at].sequence.message()
            }))
        };
        store.commit(Command::Journal, notify).await
    }
}

impl JobManager {
    /// The foreground result, with the delivery it presents for the caller to
    /// acknowledge once it has read it; a question it presents is acknowledged now.
    pub(crate) async fn wait_foreground(
        &self,
        id: JobId,
    ) -> Result<(JobEnvelope, Option<Delivery>), JobError> {
        // A released job whose finish is claimed returns with that outcome.
        let ready =
            |entry: &JobFacts| entry.deliverable() || (entry.background && !entry.finishing);
        let (envelope, presented) = self.until_job(id, ready).await?;
        if envelope.question.is_some() {
            self.acknowledge(presented).await?;
        }
        Ok((envelope, presented))
    }

    /// Block until `id` is no longer live, without claiming or hydrating output.
    pub(crate) async fn wait_settled(&self, id: JobId) -> Result<(), JobError> {
        self.until_job(id, |entry| !entry.live()).await.map(drop)
    }

    /// Job `id`'s envelope and the delivery it presents, once `ready`.
    pub(super) async fn until_job(
        &self,
        id: JobId,
        ready: fn(&JobFacts) -> bool,
    ) -> Result<(JobEnvelope, Option<Delivery>), JobError> {
        let resolve = move |state: &mut CoreState| match state.jobs.entries.get(&id) {
            None => Some(Err(JobError::Unknown(id))),
            Some(entry) => ready(entry).then(|| Ok((entry.envelope(id), entry.delivery(id)))),
        };
        self.inner.store.until(resolve).await?
    }

    /// Acknowledge what a read presented, unless the job has a newer delivery since.
    pub(crate) async fn acknowledge(&self, delivery: Option<Delivery>) -> Result<(), JobError> {
        let Some(delivery) = delivery else {
            return Ok(());
        };
        let claim = move |state: &CoreState| {
            Ok::<_, JobError>(Plan::journal(delivery.claim(state)?.into_iter().collect()))
        };
        self.inner.store.commit(Command::Journal, claim).await
    }

    /// Record that job `reader` presented `read`, for its finish to acknowledge.
    pub(crate) async fn read(&self, reader: JobId, read: Option<Delivery>) {
        let Some(read) = read else {
            return;
        };
        let record = move |state: &mut CoreState| {
            if let Some(entry) = state.jobs.entries.get_mut(&reader) {
                entry.reads.push(read);
            }
        };
        self.inner.store.live(record).await;
    }

    /// Read a bounded pending batch without acknowledging it.
    pub(crate) async fn pending_delivery(&self, owner: &AgentId) -> Result<Notification, JobError> {
        let (messages, candidates) = self.facts(|jobs| {
            let messages = messages::pending_messages(&jobs.entries, owner);
            let candidates = self.lifecycle_candidates(&jobs.entries, owner, &messages);
            (messages, candidates)
        });
        let envelopes = if candidates.is_empty() {
            Vec::new()
        } else {
            // Sized off the core.
            output::blocking(move || {
                let mut remaining = LIFECYCLE_BATCH_BYTES;
                let mut pending = Vec::new();
                for (envelope, delivery, output) in candidates {
                    if remaining == 0 {
                        break;
                    }
                    let metadata = envelope.metadata_view(&CapabilitySet::default());
                    let metadata = serde_json::to_vec(&metadata)
                        .map_or(output::PAGE_BYTES, |bytes| bytes.len());
                    let cost = output::presented_size(&output)
                        .saturating_add(metadata)
                        .saturating_add(ENVELOPE_OVERHEAD);
                    if cost > remaining && !pending.is_empty() {
                        // A later completion may still fit; an oversized first one is admitted.
                        continue;
                    }
                    pending.push((envelope, delivery));
                    remaining = remaining.saturating_sub(cost);
                }
                Ok(pending)
            })
            .await
            .map_err(|error| JobError::Output(Box::new(error)))?
        };
        let (envelopes, deliveries) = envelopes.into_iter().unzip();
        Ok(Notification {
            manager: self.clone(),
            owner: owner.clone(),
            envelopes,
            deliveries,
            messages,
        })
    }

    /// Lifecycle envelopes to present alongside `messages`, in job order, each
    /// with the output that presents it. A completion never overtakes the replies
    /// of its own job.
    fn lifecycle_candidates(
        &self,
        jobs: &HashMap<JobId, JobFacts>,
        owner: &AgentId,
        messages: &[AgentMessage],
    ) -> Vec<(JobEnvelope, Delivery, output::Output)> {
        let messages_through = messages
            .last()
            .map_or(MessageSeq::default(), |message| message.message);
        let mut candidates = jobs
            .iter()
            .filter(|(_, entry)| {
                &entry.agent == owner
                    // Filter before budgeting: a blocked low-ID child must not
                    // consume the lifecycle budget of an unrelated completion.
                    && entry.child().is_none_or(|child| {
                        child.messages.iter().all(|message| message.message <= messages_through)
                    })
            })
            .filter_map(|(&id, entry)| {
                let stamp = entry.lifecycle_pending()?;
                Some((
                    entry.envelope(id),
                    Delivery { job: id, stamp },
                    output::Output::of(self.store(), id, entry.invocation),
                ))
            })
            .collect::<Vec<_>>();
        candidates.sort_by_key(|(envelope, ..)| envelope.id);
        candidates
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn completed_job() -> (tempfile::TempDir, JobManager, AgentId, JobId) {
        let (root, manager, owner) = crate::job::tests::runtime().await;
        let spec = JobSpec {
            background: true,
            ..JobSpec::test(owner.clone(), "test")
        };
        let job = manager.test_create(spec).await;
        manager.test_finish(job, serde_json::json!("answer")).await;
        (root, manager, owner, job)
    }

    async fn question_job() -> (tempfile::TempDir, JobManager, AgentId, JobId) {
        let (root, manager, owner) = crate::job::tests::runtime().await;
        let job = manager
            .test_running(JobSpec::test(owner.clone(), "agent"))
            .await
            .into_test_id();
        let question = crate::job::tests::question("choose");
        manager.request_input(job, question).await.unwrap();
        (root, manager, owner, job)
    }

    fn notification(job: JobId, state: JobState) -> Message {
        crate::job::tests::job_events(vec![crate::job::tests::job_view(job, state)])
    }

    async fn commit_pending(manager: &JobManager, owner: &AgentId, message: Message) -> MessageSeq {
        let receipt = manager.pending_delivery(owner).await.unwrap();
        receipt.commit(Vec::new(), message).await.unwrap()
    }

    /// Asserts the owner has (or has no) pending delivery both live and after replay.
    fn assert_pending(manager: &JobManager, owner: &AgentId, pending: bool, case: &str) {
        assert_eq!(manager.has_pending(owner), pending, "live {case}");
        assert_eq!(
            manager.test_replay().has_pending(owner),
            pending,
            "replay {case}"
        );
    }

    /// A notification read beside an explicit claim delivers its job once: a
    /// claim that lands first makes the notification stale, which commits
    /// nothing, and a committed notification leaves the claim nothing to
    /// acknowledge. A delivery of another job does not make it stale.
    #[tokio::test]
    async fn a_claim_landing_first_makes_the_notification_stale() {
        for claim_first in [true, false] {
            let (_root, manager, owner, job) = completed_job().await;
            let read = manager.pending_delivery(&owner).await.unwrap();
            assert_eq!(read.envelopes()[0].id, job);
            let spec = JobSpec {
                background: true,
                ..JobSpec::test(owner.clone(), "other")
            };
            let other = manager.test_create(spec).await;
            manager.test_finish(other, serde_json::json!("other")).await;
            let message = notification(job, JobState::Completed);
            if claim_first {
                manager.test_claim(job).await.unwrap();
                let stale = read.commit(Vec::new(), message).await;
                assert!(matches!(stale, Err(JobError::Stale)), "{stale:?}");
            } else {
                read.commit(Vec::new(), message).await.unwrap();
                manager.test_claim(job).await.unwrap();
            }
            let records = manager.store().records();
            // Exactly one acknowledgement: the notification's, or the claim's.
            let acks: Vec<_> = records
                .iter()
                .filter_map(|record| match record.event {
                    SessionEvent::JobClaimed { job: claimed } => Some((claimed, false)),
                    SessionEvent::JobInjected { job: injected } => Some((injected, true)),
                    _ => None,
                })
                .collect();
            assert_eq!(acks, [(job, !claim_first)]);
            assert!(manager.has_pending(&owner), "the other job stays pending");
            // Claiming affects delivery, not later explicit output access.
            let output = manager
                .test_replay()
                .test_snapshot(job)
                .await
                .unwrap()
                .output;
            assert_eq!(output, Some(serde_json::json!("answer")));
        }
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
            assert_pending(&manager, &owner, !receipt, &receipt.to_string());
        }
    }

    #[tokio::test]
    async fn delivery_question_ack_does_not_suppress_later_terminal_result() {
        for cancelled in [false, true] {
            let (_root, manager, owner, job) = question_job().await;
            commit_pending(&manager, &owner, notification(job, JobState::WaitingInput)).await;
            if cancelled {
                manager
                    .finish(job, ToolError::cancelled().into())
                    .await
                    .unwrap();
            } else {
                manager.resume_input(job).await.unwrap();
                manager.test_finish(job, serde_json::json!("done")).await;
            }
            assert_pending(&manager, &owner, true, "terminal");
            let restored = manager.test_replay();
            let state = restored.pending_delivery(&owner).await.unwrap().envelopes()[0].state;
            let expected = if cancelled {
                JobState::Cancelled
            } else {
                JobState::Completed
            };
            assert_eq!(state, expected);
        }
    }

    /// An ask job's question is its owner's own: only the answer is delivered.
    #[tokio::test]
    async fn an_ask_jobs_question_is_not_delivered_to_its_owner() {
        let (_root, manager, owner) = crate::job::tests::runtime().await;
        let spec = JobSpec {
            background: true,
            role: JobRole::Question,
            ..JobSpec::test(owner.clone(), "ask")
        };
        let job = manager.test_running(spec).await.into_test_id();
        let question = crate::job::tests::question("choose");
        manager.request_input(job, question).await.unwrap();
        assert_pending(&manager, &owner, false, "asking");
        manager.resume_input(job).await.unwrap();
        manager.test_finish(job, serde_json::json!("chosen")).await;
        assert_pending(&manager, &owner, true, "answered");
    }

    /// A read acknowledges only the delivery it showed: what replaced it is still
    /// delivered, whether an outcome or another question, or a restart's
    /// completion replacing the completion it read.
    #[tokio::test]
    async fn stale_acknowledgement_leaves_the_later_delivery_pending() {
        let presented = async |manager: &JobManager, job| {
            manager.entry(job, |entry| entry.delivery(job)).unwrap()
        };
        for asks_again in [false, true] {
            let (_root, manager, owner, job) = question_job().await;
            let first = presented(&manager, job).await;
            manager.resume_input(job).await.unwrap();
            if asks_again {
                let question = crate::job::tests::question("again");
                manager.request_input(job, question).await.unwrap();
            } else {
                manager.test_finish(job, serde_json::json!("done")).await;
            }
            manager.acknowledge(first).await.unwrap();
            assert_pending(&manager, &owner, true, "after a stale question");
        }
        let (_root, manager, owner) = crate::job::tests::runtime().await;
        let again = serde_json::json!("again");
        let job = manager.test_resumable(owner.clone(), "agent", again).await;
        manager.test_finish(job, serde_json::json!("done")).await;
        let first = presented(&manager, job).await;
        manager.send(job, serde_json::json!("again")).await.unwrap();
        crate::tests::bounded(manager.wait(job, false))
            .await
            .unwrap();
        manager.acknowledge(first).await.unwrap();
        assert_pending(&manager, &owner, true, "after a stale completion");
    }

    #[tokio::test]
    async fn waiting_input_is_claimed_or_injected_exactly_once() {
        let (_root, manager, agent) = crate::job::tests::runtime().await;
        let spec = JobSpec {
            accepts_input: true,
            ..JobSpec::test(agent.clone(), "agent")
        };
        let lease = manager.test_running(spec).await;
        let question = crate::job::tests::question;
        manager
            .request_input(lease.id(), question("q-2"))
            .await
            .unwrap();
        let question_view = manager.wait(lease.id(), true).await.unwrap();
        assert_eq!(question_view.state, JobState::WaitingInput);
        assert_eq!(question_view.question, Some(question("q-2")));
        assert!(!manager.has_pending(&agent));
        manager.resume_input(lease.id()).await.unwrap();
        manager
            .request_input(lease.id(), question("q-3"))
            .await
            .unwrap();
        let receipt = manager.pending_delivery(&agent).await.unwrap();
        assert_eq!(receipt.envelopes().len(), 1);
        assert_eq!(receipt.envelopes()[0].question, Some(question("q-3")));
        let message = notification(lease.id(), JobState::WaitingInput);
        receipt.commit(Vec::new(), message).await.unwrap();
        assert!(!manager.has_pending(&agent));
    }
}
