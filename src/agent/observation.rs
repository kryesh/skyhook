//! Host-only observation. A snapshot and its receiver share an atomic revision boundary.
use super::{RuntimeEvent, TurnFailure};
use crate::{
    identity::AgentId,
    provider::protocol::{LiveBlock, LiveResponse, Step},
    session::{
        EventRecord, MessageSeq, ModelPurpose, RecordSeq, RequestLedger, RequestPhase, RequestSeq,
        SessionEvent,
    },
};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
};
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

#[derive(Clone, Debug)]
pub struct ContextUsage {
    pub tokens: u64,
    pub capacity: u64,
}

/// How a request's response settled, keyed by the journal sequence it produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Settlement {
    /// The assistant message committed at this sequence.
    Committed(MessageSeq),
    /// The provider cut the response: its completed content committed at this
    /// sequence, but the turn failed.
    Aborted(MessageSeq),
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
        matches!(
            self.settlement(),
            Some(Settlement::Aborted(_) | Settlement::Failed(_))
        )
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
    pub revision: u64,
    pub records: BTreeMap<RecordSeq, EventRecord>,
    /// Every request's lifecycle, folded from `records`.
    pub ledger: RequestLedger,
    pub responses: HashMap<(AgentId, RequestSeq), ObservedResponse>,
    pub activity: HashMap<AgentId, AgentActivity>,
    pub context: HashMap<AgentId, ContextUsage>,
}

impl ObservationSnapshot {
    /// Idempotent journal projection; live deltas are applied once by revision.
    pub fn apply(&mut self, update: ObservedEvent) {
        if update.revision <= self.revision {
            return;
        }
        self.revision = update.revision;
        self.reduce(update.event);
    }

