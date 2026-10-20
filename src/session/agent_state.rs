//! What the journal leaves of each started agent beside its requests: the records its
//! profile, grants and todos come from, where its last turn stands, how its work last
//! ended, the occupancy it reported and its usage, and what the fold decided of its
//! answers and failed turns. The core folds it as records commit and writes each
//! agent's row in their transaction; hosts fold [`Turns`] alone.
use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::{
    agent::{Failure, TurnFailure},
    identity::AgentId,
    provider::protocol::{Usage, UsageTotals},
    session::{
        CompletedOutcome, EventRecord, Fact, ModelPurpose, RecordSeq, RequestPhase, RequestRecord,
        RequestSeq, ResidentLedger, SessionEvent, ledger::Folded,
    },
    tool::policy::CapabilitySet,
};

/// Where an agent's journal leaves its last turn: its newest message awaits the model
/// or answers it, a failure since stopped it, or one was reported with nothing open to
/// continue.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Last {
    Awaiting,
    Answered,
    Failed(Failure),
    Refused(Failure),
}

/// How an agent's work last ended since its latest request: an idle root that was
/// interrupted completed.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Outcome {
    Completed,
    Interrupted,
    Failed(Failure),
}

/// A fact, and the record that left it with when it was journaled.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Marked<T> {
    pub(crate) value: T,
    pub(crate) entry: RecordSeq,
    pub(crate) millis: i64,
}

impl<T> Marked<T> {
    fn at(record: &EventRecord, value: T) -> Self {
        Self {
            value,
            entry: record.sequence,
            millis: record.timestamp_millis,
        }
    }
}

/// One started agent as its records leave it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AgentState {
    /// The start or model change whose profile it runs.
    pub(crate) profiled: RecordSeq,
    /// The start or mode change whose grants it holds, and those grants.
    pub(crate) granted: RecordSeq,
    pub(crate) capabilities: CapabilitySet,
    pub(crate) available_depth: usize,
    /// The replacement or checkpoint that set its todos, and its latest checkpoint.
    pub(crate) todos: Option<RecordSeq>,
    pub(crate) checkpoint: Option<RecordSeq>,
    /// What its last completed response reported, which its next request compacts
    /// for, while no checkpoint answered it and its model and mode are unchanged since.
    pub(crate) reported: Option<(RequestSeq, Usage)>,
    /// The completed request whose occupancy its context gauge shows.
    pub(crate) gauged: Option<RequestSeq>,
    pub(crate) last: Option<Marked<Last>>,
    pub(crate) outcome: Option<Marked<Outcome>>,
    /// The agent request that began its open turn.
    pub(crate) turn: Option<RequestSeq>,
    /// The latest record that ended a turn, whether or not one was open.
    pub(crate) ended: Option<RecordSeq>,
    /// Its latest finished turn: the request that began it and the record that ended it.
    pub(crate) finished: Option<(RequestSeq, RecordSeq)>,
    pub(crate) usage: UsageTotals,
}

impl AgentState {
    fn started(at: RecordSeq, capabilities: CapabilitySet, available_depth: usize) -> Self {
        Self {
            profiled: at,
            granted: at,
            capabilities,
            available_depth,
            todos: None,
            checkpoint: None,
            reported: None,
            gauged: None,
            last: None,
            outcome: None,
            turn: None,
            ended: None,
            finished: None,
            usage: UsageTotals::default(),
        }
    }

    /// Why its journal leaves its last turn short of an answer, so resuming has
    /// something to continue: a failure since its newest message, or that message, or
    /// its newest request in `ledger`, still awaiting the model.
    pub(crate) fn stopped(&self, agent: &AgentId, ledger: &ResidentLedger) -> Option<TurnFailure> {
        let request = ledger.latest(agent).and_then(|request| ledger.get(request));
        let incomplete =
            request.is_some_and(|request| !matches!(request.phase, RequestPhase::Completed { .. }));
        stopped(self.last.as_ref().map(|last| &last.value), incomplete)
    }

