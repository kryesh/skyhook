//! Rich editing is deliberately confined to the message composer. `text` contains
//! one object-replacement character per paste; use `expanded_text` to copy it and
//! `take` to send it with its attachments. Use `set` rather than assigning to `text`.
use std::{
    collections::{BTreeMap, VecDeque},
    ops::Range,
    sync::Arc,
};

use super::editor::{EditOutcome, TextField, grapheme, push_history};
use skyhook::media::Attachment;

use unicode_segmentation::UnicodeSegmentation;
use zeroize::Zeroize;

mod layout;
// Keep the composer facade stable even when callers infer these layout types.
pub use layout::ComposerLayout;

const OBJECT: &str = "\u{fffc}";

/// Shared by undo snapshots, so it zeroizes when the last one drops.
#[derive(Debug, PartialEq, Eq)]
struct Paste {
    id: usize,
    content: String,
    lines: usize,
}
impl Drop for Paste {
    fn drop(&mut self) {
        self.content.zeroize();
    }
}
impl Paste {
    fn label(&self) -> String {
        // A trailing newline terminates a line rather than creating an extra one.
        let lines = self.lines;
        format!(
            "[Pasted text #{} · {} {}]",
            self.id,
            lines,
            if lines == 1 { "line" } else { "lines" }
        )
    }
}

/// What the composer sends: typed text with pastes expanded, plus attachments.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Submission {
    pub text: String,
    pub attachments: Vec<Attachment>,
}
impl Submission {
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty() && self.attachments.is_empty()
    }
}
#[cfg(test)]
impl From<&str> for Submission {
    fn from(text: &str) -> Self {
        Self {
            text: text.into(),
            attachments: Vec::new(),
        }
    }
}

#[derive(Clone, Default)]
struct Snapshot {
    text: String,
    pastes: BTreeMap<usize, Arc<Paste>>,
    attachments: Vec<Arc<Attachment>>,
    cursor: usize,
    anchor: Option<usize>,
}
impl Drop for Snapshot {
    fn drop(&mut self) {
        self.text.zeroize();
    }
}

#[derive(Clone)]
pub struct Composer {
    text: String,
    cursor: usize,
    anchor: Option<usize>,
    pastes: BTreeMap<usize, Arc<Paste>>,
    attachments: Vec<Arc<Attachment>>,
    next_id: usize,
    undo: VecDeque<Snapshot>,
    redo: VecDeque<Snapshot>,
    width: usize,
    preferred_column: Option<usize>,
    /// Bumped by every edit, so a caller can tell whether the draft changed.
    revision: u64,
    /// Last layout, keyed by a hash of everything it was built from.
    layout_cache: std::cell::RefCell<Option<(u64, std::sync::Arc<ComposerLayout>)>>,
}
impl Default for Composer {
    fn default() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            anchor: None,
            pastes: BTreeMap::new(),
            attachments: Vec::new(),
            next_id: 1,
            undo: VecDeque::new(),
            redo: VecDeque::new(),
            width: 80,
            preferred_column: None,
            revision: 0,
            layout_cache: Default::default(),
        }
    }
}
impl Drop for Composer {
    fn drop(&mut self) {
        self.clear_sensitive();
    }
}

/// Selection and cursor mapping use the same wrapping decisions as these
/// renderable runs. Paste runs are indivisible.
#[derive(Clone)]
struct Token {
    source: Range<usize>,
    text: String,
    /// Newlines count as whitespace; a paste is an atomic non-whitespace item.
    whitespace: bool,
    newline: bool,
    paste: bool,
}