    fn reduce(&mut self, event: RuntimeEvent) {
        match event {
            RuntimeEvent::Record(record) => {
                if self.records.contains_key(&record.sequence) {
                    return;
                }
                let changed = self.ledger.observe(&record).into_iter().next().is_some();
                let agent = &record.agent;
                match &record.event {
                    SessionEvent::ModelAttemptStarted(attempt) => {
                        // One logical request survives retries; native item/block
                        // IDs may be reused. Clear only the displayed response,
                        // never the journal's per-attempt audit records.
                        let key = (agent.clone(), attempt.request);
                        self.responses.insert(key, ObservedResponse::default());
                    }
                    SessionEvent::AgentCompleted => {
                        self.activity.insert(agent.clone(), AgentActivity::Idle);
                    }
                    SessionEvent::AgentInterrupted => {
                        let stopped = AgentActivity::Stopped(TurnFailure::Interrupted);
                        self.activity.insert(agent.clone(), stopped);
                    }
                    SessionEvent::AgentFailed { failure } => {
                        let stopped = AgentActivity::Stopped(failure.clone().into());
                        self.activity.insert(agent.clone(), stopped);
                    }
                    SessionEvent::ModelFailed { attempt, failure } => {
                        // The runtime's own settlement is authoritative and may
                        // arrive before or after this record. An abort commits its
                        // partial message before journaling the failure: that
                        // commit is the attempt's settlement, and a committed
                        // response is retired, never revived as one that committed
                        // nothing.
                        let key = (agent.clone(), attempt.request);
                        let committed = self.ledger.get(attempt.request).is_some_and(|request| {
                            matches!(
                                request.phase,
                                RequestPhase::Failed {
                                    message: Some(_),
                                    ..
                                }
                            )
                        });
                        if committed {
                            self.responses.remove(&key);
                        } else {
                            let response = self.responses.entry(key).or_default();
                            if response.settlement().is_none() {
                                response.settle(Settlement::Failed(failure.clone().into()));
                            }
                        }
                    }
                    SessionEvent::MessageCommitted { .. } => {
                        self.responses.retain(|_, response| {
                            !matches!(
                                response.settlement(),
                                Some(Settlement::Committed(message) | Settlement::Aborted(message))
                                    if RecordSeq::from(*message) == record.sequence
                            )
                        });
                    }
                    _ => {}
                }
                if changed {
                    self.follow_request(&record.agent);
                }
                self.records.insert(record.sequence, *record);
            }
            RuntimeEvent::ResponseEvent {
                agent,
                request,
                event,
            } => {
                let response = self.responses.entry((agent, request)).or_default();
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
                    Settlement::Committed(message) | Settlement::Aborted(message)
                        if self.records.contains_key(&message.into()) =>
                    {
                        self.responses.remove(&key);
                    }
                    how => self.responses.entry(key).or_default().settle(how),
                }
            }
            RuntimeEvent::Activity { agent, activity } => {
                if let AgentActivity::Stopped(failure) = &activity {
                    for ((owner, _), response) in &mut self.responses {
                        if owner == &agent && response.settlement().is_none() {
                            response.settle(Settlement::Failed(failure.clone()));
                        }
                    }
                }
                self.activity.insert(agent, activity);
            }
            RuntimeEvent::Context { agent, usage } => {
                self.context.insert(agent, usage);
            }
            RuntimeEvent::TurnCompleted { .. } => {}
        }
    }

    /// Before the runtime reports an agent, and while it reports it in a model
    /// request, the agent's pending request gives its activity: a scheduled recovery
    /// reconnects and a compaction compacts. Records never move a reported agent into
    /// or out of a request, so one forwarded late cannot regress newer activity.
    fn follow_request(&mut self, agent: &AgentId) {
        let in_request = self.activity.get(agent).is_none_or(|activity| {
            matches!(
                activity,
                AgentActivity::Working
                    | AgentActivity::Compacting
                    | AgentActivity::Reconnecting { .. }
            )
        });
        let open = self
            .ledger
            .open(agent)
            .and_then(|request| self.ledger.get(request));
        let (true, Some(request)) = (in_request, open) else {
            return;
        };
        let activity = match (&request.phase, request.purpose) {
            (RequestPhase::Retrying { attempt, .. }, _) => AgentActivity::Reconnecting {
                attempt: attempt.saturating_add(1),
            },
            (_, ModelPurpose::Compaction) => AgentActivity::Compacting,
            _ => AgentActivity::Working,
        };
        self.activity.insert(agent.clone(), activity);
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

/// Updates an observer may fall behind by before it lags.
const OBSERVATION_CAPACITY: usize = 1024;

#[derive(Clone)]
pub(crate) struct RuntimeEvents {
    inner: Arc<Mutex<ObservationSnapshot>>,
    updates: broadcast::Sender<ObservedEvent>,
}

impl RuntimeEvents {
    pub fn new(records: &[EventRecord]) -> Self {
        let mut snapshot = ObservationSnapshot::default();
        for record in records {
            snapshot.reduce(RuntimeEvent::Record(Box::new(record.clone())));
        }
        snapshot.context = super::runtime::recorded_context(records, &snapshot.ledger);
        Self {
            inner: Arc::new(Mutex::new(snapshot)),
            updates: broadcast::channel(OBSERVATION_CAPACITY).0,
        }
    }

    pub fn observe(&self) -> Observation {
        let state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        Observation {
            snapshot: state.clone(),
            updates: self.updates.subscribe(),
        }
    }

    /// Read the ledger folded from every record observed so far.
    pub fn ledger<R>(&self, read: impl FnOnce(&RequestLedger) -> R) -> R {
        read(&self.inner.lock().unwrap_or_else(|e| e.into_inner()).ledger)
    }

    pub fn send(&self, event: RuntimeEvent) {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let RuntimeEvent::Record(record) = &event
            && state.records.contains_key(&record.sequence)
        {
            return;
        }
        state.revision += 1;
        state.reduce(event.clone());
        let _ = self.updates.send(ObservedEvent {
            revision: state.revision,
            event,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Failure;
    use crate::session::{
        AttemptRef, Message, ModelContext,
        tests::{self, attempt, requested},
    };
    use crate::{
        identity::SessionId,
        provider::protocol::{BlockRef, ItemKind, ResponseEvent},
    };

    fn record(hub: &RuntimeEvents, agent: &AgentId, sequence: u64, event: SessionEvent) {
        let record = tests::record(agent, sequence, event);
        hub.send(RuntimeEvent::Record(Box::new(record)));
    }

    fn activity(hub: &RuntimeEvents, agent: &AgentId, activity: AgentActivity) {
        let agent = agent.clone();
        hub.send(RuntimeEvent::Activity { agent, activity });
    }

    /// Journals request `request` for `purpose`, its context just before it.
    fn request(hub: &RuntimeEvents, agent: &AgentId, request: u64, purpose: ModelPurpose) {
        let context = ModelContext::test(purpose, tests::profile());
        record(
            hub,
            agent,
            request - 1,
            SessionEvent::ModelContext { context },
        );
        record(hub, agent, request, requested((request - 1).into()));
    }

    /// Journals `request` and starts its first attempt just after it.
    fn request_attempt(hub: &RuntimeEvents, agent: &AgentId, request: u64) {
        self::request(hub, agent, request, ModelPurpose::Agent);
        record(hub, agent, request + 1, attempt(request.into(), 1));
    }

    fn failed(request: u64, attempt: u64, error: &str) -> SessionEvent {
        SessionEvent::ModelFailed {
            attempt: AttemptRef {
                request: request.into(),
                attempt,
            },
            failure: transport(error),
        }
    }

    fn scheduled(failure: u64) -> SessionEvent {
        SessionEvent::ModelRecoveryScheduled {
            failure: failure.into(),
            delay_millis: 1000,
        }
    }

    fn committed() -> SessionEvent {
        SessionEvent::MessageCommitted {
            message: Message::Assistant(vec![]),
        }
    }

    /// A retryable failure, as the runtime schedules recoveries for.
    fn transport(error: &str) -> Failure {
        Failure::Provider(error.into(), crate::provider::ProviderErrorKind::Transport)
    }

    fn lost() -> Settlement {
        Settlement::Failed(transport("connection lost").into())
    }

    /// Streams `text` into `request`'s text block.
    fn delta(hub: &RuntimeEvents, agent: &AgentId, request: u64, text: &str) {
        let block = BlockRef::single("text");
        let (kind, text) = (ItemKind::Text, text.into());
        hub.send(RuntimeEvent::ResponseEvent {
            agent: agent.clone(),
            request: request.into(),
            event: ResponseEvent::Delta { block, kind, text },
        });
    }

    #[test]
    fn replay_shows_each_agents_last_journaled_outcome() {
        let agent = AgentId::root(SessionId::from_bytes([3; 16]));
        let refused = Failure::Refused("content filter".into());
        let failure = SessionEvent::AgentFailed {
            failure: refused.clone(),
        };
        let snapshot = RuntimeEvents::new(&[tests::record(&agent, 1, failure)])
            .observe()
            .snapshot;
        assert_eq!(
            snapshot.activity[&agent],
            AgentActivity::Stopped(refused.into())
        );
    }

    #[test]
    fn records_refine_only_the_activity_of_a_model_request() {
        let hub = RuntimeEvents::new(&[]);
        let agent = AgentId::root(SessionId::from_bytes([4; 16]));
        let current = || hub.observe().snapshot.activity[&agent].clone();
        activity(&hub, &agent, AgentActivity::Working);
        request(&hub, &agent, 2, ModelPurpose::Agent);
        activity(&hub, &agent, AgentActivity::Tools);
        record(&hub, &agent, 3, attempt(2.into(), 1));
        assert_eq!(current(), AgentActivity::Tools);
        activity(&hub, &agent, AgentActivity::Working);
        record(&hub, &agent, 4, failed(2, 1, "connection lost"));
        assert_eq!(current(), AgentActivity::Working);
        record(&hub, &agent, 5, scheduled(4));
        assert_eq!(current(), AgentActivity::Reconnecting { attempt: 2 });
        record(&hub, &agent, 6, attempt(2.into(), 2));
        assert_eq!(current(), AgentActivity::Working);
        request(&hub, &agent, 8, ModelPurpose::Compaction);
        assert_eq!(current(), AgentActivity::Compacting);
        record(&hub, &agent, 9, attempt(8.into(), 1));
        record(&hub, &agent, 10, failed(8, 1, "connection lost"));
        record(&hub, &agent, 11, scheduled(10));
        assert_eq!(current(), AgentActivity::Reconnecting { attempt: 2 });
        record(&hub, &agent, 12, SessionEvent::AgentInterrupted);
        assert_eq!(current(), AgentActivity::Stopped(TurnFailure::Interrupted));
    }

    #[test]
    fn snapshot_handoff_and_commit_do_not_duplicate_streams() {
        let hub = RuntimeEvents::new(&[]);
        let agent = AgentId::root(SessionId::from_bytes([1; 16]));
        let key = (agent.clone(), 7.into());
        delta(&hub, &agent, 7, "hello");
        let mut observation = hub.observe();
        let text =
            |snapshot: &ObservationSnapshot| snapshot.responses[&key].blocks()[0].text.clone();
        assert_eq!(text(&observation.snapshot), "hello");
        delta(&hub, &agent, 7, "!");
        let update = observation.updates.try_recv().unwrap();
        observation.snapshot.apply(update.clone());
        observation.snapshot.apply(update);
        assert_eq!(text(&observation.snapshot), "hello!");
        hub.send(RuntimeEvent::ResponseSettled {
            agent: agent.clone(),
            request: 7.into(),
            settlement: Settlement::Committed(3.into()),
        });
        record(&hub, &agent, 3, committed());
        while let Ok(update) = observation.updates.try_recv() {
            observation.snapshot.apply(update.clone());
            observation.snapshot.apply(update);
        }
        assert!(observation.snapshot.responses.is_empty());
        assert_eq!(observation.snapshot.records.len(), 1);
        assert_eq!(observation.snapshot.revision, 4);
    }

    #[test]
    fn a_retry_keeps_the_failed_partial_output_until_its_next_attempt() {
        let hub = RuntimeEvents::new(&[]);
        let agent = AgentId::root(SessionId::from_bytes([2; 16]));
        let key = (agent.clone(), 7.into());
        let replayed = |snapshot: &ObservationSnapshot| {
            let records: Vec<_> = snapshot.records.values().cloned().collect();
            RuntimeEvents::new(&records).observe().snapshot
        };
        activity(&hub, &agent, AgentActivity::Working);
        request(&hub, &agent, 7, ModelPurpose::Agent);
        delta(&hub, &agent, 7, "partial answer");
        record(&hub, &agent, 8, failed(7, 1, "connection lost"));
        record(&hub, &agent, 9, scheduled(8));
        let snapshot = hub.observe().snapshot;
        let reconnecting = AgentActivity::Reconnecting { attempt: 2 };
        assert_eq!(snapshot.activity[&agent], reconnecting);
        // Replay has no runtime to report the agent, so the journal alone does.
        assert_eq!(replayed(&snapshot).activity[&agent], reconnecting);
        let response = &snapshot.responses[&key];
        assert_eq!(response.settlement(), Some(&lost()));
        assert_eq!(response.blocks()[0].text, "partial answer");
        record(&hub, &agent, 10, attempt(7.into(), 2));
        // Duplicate journal delivery cannot roll a newer activity back.
        let recovery = snapshot.records[&9.into()].clone();
        hub.send(RuntimeEvent::Record(Box::new(recovery)));
        let started = hub.observe().snapshot;
        assert_eq!(started.activity[&agent], AgentActivity::Working);
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
        record(&hub, &agent, 11, SessionEvent::AgentCompleted);
        let completed = hub.observe().snapshot;
        assert_eq!(completed.activity[&agent], AgentActivity::Idle);
        assert_eq!(replayed(&completed).activity[&agent], AgentActivity::Idle);
    }

    /// The runtime's settlement is authoritative in either order against the
    /// journal's `ModelFailed`, and a stop settles only still-open responses.
    #[test]
    fn settlement_survives_its_failure_record_and_stops_settle_open_responses_only() {
        let hub = RuntimeEvents::new(&[]);
        let agent = AgentId::root(SessionId::from_bytes([5; 16]));
        let settle = |request: u64, settlement| {
            hub.send(RuntimeEvent::ResponseSettled {
                agent: agent.clone(),
                request: request.into(),
                settlement,
            });
        };
        let response = |request: u64| {
            let key = (agent.clone(), request.into());
            hub.observe().snapshot.responses.get(&key).cloned()
        };
        let settlement = |request| response(request).and_then(|r| r.settlement().cloned());
        // An abort commits its partial message and journals ModelFailed together.
        delta(&hub, &agent, 7, "partial");
        settle(7, Settlement::Aborted(3.into()));
        record(&hub, &agent, 4, failed(7, 1, "provider aborted response"));
        assert_eq!(settlement(7), Some(Settlement::Aborted(3.into())));
        assert_eq!(response(7).unwrap().blocks()[0].text, "partial");
        // A record-driven failure yields to the runtime's later settlement.
        record(&hub, &agent, 5, failed(8, 1, "connection lost"));
        assert_eq!(settlement(8), Some(lost()));
        settle(8, Settlement::Committed(6.into()));
        assert_eq!(settlement(8), Some(Settlement::Committed(6.into())));
        // A stop settles the open response but leaves settled ones alone.
        delta(&hub, &agent, 9, "open");
        let stopped = AgentActivity::Stopped(TurnFailure::Interrupted);
        activity(&hub, &agent, stopped);
        let interrupted = Settlement::Failed(TurnFailure::Interrupted);
        assert_eq!(settlement(9), Some(interrupted));
        assert_eq!(settlement(7), Some(Settlement::Aborted(3.into())));
        assert_eq!(settlement(8), Some(Settlement::Committed(6.into())));
        // The commit the abort announced removes the response.
        record(&hub, &agent, 3, committed());
        assert!(response(7).is_none() && response(8).is_some());
        // The journal commits the partial message before the failure record of
        // the same transaction; the live settlement can land anywhere around
        // them. Whatever the order, a response that committed is retired, never
        // revived as a failure that committed nothing.
        for (request, order) in [(20, "settle commit fail"), (30, "commit settle fail")]
            .into_iter()
            .chain([(40, "commit fail settle")])
        {
            request_attempt(&hub, &agent, request);
            let message = request + 2;
            for step in order.split(' ') {
                match step {
                    "settle" => settle(request, Settlement::Aborted(message.into())),
                    "commit" => record(&hub, &agent, message, committed()),
                    "fail" => record(&hub, &agent, request + 3, failed(request, 1, "aborted")),
                    _ => unreachable!(),
                }
            }
            assert!(response(request).is_none(), "{order}");
        }
        // A failure record for a request never observed live still registers,
        // and a later attempt that committed nothing fails even though an
        // earlier attempt of the same request committed its partial response.
        record(&hub, &agent, 45, failed(46, 1, "connection lost"));
        assert!(settlement(46).is_some());
        request_attempt(&hub, &agent, 50);
        record(&hub, &agent, 52, committed());
        record(&hub, &agent, 53, failed(50, 1, "aborted"));
        assert!(response(50).is_none());
        record(&hub, &agent, 54, attempt(50.into(), 2));
        record(&hub, &agent, 55, failed(50, 2, "connection lost"));
        assert_eq!(settlement(50), Some(lost()));
    }
}