    fn end(&mut self, at: RecordSeq) {
        self.ended = Some(at);
        if let Some(turn) = self.turn.take() {
            self.finished = Some((turn, at));
        }
    }
}

/// Why a turn whose journal leaves it at `last`, with its newest request `incomplete`,
/// stopped short of an answer, if it did.
pub(crate) fn stopped(last: Option<&Last>, incomplete: bool) -> Option<TurnFailure> {
    match last {
        Some(Last::Failed(failure)) => Some(TurnFailure::Failed(failure.clone())),
        Some(Last::Awaiting) => Some(TurnFailure::Interrupted),
        Some(Last::Answered | Last::Refused(_)) | None => {
            incomplete.then_some(TurnFailure::Interrupted)
        }
    }
}

/// Every started agent's [`AgentState`], with the rows its records and the ledger's
/// fold of them changed.
#[derive(Clone, Debug, Default)]
pub(crate) struct AgentStates {
    agents: HashMap<AgentId, AgentState>,
    /// The agents whose rows changed since they were last written: their state, their
    /// newest request, or whether they await their model.
    unwritten: BTreeSet<AgentId>,
    /// The requests changed since their rows were last written, as the records leave them.
    requests: BTreeMap<RequestSeq, RequestRecord>,
    /// The model contexts journaled since their rows were last written.
    contexts: Vec<(AgentId, ModelPurpose, RecordSeq)>,
    /// What the fold decided of the records since, for their rows.
    decided: Vec<Decided>,
}

/// What the fold decided of one record, which presenting it reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Decided {
    /// An answer, the request it committed under, and the request that began its turn.
    Answer {
        answer: RecordSeq,
        request: RequestSeq,
        turn: Option<RequestSeq>,
    },
    /// A failed turn, which its request's failure ended.
    Failed { turn: RecordSeq },
}

impl AgentStates {
    /// The states the rows hold.
    pub(crate) fn opened(agents: HashMap<AgentId, AgentState>) -> Self {
        Self {
            agents,
            ..Self::default()
        }
    }

    pub(crate) fn get(&self, agent: &AgentId) -> Option<&AgentState> {
        self.agents.get(agent)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&AgentId, &AgentState)> {
        self.agents.iter()
    }

    /// Fold `record` into `ledger`, then into the agents' states, keeping every row it
    /// changed; returns what the ledger decided of it.
    pub(crate) fn apply(
        &mut self,
        record: &EventRecord,
        ledger: &mut ResidentLedger,
    ) -> Option<Fact> {
        let Folded {
            changed,
            superseded,
            fact,
        } = ledger.fold(record);
        for request in changed.into_iter().flatten() {
            let changed = match &superseded {
                Some((replaced, left)) if *replaced == request => Some(left),
                _ => ledger.get(request),
            };
            if let Some(changed) = changed {
                self.requests.insert(request, changed.clone());
            }
        }
        self.fold(record, ledger, fact);
        fact
    }

