use super::*;
use tokio::sync::broadcast;

/// The attached handle remains authoritative even after its update stream closes.
pub(super) struct ActiveObservation {
    pub(super) session: SessionHandle,
    receiver: Option<broadcast::Receiver<ObservedEvent>>,
}

/// A snapshot and the receiver obtained from the same subscription boundary.
/// Neither half can be installed separately by the event loop.
pub struct PreparedObservation {
    pub(super) active: ActiveObservation,
    pub(super) snapshot: ObservationSnapshot,
}
impl PreparedObservation {
    pub async fn subscribe(session: SessionHandle) -> Self {
        let observation = session.observe().await;
        Self {
            active: ActiveObservation {
                session,
                receiver: Some(observation.updates),
            },
            snapshot: observation.snapshot,
        }
    }
}

impl App {
    pub fn session(&self) -> Option<&SessionHandle> {
        match &self.phase {
            Phase::Open { observation, .. } => Some(&observation.session),
            Phase::Draft { .. } => None,
        }
    }

    /// The draft this session grew out of, whose notices it now owns.
    pub(super) fn attached_draft(&self) -> Option<&AgentId> {
        match &self.phase {
            Phase::Open { attached_draft, .. } => attached_draft.as_ref(),
            Phase::Draft { .. } => None,
        }
    }

    fn receiver_mut(&mut self) -> Option<&mut broadcast::Receiver<ObservedEvent>> {
        match &mut self.phase {
            Phase::Open { observation, .. } => observation.receiver.as_mut(),
            Phase::Draft { .. } => None,
        }
    }

    pub async fn recv_observation(&mut self) -> Result<ObservedEvent, broadcast::error::RecvError> {
        match self.receiver_mut() {
            Some(receiver) => receiver.recv().await,
            None => std::future::pending().await,
        }
    }

    pub fn try_recv_observation(
        &mut self,
    ) -> Result<ObservedEvent, broadcast::error::TryRecvError> {
        self.receiver_mut()
            .map_or(Err(broadcast::error::TryRecvError::Closed), |receiver| {
                receiver.try_recv()
            })
    }

    pub fn close_observation(&mut self) {
        if let Phase::Open { observation, .. } = &mut self.phase {
            observation.receiver = None;
        }
    }

    pub async fn resubscribe(&mut self) {
        let Phase::Open { observation, .. } = &mut self.phase else {
            return;
        };
        let prepared = PreparedObservation::subscribe(observation.session.clone()).await;
        *observation = prepared.active;
        self.snapshot = prepared.snapshot;
        self.reset_projection();
    }

