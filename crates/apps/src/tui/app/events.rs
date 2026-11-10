use super::extensions::Shown;
use super::input::InputTarget;
use super::observation::PreparedObservation;
use super::queue::{Delivered, Paused, QueuedInputId};
use super::{App, PASTE_COLLAPSE_LINES, Pane, SCROLL_STEP, Stage, TOAST_TTL, menus, observation};
use crate::tui::commands::Command;
use crate::tui::editor::TextField;
use crate::tui::format;
use crate::tui::model;
use crate::tui::model::{LoadedOutput, OutputAttempt, OutputFailure, Tab, Ticket};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers as M, MouseButton, MouseEventKind,
};
#[cfg(test)]
use ratatui::layout::Rect;
use skyhook::agent::SessionHandle;
use skyhook::attachment::AttachmentError;
use skyhook::identity::{AgentId, SessionId};
use skyhook::job::PresentedOutput;
use skyhook::launch::LaunchError;
use skyhook::session::SessionTitle;
use skyhook_media::Attachment;
use skyhook_worker::job::JobId;

pub(super) async fn load_output(session: &SessionHandle, job: JobId) -> LoadedOutput {
    session
        .inspect_output_with_captures(job)
        .await
        .map(PresentedOutput::into_inspection)
        .map_err(OutputFailure::from)
}

/// Where an unsaved notice was raised: before the session existed, it could not be
/// saved; in the session, it was not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    Draft,
    /// The session could not journal it, or no longer runs.
    Session(SessionId),
}

/// Whether a continuation owns the root's UI operation or leaves it untouched.
pub enum Operation {
    Owned,
    Joined,
}

pub enum Work {
    ExtensionView {
        view: usize,
        /// What the page was read for.
        open: skyhook::product::page::Disclosed,
        result: super::extensions::Loaded,
    },
    QueueCommitted {
        id: QueuedInputId,
        generation: u64,
        revision: u64,
        delivered: Delivered,
    },
    Done {
        result: Result<(), crate::OperationError>,
    },
    Continued {
        operation: Operation,
        result: Result<(), crate::OperationError>,
    },
    Output {
        attempt: OutputAttempt,
        result: Box<LoadedOutput>,
    },
    MenuLoaded(menus::MenuLoaded),
    File {
        draft: Ticket,
        /// Composer offset just after the `@` that opened the file picker.
        at: Option<usize>,
        result: Result<Attachment, AttachmentError>,
    },
    Started {
        result: Result<Box<PreparedObservation>, LaunchError>,
    },
    /// The session observed again after its updates were lost.
    /// What the resubscription at `revision` read.
    Observed {
        revision: u64,
        result: Result<Box<observation::Shown>, skyhook::agent::HarnessError>,
    },
    /// A page of the viewed history, read for the window `window` on `direction`.
    Page {
        window: u64,
        direction: model::Direction,
        result: Result<Box<model::Loaded>, skyhook::agent::HarnessError>,
    },
    /// Interface feedback from a task the app spawned.
    Toast(String),
    /// A save of the preferences finished; a failed one says why.
    PrefsSaved(Result<(), String>),
    /// A notice shown but not journaled, for `agent` of the `owner` it was raised in.
    Unsaved {
        owner: Owner,
        agent: AgentId,
        message: String,
    },
    Interrupted {
        count: usize,
    },
    /// The session activated on the model chosen for its gone root's.
    Activated(Result<(), skyhook::agent::HarnessError>),
    Title {
        ticket: Ticket,
        title: Result<Option<SessionTitle>, skyhook::session::SessionError>,
    },
    Stopped,
    HighlightsReady,
}
/// What the held left button is doing in the transcript.
#[derive(Default)]
pub(super) enum Gesture {
    #[default]
    Released,
    /// Held without moving. Releasing toggles `toggle`'s entry when pressed
    /// on its toggle, and never moves or copies the selection, whose text
    /// may have reflowed under the pointer.
    Pressed { toggle: Option<model::EntryKey> },
    /// Moved while held: the selection follows each motion of the pointer,
    /// and is copied when released.
    Dragging,
    /// Held on the scrollbar: the view follows the pointer down the track.
    Thumb,
    /// Pressed on a page's toggle, which a release on the same toggle applies.
    PressedPage(skyhook::product::page::Toggle),
}

