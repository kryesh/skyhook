use super::*;
use skyhook::session::TitleSource;

/// One open session's place in the host.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SlotKey(u64);
impl SlotKey {
    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

/// What the active session asks of the host that owns every open session.
pub enum HostRequest {
    New,
    Open(SessionId),
    Activate(SlotKey),
    Quit,
}

/// An open session's journal, once one has been observed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeerSession {
    Draft,
    /// Its journal exists, but none of its activity has been observed yet.
    Started(SessionId),
    /// `last` is its newest observed activity's time, in epoch milliseconds.
    Open {
        id: SessionId,
        last: i64,
    },
}

/// One open session as the others see it.
#[derive(Clone, PartialEq)]
pub struct Peer {
    pub key: SlotKey,
    pub session: PeerSession,
    /// The session on screen.
    pub current: bool,
    pub state: model::AgentDisplayState,
    pub attention: bool,
    pub working: bool,
}

impl App {
    pub fn session_id(&self) -> Option<SessionId> {
        self.session().map(SessionHandle::id)
    }
    pub fn root_agent(&self) -> &AgentId {
        self.session()
            .map_or(&self.selected, SessionHandle::root_agent)
    }
    pub(super) fn show_warnings(&mut self) {
        if let Some(session) = self.session().cloned() {
            // Configuration and connection warnings describe this installation,
            // not the session: shown in the UI-only status tail, never journaled.
            for warning in session.warnings().iter().chain(session.startup_warnings()) {
                let message = format!("Startup warning: {warning}");
                self.push_local(session.root_agent().clone(), message);
            }
        }
    }
    pub(super) fn set_title(&self, title: &str) {
        let Some(session) = self.session().cloned() else {
            return;
        };
        let title = brief(title, TITLE_CHARS);
        let notices = self.root_notifier();
        tokio::spawn(async move {
            if let Err(e) = session.set_title(title).await {
                notices.send(format!("Could not save session title: {e}"));
            }
        });
    }
    /// Ask for the session's new title in a prompt of its own. The answer is
    /// kept as brief as an automatic title; an empty one returns the session to
    /// its automatic title.
    pub(super) fn rename_session(&mut self) {
        let Some(session) = self.session().cloned() else {
            self.toast("Send a message to start the session first");
            return;
        };
        let renaming = |prompt: &UiPrompt| matches!(prompt.kind, PromptKind::Rename { .. });
        if self.prompts.iter().any(renaming) {
            return;
        }
        let Some(ui) = self.launch.interaction.clone() else {
            return;
        };
        // Only the user's own title is worth editing: an automatic one is what an
        // empty answer returns to.
        let current = (self.title.as_ref())
            .filter(|title| title.source == TitleSource::User)
            .map(|title| brief(&title.text, TITLE_CHARS))
            .unwrap_or_default();
        // Shown at once: input already buffered behind the request answers it.
        let (prompt, answer) = ui.rename(current);
        self.prompt(prompt);
        let notices = self.root_notifier();
        tokio::spawn(async move {
            let Ok(answer) = answer.await else {
                return;
            };
            let title = brief(&answer, TITLE_CHARS);
            if let Err(e) = session.rename((!title.is_empty()).then_some(title)).await {
                notices.send(format!("Could not rename session: {e}"));
            }
        });
    }
    /// Read the session's title again; only the newest read is shown.
    pub(super) fn load_title(&mut self) {
        let Some(session) = self.session().cloned() else {
            return;
        };
        let ticket = Token::default();
        self.title_ticket = ticket.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let title = session.title().await.map_err(|error| error.to_string());
            let _ = tx.send(Work::Title { ticket, title });
        });
    }
    /// A draft nobody has typed into: replaced rather than kept alongside.
    pub fn untouched(&self) -> bool {
        self.session().is_none() && self.unattended()
    }
    /// A session with nothing to come back for: closed rather than kept open, so
    /// its lock is free and the session list shows it saved.
    pub fn settled(&self) -> bool {
        self.session().is_some()
            && self.unattended()
            && self.prompts.is_empty()
            && !self.active_work()
    }
    /// Nothing typed, queued or starting.
    fn unattended(&self) -> bool {
        matches!(self.start, StartState::Idle) && self.editor.is_empty() && self.queue.is_empty()
    }
    /// A notice about the interface itself: shown here, never journaled.
    pub fn local_notice(&mut self, message: impl Into<String>) {
        self.push_local(self.selected.clone(), message.into());
    }
    pub(super) fn push_local(&mut self, agent: AgentId, message: String) {
        self.unsaved_status.push((agent, message));
        self.dirty = true;
        self.content_dirty = true;
    }
    /// The modes a message can be sent in: the session's once it exists.
    pub fn modes(&self) -> &indexmap::IndexMap<ModeName, skyhook::tool::policy::Mode> {
        match self.session() {
            Some(session) => session.modes(),
            None => &self.launch.model.config().config().modes,
        }
    }

    /// Another session's app, inheriting this one's UI preferences.
    pub fn sibling(
        &self,
        observation: Option<PreparedObservation>,
        launch: Launch,
        tx: mpsc::UnboundedSender<Work>,
    ) -> Self {
        let draft = observation.is_none();
        let saved = state::SavedState {
            model: self.remembered_model.clone(),
            mode: self.remembered_mode.clone(),
            sidebar: self.sidebar,
        };
        let mut app = Self::new(observation, launch, &self.mode, saved, tx);
        if draft {
            app.model.clone_from(&self.model);
        }
        app
    }
    pub fn peer(&self, key: SlotKey, current: bool) -> Peer {
        let root = self.root_agent();
        let agent = self.projection.agents.iter().find(|a| &a.id == root);
        let mut records = self.snapshot.records.values().rev();
        let activity = records.find(|record| record.event.is_activity());
        Peer {
            key,
            session: match (self.session_id(), activity) {
                (Some(id), Some(record)) => PeerSession::Open {
                    id,
                    last: record.timestamp_millis,
                },
                (Some(id), None) => PeerSession::Started(id),
                (None, _) => PeerSession::Draft,
            },
            current,
            state: agent.map_or(model::AgentDisplayState::Ready, |a| self.agent_status(a)),
            // A rename is the user's own doing, not the session asking.
            attention: (self.prompts.iter())
                .any(|prompt| !matches!(prompt.kind, PromptKind::Rename { .. }))
                || matches!(
                    self.snapshot.activity.get(root).map(|activity| &activity.state),
                    Some(AgentActivity::Stopped(failure)) if *failure != TurnFailure::Interrupted
                ),
            working: self.active_work(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::*;
    use KeyCode::{Char, Enter};

    /// Renaming opens from its key, the palette and the sidebar title, which then
    /// shows the new title; a draft has nothing to rename yet. An open rename
    /// needs no attention from other sessions. Keys typed straight after asking,
    /// before anything else is handled, answer the rename and leave the draft be.
    #[tokio::test]
    async fn rename_opens_from_its_entry_points_and_the_sidebar_shows_the_title() {
        let (_root, mut draft) = draft_fixture().await;
        chord(&mut draft, Char('l'));
        let toast = draft.toast.as_ref().map(|(message, _)| message.as_str());
        assert_eq!(toast, Some("Send a message to start the session first"));
        assert!(draft.prompts.is_empty());

        let (_root, mut app) = fixture().await;
        let mut work = capture_work(&mut app);
        let renaming = |app: &App| {
            matches!(
                app.prompts.front().map(|prompt| &prompt.kind),
                Some(PromptKind::Rename { .. })
            )
        };
        key(&mut app, Char('/'), M::NONE);
        press(&mut app, &"rename".chars().map(Char).collect::<Vec<_>>());
        key(&mut app, Enter, M::NONE);
        assert!(renaming(&app) && app.prompt_shown());
        // Asked for by the user, not the session: nothing for peers to announce.
        assert!(!app.peer(SlotKey::default(), false).attention);
        key(&mut app, KeyCode::Esc, M::NONE);

        app.editor.set("composer draft".into());
        chord(&mut app, Char('l'));
        press(&mut app, &"Renamed".chars().map(Char).collect::<Vec<_>>());
        key(&mut app, Enter, M::NONE);
        next_title_change(&mut app).await;
        assert_eq!(app.editor.text(), "composer draft");
        assert!(app.queue.is_empty() && app.prompts.is_empty());
        bounded(async {
            while app.title.as_ref().map(|title| title.text.as_str()) != Some("Renamed") {
                let work = recv(&mut work).await;
                app.work(work);
            }
        })
        .await;
        let buffer = draw_sized_buffer(&mut app, 120, 24);
        let title = app.hits.iter().find_map(|(rect, hit)| match hit {
            Hit::SessionTitle => Some(*rect),
            _ => None,
        });
        let title = title.expect("the sidebar shows the title");
        let text: String = (title.x..title.right())
            .map(|x| buffer[(x, title.y)].symbol())
            .collect();
        assert_eq!(text.trim_end(), "Renamed");
        click(&mut app, title);
        assert!(renaming(&app));
        assert_eq!(app.prompt_input().editor.text(), "Renamed");
    }

    /// Reopening a session, and closing it, which interrupts its agents, are not
    /// the activity the session list orders by.
    #[tokio::test]
    async fn peer_last_activity_ignores_reopening_and_closing() {
        let (_root, mut app) = fixture().await;
        let last = |app: &App| match app.peer(SlotKey::default(), true).session {
            PeerSession::Open { last, .. } => last,
            session => panic!("{session:?}"),
        };
        // Observed history dates from the epoch, so anything journaled now is later.
        for record in app.snapshot.records.values_mut() {
            record.timestamp_millis = 0;
        }
        push_record(&mut app, SessionEvent::SessionReopened).await;
        push_record(&mut app, SessionEvent::AgentInterrupted).await;
        assert_eq!(last(&app), 0);
        let message = "working".into();
        push_record(&mut app, SessionEvent::Status { message }).await;
        assert!(last(&app) > 0);
    }
}
