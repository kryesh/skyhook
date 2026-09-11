use super::*;

pub enum Work {
    QueueCommitted {
        session: SessionId,
        id: u64,
        generation: u64,
        revision: u64,
        result: Result<(), String>,
    },
    Done {
        session: SessionId,
        result: Result<(), String>,
    },
    Output {
        session: SessionId,
        job: JobId,
        version: u64,
        finished: bool,
        result: Result<Value, String>,
    },
    MenuLoaded {
        id: u64,
        result: Result<Vec<Item>, String>,
    },
    File {
        draft: u64,
        result: Result<(PathBuf, String), String>,
    },
    SessionReady(Result<Option<SessionHandle>, String>),
    Started(Result<SessionHandle, String>),
    StatusFailed {
        session: Option<SessionId>,
        agent: AgentId,
        message: String,
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
    PromptChoice(usize),
    Latest,
}
impl App {
    pub fn work(&mut self, work: Work) {
        match work {
            Work::Started(Err(error)) => self.start_failed(error),
            Work::QueueCommitted {
                session,
                id,
                generation,
                revision,
                result,
            } if Some(session) == self.session_id() => {
                self.queue_committed(id, generation, revision, result);
            }
            Work::Done { session, result } if Some(session) == self.session_id() => {
                self.operation = false;
                self.awaiting_initial_input = false;
                if let Err(error) = result {
                    self.root_notifier().send(error);
                    self.paused = true;
                    self.cancel_queue_delivery();
                    if let Some(paused) = &mut self.switch_restore {
                        *paused = true;
                    }
                }
            }
            Work::Output {
                session,
                job,
                version,
                finished,
                result,
            } if Some(session) == self.session_id() => {
                self.pending_outputs.remove(&job);
                if version != *self.output_versions.get(&job).unwrap_or(&0) {
                    return;
                }
                if finished {
                    self.final_outputs.insert(job);
                }
                let value = result.unwrap_or_else(|error| json!({"error": error}));
                if self.outputs.get(&job) == Some(&value) {
                    return;
                }
                self.outputs.insert(job, value);
                self.content_cache.invalidate_job(job);
                self.content_dirty = true;
            }
            Work::MenuLoaded { id, result } => {
                let Some(menu) = self.menu.as_mut().filter(|menu| menu.id == id) else {
                    return;
                };
                match result {
                    Ok(items) => menu.items = items,
                    Err(error) => self.notice(error),
                }
            }
            Work::File { draft, result } => {
                if draft != self.draft_revision {
                    return;
                }
                match result {
                    Ok((path, content)) => {
                        self.editor
                            .insert_paste(format!("File: {}\n{content}", path.display()));
                        self.notice(format!("Attached {}", path.display()));
                    }
                    Err(error) => self.notice(error),
                }
            }
            Work::SessionReady(Err(error)) => {
                if let Some(paused) = self.switch_restore.take() {
                    self.paused = paused;
                }
                self.notice(error);
                if self.stopping {
                    self.finish_shutdown();
                }
            }
            Work::StatusFailed {
                session,
                agent,
                message,
            } if (session == self.session_id()
                && (session.is_some() || agent == self.selected))
                || (session.is_none() && self.attached_draft.as_ref() == Some(&agent)) =>
            {
                let agent = if self.attached_draft.as_ref() == Some(&agent) {
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
            .leader
            .is_some_and(|(_, t)| t.elapsed() > Duration::from_secs(2))
        {
            self.leader = None;
            self.dirty = true;
        }
        let previous = self.prompts.front().map(|p| p.id);
        self.prompts.retain(|p| !p.reply.is_closed());
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
                        && view.is_expanded(&format!("j{}", j.id), self.details)
                        && (!j.state.is_terminal() || !self.final_outputs.contains(&j.id))
                })
                .map(|j| j.id)
                .collect();
            for id in jobs {
                self.fetch_output(id);
            }
        }
        self.dirty |= self.animating;
        // Retire completed child rows once, rather than redrawing forever while idle.
        let before = self.projection.completed.len();
        self.projection
            .completed
            .retain(|_, finished| finished.elapsed() < Duration::from_secs(2));
        self.dirty |= before != self.projection.completed.len();
    }
    pub(super) fn set_output_query(&mut self, job: JobId, query: JobOutputQuery) {
        self.output_queries.insert(job, query);
        *self.output_versions.entry(job).or_default() += 1;
        self.final_outputs.remove(&job);
        self.fetch_output(job);
    }
    pub(super) fn fetch_output(&mut self, job: JobId) {
        if self.session.is_none() || !self.pending_outputs.insert(job) {
            return;
        }
        let query = self
            .output_queries
            .entry(job)
            .or_insert_with(|| {
                let mut q = JobOutputQuery::new(job);
                q.field = match self.projection.jobs.get(&job).map(|job| job.tool.as_str()) {
                    Some("exec" | "shell") => Some("/result/stdout".into()),
                    // These schemas bound long fields in the structured projection.
                    Some("read" | "write" | "replace" | "patch") => None,
                    _ => Some(String::new()),
                };
                q
            })
            .clone();
        let version = *self.output_versions.get(&job).unwrap_or(&0);
        let finished = self
            .projection
            .jobs
            .get(&job)
            .is_some_and(|job| job.state.is_terminal());
        let Some(session) = self.session.clone() else {
            return;
        };
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = session
                .inspect_output(query)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(Work::Output {
                session: session.id(),
                job,
                version,
                finished,
                result,
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
                self.preview_theme();
                return;
            }
            let target = |point: Option<(u16, u16)>| {
                point.and_then(|point| {
                    self.hits.iter().position(|(rect, hit)| {
                        rect.contains(point.into())
                            && match hit {
                                Hit::Agent(_) => true,
                                Hit::Entry(index, _) => self
                                    .entries
                                    .get(*index)
                                    .is_some_and(|entry| entry.expandable),
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
                    InputTarget::Prompt => {
                        self.prompt_editor.insert(&text);
                        if self.multiple_questions() {
                            self.question_editing = true;
                            self.invalidate_question_answer();
                        }
                    }
                    InputTarget::Composer if text.lines().count() > 12 => {
                        self.editor.insert_paste(text)
                    }
                    InputTarget::Composer => self.editor.insert(&text),
                    InputTarget::None => {}
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
                                        self.pressed_entry =
                                            self.entries.get(index).map(|entry| entry.key.clone());
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
                                    self.view().tab = tab;
                                    self.view().scroll = None;
                                    self.invalidate_content();
                                }
                                Hit::Composer => self.focus = Focus::Composer,
                                Hit::Attachments => self.command("attachments"),
                                Hit::Attention => self.activate_prompt(),
                                Hit::PromptChoice(index) => {
                                    self.activate_prompt();
                                    if self.prompt_choice != index && self.multiple_questions() {
                                        self.invalidate_question_answer();
                                    }
                                    self.prompt_choice = index;
                                    self.question_editing = false;
                                    self.prompt_reveal = true;
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
                                .entries
                                .get(*index)
                                .is_some_and(|entry| entry.key == key)
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
                self.prompt_reveal = true;
            }
            _ => {}
        }
        self.preview_theme();
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
    #[tokio::test]
    async fn attachment_reads_do_not_leak_into_the_next_submission_or_session() {
        let (_root, mut app) = draft_fixture().await;
        let attachment = |draft| Work::File {
            draft,
            result: Ok((PathBuf::from("fixture.txt"), "contents".into())),
        };
        let draft = app.draft_revision;
        app.info("Unrelated overlay", "Still the same draft".into());
        app.work(attachment(draft));
        assert_eq!(
            app.editor
                .pastes()
                .map(|(_, text)| text)
                .collect::<Vec<_>>(),
            ["File: fixture.txt\ncontents"]
        );
        app.editor.take();

        // Queue without starting a session: even a queued submission consumes its draft.
        app.paused = true;
        app.submit("submitted".into(), vec![]);
        app.dirty = false;
        app.work(attachment(draft));
        assert!(!app.editor.has_pastes());
        assert!(!app.dirty);
        let next_draft = app.draft_revision;
        app.work(attachment(next_draft));
        assert_eq!(app.editor.pastes().count(), 1);

        app.set_session(None, ObservationSnapshot::default());
        app.dirty = false;
        app.work(attachment(next_draft));
        assert!(!app.editor.has_pastes());
        assert!(!app.dirty);
        app.work(attachment(app.draft_revision));
        assert_eq!(app.editor.pastes().count(), 1);
    }
    #[tokio::test]
    async fn message_drag_selects_only_the_requested_text_including_unicode() {
        for surface in [model::Surface::User, model::Surface::Agent] {
            for word in ["bravo", "e\u{301}界🙂"] {
                let (_root, mut app) = fixture().await;
                app.entries = vec![Entry {
                    key: "selectable".into(),
                    text: format!("Sender\nAlpha **{word}** omega"),
                    surface,
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
                }];
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
                mouse(
                    &mut app,
                    Rect::new(start, y, 1, 1),
                    MouseEventKind::Down(MouseButton::Left),
                );
                mouse(
                    &mut app,
                    Rect::new(end, y, 1, 1),
                    MouseEventKind::Drag(MouseButton::Left),
                );
                mouse(
                    &mut app,
                    Rect::new(end, y, 1, 1),
                    MouseEventKind::Up(MouseButton::Left),
                );
                let selected = draw_buffer(&mut app);
                let color = crate::tui::render::Palette::new(app.light).selected;
                // Terminals paint wide characters from their leading cell; Ratatui's
                // backend diff skips their continuation cells.
                let mut column = x;
                while column < x + 5 {
                    assert_eq!(selected[(column, y)].bg, color);
                    column += unicode_width::UnicodeWidthStr::width(selected[(column, y)].symbol())
                        .max(1) as u16;
                }
                assert_ne!(selected[(x - 1, y)].bg, color);
                assert_ne!(selected[(x + 5, y)].bg, color);
                key(&mut app, KeyCode::Char('x'), M::CONTROL);
                key(&mut app, KeyCode::Char('y'), M::NONE);
                assert_eq!(app.clipboard.as_deref(), Some(word));
                app.session.as_ref().unwrap().shutdown().await.unwrap();
            }
        }
    }
    #[tokio::test]
    async fn pre_job_failure_updates_the_existing_tool_card_and_expands_in_place() {
        use crate::tui::theme::ContentTheme;
        use skyhook::{
            provider::protocol::{AssistantItem, Message, ToolCall, ToolResult},
            session::EventRecord,
        };
        let (_root, mut app) = fixture().await;
        let sequence = app.snapshot.records.last_key_value().unwrap().0 + 1;
        app.snapshot.records.insert(
            sequence,
            EventRecord {
                version: 1,
                sequence,
                timestamp_millis: 0,
                agent: app.selected.clone(),
                event: SessionEvent::MessageCommitted {
                    message: Message::Assistant(vec![AssistantItem::tool_call(
                        "call-item",
                        0,
                        ToolCall {
                            id: "denied-call".into(),
                            name: "exec".into(),
                            arguments: serde_json::json!({"argv": ["cargo", "test"]}),
                        },
                    )]),
                },
            },
        );
        app.refresh();
        draw(&mut app);
        let key = app
            .entries
            .iter()
            .find(|entry| entry.surface == model::Surface::Tool)
            .unwrap()
            .key
            .clone();
        app.snapshot.records.insert(sequence + 1, EventRecord {
            version: 1, sequence: sequence + 1, timestamp_millis: 1, agent: app.selected.clone(),
            event: SessionEvent::MessageCommitted { message: Message::Tool(vec![ToolResult {
                call_id: "denied-call".into(), name: "exec".into(),
                result: serde_json::json!({"error": "Permission was denied", "code": "permission_denied", "executed": false}),
                images: vec![], is_error: true,
            }]) },
        });
        let records = serde_json::to_vec(&app.snapshot.records).unwrap();
        app.refresh();
        for light in [false, true] {
            app.light = light;
            let buffer = draw_buffer(&mut app);
            let cards = app
                .entries
                .iter()
                .filter(|entry| entry.surface == model::Surface::Tool)
                .collect::<Vec<_>>();
            assert_eq!(cards.len(), 1);
            assert_eq!(cards[0].key, key);
            assert!(cards[0].text.contains("Failed"));
            assert!(!cards[0].text.contains("Permission was denied"));
            assert!(cards[0].job.is_none());
            assert!(cards[0].expandable);
            assert!(buffer.content.windows(6).any(|cells| {
                cells.iter().map(|cell| cell.symbol()).collect::<String>() == "Failed"
                    && cells
                        .iter()
                        .all(|cell| cell.fg == ContentTheme::new(light).error)
            }));
            let index = app
                .entries
                .iter()
                .position(|entry| entry.key == key)
                .unwrap();
            let hit = app
                .hits
                .iter()
                .find_map(|(rect, hit)| {
                    matches!(hit, Hit::Entry(i, true) if *i == index).then_some(*rect)
                })
                .unwrap();
            click(&mut app, hit);
            draw(&mut app);
            let card = app.entries.iter().find(|entry| entry.key == key).unwrap();
            assert!(card.text.contains("Arguments"));
            assert!(card.text.contains("Output"));
            assert!(card.text.contains("Permission was denied"));
            assert!(card.text.contains("permission_denied"));
            assert!(card.document.is_some());
            assert_eq!(
                app.entries
                    .iter()
                    .filter(|entry| entry.surface == model::Surface::Tool)
                    .count(),
                1
            );
            let hit = app
                .hits
                .iter()
                .find_map(|(rect, hit)| {
                    matches!(hit, Hit::Entry(i, true) if *i == index).then_some(*rect)
                })
                .unwrap();
            click(&mut app, hit);
            draw(&mut app);
        }
        assert_eq!(serde_json::to_vec(&app.snapshot.records).unwrap(), records);
        app.session.as_ref().unwrap().shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn file_preview_uses_source_fields_and_can_continue_truncated_output() {
        let (_root, mut app) = fixture().await;
        let source = (0..100)
            .map(|index| format!("// original source line {index:03}\n"))
            .collect::<String>();
        std::fs::write(app.launch.workspace.join("example.rs"), &source).unwrap();
        app.session
            .as_ref()
            .unwrap()
            .run_script("return await tool.read({path:'example.rs'});")
            .await
            .unwrap();
        app.snapshot = app.session.as_ref().unwrap().observe().await.snapshot;
        app.refresh();
        let job = app
            .projection
            .jobs
            .values()
            .find(|job| job.tool == "read")
            .unwrap()
            .id;
        app.fetch_output(job);
        let query = app.output_queries[&job].clone();
        assert!(query.field.is_none());
        let output = app
            .session
            .as_ref()
            .unwrap()
            .inspect_output(query)
            .await
            .unwrap();
        let prefix = output["result"]["content"].as_str().unwrap();
        assert!(source.starts_with(prefix));
        let position = output["truncated"][0].clone();
        app.outputs.insert(job, output);
        app.pending_outputs.clear();
        app.command("details");
        draw(&mut app);
        app.view().row = app
            .entries
            .iter()
            .position(|entry| entry.job == Some(job))
            .unwrap();
        app.output_menu();
        let menu = app.menu.as_mut().unwrap();
        menu.selected = menu
            .items
            .iter()
            .position(|item| item.value == "next")
            .unwrap();
        app.choose();
        let query = app.output_queries[&job].clone();
        assert_eq!(
            query.start,
            Some(position["next_start"].as_u64().unwrap() as usize)
        );
        assert_eq!(
            query.offset,
            position["next_offset"]
                .as_u64()
                .map(|offset| offset as usize)
        );
        let page = app
            .session
            .as_ref()
            .unwrap()
            .inspect_output(query)
            .await
            .unwrap();
        assert_eq!(page["preview"]["field"], "/result/content");
        assert!(
            page["preview"]["lines"]
                .as_array()
                .unwrap()
                .iter()
                .any(|line| line.as_str().unwrap().contains("source line 099"))
        );
        assert_eq!(
            std::fs::read_to_string(app.launch.workspace.join("example.rs")).unwrap(),
            source
        );
        app.session.as_ref().unwrap().shutdown().await.unwrap();
    }
}
