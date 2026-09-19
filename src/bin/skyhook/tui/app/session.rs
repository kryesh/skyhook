use super::*;

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
    pub(super) fn set_session(&mut self, observation: Option<PreparedObservation>) {
        self.cancel_queue_delivery();
        self.queue_sender = None;
        self.queue_activity_revision = 0;
        self.initial_input = None;
        self.attached_draft = None;
        self.install_observation(observation);
        self.selected = self
            .session()
            .map(|s| s.root_agent().clone())
            .unwrap_or_else(draft_root);
        self.toast = None;
        self.views.clear();
        self.content_cache = model::ContentCache::default();
        self.render.reset_session();
        self.outputs.clear();
        self.prompts.clear();
        self.reset_prompt();
        self.prompt_active = false;
        self.reset_session_draft();
        self.switching = None;
        self.start = StartState::Idle;
        self.deferred_switch = None;
        self.paused = !self.queue.is_empty();
        self.operation = false;
        self.menu = None;
        self.unsaved_status.clear();
        self.tree_cursor = 0;
        self.tree_scroll = 0;
        self.selection = None;
        self.reset_projection();
        if let Some(root) = self
            .projection
            .agents
            .iter()
            .find(|a| a.id == self.selected)
        {
            self.model.clone_from(&root.model);
        }
        self.show_warnings();
        if self.stopping {
            self.finish_shutdown();
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
    /// Invariant: at most one creation or switch is in flight. A switch never
    /// starts while another switch or a creation is pending (the latter defers
    /// it), so a `SessionReady` completion is always the current one.
    pub(super) fn switch(&mut self, id: Option<SessionId>) {
        if self.stopping || self.switching.is_some() {
            return;
        }
        if self.start.is_creating() {
            self.deferred_switch = Some(id);
            self.paused = true;
            return;
        }
        if id.is_some() && id == self.session_id() {
            self.select(self.root_agent().clone());
            return;
        }
        let was_paused = self.paused;
        self.paused = true;
        self.cancel_queue_delivery();
        self.switching = Some(was_paused);
        self.notice(if id.is_some() {
            "Opening session…"
        } else {
            "New session"
        });
        let old = self.session().cloned();
        let launch = self.launch.clone();
        let tx = self.tx.clone();
        let status = self.status.clone();
        tokio::spawn(async move {
            status.flush().await;
            let result = async {
                let destination = match id {
                    Some(id) => Some(launch.create(Some(id)).await?),
                    None => None,
                };
                if let Some(old) = old
                    && let Err(error) = old.shutdown().await
                {
                    if let Some(destination) = destination {
                        let _ = destination.shutdown().await;
                    }
                    return Err(error.to_string());
                }
                Ok(destination)
            }
            .await;
            if let Err(error) = tx.send(Work::SessionReady { result })
                && let Work::SessionReady {
                    result: Ok(Some(session)),
                    ..
                } = error.0
            {
                let _ = session.shutdown().await;
            }
        });
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