impl Token {
    fn plain(text: &str, offset: usize) -> impl Iterator<Item = Self> + '_ {
        text.grapheme_indices(true).map(move |(i, g)| {
            let newline = g == "\n" || g == "\r\n";
            let text = if newline {
                String::new()
            } else if g == "\t" {
                "    ".to_owned()
            } else {
                g.chars()
                    .map(|c| if c.is_control() { '\u{fffd}' } else { c })
                    .collect()
            };
            Self {
                source: offset + i..offset + i + g.len(),
                text,
                whitespace: newline || g.chars().all(char::is_whitespace),
                newline,
                paste: false,
            }
        })
    }

    fn boundary(tokens: &[Self], text_len: usize, offset: usize) -> usize {
        let offset = offset.min(text_len);
        tokens
            .iter()
            .find(|t| t.source.start < offset && offset < t.source.end)
            .map_or(offset, |t| t.source.start)
    }
}
impl Composer {
    /// Move to a source-byte offset, snapping inside graphemes/pastes to the start.
    #[cfg(test)]
    pub fn set_cursor(&mut self, cursor: usize) {
        self.set_selection(None, cursor);
    }
    #[cfg(test)]
    pub fn set_selection(&mut self, anchor: Option<usize>, cursor: usize) {
        self.cursor = self.boundary(cursor);
        self.anchor = anchor.map(|a| self.boundary(a));
        self.preferred_column = None;
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.attachments.is_empty()
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn attachments(&self) -> &[Arc<Attachment>] {
        &self.attachments
    }
    pub fn attach(&mut self, attachment: Attachment) -> EditOutcome {
        self.save();
        self.attachments.push(Arc::new(attachment));
        EditOutcome::Changed
    }
    pub fn remove_attachment(&mut self, index: usize) -> EditOutcome {
        if index >= self.attachments.len() {
            return EditOutcome::Ignored;
        }
        self.save();
        self.attachments.remove(index);
        EditOutcome::Changed
    }
    /// Delete a source range as one undoable edit.
    pub fn delete(&mut self, range: Range<usize>) {
        self.normalize();
        self.save();
        self.anchor = None;
        self.delete_range(range);
    }
    pub fn has_pastes(&self) -> bool {
        !self.pastes.is_empty()
    }
    /// Inline items in document order, not insertion order.
    pub fn pastes(&self) -> impl Iterator<Item = (usize, &str)> {
        self.pastes.values().map(|p| (p.id, p.content.as_str()))
    }
    pub fn paste(&self, id: usize) -> Option<&str> {
        self.pastes
            .values()
            .find(|p| p.id == id)
            .map(|p| p.content.as_str())
    }
    pub fn remove_paste(&mut self, id: usize) -> EditOutcome {
        let Some(offset) = self
            .pastes
            .iter()
            .find(|(_, p)| p.id == id)
            .map(|(&offset, _)| offset)
        else {
            return EditOutcome::Ignored;
        };
        self.normalize();
        self.save();
        let cursor = self.cursor;
        self.delete_range(offset..offset + OBJECT.len());
        self.cursor = if cursor > offset {
            cursor - OBJECT.len()
        } else {
            cursor
        };
        self.anchor = None;
        self.normalize();
        EditOutcome::Changed
    }
    pub fn set_width(&mut self, width: usize) {
        let width = width.max(1);
        if self.width != width {
            self.preferred_column = None;
        }
        self.width = width;
    }
    pub fn expanded_text(&self) -> String {
        self.expand(0..self.text.len())
    }
    pub fn selected_text(&self) -> Option<String> {
        self.selection()
            .filter(|r| !r.is_empty())
            .map(|range| self.expand(range))
    }
    fn expand(&self, range: Range<usize>) -> String {
        let mut out = String::new();
        let mut start = range.start;
        for (&offset, paste) in self.pastes.range(range.clone()) {
            out.push_str(&self.text[start..offset]);
            out.push_str(&paste.content);
            start = offset + OBJECT.len();
        }
        out.push_str(&self.text[start..range.end]);
        out
    }
    pub fn clear_sensitive(&mut self) {
        self.text.zeroize();
        self.pastes.clear();
        self.attachments.clear();
        self.undo.clear();
        self.redo.clear();
        self.cursor = 0;
        self.anchor = None;
        self.preferred_column = None;
        self.next_id = 1;
        self.revision += 1;
        self.layout_cache.take();
    }
    /// Replace the whole document as plain text (history/menu recall).
    pub fn set(&mut self, text: String) -> EditOutcome {
        let text_changed = self.text != text || self.has_pastes() || !self.attachments.is_empty();
        self.clear_sensitive();
        self.text = text;
        self.cursor = self.text.len();
        EditOutcome::handled(text_changed)
    }
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            text: self.text.clone(),
            pastes: self.pastes.clone(),
            attachments: self.attachments.clone(),
            cursor: self.cursor,
            anchor: self.anchor,
        }
    }
    fn restore(&mut self, mut snapshot: Snapshot) {
        self.text.zeroize();
        self.text = std::mem::take(&mut snapshot.text);
        self.pastes = std::mem::take(&mut snapshot.pastes);
        self.attachments = std::mem::take(&mut snapshot.attachments);
        self.cursor = snapshot.cursor;
        self.anchor = snapshot.anchor;
        self.preferred_column = None;
        self.revision += 1;
    }
    fn save(&mut self) {
        let snapshot = self.snapshot();
        push_history(&mut self.undo, snapshot);
        self.redo.clear();
        self.preferred_column = None;
        self.revision += 1;
    }
    fn tokens(&self) -> Vec<Token> {
        let mut tokens = Vec::new();
        let mut offset = 0;
        while offset < self.text.len() {
            if let Some(paste) = self.pastes.get(&offset) {
                tokens.push(Token {
                    source: offset..offset + OBJECT.len(),
                    text: paste.label(),
                    whitespace: false,
                    newline: false,
                    paste: true,
                });
                offset += OBJECT.len();
            } else {
                let end = self
                    .pastes
                    .range(offset..)
                    .next()
                    .map_or(self.text.len(), |(&i, _)| i);
                tokens.extend(Token::plain(&self.text[offset..end], offset));
                offset = end;
            }
        }
        tokens
    }
    fn boundary(&self, offset: usize) -> usize {
        self.unit(offset).start
    }
    fn normalize(&mut self) {
        self.cursor = self.boundary(self.cursor);
        self.anchor = self.anchor.map(|a| self.boundary(a));
    }
    fn delete_range(&mut self, range: Range<usize>) {
        let removed = range.end - range.start;
        if removed == 0 {
            return;
        }
        let old = std::mem::take(&mut self.pastes);
        self.pastes = old
            .into_iter()
            .filter_map(|(i, p)| {
                if range.contains(&i) {
                    None
                } else {
                    Some((if i >= range.end { i - removed } else { i }, p))
                }
            })
            .collect();
        self.text.replace_range(range.clone(), "");
        self.cursor = range.start;
    }
    fn delete_selection(&mut self) -> bool {
        let range = self.selection();
        self.anchor = None;
        if let Some(range) = range.filter(|r| !r.is_empty()) {
            self.delete_range(range);
            true
        } else {
            false
        }
    }
    fn insert_raw(&mut self, text: &str) {
        let old = std::mem::take(&mut self.pastes);
        self.pastes = old
            .into_iter()
            .map(|(i, p)| (if i >= self.cursor { i + text.len() } else { i }, p))
            .collect();
        self.text.insert_str(self.cursor, text);
        self.cursor += text.len();
    }
    /// Insert `content` as an inline item. Pasting the same content again right
    /// after its item expands that item into editable text instead.
    pub fn insert_paste(&mut self, mut content: String) -> EditOutcome {
        if content.is_empty() {
            return EditOutcome::Handled;
        }
        self.normalize();
        self.save();
        if !self.delete_selection()
            && let Some(offset) = self.cursor.checked_sub(OBJECT.len())
            && self
                .pastes
                .get(&offset)
                .is_some_and(|p| p.content == content)
        {
            self.delete_range(offset..self.cursor);
            self.insert_raw(&content);
            content.zeroize();
            return EditOutcome::Changed;
        }
        let offset = self.cursor;
        self.insert_raw(OBJECT);
        let id = self.next_id;
        self.next_id += 1;
        let lines = content.lines().count().max(1);
        self.pastes
            .insert(offset, Arc::new(Paste { id, content, lines }));
        EditOutcome::Changed
    }
    /// Clear a local draft as one undoable edit (for Ctrl+C). Unlike sending or
    /// clearing sensitive data, this intentionally retains an undo snapshot.
    pub fn clear(&mut self) -> EditOutcome {
        self.normalize();
        let text_changed = !self.is_empty();
        if text_changed {
            self.save();
        }
        self.text.zeroize();
        self.pastes.clear();
        self.attachments.clear();
        self.cursor = 0;
        self.anchor = None;
        self.preferred_column = None;
        EditOutcome::handled(text_changed)
    }

    /// Expand pastes and take the attachments. Sending starts a fresh undo history.
    pub fn take(&mut self) -> Submission {
        let text = self.expanded_text();
        let attachments = std::mem::take(&mut self.attachments);
        self.clear_sensitive();
        let attachments = attachments.into_iter().map(Arc::unwrap_or_clone).collect();
        Submission { text, attachments }
    }
    /// Rows the draft takes at the composer's width.
    pub fn rows(&self) -> usize {
        self.layout(self.width).rows.len()
    }
    /// The first of `visible` rows shown while the view follows the cursor.
    pub fn cursor_top(&self, visible: usize) -> usize {
        let (row, _) = self.layout(self.width).cursor;
        row.saturating_sub(visible.saturating_sub(1))
    }
    /// Move the cursor `rows` rows, keeping its column, as a page key does.
    pub fn page(&mut self, rows: usize, down: bool) {
        let cursor = self.rows_away(rows, down);
        self.place(cursor, None);
    }
    /// Where the cursor lands `rows` rows away, nearest its preferred column.
    fn rows_away(&mut self, rows: usize, down: bool) -> usize {
        let layout = self.layout(self.width);
        let (row, column) = layout.cursor;
        let column = *self.preferred_column.get_or_insert(column);
        let target = if down {
            (row + rows).min(layout.rows.len() - 1)
        } else {
            row.saturating_sub(rows)
        };
        if target == row {
            self.cursor
        } else {
            layout.closest(target, column)
        }
    }
    /// Replace the draft with a submission taken back for editing.
    pub fn set_submission(&mut self, submission: Submission) {
        self.set(submission.text);
        self.attachments = submission.attachments.into_iter().map(Arc::new).collect();
    }
}

