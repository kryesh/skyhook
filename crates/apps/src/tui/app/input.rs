use super::events::Work;
use super::extensions::Shown;
use super::queue::Paused;
use super::{App, HistoryBrowse, Pane};
use crate::tui::commands::{self, Command, Context};
use crate::tui::editor::TextField;
use crate::tui::model;
use crate::tui::model::Tab;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers as M};
#[cfg(test)]
use ratatui::layout::Rect;
use skyhook::config::mode::ModeName;

/// What takes the keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputTarget {
    Menu,
    Prompt,
    Pane(Pane),
}

/// Tab and Shift+Tab step through a pane's rows as Down and Up do.
fn rows_key(key: KeyEvent) -> KeyEvent {
    match key.code {
        KeyCode::Tab | KeyCode::BackTab => {
            let reverse = key.code == KeyCode::BackTab || key.modifiers.contains(M::SHIFT);
            KeyEvent::new(if reverse { KeyCode::Up } else { KeyCode::Down }, M::NONE)
        }
        _ => key,
    }
}

impl App {
    /// Show an inspector tab at its latest activity.
    pub(super) fn set_tab(&mut self, tab: Tab) {
        self.panel = Shown::Transcript;
        self.tab = tab;
        self.follow_latest();
        self.invalidate_content();
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
            self.select_mode(&mode);
        }
        self.dirty = true;
    }

    /// Send in `mode` from the next message, if the modes on offer have it. A fixed
    /// root keeps the mode it was opened in.
    pub fn select_mode(&mut self, mode: &ModeName) -> bool {
        if self.launch.identity().root_mode == skyhook::product::RootMode::Fixed {
            return *mode == self.mode;
        }
        let offered = !self.is_view_only() && self.modes().contains_key(mode);
        if offered {
            self.mode.clone_from(mode);
        }
        offered
    }
}

impl App {
    /// Where keys go: an open menu or prompt takes every key, else the focused pane as
    /// the surface shown lets it take them.
    pub fn input_target(&self) -> InputTarget {
        let typing = !self.is_view_only();
        if self.menu.is_some() {
            return InputTarget::Menu;
        }
        if typing && self.prompt_shown() {
            return InputTarget::Prompt;
        }
        InputTarget::Pane(match self.focus {
            Pane::Scope if typing && self.scope_editable() => Pane::Scope,
            Pane::Scope | Pane::Composer if self.composer_shown() => Pane::Composer,
            Pane::Scope | Pane::Composer => Pane::Content,
            pane => pane,
        })
    }

    /// Whether text, pasted or typed, and the wheel over it reach the composer: it has
    /// focus, or no other pane taking text has.
    pub(super) fn composer_input(&self) -> bool {
        matches!(
            self.input_target(),
            InputTarget::Pane(Pane::Composer | Pane::Tree | Pane::Content)
        ) && self.composer_shown()
    }

    pub(super) fn key(&mut self, key: KeyEvent) {
        let pane = match self.input_target() {
            InputTarget::Menu => return self.menu_key(key),
            InputTarget::Prompt => return self.prompt_key(key),
            InputTarget::Pane(pane) => pane,
        };
        // Focus follows the visible surface: a scope panel frozen, or out of view on
        // another tab or agent, hands its keys to the composer, and a composer not
        // shown its keys to the conversation.
        self.focus = match (self.focus, pane) {
            (Pane::Scope, Pane::Composer) | (Pane::Composer | Pane::Scope, Pane::Content) => pane,
            (focus, _) => focus,
        };
        if self.shared_key(key, pane) {
            return;
        }
        match pane {
            Pane::Composer => self.composer_key(key),
            Pane::Scope => self.scope_key(key),
            Pane::Tree => self.tree_key(key),
            Pane::Content => self.content_key(key),
        }
    }

