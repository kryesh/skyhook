//! A draft's scope panel: the factory's form, the first entry of the root's
//! conversation, edited while the first message is written, sent with it, then
//! kept there frozen.
use super::*;
use crate::Opening;
use crate::form::{FieldKind, FieldValue, ScopeForm, ScopeReply};
use crate::tui::model::{EntryKey, Surface};
use crate::tui::tool_view::{Document, Role, Run, Section, Wrap};
use std::ops::RangeInclusive;
use unicode_width::UnicodeWidthStr;

pub struct ScopePanel {
    form: ScopeForm,
    drafts: Vec<Draft>,
    pub cursor: usize,
    state: State,
    /// The panel has the keys, as its entry was last built.
    editing: bool,
    /// The focused field is to be brought into view once its rows are laid out.
    reveal: bool,
}

enum State {
    /// Written beside the first message. `invalid` is a field whose text is not a
    /// number in range, until it is edited again.
    Editing { invalid: Option<usize> },
    /// The first message went out with these answers; they no longer change.
    Sent(ScopeReply),
}

/// Rows from the panel's title to its first field: the title and a blank row.
const FIELD_ROWS_START: usize = 2;

/// A field as it is being written: what it takes, and its value so far.
enum Draft {
    /// The option at `selected`.
    Choice {
        options: Vec<String>,
        selected: usize,
    },
    /// A number as typed, checked against `range` when the values are sent.
    Number {
        text: String,
        range: RangeInclusive<u16>,
    },
}

impl Draft {
    fn new(kind: &FieldKind) -> Self {
        match kind {
            FieldKind::Choice(choice) => Self::Choice {
                options: choice.options().to_vec(),
                selected: choice.selected(),
            },
            FieldKind::Integer { value, range } => Self::Number {
                text: value.to_string(),
                range: range.clone(),
            },
        }
    }

    /// Whether the draft still holds the value of `kind`, the field it was made from.
    fn unchanged(&self, kind: &FieldKind) -> bool {
        match (self, kind) {
            (Self::Choice { selected, .. }, FieldKind::Choice(choice)) => {
                *selected == choice.selected()
            }
            (Self::Number { text, .. }, FieldKind::Integer { value, .. }) => {
                *text == value.to_string()
            }
            _ => false,
        }
    }
}

/// A number field whose text is not a number in its range.
#[derive(Debug)]
pub(super) struct Invalid {
    field: usize,
    range: RangeInclusive<u16>,
}

/// How a field reads in the panel.
pub struct ScopeRow<'a> {
    pub label: &'a str,
    pub value: String,
    pub cycles: bool,
    pub invalid: bool,
}

impl ScopePanel {
    fn new(form: ScopeForm) -> Self {
        Self {
            drafts: (form.fields.iter())
                .map(|field| Draft::new(&field.kind))
                .collect(),
            form,
            cursor: 0,
            state: State::Editing { invalid: None },
            editing: false,
            reveal: false,
        }
    }

    /// Whether a value being written differs from the form's: a draft with such a
    /// panel is kept.
    pub fn edited(&self) -> bool {
        let fields = self.form.fields.iter();
        self.editable()
            && !(self.drafts.iter().zip(fields)).all(|(draft, field)| draft.unchanged(&field.kind))
    }

    /// The panel as a conversation entry: title, blank row, a row per field and,
    /// while edited, a blank row, the focused field's help and the key guidance.
    /// One logical line per field, so a line's offset names its field.
    pub(super) fn entry(&self) -> Entry {
        let labels = self.rows().map(|row| row.label.width()).max().unwrap_or(0);
        let mut body = Document::default();
        let blank = || Section::Text {
            indent: 0,
            runs: vec![Run::new(" ", Role::Plain)],
            wrap: Wrap::Hard,
        };
        body.sections.push(blank());
        for (index, row) in self.rows().enumerate() {
            let focused = self.editing && index == self.cursor;
            let value = if row.invalid {
                Role::Error
            } else if self.editable() {
                Role::Indicator
            } else {
                Role::Plain
            };
            let shown = match (row.cycles, self.editable()) {
                (true, true) => format!("◂ {} ▸", row.value),
                (false, true) if focused => format!("{}▌", row.value),
                _ => row.value,
            };
            let marker = if focused { "▸ " } else { "  " };
            body.sections.push(Section::Text {
                indent: 0,
                runs: vec![
                    Run::new(marker, Role::Indicator),
                    Run::new(format!("{:>labels$}  ", row.label), Role::Muted),
                    Run::new(shown, value),
                ],
                wrap: Wrap::Hard,
            });
        }
        if self.editing {
            body.sections.push(blank());
            for text in [self.help(), "↑↓ field · ←→ change · Esc done"] {
                body.sections.push(Section::Text {
                    indent: 0,
                    runs: vec![Run::new(text, Role::Muted)],
                    wrap: Wrap::Hard,
                });
            }
        }
        let header = vec![Run::new(self.title().to_owned(), Role::Muted)];
        let mut entry = Entry::card(EntryKey::Scope, header, Some(body));
        entry.surface = Surface::Panel;
        entry
    }