impl TextField for Composer {
    /// Raw document text; inline pastes occupy one object-replacement grapheme.
    fn text(&self) -> &str {
        &self.text
    }
    fn cursor(&self) -> usize {
        self.cursor
    }
    fn anchor(&self) -> Option<usize> {
        self.anchor
    }
    /// A whole paste, or a grapheme of the text between pastes.
    fn unit(&self, offset: usize) -> Range<usize> {
        let before = self.pastes.range(..=offset).next_back();
        let start = before.map_or(0, |(&paste, _)| paste + OBJECT.len());
        if offset < start {
            return start - OBJECT.len()..start;
        }
        let end = self.pastes.range(offset..).next();
        let segment = &self.text[start..end.map_or(self.text.len(), |(&paste, _)| paste)];
        let unit = grapheme(segment, offset - start);
        start + unit.start..start + unit.end
    }
    fn is_object(&self, unit: &Range<usize>) -> bool {
        self.pastes.contains_key(&unit.start)
    }
    fn vertical(&mut self, down: bool) -> usize {
        self.rows_away(1, down)
    }
    fn forget_column(&mut self) {
        self.preferred_column = None;
    }
    fn place(&mut self, cursor: usize, anchor: Option<usize>) {
        self.cursor = self.boundary(cursor);
        self.anchor = anchor;
    }
    fn erase(&mut self, range: Range<usize>) -> bool {
        let changed = !range.is_empty();
        if changed {
            self.save();
        }
        self.anchor = None;
        self.delete_range(range);
        self.normalize();
        changed
    }
    fn insert(&mut self, text: &str) -> EditOutcome {
        if text.is_empty() {
            return EditOutcome::Handled;
        }
        self.normalize();
        let text_changed = self.selection().is_none_or(|range| {
            self.text[range.clone()] != *text || self.pastes.range(range).next().is_some()
        });
        if text_changed {
            self.save();
        }
        self.delete_selection();
        self.insert_raw(text);
        // Inserting a combining mark can merge with its neighbor. Never leave
        // the caret in the middle of the newly formed grapheme.
        let unit = self.unit(self.cursor);
        if unit.start < self.cursor {
            self.cursor = unit.end;
        }
        EditOutcome::handled(text_changed)
    }
    fn step_history(&mut self, redo: bool) -> bool {
        let Some(snapshot) = (if redo { &mut self.redo } else { &mut self.undo }).pop_back() else {
            return false;
        };
        let current = self.snapshot();
        push_history(if redo { &mut self.undo } else { &mut self.redo }, current);
        let changed = self.text != snapshot.text || self.pastes != snapshot.pastes;
        self.restore(snapshot);
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::editor::HISTORY_LIMIT;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers as M};

