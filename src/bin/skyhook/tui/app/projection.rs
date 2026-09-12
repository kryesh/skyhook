use super::*;

impl App {
    pub(super) fn notifier(&self) -> crate::tui::status::StatusSender {
        self.status.sender(self.session.as_ref(), &self.selected)
    }
    pub(super) fn root_notifier(&self) -> crate::tui::status::StatusSender {
        self.status.sender(self.session.as_ref(), self.root_agent())
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
        if let RuntimeEvent::Record(record) = &event.event
            && &record.agent == self.root_agent()
            && record.sequence > self.initial_input_after
            && matches!(
                &record.event,
                SessionEvent::MessageCommitted {
                    message: skyhook::provider::protocol::Message::User(_)
                }
            )
        {
            self.awaiting_initial_input = false;
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
            let changes = self.content_cache.update(
                &mut self.entries,
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
            );
            self.render.content_changed(changes);
            // Unsaved statuses are an exceptional external tail. A conservative
            // model rebuild can drop it even if entry ordering stays compatible.
            for (index, (agent, message)) in self.unsaved_status.iter().enumerate() {
                if agent != &self.selected {
                    continue;
                }
                let key = format!("unsaved-status-{index}");
                if self
                    .entries
                    .iter()
                    .rev()
                    .take(self.unsaved_status.len())
                    .any(|entry| entry.key == key)
                {
                    continue;
                }
                self.render.changes.dirty.push(self.entries.len());
                self.entries.push(Entry {
                    key,
                    text: format!("Status · {message}"),
                    surface: model::Surface::Status,
                    expandable: false,
                    default_open: false,
                    running: false,
                    footer: None,
                    request: None,
                    indent: 0,
                    job: None,
                    compact_after: false,
                    header: None,
                    document: None,
                });
            }
            self.content_dirty = false;
        }
    }
    pub fn busy(&self) -> bool {
        self.switch_restore.is_some()
            || self.creating
            || self.stopping
            || self.operation
            || self.snapshot.revision < self.queue_activity_revision
            || self.queue.iter().any(|input| input.delivery.is_some())
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
    pub fn agent_status(&self, agent: &model::AgentInfo) -> (bool, String) {
        let pending = self.prompts.iter().find(|prompt| match &prompt.kind {
            PromptKind::Approval(request) => request.agent == agent.id,
            PromptKind::Questions { agent: owner, .. } => owner == &agent.id,
            _ => false,
        });
        match pending.map(|prompt| &prompt.kind) {
            Some(PromptKind::Approval(_)) => (false, "Waiting for permission".into()),
            Some(_) => (false, "Waiting for user input".into()),
            None => self.projection.status(agent, &self.snapshot),
        }
    }
}
