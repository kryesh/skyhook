//! Every open session: one `App` each, one of them on screen.
use super::app::{App, HostRequest, PreparedObservation, SlotKey, Stage, Work};
use skyhook::identity::SessionId;
use skyhook::interaction::{Prompt, UiInteraction};
use skyhook::launch::{Interface, Launch, LaunchError, OpenedSession};
use skyhook::product::{Attendance, Opening};
use skyhook::{provider::model::ModelRef, session::ObservedEvent};
use std::sync::Arc;
use tokio::sync::{
    broadcast::error::{RecvError, TryRecvError},
    mpsc,
};

/// Observation events reduced together before one projection rebuild.
const OBSERVATION_BURST: usize = 255;

struct Slot {
    key: SlotKey,
    app: App,
    work: mpsc::UnboundedReceiver<Work>,
    prompts: mpsc::UnboundedReceiver<Prompt>,
}
pub enum SlotEvent {
    Observation(Result<ObservedEvent, RecvError>),
    Prompt(Prompt),
    Work(Work),
}
impl Slot {
    async fn next(&mut self) -> SlotEvent {
        tokio::select! {
            event = self.app.recv_observation() => SlotEvent::Observation(event),
            Some(prompt) = self.prompts.recv() => SlotEvent::Prompt(prompt),
            Some(work) = self.work.recv() => SlotEvent::Work(work),
        }
    }
}

/// A saved session opened in the background, with its prompt channel.
pub struct Opened {
    id: SessionId,
    /// The slot that asked, and its composer revision then: focus follows only an idle user.
    from: (SlotKey, u64),
    ui: Arc<UiInteraction>,
    prompts: mpsc::UnboundedReceiver<Prompt>,
    result: Result<PreparedObservation, LaunchError>,
}
pub enum HostEvent {
    Slot(usize, SlotEvent),
    Opened(Box<Opened>),
}

pub struct Host {
    /// What a new draft and a saved session open with.
    process: Launch,
    slots: Vec<Slot>,
    active: usize,
    next_key: SlotKey,
    /// Rotates the first slot polled, so a streaming session cannot starve the rest.
    turn: usize,
    quitting: bool,
    /// Peers are summarized on each tick, or at once when the slots change.
    peers_due: bool,
    opening: Vec<SessionId>,
    /// Sessions asked for while a closing slot still holds their lock, with who
    /// asked: opened once that slot is gone.
    waiting: Vec<(SessionId, (SlotKey, u64))>,
    opened_tx: mpsc::UnboundedSender<Opened>,
    opened: mpsc::UnboundedReceiver<Opened>,
}

/// A session opened and observed, off the event loop, its prompts asked through `ui`; a
/// session it cannot observe is shut down.
pub async fn open(
    launch: &Launch,
    ui: &Arc<UiInteraction>,
    opening: Opening<'_>,
    model: Option<&ModelRef>,
) -> Result<PreparedObservation, LaunchError> {
    let interface = Interface::Attended(ui.clone());
    let OpenedSession { session, extension } = launch.create(opening, &interface).await?;
    let result = async {
        let context = launch.context(opening, Attendance::Attended);
        extension.opened(&session, context).await?;
        if let Some(model) = model {
            session.activate_on(model).await?;
        }
        let scope = extension.scope(&session).await?;
        let budget = super::model::Budget::DEFAULT;
        let mut observation = PreparedObservation::load(session.clone(), extension, budget).await?;
        observation.scope = scope;
        Ok::<_, LaunchError>(observation)
    }
    .await;
    if result.is_err() {
        let _ = session.shutdown().await;
    }
    result
}

/// Run `open` off the event loop and hand what it opened to `deliver`; a session
/// `deliver` hands back, as nobody received it, is shut down.
pub fn open_in_background(
    open: impl Future<Output = Result<PreparedObservation, LaunchError>> + Send + 'static,
    deliver: impl FnOnce(Result<PreparedObservation, LaunchError>) -> Option<PreparedObservation>
    + Send
    + 'static,
) {
    tokio::spawn(async move {
        if let Some(unreceived) = deliver(open.await) {
            let _ = unreceived.session().shutdown().await;
        }
    });
}

/// A prompt channel of a session's own: a prompt then belongs to its session without
/// having to name it.
pub fn prompts() -> (Arc<UiInteraction>, mpsc::UnboundedReceiver<Prompt>) {
    let (ui, prompts) = UiInteraction::new();
    (Arc::new(ui), prompts)
}

