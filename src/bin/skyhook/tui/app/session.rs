use super::*;

impl App {
    pub fn session_id(&self) -> Option<SessionId> {
        self.session.as_ref().map(SessionHandle::id)
    }
    pub fn root_agent(&self) -> &AgentId {
        self.session
            .as_ref()
            .map_or(&self.selected, SessionHandle::root_agent)
    }
    pub(super) fn show_warnings(&mut self) {
        if let Some(session) = &self.session {
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
    pub fn set_session(&mut self, session: Option<SessionHandle>, snapshot: ObservationSnapshot) {
        self.cancel_queue_delivery();
        self.queue_sender = None;
        self.queue_activity_revision = 0;
        self.awaiting_initial_input = false;
        self.attached_draft = None;
        self.session = session;
        self.snapshot = snapshot;
        self.selected = self
            .session
            .as_ref()
            .map(|s| s.root_agent().clone())
            .unwrap_or_else(draft_root);
        self.toast = None;
        self.views.clear();
        self.content_cache = model::ContentCache::default();
        self.render.reset_session();
        self.outputs.clear();
        self.pending_outputs.clear();
        self.final_outputs.clear();
        self.output_versions.clear();
        self.output_queries.clear();
        self.prompts.clear();
        self.suspended_prompt = None;
        self.reset_prompt();
        self.prompt_active = false;
        self.editor = Composer::default();
        self.draft_revision = self.draft_revision.wrapping_add(1);
        self.images.clear();
        self.queue.clear();
        self.switch_restore = None;
        self.creating = false;
        self.pending_start = None;
        self.deferred_switch = None;
        self.paused = false;
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
        let Some(session) = &self.session else {
            return;
        };
        let path = session.directory().join("ui.json");
        let title = crate::tui::format::brief(title, 100);
        if !path.exists() {
            let notices = self.root_notifier();
            tokio::task::spawn_blocking(move || {
                if let Err(e) = state::atomic_write(
                    &path,
                    &serde_json::to_vec(&json!({"title":title})).unwrap_or_default(),
                ) {
                    notices.send(format!("Could not save session title: {e}"));
                }
            });
        }
    }
    pub(super) fn switch(&mut self, id: Option<SessionId>) {
        if self.stopping || self.switch_restore.is_some() {
            return;
        }
        if self.creating {
            self.deferred_switch = Some(id);
            self.paused = true;
            return;
        }
        if id.is_some() && id == self.session_id() {
            self.select(self.root_agent().clone());
            return;
        }
        self.switch_restore = Some(self.paused);
        self.paused = true;
        self.cancel_queue_delivery();
        self.notice(if id.is_some() {
            "Opening session…"
        } else {
            "New session"
        });
        let old = self.session.clone();
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
            if let Err(error) = tx.send(Work::SessionReady(result))
                && let Work::SessionReady(Ok(Some(session))) = error.0
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
        app.snapshot.activity.insert(
            agent.clone(),
            AgentActivity::Reconnecting {
                attempt: 2,
                max_attempts: Some(3),
            },
        );
        assert!(app.busy());
        app.snapshot
            .activity
            .insert(agent, AgentActivity::Interrupted);
        assert!(!app.busy());
    }
}
