use super::*;

impl App {
    pub(super) fn notifier(&self) -> crate::tui::status::StatusSender {
        self.status.sender(self.session(), &self.selected)
    }
    pub(super) fn root_notifier(&self) -> crate::tui::status::StatusSender {
        self.status.sender(self.session(), self.root_agent())
    }
    pub fn notice(&self, message: impl Into<String>) {
        self.notifier().send(message);
    }
    /// Transient UI feedback; never sent to the status log or conversation.
    pub(super) fn toast(&mut self, message: impl Into<String>) {
        self.toast = Some((message.into(), Instant::now()));
        self.dirty = true;
    }
    pub fn view(&mut self) -> &mut View {
        self.views.entry(self.selected.clone()).or_default()
    }
    pub fn observe(&mut self, event: ObservedEvent) -> bool {
        if let RuntimeEvent::Record(record) = &event.event {
            observe_initial_input(&mut self.initial_input, record);
        }
        let (records, repaint, content) = match &event.event {
            RuntimeEvent::Record(_) => (true, true, true),
            RuntimeEvent::ResponseEvent { agent, .. }
            | RuntimeEvent::ResponseSettled { agent, .. } => {
                (false, agent == &self.selected, agent == &self.selected)
            }
            RuntimeEvent::Activity { agent, .. } => (false, true, agent == &self.selected),
            RuntimeEvent::Context { agent, .. } => (false, agent == &self.selected, false),
            RuntimeEvent::TurnCompleted { .. } => (false, true, false),
        };
        // Lifecycle updates can replace the live response, but do not change
        // recorded history. Invalidate that response without rebuilding history.
        let append_only = if let RuntimeEvent::ResponseEvent { agent, request, .. } = &event.event {
            self.content_cache.observe_response(agent, *request);
            true
        } else {
            false
        };
        self.snapshot.apply(event);
        self.deliver_queue();
        self.dirty |= repaint;
        if content {
            if append_only && self.unsaved_status.is_empty() {
                self.content_dirty = true;
            } else {
                self.invalidate_content();
            }
        }
        records
    }
    pub(super) fn invalidate_content(&mut self) {
        self.content_dirty = true;
        self.content_revision = self.content_revision.wrapping_add(1);
    }
    pub fn refresh(&mut self) {
        self.projection.rebuild(&self.snapshot);
        self.dirty = true;
        self.invalidate_content();
    }
    pub fn reset_projection(&mut self) {
        self.projection = Projection::default();
        self.refresh();
    }
    pub fn rebuild_content(&mut self) {
        if self.content_dirty {
            let view = self.views.entry(self.selected.clone()).or_default();
            let overlay = self
                .unsaved_status
                .iter()
                .enumerate()
                .filter(|(_, (agent, _))| agent == &self.selected)
                .map(|(index, (_, message))| {
                    Entry::new(
                        model::EntryKey::UnsavedStatus(index),
                        format!("Status · {message}"),
                        model::Surface::Status,
                    )
                })
                .collect();
            let changes = self.content_cache.update(
                &self.snapshot,
                &self.projection,
                model::EntryView {
                    agent: &self.selected,
                    view,
                    thinking: self.thinking,
                    all_details: self.details,
                },
                &self.outputs,
                self.content_revision,
                overlay,
            );
            self.render.content_changed(changes);
            self.content_dirty = false;
        }
    }
    /// The root's turn ended, or is held, interrupted: nothing more to interrupt.
    pub(super) fn root_interrupted(&self) -> bool {
        matches!(
            self.snapshot.activity.get(self.root_agent()),
            Some(AgentActivity::Interrupted | AgentActivity::Failed(_))
        )
    }

    pub fn busy(&self) -> bool {
        self.switching.is_some()
            || self.start.is_creating()
            || self.stopping
            || self.operation
            || self.snapshot.revision < self.queue_activity_revision
            || self.queue.iter().any(|input| input.in_flight.is_some())
            || matches!(
                self.snapshot.activity.get(self.root_agent()),
                Some(
                    AgentActivity::Working
                        | AgentActivity::Reconnecting { .. }
                        | AgentActivity::Tools
                        | AgentActivity::Compacting
                        | AgentActivity::WaitingChildren
                )
            )
    }
    pub(super) fn active_work(&self) -> bool {
        self.busy()
            || self
                .projection
                .jobs
                .values()
                .any(|j| !j.state.is_terminal())
    }
    pub fn agent_status(&self, agent: &model::AgentInfo) -> model::AgentDisplayState {
        let pending = self.prompts.iter().find(|prompt| match &prompt.kind {
            PromptKind::Approval { request, .. } => request.agent == agent.id,
            PromptKind::Questions { agent: owner, .. } => owner == &agent.id,
            _ => false,
        });
        match pending.map(|prompt| &prompt.kind) {
            Some(PromptKind::Approval { .. }) => {
                model::AgentDisplayState::Waiting(model::WaitReason::Permission)
            }
            Some(_) => model::AgentDisplayState::Waiting(model::WaitReason::Input),
            None => self.projection.status(agent, &self.snapshot),
        }
    }
}
