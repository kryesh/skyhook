//! The lifecycle of every model request folded from the journal: how each request
//! ended and which committed message answered it. Session statistics and host
//! projections share this one fold instead of re-deriving it from attempt events.
use std::{
    collections::{BTreeMap, HashMap},
    time::Duration,
};

use crate::{
    identity::AgentId,
    provider::protocol::Usage,
    session::{
        CompactionFailure, EventRecord, Message, MessageSeq, ModelFailureKind, ModelPurpose,
        ProfileSnapshot, RecordSeq, RequestSeq, SessionEvent,
    },
};

/// Where a request is in its lifecycle. Requests are sequential per agent, so an
/// assistant message committed while the agent's newest request is open belongs to
/// it; `ResponseCompleted` confirms the link when its record lands.
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
    /// The attempt failed and the next one starts after `delay`.
    Retrying {
        attempt: u64,
        delay: Duration,
        error: String,
    },
    /// The last attempt failed, or none started before the request failed. A
    /// provider abort commits its partial response as `message` before failing.
    Failed {
        attempt: Option<u64>,
        error: String,
        message: Option<MessageSeq>,
    },
    /// The model declined; never retried automatically.
    Refused {
        attempt: u64,
        error: String,
    },
    /// Cancelled, or left open when the session stopped. `attempt` is the last one
    /// started, which may have been cut mid-stream or during its retry backoff.
    Interrupted {
        attempt: Option<u64>,
    },
    Completed {
        attempt: u64,
    },
}

impl RequestPhase {
    /// Whether the request may still produce an outcome.
    #[must_use]
    pub fn pending(&self) -> bool {
        matches!(
            self,
            Self::Requested | Self::Open { .. } | Self::Retrying { .. }
        )
    }