    /// The field shown `line` logical lines after the panel's title, while fields can be
    /// edited.
    pub(in crate::tui) fn field_at(&self, line: usize) -> Option<usize> {
        let field = line.checked_sub(FIELD_ROWS_START)?;
        (self.editable() && field < self.drafts.len()).then_some(field)
    }

    pub fn title(&self) -> &str {
        &self.form.title
    }

    pub fn editable(&self) -> bool {
        matches!(self.state, State::Editing { .. })
    }

    /// The answers a session starts with, freezing values still being written; the
    /// first field that is not a number in range otherwise.
    pub(super) fn freeze(&mut self) -> Result<ScopeReply, Invalid> {
        let reply = match &self.state {
            State::Sent(reply) => reply.clone(),
            State::Editing { .. } => self.reply()?,
        };
        self.state = State::Sent(reply.clone());
        Ok(reply)
    }

    /// A panel already sent with `form`'s values, as a session the app did not start
    /// shows the scope it runs under; none when a value is out of its field's range.
    pub(super) fn sent(form: ScopeForm) -> Option<Self> {
        let mut panel = Self::new(form);
        panel.freeze().ok()?;
        Some(panel)
    }

    /// A session that failed to start takes nothing with it: its values are written
    /// again until the next start.
    pub(super) fn reopen(&mut self) {
        self.state = State::Editing { invalid: None };
    }

    /// The focused field's help line.
    pub fn help(&self) -> &str {
        &self.form.fields[self.cursor].prompt
    }

    pub fn rows(&self) -> impl Iterator<Item = ScopeRow<'_>> {
        (self.form.fields.iter().zip(&self.drafts).enumerate()).map(|(index, (field, draft))| {
            let value = match draft {
                Draft::Choice { options, selected } => options[*selected].clone(),
                Draft::Number { text, .. } => text.clone(),
            };
            ScopeRow {
                label: &field.name,
                value,
                cycles: matches!(draft, Draft::Choice { .. }),
                invalid: matches!(self.state, State::Editing { invalid } if invalid == Some(index)),
            }
        })
    }

    fn step(&mut self, down: bool) -> bool {
        let last = self.drafts.len().saturating_sub(1);
        match (down, self.cursor) {
            (true, cursor) if cursor < last => self.cursor += 1,
            (false, cursor) if cursor > 0 => self.cursor -= 1,
            (true, _) => return false,
            (false, _) => {}
        }
        true
    }

    fn cycle(&mut self, reverse: bool) {
        if let Draft::Choice { options, selected } = &mut self.drafts[self.cursor] {
            let step = if reverse { options.len() - 1 } else { 1 };
            *selected = (*selected + step) % options.len();
        }
    }

    fn edit(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char(c) => self.insert(c.encode_utf8(&mut [0; 4])),
            KeyCode::Backspace
                if let Draft::Number { text, .. } = &mut self.drafts[self.cursor] =>
            {
                text.pop();
                self.state = State::Editing { invalid: None };
            }
            _ => {}
        }
    }

    /// Type `typed` into the focused number field, which takes only its digits.
    fn insert(&mut self, typed: &str) {
        let Draft::Number { text, .. } = &mut self.drafts[self.cursor] else {
            return;
        };
        let before = text.len();
        text.extend(typed.chars().filter(char::is_ascii_digit));
        if text.len() != before {
            self.state = State::Editing { invalid: None };
        }
    }

    /// The values to send; the first field that is not a number in range otherwise.
    fn reply(&self) -> Result<ScopeReply, Invalid> {
        (self.drafts.iter().enumerate())
            .map(|(field, draft)| match draft {
                Draft::Choice { selected, .. } => Ok(FieldValue::Choice(*selected)),
                Draft::Number { text, range } => (text.parse().ok())
                    .filter(|value| range.contains(value))
                    .map(FieldValue::Integer)
                    .ok_or_else(|| Invalid {
                        field,
                        range: range.clone(),
                    }),
            })
            .collect()
    }
}

