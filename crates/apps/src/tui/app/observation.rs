use super::events::Work;
use super::{App, Phase};
use crate::tui::model;
use crate::tui::model::{Projection, Tab};
use skyhook::agent::HarnessError;
use skyhook::agent::SessionHandle;
use skyhook::identity::AgentId;
use skyhook::session::Observation;
use skyhook::session::RecordSeq;
use skyhook::session::{ObservationSnapshot, ObservedEvent};
use tokio::sync::broadcast;

/// The attached handle remains authoritative even after its update stream closes.
pub(super) struct ActiveObservation {
    pub(super) session: SessionHandle,
    pub(super) extension: std::sync::Arc<dyn skyhook::product::SessionExtension>,
    link: Link,
}

/// How the app receives the session's updates.
enum Link {
    Live(broadcast::Receiver<ObservedEvent>),
    /// Updates were lost. The session was observed again at `revision`, and what the
    /// app shows of it is being read, after a read that `failed` if one did; until
    /// that lands, the updates after it are `held` as they arrive, so none is lost
    /// again meanwhile. Root failures after `since`, the last record a landed read
    /// checked, are still unchecked: a read superseded or failed leaves them so.
    Reading {
        failed: bool,
        revision: u64,
        since: RecordSeq,
        updates: broadcast::Receiver<ObservedEvent>,
        held: Vec<ObservedEvent>,
    },
    /// Reading it failed: the next tick reads again, from the same unchecked `since`.
    Failed {
        since: RecordSeq,
    },
    /// The session sends no more.
    Closed,
}

/// A snapshot and the receiver obtained from the same subscription boundary, with
/// what the app shows of the session read after it. Neither half can be installed
/// separately by the event loop.
pub struct PreparedObservation {
    pub(super) active: ActiveObservation,
    /// The scope its extension says it runs under, shown frozen when the app opens it.
    pub(crate) scope: Option<skyhook::product::form::ScopeForm>,
    /// What its windows read and keep.
    pub(super) budget: model::Budget,
    pub(super) shown: Shown,
}

/// A snapshot of a session, with what the app shows of it read after it.
pub struct Shown {
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
    pub async fn load(
        session: SessionHandle,
        extension: std::sync::Arc<dyn skyhook::product::SessionExtension>,
        budget: model::Budget,
    ) -> Result<Self, HarnessError> {
        let root = session.root_agent().clone();
        let view = (root, Tab::Conversation, model::Edge::End);
        let Observation { snapshot, updates } = session.store().observe();
        let shown = Shown::read(&session, snapshot, Some(view), None, budget).await?;
        let link = Link::Live(updates);
        let active = ActiveObservation {
            session,
            extension,
            link,
        };
        Ok(Self {
            active,
            scope: None,
            budget,
            shown,
        })
    }

    pub fn session(&self) -> &SessionHandle {
        &self.active.session
    }
}

impl Shown {
    /// What the app shows of `session` after `snapshot`, viewing `agent`'s `tab`
    /// around `focus` if a view is shown, and whether the root failed a turn after
    /// `read`.
    async fn read(
        session: &SessionHandle,
        snapshot: ObservationSnapshot,
        view: Option<(AgentId, Tab, model::Edge)>,
        read: Option<RecordSeq>,
        budget: model::Budget,
    ) -> Result<Self, HarnessError> {
        let agents = session.store().agents().await?;
        let owners = agents
            .agents
            .iter()
            .filter_map(|agent| agent.outline.owner_job);
        let owners = session.jobs().views(owners.collect()).await?;
        let projection = Projection::new(agents, owners, &snapshot, session.host());
        let mut window = None;
        if let Some((agent, tab, focus)) = view {
            let id = super::pages::window_id();
            let mut shown = model::Window::new(agent, tab, id, focus, budget);
            for (direction, want) in shown.plan(&[], focus..=focus) {
                let loaded = super::pages::load(session, &shown.agent, want, budget).await?;
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
            snapshot: Box::new(snapshot),
            projection: Box::new(projection),
            window,
            root_failed,
        })
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
        let Phase::Open { observation, .. } = &mut self.phase else {
            return None;
        };
        match &mut observation.link {
            Link::Live(receiver)
            | Link::Reading {
                updates: receiver, ..
            } => Some(receiver),
            Link::Failed { .. } | Link::Closed => None,
        }
    }