    fn key(editor: &mut Composer, code: KeyCode, modifiers: M) {
        assert_ne!(
            editor.handle(KeyEvent::new(code, modifiers)),
            EditOutcome::Ignored
        );
    }

    pub(super) fn plain(text: &str) -> Composer {
        let mut editor = Composer::default();
        editor.set(text.to_owned());
        editor
    }

    fn undo(editor: &mut Composer) {
        key(editor, KeyCode::Char('-'), M::CONTROL);
    }

    fn redo(editor: &mut Composer) {
        key(editor, KeyCode::Char('.'), M::CONTROL);
    }

    /// Press `code` and check the resulting caret.
    fn step(editor: &mut Composer, code: KeyCode, modifiers: M, cursor: usize) {
        key(editor, code, modifiers);
        assert_eq!(editor.cursor, cursor, "after {code:?}");
    }

    fn assert_document_invariants(editor: &Composer) {
        let tokens = editor.tokens();
        let len = editor.text().len();
        for position in [Some(editor.cursor()), editor.anchor()]
            .into_iter()
            .flatten()
        {
            assert_eq!(Token::boundary(&tokens, len, position), position);
        }
        for token in &tokens {
            let offsets = token.source.clone();
            for offset in offsets.filter(|&offset| editor.text.is_char_boundary(offset)) {
                assert_eq!(editor.unit(offset), token.source);
            }
        }
        for (&offset, paste) in &editor.pastes {
            assert_eq!(&editor.text()[offset..offset + OBJECT.len()], OBJECT);
            assert!(paste.id < editor.next_id);
            let mut tokens = tokens.iter();
            assert!(tokens.any(|token| token.source.start == offset && token.paste));
        }
    }