    fn last_attempt(&self) -> Option<u64> {
        match self {
            Self::Requested => None,
            Self::Open { attempt, .. }
            | Self::Retrying { attempt, .. }
            | Self::Refused { attempt, .. }
            | Self::Completed { attempt } => Some(*attempt),
            Self::Failed { attempt, .. } | Self::Interrupted { attempt } => *attempt,
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
    /// When the request last left an attempt: stamped by every transition out of
    /// `Open`, cleared when the next attempt starts.
    pub finished_millis: Option<i64>,
    pub attempts: u64,
    /// Every usage report for the request, over all its attempts.
    pub usage: Usage,
    pub phase: RequestPhase,
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
#[derive(Default)]
pub struct RequestLedger {
    requests: BTreeMap<RequestSeq, RequestRecord>,
    /// Each agent's newest request, whatever its phase.
    latest: HashMap<AgentId, RequestSeq>,
    messages: HashMap<MessageSeq, RequestSeq>,
    /// The request each `ModelFailed` record failed, for the recovery that cites it.
    failures: HashMap<RecordSeq, RequestSeq>,
    contexts: HashMap<RecordSeq, (ModelPurpose, ProfileSnapshot)>,
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
                        finished_millis: None,
                        attempts: 0,
                        usage: Usage::default(),
                        phase: RequestPhase::Requested,
                    },
                );
                self.latest.insert(record.agent.clone(), request);
                changes.named = Some(request);
            }
            SessionEvent::ModelAttemptStarted(attempt) => {
                if let Some(request) = self.requests.get_mut(&attempt.request) {
                    changes.named = Some(attempt.request);
                    request.attempts += 1;
                    request.finished_millis = None;
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
            SessionEvent::ModelFailed {
                attempt,
                error,
                kind,
            } => {
                let (attempt, request) = (attempt.attempt, attempt.request);
                self.failures.insert(record.sequence, request);
                changes.named = self.settle(request, at, |phase| match kind {
                    ModelFailureKind::Refusal => RequestPhase::Refused {
                        attempt,
                        error: error.clone(),
                    },
                    ModelFailureKind::Error => RequestPhase::Failed {
                        attempt: Some(attempt),
                        error: error.clone(),
                        message: phase.committed(),
                    },
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
                        error,
                        ..
                    } = &failed.phase
                {
                    failed.phase = RequestPhase::Retrying {
                        attempt: *attempt,
                        delay: Duration::from_millis(*delay_millis),
                        error: error.clone(),
                    };
                    failed.finished_millis = Some(at);
                    changes.named = Some(request);
                }
            }
            SessionEvent::ModelAttemptInterrupted(attempt) => {
                let interrupted = RequestPhase::Interrupted {
                    attempt: Some(attempt.attempt),
                };
                changes.named = self.settle(attempt.request, at, |_| interrupted);
            }
            SessionEvent::ResponseCompleted {
                attempt, message, ..
            } => {
                self.messages.insert(*message, attempt.request);
                let completed = RequestPhase::Completed {
                    attempt: attempt.attempt,
                };
                changes.named = self.settle(attempt.request, at, |_| completed);
            }
            SessionEvent::Compaction { checkpoint } => {
                let completed = RequestPhase::Completed {
                    attempt: checkpoint.attempt.attempt,
                };
                changes.named = self.settle(checkpoint.attempt.request, at, |_| completed);
            }
            SessionEvent::CompactionSkipped { attempt, .. } => {
                let completed = RequestPhase::Completed {
                    attempt: attempt.attempt,
                };
                changes.named = self.settle(attempt.request, at, |_| completed);
            }
            SessionEvent::CompactionFailed { failure, error } => {
                let (request, attempt) = match failure {
                    CompactionFailure::BeforeRequest => return changes,
                    CompactionFailure::Requested(request) => (*request, None),
                    CompactionFailure::Attempted(attempt) => {
                        (attempt.request, Some(attempt.attempt))
                    }
                };
                changes.named = self.settle(request, at, |_| RequestPhase::Failed {
                    attempt,
                    error: error.clone(),
                    message: None,
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
        changes
    }

    /// Returns the request when the ledger has it.
    fn settle(
        &mut self,
        request: RequestSeq,
        at: i64,
        phase: impl FnOnce(&RequestPhase) -> RequestPhase,
    ) -> Option<RequestSeq> {
        let record = self.requests.get_mut(&request)?;
        record.phase = phase(&record.phase);
        record.finished_millis = Some(at);
        Some(request)
    }

    /// Close the agent's pending request, if any, as interrupted, returning it.
    fn interrupt(&mut self, agent: &AgentId, at: i64) -> Option<RequestSeq> {
        let request = *self.latest.get(agent)?;
        let pending = self.requests.get_mut(&request)?;
        if !pending.phase.pending() {
            return None;
        }
        pending.phase = RequestPhase::Interrupted {
            attempt: pending.phase.last_attempt(),
        };
        pending.finished_millis = Some(at);
        Some(request)
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
            .filter(|request| self.requests[request].phase.pending())
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
        provider::protocol::{AssistantItem, HistoryLifetime},
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
            self.record(SessionEvent::ModelRequested {
                context,
                checkpoint: None,
                history: Vec::new(),
                tail: Vec::new(),
                history_lifetime: HistoryLifetime::default(),
            })
            .request()
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
            kind: ModelFailureKind,
        ) -> RecordSeq {
            self.record(SessionEvent::ModelFailed {
                attempt: attempt_ref(request, attempt),
                error: "boom".into(),
                kind,
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

        fn finished(&self, request: RequestSeq) -> Option<i64> {
            self.record_of(request).finished_millis
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

    fn open(attempt: u64, message: Option<MessageSeq>) -> RequestPhase {
        RequestPhase::Open { attempt, message }
    }

    fn failed(attempt: Option<u64>, message: Option<MessageSeq>) -> RequestPhase {
        RequestPhase::Failed {
            attempt,
            error: "boom".into(),
            message,
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
        let failure = journal.failed(request, 1, ModelFailureKind::Error);
        assert_eq!(journal.phase(request), &failed(Some(1), None));
        assert_eq!(journal.finished(request), Some(at(failure)));
        journal.recover(failure, 250);
        // The recovery names only its failure; the ledger reports the request.
        assert_eq!(journal.changed, [request]);
        let retrying = RequestPhase::Retrying {
            attempt: 1,
            delay: Duration::from_millis(250),
            error: "boom".into(),
        };
        assert_eq!(journal.phase(request), &retrying);
        assert_eq!(journal.open(), Some(request));
        // A schedule citing a failure the request has already left changes nothing.
        journal.recover(failure, 1);
        assert_eq!(journal.phase(request), &retrying);
        assert_eq!(journal.changed, []);

        journal.attempt(request, 2);
        assert_eq!(journal.phase(request), &open(2, None));
        assert_eq!(journal.finished(request), None);
        journal.usage(request, usage(10, 2, 3));
        journal.usage(request, usage(1, 1, 1));
        let message = journal.reply();
        assert_eq!(journal.phase(request), &open(2, Some(message)));
        assert_eq!(journal.request_of(message), Some(request));
        let completed = journal.record(SessionEvent::ResponseCompleted {
            attempt: attempt_ref(request, 2),
            message,
            outcome: CompletedOutcome::Answer,
        });
        let record = journal.record_of(request);
        assert_eq!(record.phase, RequestPhase::Completed { attempt: 2 });
        assert_eq!((record.attempts, record.usage), (2, usage(11, 3, 4)));
        assert_eq!(record.finished_millis, Some(at(completed)));
        assert_eq!(journal.open(), None);
        assert_eq!(journal.ledger.latest(&journal.agent), Some(request));
    }

    #[test]
    fn failures_settle_by_kind_and_keep_an_aborted_reply() {
        let mut journal = Journal::new();
        let refused = journal.attempted(ModelPurpose::Agent);
        journal.failed(refused, 1, ModelFailureKind::Refusal);
        assert_eq!(
            journal.phase(refused),
            &RequestPhase::Refused {
                attempt: 1,
                error: "boom".into()
            }
        );

        // A provider abort commits its partial response before failing.
        let aborted = journal.attempted(ModelPurpose::Agent);
        let message = journal.reply();
        journal.failed(aborted, 1, ModelFailureKind::Error);
        assert_eq!(journal.phase(aborted), &failed(Some(1), Some(message)));
        assert_eq!(journal.request_of(message), Some(aborted));
        // A message outside an open attempt belongs to no request.
        let stray = journal.reply();
        assert_eq!(journal.request_of(stray), None);

        let unattempted = journal.request(ModelPurpose::Compaction);
        journal.record(SessionEvent::CompactionFailed {
            failure: CompactionFailure::Requested(unattempted),
            error: "boom".into(),
        });
        assert_eq!(journal.phase(unattempted), &failed(None, None));
    }

    #[test]
    fn interruptions_close_only_pending_requests() {
        let mut journal = Journal::new();
        let requested = journal.request(ModelPurpose::Agent);
        let stop = journal.record(SessionEvent::AgentInterrupted);
        let unattempted = RequestPhase::Interrupted { attempt: None };
        assert_eq!(journal.phase(requested), &unattempted);
        assert_eq!(journal.finished(requested), Some(at(stop)));
        assert_eq!(journal.changed, [requested]);
        // A settled request is left alone by a later interruption.
        journal.record(SessionEvent::AgentInterrupted);
        assert_eq!(journal.phase(requested), &unattempted);
        assert_eq!(journal.changed, []);

        let after_attempt = RequestPhase::Interrupted { attempt: Some(1) };
        let retrying = journal.attempted(ModelPurpose::Agent);
        let failure = journal.failed(retrying, 1, ModelFailureKind::Error);
        journal.recover(failure, 5);
        journal.record(SessionEvent::AgentInterrupted);
        assert_eq!(journal.phase(retrying), &after_attempt);

        let cut = journal.attempted(ModelPurpose::Agent);
        journal.record(SessionEvent::ModelAttemptInterrupted(attempt_ref(cut, 1)));
        assert_eq!(journal.phase(cut), &after_attempt);

        // A newer request supersedes whatever the previous one left open.
        let open = journal.attempted(ModelPurpose::Agent);
        let next = journal.request(ModelPurpose::Agent);
        assert_eq!(journal.changed, [open, next]);
        assert_eq!(journal.phase(open), &after_attempt);
        assert_eq!(journal.open(), Some(next));
    }

    #[test]
    fn compaction_outcomes_complete_their_request() {
        let mut journal = Journal::new();
        let skipped = journal.attempted(ModelPurpose::Compaction);
        journal.record(SessionEvent::CompactionSkipped {
            attempt: attempt_ref(skipped, 1),
            reason: "nothing".into(),
        });
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
        let completed = RequestPhase::Completed { attempt: 1 };
        assert_eq!(journal.phase(skipped), &completed);
        assert_eq!(journal.phase(checkpointed), &completed);
        let record = journal.record_of(checkpointed);
        assert_eq!(record.purpose, ModelPurpose::Compaction);
    }
}