impl App {
    /// A draft opens with its factory's form in view.
    pub(super) fn open_scope_panel(&mut self) {
        let form = self
            .launch
            .factory
            .form(self.launch.context(Opening::New(None)));
        self.scope = form
            .filter(|form| !form.fields.is_empty())
            .map(ScopePanel::new);
    }

    /// The panel shows only in the root's conversation.
    fn scope_shown(&self) -> bool {
        self.scope.is_some() && self.tab == Tab::Conversation && self.selected == *self.root_agent()
    }

    pub fn scope_editable(&self) -> bool {
        self.scope_shown() && self.scope.as_ref().is_some_and(ScopePanel::editable)
    }

    /// Whether the panel shows, its entry rebuilt when the keys moved in or out of it.
    pub(super) fn track_scope(&mut self) -> bool {
        let shown = self.scope_shown();
        let editing = shown && self.focus == Focus::Scope;
        if let Some(panel) = self.scope.as_mut()
            && panel.editing != editing
        {
            panel.editing = editing;
            self.content_cache.invalidate_entry(EntryKey::Scope);
            self.content_dirty = true;
        }
        shown
    }

    /// The panel changed: its entry is rebuilt, and while it has the keys the focused
    /// field comes into view.
    pub(super) fn scope_changed(&mut self) {
        if self.focus == Focus::Scope
            && let Some(panel) = self.scope.as_mut()
        {
            panel.reveal = true;
        }
        self.content_cache.invalidate_entry(EntryKey::Scope);
        self.content_dirty = true;
        self.dirty = true;
    }

    /// The panel takes the keys.
    fn enter_scope(&mut self) {
        self.focus = Focus::Scope;
        self.scope_changed();
    }

    /// Scroll no further than it takes to show every row of the focused field, once the
    /// rows are laid out; until the panel is loaded, read from the conversation's start,
    /// where it is.
    pub(in crate::tui) fn reveal_scope(&mut self) {
        let Some(panel) = (self.scope.as_ref()).filter(|panel| panel.reveal) else {
            return;
        };
        if self.focus != Focus::Scope {
            return;
        }
        let entry = (self.entries().iter()).position(|entry| entry.key() == &EntryKey::Scope);
        let Some((entry, start)) =
            entry.and_then(|entry| Some((entry, self.render.rows.entry_start(entry)?)))
        else {
            self.scroll_to_span(Some(0.0));
            return;
        };
        let lines = self.render.rows.header_lines(entry);
        let field =
            |line: &Option<usize>| line.and_then(|line| panel.field_at(line)) == Some(panel.cursor);
        let first = lines.iter().position(field);
        let last = lines.iter().rposition(field);
        if let Some((first, last)) = first.zip(last) {
            let top = self.scroll_offset();
            let height = self.render.content_rect.height as usize;
            let end = (start + last + 1).saturating_sub(height);
            let shown = top.max(end).min(start + first);
            if shown != top {
                self.scroll_to(Some(shown));
            }
        }
        self.scope.as_mut().expect("revealed").reveal = false;
    }

    /// `Edit scope`, or a click on a field: the panel takes the keys.
    pub(super) fn focus_scope(&mut self, field: Option<usize>) {
        if !self.scope_editable() {
            return;
        }
        let panel = self.scope.as_mut().expect("editable");
        if let Some(field) = field {
            panel.cursor = field.min(panel.drafts.len().saturating_sub(1));
        }
        self.enter_scope();
    }

