use super::*;
use crate::launch::LaunchError;
use crate::tui::tool_view::OutputView;

// Query presence, capture selection and preview metadata belong to core.
async fn load_output(session: &SessionHandle, query: JobOutputQuery) -> Result<OutputView, String> {
    session
        .inspect_output_with_captures(query)
        .await
        .map(OutputView::from)
        .map_err(|error| error.to_string())
}

pub enum Work {
    QueueCommitted {
        id: QueuedInputId,
        generation: u64,
        revision: u64,
        result: Result<(), skyhook::agent::HarnessError>,
    },
    Done {
        result: Result<(), String>,
    },
    Output {
        attempt: OutputAttempt,
        finished: bool,
        result: Box<Result<OutputView, String>>,
    },
    MenuLoaded(menus::MenuLoaded),
    File {
        draft: Token,
        /// Composer offset just after the `@` that opened the file picker.
        at: Option<usize>,
        result: Result<Attachment, String>,
    },
    Started {
        result: Result<SessionHandle, LaunchError>,
    },
    StatusFailed {
        session: Option<SessionId>,
        agent: AgentId,
        message: String,
    },
    Interrupted {
        count: usize,
    },
    Stopped,
    HighlightsReady,
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
    Queue,
    PromptChoice(usize),
    Latest,
}
impl App {
    pub fn work(&mut self, work: Work) {
        match work {
            Work::Started { result: Err(error) } => self.start_failed(error),
            Work::QueueCommitted {
                id,
                generation,
                revision,
                result,
            } => self.queue_committed(id, generation, revision, result),
            Work::Interrupted { count } => {
                if count > 0 {
                    // Journal the status only for an interruption that happened.
                    self.root_notifier().send("Interrupted");
                } else {
                    // UI-only: nothing was stopped, so nothing is recorded.
                    self.toast("Nothing to interrupt");
                }
            }
            Work::Done { result } => {
                self.operation = false;
                self.initial_input = None;
                if let Err(error) = result {
                    self.root_notifier().send(error);
                    self.pause_queue();
                }
            }
            Work::Output {
                attempt,
                finished,
                result,
            } => {
                let job = attempt.job();
                if !self.outputs.complete(attempt, finished, *result) {
                    return;
                }
                self.content_cache.invalidate_job(job);
                self.content_dirty = true;
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
                            self.notice(format!("Attached {}", file.display()));
                        }
                        self.editor.attach(attachment);
                    }
                    Err(error) => self.notice(error),
                }
            }
            Work::StatusFailed {
                session,
                agent,
                message,
            } if (session == self.session_id()
                && (session.is_some() || agent == self.selected))
                || (session.is_none() && self.attached_draft() == Some(&agent)) =>
            {
                let agent = if self.attached_draft() == Some(&agent) {
                    self.root_agent().clone()
                } else {
                    agent
                };
                self.unsaved_status.push((agent, message));
                self.invalidate_content();
            }
            Work::Stopped => self.exit = true,
            _ => {}
        }
        self.dirty = true;
    }
    pub fn tick(&mut self) {
        self.tick_count = self.tick_count.wrapping_add(1);
        if self
            .toast
            .as_ref()
            .is_some_and(|(_, t)| t.elapsed() > Duration::from_secs(2))
        {
            self.toast = None;
            self.dirty = true;
        }
        let previous = self.prompts.front().map(|p| p.id);
        self.prompts.retain(|p| !p.is_closed());
        if previous != self.prompts.front().map(|p| p.id) {
            self.dirty = true;
            self.reset_prompt();
        }
        if self.prompts.is_empty() {
            self.prompt_active = false;
        }
        self.deliver_queue();
        if self.last_output.elapsed() >= Duration::from_millis(500) {
            self.last_output = Instant::now();
            let view = self.views.entry(self.selected.clone()).or_default();
            let jobs: Vec<_> = self
                .projection
                .jobs
                .values()
                .filter(|j| {
                    j.agent == self.selected
                        && view.is_expanded(&model::EntryKey::Job(j.id), self.details)
                        && (!j.state.is_terminal() || !self.outputs.is_final(j.id))
                })
                .map(|j| j.id)
                .collect();
            for id in jobs {
                self.fetch_output(id);
            }
        }
        self.dirty |= self.animating;
        // Retire completed child rows once, rather than redrawing forever while idle.
        self.dirty |= self.projection.retire_completed_grace();
    }
    pub(super) fn set_output_query(&mut self, query: JobOutputQuery) {
        let job = query.job;
        self.outputs.set_query(query);
        self.fetch_output(job);
    }
    pub(super) fn fetch_output(&mut self, job: JobId) {
        let Some(session) = self.session().cloned() else {
            return;
        };
        let Some((attempt, query)) = self.outputs.begin(job) else {
            return;
        };
        let finished = self
            .projection
            .jobs
            .get(&job)
            .is_some_and(|job| job.state.is_terminal());
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = load_output(&session, query).await;
            let _ = tx.send(Work::Output {
                attempt,
                finished,
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
                }) && index < menu.filtered().len()
                    && menu.selected != index
                {
                    menu.selected = index;
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
                            menu.input.insert(&text);
                            menu.selected = 0;
                        }
                    }
                    InputTarget::Search => {
                        self.search_editor.as_mut().unwrap().insert(&text);
                    }
                    InputTarget::Prompt
                        if self
                            .prompts
                            .front()
                            .is_some_and(|prompt| prompt.takes_text()) =>
                    {
                        let outcome = self.prompt_input_mut().editor.insert(&text);
                        if self.multiple_questions() && outcome.text_changed {
                            self.set_question_editing(true);
                            self.invalidate_question_answer();
                        }
                    }
                    InputTarget::Composer if text.lines().count() > 12 => {
                        self.editor.insert_paste(text);
                    }
                    InputTarget::Composer => {
                        self.editor.insert(&text);
                    }
                    InputTarget::Prompt | InputTarget::None => {}
                }
            }
            Event::Mouse(mouse) => {
                let point = (mouse.column, mouse.row);
                match mouse.kind {
                    MouseEventKind::Moved => self.hover = Some(point),
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                        if self.menu.is_some() =>
                    {
                        self.menu_key(KeyEvent::new(
                            if mouse.kind == MouseEventKind::ScrollUp {
                                KeyCode::Up
                            } else {
                                KeyCode::Down
                            },
                            M::NONE,
                        ));
                    }
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                        if matches!(self.input_target(), InputTarget::Prompt)
                            && self.composer_rect.contains(point.into()) =>
                    {
                        self.scroll_prompt(
                            self.prompt_options_rect.contains(point.into()),
                            if mouse.kind == MouseEventKind::ScrollUp {
                                -3
                            } else {
                                3
                            },
                        );
                    }
                    MouseEventKind::ScrollUp => {
                        if self.tree_rect.contains(point.into()) {
                            self.tree_scroll = self.tree_scroll.saturating_sub(3);
                        } else {
                            self.scroll(-3);
                        }
                    }
                    MouseEventKind::ScrollDown => {
                        if self.tree_rect.contains(point.into()) {
                            self.tree_scroll += 3;
                        } else {
                            self.scroll(3);
                        }
                    }
                    MouseEventKind::Down(MouseButton::Left) => {
                        self.pressed_entry = None;
                        self.selection = None;
                        let mut select_text = true;
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
                                    self.view().row = index;
                                    if toggle {
                                        self.pressed_entry = self
                                            .entries()
                                            .get(index)
                                            .map(|entry| entry.key().clone());
                                    }
                                }
                                Hit::Menu(index) => {
                                    if let Some(menu) = &mut self.menu {
                                        menu.selected = index;
                                    }
                                    self.choose();
                                }
                                Hit::Tab(tab) => {
                                    self.selection = None;
                                    self.tab = tab;
                                    self.view().scroll = None;
                                    self.invalidate_content();
                                }
                                Hit::Composer => self.focus = Focus::Composer,
                                Hit::Attachments => self.command(Command::Attachments),
                                Hit::Attention => self.activate_prompt(),
                                Hit::Sessions => self.command(Command::Sessions),
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
                                    self.view().scroll = None;
                                    // The text-only overlay sits over selectable history.
                                    // Do not pin the viewport again by selecting beneath it.
                                    select_text = false;
                                }
                            }
                        }
                        if select_text && let Some(position) = self.text_position(point) {
                            let scroll = self
                                .views
                                .get(&self.selected)
                                .and_then(|v| v.scroll)
                                .unwrap_or(
                                    self.content_rows
                                        .saturating_sub(self.content_rect.height as usize),
                                );
                            // Hold the viewport still while selecting a streaming reply.
                            self.view().scroll = Some(scroll);
                            self.focus = Focus::Content;
                            self.selection = Some((position, position));
                        }
                    }
                    MouseEventKind::Up(MouseButton::Left) => {
                        if self.pressed_entry.is_none()
                            && let Some((anchor, _)) = self.selection
                            && let Some(head) = self.text_position(point)
                        {
                            self.selection = Some((anchor, head));
                        }
                        if let Some(key) = self.pressed_entry.take()
                            && let Some((_, Hit::Entry(index, true))) = self
                                .hits
                                .iter()
                                .rev()
                                .find(|(rect, _)| rect.contains(point.into()))
                            && self
                                .entries()
                                .get(*index)
                                .is_some_and(|entry| entry.key() == &key)
                        {
                            self.view().row = *index;
                            self.toggle();
                        }
                    }
                    MouseEventKind::Drag(MouseButton::Left)
                        if self.content_rect.contains(point.into()) =>
                    {
                        self.pressed_entry = None;
                        if let Some((anchor, _)) = self.selection
                            && let Some(head) = self.text_position(point)
                        {
                            self.selection = Some((anchor, head));
                        }
                    }
                    _ => {}
                }
            }
            Event::Resize(..) => {
                self.selection = None;
                self.reveal_prompt();
            }
            _ => {}
        }
    }
    pub(super) fn text_position(
        &self,
        point: (u16, u16),
    ) -> Option<crate::tui::render::TextPosition> {
        if !self.content_rect.contains(point.into()) {
            return None;
        }
        let scroll = self
            .views
            .get(&self.selected)
            .and_then(|view| view.scroll)
            .unwrap_or(
                self.content_rows
                    .saturating_sub(self.content_rect.height as usize),
            );
        let row = (scroll + point.1.saturating_sub(self.content_rect.y) as usize)
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
    use super::super::tests::*;
    use super::*;
    use skyhook::session::Message;

    async fn fetch_output(app: &mut App, job: JobId) -> Value {
        let mut rx = capture_work(app);
        app.fetch_output(job);
        let work = recv(&mut rx).await;
        assert!(matches!(work, Work::Output { ref attempt, .. } if attempt.job() == job));
        app.work(work);
        app.outputs.get(&job).unwrap().value().clone()
    }

    fn capture_text(output: &Value, field: &str) -> String {
        let mut captures = output["presentation"]["captures"]
            .as_array()
            .into_iter()
            .flatten();
        let capture = captures.find(|capture| capture["field"].as_str() == Some(field));
        let lines = capture
            .and_then(|capture| capture["output"]["presentation"]["preview"]["lines"].as_array());
        let lines = lines.into_iter().flatten().filter_map(Value::as_str);
        lines.collect::<Vec<_>>().join("\n")
    }

    fn job_text(app: &App, job: JobId) -> String {
        let mut entries = app.entries().iter();
        entries
            .find(|entry| entry.job_id() == Some(job))
            .unwrap()
            .text()
            .into()
    }

    #[tokio::test]
    async fn automatic_output_follows_live_captures_then_structured_completion() {
        let (_root, mut app) = draft_fixture().await;
        app.launch.approve_all = true;
        let session = app.launch.create(None).await.unwrap();
        attach(&mut app, session.clone()).await;
        let launched = session.run_script(format!("return await tool.shell({});", json!({
            "command": "printf 'live stdout\\n'; printf 'live stderr\\n' >&2; while [ ! -e release ]; do sleep 0.01; done; exit 1",
            "timeout": 10,
            "bg": true,
        }))).await.unwrap();
        let job: JobId = serde_json::from_value(launched.value["value"]["id"].clone()).unwrap();
        let live = |output: &Value| {
            capture_text(output, "/result/stdout").contains("live stdout")
                && capture_text(output, "/result/stderr").contains("live stderr")
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            while !live(
                load_output(&session, JobOutputQuery::new(job))
                    .await
                    .unwrap()
                    .value(),
            ) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        app.snapshot = session.observe().await.snapshot;
        app.refresh();
        assert!(live(&fetch_output(&mut app, job).await));
        assert!(app.outputs.query(job).is_none());
        app.command(Command::Details);
        draw(&mut app);
        let text = job_text(&app, job);
        assert!(
            text.contains("live stdout") && text.contains("live stderr"),
            "{text}"
        );

        std::fs::write(app.launch.workspace.join("release"), "").unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            // The live job settles just after its journal commit.
            while !app.projection.jobs[&job].state.is_terminal()
                || !session
                    .inspect_jobs(session.root_agent())
                    .await
                    .iter()
                    .any(|envelope| envelope.id == job && envelope.state.is_terminal())
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
                app.snapshot = session.observe().await.snapshot;
                app.refresh();
            }
        })
        .await
        .unwrap();
        let complete = fetch_output(&mut app, job).await;
        let result = json!({"exit_code": 1, "stdout": "live stdout\n", "stderr": "live stderr\n"});
        for field in ["exit_code", "stdout", "stderr"] {
            assert_eq!(complete["result"][field], result[field]);
        }
        assert!(app.outputs.query(job).is_none());

        let mut query = JobOutputQuery::new(job);
        query.field = Some("/result/stderr".parse().unwrap());
        app.outputs.set_query(query);
        let selected = fetch_output(&mut app, job).await;
        assert_eq!(
            selected["presentation"]["preview"]["field"],
            "/result/stderr"
        );
        let mut captures = selected["presentation"]["captures"]
            .as_array()
            .into_iter()
            .flatten();
        assert!(captures.all(|capture| capture["output"].is_null()));
        let field = app.outputs.query(job).unwrap().field.clone();
        assert_eq!(field, Some("/result/stderr".parse().unwrap()));
        session.shutdown().await.unwrap();
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
        assert_eq!(app.editor.attachments(), std::slice::from_ref(&text));
        app.editor.take();

        // Queue without starting a session: even a queued submission consumes its draft.
        app.paused = true;
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
            attachments: vec![png_attachment("image.png")],
        });
        app.queue = VecDeque::from([queued]);
        app.command(Command::Queue);
        app.choose();
        assert!(!ticket.matches(&app.draft_ticket));
        app.dirty = false;
        app.work(attachment(ticket));
        assert!(!app.dirty);
        assert_eq!(app.editor.attachments(), [png_attachment("image.png")]);
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
        assert!(!app.prompt_active);
    }

    #[tokio::test]
    async fn message_drag_selects_only_the_requested_text_including_unicode() {
        for surface in [model::Surface::User, model::Surface::Agent] {
            for word in ["bravo", "e\u{301}界🙂"] {
                let (_root, mut app) = fixture().await;
                let text = format!("Sender\nAlpha **{word}** omega");
                let entry = Entry::new(model::EntryKey::UnsavedStatus(0), text, surface);
                app.install_entries(vec![entry]);
                app.content_dirty = false;
                let buffer = draw_buffer(&mut app);
                let (x, y) = (0..24)
                    .find_map(|y| {
                        let row = (0..60).map(|x| buffer[(x, y)].symbol()).collect::<String>();
                        row.find("Alpha ").map(|x| (x as u16 + 6, y))
                    })
                    .unwrap();
                // Both fixtures occupy five cells; drag backwards as well as forwards.
                let (start, end) = if surface == model::Surface::User {
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
                let color = crate::tui::render::Palette::new().selected;
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
                chord(&mut app, KeyCode::Char('y'));
                assert_eq!(app.clipboard.as_deref(), Some(word));
                app.session().unwrap().shutdown().await.unwrap();
            }
        }
    }

    #[tokio::test]
    async fn pre_job_failure_updates_the_existing_tool_card_and_expands_in_place() {
        use crate::tui::theme::ContentTheme;
        use skyhook::provider::protocol::{AssistantItem, ToolCall, ToolResult};
        let (_root, mut app) = fixture().await;
        async fn commit(app: &mut App, message: Message) {
            push_record(app, SessionEvent::MessageCommitted { message }).await;
            app.refresh();
        }
        let tool_cards = |app: &App| {
            let entries = app.entries().iter();
            let cards = entries.filter(|entry| entry.surface == model::Surface::Tool);
            cards.cloned().collect::<Vec<_>>()
        };
        let call = ToolCall::new("denied-call", "exec", json!({"argv": ["cargo", "test"]}));
        let call = AssistantItem::tool_call("call-item", 0, call.unwrap());
        commit(&mut app, Message::Assistant(vec![call])).await;
        draw(&mut app);
        let key = tool_cards(&app)[0].key().clone();
        let result = ToolResult {
            call_id: "denied-call".into(),
            name: "exec".into(),
            result: json!({"error": "Permission was denied", "code": "permission_denied", "executed": false}),
            images: vec![],
            is_error: true,
        };
        commit(&mut app, Message::Tool(vec![result])).await;
        let records = serde_json::to_vec(&app.snapshot.records).unwrap();
        let buffer = draw_buffer(&mut app);
        let cards = tool_cards(&app);
        assert_eq!(
            (cards.len(), cards[0].key(), cards[0].job_id()),
            (1, &key, None)
        );
        assert!(cards[0].text().contains("Failed") && cards[0].expandable());
        assert!(!cards[0].text().contains("Permission was denied"));
        let error = ContentTheme::new().error;
        assert!(buffer.content.windows(6).any(|cells| {
            cells.iter().map(|cell| cell.symbol()).collect::<String>() == "Failed"
                && cells.iter().all(|cell| cell.fg == error)
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
        for part in [
            "Arguments",
            "Output",
            "Permission was denied",
            "permission_denied",
        ] {
            assert!(cards[0].text().contains(part));
        }
        assert!(cards[0].document().is_some());
        toggle(&mut app);
        assert_eq!(serde_json::to_vec(&app.snapshot.records).unwrap(), records);
    }

    #[tokio::test]
    async fn expanded_large_script_and_tool_results_use_structured_output() {
        let (_root, mut app) = fixture().await;
        for index in 0..250 {
            let path = app.launch.workspace.join(format!("item-{index:03}.json"));
            std::fs::write(path, "{}").unwrap();
        }
        run_script(&mut app, "return await tool.glob({pattern:'item-*.json'});").await;
        let records = serde_json::to_vec(&app.snapshot.records).unwrap();
        let mut jobs = Vec::new();
        for tool in ["script", "glob"] {
            let job = job_named(&app, tool);
            app.fetch_output(job);
            assert!(app.outputs.query(job).is_none());
            let output = load_output(app.session().unwrap(), JobOutputQuery::new(job)).await;
            let output = output.unwrap();
            let value = output.value();
            assert!(value.get("result").is_some(), "{tool}: {value}");
            assert_eq!(
                value.pointer("/presentation/preview"),
                Some(&serde_json::Value::Null),
                "{tool}: {value}"
            );
            // Script values keep their original envelope shape. The script's
            // own truncation markers point into its independently saved return.
            if tool == "script" {
                assert!(value["result"]["value"]["meta"].is_null());
                assert!(value["result"]["value"]["presentation"].is_null());
                assert_eq!(
                    value["presentation"]["truncated"][0]["field"],
                    "/result/value/result/paths"
                );
            }
            let truncated = value["presentation"]["truncated"].as_array();
            assert!(
                truncated.is_some_and(|fields| !fields.is_empty()),
                "{tool}: {value}"
            );
            app.outputs.insert_product(job, output);
            jobs.push(job);
        }
        app.outputs.clear_pending();
        app.command(Command::Details);
        draw(&mut app);
        for job in jobs {
            let text = job_text(&app, job);
            assert!(text.contains("\"truncated\""), "{text}");
            assert!(text.contains("\n    \"result\": {\n      \""), "{text}");
        }
        assert_eq!(serde_json::to_vec(&app.snapshot.records).unwrap(), records);
    }

    #[tokio::test]
    async fn file_preview_uses_source_fields_and_can_continue_truncated_output() {
        let (_root, mut app) = fixture().await;
        // Exceed the automatic 100-line limit without relying on its byte budget.
        let source: String = (0..101)
            .map(|index| format!("// original source line {index:03}\n"))
            .collect();
        std::fs::write(app.launch.workspace.join("example.rs"), &source).unwrap();
        run_script(&mut app, "return await tool.read({path:'example.rs'});").await;
        let job = job_named(&app, "read");
        app.fetch_output(job);
        assert!(app.outputs.query(job).is_none());
        let session = app.session().unwrap().clone();
        let output = session
            .inspect_output(JobOutputQuery::new(job))
            .await
            .unwrap();
        assert!(source.starts_with(output["result"]["content"].as_str().unwrap()));
        let position = output["presentation"]["truncated"][0].clone();
        app.outputs
            .insert_product(job, OutputView::historical(output));
        app.outputs.clear_pending();
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
        menu.selected = next.unwrap();
        app.choose();
        let query = app.outputs.query(job).unwrap().clone();
        let at = |key: &str| position[key].as_u64().map(|value| value as usize);
        assert!(query.start.is_some());
        assert_eq!(
            (query.start, query.offset.unwrap_or(0)),
            (at("next_start"), at("next_offset").unwrap_or(0))
        );
        assert_eq!(query.field, Some("/result/content".parse().unwrap()));
    }
}
