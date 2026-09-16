//! Durable queued-submission intent, exact attempt binding, and final settlement.

use super::{
    AppendIdentity, EventRecord, QueueIntent, QueueSettlement, SessionError, SessionEvent,
    SessionStore,
};
use crate::{identity::AgentId, provider::protocol::Message};

/// Replayed immutable intent and its durable retirement state. An absent
/// settlement is unresolved, not permission to retry: the caller must inspect
/// the exact attempt's committed message while owning dispatch/recovery authority.
#[derive(Clone, Debug, PartialEq)]
pub struct QueueIntentRecord {
    pub agent: AgentId,
    pub intent: QueueIntent,
    /// The exact attempt's committed user message, when one was journaled.
    pub message: Option<AppendIdentity>,
    pub settlement: Option<QueueSettlement>,
    pub acknowledged: bool,
}

impl SessionStore {
    /// Every intent, including acknowledged ones. Fails closed while the writer is uncertain.
    pub async fn queue_intents(&self) -> Result<Vec<QueueIntentRecord>, SessionError> {
        Ok(intent_records(&self.reconciled_records().await?))
    }
}

/// Fold the journal into intent records in one pass. The database guarantees every
/// attempt-bound record follows its intent.
fn intent_records(records: &[EventRecord]) -> Vec<QueueIntentRecord> {
    let mut intents: Vec<QueueIntentRecord> = Vec::new();
    let mut index = std::collections::HashMap::new();
    for record in records {
        if let SessionEvent::QueueIntent { intent } = &record.event {
            index.insert(intent.attempt, intents.len());
            intents.push(QueueIntentRecord {
                agent: record.agent.clone(),
                intent: intent.clone(),
                message: None,
                settlement: None,
                acknowledged: false,
            });
            continue;
        }
        let Some(&position) = record.queue_attempt.and_then(|attempt| index.get(&attempt)) else {
            continue;
        };
        let intent = &mut intents[position];
        match &record.event {
            SessionEvent::MessageCommitted { .. } => {
                intent.message = Some(record.append_identity())
            }
            SessionEvent::QueueSettlement { settlement, .. } => {
                intent.settlement = Some(*settlement);
            }
            SessionEvent::QueueAcknowledged { .. } => intent.acknowledged = true,
            _ => {}
        }
    }
    intents
}