    /// Fold `record`, after `ledger` has, which decided `fact` of it. Each record that
    /// moves the agent's newest request or whether it awaits its model marks its row.
    fn fold(&mut self, record: &EventRecord, ledger: &ResidentLedger, fact: Option<Fact>) {
        let (agent, at) = (&record.agent, record.sequence);
        let state = match &record.event {
            SessionEvent::AgentStarted {
                capabilities,
                available_depth,
                ..
            } => {
                let granted = capabilities.iter().copied().collect();
                let started = AgentState::started(at, granted, *available_depth as usize);
                self.agents.insert(agent.clone(), started);
                self.unwritten.insert(agent.clone());
                return;
            }
            SessionEvent::ModelContext { context } => {
                self.contexts.push((agent.clone(), context.purpose, at));
                return;
            }
            _ => match self.agents.get_mut(agent) {
                Some(state) => state,
                None => return,
            },
        };
        match &record.event {
            SessionEvent::ModelChanged { .. } => {
                state.profiled = at;
                state.reported = None;
            }
            SessionEvent::ModeChanged { capabilities, .. } => {
                state.granted = at;
                state.capabilities = capabilities.iter().copied().collect();
                state.reported = None;
            }
            SessionEvent::TodosReplaced { .. } => state.todos = Some(at),
            SessionEvent::Compaction { .. } => {
                (state.todos, state.checkpoint) = (Some(at), Some(at));
                (state.reported, state.gauged) = (None, None);
            }
            // A summary round a fresh one could pass leaves the need.
            SessionEvent::CompactionFailed { error, .. } if error.keeps_need() => return,
            SessionEvent::CompactionFailed { .. } => state.reported = None,
            SessionEvent::ModelRequested { .. } => {
                state.outcome = None;
                if let Some(TurnStep::Begins) = turn_step(record, ledger) {
                    state.turn.get_or_insert(at.request());
                }
            }
            SessionEvent::ResponseCompleted { attempt, .. } => {
                let completed = ledger.get(attempt.request);
                if let Some(usage) = completed.and_then(|request| request.phase.completed_usage()) {
                    state.reported = Some((attempt.request, usage));
                }
                if let Some(Some((request, ..))) = gauged(ledger, record) {
                    state.gauged = Some(request);
                }
                if let Some(TurnStep::Ends) = turn_step(record, ledger) {
                    state.end(at);
                }
            }
            SessionEvent::MessageCommitted { message } => {
                if let Some(Fact::Answers(request)) = fact {
                    let (answer, turn) = (at, state.turn);
                    (self.decided).push(Decided::Answer {
                        answer,
                        request,
                        turn,
                    });
                }
                let last = match message.answers() {
                    true => Last::Answered,
                    false => Last::Awaiting,
                };
                state.last = Some(Marked::at(record, last));
            }
            // Only a failure that ends an open turn stops it: one with nothing left to
            // continue is reported, and the agent rests.
            SessionEvent::AgentFailed { failure } => {
                if let Some(Fact::EndedOn) = fact {
                    self.decided.push(Decided::Failed { turn: at });
                }
                let last = match state.stopped(agent, ledger) {
                    Some(_) => Last::Failed(failure.clone()),
                    None => Last::Refused(failure.clone()),
                };
                state.last = Some(Marked::at(record, last));
                state.outcome = Some(Marked::at(record, Outcome::Failed(failure.clone())));
                state.end(at);
            }
            SessionEvent::AgentInterrupted => {
                if state.outcome.is_none() {
                    let idle = agent.parent().is_none() && state.turn.is_none();
                    let outcome = match idle {
                        true => Outcome::Completed,
                        false => Outcome::Interrupted,
                    };
                    state.outcome = Some(Marked::at(record, outcome));
                }
                state.end(at);
            }
            SessionEvent::AgentCompleted => {
                state.outcome = Some(Marked::at(record, Outcome::Completed));
                state.end(at);
            }
            SessionEvent::Usage { usage, .. } => state.usage.add(*usage),
            _ => return,
        }
        self.unwritten.insert(agent.clone());
    }

    /// The rows changed since the last take: the requests, the started agents as they
    /// are now, the model contexts journaled since and what the fold decided.
    pub(crate) fn take_unwritten(&mut self) -> Rows<'_> {
        let agents = std::mem::take(&mut self.unwritten).into_iter();
        Rows {
            requests: std::mem::take(&mut self.requests),
            agents: agents
                .filter_map(|agent| self.agents.get_key_value(&agent))
                .collect(),
            contexts: std::mem::take(&mut self.contexts),
            decided: std::mem::take(&mut self.decided),
        }
    }

    /// Whether every change has been taken for its row.
    pub(crate) fn written(&self) -> bool {
        (self.unwritten.is_empty() && self.requests.is_empty())
            && (self.contexts.is_empty() && self.decided.is_empty())
    }
}

