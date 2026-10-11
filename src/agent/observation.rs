//! Host-only observation. A snapshot and its receiver share an atomic revision boundary.
use super::{RuntimeEvent, TurnFailure};
use crate::{
    identity::{AgentId, JobId},
    job::{JobEnvelope, JobState},
    provider::protocol::{LiveBlock, LiveResponse, Step},
    session::{
        CommittedUsage, EventRecord, MessageSeq, RecordSeq, RequestLedger, RequestSeq,
        SessionEvent, SessionPhase,
    },
};
use std::collections::{BTreeMap, HashMap};
use tokio::sync::broadcast;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentActivity {
    Idle,
    Working,
    Reconnecting {
        attempt: u64,
    },
    Tools,
    WaitingChildren,
    Compacting,
    /// The turn ended without an answer and can be continued.
    Stopped(TurnFailure),
}

impl AgentActivity {
    /// Whether the agent is mid-turn.
    #[must_use]
    pub fn is_busy(&self) -> bool {
        !matches!(self, Self::Idle | Self::Stopped(_))
    }

    /// Whether the last turn can be continued.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Stopped(_))
    }
}

/// An agent's activity and when it last started or stopped being busy (epoch
/// milliseconds): its turn start while busy, its turn end otherwise.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservedActivity {
    pub state: AgentActivity,
    pub since: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextUsage {
    pub tokens: u64,
    pub capacity: u64,
}

/// Each agent's context as its last committed response reported it, at the capacity of
/// that request's model, until a checkpoint replaces the agent's history: a fold of the
/// journal.
#[derive(Clone, Debug, Default)]
pub struct ContextGauge {
    committed: CommittedUsage,
    usage: HashMap<AgentId, ContextUsage>,
}

impl ContextGauge {
    #[must_use]
    pub fn get(&self, agent: &AgentId) -> Option<&ContextUsage> {
        self.usage.get(agent)
    }

    /// Fold `record`, which `ledger` has already observed.
    fn fold(&mut self, ledger: &RequestLedger, record: &EventRecord) {
        let agent = &record.agent;
        if matches!(record.event, SessionEvent::Compaction { .. }) {
            self.usage.remove(agent);
        }
        let Some((request, usage)) = self.committed.fold(record) else {
            return;
        };
        // Only an agent's own requests complete responses; a summary installs a checkpoint.
        if let (Some(requested), Some(tokens)) = (ledger.get(request), usage.occupancy()) {
            let capacity = requested.profile.profile.max_context.get();
            self.usage
                .insert(agent.clone(), ContextUsage { tokens, capacity });
        }
    }
}

/// How a request's response settled, keyed by the journal sequence it produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Settlement {
    /// The assistant message committed at this sequence.
    Committed(MessageSeq),
    /// Nothing committed.
    Failed(TurnFailure),
}

/// One request's response as observers see it: provisional blocks while it streams,
/// the final provisional view once it has ended, then how it settled until its
/// commit replaces it.
#[derive(Clone, Debug)]
pub enum ObservedResponse {
    Streaming(LiveResponse),
    Ended(Vec<LiveBlock>),
    Settled {
        blocks: Vec<LiveBlock>,
        how: Settlement,
    },
}

impl Default for ObservedResponse {
    fn default() -> Self {
        Self::Streaming(LiveResponse::default())
    }
}

impl ObservedResponse {
    /// Blocks in arrival order.
    #[must_use]
    pub fn blocks(&self) -> &[LiveBlock] {
        match self {
            Self::Streaming(live) => live.blocks(),
            Self::Ended(blocks) | Self::Settled { blocks, .. } => blocks,
        }
    }

    /// Whether `block` is the one still being streamed.
    #[must_use]
    pub fn streaming(&self, block: &LiveBlock) -> bool {
        matches!(self, Self::Streaming(live) if live.current() == Some(&block.block))
    }

    #[must_use]
    pub fn settlement(&self) -> Option<&Settlement> {
        match self {
            Self::Settled { how, .. } => Some(how),
            Self::Streaming(_) | Self::Ended(_) => None,
        }
    }

    /// Whether the response settled without becoming a complete answer.
    #[must_use]
    pub fn incomplete(&self) -> bool {
        matches!(self.settlement(), Some(Settlement::Failed(_)))
    }