/// Queue rules the schema cannot express: an attempt binds only its own queue
/// events, a model change, or a user message equal to the intent's content.
/// Ordering, uniqueness and agent ownership are enforced by the database.
pub(super) fn validate_queue_record(
    records: &[EventRecord],
    record: &EventRecord,
) -> Result<(), SessionError> {
    let invalid = SessionError::InvalidQueue;
    let Some(attempt) = record.queue_attempt else {
        return Ok(());
    };
    match &record.event {
        event if event.queue_attempt().is_some() => {
            if event.queue_attempt() != Some(attempt) {
                return Err(invalid("queue event and record attempt differ"));
            }
        }
        SessionEvent::ModelChanged { .. } => {}
        SessionEvent::MessageCommitted {
            message: Message::User(content),
        } => {
            let differs = records.iter().rev().any(|prior| {
                matches!(&prior.event, SessionEvent::QueueIntent { intent }
                    if intent.attempt == attempt && intent.content != *content)
            });
            if differs {
                return Err(invalid("message differs from its queued intent"));
            }
        }
        _ => {
            return Err(invalid(
                "only queue model/user-message events may carry attempt identity",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{identity::QueueAttemptId, provider::protocol::UserContent};

    fn new_intent() -> QueueIntent {
        QueueIntent {
            attempt: QueueAttemptId::generate().unwrap(),
            content: vec![UserContent::Text {
                text: "immutable prompt".into(),
            }],
            model: Some("requested-model".into()),
        }
    }

    fn message(intent: &QueueIntent) -> SessionEvent {
        SessionEvent::MessageCommitted {
            message: Message::User(intent.content.clone()),
        }
    }

    async fn reopen(
        store: SessionStore,
        root: &std::path::Path,
    ) -> (SessionStore, Vec<EventRecord>) {
        let id = store.id();
        store.close().await.unwrap();
        drop(store);
        SessionStore::open(root, id).await.unwrap()
    }

    #[tokio::test]
    async fn intent_message_settlement_and_acknowledgement_survive_reopen() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::create(root.path()).await.unwrap();
        let id = store.id();
        let agent = crate::session::fixture::started(&store, root.path()).await;
        let intent = new_intent();
        let attempt = intent.attempt;
        let event = SessionEvent::QueueIntent {
            intent: intent.clone(),
        };
        let intent_record = store.append(agent.clone(), event).await.unwrap();
        assert_eq!(intent_record.queue_attempt, Some(attempt));
        let identity = intent_record.append_identity();
        assert_eq!(
            (identity.session, identity.queue_attempt),
            (id, Some(attempt))
        );
        let pending = store.queue_intents().await.unwrap();
        assert_eq!((&pending[0].intent, &pending[0].agent), (&intent, &agent));
        assert_eq!((pending[0].message, pending[0].settlement), (None, None));
        let accepted = store.accept_append_bound(agent.clone(), Some(attempt), message(&intent));
        let accepted = accepted.await.unwrap();
        let identity = accepted.identity();
        let committed = accepted.committed().await.unwrap();
        assert_eq!(
            (identity.event, identity.queue_attempt),
            (committed.id, Some(attempt))
        );
        let settlement = QueueSettlement::Committed {
            event: committed.id,
        };
        let event = SessionEvent::QueueSettlement {
            attempt,
            settlement,
        };
        store.append(agent.clone(), event).await.unwrap();
        let event = SessionEvent::QueueAcknowledged { attempt };
        store.append(agent, event).await.unwrap();
        let (store, records) = reopen(store, root.path()).await;
        assert!(records.contains(&intent_record));
        let recovered = store.queue_intents().await.unwrap();
        assert_eq!(
            recovered[0].message.map(|message| message.event),
            Some(committed.id)
        );
        assert_eq!(recovered[0].settlement, Some(settlement));
        assert!(recovered[0].acknowledged);
    }

    /// The journal rejects out-of-order, duplicate, foreign and altered attempt events,
    /// and an abandoned attempt does not block a fresh one.
    #[tokio::test]
    async fn queue_records_are_validated_against_the_journal() {
        let session = crate::session::fixture::MemorySession::new().await;
        let (store, agent) = (&session.store, session.agent.clone());
        let child = session.start_child(&agent, 0, None).await;
        let intent = new_intent();
        let attempt = intent.attempt;
        let queued = SessionEvent::QueueIntent {
            intent: intent.clone(),
        };
        let bound = async |agent: &AgentId, event| {
            let accepted = store.accept_append_bound(agent.clone(), Some(attempt), event);
            accepted.await?.committed().await
        };
        let not_committed = || SessionEvent::QueueSettlement {
            attempt,
            settlement: QueueSettlement::NotCommitted,
        };
        let ack = || SessionEvent::QueueAcknowledged { attempt };
        store.append(agent.clone(), queued.clone()).await.unwrap();
        let mut altered = intent.clone();
        altered.content.push(UserContent::Text { text: "x".into() });
        for (agent, event) in [
            (&agent, queued),
            (&agent, ack()),
            (&child, message(&intent)),
            (&agent, message(&altered)),
            (&agent, SessionEvent::AgentCompleted),
        ] {
            assert!(bound(agent, event).await.is_err());
        }
        let committed = bound(&agent, message(&intent)).await.unwrap();
        for event in [message(&intent), not_committed(), ack()] {
            assert!(bound(&agent, event).await.is_err());
        }
        let settled = QueueSettlement::Committed {
            event: committed.id,
        };
        let settle = SessionEvent::QueueSettlement {
            attempt,
            settlement: settled,
        };
        bound(&agent, settle.clone()).await.unwrap();
        assert!(bound(&agent, settle).await.is_err());
        bound(&agent, ack()).await.unwrap();
        assert!(bound(&agent, ack()).await.is_err());
        let fresh = SessionEvent::QueueIntent {
            intent: new_intent(),
        };
        store.append(agent, fresh).await.unwrap();
    }
}