    /// Where updates wait while what the app shows is read again, if it is.
    pub(super) fn held(&mut self) -> Option<&mut Vec<ObservedEvent>> {
        match &mut self.phase {
            Phase::Open {
                observation:
                    ActiveObservation {
                        link: Link::Reading { held, .. },
                        ..
                    },
                ..
            } => Some(held),
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
            && matches!(observation.link, Link::Failed { .. })
        {
            self.resubscribe();
        }
    }

    /// Observe the session again once updates were lost. The new subscription is taken
    /// at once, so its updates drain as they arrive and are held, while what the app
    /// shows of it, its agents and the viewed pages, is read off the event loop.
    pub fn resubscribe(&mut self) {
        let Phase::Open { observation, .. } = &mut self.phase else {
            return;
        };
        let failed = matches!(observation.link, Link::Failed { .. });
        // The live snapshot below is provisional: what a superseded or failed read did
        // not check stays unchecked.
        let since = match observation.link {
            Link::Reading { since, .. } | Link::Failed { since } => since,
            Link::Live(_) | Link::Closed => self.snapshot.applied,
        };
        let Observation { snapshot, updates } = observation.session.store().observe();
        let revision = snapshot.revision;
        observation.link = Link::Reading {
            failed,
            revision,
            since,
            updates,
            held: Vec::new(),
        };
        let (session, tx, budget) = (observation.session.clone(), self.tx.clone(), self.budget);
        // A session out of view reads no view again; one in view reads it around its top.
        let view = (self.window.as_ref())
            .map(|window| (window.agent.clone(), window.tab, self.focus_of_view()));
        // The live slice is the new subscription's from now on, so what streams while the
        // view is read shows; the view's history waits for the read.
        self.snapshot = snapshot.clone();
        self.forget_unread_responses();
        self.content_dirty = true;
        self.dirty = true;
        tokio::spawn(async move {
            let result = Shown::read(&session, snapshot, view, Some(since), budget).await;
            let result = result.map(Box::new);
            let _ = tx.send(Work::Observed { revision, result });
        });
    }

    /// Install what the resubscription at `read` read, if this session still waits for
    /// it, then fold the updates held meanwhile.
    pub(super) fn observed_again(&mut self, read: u64, result: Result<Box<Shown>, HarnessError>) {
        let Phase::Open { observation, .. } = &mut self.phase else {
            return;
        };
        let Link::Reading {
            failed,
            revision,
            since,
            ..
        } = observation.link
        else {
            return;
        };
        // What a superseded resubscription read, or failed to, is older than the
        // updates held.
        if read != revision {
            return;
        }
        let shown = match result {
            Ok(shown) => shown,
            Err(error) => {
                observation.link = Link::Failed { since };
                // A failure repeated as it is read again is reported once.
                if !failed {
                    self.local_notice(error.to_string());
                }
                return;
            }
        };
        let Link::Reading { updates, held, .. } =
            std::mem::replace(&mut observation.link, Link::Closed)
        else {
            unreachable!("matched above")
        };
        observation.link = Link::Live(updates);
        let Shown {
            snapshot,
            projection,
            window,
            root_failed,
        } = *shown;
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
                Some(model::Window::new(
                    shown.agent,
                    shown.tab,
                    id,
                    focus,
                    self.budget,
                ))
            }
            (_, None) => None,
        };
        self.root_failed |= root_failed;
        // Notices of output written while lagging are lost with the updates.
        self.outputs.changed_all();
        self.refresh();
        self.load_title();
        let mut records = false;
        for event in held {
            records |= self.observe(event);
        }
        self.observed(records);
    }
}

#[cfg(test)]
mod tests {
    use super::super::played::*;
    use super::super::tests::*;
    use super::*;
    use crate::tui::app::queue::Paused;
    use crate::tui::app::queue::RowState;
    use skyhook::session::RuntimeEvent;
    use skyhook::session::SessionEvent;
    use std::assert_matches;

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
        let observed = crate::tests::bounded(work.recv()).await.unwrap();
        assert!(matches!(observed, Work::Observed { result: Ok(_), .. }));
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
    async fn closed_receiver_retains_session_and_resubscribe_installs_snapshot_with_updates() {
        let (_root, mut app) = fixture().await;
        let session = app.session().unwrap().clone();
        let root = session.root_agent().clone();
        let (sender, receiver) = broadcast::channel(1);
        replace_receiver(&mut app, receiver);
        drop(sender);
        let closed = app.recv_observation().await;
        assert_matches!(closed, Err(broadcast::error::RecvError::Closed));
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
            .record_status(root.clone(), "after subscription".into())
            .await
            .unwrap();
        let event = crate::tests::bounded(app.recv_observation()).await.unwrap();
        assert_matches!(&event.event, RuntimeEvent::Record(record)
            if matches!(&record.event, SessionEvent::Status { message } if message == "after subscription"));
        // Lag likewise replaces both the snapshot and the receiver.
        let (sender, receiver) = broadcast::channel(1);
        sender.send(event.clone()).unwrap();
        sender.send(event).unwrap();
        replace_receiver(&mut app, receiver);
        let lagged = app.recv_observation().await;
        assert_matches!(lagged, Err(broadcast::error::RecvError::Lagged(_)));
        resubscribe(&mut app).await;
        assert!(has_status(&mut app, "after subscription"));
        let empty = app.try_recv_observation();
        assert_matches!(empty, Err(broadcast::error::TryRecvError::Empty));
        // What arrives while the app reads its view again waits for that read.
        let mut work = capture_work(&mut app);
        app.resubscribe();
        let reading = session.record_status(root, "while reading".into());
        reading.await.unwrap();
        let held = crate::tests::bounded(app.recv_observation()).await.unwrap();
        assert!(!app.observe(held));
        app.work(crate::tests::bounded(work.recv()).await.unwrap());
        assert!(has_status(&mut app, "while reading"));
        session.shutdown().await.unwrap();
    }

