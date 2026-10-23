use super::*;
use crate::launch::{AttachmentError, LaunchError};
use skyhook::job::PresentedOutput;

// Query presence, capture selection and preview metadata belong to core.
pub(super) async fn load_output(session: &SessionHandle, query: JobOutputQuery) -> LoadedOutput {
    session
        .inspect_output_with_captures(query)
        .await
        .map(PresentedOutput::into_job_view)
        .map_err(|error| error.to_string())
}

/// Whether a continuation owns the root's UI operation or leaves it untouched.
pub enum Operation {
    Owned,
    Joined,
}

pub enum Work {
    QueueCommitted {
        id: QueuedInputId,
        generation: u64,
        revision: u64,
        delivered: Delivered,
    },
    Done {
        result: Result<(), crate::launch::OperationError>,
    },
    Continued {
        operation: Operation,
        result: Result<(), crate::launch::OperationError>,
    },
    Output {
        attempt: OutputAttempt,
        result: Box<LoadedOutput>,
    },
    MenuLoaded(menus::MenuLoaded),
    File {
        draft: Token,
        /// Composer offset just after the `@` that opened the file picker.
        at: Option<usize>,
        result: Result<Attachment, AttachmentError>,
    },
    Started {
        result: Result<PreparedObservation, LaunchError>,
    },
    /// The session observed again after its updates were lost.
    Observed(Result<Box<PreparedObservation>, skyhook::agent::HarnessError>),
    /// A page of the viewed history, read for the window `window` on `direction`.
    Page {
        window: u64,
        direction: model::Direction,
        result: Result<Box<model::Loaded>, skyhook::agent::HarnessError>,
    },
    /// Interface feedback from a task the app spawned.
    Toast(String),
    /// A notice from before the session existed: shown, never journaled.
    LocalStatus {
        agent: AgentId,
        message: String,
    },
    /// A notice the session did not journal: it could not, or it no longer runs.
    Unsaved {
        session: SessionId,
        agent: AgentId,
        message: String,
    },
    Interrupted {
        count: usize,
    },
    /// The session activated on the model chosen for its gone root's.
    Activated(Result<(), skyhook::agent::HarnessError>),
    Title {
        ticket: Token,
        title: Result<Option<SessionTitle>, String>,
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
}

#[derive(Clone)]
pub enum Hit {
    Agent(AgentId),
    Entry(usize, bool),
    Menu(usize),
    Tab(Tab),
    Composer,
    Attachments,
    Attention,
    Sessions,
    SessionTitle,
    Queue,
    PromptChoice(usize),
    Latest,
    /// The transcript's scrollbar track.
    Scrollbar,
}
impl App {
    pub fn work(&mut self, work: Work) {
        match work {
            Work::Started {
                result: Ok(prepared),
            } => self.session_started(prepared),
            Work::Started { result: Err(error) } => self.start_failed(error),
            Work::Observed(result) => self.observed_again(result),
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
                self.operation = false;
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
            Work::LocalStatus { agent, message } => {
                // The draft's notices move to the session that grew out of it.
                if agent != *self.root_agent() && self.attached_draft() != Some(&agent) {
                    return;
                }
                self.push_local(self.root_agent().clone(), message);
            }
            Work::Unsaved {
                session,
                agent,
                message,
            } => {
                if self.session_id() != Some(session) {
                    return;
                }
                self.push_local(agent, message);
            }
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
            Work::Stopped => self.exit = true,
            Work::HighlightsReady => {
                self.render.highlights.poll();
            }
        }
        self.dirty = true;
    }
    /// Report an operation's failure, except a failed journal: the session it
    /// stopped shows that.
    fn report(&self, error: &crate::launch::OperationError) {
        let notice = match error {
            _ if error.journal_failed() => return,
            crate::launch::OperationError::Harness(error) => {
                crate::tui::format::harness_error(error)
            }
            error => error.to_string(),
        };
        self.root_notifier().send(notice);
    }
    pub fn tick(&mut self) {
        self.tick_count = self.tick_count.wrapping_add(1);
        let clock = model::Clock::now();
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
        self.dirty |= self.animating;
        // Retire completed child rows once, rather than redrawing forever while idle.
        self.dirty |= self.projection.retire_completed_grace();
    }
    pub(super) fn fetch_output(&mut self, job: JobId) {
        let Some(session) = self.session().cloned() else {
            return;
        };
        let Some((attempt, query)) = self.outputs.begin(job) else {
            return;
        };
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = load_output(&session, query).await;
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
                if let Some(index) = self.hits.iter().rev().find_map(|(rect, hit)| match hit {
                    Hit::Menu(index) if rect.contains(point.into()) => Some(*index),
                    _ => None,
                }) && menu.point(index)
                {
                    self.dirty = true;
                }
                return;
            }
            let target = |point: Option<(u16, u16)>| {
                point.and_then(|point| {
                    self.hits.iter().position(|(rect, hit)| {
                        rect.contains(point.into())
                            && match hit {
                                Hit::Agent(_) => true,
                                Hit::Entry(index, _) => self
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
        match event {
            Event::Key(mut key) if key.kind != KeyEventKind::Release => {
                key.kind = KeyEventKind::Press;
                key.state = crossterm::event::KeyEventState::NONE;
                self.key(key);
            }
            Event::Paste(text) => {
                let text = model::clean(&text);
                match self.input_target() {
                    InputTarget::Menu => {
                        if let Some(menu) = &mut self.menu {
                            menu.filter(|input| input.insert(&text));
                        }
                    }
                    InputTarget::Prompt
                        if self
                            .prompts
                            .front()
                            .is_some_and(|prompt| prompt.takes_text()) =>
                    {
                        let outcome = self.prompt_input_mut().editor.insert(&text);
                        if self.multiple_questions() && outcome.text_changed() {
                            self.set_question_editing(true);
                            self.invalidate_question_answer();
                        }
                    }
                    InputTarget::Composer => {
                        if text.lines().count() > PASTE_COLLAPSE_LINES {
                            self.editor.insert_paste(text);
                        } else {
                            self.editor.insert(&text);
                        }
                        self.composer_scroll = None;
                    }
                    InputTarget::Prompt | InputTarget::None => {}
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
                            && self.composer_rect.contains(point.into())
                        {
                            let options = self.prompt_options_rect.contains(point.into());
                            let (view, len) = self.prompt_view(options);
                            view.scroll(step, len);
                        } else if matches!(self.input_target(), InputTarget::Composer)
                            && self.composer_rect.contains(point.into())
                            && self.composer_overflows()
                        {
                            self.scroll_composer(step);
                        } else if self.tree_rect.contains(point.into()) {
                            let len = self.projection.visible(&self.selected).count();
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
                            .hits
                            .iter()
                            .rev()
                            .find(|(rect, _)| rect.contains(point.into()))
                            .cloned()
                        {
                            match hit {
                                Hit::Agent(agent) => self.select(agent),
                                Hit::Entry(index, toggle) => {
                                    self.focus = Focus::Content;
                                    self.select_entry(index);
                                    if toggle {
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
                                Hit::Composer => self.focus = Focus::Composer,
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
                                    if self.prompt_input().choice != index
                                        && self.multiple_questions()
                                    {
                                        self.invalidate_question_answer();
                                    }
                                    self.prompt_input_mut().choice = index;
                                    self.set_question_editing(false);
                                    self.reveal_prompt();
                                }
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
                            }
                        }
                        if select_text && let Some(position) = self.text_position(point) {
                            // Hold the viewport still while selecting a streaming reply.
                            self.scroll_to(Some(self.scroll_offset()));
                            self.focus = Focus::Content;
                            self.selection =
                                crate::tui::render::Selection::new(&self.render.rows, position);
                        }
                    }
                    MouseEventKind::Up(MouseButton::Left) => {
                        match std::mem::take(&mut self.gesture) {
                            // The last drag motion placed the head: the text under the
                            // pointer may have reflowed since. Copied once, on release.
                            Gesture::Dragging => {
                                if let Some(text) = self.selected_text() {
                                    self.copy_text(text);
                                }
                            }
                            Gesture::Pressed { toggle: Some(key) } => {
                                if let Some((_, Hit::Entry(index, true))) = self
                                    .hits
                                    .iter()
                                    .rev()
                                    .find(|(rect, _)| rect.contains(point.into()))
                                    && self
                                        .entries()
                                        .get(*index)
                                        .is_some_and(|entry| entry.key() == &key)
                                {
                                    self.select_entry(*index);
                                    self.toggle();
                                }
                            }
                            Gesture::Pressed { toggle: None }
                            | Gesture::Released
                            | Gesture::Thumb => {}
                        }
                    }
                    MouseEventKind::Drag(MouseButton::Left)
                        if matches!(self.gesture, Gesture::Thumb) =>
                    {
                        self.scroll_to_track(point.1);
                    }
                    MouseEventKind::Drag(MouseButton::Left)
                        if self.content_rect.contains(point.into())
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
        let track = self.content_rect;
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
        if !self.content_rect.contains(point.into()) {
            return None;
        }
        let row = (self.scroll_offset() + point.1.saturating_sub(self.content_rect.y) as usize)
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
    use skyhook::job::Continuation;

    /// A viewed session's notices are shown for the agent they concern, a saved
    /// child included, and never journaled.
    #[tokio::test]
    async fn viewed_child_notices_are_shown_not_journaled() {
        let (_root, mut app, mut model, session) = played_fixture().await;
        let (children, _turns) = spawn_children(&mut app, &mut model, 1).await;
        let child = children[0].clone();
        bounded(session.shutdown()).await.unwrap();
        session.close().await.unwrap();

        let viewed = app.launch.create(Some(session.id())).await.unwrap();
        let before = journal(&viewed).await;
        let prepared = prepared(viewed.clone()).await;
        let (tx, mut work) = mpsc::unbounded_channel();
        app = app.sibling(Some(prepared), app.launch.clone(), tx);
        app.select(child.clone());
        app.notice("about the child");
        bounded(async {
            while unsaved(&app, &child).is_empty() {
                app.work(work.recv().await.expect("work channel is open"));
            }
        })
        .await;
        let after = journal(&viewed).await;
        bounded(viewed.shutdown()).await.unwrap();
        viewed.close().await.unwrap();
        assert_eq!(unsaved(&app, &child), ["about the child"]);
        assert_eq!(
            before, after,
            "a viewed session's notices are never journaled"
        );
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
            app.dirty = false;
            app.work(attachment(stale));
            assert!(app.editor.attachments().is_empty());
            assert!(!app.dirty);
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
        app.dirty = false;
        app.work(attachment(ticket));
        assert!(!app.dirty);
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
        app.hits.push((cell, Hit::PromptChoice(0)));
        drop(response);
        app.tick();
        assert!(app.prompts.is_empty());
        mouse(&mut app, cell, MouseEventKind::Down(MouseButton::Left));
        app.event(Event::Resize(80, 24));
        assert!(app.prompts.is_empty());
    }

    /// Releasing after the streaming reply rewraps under the pointer: a press and
    /// release without movement copies nothing, and a drag keeps the selection it made.
    #[tokio::test]
    async fn releasing_after_a_rewrap_copies_only_what_a_drag_selected() {
        use skyhook::provider::protocol::ItemKind;
        let left = MouseButton::Left;
        for drag in [false, true] {
            let (_root, mut app, mut model, _session) = played_fixture().await;
            let text = format!("{} beta", "a".repeat(46));
            let asked = ask(app.session().unwrap(), "question");
            let turn = model.turn().await;
            turn.delta("text", ItemKind::Text, &text);
            observe_until(&mut app, |app| streamed(&app.snapshot, &text)).await;
            let screen = draw(&mut app);
            let (x, y) = (screen.lines().enumerate())
                .find_map(|(y, line)| Some((line.find(" beta")? + 1, y)))
                .unwrap();
            let start = Rect::new(x as u16, y as u16, 1, 1);
            let end = if drag {
                Rect {
                    x: start.x + 2,
                    ..start
                }
            } else {
                start
            };
            app.clipboard = Some("earlier".into());
            mouse(&mut app, start, MouseEventKind::Down(left));
            if drag {
                mouse(&mut app, end, MouseEventKind::Drag(left));
            }
            turn.delta("text", ItemKind::Text, "gamma");
            let grown = format!("{text}gamma");
            observe_until(&mut app, |app| streamed(&app.snapshot, &grown)).await;
            let after = draw(&mut app);
            assert!(!after.contains("a beta"), "the word wraps:\n{after}");
            let (selected, copied) = if drag { ("be", "be") } else { ("", "earlier") };
            let selection = |app: &App| app.selection.as_ref().map(|s| s.text().to_owned());
            if drag {
                assert_eq!(selection(&app).as_deref(), Some(selected));
            }
            mouse(&mut app, end, MouseEventKind::Up(left));
            assert_eq!(selection(&app).as_deref(), Some(selected));
            assert_eq!(app.clipboard.as_deref(), Some(copied));
            if !drag {
                assert!(app.toast.is_none());
            }
            turn.answer(&grown);
            bounded(asked).await.unwrap().unwrap();
        }
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
        let color = crate::tui::theme::THEME.selected;
        // The selected cells of the transcript, in reading order.
        let highlighted = |app: &App, buffer: &ratatui::buffer::Buffer| {
            let area = app.content_rect;
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
            let color = crate::tui::theme::THEME.selected;
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
            let content = app.content_rect;
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

    #[tokio::test]
    async fn pre_job_failure_updates_the_existing_tool_card_and_expands_in_place() {
        use crate::tui::theme::THEME;
        let (_root, mut app, mut model, _session) = played_fixture().await;
        let tool_cards = |app: &App| {
            let entries = app.entries().iter();
            let cards = entries.filter(|entry| entry.surface == model::Surface::Tool);
            cards.cloned().collect::<Vec<_>>()
        };
        let _asked = ask(app.session().unwrap(), "test it");
        model.turn().await.end(vec![failing_call("failing-call")]);
        observe_record(&mut app, committed(assistant)).await;
        draw(&mut app);
        let key = tool_cards(&app)[0].key().clone();
        observe_record(&mut app, committed(results)).await;
        let error = tool_error(&app.journal).to_owned();
        let records = serde_json::to_vec(&app.journal).unwrap();
        let buffer = draw_buffer(&mut app);
        let cards = tool_cards(&app);
        assert_eq!(
            (cards.len(), cards[0].key(), cards[0].job_id()),
            (1, &key, None)
        );
        assert!(cards[0].text().contains("Failed") && cards[0].expandable());
        assert!(!cards[0].text().contains(&error));
        assert!(buffer.content.windows(6).any(|cells| {
            cells.iter().map(|cell| cell.symbol()).collect::<String>() == "Failed"
                && cells.iter().all(|cell| cell.fg == THEME.error)
        }));
        let index = app.entries().iter().position(|entry| entry.key() == &key);
        let toggle = |app: &mut App| {
            let mut hits = app.hits.iter();
            let hit = hits.find_map(|(rect, hit)| {
                matches!(hit, Hit::Entry(i, true) if Some(*i) == index).then_some(*rect)
            });
            click(app, hit.unwrap());
            draw(app);
        };
        toggle(&mut app);
        let cards = tool_cards(&app);
        assert_eq!((cards.len(), cards[0].key()), (1, &key));
        for part in ["Arguments", "Output", error.as_str()] {
            assert!(cards[0].text().contains(part));
        }
        assert!(cards[0].document().is_some());
        toggle(&mut app);
        assert_eq!(serde_json::to_vec(&app.journal).unwrap(), records);
    }

    #[tokio::test]
    async fn file_preview_uses_source_fields_and_can_continue_truncated_output() {
        let (_root, mut app) = fixture().await;
        // Exceed the automatic 200-line limit without relying on its byte budget.
        let source: String = (0..201)
            .map(|index| format!("// original source line {index:03}\n"))
            .collect();
        std::fs::write(app.launch.workspace.join("example.rs"), &source).unwrap();
        run_script(&mut app, "return await tool.read({path:'example.rs'});").await;
        let job = job_named(&app, "read");
        let output = fetch_output(&mut app, job).await;
        assert!(source.starts_with(output.result.as_ref().unwrap()["content"].as_str().unwrap()));
        let Some(Continuation::Lines {
            field,
            start,
            offset,
        }) = output.continuation()
        else {
            panic!("a text continuation")
        };
        app.command(Command::Details);
        draw(&mut app);
        select_job(&mut app, job);
        app.output_menu();
        let menu = app.menu.as_mut().unwrap();
        let MenuKind::Output(_, items) = &menu.kind else {
            panic!("output menu")
        };
        let next = items
            .iter()
            .position(|item| item.value == menus::OutputAction::Next);
        menu.select(next.unwrap());
        app.choose();
        let query = app.outputs.query(job).unwrap().clone();
        assert_eq!((query.start, query.offset), (Some(start), offset));
        assert_eq!(query.field.as_ref(), field);
        assert_eq!(field.unwrap().as_str(), "/result/content");
    }

    /// A drag across a soft wrap spans no source text: releasing it leaves the
    /// clipboard and toast untouched.
    #[tokio::test]
    async fn an_empty_drag_copies_nothing() {
        let (_root, mut app) = fixture().await;
        app.local_notice("word ".repeat(40));
        draw(&mut app);
        let content = app.content_rect;
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
