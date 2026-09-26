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
    /// The inspector tab before or after the shown one, wrapping.
    pub(super) fn cycle_tab(&mut self, backwards: bool) {
        self.selection = None;
        self.tab = self.tab.next(backwards);
        self.view().scroll = None;
        self.invalidate_content();
    }

    /// View the agent above or below the viewed one in the tree, wrapping, at its
    /// latest activity, and keep the tree focused for the next step.
    pub(super) fn step_agent(&mut self, backwards: bool) {
        let agents = self.projection.visible(&self.selected);
        let Some(current) = agents.iter().position(|agent| agent.id == self.selected) else {
            return;
        };
        let step = if backwards { agents.len() - 1 } else { 1 };
        let next = agents[(current + step) % agents.len()].id.clone();
        self.select(next);
        let shown = self.projection.visible(&self.selected);
        let cursor = shown.iter().position(|agent| agent.id == self.selected);
        self.tree_cursor = cursor.unwrap_or(0);
        self.view().scroll = None;
        self.focus = Focus::Tree;
    }

    /// Step the mode the next message is sent in, through the configured order.
    fn cycle_mode(&mut self, reverse: bool) {
        let modes = self.modes();
        let current = modes.get_index_of(&self.mode).unwrap_or(0);
        let step = if reverse {
            modes.len().saturating_sub(1)
        } else {
            1
        };
        let next = modes.get_index((current + step) % modes.len().max(1));
        if let Some(mode) = next.map(|(mode, _)| mode.clone()) {
            self.mode = mode;
        }
        self.dirty = true;
    }
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
                    let query = editor.text().to_owned();
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
            let takes_text = self
                .prompts
                .front()
                .is_some_and(|prompt| prompt.takes_text());
            let multiple = self.multiple_questions();
            let editing_question = multiple
                && matches!(
                self.prompts.front().map(|p| &p.kind),
                Some(PromptKind::Questions { questions, .. }) if self.question_index() < questions.len());
            let old_choice = self.prompt_input().choice;
            // Authentication editors contain secrets: never snapshot them for
            // ordinary question draft change detection.
            let mut text_changed = false;
            let old_index = self.question_index();
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
                KeyCode::Left | KeyCode::Right if multiple && !self.question_editing() => {
                    self.switch_question(if key.code == KeyCode::Left { -1 } else { 1 });
                }
                KeyCode::Tab | KeyCode::BackTab if editing_question => {
                    self.set_question_editing(!self.question_editing());
                }
                KeyCode::Up | KeyCode::BackTab => {
                    self.set_question_editing(false);
                    if !options.is_empty() {
                        self.prompt_input_mut().choice =
                            (self.prompt_input().choice + options.len() - 1) % options.len();
                        self.prompt_input_mut().options_scrolled = false;
                    }
                }
                KeyCode::Down | KeyCode::Tab => {
                    self.set_question_editing(false);
                    if !options.is_empty() {
                        self.prompt_input_mut().choice =
                            (self.prompt_input().choice + 1) % options.len();
                        self.prompt_input_mut().options_scrolled = false;
                    }
                }
                KeyCode::Enter => self.answer(),
                // A choice-only prompt has no text to edit.
                _ if !takes_text => {}
                _ => {
                    if editing_question
                        && matches!(
                            key.code,
                            KeyCode::Char(_) | KeyCode::Backspace | KeyCode::Delete
                        )
                    {
                        self.set_question_editing(true);
                    }
                    text_changed = self.prompt_input_mut().editor.handle(key).text_changed;
                }
            }
            if editing_question
                && key.code != KeyCode::Enter
                && self.question_index() == old_index
                && (self.prompt_input().choice != old_choice || text_changed)
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
                self.command(action);
            }
            return;
        }
        if let Some(action) = self.keys.action(None, key) {
            self.command(action);
            return;
        }
        if self.keys.prefix(key) {
            self.leader = Some(key);
            return;
        }
        // Tab steps through whatever has focus: modes in the composer, rows elsewhere.
        let key = match key.code {
            KeyCode::Tab | KeyCode::BackTab if self.focus != Focus::Composer => {
                let reverse = key.code == KeyCode::BackTab || key.modifiers.contains(M::SHIFT);
                KeyEvent::new(if reverse { KeyCode::Up } else { KeyCode::Down }, M::NONE)
            }
            _ => key,
        };
        match key.code {
            KeyCode::Tab | KeyCode::BackTab => {
                let reverse = key.code == KeyCode::BackTab || key.modifiers.contains(M::SHIFT);
                self.cycle_mode(reverse);
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
                if self.focus == Focus::Composer && !self.editor.is_empty() {
                    self.editor.clear();
                } else if self.busy() && !self.root_interrupted() {
                    self.interrupt();
                } else {
                    self.command(Command::Exit);
                }
                return;
            }
            _ => {}
        }
        match self.focus {
            Focus::Composer => match key.code {
                KeyCode::Enter if !key.modifiers.is_empty() => {
                    self.editor.insert("\n");
                }
                KeyCode::Char('j') if key.modifiers.contains(M::CONTROL) => {
                    self.editor.insert("\n");
                }
                KeyCode::Enter => {
                    let has_pastes = self.editor.has_pastes();
                    let mut submission = self.editor.take();
                    let text = &submission.text;
                    if !has_pastes && text.starts_with('/') && !text.contains('\n') {
                        let command = text.trim_start_matches('/').trim().to_owned();
                        // Attachments stay in the draft while a command runs.
                        submission.text.clear();
                        self.editor.set_submission(submission);
                        match command.parse() {
                            Ok(command) => self.command(command),
                            Err(_) if command.is_empty() => {}
                            Err(_) => {
                                self.notice(format!("Unknown command: /{command}. Use /help."))
                            }
                        }
                    } else {
                        self.paused = false;
                        self.submit(submission);
                    }
                }
                KeyCode::Up if key.modifiers.is_empty() && self.editor.is_first_visual_row() => {
                    self.prompt_history(false)
                }
                KeyCode::Down if key.modifiers.is_empty() && self.editor.is_last_visual_row() => {
                    self.prompt_history(true)
                }
                KeyCode::Char('/') if self.editor.text().is_empty() => {
                    self.command(Command::Commands)
                }
                KeyCode::Char('@') => {
                    // Keep the typed `@`: a chosen file replaces it, cancelling leaves it.
                    self.editor.insert("@");
                    self.open_files(Some(self.editor.cursor()));
                }
                _ => {
                    self.editor.handle(key);
                }
            },
            Focus::Tree => {
                let agents = self.projection.visible(&self.selected);
                match key.code {
                    KeyCode::Up | KeyCode::Down => self.step_agent(key.code == KeyCode::Up),
                    KeyCode::Enter => {
                        if let Some(agent) = agents.get(self.tree_cursor) {
                            self.select(agent.id.clone());
                        }
                    }
                    KeyCode::Left => self.command(Command::Parent),
                    KeyCode::Right => self.command(Command::Child),
                    _ => {}
                }
            }
            Focus::Content => match key.code {
                KeyCode::Home => self.view().scroll = Some(0),
                KeyCode::End => self.view().scroll = None,
                KeyCode::Up | KeyCode::Down => {
                    let current = self.view().row;
                    let next = if key.code == KeyCode::Up {
                        (0..current.min(self.entries().len())).rev().find(|&index| {
                            crate::tui::render::entry_selectable(&self.entries()[index])
                        })
                    } else {
                        (current.saturating_add(1)..self.entries().len()).find(|&index| {
                            crate::tui::render::entry_selectable(&self.entries()[index])
                        })
                    };
                    if let Some(next) = next {
                        self.view().row = next;
                        self.reveal_row();
                    }
                }
                KeyCode::Enter => self.toggle(),
                KeyCode::Char('[' | ']') => self.cycle_tab(key.code == KeyCode::Char('[')),
                KeyCode::Char('/') => self.search_editor = Some(Editor::default()),
                KeyCode::Char('n' | 'N') => self.find(key.code == KeyCode::Char('N')),
                KeyCode::Char('y') => self.copy(),
                KeyCode::Char('o') => self.output_menu(),
                KeyCode::Char('c') => {
                    let row = self.view().row;
                    if let Some(job) = self.entries().get(row).and_then(|e| e.job_id()) {
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
        if let Some(entry) = self.entries().get(row)
            && entry.expandable()
        {
            // Borrow only metadata; do not clone the trace/document to toggle.
            let key = entry.key().clone();
            let job = entry.job_id();
            let closing = entry.is_expanded(&self.views[&self.selected], self.tab, self.details);
            let view = self.view();
            view.set_expanded(key, !closing);
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
        let count = self.entries().len();
        for step in 1..=count {
            let index = if backwards {
                (start + count - step) % count
            } else {
                (start + step) % count
            };
            if crate::tui::render::entry_selectable(&self.entries()[index])
                && self.entries()[index].text().to_lowercase().contains(&query)
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
                .entries()
                .get(row)
                .filter(|entry| crate::tui::render::entry_selectable(entry))
                .or_else(|| {
                    self.entries()
                        .iter()
                        .rev()
                        .find(|e| e.surface == model::Surface::Agent)
                })
                .map(|e| model::clean(e.text()));
        }
        if self.clipboard.is_some() {
            self.toast("Copied to terminal clipboard");
        }
    }
    pub(super) fn interrupt(&mut self) {
        self.pause_queue();
        let Some(session) = self.session().cloned() else {
            return;
        };
        // Recorded from the resolved count, so a press that stopped nothing leaves
        // no journal entry claiming otherwise. The row follows the interrupt round
        // trip, so input submitted within it can be journaled first.
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let count = session.interrupt().await;
            let _ = tx.send(Work::Interrupted { count });
        });
    }
    pub(super) fn prompt_history(&mut self, forward: bool) {
        if self.history.is_empty() {
            return;
        }
        let n = self.history.len();
        match self.history_browse.as_mut() {
            None if forward => return,
            None => {
                self.history_browse = Some(HistoryBrowse {
                    index: n - 1,
                    draft: std::mem::take(&mut self.editor),
                });
            }
            Some(browse) => {
                browse.index = if forward {
                    browse.index.saturating_add(1)
                } else {
                    browse.index.saturating_sub(1)
                };
            }
        }
        let browse = self
            .history_browse
            .as_ref()
            .expect("history browse started");
        if browse.index >= n {
            self.editor = self.history_browse.take().unwrap().draft;
        } else {
            self.editor.set(self.history[browse.index].clone());
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
        app.start = StartState::Creating(PendingStart::Script(PathBuf::from("pending.js")));
        let (first, second) = ("first\n".repeat(13), "second\n".repeat(14));
        app.editor.insert("before after");
        app.editor.set_cursor("before ".len());
        app.event(Event::Paste(first.clone()));
        app.editor.insert(" between ");
        app.event(Event::Paste(second.clone()));
        let expected = format!("before {first} between {second}after");
        assert_eq!(app.editor.expanded_text(), expected);
        assert_eq!(app.editor.pastes().count(), 2);
        app.editor.set_selection(Some(0), app.editor.text().len());
        app.copy();
        assert_eq!(app.clipboard.as_deref(), Some(expected.as_str()));
        app.editor.set_selection(None, app.editor.cursor());

        let original_allocation = app.editor.text().as_ptr();
        app.history.push("older prompt".into());
        app.prompt_history(false);
        assert_eq!(app.editor.expanded_text(), "older prompt");
        app.prompt_history(true);
        assert_eq!(app.editor.expanded_text(), expected);
        assert_eq!(app.editor.pastes().count(), 2);
        assert_eq!(app.editor.text().as_ptr(), original_allocation);
        assert!(app.history_browse.is_none());

        key(&mut app, KeyCode::Enter, M::NONE);
        assert_eq!(app.queue.len(), 1);
        assert_eq!(app.queue[0].submission.text, expected);
        assert!(app.editor.is_empty() && !app.editor.has_pastes());
    }

    #[tokio::test]
    async fn composer_short_pastes_attachment_removal_and_clearing_use_editor_history() {
        let (_root, mut app) = draft_fixture().await;
        app.event(Event::Paste("short\npaste".into()));
        assert!(!app.editor.has_pastes());
        assert_eq!(app.editor.text(), "short\npaste");
        app.event(Event::Paste("long\n".repeat(13)));
        app.command(Command::Attachments);
        assert_eq!(app.menu.as_ref().unwrap().kind.items().len(), 1);
        key(&mut app, KeyCode::Delete, M::NONE);
        assert!(!app.editor.has_pastes());
        assert_eq!(app.editor.expanded_text(), "short\npaste");
        key(&mut app, KeyCode::Esc, M::NONE);
        key(&mut app, KeyCode::Char('-'), M::CONTROL);
        assert_eq!(app.editor.pastes().count(), 1);
        // Clearing the draft is undoable, pastes included.
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
        assert!(draw(&mut app).contains("word word word"));
        assert!(!app.editor.is_first_visual_row());
        let cursor = app.editor.cursor();
        key(&mut app, KeyCode::Up, M::NONE);
        assert_eq!(app.editor.expanded_text(), expected);
        assert!(app.editor.cursor() < cursor && app.history_browse.is_none());
        app.editor.set_cursor(0);
        key(&mut app, KeyCode::Up, M::NONE);
        assert_eq!(app.editor.expanded_text(), "older prompt");
    }

    #[tokio::test]
    async fn tab_steps_modes_in_the_composer_and_rows_in_the_focused_pane() {
        let (_root, mut app) = draft_fixture().await;
        let mut config = app.launch.model.config().config().clone();
        config.modes = skyhook::config::Config::from_yaml(
            "modes:\n  general:\n    capabilities: [read]\n  look:\n    capabilities: []\n  none:\n    capabilities: []",
        )
        .unwrap()
        .modes;
        app.launch.model = config.into_runtime().unwrap().default_model();
        // A session offers the modes it was opened with.
        let session = app.launch.create(None).await.unwrap();
        attach(&mut app, session).await;
        let entries = (0..12).map(|index| {
            let text = format!("skyhook\nMessage {index}");
            model::Entry::new(
                model::EntryKey::UnsavedStatus(index),
                text,
                model::Surface::Agent,
            )
        });
        app.install_entries(entries.collect());
        app.content_dirty = false;
        let markers = |buffer: &ratatui::buffer::Buffer| {
            let cells = buffer.content.iter().filter(|cell| cell.symbol() == "▌");
            cells.map(|cell| cell.fg).collect::<Vec<_>>()
        };
        assert!(markers(&draw_buffer(&mut app)).is_empty());
        for (code, mode) in [
            (KeyCode::Tab, "look"),
            (KeyCode::Tab, "none"),
            (KeyCode::Tab, "general"),
            (KeyCode::BackTab, "none"),
        ] {
            key(&mut app, code, M::NONE);
            assert!(app.mode == mode && app.focus == Focus::Composer, "{mode}");
        }
        assert!(draw(&mut app).contains("none · "));
        assert_eq!(app.view().row, 0);

        app.focus = Focus::Content;
        key(&mut app, KeyCode::Tab, M::NONE);
        key(&mut app, KeyCode::Tab, M::NONE);
        key(&mut app, KeyCode::BackTab, M::NONE);
        assert_eq!((app.view().row, app.mode.as_str()), (1, "none"));
        assert_eq!(
            markers(&draw_buffer(&mut app)),
            [ratatui::style::Color::Rgb(255, 255, 255)]
        );

        let mut child = app.projection.agents[0].clone();
        child.id = child.id.child(1);
        app.projection.agents.push(child);
        draw(&mut app);
        let (root, child) = (
            app.projection.agents[0].id.clone(),
            app.projection.agents[1].id.clone(),
        );
        // In the tree, a step views the next agent at its latest activity, wrapping.
        app.focus = Focus::Tree;
        app.view().scroll = Some(0);
        for (code, viewed) in [
            (KeyCode::Tab, &child),
            (KeyCode::BackTab, &root),
            (KeyCode::Down, &child),
            (KeyCode::Down, &root),
            (KeyCode::Up, &child),
        ] {
            key(&mut app, code, M::NONE);
            assert!(app.selected == *viewed && app.focus == Focus::Tree);
            assert_eq!(app.view().scroll, None);
        }
        assert!(draw(&mut app).contains('▌'));
        // The leader arrows do the same from anywhere, and step the inspector tabs.
        key(&mut app, KeyCode::Esc, M::NONE);
        for (code, viewed) in [
            (KeyCode::Up, &root),
            (KeyCode::Up, &child),
            (KeyCode::Down, &root),
        ] {
            chord(&mut app, code);
            assert!(app.selected == *viewed && app.focus == Focus::Tree);
        }
        for (code, tab) in [
            (KeyCode::Right, Tab::Requests),
            (KeyCode::Right, Tab::Jobs),
            (KeyCode::Right, Tab::Conversation),
            (KeyCode::Left, Tab::Jobs),
        ] {
            chord(&mut app, code);
            assert_eq!(app.tab, tab);
        }
    }
}
