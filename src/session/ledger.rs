//! The lifecycle of every model request folded from the journal: how each request
//! ended and which committed message answered it. Session statistics and host
//! projections share this one fold instead of re-deriving it from attempt events.
use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::{
    agent::{CompactionFault, Failure},
    identity::AgentId,
    provider::{ProviderError, protocol::Usage},
    session::{
        CompactionFailure, EventRecord, Message, MessageSeq, ModelPurpose, ProfileSnapshot,
        RecordSeq, RequestSeq, SessionEvent,
    },
};

/// Where a request is in its lifecycle. Requests are sequential per agent, so an
/// assistant message committed while the agent's newest request is open belongs to
/// it; `ResponseCompleted` confirms the link when its record lands. Times are epoch
/// milliseconds; a settled phase carries `at`, when the request left its last attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RequestPhase {
    /// Journaled, with no attempt started yet.
    Requested,
    /// An attempt is in flight. `message` is the assistant message it committed
    /// whose outcome has not been observed yet.
    Open {
        attempt: u64,
        message: Option<MessageSeq>,
    },
    /// The attempt failed and the next one starts at `due`. Only a provider
    /// failure is retried.
    Retrying {
        attempt: u64,
        failure: ProviderError,
        due: i64,
    },
    /// The last attempt failed, or none started before the request failed. A
    /// provider abort commits its partial response as `message` before failing; a
    /// refusal is never retried automatically.
    Failed {
        attempt: Option<u64>,
        failure: RequestFailure,
        message: Option<MessageSeq>,
        at: i64,
    },
    /// Cancelled, or left open when the session stopped. `attempt` is the last one
    /// started, which may have been cut mid-stream or during its retry backoff.
    Interrupted {
        attempt: Option<u64>,
        at: i64,
    },
    Completed {
        attempt: u64,
        at: i64,
    },
}