    #[test]
    fn ordered_verbatim_expansion_and_literal_object_character() {
        let mut editor = plain("before ");
        editor.insert_paste("a\n\n b\r\n".into());
        editor.insert(" middle ");
        editor.insert_paste("末尾\n".into());
        editor.insert(OBJECT);
        let expected = "before a\n\n b\r\n middle 末尾\n\u{fffc}";
        assert_eq!(editor.expanded_text(), expected);
        assert_eq!(
            editor.pastes().map(|(id, _)| id).collect::<Vec<_>>(),
            [1, 2]
        );
        assert_eq!(editor.take().text, expected);
        assert!(editor.is_empty() && !editor.has_pastes());
        undo(&mut editor);
        assert!(editor.is_empty(), "sent documents cannot reappear via undo");
    }

    #[test]
    fn inserting_before_paste_tracks_positions_and_stable_ids() {
        let mut editor = plain("abc");
        editor.insert_paste("first".into());
        editor.set_selection(editor.anchor(), 0);
        editor.insert_paste("second".into());
        editor.insert(" ");
        assert_eq!(editor.expanded_text(), "second abcfirst");
        assert_eq!(
            editor.pastes().collect::<Vec<_>>(),
            [(2, "second"), (1, "first")]
        );
        assert_eq!(editor.paste(1), Some("first"));
        assert_eq!(editor.remove_paste(2), EditOutcome::Changed);
        assert_eq!(
            (editor.expanded_text().as_str(), editor.cursor),
            (" abcfirst", 1)
        );
        undo(&mut editor);
        assert_eq!(editor.expanded_text(), "second abcfirst");
        assert_eq!(editor.paste(2), Some("second"));
        redo(&mut editor);
        assert_eq!(editor.expanded_text(), " abcfirst");
        assert_eq!(editor.remove_paste(99), EditOutcome::Ignored);
    }