    /// A read a later resubscription superseded leaves that one reading, even failed.
    #[tokio::test]
    async fn a_superseded_failed_read_leaves_the_current_one_reading() {
        let (_root, mut app) = fixture().await;
        let session = app.session().unwrap().clone();
        let root = session.root_agent().clone();
        let mut work = capture_work(&mut app);
        app.resubscribe();
        let Work::Observed { revision, .. } = crate::tests::bounded(work.recv()).await.unwrap()
        else {
            panic!("a resubscription reads its view");
        };
        session.record_status(root, "between".into()).await.unwrap();
        let held = crate::tests::bounded(app.recv_observation()).await.unwrap();
        app.observe(held);
        app.resubscribe();
        let result = Err(HarnessError::Interrupted);
        app.work(Work::Observed { revision, result });
        assert!(app.held().is_some());
        app.work(crate::tests::bounded(work.recv()).await.unwrap());
        assert!(app.held().is_none() && has_status(&mut app, "between"));
    }

    /// A root failure the lag hid stays to be checked when the read that would have
    /// found it is superseded: the queue still pauses, holding a later submission.
    #[tokio::test]
    async fn a_superseded_read_leaves_the_root_failure_it_missed_to_the_next() {
        use skyhook::provider::ProviderErrorKind;
        let (_root, mut app, mut model, session) = played_fixture().await;
        let asked = ask(&session, "question");
        model
            .turn()
            .await
            .fail(ProviderErrorKind::InvalidRequest.error("scripted"));
        let _ = crate::tests::bounded(asked).await;
        let mut work = capture_work(&mut app);
        app.resubscribe();
        // An update after the first subscription makes the second a newer one.
        let root = session.root_agent().clone();
        session.record_status(root, "between".into()).await.unwrap();
        let held = crate::tests::bounded(app.recv_observation()).await.unwrap();
        app.observe(held);
        app.resubscribe();
        for _ in 0..2 {
            app.work(crate::tests::bounded(work.recv()).await.unwrap());
        }
        assert!(app.held().is_none(), "the latest read lands");
        assert_eq!(app.paused, Some(Paused::Stopped));
        app.submit("second".into());
        let row = app.queue.iter().find(|row| row.submission.text == "second");
        assert!(matches!(row.unwrap().state, RowState::Waiting));
        session.shutdown().await.unwrap();
    }

    /// While the view is read again, a reply to a request the lagged slice never saw
    /// shows as it streams.
    #[tokio::test]
    async fn a_reply_streams_while_the_view_is_read_again() {
        use skyhook::provider::protocol::ItemKind;
        let (_root, mut app, mut model, session) = played_fixture().await;
        let _work = capture_work(&mut app);
        app.resubscribe();
        let _asked = ask(&session, "question");
        let turn = model.turn().await;
        turn.delta("text", ItemKind::Text, "streams meanwhile");
        observe_until(&mut app, |app| {
            (app.entries().iter()).any(|entry| entry.text().contains("streams meanwhile"))
        })
        .await;
        assert!(app.held().is_some());
    }

    /// Another agent's changed response is read once its conversation is presented, and
    /// one that retires unread leaves nothing to read.
    #[tokio::test]
    async fn a_response_is_read_once_its_conversation_is_presented() {
        use skyhook::provider::protocol::ItemKind;
        let (_root, mut app, mut model, _session) = played_fixture().await;
        let (children, mut turns) = spawn_children(&mut app, &mut model, 2).await;
        // The children's turns arrive in whichever order they were requested.
        let context = skyhook::provider::protocol::ContextId::from(&children[0]);
        if turns[0].context != context {
            turns.swap(0, 1);
        }
        let mut turns = turns.into_iter();
        let (shown, retired) = (turns.next().unwrap(), turns.next().unwrap());
        let unread = |app: &App, child: &AgentId| {
            (app.unread_responses.iter()).any(|(agent, _)| agent == child)
        };
        let read = |app: &App, text: &str| {
            (app.snapshot.views.responses.iter()).any(|((agent, _), response)| {
                agent == &children[0] && response.blocks().iter().any(|b| b.text.contains(text))
            })
        };
        shown.delta("text", ItemKind::Text, "child text");
        observe_until(&mut app, |app| unread(app, &children[0])).await;
        app.select(children[0].clone());
        draw(&mut app);
        assert!(read(&app, "child text") && !unread(&app, &children[0]));
        retired.delta("text", ItemKind::Text, "never shown");
        observe_until(&mut app, |app| unread(app, &children[1])).await;
        retired.answer("never shown");
        observe_until(&mut app, |app| !unread(app, &children[1])).await;
    }
}
