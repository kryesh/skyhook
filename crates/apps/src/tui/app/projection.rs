use super::queue::RowState;
use super::{App, Phase, Stage};
use crate::tui::model;
use crate::tui::model::{Entry, View};
use skyhook::identity::AgentId;
use skyhook::interaction::PromptKind;
use skyhook::session::{Message, ObservedEvent, RuntimeEvent, SessionEvent};
use skyhook_worker::job::JobId;
use std::time::Instant;

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
        // While what the app shows is read again, its history waits for the updates;
        // the live slice, observed again already, follows them, so what streams shows.
        if self.held().is_some() {
            if let RuntimeEvent::ResponseChanged { agent, request } = &event.event {
                self.changed_response(agent, *request);
            }
            self.apply_live(event.clone());
            self.content_cache.observe(&event.event);
            self.dirty = true;
            self.content_dirty = true;
            self.held().expect("held while reading").push(event);
            return false;
        }
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
        // A response that changed is read as it streamed so far: a read may run ahead
        // of the updates still queued, which each fold of their own after it.
        if let RuntimeEvent::ResponseChanged { agent, request } = &event.event {
            self.changed_response(agent, *request);
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
            RuntimeEvent::ResponseChanged { agent, .. }
            | RuntimeEvent::ResponseSettled { agent, .. } => {
                (false, agent == &self.selected, agent == &self.selected)
            }
            RuntimeEvent::Activity { agent, .. } => (false, true, agent == &self.selected),
            RuntimeEvent::TurnCompleted { .. } => (false, true, false),
            RuntimeEvent::OutputChanged { job, .. } => (false, self.outputs.changed(*job), false),
            RuntimeEvent::Phase(_) => (false, true, true),
        };
        let applied = self.apply_live(event);
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
    /// Read a changed response whose conversation is presented; another is read once it
    /// is, the session telling of it no more until then.
    fn changed_response(&mut self, agent: &AgentId, request: skyhook::session::RequestSeq) {
        if self.presents(agent) {
            self.read_response(agent, request);
        } else {
            self.unread_responses.insert((agent.clone(), request));
        }
    }

    /// Whether `agent`'s streamed responses are on screen: it is viewed on the Conversation
    /// tab of the transcript rather than a view, and the session is in the foreground,
    /// holding a window. Both a notice and the catch-up read only through this.
    fn presents(&self, agent: &AgentId) -> bool {
        agent == &self.selected
            && matches!(self.panel, super::Shown::Transcript)
            && self.tab == crate::tui::model::Tab::Conversation
            && self.window.is_some()
    }

    /// Read what streamed for the presented conversation while it was not presented.
    pub(crate) fn read_presented_responses(&mut self) {
        let agent = self.selected.clone();
        if !self.presents(&agent) {
            return;
        }
        let unread: Vec<_> = (self.unread_responses.iter())
            .filter(|(owner, _)| owner == &agent)
            .map(|(_, request)| *request)
            .collect();
        if unread.is_empty() {
            return;
        }
        for request in unread {
            self.read_response(&agent, request);
        }
        self.invalidate_content();
    }

    /// Apply `event` to the live slice. A response it lets go of has nothing left to read;
    /// one it holds no record of yet, its notice outrunning the records, stays unread.
    fn apply_live(&mut self, event: ObservedEvent) -> skyhook::session::Applied {
        let responses = &self.snapshot.views.responses;
        let held: Vec<_> = (self.unread_responses.iter())
            .filter(|key| responses.contains_key(*key))
            .cloned()
            .collect();
        let applied = self.snapshot.apply(event);
        let responses = &self.snapshot.views.responses;
        for key in held {
            if !responses.contains_key(&key) {
                self.unread_responses.remove(&key);
            }
        }
        applied
    }

    /// A live slice observed again holds every response as it streamed so far.
    pub(super) fn forget_unread_responses(&mut self) {
        self.unread_responses.clear();
    }

    /// Read `agent`'s `request` as it streamed so far into the live slice. One the
    /// session let go of keeps what the slice holds until its own updates arrive.
    fn read_response(&mut self, agent: &AgentId, request: skyhook::session::RequestSeq) {
        self.unread_responses.remove(&(agent.clone(), request));
        let read = self
            .session()
            .and_then(|session| session.store().response(agent, request));
        if let Some(response) = read {
            (self.snapshot.views.responses).insert((agent.clone(), request), response);
        }
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
        // Updates held for a read that has not landed change nothing yet.
        if !records && self.held().is_some() {
            return;
        }
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
        let shown = self.track_scope();
        if self.content_dirty
            && let Some(window) = &mut self.window
        {
            let panel = self.scope.as_ref().filter(|_| shown);
            let scope = panel.map(|panel| move || panel.entry());
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
                    scope: scope.as_ref().map(|scope| scope as &dyn Fn() -> Entry),
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
        let claimed = match (&self.phase, self.stage) {
            (_, Stage::Stopping | Stage::Stopped) => true,
            (Phase::Draft { start, .. }, Stage::Running) => start.is_some(),
            (Phase::Open { owns_root, .. }, Stage::Running) => *owns_root,
        };
        claimed
            || self.snapshot.revision < self.queue_activity_revision
            || (self.queue.iter()).any(|input| matches!(input.state, RowState::InFlight(_)))
            || self
                .snapshot
                .activity
                .get(self.root_agent())
                .is_some_and(|activity| activity.state.is_busy())
    }
    pub(super) fn active_work(&self) -> bool {
        self.busy() || self.snapshot.open_jobs() || std::sync::Arc::strong_count(&self.tasks) > 1
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