    /// Shift+Tab: into the first field, on to the next, and back to the composer
    /// after the last.
    pub(super) fn cycle_scope_focus(&mut self) {
        if !self.scope_editable() {
            return;
        }
        let panel = self.scope.as_mut().expect("editable");
        match self.focus {
            Focus::Scope if panel.step(true) => self.scope_changed(),
            Focus::Scope => {
                self.focus = Focus::Composer;
                self.scope_changed();
            }
            _ => {
                panel.cursor = 0;
                self.enter_scope();
            }
        }
    }

    pub(super) fn scope_key(&mut self, key: KeyEvent) {
        let panel = self.scope.as_mut().expect("editable");
        match key.code {
            KeyCode::Up => {
                panel.step(false);
            }
            KeyCode::Down => {
                panel.step(true);
            }
            KeyCode::Left => panel.cycle(true),
            KeyCode::Right => panel.cycle(false),
            // Enter cycles a choice; on a typed field it moves on, leaving the
            // panel after its last row.
            KeyCode::Enter => match panel.drafts[panel.cursor] {
                Draft::Choice { .. } => panel.cycle(false),
                Draft::Number { .. } => {
                    if !panel.step(true) {
                        self.focus = Focus::Composer;
                    }
                }
            },
            KeyCode::Char(_) | KeyCode::Backspace => panel.edit(key.code),
            _ => {}
        }
        self.scope_changed();
    }

    /// A paste into the focused field.
    pub(super) fn scope_paste(&mut self, text: &str) {
        if let Some(panel) = self.scope.as_mut().filter(|panel| panel.editable()) {
            panel.insert(text);
            self.scope_changed();
        }
    }

    /// Whether the panel's values can be sent; when one is invalid, the panel
    /// shows which.
    pub(super) fn scope_ready(&mut self) -> bool {
        let Some(panel) = self.scope.as_ref().filter(|panel| panel.editable()) else {
            return true;
        };
        match panel.reply() {
            Ok(_) => true,
            Err(invalid) => {
                self.scope_invalid(invalid);
                false
            }
        }
    }

    /// Check the panel's values for the submission that would start the session; an
    /// empty draft takes the form's own message. Returns false, leaving the
    /// submission as it was, when a value is invalid; the panel then shows which.
    pub(super) fn take_scope(&mut self, submission: &mut Submission) -> bool {
        let Some(panel) = self.scope.as_ref().filter(|panel| panel.editable()) else {
            return true;
        };
        match panel.reply() {
            Ok(_) => {
                if submission.is_empty() {
                    submission.text.clone_from(&panel.form.message);
                }
                true
            }
            Err(invalid) => {
                self.scope_invalid(invalid);
                false
            }
        }
    }

    /// A draft whose last unsent message is gone writes its values again: nothing is
    /// left to carry them.
    pub(super) fn release_scope(&mut self) {
        if self.session().is_some() || self.creating.is_some() || !self.queue.is_empty() {
            return;
        }
        if let Some(panel) = self.scope.as_mut().filter(|panel| !panel.editable()) {
            panel.reopen();
            self.scope_changed();
        }
    }

    /// The values leave with the first queued message: from then on they are frozen.
    pub(super) fn send_scope(&mut self) {
        if let Some(panel) = self.scope.as_mut().filter(|panel| panel.editable()) {
            panel.freeze().expect("checked by take_scope");
            self.scope_changed();
        }
    }

