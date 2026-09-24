//! Host-only observation. A snapshot and its receiver share an atomic revision boundary.
use super::{RuntimeEvent, TurnFailure};
use crate::{
    identity::AgentId,
    provider::protocol::{LiveBlock, LiveResponse, Step},
    session::{
        EventRecord, Message, MessageSeq, ModelFailureKind, RecordSeq, RequestSeq, SessionEvent,
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
        self.reduce(update.event, true);
    }

    /// `live` is false only while replaying history on open, where records are the
    /// sole source of activity. Live, the driver also emits activity directly.
    fn reduce(&mut self, event: RuntimeEvent, live: bool) {
        match event {
            RuntimeEvent::Record(record) => {
                if self.records.contains_key(&record.sequence) {
                    return;
                }
                match &record.event {
                    SessionEvent::ModelRecoveryScheduled { failure, .. } => {
                        // ModelFailed settles only its response. Recovery keeps the
                        // agent active without turning a transport failure into a
                        // terminal agent/job failure.
                        if let Some(SessionEvent::ModelFailed { attempt, .. }) =
                            self.records.get(failure).map(|record| &record.event)
                        {
                            let attempt = attempt.attempt.saturating_add(1);
                            self.activity.insert(
                                record.agent.clone(),
                                AgentActivity::Reconnecting { attempt },
                            );
                        }
                    }
                    SessionEvent::ModelAttemptStarted(attempt) => {
                        // One logical request survives retries; native item/block
                        // IDs may be reused. Clear only the displayed response,
                        // never the journal's per-attempt audit records.
                        self.responses.insert(
                            (record.agent.clone(), attempt.request),
                            ObservedResponse::default(),
                        );
                        let request = RecordSeq::from(attempt.request);
                        let context = self.records.get(&request).and_then(|request| {
                            crate::session::request_context(request, |sequence| {
                                self.records.get(&sequence)
                            })
                        });
                        let activity = if context.is_some_and(|context| {
                            context.purpose == crate::session::ModelPurpose::Compaction
                        }) {
                            AgentActivity::Compacting
                        } else {
                            AgentActivity::Working
                        };
                        // Live, every attempt site emits this activity itself before
                        // appending, and a late-forwarded record would regress newer
                        // state (a root blocked in `Tools` shown as working). Only the
                        // journal-only `Reconnecting` needs the record to end it.
                        if !live
                            || matches!(
                                self.activity.get(&record.agent),
                                None | Some(AgentActivity::Reconnecting { .. })
                            )
                        {
                            self.activity.insert(record.agent.clone(), activity);
                        }
                    }
                    SessionEvent::ModelRequested { .. }
                        if matches!(
                            self.activity.get(&record.agent),
                            Some(AgentActivity::Reconnecting { .. })
                        ) =>
                    {
                        self.activity
                            .insert(record.agent.clone(), AgentActivity::Working);
                    }
                    SessionEvent::AgentCompleted => {
                        self.activity
                            .insert(record.agent.clone(), AgentActivity::Idle);
                    }
                    SessionEvent::AgentInterrupted => {
                        self.activity.insert(
                            record.agent.clone(),
                            AgentActivity::Stopped(TurnFailure::Interrupted),
                        );
                    }
                    SessionEvent::AgentFailed { error } => {
                        // Replaying this re-arms the host's retry affordance, so a
                        // resumed session can continue a failed turn instead of
                        // appearing idle. A later attempt/completion overrides it.
                        // Live, the driver also emits the typed failure, in either
                        // order with this record; the rendered one never replaces it.
                        if !live
                            || !self
                                .activity
                                .get(&record.agent)
                                .is_some_and(AgentActivity::is_retryable)
                        {
                            self.activity.insert(
                                record.agent.clone(),
                                AgentActivity::Stopped(TurnFailure::Other(error.clone())),
                            );
                        }
                    }
                    SessionEvent::ModelFailed {
                        attempt,
                        error,
                        kind,
                    } => {
                        // The runtime's own settlement is authoritative and may
                        // arrive before or after this record. An abort commits its
                        // partial message before journaling the failure: that
                        // commit is the attempt's settlement, and a committed
                        // response is retired, never revived as one that committed
                        // nothing.
                        let key = (record.agent.clone(), attempt.request);
                        // A request precedes its failure; a journal that says otherwise
                        // has no partial commit to find.
                        let request = RecordSeq::from(attempt.request);
                        let after_request = request..record.sequence.max(request);
                        let committed = self
                            .records
                            .range(after_request)
                            .rev()
                            .filter(|(_, earlier)| earlier.agent == record.agent)
                            .take_while(|(_, earlier)| {
                                !matches!(earlier.event, SessionEvent::ModelAttemptStarted(started)
                                    if started == *attempt)
                            })
                            .any(|(_, earlier)| {
                                matches!(
                                    earlier.event,
                                    SessionEvent::MessageCommitted {
                                        message: Message::Assistant(_)
                                    }
                                )
                            });
                        if committed {
                            self.responses.remove(&key);
                        } else {
                            let response = self.responses.entry(key).or_default();
                            if response.settlement().is_none() {
                                let failure = match kind {
                                    ModelFailureKind::Refusal => {
                                        TurnFailure::Refused(error.clone())
                                    }
                                    ModelFailureKind::Error => TurnFailure::Other(error.clone()),
                                };
                                response.settle(Settlement::Failed(failure));
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
            RuntimeEvent::Context {
                agent,
                tokens,
                capacity,
            } => {
                self.context
                    .insert(agent, ContextUsage { tokens, capacity });
            }
            RuntimeEvent::TurnCompleted { .. } => {}
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

#[derive(Clone)]
pub(crate) struct RuntimeEvents {
    inner: Arc<Mutex<ObservationSnapshot>>,
    updates: broadcast::Sender<ObservedEvent>,
}

impl RuntimeEvents {
    pub fn new(records: &[EventRecord]) -> Self {
        let mut snapshot = ObservationSnapshot::default();
        for record in records {
            snapshot.reduce(RuntimeEvent::Record(Box::new(record.clone())), false);
        }
        snapshot.context = super::runtime::recorded_context(records);
        Self {
            inner: Arc::new(Mutex::new(snapshot)),
            updates: broadcast::channel(1024).0,
        }
    }

    pub fn observe(&self) -> Observation {
        let state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        Observation {
            snapshot: state.clone(),
            updates: self.updates.subscribe(),
        }
    }

    /// Whether the agent's last turn failed or was interrupted and can be continued.
    pub(crate) fn retryable(&self, agent: &AgentId) -> bool {
        let state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        state
            .activity
            .get(agent)
            .is_some_and(AgentActivity::is_retryable)
    }

    pub fn send(&self, event: RuntimeEvent) {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let RuntimeEvent::Record(record) = &event
            && state.records.contains_key(&record.sequence)
        {
            return;
        }
        state.revision += 1;
        state.reduce(event.clone(), true);
        let _ = self.updates.send(ObservedEvent {
            revision: state.revision,
            event,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{AttemptRef, Message};
    use crate::{
        identity::SessionId,
        provider::protocol::{BlockId, BlockRef, ItemId, ItemKind, ResponseEvent},
    };

    #[test]
    fn replayed_agent_failure_re_arms_the_retry_gate_and_later_work_clears_it() {
        use crate::session::{EventRecord, SessionEvent};
        let id = SessionId::from_bytes([3; 16]);
        let agent = AgentId::root(id);
        let record = |sequence: u64, event: SessionEvent| EventRecord {
            id: crate::identity::EventId::from_bytes([sequence as u8; 16]),
            sequence: sequence.into(),
            timestamp_millis: 0,
            agent: agent.clone(),
            event,
        };
        let failure = SessionEvent::AgentFailed {
            error: "the model declined to respond: content filter".into(),
        };
        // A reopened session must observe the failure, or its retry affordance
        // reports nothing to continue.
        let records = vec![record(1, failure.clone())];
        let snapshot = RuntimeEvents::new(&records).observe().snapshot;
        let activity = snapshot.activity.get(&agent).cloned();
        assert!(matches!(
            activity,
            Some(AgentActivity::Stopped(TurnFailure::Other(error))) if error.contains("declined to respond")
        ));
        // A later attempt supersedes it, so a continued turn is not stuck failed.
        let records = vec![
            record(1, failure),
            record(
                2,
                SessionEvent::ModelAttemptStarted(AttemptRef {
                    request: 1.into(),
                    attempt: 1,
                }),
            ),
        ];
        let snapshot = RuntimeEvents::new(&records).observe().snapshot;
        assert_eq!(snapshot.activity.get(&agent), Some(&AgentActivity::Working));
        // Live, the driver's typed failure and the forwarded record arrive in either
        // order; the typed one is what observers keep.
        let typed = AgentActivity::Stopped(TurnFailure::Aborted);
        let failed = SessionEvent::AgentFailed {
            error: TurnFailure::Aborted.to_string(),
        };
        for record_first in [true, false] {
            let hub = RuntimeEvents::new(&[]);
            let activity = |activity| RuntimeEvent::Activity {
                agent: agent.clone(),
                activity,
            };
            hub.send(activity(AgentActivity::Working));
            if record_first {
                super::tests::record(&hub, &agent, 5, failed.clone());
            }
            hub.send(activity(typed.clone()));
            if !record_first {
                super::tests::record(&hub, &agent, 5, failed.clone());
            }
            assert_eq!(hub.observe().snapshot.activity.get(&agent), Some(&typed));
        }
    }

    /// A late-forwarded attempt record must not regress newer live activity, but
    /// still ends a journal-only `Reconnecting`.
    #[test]
    fn a_late_attempt_record_does_not_regress_newer_live_activity() {
        use crate::session::{EventRecord, SessionEvent};
        let agent = AgentId::root(SessionId::from_bytes([4; 16]));
        let attempt = |sequence: u64| {
            RuntimeEvent::Record(Box::new(EventRecord {
                id: crate::identity::EventId::from_bytes([sequence as u8; 16]),
                sequence: sequence.into(),
                timestamp_millis: 0,
                agent: agent.clone(),
                event: SessionEvent::ModelAttemptStarted(AttemptRef {
                    request: 1.into(),
                    attempt: sequence,
                }),
            }))
        };
        let activity = |activity| RuntimeEvent::Activity {
            agent: agent.clone(),
            activity,
        };
        let hub = RuntimeEvents::new(&[]);
        hub.send(activity(AgentActivity::Working));
        hub.send(activity(AgentActivity::Tools));
        hub.send(attempt(1));
        let observed = hub.observe().snapshot;
        assert_eq!(observed.activity.get(&agent), Some(&AgentActivity::Tools));
        let recovering = AgentActivity::Reconnecting { attempt: 1 };
        hub.send(activity(recovering));
        hub.send(attempt(2));
        let observed = hub.observe().snapshot;
        assert_eq!(observed.activity.get(&agent), Some(&AgentActivity::Working));
    }

    fn emit(hub: &RuntimeEvents, agent: &AgentId, event: ResponseEvent) {
        let (agent, request) = (agent.clone(), 7.into());
        hub.send(RuntimeEvent::ResponseEvent {
            agent,
            request,
            event,
        });
    }

    /// Streams `text` into request 7's text block.
    fn delta(hub: &RuntimeEvents, agent: &AgentId, text: &str) {
        let block = BlockRef {
            item: ItemId::try_from("text".to_owned()).unwrap(),
            block: BlockId::try_from("text".to_owned()).unwrap(),
        };
        let kind = ItemKind::Text;
        let text = text.into();
        emit(hub, agent, ResponseEvent::Delta { block, kind, text });
    }

    fn record(hub: &RuntimeEvents, agent: &AgentId, sequence: u64, event: SessionEvent) {
        hub.send(RuntimeEvent::Record(Box::new(EventRecord {
            id: crate::identity::EventId::generate().unwrap(),
            sequence: sequence.into(),
            timestamp_millis: 0,
            agent: agent.clone(),
            event,
        })));
    }

    /// A failure of request 7's attempt `attempt` and the recovery it schedules.
    fn failed(attempt: u64) -> SessionEvent {
        SessionEvent::ModelFailed {
            attempt: AttemptRef {
                request: 7.into(),
                attempt,
            },
            error: "connection lost".into(),
            kind: crate::session::ModelFailureKind::Error,
        }
    }

    fn scheduled(failure: u64) -> SessionEvent {
        SessionEvent::ModelRecoveryScheduled {
            failure: failure.into(),
            delay_millis: 1000,
        }
    }

    #[test]
    fn snapshot_handoff_and_commit_do_not_duplicate_streams() {
        let hub = RuntimeEvents::new(&[]);
        let agent = AgentId::root(SessionId::from_bytes([1; 16]));
        let key = (agent.clone(), 7.into());
        delta(&hub, &agent, "hello");
        let mut observation = hub.observe();
        let text =
            |snapshot: &ObservationSnapshot| snapshot.responses[&key].blocks()[0].text.clone();
        assert_eq!(text(&observation.snapshot), "hello");
        delta(&hub, &agent, "!");
        let update = observation.updates.try_recv().unwrap();
        observation.snapshot.apply(update.clone());
        observation.snapshot.apply(update);
        assert_eq!(text(&observation.snapshot), "hello!");
        hub.send(RuntimeEvent::ResponseSettled {
            agent: agent.clone(),
            request: 7.into(),
            settlement: Settlement::Committed(3.into()),
        });
        let committed = SessionEvent::MessageCommitted {
            message: Message::Assistant(vec![]),
        };
        record(&hub, &agent, 3, committed);
        while let Ok(update) = observation.updates.try_recv() {
            observation.snapshot.apply(update.clone());
            observation.snapshot.apply(update);
        }
        assert!(observation.snapshot.responses.is_empty());
        assert_eq!(observation.snapshot.records.len(), 1);
        assert_eq!(observation.snapshot.revision, 4);
    }

    #[test]
    fn recovery_is_active_replayable_and_preserves_failed_partial_output() {
        let hub = RuntimeEvents::new(&[]);
        let agent = AgentId::root(SessionId::from_bytes([2; 16]));
        let key = (agent.clone(), 7.into());
        let replayed = |snapshot: &ObservationSnapshot| {
            let records: Vec<_> = snapshot.records.values().cloned().collect();
            RuntimeEvents::new(&records).observe().snapshot
        };
        delta(&hub, &agent, "partial answer");
        let activity = AgentActivity::Working;
        hub.send(RuntimeEvent::Activity {
            agent: agent.clone(),
            activity,
        });
        record(&hub, &agent, 8, failed(1));
        let found = &hub.observe().snapshot.activity[&agent];
        assert_eq!(*found, AgentActivity::Working);
        record(&hub, &agent, 9, scheduled(8));
        let snapshot = hub.observe().snapshot;
        let reconnecting = AgentActivity::Reconnecting { attempt: 2 };
        assert_eq!(snapshot.activity[&agent], reconnecting);
        let response = &snapshot.responses[&key];
        let lost = Settlement::Failed(TurnFailure::Other("connection lost".into()));
        assert_eq!(response.settlement(), Some(&lost));
        let found = &response.blocks()[0].text;
        assert_eq!(*found, "partial answer");
        assert_eq!(replayed(&snapshot).activity[&agent], reconnecting);
        // Duplicate journal delivery cannot roll a newer activity back.
        let requested = SessionEvent::ModelRequested {
            context: 1.into(),
            checkpoint: None,
            history: Vec::new(),
            tail: Vec::new(),
            history_lifetime: Default::default(),
        };
        record(&hub, &agent, 10, requested);
        hub.send(RuntimeEvent::Record(Box::new(
            snapshot.records[&9.into()].clone(),
        )));
        let found = &hub.observe().snapshot.activity[&agent];
        assert_eq!(*found, AgentActivity::Working);
        let attempt = SessionEvent::ModelAttemptStarted(AttemptRef {
            request: 7.into(),
            attempt: 2,
        });
        record(&hub, &agent, 11, attempt);
        let started = hub.observe().snapshot;
        let response = &started.responses[&key];
        assert!(response.settlement().is_none());
        assert!(response.blocks().is_empty());
        let json = |snapshot: &ObservationSnapshot| {
            serde_json::to_value(&snapshot.records[&8.into()]).unwrap()
        };
        assert_eq!(json(&started), json(&snapshot));
        let replayed_response = &replayed(&started).responses[&key];
        assert!(replayed_response.blocks().is_empty());
        assert!(replayed_response.settlement().is_none());
        record(&hub, &agent, 12, SessionEvent::AgentCompleted);
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
        let failed_attempt = |request: u64, attempt, error: &str| SessionEvent::ModelFailed {
            attempt: AttemptRef {
                request: request.into(),
                attempt,
            },
            error: error.into(),
            kind: crate::session::ModelFailureKind::Error,
        };
        let failed = |request, error: &str| failed_attempt(request, 1, error);
        let settlement = |request: u64| {
            hub.observe().snapshot.responses[&(agent.clone(), request.into())]
                .settlement()
                .cloned()
        };
        // An abort commits its partial message and journals ModelFailed together.
        delta(&hub, &agent, "partial");
        settle(7, Settlement::Aborted(3.into()));
        record(&hub, &agent, 4, failed(7, "provider aborted response"));
        assert_eq!(settlement(7), Some(Settlement::Aborted(3.into())));
        let blocks = hub.observe().snapshot.responses[&(agent.clone(), 7.into())].blocks()[0]
            .text
            .clone();
        assert_eq!(blocks, "partial");
        // A record-driven failure yields to the runtime's later settlement.
        record(&hub, &agent, 5, failed(8, "connection lost"));
        let lost = Settlement::Failed(TurnFailure::Other("connection lost".into()));
        assert_eq!(settlement(8), Some(lost));
        settle(8, Settlement::Committed(6.into()));
        assert_eq!(settlement(8), Some(Settlement::Committed(6.into())));
        // A stop settles the open response but leaves settled ones alone.
        hub.send(RuntimeEvent::ResponseEvent {
            agent: agent.clone(),
            request: 9.into(),
            event: ResponseEvent::Delta {
                block: BlockRef {
                    item: ItemId::try_from("text".to_owned()).unwrap(),
                    block: BlockId::try_from("text".to_owned()).unwrap(),
                },
                kind: ItemKind::Text,
                text: "open".into(),
            },
        });
        hub.send(RuntimeEvent::Activity {
            agent: agent.clone(),
            activity: AgentActivity::Stopped(TurnFailure::Interrupted),
        });
        let interrupted = Settlement::Failed(TurnFailure::Interrupted);
        assert_eq!(settlement(9), Some(interrupted));
        assert_eq!(settlement(7), Some(Settlement::Aborted(3.into())));
        assert_eq!(settlement(8), Some(Settlement::Committed(6.into())));
        // The commit the abort announced removes the response.
        let committed = || SessionEvent::MessageCommitted {
            message: Message::Assistant(vec![]),
        };
        record(&hub, &agent, 3, committed());
        let responses = hub.observe().snapshot.responses;
        assert!(!responses.contains_key(&(agent.clone(), 7.into())));
        assert!(responses.contains_key(&(agent.clone(), 8.into())));
        // The journal commits the partial message before the failure record of
        // the same transaction; the live settlement can land anywhere around
        // them. Whatever the order, a response that committed is retired, never
        // revived as a failure that committed nothing.
        let started = |request: u64, attempt| {
            SessionEvent::ModelAttemptStarted(AttemptRef {
                request: request.into(),
                attempt,
            })
        };
        let aborted = |request: u64, sequence: u64, order: &str| {
            record(&hub, &agent, request, started(request, 1));
            for step in order.split(' ') {
                match step {
                    "settle" => settle(request, Settlement::Aborted(sequence.into())),
                    "commit" => record(&hub, &agent, sequence, committed()),
                    "fail" => record(&hub, &agent, sequence + 1, failed(request, "aborted")),
                    _ => unreachable!(),
                }
            }
            assert!(
                !hub.observe()
                    .snapshot
                    .responses
                    .contains_key(&(agent.clone(), request.into())),
                "{order}"
            );
        };
        aborted(10, 11, "settle commit fail");
        aborted(13, 14, "commit settle fail");
        aborted(16, 17, "commit fail settle");
        // A failure record for a request never observed live still registers,
        // and a later attempt that committed nothing fails even though an
        // earlier attempt of the same request committed its partial response.
        record(&hub, &agent, 19, failed(20, "connection lost"));
        assert!(settlement(20).is_some());
        record(&hub, &agent, 21, started(21, 1));
        record(&hub, &agent, 22, committed());
        record(&hub, &agent, 23, failed(21, "aborted"));
        assert!(
            !hub.observe()
                .snapshot
                .responses
                .contains_key(&(agent.clone(), 21.into()))
        );
        record(&hub, &agent, 24, started(21, 2));
        record(&hub, &agent, 25, failed_attempt(21, 2, "connection lost"));
        let lost = Settlement::Failed(TurnFailure::Other("connection lost".into()));
        assert_eq!(settlement(21), Some(lost));
    }

    #[test]
    fn interruption_replaces_pending_recovery() {
        let hub = RuntimeEvents::new(&[]);
        let agent = AgentId::root(SessionId::from_bytes([3; 16]));
        record(&hub, &agent, 1, failed(3));
        record(&hub, &agent, 2, scheduled(1));
        assert_eq!(
            hub.observe().snapshot.activity[&agent],
            AgentActivity::Reconnecting { attempt: 4 }
        );
        record(&hub, &agent, 3, SessionEvent::AgentInterrupted);
        let found = &hub.observe().snapshot.activity[&agent];
        assert_eq!(*found, AgentActivity::Stopped(TurnFailure::Interrupted));
    }
}