impl Host {
    pub fn new(
        app: App,
        work: mpsc::UnboundedReceiver<Work>,
        prompts: mpsc::UnboundedReceiver<Prompt>,
    ) -> Self {
        let (opened_tx, opened) = mpsc::unbounded_channel();
        let mut host = Self {
            process: app.launch.clone(),
            slots: Vec::new(),
            active: 0,
            next_key: SlotKey::default(),
            turn: 0,
            quitting: false,
            peers_due: true,
            opening: Vec::new(),
            waiting: Vec::new(),
            opened_tx,
            opened,
        };
        host.push(app, work, prompts);
        host
    }
    fn key(&self) -> SlotKey {
        self.slots[self.active].key
    }
    pub fn app(&mut self) -> &mut App {
        &mut self.slots[self.active].app
    }
    pub fn tick(&mut self) {
        self.slots.iter_mut().for_each(|slot| slot.app.tick());
        self.peers_due = true;
    }
    pub fn quit(&mut self) {
        self.quitting = true;
        self.slots.iter_mut().for_each(|slot| slot.app.shutdown());
    }
    pub fn quitting(&self) -> bool {
        self.quitting
    }
    /// Let every session finish stopping, with nothing left to draw or read input for.
    pub async fn wind_down(&mut self) {
        while self.settle() {
            let event = self.next().await;
            self.handle(event).await;
        }
    }
    /// Returns the new slot's index.
    fn push(
        &mut self,
        app: App,
        work: mpsc::UnboundedReceiver<Work>,
        prompts: mpsc::UnboundedReceiver<Prompt>,
    ) -> usize {
        self.next_key = self.next_key.next();
        self.peers_due = true;
        self.slots.push(Slot {
            key: self.next_key,
            app,
            work,
            prompts,
        });
        self.slots.len() - 1
    }
    /// A draft beside the current session, brought forward.
    fn draft(&mut self) {
        let (ui, prompts) = prompts();
        let (tx, work) = mpsc::unbounded_channel();
        let launch = self.process.clone();
        let app = self.app().sibling(None, launch, ui, tx);
        let index = self.push(app, work, prompts);
        self.activate(index);
    }
    /// A draft a new one replaces rather than joins.
    fn replaceable(&self, index: usize) -> bool {
        let slot = &self.slots[index];
        slot.app.untouched()
    }
    fn activate(&mut self, index: usize) {
        let previous = self.active;
        self.active = index;
        self.peers_due = true;
        if previous != index && previous < self.slots.len() {
            let replaceable = self.replaceable(previous);
            let left = &mut self.slots[previous].app;
            if replaceable {
                self.slots.remove(previous);
                self.active -= usize::from(previous < index);
            } else if left.settled() {
                left.shutdown();
            } else {
                // A session out of view keeps only its summary.
                left.release();
            }
        }
        self.app().dirty = true;
    }

