//! Durable queued-submission intent, exact attempt binding, and final settlement.

use super::{
    AppendIdentity, EventRecord, QueueIntent, QueueSettlement, SessionError, SessionEvent,
    SessionStore,
};
use crate::{
    identity::{AgentId, QueueAttemptId},
    provider::protocol::Message,
};

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
        Ok(intent_records(&self.reconciled_records().await?, None))
    }
}

/// Fold the journal into intent records (only `attempt`'s, when given) in one
/// pass. Validation guarantees every attempt-bound record follows its intent.
fn intent_records(
    records: &[EventRecord],
    attempt: Option<QueueAttemptId>,
) -> Vec<QueueIntentRecord> {
    let mut intents: Vec<QueueIntentRecord> = Vec::new();
    let mut index = std::collections::HashMap::new();
    for record in records {
        if let SessionEvent::QueueIntent { intent } = &record.event {
            if attempt.is_none_or(|attempt| attempt == intent.attempt) {
                index.insert(intent.attempt, intents.len());
                intents.push(QueueIntentRecord {
                    agent: record.agent.clone(),
                    intent: intent.clone(),
                    message: None,
                    settlement: None,
                    acknowledged: false,
                });
            }
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

pub(super) fn validate_queue_record(
    records: &[EventRecord],
    record: &EventRecord,
) -> Result<(), SessionError> {
    let invalid = SessionError::InvalidQueue;
    let inherent = record.event.queue_attempt();
    if inherent.is_some() && inherent != record.queue_attempt {
        return Err(invalid("queue event and record attempt differ"));
    }
    let Some(attempt) = record.queue_attempt else {
        return Ok(());
    };
    let prior = intent_records(records, Some(attempt)).pop();
    if matches!(record.event, SessionEvent::QueueIntent { .. }) {
        if prior.is_some() {
            return Err(invalid("duplicate queue attempt identity"));
        }
        return Ok(());
    }
    let Some(prior) = prior else {
        return Err(invalid("unknown queue attempt"));
    };
    if prior.agent != record.agent {
        return Err(invalid("queue attempt belongs to another agent"));
    }
    let (settled, message) = (prior.settlement, prior.message);
    match &record.event {
        SessionEvent::ModelChanged { model_profile, .. } => {
            if settled.is_some()
                || message.is_some()
                || prior.intent.model.as_ref() != Some(model_profile)
            {
                return Err(invalid(
                    "model event differs from intent or follows commitment/settlement",
                ));
            }
        }
        SessionEvent::MessageCommitted {
            message: Message::User(content),
        } => {
            if settled.is_some() || message.is_some() || *content != prior.intent.content {
                return Err(invalid(
                    "message differs from intent or attempt already committed/settled",
                ));
            }
        }
        SessionEvent::QueueSettlement { settlement, .. } => {
            if settled.is_some() {
                return Err(invalid("queue attempt already settled"));
            }
            match (settlement, message) {
                (QueueSettlement::Committed { event }, Some(message))
                    if *event == message.event => {}
                (QueueSettlement::NotCommitted, None) => {}
                _ => {
                    return Err(invalid(
                        "settlement does not identify exact committed message or absence",
                    ));
                }
            }
        }
        SessionEvent::QueueAcknowledged { .. } => {
            if settled.is_none() {
                return Err(invalid("unsettled queue attempt cannot be acknowledged"));
            }
            if prior.acknowledged {
                return Err(invalid("queue attempt already acknowledged"));
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
    use crate::{
        identity::EventId,
        media::{AttachmentRef, MAX_IMAGE_BYTES},
        provider::protocol::UserContent,
        session::AppendBoundary,
    };

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
        let agent = AgentId::root(id);
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
        let (store, records) = reopen(store, root.path()).await;
        assert_eq!(records, vec![intent_record]);
        assert_eq!(store.queue_intents().await.unwrap(), pending);
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
        let (store, _) = reopen(store, root.path()).await;
        let recovered = store.queue_intents().await.unwrap();
        assert_eq!(
            recovered[0].message.map(|message| message.event),
            Some(committed.id)
        );
        assert_eq!(recovered[0].settlement, Some(settlement));
        assert!(!recovered[0].acknowledged);
        let event = SessionEvent::QueueAcknowledged { attempt };
        store.append(agent, event).await.unwrap();
        let (store, _) = reopen(store, root.path()).await;
        assert!(store.queue_intents().await.unwrap()[0].acknowledged);
    }

    fn record(
        agent: &AgentId,
        attempt: Option<QueueAttemptId>,
        event: SessionEvent,
    ) -> EventRecord {
        EventRecord {
            id: EventId::generate().unwrap(),
            queue_attempt: attempt,
            version: crate::session::SESSION_FORMAT_VERSION,
            sequence: 0,
            timestamp_millis: 0,
            agent: agent.clone(),
            event,
        }
    }

    /// Settlement and acknowledgement are bound exactly against the journal;
    /// an abandoned attempt cannot be resumed and a fresh one may follow it.
    #[test]
    fn queue_records_are_validated_against_the_journal() {
        let agent = AgentId::root(crate::identity::SessionId::from_bytes([7; 16]));
        let intent = new_intent();
        let attempt = intent.attempt;
        let bound = |event| record(&agent, Some(attempt), event);
        let settle = |settlement| {
            bound(SessionEvent::QueueSettlement {
                attempt,
                settlement,
            })
        };
        let intent_record = bound(SessionEvent::QueueIntent {
            intent: intent.clone(),
        });
        let message_record = bound(message(&intent));
        let committed = QueueSettlement::Committed {
            event: message_record.id,
        };
        let ack = || bound(SessionEvent::QueueAcknowledged { attempt });
        let mut fresh = intent.clone();
        fresh.attempt = QueueAttemptId::generate().unwrap();
        let fresh = record(
            &agent,
            Some(fresh.attempt),
            SessionEvent::QueueIntent { intent: fresh },
        );
        let journals = [
            vec![intent_record.clone()],
            vec![intent_record.clone(), message_record.clone()],
            vec![intent_record.clone(), message_record, settle(committed)],
            vec![
                intent_record.clone(),
                settle(QueueSettlement::NotCommitted),
                ack(),
            ],
        ];
        for (journal, candidate, ok) in [
            (0, intent_record, false),
            (0, ack(), false),
            (0, bound(message(&intent)), true),
            (0, settle(QueueSettlement::NotCommitted), true),
            (1, bound(message(&intent)), false),
            (1, settle(QueueSettlement::NotCommitted), false),
            (1, settle(committed), true),
            (2, settle(committed), false),
            (2, ack(), true),
            (3, bound(message(&intent)), false),
            (3, ack(), false),
            (3, fresh, true),
        ] {
            let valid = validate_queue_record(&journals[journal], &candidate).is_ok();
            assert_eq!(valid, ok, "journal {journal}: {:?}", candidate.event);
        }
    }

    #[tokio::test]
    async fn stored_images_precede_intent_write_and_survive_reopen() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::create(root.path()).await.unwrap();
        let agent = AgentId::root(store.id());
        let png = crate::tests::png(b"durable image payload");
        let image = store
            .store_image(Some("image.png".into()), &png)
            .await
            .unwrap();
        let mut intent = new_intent();
        let attachment = AttachmentRef::Image(image.clone());
        intent.content.push(UserContent::Attachment { attachment });
        let (reached, resume) = store.pause_append_at(AppendBoundary::Write).await;
        let event = SessionEvent::QueueIntent {
            intent: intent.clone(),
        };
        let accepted = store.accept_append(agent.clone(), event).await.unwrap();
        reached.await.unwrap();
        let blob = store
            .directory()
            .join("blobs")
            .join(image.blob.sha256.to_string());
        assert_eq!(tokio::fs::read(blob).await.unwrap(), png.bytes());
        resume.send(()).unwrap();
        let record = accepted.committed().await.unwrap();
        let SessionEvent::QueueIntent { intent: durable } = record.event else {
            panic!()
        };
        assert_eq!(durable, intent);
        let (store, _) = reopen(store, root.path()).await;
        let recovered = store.queue_intents().await.unwrap();
        assert_eq!(recovered[0].intent, durable);
        let Some(UserContent::Attachment {
            attachment: AttachmentRef::Image(recovered_image),
        }) = recovered[0].intent.content.last()
        else {
            panic!()
        };
        let limit = MAX_IMAGE_BYTES as usize;
        assert_eq!(
            store.read_blob(&recovered_image.blob, limit).await.unwrap(),
            png.bytes()
        );
        // The committed message must equal the intent content exactly.
        let accepted = store.accept_append_bound(agent, Some(intent.attempt), message(&intent));
        accepted.await.unwrap().committed().await.unwrap();
    }
}