    /// The keys every pane shares: commands and their chords, moving between panes and
    /// modes, paging, interrupting and leaving. Whether `key` was one.
    fn shared_key(&mut self, key: KeyEvent, pane: Pane) -> bool {
        if let Some(prefix) = self.leader.take() {
            if key.code != KeyCode::Esc
                && let Some(action) = commands::action(Some(prefix), key)
            {
                self.command(action);
            }
            return true;
        }
        if let Some(action) = commands::action(None, key) {
            self.command(action);
            return true;
        }
        if commands::is_prefix(key) {
            self.leader = Some(key);
            return true;
        }
        let rows = matches!(pane, Pane::Tree | Pane::Content);
        // A page beside its agents has two panes: Tab moves between them.
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab)
            && rows
            && self.page_frame()
            && !self.render.tree_rect.is_empty()
        {
            self.focus = match pane {
                Pane::Tree => Pane::Content,
                _ => {
                    self.tree_cursor = self.tree_row().unwrap_or(self.tree_cursor);
                    Pane::Tree
                }
            };
            return true;
        }
        match key.code {
            // Tab steps through rows where there are rows; the composer's steps modes.
            KeyCode::Tab | KeyCode::BackTab if rows => false,
            // With the root's mode fixed, Shift+Tab walks the draft's scope fields
            // and comes back to the composer.
            KeyCode::Tab | KeyCode::BackTab => {
                let reverse = key.code == KeyCode::BackTab || key.modifiers.contains(M::SHIFT);
                if reverse
                    && self.scope_editable()
                    && self.launch.identity().root_mode == skyhook::product::RootMode::Fixed
                {
                    self.cycle_scope_focus();
                } else {
                    self.cycle_mode(reverse);
                }
                true
            }
            KeyCode::PageUp | KeyCode::PageDown => {
                let down = key.code == KeyCode::PageDown;
                // A draft taller than its box pages itself, as does the focused
                // tree; otherwise the conversation.
                if pane == Pane::Composer && self.composer_overflows() {
                    self.editor.page(self.composer_rows(), down);
                    self.composer_scroll = None;
                } else if pane == Pane::Tree {
                    self.page_tree(down);
                } else {
                    let page = self.render.content_rect.height as isize;
                    self.scroll(if down { page } else { -page });
                }
                true
            }
            KeyCode::Esc => {
                if self.busy() {
                    self.interrupt();
                } else {
                    self.focus = if self.composer_shown() {
                        Pane::Composer
                    } else {
                        Pane::Content
                    };
                }
                true
            }
            KeyCode::Char('c') if key.modifiers.contains(M::CONTROL) => {
                if pane == Pane::Composer && !self.editor.is_empty() {
                    self.editor.clear();
                } else if self.busy() && !self.root_interrupted() {
                    self.interrupt();
                } else {
                    self.command(Command::Exit);
                }
                true
            }
            _ => false,
        }
    }

    fn composer_key(&mut self, key: KeyEvent) {
        self.composer_scroll = None;
        match key.code {
            KeyCode::Enter if !key.modifiers.is_empty() => {
                self.editor.insert("\n");
            }
            KeyCode::Char('j') if key.modifiers.contains(M::CONTROL) => {
                self.editor.insert("\n");
            }
            KeyCode::Enter => {
                let text = self.editor.text();
                if !self.editor.has_pastes() && text.starts_with('/') && !text.contains('\n') {
                    let command = text.trim_start_matches('/').trim().to_owned();
                    // Attachments stay in the draft while a command runs.
                    let mut submission = self.editor.take();
                    submission.text.clear();
                    self.editor.set_submission(submission);
                    match command.parse() {
                        Ok(command) => self.command(command),
                        Err(_) if command.is_empty() => {}
                        Err(_) => self.toast(format!("Unknown command: /{command}. Use /help.")),
                    }
                } else if self.scope_ready() {
                    // An invalid scope value leaves the draft untouched in the
                    // composer while it is corrected.
                    let submission = self.editor.take();
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
            KeyCode::Up | KeyCode::Down if key.modifiers.is_empty() && self.browses_history() => {
                self.prompt_history(key.code == KeyCode::Down)
            }
            KeyCode::Char('/') if self.editor.text().is_empty() => self.command(Command::Commands),
            KeyCode::Char('@') => {
                // Keep the typed `@`: a chosen file replaces it, cancelling leaves it.
                self.editor.insert("@");
                self.open_files(Some(self.editor.cursor()));
            }
            _ => {
                self.editor.handle(key);
            }
        }
    }

    fn tree_key(&mut self, key: KeyEvent) {
        let key = rows_key(key);
        match key.code {
            KeyCode::Up | KeyCode::Down => self.step_agent(key.code == KeyCode::Up),
            KeyCode::Enter if let Some(&index) = self.tree_agents().get(self.tree_cursor) => {
                self.open_agent(self.projection.agents[index].id.clone());
            }
            KeyCode::Left => self.command(Command::Parent),
            KeyCode::Right => self.command(Command::Child),
            _ => {}
        }
    }

    fn content_key(&mut self, key: KeyEvent) {
        let key = rows_key(key);
        let page = self.page_frame();
        let shown = if page {
            Context::Content
        } else {
            Context::Inspector
        };
        if let Some(command) = commands::context_action(shown, key.code) {
            return self.command(command);
        }
        if page {
            return match key.code {
                KeyCode::Up | KeyCode::Down => self.step_page(key.code == KeyCode::Up),
                KeyCode::Enter => self.toggle_page(None),
                _ => {}
            };
        }
        match key.code {
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
            _ => {}
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

    /// Whether the draft's composer is on screen: not while a child's conversation, a
    /// view-only session or a page is shown.
    pub(crate) fn composer_shown(&self) -> bool {
        self.selected.path().is_empty() && !self.is_view_only() && !self.page_frame()
    }

    /// Copy the composer's selection while it is focused, else the transcript's,
    /// else the selected or last agent message.
    pub(super) fn copy(&mut self) {
        let composer = (self.focus == Pane::Composer)
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
            crate::text::grouped(chars as u64)
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
    use crate::tests::CONFIG;
    use crate::tui::app::PASTE_COLLAPSE_LINES;
    use crate::tui::app::Phase;
    use crate::tui::app::SCROLL_STEP;
    use crate::tui::app::lifecycle::PendingStart;
    use crossterm::event::Event;
    use crossterm::event::MouseEventKind;

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
        assert_eq!(app.focus_of_view(), model::Edge::At(root));
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
        let visible = app.render.composer_rect.height as usize - 2;
        let cursor = app.editor.cursor();
        let inside = Rect::new(2, app.render.composer_rect.y + 1, 1, 1);
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
        app.phase = Phase::Draft {
            root: app.selected.clone(),
            start: Some(PendingStart::Script { path, model, mode }),
        };
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
        app.reconfigure(&format!(
            "{CONFIG}modes:\n  general:\n    capabilities: [read, agents]\n  look:\n    capabilities: []\n  none:\n    capabilities: []"
        ))
        .await;
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
        for (code, mode) in [
            (KeyCode::Tab, "look"),
            (KeyCode::Tab, "none"),
            (KeyCode::Tab, "general"),
            (KeyCode::BackTab, "none"),
        ] {
            key(&mut app, code, M::NONE);
            assert_eq!(app.mode.as_str(), mode);
            assert!(app.focus == Pane::Composer, "{mode}");
            draw(&mut app);
        }
        assert!(draw(&mut app).contains("none · "));
        assert_eq!(app.selected_entry(), None);

        app.focus = Pane::Content;
        key(&mut app, KeyCode::Tab, M::NONE);
        key(&mut app, KeyCode::Tab, M::NONE);
        key(&mut app, KeyCode::BackTab, M::NONE);
        assert_eq!((app.selected_entry(), app.mode.as_str()), (Some(1), "none"));
        assert_eq!(
            markers(&draw_buffer(&mut app)),
            [crate::tui::theme::theme().ui.emphasis]
        );

        let root = app.root_agent().clone();
        draw(&mut app);
        // In the tree, a step views the next agent at its latest activity, wrapping.
        app.focus = Pane::Tree;
        app.scroll_to(Some(0));
        for (code, viewed) in [
            (KeyCode::Tab, &child),
            (KeyCode::BackTab, &root),
            (KeyCode::Down, &child),
            (KeyCode::Down, &root),
            (KeyCode::Up, &child),
        ] {
            key(&mut app, code, M::NONE);
            assert!(app.selected == *viewed && app.focus == Pane::Tree);
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
            assert!(app.selected == *viewed && app.focus == Pane::Tree);
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