    pub async fn session_ready(&mut self, session: SessionHandle) {
        self.session_started(PreparedObservation::subscribe(session).await);
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::*;

    fn replace_receiver(app: &mut App, receiver: broadcast::Receiver<ObservedEvent>) {
        let Phase::Open { observation, .. } = &mut app.phase else {
            panic!("fixture has a session");
        };
        observation.receiver = Some(receiver);
    }

    fn has_status(app: &App, text: &str) -> bool {
        let mut records = app.snapshot.records.values();
        records.any(
            |record| matches!(&record.event, SessionEvent::Status { message } if message == text),
        )
    }

    /// Deliver `event` as the host does: observe it, fold records, rebuild content.
    fn deliver(app: &mut App, event: RuntimeEvent) {
        let revision = app.snapshot.revision + 1;
        app.observe(ObservedEvent { revision, event });
        app.projection.rebuild(&app.snapshot);
        app.rebuild_content();
    }

    async fn journal(app: &mut App, event: SessionEvent) -> RecordSeq {
        let store = app.session().unwrap().store();
        let record = store.append(app.selected.clone(), event).await.unwrap();
        let sequence = record.sequence;
        deliver(app, RuntimeEvent::Record(Box::new(record)));
        sequence
    }

    #[tokio::test]
    async fn stopping_keeps_an_interrupted_reply_at_its_journal_position() {
        use skyhook::provider::protocol::{BlockId, BlockRef, ItemId, ItemKind, ResponseEvent};
        use skyhook::session::{AttemptRef, ModelContext, ModelPurpose, ProfileSnapshot};
        let (_root, mut app) = fixture().await;
        let agent = app.selected.clone();
        let profile = ProfileSnapshot {
            name: "fixture".into(),
            profile: app.launch.model.profile().clone(),
        };
        let context = ModelContext {
            purpose: ModelPurpose::Agent,
            profile,
            system: Vec::new(),
            tools: Vec::new(),
            response_schema: None,
        };
        let context = journal(&mut app, SessionEvent::ModelContext { context }).await;
        let requested = SessionEvent::ModelRequested {
            context,
            checkpoint: None,
            history: Vec::new(),
            tail: Vec::new(),
            history_lifetime: Default::default(),
        };
        let request = journal(&mut app, requested).await.request();
        let attempt = AttemptRef {
            request,
            attempt: 1,
        };
        journal(&mut app, SessionEvent::ModelAttemptStarted(attempt)).await;
        let block = BlockRef {
            item: ItemId::try_from("text".to_owned()).unwrap(),
            block: BlockId::try_from("text:0".to_owned()).unwrap(),
        };
        let delta = ResponseEvent::Delta {
            block,
            kind: ItemKind::Text,
            text: "cut short".into(),
        };
        let event = RuntimeEvent::ResponseEvent {
            agent: agent.clone(),
            request,
            event: delta,
        };
        deliver(&mut app, event);
        let shown = |app: &App| {
            app.entries()
                .iter()
                .any(|entry| entry.text().contains("cut short"))
        };
        assert!(shown(&app), "streaming");
        // The interruption ends the live reply before stopping settles it.
        journal(&mut app, SessionEvent::ModelAttemptInterrupted(attempt)).await;
        let activity = AgentActivity::Stopped(TurnFailure::Interrupted);
        let event = RuntimeEvent::Activity {
            agent: agent.clone(),
            activity,
        };
        deliver(&mut app, event);
        assert!(
            app.snapshot.responses[&(agent, request)]
                .settlement()
                .is_some()
        );
        assert!(shown(&app), "settled");
    }

    #[tokio::test]
    async fn closed_receiver_retains_session_and_resubscribe_installs_snapshot_with_updates() {
        let (_root, mut app) = fixture().await;
        let session = app.session().unwrap().clone();
        let root = session.root_agent().clone();
        let (sender, receiver) = broadcast::channel(1);
        replace_receiver(&mut app, receiver);
        drop(sender);
        let closed = app.recv_observation().await;
        assert!(matches!(closed, Err(broadcast::error::RecvError::Closed)));
        app.close_observation();
        assert_eq!(app.session_id(), Some(session.id()));
        let idle = tokio::time::timeout(Duration::from_millis(1), app.recv_observation()).await;
        assert!(idle.is_err());
        session
            .record_status(root.clone(), "while closed".into())
            .await
            .unwrap();
        app.resubscribe().await;
        assert!(has_status(&app, "while closed"));
        session
            .record_status(root, "after subscription".into())
            .await
            .unwrap();
        let event = tokio::time::timeout(Duration::from_secs(5), app.recv_observation()).await;
        let event = event.unwrap().unwrap();
        assert!(matches!(&event.event, RuntimeEvent::Record(record)
            if matches!(&record.event, SessionEvent::Status { message } if message == "after subscription")));
        // Lag likewise replaces both the snapshot and the receiver.
        let (sender, receiver) = broadcast::channel(1);
        sender.send(event.clone()).unwrap();
        sender.send(event).unwrap();
        replace_receiver(&mut app, receiver);
        let lagged = app.recv_observation().await;
        assert!(matches!(
            lagged,
            Err(broadcast::error::RecvError::Lagged(_))
        ));
        app.resubscribe().await;
        assert!(has_status(&app, "after subscription"));
        let empty = app.try_recv_observation();
        assert!(matches!(empty, Err(broadcast::error::TryRecvError::Empty)));
        session.shutdown().await.unwrap();
    }
}
