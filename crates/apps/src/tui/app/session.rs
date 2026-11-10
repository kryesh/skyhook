use super::events::Work;
use super::observation::PreparedObservation;
use super::prompts::UiPrompt;
use super::{App, TITLE_CHARS, scope};
use crate::text::brief;
use crate::tui::model;
use crate::tui::model::Ticket;
use skyhook::agent::SessionHandle;
use skyhook::config::mode::ModeName;
use skyhook::identity::{AgentId, SessionId};
use skyhook::interaction::PromptKind;
use skyhook::interaction::UiInteraction;
use skyhook::launch::Launch;
use skyhook::session::TitleSource;
use skyhook::session::{AgentActivity, TurnFailure};
use std::sync::Arc;
use tokio::sync::mpsc;

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

/// Whether an open session has run here: a reopened one is only viewed until
/// the first call that changes it activates it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Activation {
    Viewed,
    Activated,
}

/// An open session's journal, once one has been observed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeerSession {
    Draft,
    /// Its journal exists, but none of its activity has been observed yet.
    Started {
        id: SessionId,
        activation: Activation,
    },
    /// `last` is its newest observed activity's time, in epoch milliseconds.
    Open {
        id: SessionId,
        last: i64,
        activation: Activation,
    },
}

impl PeerSession {
    pub fn activated(self) -> bool {
        match self {
            Self::Draft => false,
            Self::Started { activation, .. } | Self::Open { activation, .. } => {
                activation == Activation::Activated
            }
        }
    }
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

/// What a session's peer summary is drawn from: it is summarized again only
/// once this changes.
#[derive(PartialEq)]
pub(super) struct PeerKey {
    session: Option<SessionId>,
    revision: u64,
    prompts: Vec<u64>,
    working: bool,
}

impl App {
    pub fn is_view_only(&self) -> bool {
        self.session().is_some_and(SessionHandle::is_view_only)
    }

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
            for warning in session.warnings() {
                self.push_local(session.root_agent().clone(), startup_warning(warning));
            }
        }
    }
    /// What the configuration's loader and the product warned of, then why the saved UI
    /// state could not be read, shown as the session's own warnings are.
    pub fn show_startup(
        &mut self,
        warnings: Vec<String>,
        unreadable: Option<crate::state::Unreadable>,
    ) {
        for warning in warnings {
            self.local_notice(startup_warning(&warning));
        }
        if let Some(unreadable) = unreadable {
            self.local_notice(unreadable.to_string());
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
        // Only the user's own title is worth editing: an automatic one is what an
        // empty answer returns to.
        let current = (self.title.as_ref())
            .filter(|title| title.source == TitleSource::User)
            .map(|title| brief(&title.text, TITLE_CHARS))
            .unwrap_or_default();
        // Shown at once: input already buffered behind the request answers it.
        let (prompt, answer) = self.ui.rename(current);
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
        let ticket = Ticket::default();
        self.title_ticket = ticket.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let title = session.store().title().await;
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
    /// Nothing typed, queued, starting or changed in the scope panel.
    fn unattended(&self) -> bool {
        !self.creating()
            && self.editor.is_empty()
            && self.queue.is_empty()
            && !self.scope.as_ref().is_some_and(scope::ScopePanel::edited)
    }
    /// A notice about the interface itself: shown here, never journaled.
    pub fn local_notice(&mut self, message: impl Into<String>) {
        self.push_local(self.selected.clone(), message.into());
    }
    pub(super) fn push_local(&mut self, agent: AgentId, message: String) {
        let entries = self.snapshot.views.entries.get(&agent).copied();
        let after = entries.and_then(|entries| entries.checked_sub(1));
        (self.unsaved_status).push(model::UnsavedStatus {
            agent,
            after,
            message,
        });
        self.dirty = true;
        self.content_dirty = true;
    }
    /// The modes a message can be sent in.
    pub fn modes(&self) -> &indexmap::IndexMap<ModeName, skyhook::config::mode::Mode> {
        &self.launch.model().config().config().modes
    }

    /// Another session's app, inheriting this one's UI preferences.
    pub fn sibling(
        &self,
        observation: Option<PreparedObservation>,
        launch: Launch,
        ui: Arc<UiInteraction>,
        tx: mpsc::UnboundedSender<Work>,
    ) -> Self {
        let draft = observation.is_none();
        let prefs = self.prefs.clone();
        let mut app = Self::new(observation, launch, ui, Some(&self.mode), prefs, tx);
        // The selection carries over only where the new launch's catalog offers it.
        let offered = app.launch.model().config().select_model(&self.model);
        if draft && offered.is_ok() {
            app.model.clone_from(&self.model);
        }
        app
    }
    /// This session as the others see it from `key`.
    pub fn peer(&mut self, key: SlotKey, current: bool) -> Peer {
        let drawn = PeerKey {
            session: self.session_id(),
            revision: self.snapshot.revision,
            prompts: self.prompts.iter().map(|prompt| prompt.id).collect(),
            working: self.active_work(),
        };
        if let Some((cached, peer)) = &self.peer
            && *cached == drawn
        {
            return Peer {
                key,
                current,
                ..peer.clone()
            };
        }
        let root = self.root_agent();
        let agent = self.projection.agents.iter().find(|a| &a.id == root);
        let activity = self.snapshot.last_activity;
        let activation = match self.snapshot.phase {
            skyhook::session::SessionPhase::Viewing => Activation::Viewed,
            _ => Activation::Activated,
        };
        let peer = Peer {
            key,
            session: match (self.session_id(), activity) {
                (Some(id), Some(last)) => PeerSession::Open {
                    id,
                    last,
                    activation,
                },
                (Some(id), None) => PeerSession::Started { id, activation },
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
        };
        self.peer = Some((drawn, peer.clone()));
        peer
    }
}

fn startup_warning(warning: &impl std::fmt::Display) -> String {
    format!("Startup warning: {warning}")
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::*;
    use crate::tui::app::Pane;
    use crate::tui::app::events::Hit;
    use crate::tui::app::input::InputTarget;
    use crate::tui::commands::Command;
    use crate::tui::editor::TextField;
    use KeyCode::{Char, Enter};
    use crossterm::event::KeyCode;
    use crossterm::event::KeyModifiers as M;

    /// What the configuration's loader and the product warned of at startup shows on the
    /// screen as a startup warning, its controls cleaned, beside the UI state's warning,
    /// and the empty draft still shows the product's placeholder below them.
    #[tokio::test]
    async fn host_startup_warnings_show_on_screen() {
        let (_root, mut app) = draft_fixture().await;
        let unreadable = serde_json::from_str::<()>("x").unwrap_err().into();
        let warning = "/workspace/product.yaml: fixture warning\u{1b}[31m".into();
        app.show_startup(vec![warning], Some(unreadable));
        let screen = draw_sized(&mut app, 120, 24);
        let shown = "Status · Startup warning: /workspace/product.yaml: fixture warning[31m";
        assert!(screen.contains(shown), "{screen}");
        assert!(
            screen.contains("Status · Could not read UI state: "),
            "{screen}"
        );
        assert!(
            screen.contains(crate::tui::theme::look().placeholder),
            "{screen}"
        );
    }

    /// A foreign session shows its recorded state read-only: no composer, title or
    /// mutation commands, while inspection commands and the sidebar's recorded
    /// capabilities stay available. The library refuses what the UI does not block.
    #[tokio::test]
    async fn foreign_sessions_allow_inspection_but_no_composer_or_mutations() {
        use crate::tui::render::sidebar::tests::recorded_capabilities;
        let (_root, mut app) = foreign_fixture().await;
        app.prefs.saved.borrow_mut().sidebar = true;
        assert!(app.is_view_only());
        assert_eq!(app.input_target(), InputTarget::Pane(Pane::Content));
        let screen = draw_sized(&mut app, 120, 30);
        assert!(screen.contains("view-only · "), "{screen}");
        assert!(screen.contains("Foreign response"), "{screen}");
        assert!(screen.contains("Recorded capabilities"), "{screen}");
        assert!(screen.contains("example.review"), "{screen}");
        assert_eq!(
            recorded_capabilities(&app),
            ["read", "example.report_detailed_findings", "example.review"]
        );
        assert_eq!(app.render.composer_rect.height, 0);
        assert!(
            !app.render
                .hits
                .iter()
                .any(|(_, hit)| matches!(hit, Hit::Composer | Hit::SessionTitle))
        );
        assert!(app.focus == Pane::Content);

        let blocked = [
            Command::Rename,
            Command::Model,
            Command::Mode,
            Command::Retry,
            Command::Resume,
            Command::Queue,
            Command::Files,
            Command::Attention,
        ];
        for command in blocked {
            assert!(!app.command_available(command));
            app.command(command);
            assert!(app.menu.is_none() && app.prompts.is_empty());
        }
        for command in [
            Command::Export,
            Command::Copy,
            Command::Agents,
            Command::Sessions,
        ] {
            assert!(app.command_available(command));
        }
        app.event(crossterm::event::Event::Paste("ignored paste".into()));
        press(&mut app, &[Char('x'), Enter, KeyCode::Esc, KeyCode::Tab]);
        assert!(app.editor.is_empty() && app.queue.is_empty());
        assert!(app.focus != Pane::Composer);
        let session = app.session().unwrap().clone();
        session.shutdown().await.unwrap();
        session.close().await.unwrap();
    }

    /// Renaming opens from its key, the palette and the sidebar title, which then
    /// shows the new title; a draft has nothing to rename yet. An open rename
    /// needs no attention from other sessions. Keys typed straight after asking,
    /// before anything else is handled, answer the rename.
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

        chord(&mut app, Char('l'));
        press(&mut app, &"Renamed".chars().map(Char).collect::<Vec<_>>());
        key(&mut app, Enter, M::NONE);
        next_title_change(&mut app).await;
        assert!(app.queue.is_empty() && app.prompts.is_empty());
        bounded(async {
            while app.title.as_ref().map(|title| title.text.as_str()) != Some("Renamed") {
                let work = recv(&mut work).await;
                app.work(work);
            }
        })
        .await;
        let buffer = draw_sized_buffer(&mut app, 120, 24);
        let title = app.render.hits.iter().find_map(|(rect, hit)| match hit {
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
}