    #[test]
    fn pasting_the_same_content_after_its_item_expands_it() {
        let mut editor = plain("a");
        editor.insert_paste("long".into());
        editor.insert_paste("other".into());
        assert_eq!(editor.pastes().count(), 2);
        editor.insert_paste("other".into());
        assert_eq!(editor.text, format!("a{OBJECT}other"));
        assert_eq!(editor.pastes().collect::<Vec<_>>(), [(1, "long")]);
        assert_eq!(editor.cursor, editor.text.len());
        undo(&mut editor);
        assert_eq!(editor.pastes().count(), 2);
    }

    #[test]
    fn home_and_end_move_by_line_and_with_ctrl_by_draft() {
        let mut editor = plain("ab\ncd\nef");
        editor.set_cursor(4);
        step(&mut editor, KeyCode::Home, M::NONE, 3);
        step(&mut editor, KeyCode::End, M::NONE, 5);
        step(&mut editor, KeyCode::Home, M::CONTROL, 0);
        step(&mut editor, KeyCode::End, M::CONTROL, 8);
    }

    #[test]
    fn paste_cursor_word_movement_and_deletion_are_atomic() {
        let mut editor = plain("L");
        editor.insert_paste("not individually editable".into());
        editor.insert("R");
        step(&mut editor, KeyCode::Left, M::NONE, 1 + OBJECT.len());
        step(&mut editor, KeyCode::Left, M::NONE, 1);
        step(&mut editor, KeyCode::Right, M::NONE, 1 + OBJECT.len());
        key(&mut editor, KeyCode::Backspace, M::NONE);
        assert_eq!(editor.text, "LR");
        assert!(!editor.has_pastes());
        undo(&mut editor);
        assert_eq!(editor.expanded_text(), "Lnot individually editableR");
        key(&mut editor, KeyCode::Left, M::NONE);
        key(&mut editor, KeyCode::Delete, M::NONE);
        assert_eq!(editor.text, "LR");
        undo(&mut editor);
        assert_eq!(editor.paste(1), Some("not individually editable"));

        let mut editor = plain("first");
        editor.insert_paste("paste".into());
        editor.insert("second");
        step(
            &mut editor,
            KeyCode::Left,
            M::CONTROL,
            "first".len() + OBJECT.len(),
        );
        step(&mut editor, KeyCode::Left, M::CONTROL, "first".len());
        step(&mut editor, KeyCode::Left, M::CONTROL, 0);
        step(&mut editor, KeyCode::Right, M::CONTROL, "first".len());
        key(&mut editor, KeyCode::Delete, M::CONTROL);
        assert_eq!(editor.text, "firstsecond");
    }