    fn settle(&mut self, how: Settlement) {
        let blocks = match std::mem::take(self) {
            Self::Streaming(live) => live.blocks().to_vec(),
            Self::Ended(blocks) | Self::Settled { blocks, .. } => blocks,
        };
        *self = Self::Settled { blocks, how };
    }
}

#[derive(Clone, Debug, Default)]
pub struct ObservationSnapshot {
    /// Where the session is in its life. Once its work stops, as a crash stops
    /// it, every turn, request, response and job it left going reads as
    /// interrupted at its last activity, as reopening settles them.
    pub phase: SessionPhase,
    pub revision: u64,
    pub records: BTreeMap<RecordSeq, EventRecord>,
    /// Every request's lifecycle, folded from `records`.
    pub ledger: RequestLedger,
    /// Every job, as the session's state holds it.
    pub jobs: BTreeMap<JobId, JobEnvelope>,
    pub responses: HashMap<(AgentId, RequestSeq), ObservedResponse>,
    pub activity: HashMap<AgentId, ObservedActivity>,
    pub context: ContextGauge,
}

pub(crate) type Responses = HashMap<(AgentId, RequestSeq), ObservedResponse>;

/// The jobs and requests an update read as interrupted because the session's
/// work stopped, which no update of their own reports.
#[derive(Debug, Default)]
pub struct StoppedReading {
    pub jobs: Vec<JobId>,
    pub requests: Vec<RequestSeq>,
}

impl ObservationSnapshot {
    /// Idempotent journal projection; live deltas are applied once by revision.
    pub fn apply(&mut self, update: ObservedEvent) -> StoppedReading {
        if update.revision <= self.revision {
            return StoppedReading::default();
        }
        self.revision = update.revision;
        match update.event {
            RuntimeEvent::Record(record) => {
                if self.records.contains_key(&record.sequence) {
                    return StoppedReading::default();
                }
                let views = Views {
                    ledger: &mut self.ledger,
                    responses: &mut self.responses,
                    context: &mut self.context,
                };
                views.record(&record);
                self.records.insert(record.sequence, *record);
            }
            RuntimeEvent::Job(job) => {
                self.jobs.insert(job.id, *job);
            }
            RuntimeEvent::Phase(phase) => self.phase = phase,
            event => {
                let records = &self.records;
                let committed = |sequence: RecordSeq| records.contains_key(&sequence);
                live(
                    &mut self.responses,
                    Some(&mut self.activity),
                    committed,
                    event,
                );
            }
        }
        self.read_phase()
    }

    /// Read what a session whose work stopped left going as interrupted at its
    /// last activity.
    pub(crate) fn read_phase(&mut self) -> StoppedReading {
        let Some(at) = self.phase.stopped_at(self.records.values()) else {
            return StoppedReading::default();
        };
        for response in self.responses.values_mut() {
            if response.settlement().is_none() {
                response.settle(Settlement::Failed(TurnFailure::Interrupted));
            }
        }
        let jobs = self.jobs.values_mut().filter_map(|job| read_job(job, at));
        StoppedReading {
            jobs: jobs.collect(),
            requests: self.ledger.stop(at),
        }
    }
}

/// Read a job a stopped session left going as interrupted at its last activity,
/// `at`, returning it if that changed it.
pub(crate) fn read_job(job: &mut JobEnvelope, at: i64) -> Option<JobId> {
    if job.state.is_terminal() {
        return None;
    }
    job.state = JobState::Interrupted;
    job.timing = job.timing.after(JobState::Interrupted, at);
    Some(job.id)
}

/// What observers fold from each committed record beside the record itself.
pub(crate) struct Views<'a> {
    pub(crate) ledger: &'a mut RequestLedger,
    pub(crate) responses: &'a mut Responses,
    pub(crate) context: &'a mut ContextGauge,
}

impl Views<'_> {
    pub(crate) fn record(self, record: &EventRecord) {
        self.ledger.observe(record);
        self.context.fold(self.ledger, record);
        let agent = &record.agent;
        match &record.event {
            SessionEvent::ModelAttemptStarted(attempt) => {
                // One logical request survives retries; native item/block
                // IDs may be reused. Clear only the displayed response,
                // never the journal's per-attempt audit records.
                let key = (agent.clone(), attempt.request);
                self.responses.insert(key, ObservedResponse::default());
            }
            SessionEvent::ModelFailed { attempt, failure } => {
                // The runtime's own settlement is authoritative and may
                // arrive before or after this record.
                let response = self.responses.entry((agent.clone(), attempt.request));
                let response = response.or_default();
                if response.settlement().is_none() {
                    response.settle(Settlement::Failed(failure.failure().into()));
                }
            }
            SessionEvent::MessageCommitted { .. } => {
                self.responses.retain(|_, response| {
                    !matches!(
                        response.settlement(),
                        Some(Settlement::Committed(message))
                            if RecordSeq::from(*message) == record.sequence
                    )
                });
            }
            _ => {}
        }
    }
}