#[derive(Clone)]
pub enum Hit {
    Agent(AgentId),
    /// A conversation entry, and whether clicking it opens or closes it.
    Entry {
        index: usize,
        toggles: bool,
    },
    Menu(usize),
    Tab(Tab),
    ExtensionTab(usize),
    Composer,
    Attachments,
    Attention,
    Sessions,
    SessionTitle,
    Queue,
    PromptChoice(usize),
    /// A row of the draft's scope panel.
    ScopeField(usize),
    Latest,
    /// The transcript's scrollbar track.
    Scrollbar,
    /// What a row of an extension's page opens or closes.
    Page(skyhook::product::page::Toggle),
}
impl App {
    pub fn work(&mut self, work: Work) {
        match work {
            Work::ExtensionView { view, open, result } => self.extension_loaded(view, open, result),
            Work::Started {
                result: Ok(prepared),
            } => self.session_started(*prepared),
            Work::Started { result: Err(error) } => self.start_failed(error),
            Work::Observed { revision, result } => self.observed_again(revision, result),
            Work::Page {
                window,
                direction,
                result,
            } => self.page_loaded(window, direction, result),
            Work::QueueCommitted {
                id,
                generation,
                revision,
                delivered,
            } => self.queue_committed(id, generation, revision, delivered),
            Work::Interrupted { count } => {
                if count > 0 {
                    // Journal the status only for an interruption that happened.
                    self.root_notifier().send("Interrupted");
                } else {
                    // UI-only: nothing was stopped, so nothing is recorded.
                    self.toast("Nothing to interrupt");
                }
            }
            Work::Activated(Ok(())) => {
                self.resume_stopped_queue();
            }
            Work::Activated(Err(error)) => {
                let error = crate::tui::format::harness_error(&error);
                self.notice(format!("Could not switch the session's model: {error}"));
            }
            Work::Done { result }
            | Work::Continued {
                operation: Operation::Owned,
                result,
            } => {
                self.own_root(false);
                if let Err(error) = result {
                    self.report(&error);
                    self.pause_queue(Paused::Stopped);
                }
            }
            Work::Continued {
                operation: Operation::Joined,
                result,
            } => {
                if let Err(error) = result {
                    self.report(&error);
                }
            }
            Work::Output { attempt, result } => {
                if let Some(job) = self.outputs.complete(attempt, *result) {
                    self.content_cache.invalidate_job(job);
                    self.content_dirty = true;
                }
            }
            Work::MenuLoaded(loaded) => {
                if !self.menu_loaded(loaded) {
                    return;
                }
            }
            Work::File { draft, at, result } => {
                if !draft.matches(&self.draft_ticket) {
                    return;
                }
                match result {
                    Ok(attachment) => {
                        // A chosen file consumes the `@` that opened the picker.
                        let at = at.filter(|&at| {
                            at > 0 && self.editor.text().get(at - 1..at) == Some("@")
                        });
                        if let Some(at) = at {
                            self.editor.delete(at - 1..at);
                        }
                        if let Some(file) = attachment.file() {
                            self.toast(format!("Attached {}", file.display()));
                        }
                        self.editor.attach(attachment);
                    }
                    Err(error) => self.toast(error.to_string()),
                }
            }
            Work::Toast(message) => self.toast(message),
            Work::PrefsSaved(Err(message)) => self.toast(message),
            Work::PrefsSaved(Ok(())) => {}
            Work::Unsaved {
                owner,
                agent,
                message,
            } => match owner {
                // The draft's notices move to the session that grew out of it.
                Owner::Draft
                    if agent == *self.root_agent() || self.attached_draft() == Some(&agent) =>
                {
                    self.push_local(self.root_agent().clone(), message);
                }
                Owner::Session(session) if self.session_id() == Some(session) => {
                    self.push_local(agent, message);
                }
                Owner::Draft | Owner::Session(_) => {}
            },
            Work::Title { ticket, title } if ticket.matches(&self.title_ticket) => {
                match title {
                    Ok(title) => self.title = title,
                    Err(error) => {
                        self.local_notice(format!("Could not read session title: {error}"))
                    }
                }
                self.dirty = true;
            }
            Work::Title { .. } => {}
            Work::Stopped => self.stage = Stage::Stopped,
            Work::HighlightsReady => {
                self.render.highlights.poll();
            }
        }
        self.dirty = true;
    }
    /// Report an operation's failure, except a failed journal: the session it
    /// stopped shows that.
    fn report(&self, error: &crate::OperationError) {
        let notice = match error {
            _ if error.journal_failed() => return,
            crate::OperationError::Harness(error) => crate::tui::format::harness_error(error),
            error => error.to_string(),
        };
        self.root_notifier().send(notice);
    }
    pub fn tick(&mut self) {
        self.tick_count = self.tick_count.wrapping_add(1);
        let clock = format::Clock::now();
        // Live counters are painted; only a new day changes entries, dating earlier times.
        if std::mem::replace(&mut self.clock, clock).day() != clock.day() {
            self.content_dirty = true;
            self.dirty = true;
        }
        if self
            .toast
            .as_ref()
            .is_some_and(|(_, t)| t.elapsed() > TOAST_TTL)
        {
            self.toast = None;
            self.dirty = true;
        }
        self.take_prompts(|prompt| prompt.is_closed());
        self.retry_observation();
        self.deliver_queue();
        self.dirty |= self.render.animating || self.extension_due();
        // Retire completed child rows once, rather than redrawing forever while idle.
        self.dirty |= self.projection.retire_completed_grace();
    }
    pub(super) fn fetch_output(&mut self, job: JobId) {
        let Some(session) = self.session().cloned() else {
            return;
        };
        let Some(attempt) = self.outputs.begin(job) else {
            return;
        };
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = load_output(&session, job).await;
            let _ = tx.send(Work::Output {
                attempt,
                result: Box::new(result),
            });
        });
    }
    pub fn event(&mut self, event: Event) {
        if matches!(&event, Event::Key(key) if key.kind == KeyEventKind::Release) {
            return;
        }
        if matches!(&event, Event::Mouse(mouse) if mouse.kind == MouseEventKind::Moved && self.hover == Some((mouse.column, mouse.row)))
        {
            return;
        }
        if let Event::Mouse(mouse) = &event
            && mouse.kind == MouseEventKind::Moved
        {
            if let Some(menu) = &mut self.menu {
                let point = (mouse.column, mouse.row);
                self.hover = Some(point);
                // Palettes own hover while open. Only real pointer movement
                // changes the keyboard selection; drawing never re-applies it.
                if let Some(index) = self
                    .render
                    .hits
                    .iter()
                    .rev()
                    .find_map(|(rect, hit)| match hit {
                        Hit::Menu(index) if rect.contains(point.into()) => Some(*index),
                        _ => None,
                    })
                    && menu.point(index)
                {
                    self.dirty = true;
                }
                return;
            }
            let target = |point: Option<(u16, u16)>| {
                point.and_then(|point| {
                    self.render.hits.iter().position(|(rect, hit)| {
                        rect.contains(point.into())
                            && match hit {
                                Hit::Agent(_) | Hit::Page(_) => true,
                                Hit::Entry { index, .. } => self
                                    .entries()
                                    .get(*index)
                                    .is_some_and(|entry| entry.expandable()),
                                _ => false,
                            }
                    })
                })
            };
            let point = (mouse.column, mouse.row);
            if target(self.hover) == target(Some(point)) {
                self.hover = Some(point);
                return;
            }
        }
        self.dirty = true;
        // A host view has no text to select: only the wheel and its toggles reach it.
        if let Event::Mouse(mouse) = &event
            && matches!(self.panel, Shown::Extension(_))
            && !matches!(
                mouse.kind,
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown | MouseEventKind::Moved
            )
            && !(matches!(
                mouse.kind,
                MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left)
            ) && self.render.hits.iter().any(|(rect, hit)| {
                matches!(hit, Hit::Page(_)) && rect.contains((mouse.column, mouse.row).into())
            }))
            && self
                .render
                .content_rect
                .contains((mouse.column, mouse.row).into())
            && self.menu.is_none()
            && !self.prompt_shown()
        {
            return;
        }
        match event {
            Event::Key(mut key) => {
                key.kind = KeyEventKind::Press;
                key.state = crossterm::event::KeyEventState::NONE;
                self.key(key);
            }
            Event::Paste(text) => {
                let text = format::clean(&text);
                match self.input_target() {
                    InputTarget::Menu => {
                        if let Some(menu) = &mut self.menu {
                            menu.filter(|input| input.insert(&text));
                        }
                    }
                    InputTarget::Prompt if self.prompt_takes_text() => {
                        let outcome = self.prompt_input_mut().editor.insert(&text);
                        if self.multiple_pages() && outcome.text_changed() {
                            self.set_page_editing(true);
                            self.invalidate_page();
                        }
                    }
                    InputTarget::Pane(Pane::Scope) => self.scope_paste(&text),
                    _ if self.composer_input() => {
                        if text.lines().count() > PASTE_COLLAPSE_LINES {
                            self.editor.insert_paste(text);
                        } else {
                            self.editor.insert(&text);
                        }
                        self.composer_scroll = None;
                    }
                    InputTarget::Prompt | InputTarget::Pane(_) => {}
                }
            }
            Event::Mouse(mouse) => {
                let point = (mouse.column, mouse.row);
                match mouse.kind {
                    MouseEventKind::Moved => self.hover = Some(point),
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                        let up = mouse.kind == MouseEventKind::ScrollUp;
                        let step = if up { -SCROLL_STEP } else { SCROLL_STEP };
                        if self.menu.is_some() {
                            let key = if up { KeyCode::Up } else { KeyCode::Down };
                            self.menu_key(KeyEvent::new(key, M::NONE));
                        } else if matches!(self.input_target(), InputTarget::Prompt)
                            && self.render.composer_rect.contains(point.into())
                        {
                            let options = self.render.prompt_options_rect.contains(point.into());
                            let (view, len) = self.prompt_view(options);
                            view.scroll(step, len);
                        } else if self.composer_input()
                            && self.render.composer_rect.contains(point.into())
                            && self.composer_overflows()
                        {
                            self.scroll_composer(step);
                        } else if self.render.tree_rect.contains(point.into()) {
                            let len = self.tree_agents().len();
                            self.tree_view.scroll(step, len);
                        } else {
                            self.scroll(step);
                        }
                    }
                    MouseEventKind::Down(MouseButton::Left) => {
                        self.gesture = Gesture::Pressed { toggle: None };
                        self.selection = None;
                        // An open palette owns the pointer, as it owns hover: the
                        // transcript beneath it is not selectable.
                        let mut select_text = self.menu.is_none();
                        if let Some((_, hit)) = self
                            .render
                            .hits
                            .iter()
                            .rev()
                            .find(|(rect, _)| rect.contains(point.into()))
                            .cloned()
                        {
                            match hit {
                                Hit::Agent(agent) => self.open_agent(agent),
                                Hit::Entry { index, toggles } => {
                                    self.focus = Pane::Content;
                                    self.select_entry(index);
                                    if toggles {
                                        let key = self.entries().get(index);
                                        let toggle = key.map(|entry| entry.key().clone());
                                        self.gesture = Gesture::Pressed { toggle };
                                    }
                                }
                                Hit::Menu(index) => {
                                    if let Some(menu) = self.menu.as_mut() {
                                        menu.point(index);
                                    }
                                    // Only the row clicked is chosen: a text page picks
                                    // nothing, and a stale hit misses.
                                    if self
                                        .menu
                                        .as_ref()
                                        .is_some_and(|menu| menu.selected() == index)
                                    {
                                        self.choose();
                                    }
                                }
                                Hit::Tab(tab) => self.set_tab(tab),
                                Hit::ExtensionTab(view) => self.select_extension(view),
                                Hit::Composer => self.focus = Pane::Composer,
                                Hit::Attachments => self.command(Command::Attachments),
                                Hit::Attention => self.activate_prompt(),
                                Hit::Sessions => self.command(Command::Sessions),
                                Hit::SessionTitle => self.command(Command::Rename),
                                Hit::Queue => self.command(Command::Queue),
                                Hit::PromptChoice(index) => {
                                    // A cancellation can arrive before stale hit geometry is redrawn.
                                    if self.prompts.is_empty() {
                                        return;
                                    }
                                    self.activate_prompt();
                                    if self.prompt_input().choice != index && self.multiple_pages()
                                    {
                                        self.invalidate_page();
                                    }
                                    self.prompt_input_mut().choice = index;
                                    self.set_page_editing(false);
                                    self.reveal_prompt();
                                }
                                Hit::ScopeField(index) => self.focus_scope(Some(index)),
                                Hit::Latest => {
                                    self.scroll_to(None);
                                    // The text-only overlay sits over selectable history.
                                    // Do not pin the viewport again by selecting beneath it.
                                    select_text = false;
                                }
                                Hit::Scrollbar => {
                                    self.gesture = Gesture::Thumb;
                                    self.scroll_to_track(point.1);
                                    select_text = false;
                                }
                                Hit::Page(toggle) => {
                                    self.focus = Pane::Content;
                                    self.gesture = Gesture::PressedPage(toggle);
                                    select_text = false;
                                }
                            }
                        }
                        if select_text && let Some(position) = self.text_position(point) {
                            // Hold the viewport still while selecting a streaming reply.
                            self.scroll_to(Some(self.scroll_offset()));
                            self.focus = Pane::Content;
                            self.selection =
                                crate::tui::render::Selection::new(&self.render.rows, position);
                        }
                    }
                    MouseEventKind::Up(MouseButton::Left) => {
                        match std::mem::take(&mut self.gesture) {
                            // The last drag motion placed the head: the text under the
                            // pointer may have reflowed since. Copied once, on release.
                            Gesture::Dragging if let Some(text) = self.selected_text() => {
                                self.copy_text(text);
                            }
                            Gesture::Pressed { toggle: Some(key) }
                                if let Some((
                                    _,
                                    Hit::Entry {
                                        index,
                                        toggles: true,
                                    },
                                )) = (self.render.hits.iter())
                                    .rev()
                                    .find(|(rect, _)| rect.contains(point.into()))
                                    && (self.entries().get(*index))
                                        .is_some_and(|entry| entry.key() == &key) =>
                            {
                                self.select_entry(*index);
                                self.toggle();
                            }
                            Gesture::PressedPage(toggle)
                                if self.render.hits.iter().rev().any(|(rect, hit)| {
                                    rect.contains(point.into())
                                        && matches!(hit, Hit::Page(at) if *at == toggle)
                                }) =>
                            {
                                self.toggle_page(Some(toggle));
                            }
                            _ => {}
                        }
                    }
                    MouseEventKind::Drag(MouseButton::Left)
                        if matches!(self.gesture, Gesture::Thumb) =>
                    {
                        self.scroll_to_track(point.1);
                    }
                    MouseEventKind::Drag(MouseButton::Left)
                        if self.render.content_rect.contains(point.into())
                            && !matches!(self.gesture, Gesture::Released) =>
                    {
                        self.gesture = Gesture::Dragging;
                        if let Some(head) = self.text_position(point)
                            && let Some(selection) = &mut self.selection
                        {
                            selection.extend(&self.render.rows, head);
                        }
                    }
                    _ => {}
                }
            }
            Event::Resize(..) => {
                self.reveal_prompt();
            }
            _ => {}
        }
    }
    /// Scroll the view's top to where row `y` sits down the scrollbar track, which
    /// spans the whole history; its last row is the end.
    fn scroll_to_track(&mut self, y: u16) {
        let track = self.render.content_rect;
        let last = track.height.saturating_sub(1);
        let along = y.saturating_sub(track.y).min(last);
        let total = crate::tui::render::Track::of(self).total;
        let at = f64::from(along) / f64::from(track.height) * total;
        self.scroll_to_span((along < last).then_some(at));
    }
    pub(super) fn text_position(
        &self,
        point: (u16, u16),
    ) -> Option<crate::tui::render::TextPosition> {
        if !self.render.content_rect.contains(point.into()) {
            return None;
        }
        let row = (self.scroll_offset()
            + point.1.saturating_sub(self.render.content_rect.y) as usize)
            .min(self.render.rows.len().checked_sub(1)?);
        if !self.render.rows[row].selectable {
            return None;
        }
        Some(crate::tui::render::TextPosition {
            row,
            byte: self.render.rows[row].byte_at_column(point.0),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::played::*;
    use super::super::tests::*;
    use super::*;
    use crate::tui::composer::Submission;
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use tokio::sync::mpsc;

    /// A viewed session's notices are shown for the agent they concern, a saved
    /// child included.
    #[tokio::test]
    async fn viewed_child_notices_are_shown() {
        let (_root, mut app, mut model, session) = played_fixture().await;
        let (children, _turns) = spawn_children(&mut app, &mut model, 1).await;
        let child = children[0].clone();
        bounded(session.shutdown()).await.unwrap();
        session.close().await.unwrap();

        let viewed = app
            .create(skyhook::product::Opening::Resume(session.id()))
            .await
            .unwrap()
            .session;
        let prepared = prepared(viewed.clone()).await;
        let (tx, mut work) = mpsc::unbounded_channel();
        app = app.sibling(Some(prepared), app.launch.clone(), app.ui.clone(), tx);
        app.select(child.clone());
        app.notice("about the child");
        bounded(async {
            while unsaved(&app, &child).is_empty() {
                app.work(work.recv().await.expect("work channel is open"));
            }
        })
        .await;
        bounded(viewed.shutdown()).await.unwrap();
        viewed.close().await.unwrap();
        assert_eq!(unsaved(&app, &child), ["about the child"]);
    }

    #[tokio::test]
    async fn attachment_reads_do_not_leak_into_the_next_submission() {
        let (_root, mut app) = draft_fixture().await;
        let text = Attachment::Text {
            file: Some(PathBuf::from("fixture.txt")),
            content: "contents".into(),
        };
        let attachment = |draft| Work::File {
            draft,
            at: None,
            result: Ok(text.clone()),
        };
        let draft = app.draft_ticket.clone();
        app.info("Unrelated overlay", "Still the same draft".into());
        app.work(attachment(draft.clone()));
        assert_eq!(app.editor.attachments(), [text.clone().into()]);
        app.editor.take();

        // Queue without starting a session: even a queued submission consumes its draft.
        app.paused = Some(Paused::Stopped);
        app.submit("submitted".into());
        let stale_then_current = |app: &mut App, stale| {
            app.work(attachment(stale));
            assert!(app.editor.attachments().is_empty());
            app.work(attachment(app.draft_ticket.clone()));
            assert_eq!(app.editor.attachments().len(), 1);
        };
        stale_then_current(&mut app, draft);

        // Tickets follow draft replacement.
        let ticket = app.draft_ticket.clone();
        let queued = app.queued_input(Submission {
            text: "queued replacement".into(),
            attachments: vec![png_attachment("image.png").await],
        });
        app.queue = VecDeque::from([queued]);
        app.command(Command::Queue);
        app.choose();
        assert!(!ticket.matches(&app.draft_ticket));
        app.work(attachment(ticket));
        assert_eq!(
            app.editor.attachments(),
            [png_attachment("image.png").await.into()]
        );
    }

    #[tokio::test]
    async fn stale_prompt_choice_hits_and_resizes_without_a_prompt_are_ignored() {
        let (_root, mut app) = fixture().await;
        let response = question(&mut app, "A question".into(), vec![]);
        let cell = Rect::new(0, 0, 1, 1);
        app.render.hits.push((cell, Hit::PromptChoice(0)));
        drop(response);
        app.tick();
        assert!(app.prompts.is_empty());
        mouse(&mut app, cell, MouseEventKind::Down(MouseButton::Left));
        app.event(Event::Resize(80, 24));
        assert!(app.prompts.is_empty());
    }

    /// Text selected while a reply streams stays selected as its reasoning
    /// collapses, as more text arrives, as the reply settles, and through a
    /// resize: the selection follows its text, not its rows.
    #[tokio::test]
    async fn streamed_selection_survives_settling_and_resize() {
        use skyhook::provider::protocol::{AssistantItem, ItemKind};
        let (_root, mut app, mut model, _session) = played_fixture().await;
        let (thought, text) = (
            "First thought\nSecond thought",
            "Alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo lima",
        );
        let full = format!("{text} mike");
        let asked = ask(app.session().unwrap(), "question");
        let turn = model.turn().await;
        let left = MouseButton::Left;
        let selection = |app: &App| app.selection.as_ref().map(|s| s.text().to_owned());
        // Press on the first cell of `from`, drag `past` cells beyond `to`'s
        // first, and release.
        let drag = |app: &mut App, from: &str, to: &str, past: u16| {
            let screen = draw(app);
            let cell = |word: &str| {
                (screen.lines().enumerate()).find_map(|(y, line)| {
                    let x = line[..line.find(word)?].chars().count();
                    Some(Rect::new(x as u16, y as u16, 1, 1))
                })
            };
            let (start, mut end) = (cell(from).unwrap(), cell(to).unwrap());
            end.x += past;
            mouse(app, start, MouseEventKind::Down(left));
            mouse(app, end, MouseEventKind::Drag(left));
            mouse(app, end, MouseEventKind::Up(left));
        };
        // A title selected while its reasoning streams, spinner and all.
        turn.delta("thinking", ItemKind::Reasoning, thought);
        observe_until(&mut app, |app| streamed(&app.snapshot, thought)).await;
        assert!(
            draw(&mut app).contains("Second thought"),
            "reasoning streams open"
        );
        drag(&mut app, "Reasoning", "Reasoning", 9);
        assert_eq!(app.clipboard.take().as_deref(), Some("Reasoning"));
        turn.delta("text", ItemKind::Text, text);
        observe_until(&mut app, |app| streamed(&app.snapshot, text)).await;
        assert!(
            !draw(&mut app).contains("Second thought"),
            "reasoning collapses"
        );
        assert_eq!(selection(&app).as_deref(), Some("Reasoning"));
        // Then text in the reply, with more streaming after it.
        drag(&mut app, "bravo", "kilo", 0);
        let selected = "bravo charlie delta echo foxtrot golf hotel india juliet ";
        assert_eq!(app.clipboard.take().as_deref(), Some(selected));
        let streaming = app.selection.as_ref().unwrap().rows();
        turn.delta("text", ItemKind::Text, " mike");
        observe_until(&mut app, |app| streamed(&app.snapshot, &full)).await;
        draw(&mut app);
        assert_eq!(selection(&app).as_deref(), Some(selected));
        turn.end(vec![
            AssistantItem::reasoning("thinking", 0, thought, None),
            AssistantItem::text("text", 1, &full),
        ]);
        bounded(asked).await.unwrap().unwrap();
        observe_until(&mut app, |app| count(&app.journal, answered) > 0).await;
        let settled = draw_buffer(&mut app);
        assert_eq!(selection(&app).as_deref(), Some(selected));
        let color = crate::tui::theme::theme().ui.selected;
        // The selected cells of the transcript, in reading order.
        let highlighted = |app: &App, buffer: &ratatui::buffer::Buffer| {
            let area = app.render.content_rect;
            let cells = area.rows().flat_map(|row| row.columns());
            let cells = cells.map(|position| &buffer[position]);
            let selected = cells.filter(|cell| cell.bg == color);
            selected.map(|cell| cell.symbol()).collect::<String>()
        };
        assert_eq!(highlighted(&app, &settled), selected);
        app.event(Event::Resize(40, 24));
        let resized = draw_sized_buffer(&mut app, 40, 24);
        assert_eq!(selection(&app).as_deref(), Some(selected));
        assert_ne!(app.selection.as_ref().unwrap().rows(), streaming);
        assert_eq!(highlighted(&app, &resized), selected);
        assert!(app.clipboard.is_none() && app.toast.is_some());
    }

    /// Releasing a drag copies just the selected text and reports how many
    /// characters were sent; a plain click copies nothing.
    #[tokio::test]
    async fn message_drag_selects_and_copies_only_the_requested_text_including_unicode() {
        use skyhook::provider::protocol::AssistantItem;
        let (_root, mut app, mut model, _session) = played_fixture().await;
        let (ascii, unicode) = ("bravo", "e\u{301}界🙂");
        let text = |first, second| format!("Alpha **{first}** omega\n\nAlpha **{second}** omega");
        let answer = vec![AssistantItem::text("drag", 0, text(unicode, ascii))];
        exchange(&mut app, &mut model, &text(ascii, unicode), answer).await;
        let buffer = draw_buffer(&mut app);
        let rows = (0..24).filter_map(|y| {
            let row = (0..60).map(|x| buffer[(x, y)].symbol()).collect::<String>();
            row.find("Alpha ").map(|x| (x as u16 + 6, y))
        });
        let (user, agent) = (model::Surface::User, model::Surface::Agent);
        let cases = [
            (user, ascii),
            (user, unicode),
            (agent, unicode),
            (agent, ascii),
        ];
        let rows: Vec<_> = rows.collect();
        assert_eq!(rows.len(), cases.len());
        for ((surface, word), (x, y)) in cases.into_iter().zip(rows.iter().copied()) {
            // Both words occupy five cells; drag backwards as well as forwards.
            let (start, end) = if surface == user {
                (x + 5, x)
            } else {
                (x, x + 5)
            };
            let left = MouseButton::Left;
            for (column, kind) in [
                (start, MouseEventKind::Down(left)),
                (end, MouseEventKind::Drag(left)),
                (end, MouseEventKind::Up(left)),
            ] {
                mouse(&mut app, Rect::new(column, y, 1, 1), kind);
            }
            let selected = draw_buffer(&mut app);
            let color = crate::tui::theme::theme().ui.selected;
            // Terminals paint wide characters from their leading cell; Ratatui's
            // backend diff skips their continuation cells.
            let mut column = x;
            while column < x + 5 {
                assert_eq!(selected[(column, y)].bg, color);
                let symbol = selected[(column, y)].symbol();
                column += unicode_width::UnicodeWidthStr::width(symbol).max(1) as u16;
            }
            assert_ne!(selected[(x - 1, y)].bg, color);
            assert_ne!(selected[(x + 5, y)].bg, color);
            // The unicode word is four characters in ten bytes.
            let chars = word.chars().count();
            let toast = format!("Copied {chars} chars to clipboard");
            assert_eq!(app.clipboard.take().as_deref(), Some(word));
            assert_eq!(app.toast.take().map(|(toast, _)| toast), Some(toast));
            chord(&mut app, KeyCode::Char('y'));
            assert_eq!(app.clipboard.take().as_deref(), Some(word));
        }
        app.toast = None;
        app.clipboard = Some("earlier".into());
        let (x, y) = rows.last().copied().unwrap();
        click(&mut app, Rect::new(x, y, 1, 1));
        assert!(app.selection.as_ref().is_some_and(|selection| {
            let (anchor, head) = selection.rows();
            anchor == head
        }));
        assert_eq!(app.clipboard.as_deref(), Some("earlier"));
        assert!(app.toast.is_none());
    }

    /// Pressing, dragging and releasing in the inspector leave its rows where
    /// they are, following or scrolled; the keyboard still reveals the row it
    /// selects.
    #[tokio::test]
    async fn inspector_selection_keeps_the_scroll_offset() {
        let (_root, mut app) = fixture().await;
        for index in 0..40 {
            app.local_notice(format!("Message number {index} with some text"));
        }
        draw(&mut app);
        let left = MouseButton::Left;
        for scroll in [None, Some(5)] {
            app.scroll_to(scroll);
            draw(&mut app);
            let offset = app.scroll_offset();
            let content = app.render.content_rect;
            let (from, to) = (
                Rect {
                    x: content.x + 4,
                    y: content.y + 3,
                    ..content
                },
                Rect {
                    x: content.x + 10,
                    y: content.y + 6,
                    ..content
                },
            );
            for (rect, kind) in [
                (from, MouseEventKind::Down(left)),
                (to, MouseEventKind::Drag(left)),
                (to, MouseEventKind::Up(left)),
            ] {
                mouse(&mut app, rect, kind);
                draw(&mut app);
                assert_eq!(app.scroll_offset(), offset, "{scroll:?} {kind:?}");
            }
            assert!(app.clipboard.take().is_some());
        }
        app.select_entry(0);
        key(&mut app, KeyCode::Down, M::NONE);
        let row = app.selected_entry().unwrap();
        assert!(row > 0);
        assert_eq!(Some(app.scroll_offset()), app.render.rows.entry_start(row));
    }

    /// A drag across a soft wrap spans no source text: releasing it leaves the
    /// clipboard and toast untouched.
    #[tokio::test]
    async fn an_empty_drag_copies_nothing() {
        let (_root, mut app) = fixture().await;
        app.local_notice("word ".repeat(40));
        draw(&mut app);
        let content = app.render.content_rect;
        let (end, start) = (
            Rect::new(content.right() - 2, content.y, 1, 1),
            Rect::new(content.x, content.y + 1, 1, 1),
        );
        app.clipboard = Some("earlier".into());
        let left = MouseButton::Left;
        mouse(&mut app, end, MouseEventKind::Down(left));
        mouse(&mut app, start, MouseEventKind::Drag(left));
        mouse(&mut app, start, MouseEventKind::Up(left));
        assert!(app.selection.as_ref().is_some_and(|selection| {
            let (anchor, head) = selection.rows();
            anchor == head && selection.text().is_empty()
        }));
        assert_eq!(app.clipboard.as_deref(), Some("earlier"));
        assert!(app.toast.is_none());
    }
}
