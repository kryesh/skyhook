//! Host-only observation. A snapshot and its receiver share an atomic revision boundary.
use super::RuntimeEvent;
use crate::{
    identity::AgentId,
    provider::protocol::{BlockKind, ResponseAssembler, ResponseSnapshot},
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

    /// Derived aggregate for compact previews; live views should use snapshot items.
    pub fn text(&self) -> String {
        self.preview(BlockKind::Text)
    }

    /// Derived aggregate for compact previews; live views should use snapshot items.
    pub fn reasoning(&self) -> String {
        self.preview(BlockKind::Reasoning)
    }

    fn preview(&self, kind: BlockKind) -> String {
        self.snapshot()
            .items
            .into_iter()
            .flat_map(|item| item.blocks)
            .filter(|block| block.kind == kind)
            .map(|block| block.text)
            .collect()
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

    // Keep the broadcast API's ownership-preserving error contract; callers can
    // recover the original event when there are no subscribers.
    #[allow(clippy::result_large_err)]
    pub fn send(
        &self,
        event: RuntimeEvent,
    ) -> Result<usize, broadcast::error::SendError<RuntimeEvent>> {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let RuntimeEvent::Record(record) = &event
            && state.records.contains_key(&record.sequence)
        {
            return Ok(0);
        }
        state.revision += 1;
        state.reduce(event.clone());
        let _ = self.updates.send(ObservedEvent {
            revision: state.revision,
            event: event.clone(),
        });
        self.legacy.send(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        identity::SessionId,
        provider::protocol::{BlockContent, ContentDelta, ItemKind, Message, ResponseEvent, Usage},
    };

    fn emit(hub: &RuntimeEvents, agent: &AgentId, event: ResponseEvent) {
        let _ = hub.send(RuntimeEvent::ResponseEvent {
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
    fn ended_reasoning_blocks_remain_ended_while_item_is_open() {
        let hub = RuntimeEvents::new(&[]);
        let agent = AgentId::root(SessionId::from_bytes([2; 16]));
        emit(
            &hub,
            &agent,
            ResponseEvent::ItemStarted {
                id: "reasoning".into(),
                position: 0,
                kind: ItemKind::Reasoning,
            },
        );
        start_block(&hub, &agent, "reasoning", "first", 0, BlockKind::Reasoning);
        emit(
            &hub,
            &agent,
            ResponseEvent::BlockDelta {
                item: "reasoning".into(),
                block: "first".into(),
                delta: ContentDelta::Text("partial".into()),
            },
        );
        emit(
            &hub,
            &agent,
            ResponseEvent::BlockEnded {
                item: "reasoning".into(),
                block: "first".into(),
                content: BlockContent::Reasoning {
                    text: "first".into(),
                },
            },
        );
        let boundary = hub.observe().snapshot.responses[&(agent.clone(), 7)].snapshot();
        assert!(!boundary.items[0].ended);
        assert!(boundary.items[0].blocks[0].ended);
        assert_eq!(boundary.items[0].blocks[0].text, "first");

        start_block(&hub, &agent, "reasoning", "second", 1, BlockKind::Reasoning);
        emit(
            &hub,
            &agent,
            ResponseEvent::BlockEnded {
                item: "reasoning".into(),
                block: "second".into(),
                content: BlockContent::Reasoning {
                    text: "second".into(),
                },
            },
        );
        emit(
            &hub,
            &agent,
            ResponseEvent::ItemStarted {
                id: "answer".into(),
                position: 1,
                kind: ItemKind::Text,
            },
        );
        start_block(&hub, &agent, "answer", "text", 0, BlockKind::Text);
        emit(
            &hub,
            &agent,
            ResponseEvent::BlockDelta {
                item: "answer".into(),
                block: "text".into(),
                delta: ContentDelta::Text("answer".into()),
            },
        );
        let observation = hub.observe();
        let response = &observation.snapshot.responses[&(agent, 7)];
        let snapshot = response.snapshot();
        assert_eq!(snapshot.items.len(), 2);
        assert!(!snapshot.items[0].ended);
        assert_eq!(snapshot.items[0].blocks.len(), 2);
        assert!(
            snapshot.items[0]
                .blocks
                .iter()
                .all(|block| block.ended && block.content.is_some())
        );
        assert!(!snapshot.items[1].blocks[0].ended);
        assert_eq!(response.reasoning(), "firstsecond");
        assert_eq!(response.text(), "answer");
        assert!(response.error.is_none());
    }

    #[test]
    fn usage_updates_are_cumulative_snapshots_not_increments() {
        let hub = RuntimeEvents::new(&[]);
        let agent = AgentId::root(SessionId::from_bytes([3; 16]));
        let usage = Usage {
            input_tokens: 10,
            cached_input_tokens: 5,
            output_tokens: 2,
        };
        emit(&hub, &agent, ResponseEvent::UsageUpdated { usage });
        let final_usage = Usage {
            output_tokens: 7,
            ..usage
        };
        emit(
            &hub,
            &agent,
            ResponseEvent::UsageUpdated { usage: final_usage },
        );
        emit(
            &hub,
            &agent,
            ResponseEvent::UsageUpdated { usage: final_usage },
        );
        assert_eq!(
            hub.observe().snapshot.responses[&(agent, 7)]
                .snapshot()
                .usage,
            final_usage
        );
    }

    #[test]
    fn invalid_event_records_error_without_mutation_or_panic() {
        let hub = RuntimeEvents::new(&[]);
        let agent = AgentId::root(SessionId::from_bytes([4; 16]));
        emit(
            &hub,
            &agent,
            ResponseEvent::ItemStarted {
                id: "text".into(),
                position: 0,
                kind: ItemKind::Text,
            },
        );
        let before = hub.observe().snapshot.responses[&(agent.clone(), 7)].snapshot();
        emit(
            &hub,
            &agent,
            ResponseEvent::BlockDelta {
                item: "text".into(),
                block: "missing".into(),
                delta: ContentDelta::Text("invalid".into()),
            },
        );
        let _ = hub.send(RuntimeEvent::ResponseSettled {
            agent: agent.clone(),
            request: 7,
            message: None,
            error: None,
        });
        let observation = hub.observe();
        let response = &observation.snapshot.responses[&(agent, 7)];
        assert_eq!(response.snapshot(), before);
        assert!(response.error.is_some());
        assert!(response.settled);
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
            observation.snapshot.responses[&(agent.clone(), 7)].text(),
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
            observation.snapshot.responses[&(agent.clone(), 7)].text(),
            "hello!"
        );
        let _ = hub.send(RuntimeEvent::ResponseSettled {
            agent: agent.clone(),
            request: 7,
            message: Some(3),
            error: None,
        });
        let _ = hub.send(RuntimeEvent::Record(Box::new(EventRecord {
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
}
