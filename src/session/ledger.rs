//! The lifecycle of model requests folded from the journal: how each request ended
//! and which committed message answered it. The fold keeps only each agent's newest
//! request and what its turn leaves open; the core writes each request's row as its
//! records change it, and what it decides of a record as a [`Fact`].
use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::{
    agent::{CompactionFault, Failure},
    identity::AgentId,
    provider::{ProviderError, protocol::Usage},
    session::{
        AttemptFailure, AttemptRef, CompactionFailure, EventRecord, Message, MessageSeq,
        ModelPurpose, ProfileSnapshot, RecordSeq, RequestSeq, SessionEvent,
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
    /// whose outcome has not been observed yet; `usage` what it reported.
    Open {
        attempt: u64,
        message: Option<MessageSeq>,
        usage: Option<Usage>,
    },
    /// The attempt failed and the next one starts at `due`. Only a provider
    /// failure is retried.
    Retrying {
        attempt: u64,
        failure: ProviderError,
        due: i64,
    },
    /// The last attempt failed, or none started before the request failed. A
    /// refusal is never retried automatically.
    Failed {
        attempt: Option<u64>,
        failure: RequestFailure,
        at: i64,
    },
    /// Cancelled, or left open when the session stopped. `attempt` is the last one
    /// started, which may have been cut mid-stream or during its retry backoff.
    Interrupted { attempt: Option<u64>, at: i64 },
    /// `usage` is what the completed attempt reported.
    Completed {
        attempt: u64,
        at: i64,
        usage: Option<Usage>,
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
    pub fn settled_at(&self) -> Option<i64> {
        match self {
            Self::Requested | Self::Open { .. } | Self::Retrying { .. } => None,
            Self::Failed { at, .. } | Self::Interrupted { at, .. } | Self::Completed { at, .. } => {
                Some(*at)
            }
        }
    }

    /// Whether a status card reports the request's failure or recovery.
    pub fn has_status_card(&self) -> bool {
        matches!(self, Self::Failed { .. } | Self::Retrying { .. })
    }

    /// Whether its response streams at the live tail: nothing settled or committed it.
    pub fn is_live_tail(&self) -> bool {
        matches!(self, Self::Requested | Self::Open { message: None, .. })
    }

    /// Whether its response stays at its journal position rather than in a status
    /// card: it completed or was interrupted.
    pub fn settled_in_place(&self) -> bool {
        matches!(self, Self::Completed { .. } | Self::Interrupted { .. })
    }

    /// What the completed attempt reported, once the request completed.
    pub fn completed_usage(&self) -> Option<Usage> {
        match self {
            Self::Completed { usage, .. } => *usage,
            _ => None,
        }
    }

    pub(crate) fn last_attempt(&self) -> Option<u64> {
        match self {
            Self::Requested => None,
            Self::Open { attempt, .. }
            | Self::Retrying { attempt, .. }
            | Self::Completed { attempt, .. } => Some(*attempt),
            Self::Failed { attempt, .. } | Self::Interrupted { attempt, .. } => *attempt,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RequestRecord {
    pub agent: AgentId,
    /// The model context it was sent under.
    pub context: RecordSeq,
    pub purpose: ModelPurpose,
    pub profile: ProfileSnapshot,
    /// The last message its history held.
    pub through: Option<MessageSeq>,
    pub requested_millis: i64,
    pub attempts: u64,
    /// Every usage report for the request, over all its attempts.
    pub usage: Usage,
    pub phase: RequestPhase,
    /// The record that moved it to its phase: the request itself, its attempt's start,
    /// or what settled it or failed the attempt it retries, which the phase reads.
    pub(crate) phased: RecordSeq,
}

/// Each agent's newest request and what its turn leaves open: the part of the
/// ledger live work reads, bounded by the session's agents rather than its history.
#[derive(Clone, Debug, Default)]
pub struct ResidentLedger {
    /// Each agent's newest request, whatever its phase.
    latest: HashMap<AgentId, RequestSeq>,
    /// Those requests.
    requests: BTreeMap<RequestSeq, RequestRecord>,
    /// Calls without a result, by agent and call id, with the message that made
    /// each and its tool name, in commit order.
    unanswered: indexmap::IndexMap<(AgentId, String), (MessageSeq, String)>,
    /// Agents whose newest history is input their model has yet to answer: a
    /// prompt, a notification or results, until a response or the turn's end.
    awaiting: BTreeSet<AgentId>,
    /// Each agent's newest model context of each purpose: the only one its next
    /// request of that purpose may name.
    contexts: HashMap<AgentId, HashMap<ModelPurpose, (RecordSeq, ProfileSnapshot)>>,
    /// The previous record, when it failed a request for good, with that request.
    failure: Option<(RecordSeq, RequestSeq)>,
}

/// What folding a record decided that presenting it reads beyond the record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fact {
    /// An assistant message, committed under this request.
    Answers(RequestSeq),
    /// A tool result, answering a call of this assistant message.
    Answered(MessageSeq),
    /// A failed turn, ended on its request's failure, which reports it.
    EndedOn,
}

/// What one record changed of a [`ResidentLedger`].
#[derive(Default)]
pub(crate) struct Folded {
    /// The requests it changed: one it interrupted, then one it named.
    pub(crate) changed: [Option<RequestSeq>; 2],
    /// The agent's previous request, which a newer one replaced, as it left it.
    pub(crate) superseded: Option<(RequestSeq, RequestRecord)>,
    pub(crate) fact: Option<Fact>,
}

/// Ledgers are equal when they hold the same requests and turns; a failure is
/// decided by the record right after it.
impl PartialEq for ResidentLedger {
    fn eq(&self, other: &Self) -> bool {
        (self.latest == other.latest)
            && (self.requests == other.requests)
            && (self.unanswered == other.unanswered)
            && (self.awaiting == other.awaiting)
            && (self.contexts == other.contexts)
    }
}

impl ResidentLedger {
    /// The ledger the rows hold: each agent's newest `requests`, its newest model
    /// contexts, the calls no result answers yet, and the agents `awaiting` their model.
    pub(crate) fn opened(
        requests: BTreeMap<RequestSeq, RequestRecord>,
        contexts: HashMap<AgentId, HashMap<ModelPurpose, (RecordSeq, ProfileSnapshot)>>,
        unanswered: indexmap::IndexMap<(AgentId, String), (MessageSeq, String)>,
        awaiting: BTreeSet<AgentId>,
    ) -> Self {
        let latest = (requests.iter())
            .map(|(request, record)| (record.agent.clone(), *request))
            .collect();
        Self {
            latest,
            requests,
            unanswered,
            awaiting,
            contexts,
            failure: None,
        }
    }

    /// Fold one record, returning what it changed.
    pub(crate) fn fold(&mut self, record: &EventRecord) -> Folded {
        let (at, agent, by) = (record.timestamp_millis, &record.agent, record.sequence);
        let mut folded = Folded::default();
        let failed = self.failure.take();
        // The request the record names, and a pending one it interrupted.
        let (mut named, mut interrupted) = (None, None);
        match &record.event {
            SessionEvent::ModelContext { context } => {
                let newest = (record.sequence, context.profile.clone());
                let contexts = self.contexts.entry(agent.clone()).or_default();
                contexts.insert(context.purpose, newest);
            }
            SessionEvent::ModelRequested {
                context, through, ..
            } => {
                let named_context = (self.contexts.get(agent).into_iter().flatten())
                    .find(|(_, (newest, _))| newest == context);
                let Some((&purpose, (_, profile))) = named_context else {
                    return folded;
                };
                let profile = profile.clone();
                // A newer request supersedes whatever the previous one left pending.
                interrupted = self.interrupt(agent, record);
                let request = record.sequence.request();
                self.requests.insert(
                    request,
                    RequestRecord {
                        agent: agent.clone(),
                        context: *context,
                        purpose,
                        profile,
                        through: *through,
                        requested_millis: at,
                        attempts: 0,
                        usage: Usage::default(),
                        phase: RequestPhase::Requested,
                        phased: record.sequence,
                    },
                );
                if let Some(previous) = self.latest.insert(agent.clone(), request) {
                    folded.superseded = self.requests.remove_entry(&previous);
                }
                named = Some(request);
            }
            SessionEvent::ModelAttemptStarted(attempt) => {
                if let Some(request) = self.requests.get_mut(&attempt.request) {
                    named = Some(attempt.request);
                    request.attempts += 1;
                    request.phase = RequestPhase::Open {
                        attempt: attempt.attempt,
                        message: None,
                        usage: None,
                    };
                    request.phased = record.sequence;
                }
            }
            SessionEvent::MessageCommitted {
                message: Message::Assistant(items),
            } => {
                self.awaiting.remove(agent);
                for call in items.iter().filter_map(|item| item.call()) {
                    let key = (agent.clone(), call.id().to_owned());
                    let made = (record.sequence.message(), call.name().to_owned());
                    self.unanswered.insert(key, made);
                }
                if let Some(&request) = self.latest.get(agent)
                    && let Some(open) = self.requests.get_mut(&request)
                    && let RequestPhase::Open { message, .. } = &mut open.phase
                {
                    *message = Some(record.sequence.message());
                    folded.fact = Some(Fact::Answers(request));
                    named = Some(request);
                }
            }
            SessionEvent::ModelFailed { attempt, failure } => {
                let (attempt, request) = (attempt.attempt, attempt.request);
                named = self.settle(request, by, |_| match failure {
                    AttemptFailure::Final(failure) => RequestPhase::Failed {
                        attempt: Some(attempt),
                        failure: failure.clone().into(),
                        at,
                    },
                    AttemptFailure::Retrying {
                        message,
                        kind,
                        delay,
                    } => RequestPhase::Retrying {
                        attempt,
                        failure: kind.error(message.clone()),
                        due: at.saturating_add_unsigned(delay.millis()),
                    },
                });
                if matches!(failure, AttemptFailure::Final(_)) {
                    self.failure = Some((by, request));
                }
            }
            SessionEvent::ModelAttemptInterrupted(attempt) => {
                named = self.settle(attempt.request, by, |_| RequestPhase::Interrupted {
                    attempt: Some(attempt.attempt),
                    at,
                });
            }
            // The response's commit, in the same transaction, attributed its message.
            SessionEvent::ResponseCompleted { attempt, .. } => {
                named = self.complete(*attempt, record);
            }
            SessionEvent::Compaction { checkpoint } => {
                named = self.complete(checkpoint.attempt, record);
            }
            SessionEvent::CompactionFailed { failure, error } => {
                let (request, attempt) = match failure {
                    CompactionFailure::BeforeRequest => return folded,
                    CompactionFailure::Requested(request) => (*request, None),
                    CompactionFailure::Attempted(attempt) => {
                        (attempt.request, Some(attempt.attempt))
                    }
                };
                if *error != CompactionFault::Interrupted {
                    self.failure = Some((by, request));
                }
                // What a summary attempt settled itself, such as its model failure, stands.
                if self.open(agent) == Some(request) {
                    named = self.settle(request, by, |_| match error {
                        CompactionFault::Interrupted => RequestPhase::Interrupted { attempt, at },
                        error => RequestPhase::Failed {
                            attempt,
                            failure: error.clone().into(),
                            at,
                        },
                    });
                }
            }
            SessionEvent::Usage { request, usage } => {
                if let Some(record) = self.requests.get_mut(request) {
                    record.usage.accumulate(*usage);
                    if let RequestPhase::Open {
                        usage: reported, ..
                    } = &mut record.phase
                    {
                        reported.get_or_insert_default().accumulate(*usage);
                    }
                    named = Some(*request);
                }
            }
            SessionEvent::MessageCommitted {
                message: Message::Tool(result),
            } => {
                let key = (agent.clone(), result.call_id.clone());
                if let Some((message, _)) = self.unanswered.shift_remove(&key) {
                    folded.fact = Some(Fact::Answered(message));
                }
                self.awaiting.insert(agent.clone());
            }
            SessionEvent::MessageCommitted {
                message: Message::User(_) | Message::State(_),
            } => {
                self.awaiting.insert(agent.clone());
            }
            SessionEvent::AgentFailed { failure } => {
                self.awaiting.remove(agent);
                let ended = failed.filter(|(failed, request)| {
                    failed.next() == by && self.ended_on(agent, *request, failure)
                });
                folded.fact = ended.map(|_| Fact::EndedOn);
            }
            SessionEvent::AgentInterrupted => {
                self.awaiting.remove(agent);
                interrupted = self.interrupt(agent, record);
            }
            _ => {}
        }
        folded.changed = [interrupted, named];
        folded
    }

    /// Move `request` to `phase` as the record `by` does. Returns the request when the
    /// ledger has it.
    fn settle(
        &mut self,
        request: RequestSeq,
        by: RecordSeq,
        phase: impl FnOnce(&RequestPhase) -> RequestPhase,
    ) -> Option<RequestSeq> {
        let record = self.requests.get_mut(&request)?;
        record.phase = phase(&record.phase);
        record.phased = by;
        Some(request)
    }

    /// Complete `attempt` with the usage it reported, as `by` does.
    fn complete(&mut self, attempt: AttemptRef, by: &EventRecord) -> Option<RequestSeq> {
        self.settle(attempt.request, by.sequence, |phase| {
            RequestPhase::Completed {
                attempt: attempt.attempt,
                at: by.timestamp_millis,
                usage: match phase {
                    RequestPhase::Open { usage, .. } => *usage,
                    _ => None,
                },
            }
        })
    }

    /// Close the agent's pending request, if any, as interrupted by `by`, returning it.
    fn interrupt(&mut self, agent: &AgentId, by: &EventRecord) -> Option<RequestSeq> {
        let request = self.open(agent)?;
        self.settle(request, by.sequence, |phase| RequestPhase::Interrupted {
            attempt: phase.last_attempt(),
            at: by.timestamp_millis,
        })
    }

    /// Whether `agent`'s turn, failing with `failure` right behind the record that
    /// failed `request`, ended on that request's failure. A turn ends on a request's
    /// failure only in its record's commit, and only as that failure: one a turn
    /// recovered from can sit right ahead of the end of another, such as a refusal at
    /// the boundary after it.
    fn ended_on(&self, agent: &AgentId, request: RequestSeq, failure: &Failure) -> bool {
        self.requests.get(&request).is_some_and(|request| {
            &request.agent == agent
                && match &request.phase {
                    RequestPhase::Failed {
                        failure: RequestFailure::Model(ended),
                        ..
                    } => ended == failure,
                    RequestPhase::Failed { .. } => true,
                    _ => false,
                }
        })
    }

    /// Read every pending request as interrupted at `at`, as a session whose
    /// work stopped leaves it, returning those it changed.
    pub(crate) fn stop(&mut self, at: i64) -> Vec<RequestSeq> {
        let mut stopped = Vec::new();
        for (request, record) in &mut self.requests {
            if record.phase.settled_at().is_none() {
                let attempt = record.phase.last_attempt();
                record.phase = RequestPhase::Interrupted { attempt, at };
                stopped.push(*request);
            }
        }
        stopped
    }

    /// `request`, while it is its agent's newest.
    pub fn get(&self, request: RequestSeq) -> Option<&RequestRecord> {
        self.requests.get(&request)
    }

    /// Each agent's newest request, in sequence order.
    pub fn iter(&self) -> impl Iterator<Item = (RequestSeq, &RequestRecord)> {
        self.requests.iter().map(|(seq, record)| (*seq, record))
    }

    /// The agent's newest request, whatever its phase.
    pub fn latest(&self, agent: &AgentId) -> Option<RequestSeq> {
        self.latest.get(agent).copied()
    }

    /// The agent's newest request while it is still pending.
    pub fn open(&self, agent: &AgentId) -> Option<RequestSeq> {
        self.latest(agent)
            .filter(|request| self.requests[request].phase.settled_at().is_none())
    }

    /// The agent's newest model context of `purpose`.
    pub(crate) fn context(&self, agent: &AgentId, purpose: ModelPurpose) -> Option<RecordSeq> {
        let contexts = self.contexts.get(agent)?;
        contexts.get(&purpose).map(|(context, _)| *context)
    }

    /// Whether the agent's newest history is input its model has yet to answer.
    pub(crate) fn awaits(&self, agent: &AgentId) -> bool {
        self.awaiting.contains(agent)
    }

    /// Calls committed responses made that no result answers yet, with their
    /// tool names, in commit order.
    pub(crate) fn unanswered(&self) -> impl Iterator<Item = (&AgentId, &str, &str)> {
        let calls = self.unanswered.iter();
        calls.map(|((agent, call), (_, name))| (agent, call.as_str(), name.as_str()))
    }

    /// The agents whose turn the journal leaves open: a request without its
    /// outcome, a call without its result, or input not yet answered.
    pub(crate) fn open_turns(&self) -> BTreeSet<&AgentId> {
        let requesting = (self.latest.keys()).filter(|agent| self.open(agent).is_some());
        let calling = self.unanswered.keys().map(|(agent, _)| agent);
        requesting.chain(calling).chain(&self.awaiting).collect()
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
            agent_state::AgentStates,
            tests::{self, usage},
        },
    };

    /// Appends records for one agent, stamped `at(sequence)`, keeping every request
    /// as the rows do and what the fold decided of each record.
    struct Journal {
        agent: AgentId,
        ledger: ResidentLedger,
        states: AgentStates,
        requests: BTreeMap<RequestSeq, RequestRecord>,
        facts: HashMap<RecordSeq, Fact>,
        /// What the last record changed.
        changed: Vec<RequestSeq>,
        next: u64,
    }

    impl Journal {
        fn new() -> Self {
            Self {
                agent: AgentId::root(SessionId::from_bytes([7; 16])),
                ledger: ResidentLedger::default(),
                states: AgentStates::default(),
                requests: BTreeMap::new(),
                facts: HashMap::new(),
                changed: Vec::new(),
                next: 1,
            }
        }

        fn record(&mut self, event: SessionEvent) -> RecordSeq {
            let sequence = RecordSeq::from(self.next);
            self.next += 1;
            let record = EventRecord {
                id: EventId::generate().unwrap(),
                sequence,
                timestamp_millis: at(sequence),
                agent: self.agent.clone(),
                event,
            };
            let fact = self.states.apply(&record, &mut self.ledger);
            self.facts.extend(fact.map(|fact| (sequence, fact)));
            let changed = self.states.take_unwritten().requests;
            self.changed = changed.keys().copied().collect();
            self.requests.extend(changed);
            sequence
        }

        fn changed(&self) -> Vec<RequestSeq> {
            self.changed.clone()
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
                failure: AttemptFailure::Final(failure("boom".into())),
            })
        }

        /// A transport failure of `attempt`, retried after `delay_millis`.
        fn retried(&mut self, request: RequestSeq, attempt: u64, delay_millis: u64) {
            self.record(SessionEvent::ModelFailed {
                attempt: attempt_ref(request, attempt),
                failure: AttemptFailure::Retrying {
                    message: "boom".into(),
                    kind: ProviderErrorKind::Transport,
                    delay: std::time::Duration::from_millis(delay_millis).into(),
                },
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
            &self.requests[&request]
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
            match self.facts.get(&message.into())? {
                Fact::Answers(request) => Some(*request),
                _ => None,
            }
        }
    }

    fn at(sequence: impl Into<RecordSeq>) -> i64 {
        sequence.into().get() as i64 * 1000
    }

    fn attempt_ref(request: RequestSeq, attempt: u64) -> AttemptRef {
        AttemptRef { request, attempt }
    }

    #[test]
    fn a_request_retries_through_its_journaled_failure_and_completes() {
        let mut journal = Journal::new();
        let request = journal.request(ModelPurpose::Agent);
        assert_eq!(journal.phase(request), &RequestPhase::Requested);
        assert_eq!(journal.open(), Some(request));
        assert_eq!(journal.record_of(request).requested_millis, at(request));

        journal.attempt(request, 1);
        journal.usage(request, usage(5, 0, 0));
        journal.retried(request, 1, 250);
        assert_eq!(journal.changed(), [request]);
        let retrying = RequestPhase::Retrying {
            attempt: 1,
            failure: ProviderErrorKind::Transport.error("boom"),
            due: journal.now() + 250,
        };
        assert_eq!(journal.phase(request), &retrying);
        assert_eq!(journal.open(), Some(request));

        journal.attempt(request, 2);
        let open = |message, usage| RequestPhase::Open {
            attempt: 2,
            message,
            usage,
        };
        assert_eq!(journal.phase(request), &open(None, None));
        journal.usage(request, usage(10, 2, 3));
        journal.usage(request, usage(1, 1, 1));
        let message = journal.reply();
        // Only the attempt that answers reports for its response.
        let answered = Some(usage(11, 3, 4));
        assert_eq!(journal.phase(request), &open(Some(message), answered));
        assert_eq!(journal.request_of(message), Some(request));
        let completed = journal.record(SessionEvent::ResponseCompleted {
            attempt: attempt_ref(request, 2),
            message,
            outcome: CompletedOutcome::Answer,
        });
        let record = journal.record_of(request);
        let at = at(completed);
        let completed = RequestPhase::Completed {
            attempt: 2,
            at,
            usage: answered,
        };
        assert_eq!(record.phase, completed);
        assert_eq!((record.attempts, record.usage), (2, usage(16, 3, 4)));
        assert_eq!(journal.open(), None);
        assert_eq!(journal.ledger.latest(&journal.agent), Some(request));
    }

    #[test]
    fn failures_settle_by_kind() {
        let mut journal = Journal::new();
        let refused = journal.attempted(ModelPurpose::Agent);
        journal.failed(refused, 1, Failure::Refused);
        let declined = RequestPhase::Failed {
            attempt: Some(1),
            failure: Failure::Refused("boom".into()).into(),
            at: journal.now(),
        };
        assert_eq!(journal.phase(refused), &declined);

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
            at: journal.now(),
        };
        assert_eq!(journal.phase(unattempted), &failed);
    }

    /// A turn ends on its request's failure only as that failure: another one
    /// right behind a failure the turn recovered from, such as a refusal at
    /// the boundary after an overflow, is not that request's.
    #[test]
    fn a_turn_ends_on_a_request_failure_only_as_that_failure() {
        let mut journal = Journal::new();
        let failed = |failure: &str| SessionEvent::AgentFailed {
            failure: Failure::Other(failure.into()),
        };
        let recovered = journal.attempted(ModelPurpose::Agent);
        journal.failed(recovered, 1, Failure::Other);
        let refused = journal.record(failed("refused"));
        assert_eq!(journal.facts.get(&refused), None);
        let ending = journal.attempted(ModelPurpose::Agent);
        journal.failed(ending, 1, Failure::Other);
        let ended = journal.record(failed("boom"));
        assert_eq!(journal.facts[&ended], Fact::EndedOn);
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
        assert_eq!(journal.changed(), [requested]);
        // A settled request is left alone by a later interruption.
        journal.record(SessionEvent::AgentInterrupted);
        assert_eq!(journal.phase(requested), &unattempted);
        assert_eq!(journal.changed(), []);

        let after_attempt = |journal: &Journal| RequestPhase::Interrupted {
            attempt: Some(1),
            at: journal.now(),
        };
        let retrying = journal.attempted(ModelPurpose::Agent);
        journal.retried(retrying, 1, 5);
        journal.record(SessionEvent::AgentInterrupted);
        assert_eq!(journal.phase(retrying), &after_attempt(&journal));

        let cut = journal.attempted(ModelPurpose::Agent);
        journal.record(SessionEvent::ModelAttemptInterrupted(attempt_ref(cut, 1)));
        assert_eq!(journal.phase(cut), &after_attempt(&journal));

        // A newer request supersedes whatever the previous one left open, which the
        // ledger then lets go of.
        let open = journal.attempted(ModelPurpose::Agent);
        let next = journal.request(ModelPurpose::Agent);
        assert_eq!(journal.changed(), [open, next]);
        assert!(journal.ledger.get(open).is_none());
        assert_eq!(journal.phase(open), &after_attempt(&journal));
        assert_eq!(journal.phase(next).settled_at(), None);
        assert_eq!(journal.open(), Some(next));
    }

    /// A result answers its agent's unanswered call of that id, once.
    #[test]
    fn results_answer_the_calls_they_name() {
        use crate::provider::protocol::{ToolCall, ToolResult};
        let mut journal = Journal::new();
        let call = |id: &str, index| {
            let call = ToolCall::new(id, "exec", serde_json::json!({})).unwrap();
            AssistantItem::tool_call(id, index, call)
        };
        let made = journal.record(SessionEvent::MessageCommitted {
            message: Message::Assistant(vec![call("a", 0), call("b", 1)]),
        });
        let result = |id: &str| SessionEvent::MessageCommitted {
            message: Message::Tool(ToolResult {
                call_id: id.into(),
                name: "exec".into(),
                result: serde_json::Value::Null,
                images: Vec::new(),
                is_error: false,
            }),
        };
        let answer = journal.record(result("b"));
        let stray = journal.record(result("b"));
        assert_eq!(journal.facts[&answer], Fact::Answered(made.message()));
        assert_eq!(journal.facts.get(&stray), None);
        let unanswered: Vec<_> = journal.ledger.unanswered().collect();
        assert_eq!(unanswered, [(&journal.agent, "a", "exec")]);
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
                trigger: crate::session::CompactionTrigger::Occupancy(100),
            },
        });
        let completed = RequestPhase::Completed {
            attempt: 1,
            at: journal.now(),
            usage: None,
        };
        assert_eq!(journal.phase(checkpointed), &completed);
        let record = journal.record_of(checkpointed);
        assert_eq!(record.purpose, ModelPurpose::Compaction);
    }
}
