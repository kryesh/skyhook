//! Lifecycle delivery, claims, and wait semantics.

use super::*;
use crate::session::{HostCursor, HostEvent, Message};

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
    /// The claim acknowledging this delivery while it is pending; once a restart
    /// superseded the result it presented, the acknowledgement of that result,
    /// queued as a reply, and never of the restarted invocation's delivery.
    pub(crate) fn claim(self, state: &CoreState) -> Option<(AgentId, SessionEvent)> {
        let Self { job, stamp } = self;
        // A pending delivery keeps its job resident: one no longer held was claimed.
        let entry = state.jobs.get(job)?;
        let event = match entry.delivery(job) {
            Some(delivery) if delivery.stamp == stamp => SessionEvent::JobClaimed { job },
            _ => SessionEvent::JobMessageDelivered {
                job,
                source: entry.child()?.superseded(stamp)?,
                notification: None,
            },
        };
        Some((entry.agent.clone(), event))
    }

    /// The claims acknowledging `reads` that still acknowledge something, each
    /// once.
    pub(crate) fn claims(
        mut reads: Vec<Self>,
        state: &CoreState,
    ) -> impl Iterator<Item = (AgentId, SessionEvent)> {
        reads.sort_by_key(|read| (read.job, read.stamp));
        reads.dedup_by_key(|read| (read.job, read.stamp));
        (reads.into_iter()).filter_map(|read| read.claim(state))
    }
}

impl Jobs {
    /// Hold for job `reader` the delivery it captured to present, in the change
    /// that captures it: a result it presents that a restart supersedes is held
    /// from its owner until the reader ends. A reader that ended, or is gone,
    /// holds nothing, as its finish already released what it held.
    fn reserve(&mut self, reader: JobId, captured: Delivery) {
        if self.get(reader).is_none_or(|entry| entry.end().is_some()) {
            return;
        }
        self.modify(reader, |entry, _| entry.holds.push(captured));
        self.modify(captured.job, |entry, _| {
            entry
                .child_mut()
                .map(|child| child.held.push(captured.stamp))
        });
    }

    /// End job `reader`'s reads and holds, once its finish acknowledged what they
    /// presented: a superseded result one held and did not acknowledge is its
    /// owner's again.
    pub(super) fn unread(&mut self, reader: JobId) {
        let holds = self.modify(reader, |entry, _| {
            entry.reads.clear();
            std::mem::take(&mut entry.holds)
        });
        for Delivery { job, stamp } in holds.into_iter().flatten() {
            self.modify(job, |entry, _| {
                entry.child_mut().map(|child| child.unhold(stamp))
            });
        }
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
    /// The host's event, read after how far the owner's last one went then.
    host: Option<(Option<HostCursor>, HostEvent)>,
}

impl Notification {
    pub(crate) fn messages(&self) -> &[AgentMessage] {
        &self.messages
    }

    pub(crate) fn envelopes(&self) -> &[JobEnvelope] {
        &self.envelopes
    }

    pub(crate) fn host(&self) -> Option<&HostEvent> {
        self.host.as_ref().map(|(_, event)| event)
    }

    /// Present the host's `event` too, which it read after `since`.
    pub(crate) fn with_host(&mut self, since: Option<HostCursor>, event: HostEvent) {
        self.host = Some((since, event));
    }

