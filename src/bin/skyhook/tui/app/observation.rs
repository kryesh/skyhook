use super::*;
use skyhook::agent::{HarnessError, Observation};
use tokio::sync::broadcast;

/// The attached handle remains authoritative even after its update stream closes.
pub(super) struct ActiveObservation {
    pub(super) session: SessionHandle,
    link: Link,
}

/// How the app receives the session's updates.
enum Link {
    Live(broadcast::Receiver<ObservedEvent>),
    /// Updates were lost: they wait for the observation being read again, after a
    /// read that `failed` if one did.
    Reading {
        failed: bool,
    },
    /// Reading it failed: the next tick reads again.
    Failed,
    /// The session sends no more.
    Closed,
}

/// A snapshot and the receiver obtained from the same subscription boundary, with
/// what the app shows of the session read after it. Neither half can be installed
/// separately by the event loop.
pub struct PreparedObservation {
    pub(super) active: ActiveObservation,
    pub(super) snapshot: Box<ObservationSnapshot>,
    pub(super) projection: Box<Projection>,
    /// The viewed agent's tab, its first page loaded around the view's top.
    pub(super) window: Option<Box<model::Window>>,
    /// Whether the root journaled a failed turn after the record the app last read.
    pub(super) root_failed: bool,
}
impl PreparedObservation {
    /// Observe `session` and read what the app shows of it first, off the event
    /// loop: every agent's summary, and its root's conversation.
    pub async fn load(session: SessionHandle) -> Result<Self, HarnessError> {
        let root = session.root_agent().clone();
        let view = (root, Tab::Conversation, None);
        Self::view(session, Some(view), None).await
    }

    /// [`load`](Self::load), viewing `agent`'s `tab` around `focus` if a view is shown,
    /// and whether the root failed a turn after `read`.
    async fn view(
        session: SessionHandle,
        view: Option<(AgentId, Tab, Option<model::Focus>)>,
        read: Option<skyhook::session::RecordSeq>,
    ) -> Result<Self, HarnessError> {
        let Observation { snapshot, updates } = session.store().observe();
        let agents = session.store().agents().await?;
        let owners = agents.agents.iter().filter_map(|agent| agent.owner_job);
        let owners = session.jobs().views(owners.collect()).await?;
        let projection = Projection::new(agents, owners, &snapshot);
        let mut window = None;
        if let Some((agent, tab, focus)) = view {
            let mut shown = model::Window::new(agent, tab, super::pages::window_id(), focus);
            for (direction, want) in shown.plan(&[], focus) {
                let loaded = super::pages::load(&session, &shown.agent, want).await?;
                shown.loaded(direction, loaded);
            }
            window = Some(Box::new(shown));
        }
        let root_failed = match read {
            Some(read) => {
                session
                    .store()
                    .failed_after(session.root_agent(), read)
                    .await?
            }
            None => false,
        };
        Ok(Self {
            active: ActiveObservation {
                session,
                link: Link::Live(updates),
            },
            snapshot: Box::new(snapshot),
            projection: Box::new(projection),
            window,
            root_failed,
        })
    }