/// The rows a transaction's records changed.
pub(crate) struct Rows<'a> {
    pub(crate) requests: BTreeMap<RequestSeq, RequestRecord>,
    pub(crate) agents: Vec<(&'a AgentId, &'a AgentState)>,
    pub(crate) contexts: Vec<(AgentId, ModelPurpose, RecordSeq)>,
    pub(crate) decided: Vec<Decided>,
}

/// How a record moves its agent's turn.
enum TurnStep {
    Begins,
    Ends,
}

/// How `record`, which `ledger` has folded, moves its agent's turn: the first agent
/// request after the last turn ended begins one, and a response that calls no tools
/// or the agent stopping ends it.
fn turn_step(record: &EventRecord, ledger: &ResidentLedger) -> Option<TurnStep> {
    match &record.event {
        SessionEvent::ModelRequested { .. } => ledger
            .get(record.sequence.request())
            .filter(|request| request.purpose == ModelPurpose::Agent)
            .map(|_| TurnStep::Begins),
        SessionEvent::ResponseCompleted { outcome, .. }
            if *outcome != CompletedOutcome::ToolUse =>
        {
            Some(TurnStep::Ends)
        }
        SessionEvent::AgentCompleted
        | SessionEvent::AgentInterrupted
        | SessionEvent::AgentFailed { .. } => Some(TurnStep::Ends),
        _ => None,
    }
}

/// What `record`, which `ledger` has folded, makes its agent's context gauge show:
/// the completed request whose response reported its occupancy, with that occupancy,
/// or nothing once a checkpoint replaced the history it measured.
pub(crate) fn gauged<'a>(
    ledger: &'a ResidentLedger,
    record: &EventRecord,
) -> Option<Option<(RequestSeq, &'a crate::session::RequestRecord, u64)>> {
    match &record.event {
        SessionEvent::Compaction { .. } => Some(None),
        // Only an agent's own requests complete responses; a summary installs a checkpoint.
        SessionEvent::ResponseCompleted { attempt, .. } => {
            let request = ledger.get(attempt.request)?;
            let tokens = request.phase.completed_usage()?.occupancy()?;
            Some(Some((attempt.request, request, tokens)))
        }
        _ => None,
    }
}

/// Agents inside a turn, with when it began (epoch milliseconds): from the first
/// model request after the last turn ended, until a response ends it without
/// calling tools or the agent stops.
#[derive(Clone, Debug, Default)]
pub struct Turns(HashMap<AgentId, i64>);

impl Turns {
    /// Fold one record after `ledger` has, returning when the turn it ended began.
    pub fn observe(&mut self, record: &EventRecord, ledger: &ResidentLedger) -> Option<i64> {
        match turn_step(record, ledger)? {
            TurnStep::Begins => {
                (self.0)
                    .entry(record.agent.clone())
                    .or_insert(record.timestamp_millis);
                None
            }
            TurnStep::Ends => self.0.remove(&record.agent),
        }
    }

    /// When the agent's open turn began.
    pub fn started(&self, agent: &AgentId) -> Option<i64> {
        self.0.get(agent).copied()
    }
}