/// Fold a live update the journal does not carry; `committed` tells whether a
/// record is in the journal yet.
pub(crate) fn live(
    responses: &mut Responses,
    activities: Option<&mut HashMap<AgentId, ObservedActivity>>,
    committed: impl Fn(RecordSeq) -> bool,
    event: RuntimeEvent,
) {
    match event {
        RuntimeEvent::Record(_)
        | RuntimeEvent::Job(_)
        | RuntimeEvent::TurnCompleted { .. }
        | RuntimeEvent::Phase(_) => {}
        RuntimeEvent::ResponseEvent {
            agent,
            request,
            event,
        } => {
            let response = responses.entry((agent, request)).or_default();
            if let ObservedResponse::Streaming(live) = response {
                *response = match std::mem::take(live).push(event) {
                    Step::Open(live) => ObservedResponse::Streaming(live),
                    Step::Ended { blocks, .. } => ObservedResponse::Ended(blocks),
                };
            }
        }
        RuntimeEvent::ResponseSettled {
            agent,
            request,
            settlement,
        } => {
            let key = (agent, request);
            match settlement {
                Settlement::Committed(message) if committed(message.into()) => {
                    responses.remove(&key);
                }
                how => responses.entry(key).or_default().settle(how),
            }
        }
        RuntimeEvent::Activity { agent, activity } => {
            if let AgentActivity::Stopped(failure) = &activity.state {
                for ((owner, _), response) in responses.iter_mut() {
                    if owner == &agent && response.settlement().is_none() {
                        response.settle(Settlement::Failed(failure.clone()));
                    }
                }
            }
            activities.map(|activities| activities.insert(agent, activity));
        }
    }
}

#[derive(Clone, Debug)]
pub struct ObservedEvent {
    pub revision: u64,
    pub event: RuntimeEvent,
}

pub struct Observation {
    pub snapshot: ObservationSnapshot,
    pub updates: broadcast::Receiver<ObservedEvent>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Failure;
    use crate::session::{
        AttemptFailure, AttemptRef, Message, ModelContext, ModelPurpose,
        tests::{self, attempt, requested},
    };
    use crate::{
        identity::SessionId,
        provider::protocol::{BlockRef, ItemKind, ResponseEvent},
    };

    /// An observer fed updates one at a time.
    type Hub = std::cell::RefCell<ObservationSnapshot>;

    /// Apply `event` to `hub` as its next update.
    fn send(hub: &Hub, event: RuntimeEvent) -> ObservedEvent {
        let revision = hub.borrow().revision + 1;
        let update = ObservedEvent { revision, event };
        hub.borrow_mut().apply(update.clone());
        update
    }