    #[test]
    fn paste_selection_copies_expanded_and_replacement_undo_restores_selection() {
        let mut editor = plain("head");
        editor.insert_paste("\nx\ny\n".into());
        editor.insert("tail");
        editor.set_selection(editor.anchor(), 4);
        key(&mut editor, KeyCode::Right, M::SHIFT);
        assert_eq!(editor.selected_text().as_deref(), Some("\nx\ny\n"));
        editor.insert("new");
        assert_eq!(editor.expanded_text(), "headnewtail");
        undo(&mut editor);
        assert_eq!(editor.expanded_text(), "head\nx\ny\ntail");
        assert_eq!(editor.selected_text().as_deref(), Some("\nx\ny\n"));
        redo(&mut editor);
        assert_eq!(editor.expanded_text(), "headnewtail");

        // Backward selections span several pastes.
        let mut editor = plain("A");
        for (paste, text) in [("111", "B"), ("222", "C")] {
            editor.insert_paste(paste.into());
            editor.insert(text);
        }
        editor.set_selection(Some(editor.text.len()), editor.cursor());
        editor.set_selection(editor.anchor(), 1);
        assert_eq!(editor.selected_text().as_deref(), Some("111B222C"));
        key(&mut editor, KeyCode::Delete, M::NONE);
        assert_eq!(editor.text, "A");
        assert!(editor.pastes.is_empty());
        undo(&mut editor);
        assert_eq!(editor.pastes().count(), 2);
        assert_eq!(editor.expanded_text(), "A111B222C");
    }

    #[test]
    fn unicode_graphemes_and_crlf_are_atomic_even_adjacent_to_pastes() {
        let mut editor = plain("e\u{301}👩‍💻界\r\n");
        assert_document_invariants(&editor);
        key(&mut editor, KeyCode::Backspace, M::NONE);
        key(&mut editor, KeyCode::Backspace, M::NONE);
        assert_eq!(editor.text, "e\u{301}👩‍💻");
        key(&mut editor, KeyCode::Backspace, M::NONE);
        assert_eq!(editor.text, "e\u{301}");
        editor.insert_paste("paste".into());
        editor.insert("\u{301}");
        assert_document_invariants(&editor);
        step(
            &mut editor,
            KeyCode::Left,
            M::NONE,
            "e\u{301}".len() + OBJECT.len(),
        );
        key(&mut editor, KeyCode::Backspace, M::NONE);
        assert!(!editor.has_pastes());
        assert_eq!(editor.text, "e\u{301}\u{301}");
        assert_eq!(editor.cursor, 0, "joining graphemes normalizes the caret");

        let mut editor = plain("a\r\nb");
        editor.set_selection(editor.anchor(), 0);
        step(&mut editor, KeyCode::Char('e'), M::CONTROL, 1);
        key(&mut editor, KeyCode::Char('k'), M::CONTROL);
        assert_eq!(editor.text, "ab");
        undo(&mut editor);
        step(&mut editor, KeyCode::Right, M::NONE, 3);
        key(&mut editor, KeyCode::Backspace, M::NONE);
        assert_eq!(editor.text, "ab");
    }

    #[test]
    fn set_and_clear_discard_pastes_and_all_history() {
        let mut editor = Composer::default();
        editor.insert_paste("secret".into());
        editor.set("replacement".into());
        assert!(!editor.has_pastes());
        undo(&mut editor);
        assert_eq!(editor.text, "replacement");
        editor.insert("x");
        undo(&mut editor);
        editor.clear_sensitive();
        assert!(editor.text.is_empty() && editor.undo.is_empty() && editor.redo.is_empty());
        assert_eq!((editor.cursor, editor.anchor), (0, None));
    }

    #[test]
    fn vertical_navigation_preserves_display_column_through_short_soft_and_wide_rows() {
        let mut editor = plain("abcdef\nx\nabcdef");
        editor.set_width(20);
        editor.set_selection(editor.anchor(), 5);
        key(&mut editor, KeyCode::Down, M::NONE);
        assert_eq!(editor.layout(20).cursor, (1, 1));
        key(&mut editor, KeyCode::Down, M::SHIFT);
        assert_eq!(editor.layout(20).cursor, (2, 5));
        assert_eq!(editor.selected_text().as_deref(), Some("\nabcde"));
        key(&mut editor, KeyCode::Up, M::NONE);
        step(&mut editor, KeyCode::Up, M::NONE, 5);

        let mut editor = plain("界界 abc def ghi");
        editor.set_width(8);
        editor.set_selection(editor.anchor(), "界".len());
        key(&mut editor, KeyCode::Down, M::NONE);
        assert_eq!(editor.layout(8).cursor, (1, 2));
        step(&mut editor, KeyCode::Up, M::NONE, "界".len());
    }