/// Why a request failed: its model attempt, or the compaction round it summarised for.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RequestFailure {
    #[error(transparent)]
    Model(#[from] Failure),
    #[error(transparent)]
    Compaction(#[from] CompactionFault),
}

impl RequestPhase {
    /// When a settled request left its last attempt; `None` while it may still
    /// produce an outcome.
    #[must_use]
    pub fn settled_at(&self) -> Option<i64> {
        match self {
            Self::Requested | Self::Open { .. } | Self::Retrying { .. } => None,
            Self::Failed { at, .. } | Self::Interrupted { at, .. } | Self::Completed { at, .. } => {
                Some(*at)
            }
        }
    }

    /// Whether a status card reports the request's failure or recovery.
    #[must_use]
    pub fn has_status_card(&self) -> bool {
        matches!(self, Self::Failed { .. } | Self::Retrying { .. })
    }

    /// Whether its response streams at the live tail: nothing settled or committed it.
    #[must_use]
    pub fn is_live_tail(&self) -> bool {
        matches!(self, Self::Requested | Self::Open { message: None, .. })
    }

    /// Whether its response stays at its journal position rather than in a status
    /// card: it completed, was interrupted, or committed its partial reply.
    #[must_use]
    pub fn settled_in_place(&self) -> bool {
        matches!(
            self,
            Self::Completed { .. }
                | Self::Interrupted { .. }
                | Self::Failed {
                    message: Some(_),
                    ..
                }
        )
    }

    /// The attempt whose response the request committed to history: a completion,
    /// or an abort's partial reply.
    #[must_use]
    pub fn committed_attempt(&self) -> Option<u64> {
        match self {
            Self::Completed { attempt, .. }
            | Self::Failed {
                attempt: Some(attempt),
                message: Some(_),
                ..
            } => Some(*attempt),
            _ => None,
        }
    }

    fn last_attempt(&self) -> Option<u64> {
        match self {
            Self::Requested => None,
            Self::Open { attempt, .. }
            | Self::Retrying { attempt, .. }
            | Self::Completed { attempt, .. } => Some(*attempt),
            Self::Failed { attempt, .. } | Self::Interrupted { attempt, .. } => *attempt,
        }
    }

    fn committed(&self) -> Option<MessageSeq> {
        match self {
            Self::Open { message, .. } => *message,
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RequestRecord {
    pub agent: AgentId,
    pub purpose: ModelPurpose,
    pub profile: ProfileSnapshot,
    pub requested_millis: i64,
    pub attempts: u64,
    /// Every usage report for the request, over all its attempts.
    pub usage: Usage,
    pub phase: RequestPhase,
    /// The record that last changed the request.
    revised: RecordSeq,
}

/// The requests one record changed: the request it names, and a pending request
/// it interrupted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RequestChanges {
    interrupted: Option<RequestSeq>,
    named: Option<RequestSeq>,
}

impl IntoIterator for RequestChanges {
    type Item = RequestSeq;
    type IntoIter = std::iter::Flatten<std::array::IntoIter<Option<RequestSeq>, 2>>;
    fn into_iter(self) -> Self::IntoIter {
        [self.interrupted, self.named].into_iter().flatten()
    }
}

/// Every model request of a journal, in sequence order.
#[derive(Clone, Debug, Default)]
pub struct RequestLedger {
    requests: BTreeMap<RequestSeq, RequestRecord>,
    /// Each agent's newest request, whatever its phase.
    latest: HashMap<AgentId, RequestSeq>,
    messages: HashMap<MessageSeq, RequestSeq>,
    /// The request each `ModelFailed` record failed, for the recovery that cites it.
    failures: HashMap<RecordSeq, RequestSeq>,
    contexts: HashMap<RecordSeq, (ModelPurpose, ProfileSnapshot)>,
    /// Each request under the record that last changed it.
    revisions: BTreeSet<(RecordSeq, RequestSeq)>,
}

impl RequestLedger {
    /// Fold one record, returning the requests it changed. Records must arrive in
    /// sequence order; a record for a request the ledger never saw is ignored.
    pub fn observe(&mut self, record: &EventRecord) -> RequestChanges {
        let at = record.timestamp_millis;
        let mut changes = RequestChanges::default();
        match &record.event {
            SessionEvent::ModelContext { context } => {
                self.contexts
                    .insert(record.sequence, (context.purpose, context.profile.clone()));
            }
            SessionEvent::ModelRequested { context, .. } => {
                let Some((purpose, profile)) = self.contexts.get(context).cloned() else {
                    return changes;
                };
                // A newer request supersedes whatever the previous one left pending.
                changes.interrupted = self.interrupt(&record.agent, at);
                let request = record.sequence.request();
                self.requests.insert(
                    request,
                    RequestRecord {
                        agent: record.agent.clone(),
                        purpose,
                        profile,
                        requested_millis: at,
                        attempts: 0,
                        usage: Usage::default(),
                        phase: RequestPhase::Requested,
                        revised: record.sequence,
                    },
                );
                self.latest.insert(record.agent.clone(), request);
                changes.named = Some(request);
            }
            SessionEvent::ModelAttemptStarted(attempt) => {
                if let Some(request) = self.requests.get_mut(&attempt.request) {
                    changes.named = Some(attempt.request);
                    request.attempts += 1;
                    request.phase = RequestPhase::Open {
                        attempt: attempt.attempt,
                        message: None,
                    };
                }
            }
            SessionEvent::MessageCommitted {
                message: Message::Assistant(_),
            } => {
                let Some(&request) = self.latest.get(&record.agent) else {
                    return changes;
                };
                if let Some(open) = self.requests.get_mut(&request)
                    && let RequestPhase::Open { message, .. } = &mut open.phase
                {
                    *message = Some(record.sequence.message());
                    self.messages.insert(record.sequence.message(), request);
                    changes.named = Some(request);
                }
            }
            SessionEvent::ModelFailed { attempt, failure } => {
                let (attempt, request) = (attempt.attempt, attempt.request);
                self.failures.insert(record.sequence, request);
                changes.named = self.settle(request, |phase| RequestPhase::Failed {
                    attempt: Some(attempt),
                    failure: failure.clone().into(),
                    message: phase.committed(),
                    at,
                });
            }
            SessionEvent::ModelRecoveryScheduled {
                failure,
                delay_millis,
            } => {
                if let Some(&request) = self.failures.get(failure)
                    && let Some(failed) = self.requests.get_mut(&request)
                    && let RequestPhase::Failed {
                        attempt: Some(attempt),
                        failure: RequestFailure::Model(Failure::Provider(message, kind)),
                        ..
                    } = &failed.phase
                {
                    failed.phase = RequestPhase::Retrying {
                        attempt: *attempt,
                        failure: kind.error(message.clone()),
                        due: at.saturating_add_unsigned(*delay_millis),
                    };
                    changes.named = Some(request);
                }
            }
            SessionEvent::ModelAttemptInterrupted(attempt) => {
                changes.named = self.settle(attempt.request, |_| RequestPhase::Interrupted {
                    attempt: Some(attempt.attempt),
                    at,
                });
            }
            SessionEvent::ResponseCompleted {
                attempt, message, ..
            } => {
                self.messages.insert(*message, attempt.request);
                changes.named = self.settle(attempt.request, |_| RequestPhase::Completed {
                    attempt: attempt.attempt,
                    at,
                });
            }
            SessionEvent::Compaction { checkpoint } => {
                let attempt = &checkpoint.attempt;
                changes.named = self.settle(attempt.request, |_| RequestPhase::Completed {
                    attempt: attempt.attempt,
                    at,
                });
            }
            SessionEvent::CompactionFailed { failure, error } => {
                let (request, attempt) = match failure {
                    CompactionFailure::BeforeRequest => return changes,
                    CompactionFailure::Requested(request) => (*request, None),
                    CompactionFailure::Attempted(attempt) => {
                        (attempt.request, Some(attempt.attempt))
                    }
                };
                // What a summary attempt settled itself, such as its model failure, stands.
                if self.open(&record.agent) != Some(request) {
                    return changes;
                }
                changes.named = self.settle(request, |_| match error {
                    CompactionFault::Interrupted => RequestPhase::Interrupted { attempt, at },
                    error => RequestPhase::Failed {
                        attempt,
                        failure: error.clone().into(),
                        message: None,
                        at,
                    },
                });
            }
            SessionEvent::Usage { request, usage } => {
                if let Some(record) = self.requests.get_mut(request) {
                    record.usage.accumulate(*usage);
                    changes.named = Some(*request);
                }
            }
            SessionEvent::AgentInterrupted => {
                changes.interrupted = self.interrupt(&record.agent, at);
            }
            _ => {}
        }
        for request in changes {
            if let Some(changed) = self.requests.get_mut(&request) {
                self.revisions.remove(&(changed.revised, request));
                changed.revised = record.sequence;
                self.revisions.insert((record.sequence, request));
            }
        }
        changes
    }

    /// Requests changed by records after `through`, for observers that fold
    /// records incrementally.
    pub fn changed_after(&self, through: RecordSeq) -> impl Iterator<Item = RequestSeq> + '_ {
        let revisions = self
            .revisions
            .range((through.next(), RequestSeq::default())..);
        revisions.map(|(_, request)| *request)
    }

    /// Returns the request when the ledger has it.
    fn settle(
        &mut self,
        request: RequestSeq,
        phase: impl FnOnce(&RequestPhase) -> RequestPhase,
    ) -> Option<RequestSeq> {
        let record = self.requests.get_mut(&request)?;
        record.phase = phase(&record.phase);
        Some(request)
    }

    /// Close the agent's pending request, if any, as interrupted, returning it.
    fn interrupt(&mut self, agent: &AgentId, at: i64) -> Option<RequestSeq> {
        let request = self.open(agent)?;
        self.settle(request, |phase| RequestPhase::Interrupted {
            attempt: phase.last_attempt(),
            at,
        })
    }

    #[must_use]
    pub fn get(&self, request: RequestSeq) -> Option<&RequestRecord> {
        self.requests.get(&request)
    }

    /// Every request in sequence order.
    pub fn iter(&self) -> impl Iterator<Item = (RequestSeq, &RequestRecord)> {
        self.requests.iter().map(|(seq, record)| (*seq, record))
    }

    /// The agent's newest request, whatever its phase.
    #[must_use]
    pub fn latest(&self, agent: &AgentId) -> Option<RequestSeq> {
        self.latest.get(agent).copied()
    }

    /// The agent's newest request while it is still pending.
    #[must_use]
    pub fn open(&self, agent: &AgentId) -> Option<RequestSeq> {
        self.latest(agent)
            .filter(|request| self.requests[request].phase.settled_at().is_none())
    }

    /// The request a committed assistant message answered.
    #[must_use]
    pub fn request_of(&self, message: MessageSeq) -> Option<RequestSeq> {
        self.messages.get(&message).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        identity::{EventId, SessionId},
        provider::{ProviderErrorKind, protocol::AssistantItem},
        session::{
            AttemptRef, CompactionCheckpoint, CompletedOutcome, ModelContext,
            tests::{self, usage},
        },
    };

    /// Appends records for one agent, stamped `at(sequence)`.
    struct Journal {
        agent: AgentId,
        ledger: RequestLedger,
        next: u64,
        /// What the last record changed.
        changed: Vec<RequestSeq>,
    }

    impl Journal {
        fn new() -> Self {
            Self {
                agent: AgentId::root(SessionId::from_bytes([7; 16])),
                ledger: RequestLedger::default(),
                next: 1,
                changed: Vec::new(),
            }
        }

        fn record(&mut self, event: SessionEvent) -> RecordSeq {
            let sequence = RecordSeq::from(self.next);
            self.next += 1;
            let changes = self.ledger.observe(&EventRecord {
                id: EventId::generate().unwrap(),
                sequence,
                timestamp_millis: at(sequence),
                agent: self.agent.clone(),
                event,
            });
            self.changed = changes.into_iter().collect();
            sequence
        }

        /// A request against a fresh context for `purpose`.
        fn request(&mut self, purpose: ModelPurpose) -> RequestSeq {
            let context = self.record(SessionEvent::ModelContext {
                context: ModelContext::test(purpose, tests::profile()),
            });
            self.record(tests::requested(context)).request()
        }

        /// A request whose first attempt has started.
        fn attempted(&mut self, purpose: ModelPurpose) -> RequestSeq {
            let request = self.request(purpose);
            self.attempt(request, 1);
            request
        }

        fn attempt(&mut self, request: RequestSeq, attempt: u64) {
            self.record(SessionEvent::ModelAttemptStarted(attempt_ref(
                request, attempt,
            )));
        }

        fn failed(
            &mut self,
            request: RequestSeq,
            attempt: u64,
            failure: fn(String) -> Failure,
        ) -> RecordSeq {
            self.record(SessionEvent::ModelFailed {
                attempt: attempt_ref(request, attempt),
                failure: failure("boom".into()),
            })
        }

        fn recover(&mut self, failure: RecordSeq, delay_millis: u64) {
            self.record(SessionEvent::ModelRecoveryScheduled {
                failure,
                delay_millis,
            });
        }

        fn reply(&mut self) -> MessageSeq {
            self.record(SessionEvent::MessageCommitted {
                message: Message::Assistant(vec![AssistantItem::text("answer", 0, "hi")]),
            })
            .message()
        }

        fn usage(&mut self, request: RequestSeq, usage: Usage) {
            self.record(SessionEvent::Usage { request, usage });
        }

        fn record_of(&self, request: RequestSeq) -> &RequestRecord {
            self.ledger.get(request).unwrap()
        }

        fn phase(&self, request: RequestSeq) -> &RequestPhase {
            &self.record_of(request).phase
        }

        /// When the last record landed.
        fn now(&self) -> i64 {
            at(self.next - 1)
        }

        fn open(&self) -> Option<RequestSeq> {
            self.ledger.open(&self.agent)
        }

        fn request_of(&self, message: MessageSeq) -> Option<RequestSeq> {
            self.ledger.request_of(message)
        }
    }

    fn at(sequence: impl Into<RecordSeq>) -> i64 {
        sequence.into().get() as i64 * 1000
    }

    fn attempt_ref(request: RequestSeq, attempt: u64) -> AttemptRef {
        AttemptRef { request, attempt }
    }

    fn failed(attempt: Option<u64>, message: Option<MessageSeq>, at: i64) -> RequestPhase {
        RequestPhase::Failed {
            attempt,
            failure: Failure::Other("boom".into()).into(),
            message,
            at,
        }
    }

    #[test]
    fn a_request_retries_through_its_journaled_failure_and_completes() {
        let mut journal = Journal::new();
        let request = journal.request(ModelPurpose::Agent);
        assert_eq!(journal.phase(request), &RequestPhase::Requested);
        assert_eq!(journal.open(), Some(request));
        assert_eq!(journal.record_of(request).requested_millis, at(request));

        journal.attempt(request, 1);
        let transport = |message| Failure::Provider(message, ProviderErrorKind::Transport);
        let failure = journal.failed(request, 1, transport);
        let failed = RequestPhase::Failed {
            attempt: Some(1),
            failure: transport("boom".into()).into(),
            message: None,
            at: at(failure),
        };
        assert_eq!(journal.phase(request), &failed);
        journal.recover(failure, 250);
        // The recovery names only its failure; the ledger reports the request.
        assert_eq!(journal.changed, [request]);
        let retrying = RequestPhase::Retrying {
            attempt: 1,
            failure: ProviderErrorKind::Transport.error("boom"),
            due: journal.now() + 250,
        };
        assert_eq!(journal.phase(request), &retrying);
        assert_eq!(journal.open(), Some(request));
        // A schedule citing a failure the request has already left changes nothing.
        journal.recover(failure, 1);
        assert_eq!(journal.phase(request), &retrying);
        assert_eq!(journal.changed, []);

        journal.attempt(request, 2);
        let open = |message| RequestPhase::Open {
            attempt: 2,
            message,
        };
        assert_eq!(journal.phase(request), &open(None));
        journal.usage(request, usage(10, 2, 3));
        journal.usage(request, usage(1, 1, 1));
        let message = journal.reply();
        assert_eq!(journal.phase(request), &open(Some(message)));
        assert_eq!(journal.request_of(message), Some(request));
        let completed = journal.record(SessionEvent::ResponseCompleted {
            attempt: attempt_ref(request, 2),
            message,
            outcome: CompletedOutcome::Answer,
        });
        let record = journal.record_of(request);
        let at = at(completed);
        assert_eq!(record.phase, RequestPhase::Completed { attempt: 2, at });
        assert_eq!((record.attempts, record.usage), (2, usage(11, 3, 4)));
        assert_eq!(journal.open(), None);
        assert_eq!(journal.ledger.latest(&journal.agent), Some(request));
    }

    #[test]
    fn failures_settle_by_kind_and_keep_an_aborted_reply() {
        let mut journal = Journal::new();
        let refused = journal.attempted(ModelPurpose::Agent);
        journal.failed(refused, 1, Failure::Refused);
        let declined = RequestPhase::Failed {
            attempt: Some(1),
            failure: Failure::Refused("boom".into()).into(),
            message: None,
            at: journal.now(),
        };
        assert_eq!(journal.phase(refused), &declined);

        // A provider abort commits its partial response before failing.
        let aborted = journal.attempted(ModelPurpose::Agent);
        let message = journal.reply();
        let failure = journal.failed(aborted, 1, Failure::Other);
        let partial = failed(Some(1), Some(message), at(failure));
        assert_eq!(journal.phase(aborted), &partial);
        assert_eq!(journal.request_of(message), Some(aborted));
        // A message outside an open attempt belongs to no request.
        let stray = journal.reply();
        assert_eq!(journal.request_of(stray), None);

        let unattempted = journal.request(ModelPurpose::Compaction);
        let fault = CompactionFault::Checkpoint(crate::session::CheckpointError::StaleTodos);
        journal.record(SessionEvent::CompactionFailed {
            failure: CompactionFailure::Requested(unattempted),
            error: fault.clone(),
        });
        let failed = RequestPhase::Failed {
            attempt: None,
            failure: RequestFailure::Compaction(fault),
            message: None,
            at: journal.now(),
        };
        assert_eq!(journal.phase(unattempted), &failed);
    }

    #[test]
    fn interruptions_close_only_pending_requests() {
        let mut journal = Journal::new();
        let requested = journal.request(ModelPurpose::Agent);
        let stop = journal.record(SessionEvent::AgentInterrupted);
        let unattempted = RequestPhase::Interrupted {
            attempt: None,
            at: at(stop),
        };
        assert_eq!(journal.phase(requested), &unattempted);
        assert_eq!(journal.changed, [requested]);
        // A settled request is left alone by a later interruption.
        journal.record(SessionEvent::AgentInterrupted);
        assert_eq!(journal.phase(requested), &unattempted);
        assert_eq!(journal.changed, []);

        let after_attempt = |journal: &Journal| RequestPhase::Interrupted {
            attempt: Some(1),
            at: journal.now(),
        };
        let retrying = journal.attempted(ModelPurpose::Agent);
        let transport = |message| Failure::Provider(message, ProviderErrorKind::Transport);
        let failure = journal.failed(retrying, 1, transport);
        journal.recover(failure, 5);
        journal.record(SessionEvent::AgentInterrupted);
        assert_eq!(journal.phase(retrying), &after_attempt(&journal));

        let cut = journal.attempted(ModelPurpose::Agent);
        journal.record(SessionEvent::ModelAttemptInterrupted(attempt_ref(cut, 1)));
        assert_eq!(journal.phase(cut), &after_attempt(&journal));

        // A newer request supersedes whatever the previous one left open.
        let through = RecordSeq::from(journal.next - 1);
        let open = journal.attempted(ModelPurpose::Agent);
        let next = journal.request(ModelPurpose::Agent);
        assert_eq!(journal.changed, [open, next]);
        // Incremental observers see each request changed since, once.
        let changed: Vec<_> = journal.ledger.changed_after(through).collect();
        assert_eq!(changed, [open, next]);
        assert_eq!(journal.phase(open), &after_attempt(&journal));
        assert_eq!(journal.phase(next).settled_at(), None);
        assert_eq!(journal.open(), Some(next));
    }

    #[test]
    fn a_checkpoint_completes_its_summary_request() {
        let mut journal = Journal::new();
        let checkpointed = journal.attempted(ModelPurpose::Compaction);
        journal.record(SessionEvent::Compaction {
            checkpoint: CompactionCheckpoint {
                frontier: 1.into(),
                message: Message::User(Vec::new()),
                todos: Vec::new(),
                retained: Vec::new(),
                attempt: attempt_ref(checkpointed, 1),
                before_tokens: 10,
                after_tokens: 5,
            },
        });
        let completed = RequestPhase::Completed {
            attempt: 1,
            at: journal.now(),
        };
        assert_eq!(journal.phase(checkpointed), &completed);
        let record = journal.record_of(checkpointed);
        assert_eq!(record.purpose, ModelPurpose::Compaction);
    }
}