    /// A fresh observer's fold of `records`.
    fn replayed<'a>(records: impl IntoIterator<Item = &'a EventRecord>) -> ObservationSnapshot {
        let hub = Hub::default();
        for record in records {
            send(&hub, RuntimeEvent::Record(Box::new(record.clone())));
        }
        hub.into_inner()
    }

    /// Journals `event` at `sequence` seconds past the epoch.
    fn record(hub: &Hub, agent: &AgentId, sequence: u64, event: SessionEvent) -> ObservedEvent {
        let mut record = tests::record(agent, sequence, event);
        record.timestamp_millis = sequence as i64 * 1000;
        send(hub, RuntimeEvent::Record(Box::new(record)))
    }

    /// Journals request `request` for `purpose`, its context just before it.
    fn request(hub: &Hub, agent: &AgentId, request: u64, purpose: ModelPurpose) {
        let context = ModelContext::test(purpose, tests::profile());
        record(
            hub,
            agent,
            request - 1,
            SessionEvent::ModelContext { context },
        );
        record(hub, agent, request, requested((request - 1).into()));
    }

    fn failed(request: u64, attempt: u64, error: &str) -> SessionEvent {
        SessionEvent::ModelFailed {
            attempt: AttemptRef {
                request: request.into(),
                attempt,
            },
            failure: AttemptFailure::Final(transport(error)),
        }
    }

    /// A failure the runtime retries a second later.
    fn retried(request: u64, attempt: u64) -> SessionEvent {
        SessionEvent::ModelFailed {
            attempt: AttemptRef {
                request: request.into(),
                attempt,
            },
            failure: AttemptFailure::Retrying {
                message: "connection lost".into(),
                kind: crate::provider::ProviderErrorKind::Transport,
                delay: std::time::Duration::from_secs(1).into(),
            },
        }
    }

    fn committed() -> SessionEvent {
        SessionEvent::MessageCommitted {
            message: Message::Assistant(vec![]),
        }
    }

    /// A retryable failure.
    fn transport(error: &str) -> Failure {
        Failure::Provider(error.into(), crate::provider::ProviderErrorKind::Transport)
    }

    fn lost() -> Settlement {
        Settlement::Failed(transport("connection lost").into())
    }

    /// Streams `text` into `request`'s text block.
    fn delta(hub: &Hub, agent: &AgentId, request: u64, text: &str) -> ObservedEvent {
        let block = BlockRef::single("text");
        let (kind, text) = (ItemKind::Text, text.into());
        send(
            hub,
            RuntimeEvent::ResponseEvent {
                agent: agent.clone(),
                request: request.into(),
                event: ResponseEvent::Delta { block, kind, text },
            },
        )
    }

    /// The gauge shows the occupancy a committed response's own attempt reported, at
    /// its model's capacity: never a failed attempt's, never output alone, and nothing
    /// once a checkpoint replaces the history.
    #[test]
    fn the_gauge_reads_the_committed_attempts_reported_occupancy() {
        use crate::provider::protocol::Usage;
        use crate::session::{CompactionCheckpoint, CompactionTrigger, CompletedOutcome};
        let hub = Hub::default();
        let agent = AgentId::root(SessionId::from_bytes([7; 16]));
        let gauge = |hub: &Hub| {
            let snapshot = hub.borrow();
            (snapshot.context.get(&agent)).map(|usage| (usage.tokens, usage.capacity))
        };
        let usage = |request: u64, usage| SessionEvent::Usage {
            request: request.into(),
            usage,
        };
        let done = |request: u64, attempt| AttemptRef {
            request: request.into(),
            attempt,
        };
        let completed = |attempt, message: u64| SessionEvent::ResponseCompleted {
            attempt,
            message: RecordSeq::from(message).message(),
            outcome: CompletedOutcome::Answer,
        };
        request(&hub, &agent, 2, ModelPurpose::Agent);
        record(&hub, &agent, 3, attempt(2.into(), 1));
        record(&hub, &agent, 4, usage(2, tests::usage(2_000, 0, 20_000)));
        record(&hub, &agent, 5, failed(2, 1, "connection lost"));
        record(&hub, &agent, 6, attempt(2.into(), 2));
        record(&hub, &agent, 7, committed());
        record(&hub, &agent, 8, completed(done(2, 2), 7));
        assert_eq!(gauge(&hub), None, "a retry that reported nothing");
        request(&hub, &agent, 10, ModelPurpose::Agent);
        record(&hub, &agent, 11, attempt(10.into(), 1));
        let output = Usage {
            output_tokens: Some(7),
            ..Usage::default()
        };
        record(&hub, &agent, 12, usage(10, output));
        record(&hub, &agent, 13, committed());
        record(&hub, &agent, 14, completed(done(10, 1), 13));
        assert_eq!(gauge(&hub), None, "output alone");
        request(&hub, &agent, 16, ModelPurpose::Agent);
        record(&hub, &agent, 17, attempt(16.into(), 1));
        record(&hub, &agent, 18, usage(16, tests::usage(1_000, 0, 10)));
        record(&hub, &agent, 19, committed());
        record(&hub, &agent, 20, completed(done(16, 1), 19));
        let capacity = tests::profile().profile.max_context.get();
        assert_eq!(gauge(&hub), Some((1_010, capacity)));
        let checkpoint = CompactionCheckpoint {
            frontier: 20.into(),
            message: Message::User(Vec::new()),
            todos: Vec::new(),
            retained: Vec::new(),
            attempt: done(16, 1),
            trigger: CompactionTrigger::Overflow,
        };
        record(&hub, &agent, 21, SessionEvent::Compaction { checkpoint });
        assert_eq!(gauge(&hub), None, "after a checkpoint");
    }

    #[test]
    fn snapshot_handoff_and_commit_do_not_duplicate_streams() {
        let hub = Hub::default();
        let agent = AgentId::root(SessionId::from_bytes([1; 16]));
        let key = (agent.clone(), 7.into());
        delta(&hub, &agent, 7, "hello");
        let mut observer = hub.borrow().clone();
        let text =
            |snapshot: &ObservationSnapshot| snapshot.responses[&key].blocks()[0].text.clone();
        assert_eq!(text(&observer), "hello");
        let settled = RuntimeEvent::ResponseSettled {
            agent: agent.clone(),
            request: 7.into(),
            settlement: Settlement::Committed(3.into()),
        };
        let updates = [
            delta(&hub, &agent, 7, "!"),
            send(&hub, settled),
            record(&hub, &agent, 3, committed()),
        ];
        for (index, update) in updates.into_iter().enumerate() {
            observer.apply(update.clone());
            observer.apply(update);
            if index == 0 {
                assert_eq!(text(&observer), "hello!");
            }
        }
        assert!(observer.responses.is_empty());
        assert_eq!(observer.records.len(), 1);
        assert_eq!(observer.revision, 4);
    }

    #[test]
    fn a_retry_keeps_the_failed_partial_output_until_its_next_attempt() {
        let hub = Hub::default();
        let agent = AgentId::root(SessionId::from_bytes([2; 16]));
        let key = (agent.clone(), 7.into());
        let replayed = |snapshot: &ObservationSnapshot| replayed(snapshot.records.values());
        request(&hub, &agent, 7, ModelPurpose::Agent);
        delta(&hub, &agent, 7, "partial answer");
        record(&hub, &agent, 8, retried(7, 1));
        let snapshot = hub.borrow().clone();
        let response = &snapshot.responses[&key];
        assert_eq!(response.settlement(), Some(&lost()));
        assert_eq!(response.blocks()[0].text, "partial answer");
        record(&hub, &agent, 10, attempt(7.into(), 2));
        // Duplicate journal delivery cannot roll a newer response back.
        let recovery = snapshot.records[&8.into()].clone();
        send(&hub, RuntimeEvent::Record(Box::new(recovery)));
        let started = hub.borrow().clone();
        for response in [
            &started.responses[&key],
            &replayed(&started).responses[&key],
        ] {
            assert!(response.settlement().is_none());
            assert!(response.blocks().is_empty());
        }
        let json = |snapshot: &ObservationSnapshot| {
            serde_json::to_value(&snapshot.records[&8.into()]).unwrap()
        };
        assert_eq!(json(&started), json(&snapshot));
    }

    /// The runtime's settlement is authoritative in either order against the
    /// journal's `ModelFailed`, and a stop settles only still-open responses.
    #[test]
    fn settlement_survives_its_failure_record_and_stops_settle_open_responses_only() {
        let hub = Hub::default();
        let agent = AgentId::root(SessionId::from_bytes([5; 16]));
        let settle = |request: u64, settlement| {
            send(
                &hub,
                RuntimeEvent::ResponseSettled {
                    agent: agent.clone(),
                    request: request.into(),
                    settlement,
                },
            );
        };
        let response = |request: u64| {
            let key = (agent.clone(), request.into());
            hub.borrow().clone().responses.get(&key).cloned()
        };
        let settlement = |request| response(request).and_then(|r| r.settlement().cloned());
        // A record-driven failure yields to the runtime's later settlement.
        record(&hub, &agent, 5, failed(8, 1, "connection lost"));
        assert_eq!(settlement(8), Some(lost()));
        settle(8, Settlement::Committed(6.into()));
        assert_eq!(settlement(8), Some(Settlement::Committed(6.into())));
        // A stop settles the open response but leaves settled ones alone.
        delta(&hub, &agent, 9, "open");
        let state = AgentActivity::Stopped(TurnFailure::Interrupted);
        let activity = ObservedActivity { state, since: 0 };
        let agent_ = agent.clone();
        send(
            &hub,
            RuntimeEvent::Activity {
                agent: agent_,
                activity,
            },
        );
        let interrupted = Settlement::Failed(TurnFailure::Interrupted);
        assert_eq!(settlement(9), Some(interrupted));
        assert_eq!(settlement(8), Some(Settlement::Committed(6.into())));
        // A failure record for a request never observed live still registers.
        record(&hub, &agent, 45, failed(46, 1, "connection lost"));
        assert!(settlement(46).is_some());
    }
}