    #[test]
    fn local_clear_is_undoable_with_paste_ids_positions_and_selection() {
        let mut editor = plain("before ");
        editor.insert_paste("private\ncontents".into());
        editor.insert(" after");
        editor.set_selection(Some(7), editor.cursor());
        editor.set_selection(editor.anchor(), 7 + OBJECT.len());
        let expected = editor.expanded_text();
        editor.clear();
        assert!(editor.is_empty() && !editor.has_pastes());
        undo(&mut editor);
        assert_eq!(editor.expanded_text(), expected);
        assert_eq!(editor.paste(1), Some("private\ncontents"));
        assert_eq!((editor.cursor, editor.anchor), (7 + OBJECT.len(), Some(7)));
        assert_eq!(editor.selected_text().as_deref(), Some("private\ncontents"));
        redo(&mut editor);
        assert!(editor.is_empty());
        editor.insert_paste("next".into());
        assert_eq!(editor.paste(2), Some("next"));
    }

    #[test]
    fn outcomes_distinguish_noops_navigation_and_actual_rich_edits() {
        let mut editor = Composer::default();
        let outcome = editor.handle(KeyEvent::new(KeyCode::Esc, M::NONE));
        assert_eq!(outcome, EditOutcome::Ignored);
        let outcome = editor.handle(KeyEvent::new(KeyCode::Backspace, M::NONE));
        assert_eq!(outcome, EditOutcome::Handled);
        assert!(!editor.insert("").text_changed());
        assert!(editor.insert("same").text_changed());
        editor.set_selection(Some(0), editor.text().len());
        assert!(!editor.insert("same").text_changed());
        assert!(!editor.set("same".into()).text_changed());
        assert!(editor.insert_paste("paste".into()).text_changed());
        editor.set_selection(Some(4), editor.text().len());
        // Identical raw object character, different rich document.
        assert!(editor.insert(OBJECT).text_changed());
        assert!(!editor.has_pastes());
        let undo = KeyEvent::new(KeyCode::Char('-'), M::CONTROL);
        assert!(editor.handle(undo).text_changed());
        assert_eq!(editor.paste(1), Some("paste"));
        assert_document_invariants(&editor);
    }

    #[test]
    fn rich_history_evicts_oldest_at_local_bound_and_restores_atomically() {
        let mut editor = Composer::default();
        editor.insert_paste("kept attachment".into());
        for _ in 0..HISTORY_LIMIT + 7 {
            editor.insert("x");
            assert!(editor.undo.len() <= HISTORY_LIMIT);
        }
        for _ in 0..HISTORY_LIMIT {
            undo(&mut editor);
            assert_document_invariants(&editor);
        }
        assert_eq!(editor.expanded_text(), "kept attachmentxxxxxxx");
        assert_eq!(editor.redo.len(), HISTORY_LIMIT);
        for _ in 0..HISTORY_LIMIT {
            redo(&mut editor);
            assert_document_invariants(&editor);
        }
        assert_eq!(editor.undo.len(), HISTORY_LIMIT);
        undo(&mut editor);
        editor.insert_paste("new attachment".into());
        assert!(editor.redo.is_empty());
        assert_eq!(editor.paste(1), Some("kept attachment"));
        assert_eq!(editor.paste(2), Some("new attachment"));
        assert_document_invariants(&editor);
    }

    #[test]
    fn attachments_are_taken_with_the_draft() {
        let mut editor = Composer::default();
        editor.insert("look");
        let file = Attachment::Text {
            file: Some("a.rs".into()),
            content: "fn a() {}".into(),
        };
        editor.attach(file.clone());
        let submission = editor.take();
        assert!(editor.is_empty());
        let attachments = vec![file];
        let text = "look".into();
        assert_eq!(submission, Submission { text, attachments });
    }
}
