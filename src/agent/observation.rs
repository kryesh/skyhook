//! Host-only observation: the live part of a session and the updates after it, which
//! share an atomic revision boundary.
use super::{RuntimeEvent, TurnFailure};
use crate::{
    identity::{AgentId, JobId},
    job::{JobEnvelope, JobState},
    provider::protocol::{LiveBlock, LiveResponse, Step, UsageTotals},
    session::{
        EventRecord, Fact, MessageSeq, ModelPurpose, RecordSeq, RequestRecord, RequestSeq,
        ResidentLedger, SessionEvent, SessionPhase, ledger::Folded,
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
    pub fn is_busy(&self) -> bool {
        !matches!(self, Self::Idle | Self::Stopped(_))
    }

    /// Whether the last turn can be continued.
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
    usage: HashMap<AgentId, ContextUsage>,
}

impl ContextGauge {
    pub fn get(&self, agent: &AgentId) -> Option<&ContextUsage> {
        self.usage.get(agent)
    }

    /// The gauge of each agent's `gauged` request.
    pub(crate) fn opened(gauged: &[(RequestSeq, RequestRecord)]) -> Self {
        let usage = gauged.iter().filter_map(|(_, request)| {
            let tokens = request.phase.completed_usage()?.occupancy()?;
            Some((request.agent.clone(), ContextUsage::of(request, tokens)))
        });
        Self {
            usage: usage.collect(),
        }
    }

    /// Fold `record`, which `ledger` has already observed.
    fn fold(&mut self, ledger: &ResidentLedger, record: &EventRecord) {
        match crate::session::agent_state::gauged(ledger, record) {
            Some(Some((_, request, tokens))) => {
                let usage = ContextUsage::of(request, tokens);
                self.usage.insert(record.agent.clone(), usage);
            }
            Some(None) => drop(self.usage.remove(&record.agent)),
            None => {}
        }
    }
}

impl ContextUsage {
    /// `tokens` of `request`'s model's context.
    fn of(request: &RequestRecord, tokens: u64) -> Self {
        let capacity = request.profile.profile.max_context.get();
        Self { tokens, capacity }
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
    pub fn blocks(&self) -> &[LiveBlock] {
        match self {
            Self::Streaming(live) => live.blocks(),
            Self::Ended(blocks) | Self::Settled { blocks, .. } => blocks,
        }
    }

    /// Whether `block` is the one still being streamed.
    pub fn streaming(&self, block: &LiveBlock) -> bool {
        matches!(self, Self::Streaming(live) if live.current() == Some(&block.block))
    }

    pub fn settlement(&self) -> Option<&Settlement> {
        match self {
            Self::Settled { how, .. } => Some(how),
            Self::Streaming(_) | Self::Ended(_) => None,
        }
    }

    /// Whether the response settled without becoming a complete answer.
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

/// The live part of a session: what its work can still change without being
/// addressed, bounded by that work rather than by its history. History is read in
/// pages: see [`SessionStore::transcript`](crate::session::SessionStore::transcript).
#[derive(Clone, Debug, Default)]
pub struct ObservationSnapshot {
    /// Where the session is in its life. Once its work stops, as a crash stops
    /// it, every turn, request, response and job it left going reads as
    /// interrupted at its last activity, as reopening settles them.
    pub phase: SessionPhase,
    pub revision: u64,
    /// The newest record the slice folds. Every later record arrives as an update.
    pub applied: RecordSeq,
    /// When the newest record that is the session's activity was journaled.
    pub last_activity: Option<i64>,
    /// Each agent's newest request, whatever its phase.
    pub requests: ResidentLedger,
    /// The jobs the session holds, as the updates leave them.
    pub jobs: BTreeMap<JobId, JobEnvelope>,
    pub activity: HashMap<AgentId, ObservedActivity>,
    pub views: Views,
    /// The usage each agent's requests reported.
    pub usage: BTreeMap<AgentId, UsageTotals>,
}

/// What observers fold from each committed record beside the record itself and the
/// requests it moved.
#[derive(Clone, Debug, Default)]
pub struct Views {
    pub responses: HashMap<(AgentId, RequestSeq), ObservedResponse>,
    pub context: ContextGauge,
    /// How many entries each agent's transcript holds, for every agent the session
    /// started: the ordinal its next entry takes.
    pub entries: BTreeMap<AgentId, u64>,
}

type Responses = HashMap<(AgentId, RequestSeq), ObservedResponse>;

/// The jobs and requests an update read as interrupted because the session's
/// work stopped, which no update of their own reports.
#[derive(Debug, Default)]
pub struct StoppedReading {
    pub jobs: Vec<JobId>,
    pub requests: Vec<RequestSeq>,
}

/// What folding an update changed beyond what the update carries.
#[derive(Debug, Default)]
pub struct Applied {
    /// The ordinal its record took in its agent's transcript: none for an update that
    /// is not a record, or a record the slice already folds.
    pub entry: Option<u64>,
    pub stopped: StoppedReading,
    /// The requests its record changed.
    pub requests: Vec<RequestSeq>,
    /// The request of its agent that a newer one replaced, as it left it: the slice
    /// holds each agent's newest request only.
    pub superseded: Option<(RequestSeq, RequestRecord)>,
    /// What the fold decided of its record that presenting it reads.
    pub fact: Option<Fact>,
    /// The jobs the session let go of, as they left them.
    pub unheld: Vec<JobEnvelope>,
}

impl ObservationSnapshot {
    /// Fold `update`, once and in revision order.
    pub fn apply(&mut self, update: ObservedEvent) -> Applied {
        if update.revision <= self.revision {
            return Applied::default();
        }
        self.revision = update.revision;
        let mut applied = match update.event {
            RuntimeEvent::Record(record) => self.record(&record),
            RuntimeEvent::Job(job) => {
                self.jobs.insert(job.id, *job);
                Applied::default()
            }
            RuntimeEvent::JobsUnheld(jobs) => Applied {
                unheld: (jobs.iter())
                    .filter_map(|job| self.jobs.remove(job))
                    .collect(),
                ..Applied::default()
            },
            RuntimeEvent::Phase(phase) => {
                self.phase = phase;
                Applied::default()
            }
            event => {
                let applied = self.applied;
                let committed = |sequence: RecordSeq| sequence <= applied;
                let responses = &mut self.views.responses;
                live(responses, Some(&mut self.activity), committed, event);
                Applied::default()
            }
        };
        applied.stopped = self.read_phase();
        applied
    }

    /// Fold `record`: what it changed, nothing for one the slice already folds.
    fn record(&mut self, record: &EventRecord) -> Applied {
        // What the slice already folds, a read of the journal may deliver again.
        if record.sequence <= self.applied {
            return Applied::default();
        }
        self.applied = record.sequence;
        if record.event.is_activity() {
            self.last_activity = Some(record.timestamp_millis);
        }
        if let SessionEvent::Usage { usage, .. } = &record.event {
            (self.usage.entry(record.agent.clone()).or_default()).add(*usage);
        }
        let Folded {
            changed,
            superseded,
            fact,
        } = self.requests.fold(record);
        self.views.record(&self.requests, record);
        Applied {
            entry: Some(self.views.count(record)),
            requests: changed.into_iter().flatten().collect(),
            superseded,
            fact,
            ..Applied::default()
        }
    }

    /// Read what a session whose work stopped left going as interrupted at its
    /// last activity.
    pub(crate) fn read_phase(&mut self) -> StoppedReading {
        let Some(at) = self.phase.stopped_at(self.last_activity) else {
            return StoppedReading::default();
        };
        for response in self.views.responses.values_mut() {
            if response.settlement().is_none() {
                response.settle(Settlement::Failed(TurnFailure::Interrupted));
            }
        }
        let jobs = self
            .jobs
            .values_mut()
            .filter(|job| !job.state.is_terminal())
            .map(|job| {
                job.state = JobState::Interrupted;
                job.timing = job.timing.after(JobState::Interrupted, at);
                job.id
            });
        StoppedReading {
            jobs: jobs.collect(),
            requests: self.requests.stop(at),
        }
    }
}

impl Views {
    /// Count committed `record` in its agent's transcript: the ordinal it takes.
    pub(crate) fn count(&mut self, record: &EventRecord) -> u64 {
        let length = self.entries.entry(record.agent.clone()).or_default();
        *length += 1;
        *length - 1
    }

    /// Fold `record`, which `ledger` has already observed.
    pub(crate) fn record(&mut self, ledger: &ResidentLedger, record: &EventRecord) {
        self.context.fold(ledger, record);
        let agent = &record.agent;
        match &record.event {
            // An agent's responses are its latest turn's: its next agent request lets go
            // of what earlier requests left settled, as an open seeds them only from its
            // latest agent request.
            SessionEvent::ModelRequested { .. }
                if ledger
                    .get(record.sequence.request())
                    .is_some_and(|request| request.purpose == ModelPurpose::Agent) =>
            {
                let request = record.sequence.request();
                self.responses.retain(|(owner, at), response| {
                    owner != agent || *at >= request || response.settlement().is_none()
                });
            }
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
        | RuntimeEvent::JobsUnheld(_)
        | RuntimeEvent::TurnCompleted { .. }
        | RuntimeEvent::OutputChanged { .. }
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
            if let Some(AgentActivity::Stopped(failure)) = activity.as_ref().map(|a| &a.state) {
                for ((owner, _), response) in responses.iter_mut() {
                    if owner == &agent && response.settlement().is_none() {
                        response.settle(Settlement::Failed(failure.clone()));
                    }
                }
            }
            if let Some(activities) = activities {
                match activity {
                    Some(activity) => activities.insert(agent, activity),
                    None => activities.remove(&agent),
                };
            }
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

    /// A fresh observer's fold of the records among `updates`.
    fn replayed<'a>(updates: impl IntoIterator<Item = &'a ObservedEvent>) -> ObservationSnapshot {
        let hub = Hub::default();
        for update in updates {
            if let RuntimeEvent::Record(record) = &update.event {
                send(&hub, RuntimeEvent::Record(record.clone()));
            }
        }
        hub.into_inner()
    }

    /// Journals `event` at `sequence` seconds past the epoch.
    fn record(hub: &Hub, agent: &AgentId, sequence: u64, event: SessionEvent) -> ObservedEvent {
        let mut record = tests::record(agent, sequence, event);
        std::sync::Arc::make_mut(&mut record).timestamp_millis = sequence as i64 * 1000;
        send(hub, RuntimeEvent::Record(record))
    }

    /// Journals request `request` for `purpose`, its context just before it.
    fn request(
        hub: &Hub,
        agent: &AgentId,
        request: u64,
        purpose: ModelPurpose,
    ) -> [ObservedEvent; 2] {
        let context = ModelContext::test(purpose, tests::profile());
        let context = SessionEvent::ModelContext { context };
        [
            record(hub, agent, request - 1, context),
            record(hub, agent, request, requested((request - 1).into())),
        ]
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
            (snapshot.views.context.get(&agent)).map(|usage| (usage.tokens, usage.capacity))
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
        let text = |snapshot: &ObservationSnapshot| {
            snapshot.views.responses[&key].blocks()[0].text.clone()
        };
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
        assert!(observer.views.responses.is_empty());
        assert_eq!(observer.applied, 3.into());
        assert_eq!(observer.revision, 4);
    }

    #[test]
    fn a_retry_keeps_the_failed_partial_output_until_its_next_attempt() {
        let hub = Hub::default();
        let agent = AgentId::root(SessionId::from_bytes([2; 16]));
        let key = (agent.clone(), 7.into());
        let mut journal = Vec::from(request(&hub, &agent, 7, ModelPurpose::Agent));
        delta(&hub, &agent, 7, "partial answer");
        let recovery = record(&hub, &agent, 8, retried(7, 1));
        journal.push(recovery.clone());
        let snapshot = hub.borrow().clone();
        let response = &snapshot.views.responses[&key];
        assert_eq!(response.settlement(), Some(&lost()));
        assert_eq!(response.blocks()[0].text, "partial answer");
        journal.push(record(&hub, &agent, 10, attempt(7.into(), 2)));
        // Duplicate journal delivery cannot roll a newer response back.
        send(&hub, recovery.event);
        let started = hub.borrow().clone();
        for response in [
            &started.views.responses[&key],
            &replayed(&journal).views.responses[&key],
        ] {
            assert!(response.settlement().is_none());
            assert!(response.blocks().is_empty());
        }
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
            hub.borrow().clone().views.responses.get(&key).cloned()
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
                activity: Some(activity),
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