    pub(super) fn scope_invalid(&mut self, Invalid { field, range }: Invalid) {
        let Some(panel) = self.scope.as_mut() else {
            return;
        };
        panel.state = State::Editing {
            invalid: Some(field),
        };
        panel.cursor = field;
        let message = format!(
            "{}: enter a number from {} to {}",
            panel.form.fields[field].name,
            range.start(),
            range.end()
        );
        self.enter_scope();
        self.toast(message);
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{draft_fixture, draw_sized, key, viewing};
    use super::*;
    use crate::form::{Choice, Field};
    use crate::tui::render::tests::statuses;
    use crate::{SessionContext, SessionFactory};
    use crossterm::event::{KeyCode, KeyModifiers as M};
    use std::sync::Arc;

    /// A level and a depth, then this many more choices.
    struct Scoped(usize);
    impl SessionFactory for Scoped {
        fn form(&self, _context: SessionContext<'_>) -> Option<ScopeForm> {
            let more = (0..self.0).map(|index| Field {
                name: format!("extra {index}"),
                prompt: "Another".into(),
                kind: FieldKind::Choice(Choice::new(vec!["a".into(), "b".into()], 0).unwrap()),
            });
            Some(ScopeForm {
                title: "Scope".into(),
                fields: [
                    Field {
                        name: "level".into(),
                        prompt: "How far".into(),
                        kind: FieldKind::Choice(
                            Choice::new(vec!["low".into(), "high".into()], 0).unwrap(),
                        ),
                    },
                    Field {
                        name: "depth".into(),
                        prompt: "How deep".into(),
                        kind: FieldKind::Integer {
                            value: 2,
                            range: 1..=5,
                        },
                    },
                ]
                .into_iter()
                .chain(more)
                .collect(),
                message: "Start.".into(),
            })
        }
    }

    /// A session the app opens rather than starts shows its extension's scope, already
    /// sent: frozen, with no field to edit.
    #[tokio::test]
    async fn an_opened_session_shows_its_scope_frozen() {
        let (_root, app) = draft_fixture().await;
        let form = Scoped(0)
            .form(app.launch.context(Opening::New(None)))
            .unwrap();
        let session = app.launch.create(Opening::New(None)).await.unwrap().session;
        let mut observation = PreparedObservation::load(session, Arc::new(()), app.budget)
            .await
            .unwrap();
        observation.scope = Some(form);
        let mut opened = app.sibling(Some(observation), app.launch.clone(), app.tx.clone());
        assert!(opened.scope.is_some() && !opened.scope_editable());
        let screen = draw_sized(&mut opened, 100, 24);
        let frozen = ["level  low", "depth  2"]
            .iter()
            .all(|row| screen.contains(row));
        assert!(frozen && !screen.contains('◂'), "{screen}");
        opened.session().unwrap().shutdown().await.unwrap();
    }

    /// The panel is edited beside the composer: Shift+Tab walks the fields, a choice
    /// cycles, a number is typed and checked, and the values leave with the first
    /// message, which the form supplies when the draft is empty. From then on the
    /// panel is frozen.
    #[tokio::test]
    async fn the_scope_panel_is_edited_beside_the_composer_and_freezes_with_the_first_message() {
        let (_root, mut app) = draft_fixture().await;
        app.launch.factory = Arc::new(Scoped(0));
        app.open_scope_panel();
        assert!(app.scope_editable() && app.focus == Focus::Composer);

        // Shift+Tab walks the fields and returns to the composer after the last.
        app.launch.identity.root_mode = crate::RootMode::Fixed;
        key(&mut app, KeyCode::BackTab, M::SHIFT);
        assert_eq!(
            (app.focus, app.scope.as_ref().unwrap().cursor),
            (Focus::Scope, 0)
        );
        key(&mut app, KeyCode::Right, M::NONE);
        key(&mut app, KeyCode::BackTab, M::SHIFT);
        key(&mut app, KeyCode::Backspace, M::NONE);
        key(&mut app, KeyCode::Char('9'), M::NONE);
        key(&mut app, KeyCode::BackTab, M::SHIFT);
        assert_eq!(app.focus, Focus::Composer);
        let screen = draw_sized(&mut app, 100, 24);
        assert!(screen.contains("◂ high ▸"), "{screen}");
        // Wrapped onto several rows, a field's rows all name it, and each field keeps some.
        let narrow = draw_sized(&mut app, 24, 40);
        let mut fields = (app.render.hits.iter())
            .filter_map(|(rect, hit)| match hit {
                Hit::ScopeField(field) => Some((rect.y, *field)),
                _ => None,
            })
            .collect::<Vec<_>>();
        fields.sort_unstable();
        assert!(fields.len() > 2, "{fields:?}\n{narrow}");
        assert!(
            fields.array_windows().all(|[a, b]| a.1 <= b.1),
            "{fields:?}"
        );
        assert_eq!(
            (fields[0].1, fields.last().unwrap().1),
            (0, 1),
            "{fields:?}"
        );

        // Out of range: the submission is refused and the field is pointed at.
        app.paused = Some(Paused::Stopped);
        key(&mut app, KeyCode::Enter, M::NONE);
        assert!(app.queue.is_empty() && app.focus == Focus::Scope);
        let panel = app.scope.as_ref().unwrap();
        assert!(panel.editable() && panel.rows().nth(1).unwrap().invalid);

        key(&mut app, KeyCode::Backspace, M::NONE);
        key(&mut app, KeyCode::Char('4'), M::NONE);
        key(&mut app, KeyCode::Esc, M::NONE);
        // Enter resumed delivery above; hold it so the queued row can be read.
        app.paused = Some(Paused::Stopped);
        app.submit(Submission {
            text: String::new(),
            attachments: Vec::new(),
        });
        assert!(!app.scope.as_ref().unwrap().editable());
        let scope = app.scope.as_mut().unwrap().freeze().expect("values sent");
        assert_eq!((scope[0].choice(), scope[1].integer()), (Some(1), Some(4)));
        assert_eq!(app.queue.len(), 1);
        assert_eq!(app.queue[0].submission.text, "Start.");
        assert!(!app.scope_editable() && !app.command_available(Command::Scope));
        app.command(Command::Scope);
        assert_eq!(app.focus, Focus::Composer);
        // Frozen: the values stay in view without the editing marks or help line.
        let screen = draw_sized(&mut app, 100, 24);
        assert!(
            screen.contains("level  high") && !screen.contains('◂'),
            "{screen}"
        );
        // Withdrawing the message that would have carried them opens the values again.
        assert!(app.remove_queued(app.queue[0].id).is_some());
        assert!(app.scope_editable());
    }

    /// A panel taller than the view scrolls only as far as the focused field needs.
    #[tokio::test]
    async fn focusing_a_field_below_the_view_scrolls_it_into_view() {
        let (_root, mut app) = draft_fixture().await;
        app.launch.factory = Arc::new(Scoped(8));
        app.launch.identity.root_mode = crate::RootMode::Fixed;
        app.open_scope_panel();
        key(&mut app, KeyCode::BackTab, M::SHIFT);
        let shown = |app: &App| {
            (app.render.hits.iter())
                .filter_map(|(_, hit)| match hit {
                    Hit::ScopeField(field) => Some(*field),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let screen = draw_sized(&mut app, 60, 12);
        assert!(
            shown(&app).contains(&0) && !shown(&app).contains(&9),
            "{screen}"
        );
        for _ in 0..9 {
            key(&mut app, KeyCode::Down, M::NONE);
        }
        let screen = draw_sized(&mut app, 60, 12);
        assert!(shown(&app).contains(&9), "{screen}");
        assert!(screen.contains("▸ extra 7"), "{screen}");
    }

    /// The panel is the first entry of the root's conversation: it scrolls away with
    /// the history, comes back into view when it takes the keys, and a child's view
    /// has none.
    #[tokio::test]
    async fn the_scope_panel_scrolls_with_the_root_conversation_and_stays_out_of_child_views() {
        let (_root, _app, session) = statuses(60).await;
        let (_viewer, mut app) = viewing(&session, Tab::Conversation).await;
        app.launch.factory = Arc::new(Scoped(0));
        app.launch.identity.root_mode = crate::RootMode::Fixed;
        app.open_scope_panel();
        app.invalidate_content();
        app.focus = Focus::Composer;
        // Following the newest statuses, the panel has scrolled away.
        let screen = draw_sized(&mut app, 100, 24);
        assert!(
            screen.contains("status 59") && !screen.contains("level"),
            "{screen}"
        );
        assert_eq!(app.entries()[0].key(), &EntryKey::Scope);
        // Taking the keys brings it back to the top of the view.
        key(&mut app, KeyCode::BackTab, M::SHIFT);
        let screen = draw_sized(&mut app, 100, 24);
        assert!(
            app.focus == Focus::Scope && screen.contains("▸ level  ◂ low ▸"),
            "{screen}"
        );
        // A child's conversation shows no panel and takes no scope keys.
        app.select(session.root_agent().child(1));
        let screen = draw_sized(&mut app, 100, 24);
        assert!(!screen.contains("level"), "{screen}");
        assert!(!app.scope_editable());
        key(&mut app, KeyCode::BackTab, M::SHIFT);
        assert_ne!(app.focus, Focus::Scope);
    }
}
