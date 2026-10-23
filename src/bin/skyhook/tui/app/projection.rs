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
    pub fn toast(&mut self, message: impl Into<String>) {
        self.toast = Some((message.into(), Instant::now()));
        self.dirty = true;
    }
    pub fn view(&mut self) -> &mut View {
        self.views.get_mut(&self.selected, self.tab)
    }
    pub fn viewed(&self) -> Option<&View> {
        self.views.get(&self.selected, self.tab)
    }
    pub fn observe(&mut self, event: ObservedEvent) -> bool {
        if let RuntimeEvent::Record(record) = &event.event {
            let retitles = match &record.event {
                SessionEvent::TitleSet { .. } | SessionEvent::TitleCleared => true,
                // An untitled session is named after its root's first prompt.
                SessionEvent::MessageCommitted {
                    message: Message::User(_),
                } => self.title.is_none() && record.agent.parent().is_none(),
                _ => false,
            };
            if retitles {
                self.load_title();
            }
        }
        // Response updates replace the live response or a settled one at its
        // journal position; records reach history through the projection.
        self.content_cache.observe(&event.event);
        let job = match &event.event {
            RuntimeEvent::Job(job) => Some(job.id),
            _ => None,
        };
        let record = match &event.event {
            RuntimeEvent::Record(record) => Some(record.clone()),
            _ => None,
        };
        // A failure replaces the session's state with what reopening shows, which no
        // update carries: it is observed again.
        let replaced = matches!(
            &event.event,
            RuntimeEvent::Phase(skyhook::session::SessionPhase::Failed(_))
        );
        let (records, repaint, content) = match &event.event {
            RuntimeEvent::Record(_) => (true, true, true),
            // The record that follows redraws the job's entries.
            RuntimeEvent::Job(_) => (false, false, false),
            // The views it lets go of are kept where they show.
            RuntimeEvent::JobsUnheld(_) => (false, false, false),
            RuntimeEvent::ResponseEvent { agent, .. }
            | RuntimeEvent::ResponseSettled { agent, .. } => {
                (false, agent == &self.selected, agent == &self.selected)
            }
            RuntimeEvent::Activity { agent, .. } => (false, true, agent == &self.selected),
            RuntimeEvent::TurnCompleted { .. } => (false, true, false),
            RuntimeEvent::OutputChanged { job, .. } => (false, self.outputs.changed(*job), false),
            RuntimeEvent::Phase(_) => (false, true, true),
        };
        let applied = self.snapshot.apply(event);
        if let Some(record) = record
            && let Some(ordinal) = applied.entry
        {
            self.observe_record(ordinal, &record, applied.fact);
        }
        self.projection.observe_requests(&applied.requests);
        if let Some((request, superseded)) = &applied.superseded
            && let Some(window) = &mut self.window
            && window.agent == superseded.agent
        {
            window.superseded(*request, superseded);
        }
        let released = self.projection.release(&applied.unheld);
        self.prune(released);
        // A stopped reading interrupts jobs without a job view of its own.
        for &job in job.iter().chain(&applied.stopped.jobs) {
            self.job_changed(job);
        }
        let records = self.projection.observe_stopped(applied.stopped) || records;
        self.dirty |= repaint;
        self.content_dirty |= content;
        if replaced {
            self.resubscribe();
        }
        records
    }
    /// Fold a record the live slice applied, its agent's entry `ordinal`, which the
    /// slice decided `fact` of.
    fn observe_record(
        &mut self,
        ordinal: u64,
        record: &std::sync::Arc<skyhook::session::EventRecord>,
        fact: Option<skyhook::session::Fact>,
    ) {
        self.projection.observe(record, &self.snapshot);
        let turn = self.projection.turn_started(&record.agent);
        if let Some(window) = self
            .window
            .as_mut()
            .filter(|window| window.agent == record.agent)
        {
            window.observe(ordinal, record, fact, turn);
        }
        if &record.agent == self.root_agent()
            && matches!(record.event, SessionEvent::AgentFailed { .. })
        {
            self.root_failed = true;
        }
        #[cfg(test)]
        self.journal.push(record.clone());
    }
    /// Let go of what is kept beside the window for jobs it no longer shows, once it
    /// changed or more was `released`, or for a view at all while none shows: one
    /// shown again builds afresh.
    pub(super) fn prune(&mut self, released: bool) {
        let shown = match &mut self.window {
            Some(window) => window.take_pruned(released),
            None => {
                self.projection.take_changes();
                Some(Default::default())
            }
        };
        let Some(shown) = shown else {
            return;
        };
        self.projection.retain_released(&shown);
        let held = &self.snapshot.jobs;
        self.outputs
            .retain(|job| shown.contains(job) || held.contains_key(job));
    }

    fn job_changed(&mut self, job: JobId) {
        self.dirty |= self.outputs.changed(job);
        self.projection.observe_job(job);
    }

    /// Act once on what a burst of observed updates changed: bring the agents in
    /// line with their owner jobs, then what follows from it.
    pub fn observed(&mut self, records: bool) {
        if records {
            self.projection.reconcile(&self.snapshot);
        }
        self.hold_after_root_failure();
        self.refresh_menu();
        self.deliver_queue();
    }
    pub(super) fn invalidate_content(&mut self) {
        self.content_dirty = true;
        self.content_cache.invalidate();
    }
    pub fn refresh(&mut self) {
        self.projection.reconcile(&self.snapshot);
        self.refresh_menu();
        self.dirty = true;
        self.invalidate_content();
    }
    pub fn rebuild_content(&mut self) {
        self.ensure_window();
        if self.content_dirty
            && let Some(window) = &mut self.window
        {
            let view = self.views.get_mut(&self.selected, self.tab);
            self.content_cache.update(
                &self.snapshot,
                window,
                &mut self.projection,
                model::EntryView {
                    agent: &self.selected,
                    tab: self.tab,
                    view,
                    all_details: self.details,
                    today: self.clock.day(),
                },
                &self.outputs,
                &self.unsaved_status,
            );
            self.content_dirty = false;
        }
    }
    /// The root's turn ended, or is held, interrupted: nothing more to interrupt.
    pub(super) fn root_interrupted(&self) -> bool {
        self.snapshot
            .activity
            .get(self.root_agent())
            .is_some_and(|activity| activity.state.is_retryable())
    }

    pub fn busy(&self) -> bool {
        self.creating.is_some()
            || self.stopping
            || self.operation
            || self.snapshot.revision < self.queue_activity_revision
            || (self.queue.iter()).any(|input| matches!(input.state, RowState::InFlight(_)))
            || self
                .snapshot
                .activity
                .get(self.root_agent())
                .is_some_and(|activity| activity.state.is_busy())
    }
    pub(super) fn active_work(&self) -> bool {
        self.busy()
            || model::open_jobs(&self.snapshot)
            || std::sync::Arc::strong_count(&self.tasks) > 1
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