/// Agents inside a turn, with when it began.
impl FromIterator<(AgentId, i64)> for Turns {
    fn from_iter<I: IntoIterator<Item = (AgentId, i64)>>(turns: I) -> Self {
        Self(turns.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent::CompactionFault,
        identity::SessionId,
        session::{
            AttemptRef, CompactionFailure, ModelContext,
            tests::{self, attempt, profile, record, requested},
        },
    };
    use std::sync::Arc;

    /// One agent's records folded into a ledger, its turns and its state, each
    /// stamped with its sequence.
    struct Fold {
        agent: AgentId,
        ledger: ResidentLedger,
        turns: Turns,
        states: AgentStates,
        next: u64,
    }

    impl Fold {
        fn new() -> Self {
            let agent = AgentId::root(SessionId::from_bytes([3; 16]));
            let mut fold = Self {
                agent,
                ledger: ResidentLedger::default(),
                turns: Turns::default(),
                states: AgentStates::default(),
                next: 1,
            };
            fold.observe(tests::agent_started(std::path::Path::new("/")));
            fold
        }

        /// Fold `event`, returning its sequence and when the turn it ended began.
        fn observe(&mut self, event: SessionEvent) -> (RecordSeq, Option<i64>) {
            let mut record = record(&self.agent, self.next, event);
            Arc::make_mut(&mut record).timestamp_millis = self.next as i64;
            self.next += 1;
            self.states.apply(&record, &mut self.ledger);
            let ended = self.turns.observe(&record, &self.ledger);
            (record.sequence, ended)
        }

        /// A request for `purpose`, and when it was made.
        fn request(&mut self, purpose: ModelPurpose) -> (RequestSeq, i64) {
            let context = ModelContext::test(purpose, profile());
            let (context, _) = self.observe(SessionEvent::ModelContext { context });
            let (request, _) = self.observe(requested(context));
            (request.request(), request.get() as i64)
        }

        fn complete(&mut self, request: RequestSeq, outcome: CompletedOutcome) -> Option<i64> {
            let completed = SessionEvent::ResponseCompleted {
                attempt: AttemptRef {
                    request,
                    attempt: 1,
                },
                message: RecordSeq::from(self.next).message(),
                outcome,
            };
            self.observe(completed).1
        }

        fn state(&self) -> &AgentState {
            self.states.get(&self.agent).unwrap()
        }
    }

    /// A turn starts at the first agent request after the last one ended, runs
    /// through tool use, and ends with an answer or when the agent stops.
    #[test]
    fn turns_run_from_an_agent_request_until_an_answer_or_a_stop() {
        let mut fold = Fold::new();
        fold.request(ModelPurpose::Compaction);
        assert_eq!(fold.turns.started(&fold.agent), None);
        let (first, start) = fold.request(ModelPurpose::Agent);
        assert_eq!(fold.complete(first, CompletedOutcome::ToolUse), None);
        let (second, _) = fold.request(ModelPurpose::Agent);
        assert_eq!(fold.turns.started(&fold.agent), Some(start));
        let answer = RecordSeq::from(fold.next);
        assert_eq!(fold.complete(second, CompletedOutcome::Answer), Some(start));
        assert_eq!(fold.turns.started(&fold.agent), None);
        // Stopping with no turn open, as an idle root does at shutdown, finishes none.
        fold.observe(SessionEvent::AgentInterrupted);
        assert_eq!(fold.state().finished, Some((first, answer)));
        let failed = SessionEvent::AgentFailed {
            failure: Failure::Other("boom".into()),
        };
        for stop in [SessionEvent::AgentInterrupted, failed] {
            let (_, start) = fold.request(ModelPurpose::Agent);
            assert_eq!(fold.observe(stop).1, Some(start));
            assert_eq!(fold.turns.started(&fold.agent), None);
        }
    }

    /// A committed response over the threshold leaves the need through failed summary
    /// rounds a fresh summary could pass, but not through an overflowing one: that
    /// round already shrank its summary, so the next request decides.
    #[test]
    fn only_a_summary_failure_a_retry_could_pass_keeps_the_need() {
        let mut fold = Fold::new();
        let (request, _) = fold.request(ModelPurpose::Agent);
        fold.observe(attempt(request, 1));
        fold.observe(SessionEvent::Usage {
            request,
            usage: tests::usage(1_000, 0, 10),
        });
        fold.complete(request, CompletedOutcome::Answer);
        let failed = |error| SessionEvent::CompactionFailed {
            failure: CompactionFailure::BeforeRequest,
            error,
        };
        for (error, keeps) in [
            (CompactionFault::Interrupted, true),
            (CompactionFault::Truncated, true),
            (CompactionFault::Overflowed, false),
        ] {
            fold.observe(failed(error.clone()));
            assert_eq!(fold.state().reported.is_some(), keeps, "{error:?}");
        }
    }
}