    /// Commit the owner's `message` presenting this batch, after the owner's
    /// `preceding` entries that apply to it, with an acknowledgement row for each
    /// job outcome and child reply it delivers, in one transaction; a host event
    /// moves the owner's cursor as it commits. Refused [`JobError::Interrupted`]
    /// once the owner's turn is not busy, and [`JobError::Stale`] when any of them is
    /// no longer pending as it was read, or the owner was given a host event since,
    /// so the owner presents what is pending now instead.
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
            host,
            ..
        } = self;
        let store = manager.inner.store.clone();
        let notify = move |state: &CoreState| {
            if !state.agents.busy(&owner) {
                return Err(JobError::Interrupted);
            }
            let jobs = &state.jobs;
            let current = deliveries.iter().all(|Delivery { job, stamp }| {
                jobs.get(*job).is_some_and(|entry| {
                    entry.agent == owner && entry.lifecycle_pending() == Some(*stamp)
                })
            });
            // A hold registered since the batch was read keeps its reply back.
            let queued = messages.iter().all(|reply| {
                let child = jobs.get(reply.id).and_then(JobFacts::child);
                child.is_some_and(|child| {
                    (child.deliverable()).any(|m| m.reply.message == reply.message)
                })
            });
            let unmoved = host
                .as_ref()
                .is_none_or(|(since, _)| state.agent_states.host_cursor(&owner) == *since);
            if !(current && queued && unmoved) {
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
                    notification: Some(notification),
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
    /// acknowledge once it has read it, held for job `reader` when one reads it;
    /// or the job as its call let go of it, unfinished, which then reports
    /// through delivery, a question included. A question alone, which its owner
    /// reads, leaves the call waiting.
    pub(crate) async fn wait_foreground(
        &self,
        id: JobId,
        reader: Option<JobId>,
    ) -> Result<(JobEnvelope, Option<Delivery>), JobError> {
        // A released job whose finish is claimed returns with that outcome.
        let ready = |entry: &JobFacts| entry.concluded() || (entry.background && !entry.finishing);
        let (envelope, presented) = self.until_job(id, ready, reader).await?;
        let presented = presented.filter(|_| envelope.state.is_terminal());
        Ok((envelope, presented))
    }

    /// Block until `id` is no longer live, without claiming or hydrating output.
    pub(crate) async fn wait_settled(&self, id: JobId) -> Result<(), JobError> {
        self.until_job(id, |entry| !entry.live(), None)
            .await
            .map(drop)
    }

    /// Job `id`'s envelope and the delivery it presents, once `ready`; a result
    /// is held for `reader` in the change that decides it.
    pub(super) async fn until_job(
        &self,
        id: JobId,
        ready: fn(&JobFacts) -> bool,
        reader: Option<JobId>,
    ) -> Result<(JobEnvelope, Option<Delivery>), JobError> {
        let resolve = move |state: &CoreState| match state.jobs.lookup(id) {
            Lookup::Held(entry) => {
                ready(entry).then(|| Ok((entry.envelope(id), entry.delivery(id))))
            }
            // Let go since the wait last decided, so as it was then.
            Lookup::Unheld(()) => None,
            Lookup::Unknown => Some(Err(JobError::Unknown(id))),
        };
        let hold =
            move |state: &mut CoreState,
                  decided: Result<(JobEnvelope, Option<Delivery>), JobError>| {
                if let (Some(reader), Ok((envelope, Some(presented)))) = (reader, &decided)
                    && envelope.state.is_terminal()
                {
                    state.jobs.reserve(reader, *presented);
                }
                decided
            };
        let wait = Wait::taking([Key::Job(id)], resolve, hold);
        self.inner.store.until(wait).await?
    }

    /// What `read` reads of job `id`, with the delivery it presents, held for
    /// `reader` in the change that reads it.
    pub(super) async fn presenting<T: Send + 'static>(
        &self,
        id: JobId,
        reader: Option<JobId>,
        read: impl FnOnce(&JobFacts) -> T + Send + 'static,
    ) -> Result<(T, Option<Delivery>), JobError> {
        let presents = move |entry: &JobFacts| (read(entry), entry.delivery(id));
        let Some(reader) = reader else {
            return self.entry(id, presents).await;
        };
        let hold = move |state: &mut CoreState| {
            let Some(entry) = state.jobs.get(id) else {
                return Err(presents);
            };
            let presented = presents(entry);
            if let Some(delivery) = presented.1 {
                state.jobs.reserve(reader, delivery);
            }
            Ok(presented)
        };
        match self.inner.store.live(hold).await {
            Ok(presented) => Ok(presented),
            // A pending delivery keeps its job resident, so there is none to hold.
            Err(presents) => self.entry(id, presents).await,
        }
    }

    /// Acknowledge what a read presented, unless the job has a newer delivery since.
    pub(crate) async fn acknowledge(&self, delivery: Option<Delivery>) -> Result<(), JobError> {
        let Some(delivery) = delivery else {
            return Ok(());
        };
        let claim = move |state: &CoreState| {
            Ok::<_, JobError>(Plan::journal(delivery.claim(state).into_iter().collect()))
        };
        self.inner.store.commit(Command::Journal, claim).await
    }

    /// Record that job `reader` presented `read`, for its finish to acknowledge.
    pub(crate) async fn read(&self, reader: JobId, read: Option<Delivery>) {
        let Some(read) = read else {
            return;
        };
        let record = move |state: &mut CoreState| {
            state.jobs.modify(reader, |entry, _| entry.reads.push(read));
        };
        self.inner.store.live(record).await;
    }

    /// Read a bounded pending batch without acknowledging it.
    pub(crate) async fn pending_delivery(&self, owner: &AgentId) -> Result<Notification, JobError> {
        let (messages, candidates) = self.facts(|jobs| {
            let messages = messages::pending_messages(jobs, owner);
            let candidates = self.lifecycle_candidates(jobs, owner, &messages);
            (messages, candidates)
        });
        let envelopes = if candidates.is_empty() {
            Vec::new()
        } else {
            // Sized off the core.
            self.store()
                .reads()
                .hold(move || {
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
            host: None,
        })
    }

    /// Lifecycle envelopes to present alongside `messages`, in job order, each
    /// with the output that presents it. A completion never overtakes the replies
    /// of its own job.
    fn lifecycle_candidates(
        &self,
        jobs: &Jobs,
        owner: &AgentId,
        messages: &[AgentMessage],
    ) -> Vec<(JobEnvelope, Delivery, output::Output)> {
        let messages_through = messages
            .last()
            .map_or(MessageSeq::default(), |message| message.message);
        let mut candidates = jobs
            .pending(owner)
            // Filter before budgeting: a blocked low-ID child must not consume
            // the lifecycle budget of an unrelated completion.
            .filter(|(_, entry)| {
                entry.child().is_none_or(|child| {
                    (child.deliverable()).all(|queued| queued.reply.message <= messages_through)
                })
            })
            .filter_map(|(id, entry)| {
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
    use std::assert_matches;

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

    /// A background agent job asking a question, and the ask whose end answers it.
    async fn question_job() -> (tempfile::TempDir, JobManager, AgentId, JobId, JobId) {
        let (root, manager, owner) = crate::job::tests::runtime().await;
        let spec = JobSpec {
            background: true,
            role: JobRole::Agent,
            ..JobSpec::test(owner.clone(), "agent")
        };
        let job = manager.test_running(spec).await.into_test_id();
        let ask = crate::job::tests::ask(&manager, job, "choose").await;
        (root, manager, owner, job, ask)
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
                assert_matches!(stale, Err(JobError::Stale));
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
            let (_root, manager, owner, job, ask) = question_job().await;
            commit_pending(&manager, &owner, notification(job, JobState::WaitingInput)).await;
            if cancelled {
                manager
                    .finish(job, ToolError::cancelled().into())
                    .await
                    .unwrap();
            } else {
                manager.test_finish(ask, serde_json::json!("chosen")).await;
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
        let asked = crate::job::tests::asked_by(job, "choose");
        manager.request_input(job, asked).await.unwrap();
        assert_pending(&manager, &owner, false, "asking");
        manager.test_finish(job, serde_json::json!("chosen")).await;
        assert_pending(&manager, &owner, true, "answered");
    }

    /// A read acknowledges only the delivery it showed: what replaced it is still
    /// delivered, whether an outcome or another question, or a restart's
    /// completion replacing the completion it read.
    #[tokio::test]
    async fn stale_acknowledgement_leaves_the_later_delivery_pending() {
        let presented = async |manager: &JobManager, job| {
            manager
                .entry(job, |entry| entry.delivery(job))
                .await
                .unwrap()
        };
        for asks_again in [false, true] {
            let (_root, manager, owner, job, ask) = question_job().await;
            let first = presented(&manager, job).await;
            manager.test_finish(ask, serde_json::json!("chosen")).await;
            if asks_again {
                crate::job::tests::ask(&manager, job, "again").await;
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

    /// A finished child whose answer is its pending result, and a job to read it.
    async fn answered_child(manager: &JobManager, owner: &AgentId) -> (JobId, JobId) {
        let job =
            (manager.test_resumable(owner.clone(), "agent", serde_json::json!("again"))).await;
        let child = manager.facts(|jobs| jobs.get(job)?.child()?.agent.clone());
        let answer = Message::Assistant(vec![crate::provider::protocol::AssistantItem::text(
            "text", 0, "first",
        )]);
        (manager
            .commit_child_message(&child.unwrap(), job, answer, "first".into(), |_| Vec::new()))
        .await
        .unwrap();
        manager.test_reply(job, "first").await;
        let reader = manager
            .test_create(JobSpec::test(owner.clone(), "script"))
            .await;
        (job, reader)
    }

    /// Restart finished job `job` with input, superseding its result.
    async fn restart(manager: &JobManager, job: JobId) {
        manager.send(job, serde_json::json!("more")).await.unwrap();
        crate::tests::bounded(manager.wait(job, false))
            .await
            .unwrap();
    }

    /// A reader's capture of a result holds it from its owner before the reader
    /// records its read, so a restart in between leaves the superseded answer to
    /// the reader alone, whether a call's wait or a presentation captured it.
    /// A reader whose finish landed before its capture holds nothing.
    #[tokio::test]
    async fn a_captured_result_is_held_before_its_read_is_recorded() {
        let (_root, manager, owner) = crate::job::tests::runtime().await;
        for present in [false, true] {
            let (job, reader) = answered_child(&manager, &owner).await;
            let read = if present {
                let options = output::OutputOptions::Model {
                    kind: ViewKind::Inspection,
                    reader: Some(reader),
                    fit: output::Fit::Preview,
                };
                let args = output::OutputArgs::new(job);
                let capabilities = CapabilitySet::default();
                let presented = manager.present_output_with(
                    args,
                    CancellationToken::new(),
                    &capabilities,
                    options,
                );
                presented.await.unwrap().acknowledges
            } else {
                manager.wait_foreground(job, Some(reader)).await.unwrap().1
            };
            restart(&manager, job).await;
            let pending = manager.pending_delivery(&owner).await.unwrap();
            assert!(pending.messages().is_empty(), "{:?}", pending.messages());
            manager.read(reader, read).await;
            manager.test_finish(reader, serde_json::Value::Null).await;
            for jobs in [manager.clone(), manager.test_replay()] {
                let pending = jobs.pending_delivery(&owner).await.unwrap();
                assert!(pending.messages().is_empty(), "{:?}", pending.messages());
            }
        }
        let (job, reader) = answered_child(&manager, &owner).await;
        manager.test_finish(reader, serde_json::Value::Null).await;
        manager.presenting(job, Some(reader), |_| ()).await.unwrap();
        restart(&manager, job).await;
        let pending = manager.pending_delivery(&owner).await.unwrap();
        assert_eq!(pending.messages().len(), 1);
    }

    /// A notification read before a hold landed on a reply it presents commits
    /// nothing.
    #[tokio::test]
    async fn a_hold_landing_after_a_notification_was_read_makes_it_stale() {
        let (_root, manager, owner) = crate::job::tests::runtime().await;
        let (job, reader) = answered_child(&manager, &owner).await;
        let (_, first) = manager.presenting(job, None, |_| ()).await.unwrap();
        restart(&manager, job).await;
        let read = manager.pending_delivery(&owner).await.unwrap();
        assert_eq!(read.messages().len(), 1);
        let first = first.unwrap();
        manager
            .store()
            .live(move |state| state.jobs.reserve(reader, first))
            .await;
        let stale = read
            .commit(Vec::new(), notification(job, JobState::Completed))
            .await;
        assert_matches!(stale, Err(JobError::Stale));
    }

    #[tokio::test]
    async fn waiting_input_is_claimed_or_injected_exactly_once() {
        let (_root, manager, agent) = crate::job::tests::runtime().await;
        let spec = JobSpec {
            accepts_input: true,
            role: JobRole::Agent,
            ..JobSpec::test(agent.clone(), "agent")
        };
        let lease = manager.test_running(spec).await;
        let job = lease.id();
        let asked = || {
            manager
                .store()
                .read(move |state| crate::job::tests::asked(state, job))
        };
        let ask = crate::job::tests::ask(&manager, job, "q-2").await;
        let question_view = manager.wait(job, true).await.unwrap();
        assert_eq!(question_view.state, JobState::WaitingInput);
        assert_eq!(asked(), Some(vec!["q-2".to_owned()]));
        assert!(!manager.has_pending(&agent));
        manager.test_finish(ask, serde_json::json!("answer")).await;
        crate::job::tests::ask(&manager, job, "q-3").await;
        let receipt = manager.pending_delivery(&agent).await.unwrap();
        assert_eq!(receipt.envelopes().len(), 1);
        assert_eq!(asked(), Some(vec!["q-3".to_owned()]));
        let message = notification(lease.id(), JobState::WaitingInput);
        receipt.commit(Vec::new(), message).await.unwrap();
        assert!(!manager.has_pending(&agent));
    }

    /// A read of a released answer that its job's completion withdraws before
    /// the read commits is stale: the answer drops out, and the completion is
    /// pending in its place.
    #[tokio::test]
    async fn a_completion_withdrawing_a_read_answer_stales_the_read() {
        let (_root, manager, owner) = crate::job::tests::runtime().await;
        let (job, child) = running_child(&manager, &owner).await;
        let text = "released-answer";
        let answer = crate::provider::protocol::AssistantItem::text("answer", 0, text);
        let answer = Message::Assistant(vec![answer]);
        let commit = manager.commit_child_message(&child, job, answer, text.into(), |_| Vec::new());
        commit.await.unwrap();
        manager.notify_owner(job).await.unwrap();
        let read = manager.pending_delivery(&owner).await.unwrap();
        assert_eq!(read.messages().len(), 1);
        manager.test_finish(job, serde_json::json!("result")).await;
        let stale = read
            .commit(Vec::new(), notification(job, JobState::Running))
            .await;
        assert_matches!(stale, Err(JobError::Stale));
        let pending = manager.pending_delivery(&owner).await.unwrap();
        assert!(pending.messages().is_empty(), "{:?}", pending.messages());
        let delivered: Vec<_> = (pending.envelopes().iter())
            .map(|envelope| (envelope.id, envelope.state))
            .collect();
        assert_eq!(delivered, [(job, JobState::Completed)]);
    }

    /// A running background agent job of `owner`, and its child.
    async fn running_child(manager: &JobManager, owner: &AgentId) -> (JobId, AgentId) {
        let spec = JobSpec {
            background: true,
            role: JobRole::Agent,
            ..JobSpec::test(owner.clone(), "agent")
        };
        let job = manager.test_running(spec).await.into_test_id();
        (job, manager.test_install(job).await)
    }

    /// Abandoning a read keeps its source message pending. Cancelling a started
    /// commit still finishes it, stales a competing read of the same events, and a
    /// later message with equal text is a new event.
    #[tokio::test]
    async fn cancelling_a_started_commit_finishes_acknowledgments_and_stales_a_competing_read() {
        for with_completion in [true, false] {
            let (_root, manager, owner) = crate::job::tests::runtime().await;
            if with_completion {
                let spec = JobSpec {
                    background: true,
                    ..JobSpec::test(owner.clone(), "test")
                };
                let completed = manager.test_create(spec).await;
                manager
                    .test_finish(completed, serde_json::json!("done"))
                    .await;
            }
            let (job, child) = running_child(&manager, &owner).await;
            let first = crate::job::tests::commit(&manager, &child, job, "progress").await;
            drop(manager.pending_delivery(&owner).await.unwrap());
            assert!(manager.has_pending(&owner));
            let batch = manager.pending_delivery(&owner).await.unwrap();
            let competing = manager.pending_delivery(&owner).await.unwrap();
            let message = notification(job, JobState::Running);
            {
                let commit = batch.commit(Vec::new(), message.clone());
                tokio::pin!(commit);
                // Dropping the caller once its commit is queued models an interrupt just
                // after this transaction's ownership boundary.
                let _ = futures_util::poll!(commit.as_mut());
            }
            // A read of the same events, even without any terminal job, delivers
            // nothing the interrupted caller's transaction delivered.
            let stale = competing.commit(Vec::new(), message.clone()).await;
            assert_matches!(stale, Err(JobError::Stale));
            let records = manager.store().records();
            let saved = records.iter().filter(|record| {
                record.agent == owner
                    && matches!(&record.event, SessionEvent::MessageCommitted { message: saved } if *saved == message)
            });
            assert_eq!(saved.count(), 1);
            assert!(!manager.has_pending(&owner));
            // Equal text is a new independent event when its source sequence differs.
            let second = crate::job::tests::commit(&manager, &child, job, "progress").await;
            assert_ne!(first, second);
            let pending = manager.pending_delivery(&owner).await.unwrap();
            assert_eq!(pending.messages().len(), 1);
            assert!(pending.envelopes().is_empty());
        }
    }
}
