//! Every open session: one `App` each, one of them on screen.
use super::{
    Launch,
    app::{App, HostRequest, PreparedObservation, Work},
};
use crate::interaction::{Prompt, UiInteraction};
use crate::launch::LaunchError;
use skyhook::{
    agent::{ObservedEvent, SessionHandle},
    identity::SessionId,
};
use std::sync::Arc;
use tokio::sync::{
    broadcast::error::{RecvError, TryRecvError},
    mpsc,
};

struct Slot {
    key: u64,
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

/// A saved session opened in the background, with the launch that owns its prompts.
pub struct Opened {
    id: SessionId,
    /// The slot that asked, and its composer text then: focus follows only an idle user.
    from: (u64, String),
    launch: Launch,
    prompts: mpsc::UnboundedReceiver<Prompt>,
    result: Result<SessionHandle, LaunchError>,
}
pub enum HostEvent {
    Slot(usize, SlotEvent),
    Opened(Opened),
}

pub struct Host {
    slots: Vec<Slot>,
    active: usize,
    next_key: u64,
    /// Rotates the first slot polled, so a streaming session cannot starve the rest.
    turn: usize,
    quitting: bool,
    opening: Vec<SessionId>,
    opened_tx: mpsc::UnboundedSender<Opened>,
    opened: mpsc::UnboundedReceiver<Opened>,
}

/// A launch whose prompts arrive on their own channel: a prompt then belongs to
/// its session without having to name it.
pub fn with_prompts(mut launch: Launch) -> (Launch, mpsc::UnboundedReceiver<Prompt>) {
    let (interaction, prompts) = UiInteraction::new();
    launch.interaction = Some(Arc::new(interaction));
    (launch, prompts)
}

impl Host {
    pub fn new(
        app: App,
        work: mpsc::UnboundedReceiver<Work>,
        prompts: mpsc::UnboundedReceiver<Prompt>,
    ) -> Self {
        let (opened_tx, opened) = mpsc::unbounded_channel();
        let mut host = Self {
            slots: Vec::new(),
            active: 0,
            next_key: 0,
            turn: 0,
            quitting: false,
            opening: Vec::new(),
            opened_tx,
            opened,
        };
        host.push(app, work, prompts);
        host
    }
    fn key(&self) -> u64 {
        self.slots[self.active].key
    }
    pub fn app(&mut self) -> &mut App {
        &mut self.slots[self.active].app
    }
    pub fn tick(&mut self) {
        self.slots.iter_mut().for_each(|slot| slot.app.tick());
    }
    pub fn quit(&mut self) {
        self.quitting = true;
        self.slots.iter_mut().for_each(|slot| slot.app.shutdown());
    }
    /// Returns the new slot's index.
    fn push(
        &mut self,
        app: App,
        work: mpsc::UnboundedReceiver<Work>,
        prompts: mpsc::UnboundedReceiver<Prompt>,
    ) -> usize {
        self.next_key += 1;
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
        let (launch, prompts) = with_prompts(self.app().launch.clone());
        let (tx, work) = mpsc::unbounded_channel();
        let app = self.app().sibling(None, launch, tx);
        let index = self.push(app, work, prompts);
        self.activate(index);
    }
    fn activate(&mut self, index: usize) {
        let previous = self.active;
        self.active = index;
        if previous != index && previous < self.slots.len() {
            self.slots[index].app.sidebar = self.slots[previous].app.sidebar;
            if self.slots[previous].app.untouched() {
                self.slots.remove(previous);
                self.active -= usize::from(previous < index);
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
            Some(opened) = opened.recv() => HostEvent::Opened(opened),
        }
    }
    pub async fn handle(&mut self, event: HostEvent) {
        match event {
            HostEvent::Slot(index, event) => {
                let app = &mut self.slots[index].app;
                match event {
                    SlotEvent::Observation(Ok(event)) => {
                        let mut records = app.observe(event);
                        // Reduce a burst once instead of rebuilding the projection per token.
                        for _ in 0..255 {
                            match app.try_recv_observation() {
                                Ok(event) => records |= app.observe(event),
                                Err(TryRecvError::Lagged(_)) => {
                                    app.resubscribe().await;
                                    break;
                                }
                                Err(_) => break,
                            }
                        }
                        if records {
                            app.projection.rebuild(&app.snapshot);
                        }
                    }
                    SlotEvent::Observation(Err(RecvError::Lagged(_))) => app.resubscribe().await,
                    SlotEvent::Observation(Err(RecvError::Closed)) => app.close_observation(),
                    SlotEvent::Prompt(prompt) => app.prompt(prompt),
                    SlotEvent::Work(Work::Started {
                        result: Ok(session),
                    }) => app.session_ready(session).await,
                    SlotEvent::Work(work) => app.work(work),
                }
            }
            HostEvent::Opened(opened) => self.opened(opened).await,
        }
    }
    async fn opened(&mut self, opened: Opened) {
        let Opened {
            id,
            from: (key, draft),
            launch,
            prompts,
            result,
        } = opened;
        self.opening.retain(|opening| *opening != id);
        match result {
            Ok(session) if self.quitting => {
                let _ = session.shutdown().await;
            }
            Ok(session) => {
                let observation = PreparedObservation::subscribe(session).await;
                let (tx, work) = mpsc::unbounded_channel();
                let app = self.app().sibling(Some(observation), launch, tx);
                let idle = self.key() == key && self.app().editor.text() == draft;
                let index = self.push(app, work, prompts);
                if idle {
                    self.activate(index);
                } else {
                    self.app().toast("Session opened");
                }
            }
            Err(error) => self.app().local_notice(error.to_string()),
        }
    }
    /// Apply what the last event asked for. Returns false once nothing is left open.
    pub fn settle(&mut self) -> bool {
        match self.app().host.take() {
            Some(HostRequest::New) if !self.app().untouched() => self.draft(),
            Some(HostRequest::New) | None => {}
            Some(HostRequest::Activate(key)) => {
                if let Some(index) = self.slots.iter().position(|slot| slot.key == key) {
                    self.activate(index);
                }
            }
            Some(HostRequest::Open(id)) => self.open(id),
            Some(HostRequest::Quit) => self.quit(),
        }
        if !self.quitting && self.slots.iter().all(|slot| slot.app.exit) {
            // Closing the last session leaves a fresh draft, not an empty screen.
            self.draft();
        }
        let (active, sidebar) = (self.key(), self.app().sidebar);
        self.slots.retain(|slot| !slot.app.exit);
        if self.slots.is_empty() {
            return false;
        }
        match self.slots.iter().position(|slot| slot.key == active) {
            Some(index) => self.active = index,
            None => {
                self.active = self.active.min(self.slots.len() - 1);
                self.app().sidebar = sidebar;
                self.app().dirty = true;
            }
        }
        let current = self.key();
        let peers = self.slots.iter();
        let peers: Vec<_> = peers
            .map(|slot| slot.app.peer(slot.key, slot.key == current))
            .collect();
        let app = self.app();
        if app.peers != peers {
            app.peers = peers;
            app.dirty = true;
        }
        true
    }
    fn open(&mut self, id: SessionId) {
        let live = |slot: &Slot| slot.app.session_id() == Some(id);
        if let Some(index) = self.slots.iter().position(live) {
            return self.activate(index);
        }
        self.app().toast("Opening session…");
        if self.opening.contains(&id) {
            return;
        }
        self.opening.push(id);
        let from = (self.key(), self.app().editor.text().to_owned());
        let (launch, prompts) = with_prompts(self.app().launch.clone());
        let opened = self.opened_tx.clone();
        tokio::spawn(async move {
            let result = launch.create(Some(id)).await;
            let sent = opened.send(Opened {
                id,
                from,
                launch,
                prompts,
                result,
            });
            if let Err(mpsc::error::SendError(Opened {
                result: Ok(session),
                ..
            })) = sent
            {
                let _ = session.shutdown().await;
            }
        });
    }
    /// Shut down every handle still held, waiting for the ones still being opened.
    pub async fn close(mut self) -> Result<(), String> {
        while !self.opening.is_empty()
            && let Some(opened) = self.opened.recv().await
        {
            self.opening.retain(|opening| *opening != opened.id);
            if let Ok(session) = opened.result {
                let _ = session.shutdown().await;
            }
        }
        let mut result = Ok(());
        for mut slot in self.slots {
            slot.work.close();
            while let Ok(work) = slot.work.try_recv() {
                if let Work::Started {
                    result: Ok(session),
                } = work
                {
                    let _ = session.shutdown().await;
                }
            }
            slot.app.status.flush().await;
            if let Some(session) = slot.app.session()
                && let Err(error) = session.shutdown().await
            {
                result = Err(error.to_string());
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::{app::tests::draft_fixture, keys::Command};
    use std::time::Duration;

    async fn host() -> (tempfile::TempDir, Host) {
        let (root, draft) = draft_fixture().await;
        let (launch, prompts) = with_prompts(draft.launch.clone());
        let (tx, work) = mpsc::unbounded_channel();
        let app = draft.sibling(None, launch, tx);
        (root, Host::new(app, work, prompts))
    }
    async fn saved(host: &mut Host) -> SessionId {
        let session = host.app().launch.create(None).await.unwrap();
        session.shutdown().await.unwrap();
        session.id()
    }
    /// Pump events until `done`; false when the host closed or time ran out first.
    async fn drive(host: &mut Host, wait: Duration, done: impl Fn(&Host) -> bool) -> bool {
        tokio::time::timeout(wait, async {
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
        .unwrap_or(false)
    }
    const WAIT: Duration = Duration::from_secs(10);
    fn live(host: &Host, id: SessionId) -> bool {
        host.slots.iter().any(|s| s.app.session_id() == Some(id))
    }
    /// Open `id` as a user would, again if its previous owner had not let go yet.
    async fn open(host: &mut Host, id: SessionId) {
        while !live(host, id) {
            host.app().host = Some(HostRequest::Open(id));
            let settled = |host: &Host| !host.opening.contains(&id);
            assert!(drive(host, WAIT, settled).await);
        }
    }
    fn sessions(host: &Host) -> Vec<Option<SessionId>> {
        host.slots.iter().map(|s| s.app.session_id()).collect()
    }

    #[tokio::test]
    async fn sessions_open_alongside_each_other_and_close_one_at_a_time() {
        let (_root, mut host) = host().await;
        let (first, second) = (saved(&mut host).await, saved(&mut host).await);
        open(&mut host, first).await;
        // The untouched draft made way; an open session does not.
        assert_eq!(sessions(&host), [Some(first)]);
        open(&mut host, second).await;
        assert_eq!(sessions(&host), [Some(first), Some(second)]);
        assert_eq!(host.app().session_id(), Some(second));
        assert_eq!(host.app().peers.len(), 2);
        // Opening a live session again only brings it forward.
        host.app().host = Some(HostRequest::Open(first));
        host.settle();
        assert_eq!(host.app().session_id(), Some(first));
        assert_eq!(host.slots.len(), 2);

        host.app().command(Command::New);
        assert!(
            drive(&mut host, Duration::from_secs(5), |host| host.slots.len()
                == 3)
            .await
        );
        assert!(host.app().untouched());
        host.app().command(Command::New);
        host.settle();
        assert_eq!(host.slots.len(), 3, "an untouched draft is reused");

        let key = host.slots[1].key;
        host.app().host = Some(HostRequest::Activate(key));
        host.settle();
        assert_eq!(sessions(&host), [Some(first), Some(second)]);
        host.app().command(Command::Close);
        let closed = |host: &Host| sessions(host) == [Some(first)];
        assert!(drive(&mut host, WAIT, closed).await);
        // Closing released the session: it can be opened again.
        open(&mut host, second).await;

        host.app().command(Command::Exit);
        assert!(!drive(&mut host, WAIT, |_| false).await);
        assert!(host.slots.is_empty());
        host.close().await.unwrap();
    }

    #[tokio::test]
    async fn closing_the_last_session_leaves_a_draft_and_quit_waits_for_an_open() {
        let (_root, mut host) = host().await;
        let id = saved(&mut host).await;
        open(&mut host, id).await;
        host.app().command(Command::Close);
        let drafted = |host: &Host| sessions(host) == [None];
        assert!(drive(&mut host, WAIT, drafted).await);

        // A session that cannot be opened is reported on screen and changes nothing.
        let missing = SessionId::from_bytes([9; 16]);
        host.app().host = Some(HostRequest::Open(missing));
        let settled = |host: &Host| !host.opening.contains(&missing);
        assert!(drive(&mut host, WAIT, settled).await);
        assert_eq!(sessions(&host), [None]);
        assert!(crate::tui::app::tests::draw(host.app()).contains("Status ·"));

        host.app().host = Some(HostRequest::Open(id));
        host.settle();
        host.app().host = Some(HostRequest::Open(id));
        host.settle();
        assert_eq!(host.opening, [id], "a repeated request opens nothing more");
        host.quit();
        assert!(!drive(&mut host, WAIT, |_| false).await);
        host.close().await.unwrap();
    }

    #[tokio::test]
    async fn a_background_prompt_stays_with_its_session_and_is_announced() {
        let (_root, mut host) = host().await;
        let (first, second) = (saved(&mut host).await, saved(&mut host).await);
        open(&mut host, first).await;
        open(&mut host, second).await;
        let background = host.slots[0].app.session().unwrap().clone();
        let script = tokio::spawn(async move {
            let script = "return await tool.exec({command:['true']});";
            background.run_script(script).await
        });
        let asked = |host: &Host| !host.slots[0].app.prompts.is_empty();
        assert!(drive(&mut host, WAIT, asked).await);
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
        assert!(!drive(&mut host, WAIT, |_| false).await);
        let _ = script.await.unwrap();
        host.close().await.unwrap();
    }
}
