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
    snapshot: ObservationSnapshot,
    recovered: Result<Vec<skyhook::agent::RecoveredQueuedPrompt>, String>,
}
impl PreparedObservation {
    pub async fn subscribe(session: SessionHandle) -> Self {
        let recovered = session
            .recover_queued_prompts()
            .await
            .map_err(|error| error.to_string());
        let observation = session.observe().await;
        Self {
            active: ActiveObservation {
                session,
                receiver: Some(observation.updates),
            },
            snapshot: observation.snapshot,
            recovered,
        }
    }
}

impl App {
    pub fn session(&self) -> Option<&SessionHandle> {
        self.observation.as_ref().map(|active| &active.session)
    }

    /// Install a freshly attached session: every journal row is restored.
    pub(super) fn install_observation(&mut self, prepared: Option<PreparedObservation>) {
        self.install(prepared, true);
    }

    /// `restore` says whether journal rows the queue does not know are missing
    /// (restore them) or were removed by the user (leave them to their abandon).
    fn install(&mut self, prepared: Option<PreparedObservation>, restore: bool) {
        let (active, snapshot, recovered) = match prepared {
            Some(PreparedObservation {
                active,
                snapshot,
                recovered,
            }) => (Some(active), snapshot, recovered),
            None => (None, ObservationSnapshot::default(), Ok(Vec::new())),
        };
        self.observation = active;
        self.snapshot = snapshot;
        self.restore_durable_queue(recovered, restore);
    }

    fn receiver_mut(&mut self) -> Option<&mut broadcast::Receiver<ObservedEvent>> {
        self.observation.as_mut()?.receiver.as_mut()
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
        if let Some(active) = &mut self.observation {
            active.receiver = None;
        }
    }

    pub async fn resubscribe(&mut self) {
        let Some(session) = self.session().cloned() else {
            return;
        };
        let prepared = PreparedObservation::subscribe(session).await;
        // The queue already holds this session's rows: as for a requested scan,
        // unknown rows are restored only after a failed scan.
        let restore = matches!(
            self.queue_scan,
            queue::QueueScan::Failed | queue::QueueScan::Running { restore: true }
        );
        self.install(Some(prepared), restore);
        self.reset_projection();
    }

    /// All successful async creation/switch completions cross this gate. At
    /// most one creation or switch is in flight (see `begin_session`/`switch`),
    /// so every completion that arrives is the current one.
    pub async fn session_ready(&mut self, session: Option<SessionHandle>, started: bool) {
        let prepared = match session {
            Some(session) => Some(PreparedObservation::subscribe(session).await),
            None => None,
        };
        if started {
            if let Some(prepared) = prepared {
                self.session_started(prepared);
            }
        } else {
            self.set_session(prepared);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::*;

    fn has_status(app: &App, text: &str) -> bool {
        let mut records = app.snapshot.records.values();
        records.any(
            |record| matches!(&record.event, SessionEvent::Status { message } if message == text),
        )
    }

    #[tokio::test]
    async fn closed_receiver_retains_session_and_resubscribe_installs_snapshot_with_updates() {
        let (_root, mut app) = fixture().await;
        let session = app.session().unwrap().clone();
        let root = session.root_agent().clone();
        let (sender, receiver) = broadcast::channel(1);
        app.observation.as_mut().unwrap().receiver = Some(receiver);
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
        app.observation.as_mut().unwrap().receiver = Some(receiver);
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
