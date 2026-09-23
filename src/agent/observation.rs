//! Host-only observation. A snapshot and its receiver share an atomic revision boundary.
use super::RuntimeEvent;
use crate::{
    identity::AgentId,
    provider::protocol::{LiveBlock, LiveResponse, Step},
    session::{EventRecord, SessionEvent},
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
    Reconnecting { attempt: u64 },
    Tools,
    WaitingChildren,
    Compacting,
    Interrupted,
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct ContextUsage {
    pub tokens: u64,
    pub capacity: u64,
}

/// One request's response as observers see it: provisional blocks while it streams,
/// the final provisional view once it has ended, until its commit replaces it.
#[derive(Clone, Debug, Default)]
pub struct ObservedResponse {
    live: LiveResponse,
    ended: Option<Vec<LiveBlock>>,
    pub message: Option<u64>,
    pub settled: bool,
    pub error: Option<String>,
}

impl ObservedResponse {
    /// Blocks in arrival order.
    #[must_use]
    pub fn blocks(&self) -> &[LiveBlock] {
        self.ended.as_deref().unwrap_or_else(|| self.live.blocks())
    }

    /// Whether `block` is the one still being streamed.
    #[must_use]
    pub fn streaming(&self, block: &LiveBlock) -> bool {
        !self.settled && self.ended.is_none() && self.live.current() == Some(&block.block)
    }
}

#[derive(Clone, Debug, Default)]
pub struct ObservationSnapshot {
    pub revision: u64,
    pub records: BTreeMap<u64, EventRecord>,
    pub responses: HashMap<(AgentId, u64), ObservedResponse>,
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
                    SessionEvent::ModelRecoveryScheduled { attempt, .. } => {
                        // ModelFailed settles only its response. Recovery keeps the
                        // agent active without turning a transport failure into a
                        // terminal agent/job failure.
                        self.activity.insert(
                            record.agent.clone(),
                            AgentActivity::Reconnecting { attempt: *attempt },
                        );
                    }
                    SessionEvent::ModelAttemptStarted { request, .. } => {
                        // One logical request survives retries; native item/block
                        // IDs may be reused. Clear only the displayed response,
                        // never the journal's per-attempt audit records.
                        self.responses.insert(
                            (record.agent.clone(), *request),
                            ObservedResponse::default(),
                        );
                        let activity = if self.records.get(request).is_some_and(|record| {
                            matches!(
                                record.event,
                                SessionEvent::ModelRequested {
                                    purpose: crate::session::ModelPurpose::Compaction,
                                    ..
                                }
                            )
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
                        self.activity
                            .insert(record.agent.clone(), AgentActivity::Interrupted);
                    }
                    SessionEvent::AgentFailed { error } => {
                        // Replaying this re-arms the host's retry affordance, so a
                        // resumed session can continue a failed turn instead of
                        // appearing idle. A later attempt/completion overrides it.
                        self.activity
                            .insert(record.agent.clone(), AgentActivity::Failed(error.clone()));
                    }
                    SessionEvent::ModelFailed { request, error, .. } => {
                        let response = self
                            .responses
                            .entry((record.agent.clone(), *request))
                            .or_default();
                        response.settled = true;
                        response.error = Some(error.clone());
                    }
                    SessionEvent::MessageCommitted { .. } => {
                        self.responses
                            .retain(|_, response| response.message != Some(record.sequence));
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
                if response.ended.is_none() {
                    match std::mem::take(&mut response.live).push(event) {
                        Step::Open(live) => response.live = live,
                        Step::Ended { blocks, .. } => response.ended = Some(blocks),
                    }
                }
            }
            RuntimeEvent::ResponseSettled {
                agent,
                request,
                message,
                error,
            } => {
                let key = (agent, request);
                if message.is_some_and(|sequence| self.records.contains_key(&sequence)) {
                    self.responses.remove(&key);
                } else {
                    let response = self.responses.entry(key).or_default();
                    response.message = message;
                    response.settled = true;
                    // Do not erase a protocol validation failure on successful settlement.
                    if error.is_some() {
                        response.error = error;
                    }
                }
            }
            RuntimeEvent::Activity { agent, activity } => {
                if matches!(
                    activity,
                    AgentActivity::Interrupted | AgentActivity::Failed(_)
                ) {
                    for ((owner, _), response) in &mut self.responses {
                        if owner == &agent && !response.settled {
                            response.settled = true;
                            response.error = Some("Response interrupted".into());
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
        matches!(
            state.activity.get(agent),
            Some(AgentActivity::Failed(_) | AgentActivity::Interrupted)
        )
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
    use crate::{
        identity::SessionId,
        provider::protocol::{BlockId, BlockRef, ItemId, ItemKind, Message, ResponseEvent},
    };

    #[test]
    fn replayed_agent_failure_re_arms_the_retry_gate_and_later_work_clears_it() {
        use crate::session::{EventRecord, SessionEvent};
        let id = SessionId::from_bytes([3; 16]);
        let agent = AgentId::root(id);
        let record = |sequence: u64, event: SessionEvent| EventRecord {
            id: crate::identity::EventId::from_bytes([sequence as u8; 16]),
            sequence,
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
            Some(AgentActivity::Failed(error)) if error.contains("declined to respond")
        ));
        // A later attempt supersedes it, so a continued turn is not stuck failed.
        let records = vec![
            record(1, failure),
            record(
                2,
                SessionEvent::ModelAttemptStarted {
                    request: 1,
                    attempt: 1,
                },
            ),
        ];
        let snapshot = RuntimeEvents::new(&records).observe().snapshot;
        assert_eq!(snapshot.activity.get(&agent), Some(&AgentActivity::Working));
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
                sequence,
                timestamp_millis: 0,
                agent: agent.clone(),
                event: SessionEvent::ModelAttemptStarted {
                    request: 1,
                    attempt: sequence,
                },
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
        let (agent, request) = (agent.clone(), 7);
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

    fn stream_text(hub: &RuntimeEvents, agent: &AgentId, text: &str) {
        delta(hub, agent, text);
    }

    fn record(hub: &RuntimeEvents, agent: &AgentId, sequence: u64, event: SessionEvent) {
        hub.send(RuntimeEvent::Record(Box::new(EventRecord {
            id: crate::identity::EventId::generate().unwrap(),
            sequence,
            timestamp_millis: 0,
            agent: agent.clone(),
            event,
        })));
    }

    fn scheduled(attempt: u64) -> SessionEvent {
        SessionEvent::ModelRecoveryScheduled {
            request: 7,
            attempt,
            delay_millis: 1000,
            error: "connection lost".into(),
        }
    }

    #[test]
    fn snapshot_handoff_and_commit_do_not_duplicate_streams() {
        let hub = RuntimeEvents::new(&[]);
        let agent = AgentId::root(SessionId::from_bytes([1; 16]));
        let key = (agent.clone(), 7);
        stream_text(&hub, &agent, "hello");
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
            request: 7,
            message: Some(3),
            error: None,
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
        let key = (agent.clone(), 7);
        let replayed = |snapshot: &ObservationSnapshot| {
            let records: Vec<_> = snapshot.records.values().cloned().collect();
            RuntimeEvents::new(&records).observe().snapshot
        };
        stream_text(&hub, &agent, "partial answer");
        let activity = AgentActivity::Working;
        hub.send(RuntimeEvent::Activity {
            agent: agent.clone(),
            activity,
        });
        let failed = SessionEvent::ModelFailed {
            request: 7,
            attempt: 1,
            error: "connection lost".into(),
            kind: crate::session::ModelFailureKind::Error,
        };
        record(&hub, &agent, 8, failed);
        let found = &hub.observe().snapshot.activity[&agent];
        assert_eq!(*found, AgentActivity::Working);
        record(&hub, &agent, 9, scheduled(2));
        let snapshot = hub.observe().snapshot;
        let reconnecting = AgentActivity::Reconnecting { attempt: 2 };
        assert_eq!(snapshot.activity[&agent], reconnecting);
        let response = &snapshot.responses[&key];
        assert!(response.settled);
        assert_eq!(response.error.as_deref(), Some("connection lost"));
        let found = &response.blocks()[0].text;
        assert_eq!(*found, "partial answer");
        assert_eq!(replayed(&snapshot).activity[&agent], reconnecting);
        // Duplicate journal delivery cannot roll a newer activity back.
        let requested = SessionEvent::ModelRequested {
            context: 1,
            history: Vec::new(),
            tail: Vec::new(),
            history_lifetime: Default::default(),
            purpose: crate::session::ModelPurpose::Agent,
        };
        record(&hub, &agent, 10, requested);
        hub.send(RuntimeEvent::Record(Box::new(snapshot.records[&9].clone())));
        let found = &hub.observe().snapshot.activity[&agent];
        assert_eq!(*found, AgentActivity::Working);
        let attempt = SessionEvent::ModelAttemptStarted {
            request: 7,
            attempt: 2,
        };
        record(&hub, &agent, 11, attempt);
        let started = hub.observe().snapshot;
        let response = &started.responses[&key];
        assert!(!response.settled);
        assert!(response.error.is_none());
        assert!(response.blocks().is_empty());
        let json =
            |snapshot: &ObservationSnapshot| serde_json::to_value(&snapshot.records[&8]).unwrap();
        assert_eq!(json(&started), json(&snapshot));
        let replayed_response = &replayed(&started).responses[&key];
        assert!(replayed_response.blocks().is_empty());
        assert!(replayed_response.error.is_none());
        record(&hub, &agent, 12, SessionEvent::AgentCompleted);
        let completed = hub.observe().snapshot;
        assert_eq!(completed.activity[&agent], AgentActivity::Idle);
        assert_eq!(replayed(&completed).activity[&agent], AgentActivity::Idle);
    }

    #[test]
    fn interruption_replaces_pending_recovery() {
        let hub = RuntimeEvents::new(&[]);
        let agent = AgentId::root(SessionId::from_bytes([3; 16]));
        record(&hub, &agent, 1, scheduled(3));
        record(&hub, &agent, 2, SessionEvent::AgentInterrupted);
        let found = &hub.observe().snapshot.activity[&agent];
        assert_eq!(*found, AgentActivity::Interrupted);
    }
}