    pub fn session(&self) -> &SessionHandle {
        &self.active.session
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
            Phase::Open {
                observation:
                    ActiveObservation {
                        link: Link::Live(receiver),
                        ..
                    },
                ..
            } => Some(receiver),
            _ => None,
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
            observation.link = Link::Closed;
        }
    }

    /// Observe the session again, once its last read failed.
    pub(super) fn retry_observation(&mut self) {
        if let Phase::Open { observation, .. } = &self.phase
            && matches!(observation.link, Link::Failed)
        {
            self.resubscribe();
        }
    }

    /// Observe the session again once updates were lost, off the event loop: the
    /// updates after the loss are not read until the new observation lands, which
    /// reads the viewed pages again.
    pub fn resubscribe(&mut self) {
        let Phase::Open { observation, .. } = &mut self.phase else {
            return;
        };
        let failed = matches!(observation.link, Link::Failed);
        observation.link = Link::Reading { failed };
        let (session, tx) = (observation.session.clone(), self.tx.clone());
        // A session out of view reads no view again; one in view reads it around its top.
        let view = (self.window.as_ref())
            .map(|window| (window.agent.clone(), window.tab, self.focus_of_view()));
        let read = self.snapshot.applied;
        tokio::spawn(async move {
            let result = PreparedObservation::view(session, view, Some(read)).await;
            let _ = tx.send(Work::Observed(result.map(Box::new)));
        });
    }

    /// Install the observation a resubscription read, if this session still shows it.
    pub(super) fn observed_again(
        &mut self,
        result: Result<Box<PreparedObservation>, HarnessError>,
    ) {
        let Phase::Open { observation, .. } = &mut self.phase else {
            return;
        };
        let prepared = match result {
            Ok(prepared) if prepared.session().id() == observation.session.id() => prepared,
            Ok(_) => return,
            Err(error) => {
                let Link::Reading { failed } = observation.link else {
                    return;
                };
                observation.link = Link::Failed;
                // A failure repeated as it is read again is reported once.
                if !failed {
                    self.local_notice(error.to_string());
                }
                return;
            }
        };
        let PreparedObservation {
            active,
            snapshot,
            projection,
            window,
            root_failed,
        } = *prepared;
        *observation = active;
        self.snapshot = *snapshot;
        self.projection = *projection;
        // A view read for what is still shown replaces it. One shown since, or kept
        // through the loss, missed updates: it is read again.
        let same = |read: &model::Window, shown: &model::Window| {
            (&read.agent, read.tab) == (&shown.agent, shown.tab)
        };
        self.window = match (window, self.window.take()) {
            (Some(read), Some(shown)) if same(&read, &shown) => Some(*read),
            (_, Some(shown)) => {
                let focus = self.focus_of_view();
                let id = super::pages::window_id();
                Some(model::Window::new(shown.agent, shown.tab, id, focus))
            }
            (_, None) => None,
        };
        self.root_failed |= root_failed;
        // Notices of output written while lagging are lost with the updates.
        self.outputs.changed_all();
        self.hold_after_root_failure();
        self.refresh();
        self.load_title();
    }
}

#[cfg(test)]
mod tests {
    use super::super::played::*;
    use super::super::tests::*;
    use super::*;

    fn replace_receiver(app: &mut App, receiver: broadcast::Receiver<ObservedEvent>) {
        let Phase::Open { observation, .. } = &mut app.phase else {
            panic!("fixture has a session");
        };
        observation.link = Link::Live(receiver);
    }

    /// Resubscribe as a lagged receiver does, and install what the work it sends reads.
    async fn resubscribe(app: &mut App) {
        let mut work = capture_work(app);
        app.resubscribe();
        assert!(
            app.try_recv_observation().is_err(),
            "nothing is read meanwhile"
        );
        let observed = crate::tests::bounded(work.recv()).await.unwrap();
        assert!(matches!(observed, Work::Observed(Ok(_))));
        app.work(observed);
    }

    /// Whether the app's conversation shows the status `text`.
    fn has_status(app: &mut App, text: &str) -> bool {
        app.content_dirty = true;
        app.rebuild_content();
        let status = format!("Status · {text}");
        app.entries().iter().any(|entry| entry.text() == status)
    }

    #[tokio::test]
    async fn stopping_keeps_an_interrupted_reply_at_its_journal_position() {
        use skyhook::provider::protocol::ItemKind;
        let (_root, mut app, mut model, session) = played_fixture().await;
        let agent = app.selected.clone();
        let _asked = ask(&session, "question");
        let turn = model.turn().await;
        turn.delta("text", ItemKind::Text, "cut short");
        let shown = |app: &App| {
            app.entries()
                .iter()
                .any(|entry| entry.text().contains("cut short"))
        };
        observe_until(&mut app, shown).await;
        // The interruption ends the live reply before stopping settles it.
        session.interrupt().await;
        observe_until(&mut app, |app| stopped(&app.snapshot, &app.selected)).await;
        let request = latest_request(&app.journal).unwrap();
        assert!(
            app.snapshot.views.responses[&(agent, request)]
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
        assert!(futures_util::poll!(std::pin::pin!(app.recv_observation())).is_pending());
        session
            .record_status(root.clone(), "while closed".into())
            .await
            .unwrap();
        resubscribe(&mut app).await;
        assert!(has_status(&mut app, "while closed"));
        session
            .record_status(root, "after subscription".into())
            .await
            .unwrap();
        let event = crate::tests::bounded(app.recv_observation()).await.unwrap();
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
        resubscribe(&mut app).await;
        assert!(has_status(&mut app, "after subscription"));
        let empty = app.try_recv_observation();
        assert!(matches!(empty, Err(broadcast::error::TryRecvError::Empty)));
        session.shutdown().await.unwrap();
    }
}
