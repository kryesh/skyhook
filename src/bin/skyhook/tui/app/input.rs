use super::*;

#[derive(Clone, Copy)]
pub(super) enum InputTarget {
    Menu,
    Search,
    Prompt,
    Composer,
    None,
}

impl App {
    pub(super) fn input_target(&self) -> InputTarget {
        if self.menu.is_some() {
            InputTarget::Menu
        } else if self.search_editor.is_some() {
            InputTarget::Search
        } else if self.prompt_active && !self.prompts.is_empty() {
            InputTarget::Prompt
        } else if !self.selected.path().is_empty() {
            InputTarget::None
        } else {
            InputTarget::Composer
        }
    }
    pub(super) fn key(&mut self, key: KeyEvent) {
        let target = self.input_target();
        if matches!(target, InputTarget::None) && self.focus == Focus::Composer {
            self.focus = Focus::Content;
        }
        if matches!(target, InputTarget::Menu) {
            self.menu_key(key);
            return;
        }
        if matches!(target, InputTarget::Search) {
            let editor = self.search_editor.as_mut().unwrap();
            match key.code {
                KeyCode::Esc => self.search_editor = None,
                KeyCode::Enter => {
                    let query = editor.text.clone();
                    self.search_editor = None;
                    self.view().query = query;
                    self.find(false);
                }
                _ => {
                    editor.handle(key);
                }
            }
            return;
        }
        if matches!(target, InputTarget::Prompt) {
            let options = self.prompt_options();
            let multiple = self.multiple_questions();
            let editing_question = multiple
                && matches!(
                self.prompts.front().map(|p| &p.kind),
                Some(PromptKind::Questions { questions, .. }) if self.question_index < questions.len());
            let old_choice = self.prompt_choice;
            // Authentication editors contain secrets: never snapshot them for
            // ordinary question draft change detection.
            let old_text = editing_question.then(|| self.prompt_editor.text.clone());
            let old_index = self.question_index;
            match key.code {
                KeyCode::Esc => {
                    self.cancel_prompt();
                    return;
                }
                KeyCode::PageUp | KeyCode::PageDown => {
                    let options = key.modifiers.contains(M::CONTROL);
                    let height = if options {
                        self.prompt_options_rect.height
                    } else {
                        self.prompt_body_rect.height
                    };
                    self.scroll_prompt(
                        options,
                        height.max(1) as isize * if key.code == KeyCode::PageUp { -1 } else { 1 },
                    );
                }
                KeyCode::Left | KeyCode::Right if multiple && !self.question_editing => {
                    self.switch_question(if key.code == KeyCode::Left { -1 } else { 1 });
                }
                KeyCode::Tab | KeyCode::BackTab if editing_question => {
                    self.question_editing = !self.question_editing;
                }
                KeyCode::Up | KeyCode::BackTab => {
                    self.question_editing = false;
                    if !options.is_empty() {
                        self.prompt_choice =
                            (self.prompt_choice + options.len() - 1) % options.len();
                        self.prompt_reveal = true;
                    }
                }
                KeyCode::Down | KeyCode::Tab => {
                    self.question_editing = false;
                    if !options.is_empty() {
                        self.prompt_choice = (self.prompt_choice + 1) % options.len();
                        self.prompt_reveal = true;
                    }
                }
                KeyCode::Enter => self.answer(),
                _ => {
                    if editing_question
                        && matches!(
                            key.code,
                            KeyCode::Char(_) | KeyCode::Backspace | KeyCode::Delete
                        )
                    {
                        self.question_editing = true;
                    }
                    self.prompt_editor.handle(key);
                }
            }
            if editing_question
                && key.code != KeyCode::Enter
                && self.question_index == old_index
                && (self.prompt_choice != old_choice
                    || old_text
                        .as_deref()
                        .is_some_and(|text| self.prompt_editor.text != text))
            {
                self.invalidate_question_answer();
            }
            return;
        }
        if let Some(prefix) = self.leader.take() {
            if key.code == KeyCode::Esc {
                return;
            }
            if let Some(action) = self.keys.action(Some(prefix), key) {
                self.command(&action);
            }
            return;
        }
        if let Some(action) = self.keys.action(None, key) {
            self.command(&action);
            return;
        }
        if self.keys.prefix(key) {
            self.leader = Some(key);
            return;
        }
        match key.code {
            KeyCode::Tab | KeyCode::BackTab => {
                let reverse = key.code == KeyCode::BackTab || key.modifiers.contains(M::SHIFT);
                self.focus = match (self.focus, reverse) {
                    (Focus::Composer, false) | (Focus::Content, true) => Focus::Tree,
                    (Focus::Tree, false) | (Focus::Composer, true) => Focus::Content,
                    _ => Focus::Composer,
                };
                if self.focus == Focus::Tree && self.tree_rect.height == 0 {
                    self.focus = if reverse {
                        Focus::Composer
                    } else {
                        Focus::Content
                    };
                }
                if !self.selected.path().is_empty() && self.focus == Focus::Composer {
                    self.focus = if reverse && self.tree_rect.height > 0 {
                        Focus::Tree
                    } else {
                        Focus::Content
                    };
                }
                if self.focus == Focus::Content {
                    let current = self.view().row;
                    let visible: Vec<_> = self
                        .hits
                        .iter()
                        .filter_map(|(_, hit)| {
                            if let Hit::Entry(index, _) = hit {
                                Some(*index)
                            } else {
                                None
                            }
                        })
                        .collect();
                    if !visible.contains(&current)
                        && let Some(first) = visible.first()
                    {
                        self.view().row = *first;
                    }
                }
                return;
            }
            KeyCode::PageUp => {
                self.scroll(-(self.content_rect.height as isize));
                return;
            }
            KeyCode::PageDown => {
                self.scroll(self.content_rect.height as isize);
                return;
            }
            KeyCode::Char('u' | 'd') if key.modifiers.contains(M::CONTROL | M::ALT) => {
                self.scroll(
                    (self.content_rect.height as isize / 2)
                        * if key.code == KeyCode::Char('u') {
                            -1
                        } else {
                            1
                        },
                );
                return;
            }
            KeyCode::Esc => {
                if self.busy() {
                    self.interrupt();
                } else {
                    self.focus = if self.selected.path().is_empty() {
                        Focus::Composer
                    } else {
                        Focus::Content
                    };
                }
                return;
            }
            KeyCode::Char('c') if key.modifiers.contains(M::CONTROL) => {
                if self.focus == Focus::Composer && !self.editor.text.is_empty() {
                    self.editor.clear();
                } else if self.busy() {
                    self.interrupt();
                } else {
                    self.command("exit");
                }
                return;
            }
            _ => {}
        }
        match self.focus {
            Focus::Composer => match key.code {
                KeyCode::Enter if !key.modifiers.is_empty() => self.editor.insert("\n"),
                KeyCode::Char('j') if key.modifiers.contains(M::CONTROL) => {
                    self.editor.insert("\n")
                }
                KeyCode::Enter => {
                    let has_pastes = self.editor.has_pastes();
                    let text = self.editor.take();
                    if !has_pastes && text.starts_with('/') && !text.contains('\n') {
                        let command = text.trim_start_matches('/').trim().to_owned();
                        self.command(&command);
                    } else {
                        let images = std::mem::take(&mut self.images);
                        self.paused = false;
                        self.submit(text, images);
                    }
                }
                KeyCode::Up if key.modifiers.is_empty() && self.editor.is_first_visual_row() => {
                    self.prompt_history(false)
                }
                KeyCode::Down if key.modifiers.is_empty() && self.editor.is_last_visual_row() => {
                    self.prompt_history(true)
                }
                KeyCode::Char('/') if self.editor.text.is_empty() => self.command("commands"),
                KeyCode::Char('@') => self.command("files"),
                _ => {
                    self.editor.handle(key);
                }
            },
            Focus::Tree => {
                let agents = self.projection.visible(&self.selected);
                match key.code {
                    KeyCode::Up => self.tree_cursor = self.tree_cursor.saturating_sub(1),
                    KeyCode::Down => {
                        self.tree_cursor =
                            (self.tree_cursor + 1).min(agents.len().saturating_sub(1))
                    }
                    KeyCode::Enter => {
                        if let Some(agent) = agents.get(self.tree_cursor) {
                            self.select(agent.id.clone());
                        }
                    }
                    KeyCode::Left => self.command("parent"),
                    KeyCode::Right => self.command("child"),
                    _ => {}
                }
            }
            Focus::Content => match key.code {
                KeyCode::Home => self.view().scroll = Some(0),
                KeyCode::End => self.view().scroll = None,
                KeyCode::Up | KeyCode::Down => {
                    let current = self.view().row;
                    let next = if key.code == KeyCode::Up {
                        (0..current.min(self.entries.len())).rev().find(|&index| {
                            crate::tui::render::entry_selectable(&self.entries[index])
                        })
                    } else {
                        (current.saturating_add(1)..self.entries.len()).find(|&index| {
                            crate::tui::render::entry_selectable(&self.entries[index])
                        })
                    };
                    if let Some(next) = next {
                        self.view().row = next;
                        self.reveal_row();
                    }
                }
                KeyCode::Enter => self.toggle(),
                KeyCode::Char('[' | ']') => {
                    self.selection = None;
                    let tab = self.view().tab.next(key.code == KeyCode::Char('['));
                    self.view().tab = tab;
                    self.view().scroll = None;
                    self.invalidate_content();
                }
                KeyCode::Char('/') => self.search_editor = Some(Editor::default()),
                KeyCode::Char('n' | 'N') => self.find(key.code == KeyCode::Char('N')),
                KeyCode::Char('y') => self.copy(),
                KeyCode::Char('o') => self.output_menu(),
                KeyCode::Char('c') => {
                    let row = self.view().row;
                    if let Some(job) = self.entries.get(row).and_then(|e| e.job) {
                        self.confirm(ConfirmAction::CancelJob(job));
                    }
                }
                _ => {}
            },
        }
    }
    pub(super) fn scroll(&mut self, delta: isize) {
        let max = self
            .content_rows
            .saturating_sub(self.content_rect.height as usize);
        let old = self.view().scroll.unwrap_or(max);
        let next = old.saturating_add_signed(delta).min(max);
        self.view().scroll = if next == max { None } else { Some(next) };
    }
    pub(super) fn reveal_row(&mut self) {
        let row = self.view().row;
        if let Some(line) = self.render.rows.entry_start(row) {
            self.view().scroll = Some(line);
        }
    }
    pub(super) fn toggle(&mut self) {
        let row = self.view().row;
        if let Some(entry) = self.entries.get(row)
            && entry.expandable
        {
            // Toggling needs metadata, not a clone of the entire trace/document.
            let (key, job, surface, default_open) = (
                entry.key.clone(),
                entry.job,
                entry.surface,
                entry.default_open,
            );
            let all = self.details
                && (job.is_some()
                    || (self.view().tab == Tab::Conversation && surface == model::Surface::Tool));
            let view = self.view();
            let closing = view.is_expanded(&key, all || default_open);
            if closing {
                view.expanded.remove(&key);
                view.collapsed.insert(key);
            } else {
                view.collapsed.remove(&key);
                view.expanded.insert(key);
            }
            self.selection = None;
            self.invalidate_content();
            if !closing && let Some(job) = job {
                self.fetch_output(job);
            }
        }
    }
    pub(super) fn select(&mut self, agent: AgentId) {
        self.selected = agent;
        self.focus = Focus::Content;
        self.selection = None;
        self.invalidate_content();
    }
    pub(super) fn find(&mut self, backwards: bool) {
        let query = self.view().query.to_lowercase();
        if query.is_empty() {
            return;
        }
        let start = self.view().row;
        let count = self.entries.len();
        for step in 1..=count {
            let index = if backwards {
                (start + count - step) % count
            } else {
                (start + step) % count
            };
            if crate::tui::render::entry_selectable(&self.entries[index])
                && self.entries[index].text.to_lowercase().contains(&query)
            {
                self.view().row = index;
                self.reveal_row();
                return;
            }
        }
        self.notice("No matches");
    }
    pub(super) fn copy(&mut self) {
        if self.focus == Focus::Composer
            && let Some(text) = self.editor.selected_text()
        {
            self.clipboard = Some(text.to_owned());
            self.toast("Copied selected input");
            return;
        }
        if let Some((a, b)) = self.selection
            && a != b
        {
            self.clipboard = Some(crate::tui::render::selected_text(&self.render.rows, (a, b)));
        } else {
            let row = self.view().row;
            self.clipboard = self
                .entries
                .get(row)
                .filter(|entry| crate::tui::render::entry_selectable(entry))
                .or_else(|| {
                    self.entries
                        .iter()
                        .rev()
                        .find(|e| e.surface == model::Surface::Agent)
                })
                .map(|e| model::clean(&e.text));
        }
        if self.clipboard.is_some() {
            self.toast("Copied to terminal clipboard");
        }
    }
    pub(super) fn interrupt(&mut self) {
        self.paused = true;
        self.cancel_queue_delivery();
        let Some(session) = self.session.clone() else {
            return;
        };
        // Record the action immediately, before any subsequent prompt can be submitted.
        self.root_notifier().send("Interrupted");
        tokio::spawn(async move {
            session.interrupt().await;
        });
    }
    pub(super) fn prompt_history(&mut self, forward: bool) {
        if self.history.is_empty() {
            return;
        }
        let n = self.history.len();
        if self.history_index.is_none() {
            if forward {
                return;
            }
            self.history_draft = self.editor.clone();
            self.history_index = Some(n - 1);
        } else {
            self.history_index = Some(if forward {
                self.history_index.unwrap() + 1
            } else {
                self.history_index.unwrap().saturating_sub(1)
            });
        }
        if let Some(i) = self.history_index {
            if i >= n {
                self.editor = self.history_draft.clone();
                self.history_index = None;
            } else {
                self.editor.set(self.history[i].clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::*;
    #[tokio::test]
    async fn composer_pastes_submit_in_place_and_restore_history_drafts() {
        let (_root, mut app) = draft_fixture().await;
        // Keep the submitted message queued without starting a provider/session.
        app.creating = true;
        let first = "first\n".repeat(13);
        let second = "second\n".repeat(14);
        app.editor.insert("before after");
        app.editor.cursor = "before ".len();
        app.event(Event::Paste(first.clone()));
        app.editor.insert(" between ");
        app.event(Event::Paste(second.clone()));
        let expected = format!("before {first} between {second}after");
        assert_eq!(app.editor.expanded_text(), expected);
        assert_eq!(app.editor.pastes().count(), 2);
        app.editor.anchor = Some(0);
        app.editor.cursor = app.editor.text.len();
        app.copy();
        assert_eq!(app.clipboard.as_deref(), Some(expected.as_str()));
        app.editor.anchor = None;

        app.history.push("older prompt".into());
        app.prompt_history(false);
        assert_eq!(app.editor.expanded_text(), "older prompt");
        app.prompt_history(true);
        assert_eq!(app.editor.expanded_text(), expected);
        assert_eq!(app.editor.pastes().count(), 2);

        key(&mut app, KeyCode::Enter, M::NONE);
        assert_eq!(app.queue.len(), 1);
        assert_eq!(app.queue[0].text, expected);
        assert!(app.editor.is_empty());
        assert!(!app.editor.has_pastes());
    }
    #[tokio::test]
    async fn composer_short_pastes_and_attachment_removal_use_editor_history() {
        let (_root, mut app) = draft_fixture().await;
        app.event(Event::Paste("short\npaste".into()));
        assert!(!app.editor.has_pastes());
        assert_eq!(app.editor.text, "short\npaste");
        app.event(Event::Paste("long\n".repeat(13)));
        app.command("attachments");
        assert_eq!(app.menu.as_ref().unwrap().items.len(), 1);
        key(&mut app, KeyCode::Delete, M::NONE);
        assert!(!app.editor.has_pastes());
        assert_eq!(app.editor.expanded_text(), "short\npaste");
        key(&mut app, KeyCode::Esc, M::NONE);
        app.editor
            .handle(KeyEvent::new(KeyCode::Char('-'), M::CONTROL));
        assert_eq!(app.editor.pastes().count(), 1);
    }
    #[tokio::test]
    async fn composer_clear_draft_can_undo_text_and_pastes() {
        let (_root, mut app) = draft_fixture().await;
        app.editor.insert("before ");
        app.event(Event::Paste("payload\n".repeat(13)));
        app.editor.insert(" after");
        let expected = app.editor.expanded_text();
        key(&mut app, KeyCode::Char('c'), M::CONTROL);
        assert!(app.editor.is_empty());
        key(&mut app, KeyCode::Char('-'), M::CONTROL);
        assert_eq!(app.editor.expanded_text(), expected);
        assert_eq!(app.editor.pastes().count(), 1);
    }
    #[tokio::test]
    async fn composer_wraps_words_and_vertical_arrows_do_not_skip_to_history() {
        let (_root, mut app) = draft_fixture().await;
        app.history.push("older prompt".into());
        app.editor.insert(&format!("{}ending", "word ".repeat(16)));
        let expected = app.editor.expanded_text();
        let screen = draw(&mut app);
        assert!(screen.contains("word word word"));
        assert!(!app.editor.is_first_visual_row());
        let cursor = app.editor.cursor;
        key(&mut app, KeyCode::Up, M::NONE);
        assert_eq!(app.editor.expanded_text(), expected);
        assert!(app.editor.cursor < cursor);
        assert!(app.history_index.is_none());
        app.editor.cursor = 0;
        key(&mut app, KeyCode::Up, M::NONE);
        assert_eq!(app.editor.expanded_text(), "older prompt");
    }
    #[tokio::test]
    async fn tab_focus_marks_visible_messages_and_tracks_tree_navigation() {
        let (_root, mut app) = fixture().await;
        app.entries = (0..12)
            .map(|index| model::Entry {
                key: format!("message{index}"),
                text: format!("skyhook\nMessage {index}"),
                surface: model::Surface::Agent,
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
            })
            .collect();
        app.content_dirty = false;
        let buffer = draw_buffer(&mut app);
        assert_eq!(buffer[(0, 3)].bg, ratatui::style::Color::Rgb(0, 0, 0));
        assert!(!buffer.content.iter().any(|cell| cell.symbol() == "▌"));
        assert_eq!(app.view().row, 0);
        key(&mut app, KeyCode::Tab, M::NONE);
        let buffer = draw_buffer(&mut app);
        assert!(app.view().row > 0, "Tab should select a visible message");
        let markers: Vec<_> = buffer
            .content
            .iter()
            .filter(|cell| cell.symbol() == "▌")
            .collect();
        assert_eq!(markers.len(), 1);
        assert_eq!(markers[0].fg, ratatui::style::Color::Rgb(255, 255, 255));
        key(&mut app, KeyCode::Tab, M::NONE);
        assert!(!draw(&mut app).contains('▌'));

        let mut child = app.projection.agents[0].clone();
        child.id = child.id.child(1);
        app.projection.agents.push(child);
        draw(&mut app);
        key(&mut app, KeyCode::Tab, M::NONE);
        let buffer = draw_buffer(&mut app);
        let tree_y = app.tree_rect.y;
        assert_eq!(buffer[(2, tree_y + 1)].symbol(), "▌");
        key(&mut app, KeyCode::Down, M::NONE);
        let buffer = draw_buffer(&mut app);
        assert_ne!(buffer[(2, tree_y + 1)].symbol(), "▌");
        assert_eq!(buffer[(6, tree_y + 2)].symbol(), "▌");
        app.command("commands");
        assert!(
            app.menu
                .as_ref()
                .unwrap()
                .items
                .iter()
                .all(|item| !matches!(
                    item.value.as_str(),
                    "commands" | "inspect" | "child" | "parent" | "diagnostics"
                ))
        );
        let buffer = draw_buffer(&mut app);
        assert_ne!(buffer[(6, tree_y + 2)].symbol(), "▌");
        assert_eq!(
            buffer
                .content
                .iter()
                .filter(|cell| cell.symbol() == "▌")
                .count(),
            1
        );
        key(&mut app, KeyCode::Esc, M::NONE);
        key(&mut app, KeyCode::Char('x'), M::CONTROL);
        key(&mut app, KeyCode::Up, M::NONE);
        assert_eq!(app.selected, app.projection.agents[0].id);
        key(&mut app, KeyCode::Char('x'), M::CONTROL);
        key(&mut app, KeyCode::Down, M::NONE);
        assert_eq!(app.selected, app.projection.agents[1].id);
        key(&mut app, KeyCode::Char('x'), M::CONTROL);
        key(&mut app, KeyCode::Char('i'), M::NONE);
        assert!(matches!(app.focus, Focus::Content));
        assert_eq!(app.view().tab, Tab::Conversation);
        app.session.as_ref().unwrap().shutdown().await.unwrap();
    }
}
