//! Host-only observation. A snapshot and its receiver share an atomic revision boundary.
use super::{RuntimeEvent, TurnFailure};
use crate::{
    identity::{AgentId, JobId},
    job::{JobEnvelope, JobState},
    provider::protocol::{LiveBlock, LiveResponse, Step},
    session::{
        EventRecord, MessageSeq, RecordSeq, RequestLedger, RequestPhase, RequestSeq, SessionEvent,
        SessionPhase,
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
    pub context: HashMap<AgentId, ContextUsage>,
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
                    &mut self.context,
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
}

impl Views<'_> {
    pub(crate) fn record(self, record: &EventRecord) {
        self.ledger.observe(record);
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
                        response.settle(Settlement::Failed(failure.failure().into()));
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
    }
}

/// Fold a live update the journal does not carry; `committed` tells whether a
/// record is in the journal yet.
pub(crate) fn live(
    responses: &mut Responses,
    activities: Option<&mut HashMap<AgentId, ObservedActivity>>,
    context: &mut HashMap<AgentId, ContextUsage>,
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
                Settlement::Committed(message) | Settlement::Aborted(message)
                    if committed(message.into()) =>
                {
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
        RuntimeEvent::Context { agent, usage } => {
            context.insert(agent, usage);
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

    /// Journals `request` and starts its first attempt just after it.
    fn request_attempt(hub: &Hub, agent: &AgentId, request: u64) {
        self::request(hub, agent, request, ModelPurpose::Agent);
        record(hub, agent, request + 1, attempt(request.into(), 1));
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
                    "commit" => drop(record(&hub, &agent, message, committed())),
                    "fail" => drop(record(
                        &hub,
                        &agent,
                        request + 3,
                        failed(request, 1, "aborted"),
                    )),
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
