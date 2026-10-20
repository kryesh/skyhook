use super::*;

#[derive(Clone, Copy)]
pub enum InputTarget {
    Menu,
    Prompt,
    Composer,
    None,
}

impl App {
    /// Show an inspector tab at its latest activity.
    pub(super) fn set_tab(&mut self, tab: Tab) {
        self.tab = tab;
        self.follow_latest();
        self.invalidate_content();
    }

    /// Drop any text selection and follow the viewed tab's latest activity.
    pub(super) fn follow_latest(&mut self) {
        self.selection = None;
        self.view().top = None;
    }

    /// View the agent above or below the viewed one in the tree, wrapping, at its
    /// latest activity, and keep the tree focused for the next step.
    pub(super) fn step_agent(&mut self, backwards: bool) {
        let (agents, selected) = (&self.projection.agents, &self.selected);
        let shown: Vec<_> = self.projection.visible(selected).collect();
        let Some(current) = shown.iter().position(|&i| &agents[i].id == selected) else {
            return;
        };
        let step = if backwards { shown.len() - 1 } else { 1 };
        self.select(agents[shown[(current + step) % shown.len()]].id.clone());
        let (agents, selected) = (&self.projection.agents, &self.selected);
        let cursor = self
            .projection
            .visible(selected)
            .position(|i| &agents[i].id == selected);
        self.tree_cursor = cursor.unwrap_or(0);
        self.view().top = None;
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

    /// Send in `mode` from the next message, if the modes on offer have it.
    pub fn select_mode(&mut self, mode: &ModeName) -> bool {
        let offered = self.modes().contains_key(mode);
        if offered {
            self.mode.clone_from(mode);
        }
        offered
    }
}

impl App {
    pub fn input_target(&self) -> InputTarget {
        if self.menu.is_some() {
            InputTarget::Menu
        } else if self.prompt_shown() {
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
        if matches!(target, InputTarget::Prompt) {
            let options = self.prompt_options().len();
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
                // As in the composer: clear typed text, and back out of an empty field.
                KeyCode::Char('c') if key.modifiers.contains(M::CONTROL) => {
                    if takes_text && !self.prompt_input().editor.text().is_empty() {
                        self.prompts
                            .front_mut()
                            .expect("active prompt")
                            .clear_input();
                        text_changed = true;
                    } else {
                        self.cancel_prompt();
                        return;
                    }
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
                KeyCode::Up | KeyCode::BackTab | KeyCode::Down | KeyCode::Tab => {
                    self.set_question_editing(false);
                    if options > 0 {
                        let back = matches!(key.code, KeyCode::Up | KeyCode::BackTab);
                        let input = self.prompt_input_mut();
                        let step = if back { options - 1 } else { 1 };
                        input.choice = (input.choice + step) % options;
                        input.options_scrolled = false;
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
                    text_changed = self.prompt_input_mut().editor.handle(key).text_changed();
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
            KeyCode::PageUp | KeyCode::PageDown => {
                let down = key.code == KeyCode::PageDown;
                // A draft taller than its box pages itself; otherwise the conversation.
                if self.focus == Focus::Composer && self.composer_overflows() {
                    self.editor.page(self.composer_rows(), down);
                    self.composer_scroll = None;
                } else {
                    let page = self.content_rect.height as isize;
                    self.scroll(if down { page } else { -page });
                }
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
        if self.focus == Focus::Composer {
            self.composer_scroll = None;
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
                                self.toast(format!("Unknown command: /{command}. Use /help."))
                            }
                        }
                    } else {
                        // Enter resumes the queue; with an edit open it ends the edit,
                        // an empty draft discarding it.
                        match self.paused {
                            Some(Paused::Editing { .. }) if submission.is_empty() => {
                                self.discard_edit();
                                self.deliver_queue();
                            }
                            Some(Paused::Editing { .. }) => {}
                            _ => self.paused = None,
                        }
                        self.submit(submission);
                    }
                }
                KeyCode::Up | KeyCode::Down
                    if key.modifiers.is_empty() && self.browses_history() =>
                {
                    self.prompt_history(key.code == KeyCode::Down)
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
            Focus::Tree => match key.code {
                KeyCode::Up | KeyCode::Down => self.step_agent(key.code == KeyCode::Up),
                KeyCode::Enter => {
                    let index = self
                        .projection
                        .visible(&self.selected)
                        .nth(self.tree_cursor);
                    if let Some(index) = index {
                        self.select(self.projection.agents[index].id.clone());
                    }
                }
                KeyCode::Left => self.command(Command::Parent),
                KeyCode::Right => self.command(Command::Child),
                _ => {}
            },
            Focus::Content => match key.code {
                KeyCode::Home => self.scroll_to_span(Some(0)),
                KeyCode::End => self.scroll_to(None),
                KeyCode::Up | KeyCode::Down => {
                    let current = self.selected_entry().unwrap_or(0);
                    let next = if key.code == KeyCode::Up {
                        (0..current.min(self.entries().len()))
                            .rev()
                            .find(|&index| self.entries()[index].selectable())
                    } else {
                        (current.saturating_add(1)..self.entries().len())
                            .find(|&index| self.entries()[index].selectable())
                    };
                    if let Some(next) = next {
                        self.select_entry(next);
                        self.reveal_row();
                    }
                }
                KeyCode::Enter => self.toggle(),
                KeyCode::Char('[' | ']') => {
                    self.set_tab(self.tab.next(key.code == KeyCode::Char('[')));
                }
                KeyCode::Char('y') => self.copy(),
                KeyCode::Char('o') => self.output_menu(),
                KeyCode::Char('c') => {
                    let row = self.selected_entry();
                    if let Some(job) = row.and_then(|row| self.entries()[row].job_id()) {
                        self.confirm(ConfirmAction::CancelJob(job));
                    }
                }
                _ => {}
            },
        }
    }
    /// Whether the draft is taller than its box, so scrolling moves through it.
    pub(super) fn composer_overflows(&self) -> bool {
        self.editor.rows() > self.composer_rows()
    }
    /// Draft rows the composer box shows.
    fn composer_rows(&self) -> usize {
        self.composer_rect.height.saturating_sub(2) as usize
    }
    /// Scroll the draft's view without moving its cursor.
    pub(super) fn scroll_composer(&mut self, step: isize) {
        let visible = self.composer_rows();
        let bottom = self.editor.rows().saturating_sub(visible);
        // From the rows shown, which a resize may have clamped below the stored top.
        let top = (self.composer_scroll)
            .map_or_else(|| self.editor.cursor_top(visible), |top| top.min(bottom));
        self.composer_scroll = Some(top.saturating_add_signed(step).min(bottom));
        self.dirty = true;
    }
    fn latest_row(&self) -> usize {
        let rows = self.render.rows.len();
        rows.saturating_sub(self.content_rect.height as usize)
    }
    /// The first transcript row in view: the pinned one, else the latest page.
    pub(in crate::tui) fn scroll_offset(&self) -> usize {
        let latest = self.latest_row();
        (self.render.scroll()).map_or(latest, |scroll| scroll.min(latest))
    }
    /// Pin the view's top at `row` of the rows laid out, or follow the newest entries.
    pub(in crate::tui) fn scroll_to(&mut self, row: Option<usize>) {
        let top = row.and_then(|row| crate::tui::render::anchor(self, row));
        self.render.top = match (&top, row) {
            (Some(_), Some(row)) => crate::tui::render::Top::Row(row),
            _ => crate::tui::render::Top::Latest,
        };
        self.view().top = top;
    }
    /// Pin the view's top at `row` of the whole history's span, read there when it is
    /// not loaded, or follow the newest entries.
    pub(in crate::tui) fn scroll_to_span(&mut self, row: Option<usize>) {
        match row.map(|row| crate::tui::render::Track::of(self).locate(row)) {
            Some(crate::tui::render::Spot::Ordinal(ordinal)) => {
                self.view().top = Some(model::Anchor::ordinal(ordinal));
                self.render.top = crate::tui::render::Top::Pending;
            }
            Some(crate::tui::render::Spot::Row(row)) => self.scroll_to(Some(row)),
            None => self.scroll_to(None),
        }
    }
    pub(super) fn scroll(&mut self, delta: isize) {
        // Until the view's anchor is laid out, or while only a window's loading rows
        // are, the rows shown are not where it is: one read by ordinal moves by entries,
        // a row each, as the track spans what is not loaded; any other stays. Only a
        // page read there finds the end, and gives up a place past it for the newest.
        if self.render.top == crate::tui::render::Top::Pending
            || self.window.as_ref().is_some_and(model::Window::opening)
        {
            if let Some(top) = self.view().top.as_mut()
                && let Some((model::Focus::Ordinal(ordinal), _)) = top.place
            {
                *top = model::Anchor::ordinal(ordinal.saturating_add_signed(delta as i64));
            }
            return;
        }
        let latest = self.latest_row();
        let next = self
            .scroll_offset()
            .saturating_add_signed(delta)
            .min(latest);
        // The newest rows follow the newest entries once those are loaded.
        let later = self.window.as_ref().is_some_and(|w| w.content.ends().1);
        self.scroll_to((next != latest || later).then_some(next));
    }
    /// The selected entry, as the content shows it now.
    pub(in crate::tui) fn selected_entry(&self) -> Option<usize> {
        let key = self.viewed()?.selected.as_ref()?;
        let entries = self.entries();
        let hint = self.render.selected;
        if entries.get(hint).is_some_and(|entry| entry.key() == key) {
            return Some(hint);
        }
        entries.iter().position(|entry| entry.key() == key)
    }
    pub(in crate::tui) fn select_entry(&mut self, index: usize) {
        let key = self.entries().get(index).map(|entry| entry.key().clone());
        self.render.selected = index;
        self.view().selected = key;
    }
    pub(super) fn reveal_row(&mut self) {
        let line = (self.selected_entry()).and_then(|row| self.render.rows.entry_start(row));
        if line.is_some() {
            self.scroll_to(line);
        }
    }
    pub(super) fn toggle(&mut self) {
        let row = self.selected_entry();
        if let Some(entry) = row.and_then(|row| self.entries().get(row))
            && entry.expandable()
        {
            // Borrow only metadata; do not clone the trace/document to toggle.
            let (key, open) = (entry.key().clone(), entry.open());
            // An override from a toggle not yet rebuilt into the entries wins.
            let closing = self.view().is_expanded(&key, open);
            self.view().set_expanded(key.clone(), !closing);
            self.selection = None;
            self.content_cache.invalidate_entry(key);
            self.content_dirty = true;
        }
    }
    pub(super) fn select(&mut self, agent: AgentId) {
        self.selected = agent;
        self.focus = Focus::Content;
        self.selection = None;
        self.invalidate_content();
    }
    /// Copy the composer's selection while it is focused, else the transcript's,
    /// else the selected or last agent message.
    pub(super) fn copy(&mut self) {
        let composer = (self.focus == Focus::Composer)
            .then(|| self.editor.selected_text())
            .flatten();
        let text = composer.or_else(|| self.selected_text()).or_else(|| {
            let entries = self.entries();
            let selected = (self.selected_entry())
                .and_then(|row| entries.get(row))
                .filter(|entry| entry.selectable());
            let last = || {
                let mut entries = entries.iter().rev();
                entries.find(|entry| entry.surface == model::Surface::Agent)
            };
            selected.or_else(last).map(|entry| entry.text().to_owned())
        });
        if let Some(text) = text.filter(|text| !text.is_empty()) {
            self.copy_text(text);
        }
    }
    /// The transcript text the pointer selected, unless it holds none. Distinct
    /// endpoints are not enough: a span across a soft wrap has no source text.
    pub(super) fn selected_text(&self) -> Option<String> {
        let text = self.selection.as_ref()?.text();
        (!text.is_empty()).then(|| text.to_owned())
    }
    /// Send `text` to the terminal clipboard and say how much was sent. OSC 52
    /// reports nothing back, so that is all the message can claim.
    pub(super) fn copy_text(&mut self, text: String) {
        let chars = text.chars().count();
        let unit = if chars == 1 { "char" } else { "chars" };
        self.toast(format!(
            "Copied {} {unit} to clipboard",
            crate::tui::format::grouped(chars)
        ));
        self.clipboard = Some(text);
    }
    pub(super) fn interrupt(&mut self) {
        self.pause_queue(Paused::Stopped);
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
    /// Whether Up/Down recall prompts rather than move within the draft.
    fn browses_history(&mut self) -> bool {
        let revision = self.editor.revision();
        self.history_browse
            .take_if(|browse| browse.revision != revision);
        self.history_browse.is_some() || self.editor.is_empty()
    }
    fn prompt_history(&mut self, forward: bool) {
        if self.history.is_empty() {
            return;
        }
        let n = self.history.len();
        let (index, draft) = match self.history_browse.take() {
            None if forward => return,
            None => (n - 1, std::mem::take(&mut self.editor)),
            Some(browse) if forward => (browse.index + 1, browse.draft),
            Some(browse) => (browse.index.saturating_sub(1), browse.draft),
        };
        if index >= n {
            self.editor = draft;
            return;
        }
        self.editor.set(self.history[index].clone());
        let revision = self.editor.revision();
        self.history_browse = Some(HistoryBrowse {
            index,
            draft,
            revision,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::played::*;
    use super::super::tests::*;
    use super::*;

    /// Each tab keeps its own reading state: a job opened in one tab stays closed
    /// in the other, and closing it there leaves the first open; the row stays
    /// with its tab, while a switch returns to the latest
    /// activity; and `/details` reopens what a hidden tab collapsed.
    #[tokio::test]
    async fn tabs_keep_reading_state_independently() {
        let (_root, mut app) = fixture().await;
        run_script(&mut app, "return await tool.exec({command:['true']});").await;
        let key = model::EntryKey::Job(job_named(&app, "exec"));
        let job_row = |app: &App| app.entries().iter().position(|entry| *entry.key() == key);
        for (tab, open) in [
            (Tab::Conversation, false),
            (Tab::Jobs, false),
            (Tab::Jobs, true),
            (Tab::Conversation, true),
        ] {
            app.set_tab(tab);
            settled(&mut app).await;
            let row = job_row(&app).unwrap();
            assert_eq!(app.entries()[row].open(), open, "{tab:?}");
            app.select_entry(row);
            app.toggle();
        }
        let row = app.selected_entry().unwrap();
        app.scroll_to(Some(0));
        app.set_tab(Tab::Jobs);
        settled(&mut app).await;
        let listed = job_row(&app).unwrap() - 1;
        assert_ne!(listed, row);
        app.select_entry(listed);
        app.command(Command::Details);
        app.set_tab(Tab::Conversation);
        settled(&mut app).await;
        assert_eq!(app.selected_entry(), Some(row));
        assert!(app.view().top.is_none());
        assert!(app.entries()[job_row(&app).unwrap()].open());
        app.set_tab(Tab::Jobs);
        settled(&mut app).await;
        assert_eq!(app.selected_entry(), Some(listed));
        // Let go of and shown again, a view on a nested job reads its root's page.
        app.scroll_to(app.render.rows.entry_start(job_row(&app).unwrap()));
        app.release();
        settled(&mut app).await;
        let top = app.view().top.as_ref().and_then(|top| top.key.clone());
        assert_eq!(top, Some(key));
        let root = model::Focus::Job(job_named(&app, "script"));
        assert_eq!(app.focus_of_view(), Some(root));
    }

    /// A draft taller than its box scrolls under the wheel without moving its cursor,
    /// pages its cursor with Page Up, and follows the cursor again once edited.
    #[tokio::test]
    async fn a_tall_draft_scrolls_and_pages_in_its_box() {
        let (_root, mut app) = draft_fixture().await;
        let lines: Vec<_> = (0..40).map(|line| format!("line {line}")).collect();
        app.editor.insert(&lines.join("\n"));
        draw(&mut app);
        assert!(app.composer_overflows());
        let visible = app.composer_rect.height as usize - 2;
        let cursor = app.editor.cursor();
        let inside = Rect::new(2, app.composer_rect.y + 1, 1, 1);
        mouse(&mut app, inside, MouseEventKind::ScrollUp);
        let top = app.composer_scroll.unwrap();
        assert!(top < 40 - visible);
        assert_eq!(app.editor.cursor(), cursor);
        let screen = draw(&mut app);
        assert!(screen.contains(&format!("line {top} ")) && !screen.contains("line 39"));

        key(&mut app, KeyCode::PageUp, M::NONE);
        assert_eq!(app.composer_scroll, None);
        let screen = draw(&mut app);
        let row = 39 - visible;
        assert!(screen.contains(&format!("line {row} ")) && !screen.contains("line 39"));
        key(&mut app, KeyCode::Char('x'), M::NONE);
        assert_eq!(app.composer_scroll, None);

        // Wheel movement starts from the rows shown, even when a resize has since
        // clamped a stored top that no longer fits.
        let bottom = 40 - visible;
        app.composer_scroll = Some(bottom + 20);
        mouse(&mut app, inside, MouseEventKind::ScrollUp);
        assert_eq!(app.composer_scroll, Some(bottom - SCROLL_STEP as usize));
        // A paste long enough to collapse follows the cursor again too.
        let pasted = "pasted\n".repeat(PASTE_COLLAPSE_LINES + 1);
        app.event(Event::Paste(pasted));
        assert_eq!(app.composer_scroll, None);
    }

    /// Every copy reports the Unicode characters it sent, not bytes, and claims
    /// no more than that.
    #[tokio::test]
    async fn copies_report_how_many_characters_were_sent() {
        let (_root, mut app) = draft_fixture().await;
        for (text, toast) in [
            ("界".to_owned(), "Copied 1 char to clipboard"),
            ("é🙂".to_owned(), "Copied 2 chars to clipboard"),
            ("界".repeat(1234), "Copied 1,234 chars to clipboard"),
            ("x".repeat(1_234_567), "Copied 1,234,567 chars to clipboard"),
        ] {
            app.copy_text(text.clone());
            assert_eq!(app.clipboard.take(), Some(text));
            assert_eq!(
                app.toast.take().map(|(toast, _)| toast).as_deref(),
                Some(toast)
            );
        }
    }

    #[tokio::test]
    async fn composer_pastes_submit_in_place() {
        let (_root, mut app) = draft_fixture().await;
        // Keep the submitted message queued without starting a provider/session.
        let (path, model, mode) = ("pending.js".into(), app.model.clone(), app.mode.clone());
        app.creating = Some(PendingStart::Script { path, model, mode });
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
        let toast = format!("Copied {} chars to clipboard", expected.chars().count());
        assert_eq!(app.toast.take().map(|(toast, _)| toast), Some(toast));
        app.editor.set_selection(None, app.editor.cursor());
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
    async fn arrows_recall_history_only_from_an_empty_draft() {
        let (_root, mut app) = draft_fixture().await;
        app.history.push("older prompt".into());
        app.history.push("newer\nprompt".into());
        app.editor.insert(&format!("{}ending", "word ".repeat(16)));
        let expected = app.editor.expanded_text();
        assert!(draw(&mut app).contains("word word word"));
        let cursor = app.editor.cursor();
        key(&mut app, KeyCode::Up, M::NONE);
        assert!(app.editor.cursor() < cursor);
        app.editor.set_cursor(0);
        key(&mut app, KeyCode::Up, M::NONE);
        assert_eq!(app.editor.expanded_text(), expected);

        key(&mut app, KeyCode::Char('c'), M::CONTROL);
        // A multi-line recalled prompt keeps stepping through history.
        for (code, text) in [
            (KeyCode::Up, "newer\nprompt"),
            (KeyCode::Up, "older prompt"),
            (KeyCode::Down, "newer\nprompt"),
            (KeyCode::Down, ""),
        ] {
            key(&mut app, code, M::NONE);
            assert_eq!(app.editor.expanded_text(), text);
        }
        // Leaving history restores the draft's undo history.
        key(&mut app, KeyCode::Char('z'), M::CONTROL);
        assert_eq!(app.editor.expanded_text(), expected);
        key(&mut app, KeyCode::Char('c'), M::CONTROL);
        key(&mut app, KeyCode::Up, M::NONE);
        // Editing the recalled prompt makes it a draft the arrows move within.
        app.editor.insert("!");
        key(&mut app, KeyCode::Up, M::NONE);
        assert_eq!(app.editor.expanded_text(), "newer\nprompt!");
        assert_eq!(app.editor.cursor(), "newer".len());
    }

    #[tokio::test]
    async fn tab_steps_modes_in_the_composer_and_rows_in_the_focused_pane() {
        let (_root, mut app) = draft_fixture().await;
        let mut config = app.launch.model.config().config().clone();
        config.modes = skyhook::config::Config::from_yaml(
            "modes:\n  general:\n    capabilities: [read, agents]\n  look:\n    capabilities: []\n  none:\n    capabilities: []",
        )
        .unwrap()
        .modes;
        app.launch.model = config.into_runtime().unwrap().default_model();
        // A session offers the modes it was opened with.
        let (session, mut model) = played(&app.launch).await;
        attach(&mut app, session).await;
        let (children, _turns) = spawn_children(&mut app, &mut model, 1).await;
        let child = children[0].clone();
        for index in 0..12 {
            app.local_notice(format!("Message {index}"));
        }
        let markers = |buffer: &ratatui::buffer::Buffer| {
            let cells = buffer.content.iter().filter(|cell| cell.symbol() == "▌");
            cells.map(|cell| cell.fg).collect::<Vec<_>>()
        };
        assert!(markers(&draw_buffer(&mut app)).is_empty());
        // Cycling only chooses the next message's mode: the session and the saved
        // preference stay as they were, drawn previews included.
        let journaled = || app.session().unwrap().store().observe().snapshot.applied;
        let (records, remembered) = (journaled(), app.remembered_mode.clone());
        for (code, mode) in [
            (KeyCode::Tab, "look"),
            (KeyCode::Tab, "none"),
            (KeyCode::Tab, "general"),
            (KeyCode::BackTab, "none"),
        ] {
            key(&mut app, code, M::NONE);
            assert_eq!(app.mode.as_str(), mode);
            assert!(app.focus == Focus::Composer, "{mode}");
            draw(&mut app);
        }
        assert!(draw(&mut app).contains("none · "));
        let journaled = app.session().unwrap().store().observe().snapshot.applied;
        assert_eq!((journaled, &app.remembered_mode), (records, &remembered));
        assert_eq!(app.selected_entry(), None);

        app.focus = Focus::Content;
        key(&mut app, KeyCode::Tab, M::NONE);
        key(&mut app, KeyCode::Tab, M::NONE);
        key(&mut app, KeyCode::BackTab, M::NONE);
        assert_eq!((app.selected_entry(), app.mode.as_str()), (Some(1), "none"));
        assert_eq!(
            markers(&draw_buffer(&mut app)),
            [ratatui::style::Color::Rgb(255, 255, 255)]
        );

        let root = app.root_agent().clone();
        draw(&mut app);
        // In the tree, a step views the next agent at its latest activity, wrapping.
        app.focus = Focus::Tree;
        app.scroll_to(Some(0));
        for (code, viewed) in [
            (KeyCode::Tab, &child),
            (KeyCode::BackTab, &root),
            (KeyCode::Down, &child),
            (KeyCode::Down, &root),
            (KeyCode::Up, &child),
        ] {
            key(&mut app, code, M::NONE);
            assert!(app.selected == *viewed && app.focus == Focus::Tree);
            assert!(app.view().top.is_none());
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