    pub async fn next(&mut self) -> HostEvent {
        let Self {
            slots,
            opened,
            turn,
            ..
        } = self;
        *turn = turn.wrapping_add(1);
        let mut pending: Vec<_> = slots
            .iter_mut()
            .enumerate()
            .map(|(index, slot)| Box::pin(async move { (index, slot.next().await) }))
            .collect();
        let first = *turn % pending.len();
        pending.rotate_left(first);
        tokio::select! {
            ((index, event), ..) = futures_util::future::select_all(pending) => {
                HostEvent::Slot(index, event)
            }
            Some(opened) = opened.recv() => HostEvent::Opened(Box::new(opened)),
        }
    }
    pub async fn handle(&mut self, event: HostEvent) {
        match event {
            HostEvent::Slot(index, event) => {
                let app = &mut self.slots[index].app;
                match event {
                    SlotEvent::Observation(Ok(event)) => {
                        let mut records = app.observe(event);
                        for _ in 0..OBSERVATION_BURST {
                            match app.try_recv_observation() {
                                Ok(event) => records |= app.observe(event),
                                Err(TryRecvError::Lagged(_)) => {
                                    app.resubscribe();
                                    break;
                                }
                                Err(_) => break,
                            }
                        }
                        app.observed(records);
                    }
                    SlotEvent::Observation(Err(RecvError::Lagged(_))) => app.resubscribe(),
                    SlotEvent::Observation(Err(RecvError::Closed)) => app.close_observation(),
                    SlotEvent::Prompt(prompt) => app.prompt(prompt),
                    SlotEvent::Work(work) => app.work(work),
                }
            }
            HostEvent::Opened(opened) => self.opened(*opened).await,
        }
    }
    async fn opened(&mut self, opened: Opened) {
        let Opened {
            id,
            from: (key, revision),
            ui,
            prompts,
            result,
        } = opened;
        self.opening.retain(|opening| *opening != id);
        match result {
            Ok(observation) if self.quitting => {
                let _ = observation.session().shutdown().await;
            }
            Ok(observation) => {
                let (tx, work) = mpsc::unbounded_channel();
                let launch = self.process.clone();
                let mut app = self.app().sibling(Some(observation), launch, ui, tx);
                let idle = self.key() == key && self.app().editor.revision() == revision;
                if !idle {
                    // Opened out of view, it keeps only its summary.
                    app.release();
                }
                let index = self.push(app, work, prompts);
                if idle {
                    self.activate(index);
                } else {
                    self.app().toast("Session opened");
                }
            }
            // Only the toast: the session on screen is not what failed to open.
            Err(error) if error.already_open() => {
                let name = self.app().launch.identity().display_name;
                self.app()
                    .toast(format!("Session is open in another {name} instance"));
            }
            Err(error) => self.app().local_notice(error.to_string()),
        }
    }
    /// Apply what the last event asked for. Returns false once nothing is left open.
    pub fn settle(&mut self) -> bool {
        match self.app().host.take() {
            Some(HostRequest::New) if !self.replaceable(self.active) => self.draft(),
            Some(HostRequest::Activate(key))
                if let Some(index) = (self.slots.iter())
                    .position(|slot| slot.key == key && !slot.app.stopping()) =>
            {
                self.activate(index);
            }
            Some(HostRequest::New | HostRequest::Activate(_)) | None => {}
            Some(HostRequest::Open(id)) => self.open(id),
            Some(HostRequest::Quit) => self.quit(),
        }
        if !self.quitting
            && self.app().stage == Stage::Stopped
            && self.slots.iter().all(|slot| slot.app.stopping())
        {
            // Closing the last session leaves a fresh draft, not an empty screen.
            self.draft();
        }
        let (active, open) = (self.key(), self.slots.len());
        self.slots.retain(|slot| slot.app.stage != Stage::Stopped);
        if self.slots.is_empty() {
            return false;
        }
        self.peers_due |= self.slots.len() != open;
        match self.slots.iter().position(|slot| slot.key == active) {
            Some(index) => self.active = index,
            None => {
                let live = self.slots.iter().position(|slot| !slot.app.stopping());
                self.active = live.unwrap_or_default();
                self.app().dirty = true;
            }
        }
        // Launching reads the active slot, so only once `active` is valid again.
        if !self.quitting {
            let slots = &self.slots;
            let held = |id| slots.iter().any(|slot| slot.app.session_id() == Some(id));
            let released: Vec<_> = (self.waiting.extract_if(.., |(id, _)| !held(*id))).collect();
            for (id, from) in released {
                self.launch(id, from);
            }
        }
        if std::mem::take(&mut self.peers_due) {
            let current = self.key();
            let peers = self.slots.iter_mut().filter(|slot| !slot.app.stopping());
            let peers: Vec<_> = peers
                .map(|slot| slot.app.peer(slot.key, slot.key == current))
                .collect();
            let app = self.app();
            if app.peers != peers {
                app.peers = peers;
                app.dirty = true;
            }
        }
        true
    }
    fn open(&mut self, id: SessionId) {
        let holds = |slot: &Slot| slot.app.session_id() == Some(id);
        let held = self.slots.iter().position(holds);
        if let Some(index) = held
            && !self.slots[index].app.stopping()
        {
            return self.activate(index);
        }
        self.app().toast("Opening session…");
        if self.opening.contains(&id) || self.waiting.iter().any(|(waiting, _)| *waiting == id) {
            return;
        }
        let from = (self.key(), self.app().editor.revision());
        match held {
            Some(_) => self.waiting.push((id, from)),
            None => self.launch(id, from),
        }
    }
    fn launch(&mut self, id: SessionId, from: (SlotKey, u64)) {
        self.opening.push(id);
        let (ui, prompts) = prompts();
        let (launch, asks) = (self.process.clone(), ui.clone());
        let opened = self.opened_tx.clone();
        open_in_background(
            async move { open(&launch, &asks, Opening::Resume(id), None).await },
            move |result| {
                let opened = opened.send(Opened {
                    id,
                    from,
                    ui,
                    prompts,
                    result,
                });
                match opened {
                    Err(mpsc::error::SendError(Opened {
                        result: Ok(observation),
                        ..
                    })) => Some(observation),
                    _ => None,
                }
            },
        );
    }
    /// Shut down every handle still held, waiting for the ones still being opened.
    pub async fn close(mut self) -> Result<(), skyhook::agent::HarnessError> {
        while !self.opening.is_empty()
            && let Some(opened) = self.opened.recv().await
        {
            self.opening.retain(|opening| *opening != opened.id);
            if let Ok(observation) = opened.result {
                let _ = observation.session().shutdown().await;
            }
        }
        let mut result = Ok(());
        for mut slot in self.slots {
            slot.work.close();
            while let Ok(work) = slot.work.try_recv() {
                if let Work::Started {
                    result: Ok(prepared),
                } = work
                {
                    let _ = prepared.session().shutdown().await;
                }
            }
            slot.app.status.flush().await;
            if let Some(session) = slot.app.session()
                && let Err(error) = session.shutdown().await
            {
                result = Err(error);
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::{app::tests::draft_fixture, commands::Command, editor::TextField};
    use skyhook::session::SessionStore;

    async fn host() -> (tempfile::TempDir, Host) {
        let (root, draft) = draft_fixture().await;
        let (ui, prompts) = prompts();
        let (tx, work) = mpsc::unbounded_channel();
        let app = draft.sibling(None, draft.launch.clone(), ui, tx);
        (root, Host::new(app, work, prompts))
    }
    async fn saved(host: &mut Host) -> SessionId {
        let session = host.app().create(Opening::New(None)).await.unwrap().session;
        session.shutdown().await.unwrap();
        session.close().await.unwrap();
        session.id()
    }
    /// Pump events until `done`; false when the host closed first.
    async fn drive(host: &mut Host, done: impl Fn(&Host) -> bool) -> bool {
        crate::tests::bounded(async {
            while host.settle() {
                if done(host) {
                    return true;
                }
                let event = host.next().await;
                host.handle(event).await;
            }
            false
        })
        .await
    }
    fn live(host: &Host, id: SessionId) -> bool {
        host.slots.iter().any(|s| s.app.session_id() == Some(id))
    }
    /// Open `id` as a user would.
    async fn open(host: &mut Host, id: SessionId) {
        host.app().host = Some(HostRequest::Open(id));
        assert!(drive(host, |host| live(host, id)).await);
    }
    fn sessions(host: &Host) -> Vec<Option<SessionId>> {
        host.slots.iter().map(|s| s.app.session_id()).collect()
    }
    /// Choose `id` from the session switcher, which must show it locked.
    async fn choose_locked(host: &mut Host, id: SessionId) {
        use crate::tui::app::{MenuKind, SavedStatus, SessionRef};
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
        host.app().command(Command::Sessions);
        let slot = &mut host.slots[host.active];
        let row = SessionRef::Saved(id, SavedStatus::Locked);
        let row = crate::tests::bounded(async {
            loop {
                let work = slot.work.recv().await.expect("work channel open");
                slot.app.work(work);
                if let Some(MenuKind::Sessions(items)) =
                    slot.app.menu.as_ref().map(|menu| &menu.kind)
                    && !items.is_empty()
                {
                    break items.iter().position(|item| item.value == row);
                }
            }
        })
        .await;
        slot.app
            .menu
            .as_mut()
            .unwrap()
            .select(row.expect("a locked row"));
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        slot.app.event(Event::Key(enter));
    }

    /// A resume that names a model runs the session on it from the start: the
    /// switch is made before anything runs, not left for the next message.
    #[tokio::test]
    async fn a_resume_naming_a_model_switches_to_it_up_front() {
        use crate::{tests::models, tui::app::tests::launch_of};
        let (root, host) = host().await;
        let yaml = models(&["second"]);
        let permissions = skyhook::launch::request::PermissionRequest::Mode(None);
        let product = skyhook::product::Skyhook;
        let launch = launch_of(&product, root.path(), &yaml, permissions).await;
        let ui = host.slots[0].app.ui.clone();
        let session = super::open(&launch, &ui, Opening::New(None), None)
            .await
            .unwrap();
        let session = session.session().clone();
        session.shutdown().await.unwrap();
        session.close().await.unwrap();
        let second: ModelRef = "test/second".parse().unwrap();
        let resume = Opening::Resume(session.id());
        let prepared = super::open(&launch, &ui, resume, Some(&second))
            .await
            .unwrap();
        let session = prepared.session().clone();
        assert!(session.phase().runs());
        let (tx, _) = mpsc::unbounded_channel();
        let app = host.slots[0].app.sibling(Some(prepared), launch, ui, tx);
        assert_eq!(app.model, second);
        session.shutdown().await.unwrap();
    }

    /// Switching away keeps a session only while there is something to come back
    /// to; one left settled is closed, freeing its lock for the saved list.
    #[tokio::test]
    async fn switching_away_closes_only_settled_sessions() {
        let (_root, mut host) = host().await;
        let sessions_root = host.app().launch.sessions().to_owned();
        let (first, second) = (saved(&mut host).await, saved(&mut host).await);
        open(&mut host, first).await;
        // The untouched draft made way.
        assert_eq!(sessions(&host), [Some(first)]);
        // An unsent draft keeps a session open beside the next.
        host.app().editor.set("unsent".into());
        open(&mut host, second).await;
        assert_eq!(sessions(&host), [Some(first), Some(second)]);
        assert_eq!(host.app().session_id(), Some(second));
        assert_eq!(host.app().peers.len(), 2);
        // Opening a live session again only brings it forward, and the settled one
        // left behind is closed.
        host.app().host = Some(HostRequest::Open(first));
        host.settle();
        assert_eq!(host.app().session_id(), Some(first));
        assert!(drive(&mut host, |host| sessions(host) == [Some(first)]).await);
        assert!(!SessionStore::is_open(&sessions_root, second).await.unwrap());

        host.app().command(Command::New);
        assert!(drive(&mut host, |host| host.slots.len() == 2).await);
        assert!(host.app().untouched());
        host.app().command(Command::New);
        host.settle();
        assert_eq!(host.slots.len(), 2, "an untouched draft is reused");
        let key = host.slots[0].key;
        host.app().host = Some(HostRequest::Activate(key));
        host.settle();
        assert_eq!(sessions(&host), [Some(first)]);

        host.app().command(Command::Close);
        assert!(drive(&mut host, |host| sessions(host) == [None]).await);
        // Closing released the session: it can be opened again.
        open(&mut host, first).await;

        host.app().command(Command::Exit);
        assert!(!drive(&mut host, |_| false).await);
        assert!(host.slots.is_empty());
        host.close().await.unwrap();
    }

    /// A session being closed is gone as far as switching goes: choosing it again,
    /// though the switcher shows it locked meanwhile, opens it afresh once the
    /// closing slot lets go of it.
    #[tokio::test]
    async fn a_closing_session_is_reopened_not_brought_back() {
        let (_root, mut host) = host().await;
        let (first, second) = (saved(&mut host).await, saved(&mut host).await);
        open(&mut host, first).await;
        host.app().editor.set("unsent".into());
        open(&mut host, second).await;
        let closing = host.key();
        // Held at its first step, the closing slot's shutdown keeps the lock.
        let (status, release) = crate::tui::status::StatusLog::held();
        host.app().status = status;
        host.app().host = Some(HostRequest::Open(first));
        host.settle();
        assert!(host.slots[1].app.stopping());
        assert_eq!(host.app().peers.len(), 1);
        host.app().host = Some(HostRequest::Activate(closing));
        host.settle();
        assert_eq!(host.app().session_id(), Some(first));
        // Still locked, it is listed as such; choosing it waits.
        choose_locked(&mut host, second).await;
        host.settle();
        assert_eq!(host.app().session_id(), Some(first));
        release.send(()).unwrap();

        let reopened = |host: &Host| host.slots[host.active].app.session_id() == Some(second);
        assert!(drive(&mut host, reopened).await);
        assert_ne!(host.key(), closing);
        assert_eq!(sessions(&host), [Some(first), Some(second)]);
        host.quit();
        assert!(host.quitting());
        crate::tests::bounded(host.wind_down()).await;
        host.close().await.unwrap();
    }

    /// Reopening waits the same when the closing slot sits before the active one, so
    /// removing it shifts the active slot's place.
    #[tokio::test]
    async fn a_closing_session_before_the_active_one_is_reopened() {
        let (_root, mut host) = host().await;
        let (first, second) = (saved(&mut host).await, saved(&mut host).await);
        open(&mut host, first).await;
        host.app().editor.set("unsent".into());
        open(&mut host, second).await;
        host.app().editor.set("unsent".into());
        host.app().host = Some(HostRequest::Activate(host.slots[0].key));
        host.settle();
        // With its draft gone the first session is settled: leaving it closes it.
        host.app().editor.set(String::new());
        host.app().host = Some(HostRequest::Activate(host.slots[1].key));
        host.settle();
        assert!(host.slots[0].app.stopping() && host.active == 1);
        host.app().host = Some(HostRequest::Open(first));
        host.settle();

        let reopened = |host: &Host| host.slots[host.active].app.session_id() == Some(first);
        assert!(drive(&mut host, reopened).await);
        assert_eq!(sessions(&host), [Some(second), Some(first)]);
        host.quit();
        assert!(host.quitting());
        crate::tests::bounded(host.wind_down()).await;
        host.close().await.unwrap();
    }

    #[tokio::test]
    async fn closing_the_last_session_leaves_a_draft_and_quit_waits_for_an_open() {
        let (_root, mut host) = host().await;
        let id = saved(&mut host).await;
        open(&mut host, id).await;
        host.app().command(Command::Close);
        let drafted = |host: &Host| sessions(host) == [None];
        assert!(drive(&mut host, drafted).await);

        // A session that cannot be opened is reported on screen and changes nothing.
        let missing = SessionId::from_bytes([9; 16]);
        host.app().host = Some(HostRequest::Open(missing));
        let settled = |host: &Host| !host.opening.contains(&missing);
        assert!(drive(&mut host, settled).await);
        assert_eq!(sessions(&host), [None]);
        assert!(crate::tui::app::tests::draw(host.app()).contains("Status ·"));

        host.app().host = Some(HostRequest::Open(id));
        host.settle();
        host.app().host = Some(HostRequest::Open(id));
        host.settle();
        assert_eq!(host.opening, [id], "a repeated request opens nothing more");
        host.quit();
        assert!(host.quitting());
        crate::tests::bounded(host.wind_down()).await;
        host.close().await.unwrap();
    }

    /// A session another instance holds fails to open with a toast: the session
    /// on screen keeps its draft.
    #[tokio::test]
    async fn a_session_open_elsewhere_is_toasted() {
        let (_root, mut host) = host().await;
        let id = saved(&mut host).await;
        let sessions_root = host.app().launch.sessions().to_owned();
        let _elsewhere = SessionStore::open(&sessions_root, id, None).await.unwrap();
        host.app().editor.set("unsent".into());
        choose_locked(&mut host, id).await;
        assert!(drive(&mut host, |host| !host.opening.contains(&id)).await);
        assert_eq!(sessions(&host), [None]);
        assert_eq!(host.app().editor.text(), "unsent");
        let toast = host
            .app()
            .toast
            .as_ref()
            .map(|(message, _)| message.as_str());
        assert_eq!(toast, Some("Session is open in another Skyhook instance"));
        host.quit();
        assert!(host.quitting());
        crate::tests::bounded(host.wind_down()).await;
        host.close().await.unwrap();
    }

    #[tokio::test]
    async fn a_background_prompt_stays_with_its_session_and_is_announced() {
        let (_root, mut host) = host().await;
        let (first, second) = (saved(&mut host).await, saved(&mut host).await);
        open(&mut host, first).await;
        // A draft keeps the first session open once the second is on screen.
        host.app().editor.set("unsent".into());
        open(&mut host, second).await;
        let background = host.slots[0].app.session().unwrap().clone();
        let script = tokio::spawn(async move {
            let script = "return await tool.exec({command:['true']});";
            background.run_script(script).await
        });
        let asked = |host: &Host| !host.slots[0].app.prompts.is_empty();
        assert!(drive(&mut host, asked).await);
        host.tick();
        host.settle();
        assert!(host.app().prompts.is_empty());
        let peers = &host.app().peers;
        assert_eq!(
            peers.iter().map(|p| p.attention).collect::<Vec<_>>(),
            [true, false]
        );
        let screen = crate::tui::app::tests::draw(host.app());
        assert!(screen.contains("1 other session(s) need attention"));

        host.quit();
        assert!(!drive(&mut host, |_| false).await);
        let _ = script.await.unwrap();
        host.close().await.unwrap();
    }
}
