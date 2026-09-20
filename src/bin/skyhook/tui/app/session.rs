use super::*;

/// What the active session asks of the host that owns every open session.
pub enum HostRequest {
    New,
    Open(SessionId),
    Activate(u64),
    Quit,
}

/// One open session as the others see it.
#[derive(Clone, PartialEq)]
pub struct Peer {
    pub key: u64,
    pub session: Option<SessionId>,
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
            for warning in session.warnings() {
                self.notice(format!("Startup warning: {warning}"));
            }
            // Connection diagnostics belong only to this host installation, not
            // the journal or the agent's context. Reuse the UI-only status tail
            // rather than notice(), which persists statuses for active sessions.
            self.unsaved_status
                .extend(session.startup_warnings().iter().map(|warning| {
                    (
                        session.root_agent().clone(),
                        format!("Startup warning: {warning}"),
                    )
                }));
            if !session.startup_warnings().is_empty() {
                self.dirty = true;
                self.invalidate_content();
            }
        }
    }
    pub(super) fn set_title(&self, title: &str) {
        let Some(session) = self.session().cloned() else {
            return;
        };
        let title = crate::tui::format::brief(title, 100);
        let notices = self.root_notifier();
        tokio::spawn(async move {
            if let Err(e) = session.set_title(title).await {
                notices.send(format!("Could not save session title: {e}"));
            }
        });
    }
    /// A draft nobody has typed into: replaced rather than kept alongside.
    pub fn untouched(&self) -> bool {
        self.session().is_none()
            && matches!(self.start, StartState::Idle)
            && self.editor.is_empty()
            && self.queue.is_empty()
    }
    /// A notice about the interface itself: shown here, never journaled.
    pub fn local_notice(&mut self, message: impl Into<String>) {
        self.unsaved_status
            .push((self.selected.clone(), message.into()));
        self.dirty = true;
        self.invalidate_content();
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
            sidebar: self.sidebar,
        };
        let mut app = Self::new(observation, launch, saved, tx);
        if draft {
            app.model.clone_from(&self.model);
        }
        app
    }
    pub fn peer(&self, key: u64, current: bool) -> Peer {
        let root = self.root_agent();
        let agent = self.projection.agents.iter().find(|a| &a.id == root);
        Peer {
            key,
            session: self.session_id(),
            current,
            state: agent.map_or(model::AgentDisplayState::Ready, |a| self.agent_status(a)),
            attention: !self.prompts.is_empty()
                || matches!(
                    self.snapshot.activity.get(root),
                    Some(AgentActivity::Failed(_))
                ),
            working: !self.stopping && self.active_work(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::*;
    #[tokio::test]
    async fn reconnecting_keeps_input_delivery_busy_until_interrupted() {
        let (_root, mut app) = draft_fixture().await;
        let agent = app.root_agent().clone();
        app.snapshot
            .activity
            .insert(agent.clone(), AgentActivity::Reconnecting { attempt: 2 });
        assert!(app.busy());
        app.snapshot
            .activity
            .insert(agent, AgentActivity::Interrupted);
        assert!(!app.busy());
    }
}
