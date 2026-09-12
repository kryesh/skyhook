//! Host-only observation. A snapshot and its receiver share an atomic revision boundary.
use super::RuntimeEvent;
use crate::{
    identity::AgentId,
    provider::protocol::{ResponseAssembler, ResponseSnapshot},
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
    Reconnecting {
        attempt: u64,
        max_attempts: Option<u64>,
    },
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

#[derive(Clone, Debug, Default)]
pub struct LiveResponse {
    /// The sole source of truth for provider-native response state.
    assembler: ResponseAssembler,
    pub message: Option<u64>,
    pub settled: bool,
    pub error: Option<String>,
}

impl LiveResponse {
    pub fn snapshot(&self) -> ResponseSnapshot {
        self.assembler.snapshot()
    }
}

#[derive(Clone, Debug, Default)]
pub struct ObservationSnapshot {
    pub revision: u64,
    pub records: BTreeMap<u64, EventRecord>,
    pub responses: HashMap<(AgentId, u64), LiveResponse>,
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
                match &record.event {
                    SessionEvent::ModelRecoveryScheduled {
                        attempt,
                        max_attempts,
                        ..
                    } => {
                        // ModelFailed settles only its response. Recovery keeps the
                        // agent active without turning a transport failure into a
                        // terminal agent/job failure.
                        self.activity.insert(
                            record.agent.clone(),
                            AgentActivity::Reconnecting {
                                attempt: *attempt,
                                max_attempts: *max_attempts,
                            },
                        );
                    }
                    SessionEvent::ModelAttemptStarted { request, .. } => {
                        // One logical request survives retries; native item/block
                        // IDs may be reused. Clear only the displayed assembler,
                        // never the journal's per-attempt audit records.
                        self.responses
                            .insert((record.agent.clone(), *request), LiveResponse::default());
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
                        self.activity.insert(record.agent.clone(), activity);
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
                if let Err(error) = response.assembler.push(&event) {
                    response.error = Some(error.to_string());
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
    legacy: broadcast::Sender<RuntimeEvent>,
}

impl RuntimeEvents {
    pub fn new(records: &[EventRecord]) -> Self {
        let mut snapshot = ObservationSnapshot::default();
        for record in records {
            snapshot.reduce(RuntimeEvent::Record(Box::new(record.clone())));
        }
        snapshot.context = super::runtime::recorded_context(records);
        Self {
            inner: Arc::new(Mutex::new(snapshot)),
            updates: broadcast::channel(1024).0,
            legacy: broadcast::channel(1024).0,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
        self.legacy.subscribe()
    }

    pub fn observe(&self) -> Observation {
        let state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        Observation {
            snapshot: state.clone(),
            updates: self.updates.subscribe(),
        }
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
            event: event.clone(),
        });
        let _ = self.legacy.send(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        identity::SessionId,
        provider::protocol::{BlockKind, ContentDelta, ItemKind, Message, ResponseEvent},
    };

    fn emit(hub: &RuntimeEvents, agent: &AgentId, event: ResponseEvent) {
        hub.send(RuntimeEvent::ResponseEvent {
            agent: agent.clone(),
            request: 7,
            event,
        });
    }

    fn start_block(
        hub: &RuntimeEvents,
        agent: &AgentId,
        item: &str,
        block: &str,
        position: usize,
        kind: BlockKind,
    ) {
        emit(
            hub,
            agent,
            ResponseEvent::BlockStarted {
                item: item.into(),
                id: block.into(),
                position,
                kind,
            },
        );
    }

    #[test]
    fn snapshot_handoff_and_commit_do_not_duplicate_streams() {
        let hub = RuntimeEvents::new(&[]);
        let agent = AgentId::root(SessionId::from_bytes([1; 16]));
        emit(
            &hub,
            &agent,
            ResponseEvent::ItemStarted {
                id: "text".into(),
                position: 0,
                kind: ItemKind::Text,
            },
        );
        start_block(&hub, &agent, "text", "text", 0, BlockKind::Text);
        emit(
            &hub,
            &agent,
            ResponseEvent::BlockDelta {
                item: "text".into(),
                block: "text".into(),
                delta: ContentDelta::Text("hello".into()),
            },
        );
        let mut observation = hub.observe();
        assert_eq!(
            observation.snapshot.responses[&(agent.clone(), 7)]
                .snapshot()
                .items[0]
                .blocks[0]
                .text,
            "hello"
        );
        emit(
            &hub,
            &agent,
            ResponseEvent::BlockDelta {
                item: "text".into(),
                block: "text".into(),
                delta: ContentDelta::Text("!".into()),
            },
        );
        let update = observation.updates.try_recv().unwrap();
        observation.snapshot.apply(update.clone());
        observation.snapshot.apply(update);
        assert_eq!(
            observation.snapshot.responses[&(agent.clone(), 7)]
                .snapshot()
                .items[0]
                .blocks[0]
                .text,
            "hello!"
        );
        hub.send(RuntimeEvent::ResponseSettled {
            agent: agent.clone(),
            request: 7,
            message: Some(3),
            error: None,
        });
        hub.send(RuntimeEvent::Record(Box::new(EventRecord {
            version: 1,
            sequence: 3,
            timestamp_millis: 0,
            agent,
            event: SessionEvent::MessageCommitted {
                message: Message::Assistant(vec![]),
            },
        })));
        while let Ok(update) = observation.updates.try_recv() {
            observation.snapshot.apply(update.clone());
            observation.snapshot.apply(update);
        }
        assert!(observation.snapshot.responses.is_empty());
        assert_eq!(observation.snapshot.records.len(), 1);
        assert_eq!(observation.snapshot.revision, 6);
    }
    fn record(hub: &RuntimeEvents, agent: &AgentId, sequence: u64, event: SessionEvent) {
        hub.send(RuntimeEvent::Record(Box::new(EventRecord {
            version: crate::session::SESSION_FORMAT_VERSION,
            sequence,
            timestamp_millis: 0,
            agent: agent.clone(),
            event,
        })));
    }

    #[test]
    fn recovery_is_active_replayable_and_preserves_failed_partial_output() {
        let hub = RuntimeEvents::new(&[]);
        let agent = AgentId::root(SessionId::from_bytes([2; 16]));
        emit(
            &hub,
            &agent,
            ResponseEvent::ItemStarted {
                id: "text".into(),
                position: 0,
                kind: ItemKind::Text,
            },
        );
        start_block(&hub, &agent, "text", "text", 0, BlockKind::Text);
        emit(
            &hub,
            &agent,
            ResponseEvent::BlockDelta {
                item: "text".into(),
                block: "text".into(),
                delta: ContentDelta::Text("partial answer".into()),
            },
        );
        hub.send(RuntimeEvent::Activity {
            agent: agent.clone(),
            activity: AgentActivity::Working,
        });
        record(
            &hub,
            &agent,
            8,
            SessionEvent::ModelFailed {
                request: 7,
                attempt: 1,
                error: "connection lost".into(),
            },
        );
        assert_eq!(
            hub.observe().snapshot.activity[&agent],
            AgentActivity::Working
        );
        record(
            &hub,
            &agent,
            9,
            SessionEvent::ModelRecoveryScheduled {
                request: 7,
                attempt: 2,
                max_attempts: Some(3),
                delay_millis: 1000,
                error: "connection lost".into(),
            },
        );
        let snapshot = hub.observe().snapshot;
        assert_eq!(
            snapshot.activity[&agent],
            AgentActivity::Reconnecting {
                attempt: 2,
                max_attempts: Some(3)
            }
        );
        let response = &snapshot.responses[&(agent.clone(), 7)];
        assert!(response.settled);
        assert_eq!(response.error.as_deref(), Some("connection lost"));
        assert_eq!(
            response.snapshot().items[0].blocks[0].text,
            "partial answer"
        );
        let records: Vec<_> = snapshot.records.values().cloned().collect();
        let replayed = RuntimeEvents::new(&records).observe().snapshot;
        assert_eq!(replayed.activity[&agent], snapshot.activity[&agent]);
        // Duplicate journal delivery cannot roll a newer activity back.
        record(
            &hub,
            &agent,
            10,
            SessionEvent::ModelRequested {
                context: 1,
                messages: Vec::new(),
                purpose: crate::session::ModelPurpose::Agent,
            },
        );
        hub.send(RuntimeEvent::Record(Box::new(records[1].clone())));
        assert_eq!(
            hub.observe().snapshot.activity[&agent],
            AgentActivity::Working
        );
        record(
            &hub,
            &agent,
            11,
            SessionEvent::ModelAttemptStarted {
                request: 7,
                attempt: 2,
            },
        );
        let started = hub.observe().snapshot;
        let response = &started.responses[&(agent.clone(), 7)];
        assert!(!response.settled);
        assert!(response.error.is_none());
        assert!(response.snapshot().items.is_empty());
        assert_eq!(
            serde_json::to_value(&started.records[&8]).unwrap(),
            serde_json::to_value(&snapshot.records[&8]).unwrap()
        );
        let records: Vec<_> = started.records.values().cloned().collect();
        let replayed = RuntimeEvents::new(&records).observe().snapshot;
        assert!(
            replayed.responses[&(agent.clone(), 7)]
                .snapshot()
                .items
                .is_empty()
        );
        assert!(replayed.responses[&(agent.clone(), 7)].error.is_none());
        record(&hub, &agent, 12, SessionEvent::AgentCompleted);
        assert_eq!(hub.observe().snapshot.activity[&agent], AgentActivity::Idle);
        let records: Vec<_> = hub.observe().snapshot.records.values().cloned().collect();
        assert_eq!(
            RuntimeEvents::new(&records).observe().snapshot.activity[&agent],
            AgentActivity::Idle
        );
    }

    #[test]
    fn interruption_replaces_pending_recovery() {
        let hub = RuntimeEvents::new(&[]);
        let agent = AgentId::root(SessionId::from_bytes([3; 16]));
        record(
            &hub,
            &agent,
            1,
            SessionEvent::ModelRecoveryScheduled {
                request: 7,
                attempt: 3,
                max_attempts: Some(3),
                delay_millis: 1000,
                error: "connection lost".into(),
            },
        );
        record(&hub, &agent, 2, SessionEvent::AgentInterrupted);
        assert_eq!(
            hub.observe().snapshot.activity[&agent],
            AgentActivity::Interrupted
        );
    }
}
