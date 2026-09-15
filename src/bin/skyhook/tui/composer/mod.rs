//! Rich editing is deliberately confined to the message composer. `text` contains
//! one object-replacement character per paste; use `expanded_text` to copy it and
//! `take` to send it with its attachments. Use `set` rather than assigning to `text`.
use std::{
    collections::{BTreeMap, VecDeque},
    ops::Range,
};

use super::editor::{EditOutcome, push_history};
use skyhook::media::Attachment;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers as M};
use unicode_segmentation::UnicodeSegmentation;
use zeroize::Zeroize;

mod layout;
// Keep the composer facade stable even when callers infer these layout types.
#[allow(unused_imports)]
pub use layout::{ComposerLayout, ComposerRow, ComposerSpan};

const OBJECT: &str = "\u{fffc}";

#[derive(Clone, Debug, PartialEq, Eq)]
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
    pastes: BTreeMap<usize, Paste>,
    attachments: Vec<Attachment>,
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
    pastes: BTreeMap<usize, Paste>,
    attachments: Vec<Attachment>,
    next_id: usize,
    undo: VecDeque<Snapshot>,
    redo: VecDeque<Snapshot>,
    width: usize,
    preferred_column: Option<usize>,
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
    /// Raw document text; inline pastes occupy one object-replacement grapheme.
    pub fn text(&self) -> &str {
        &self.text
    }
    pub fn cursor(&self) -> usize {
        self.cursor
    }
    pub fn anchor(&self) -> Option<usize> {
        self.anchor
    }

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
    pub fn attachments(&self) -> &[Attachment] {
        &self.attachments
    }
    pub fn attach(&mut self, attachment: Attachment) -> EditOutcome {
        self.save();
        self.attachments.push(attachment);
        EditOutcome::CHANGED
    }
    pub fn remove_attachment(&mut self, index: usize) -> EditOutcome {
        if index >= self.attachments.len() {
            return EditOutcome::default();
        }
        self.save();
        self.attachments.remove(index);
        EditOutcome::CHANGED
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
            return EditOutcome::default();
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
        EditOutcome::CHANGED
    }
    pub fn set_width(&mut self, width: usize) {
        let width = width.max(1);
        if self.width != width {
            self.preferred_column = None;
        }
        self.width = width;
    }
    pub fn is_first_visual_row(&self) -> bool {
        self.layout(self.width).cursor.0 == 0
    }
    pub fn is_last_visual_row(&self) -> bool {
        let layout = self.layout(self.width);
        layout.cursor.0 + 1 == layout.rows.len()
    }
    pub fn expanded_text(&self) -> String {
        self.expand(0..self.text.len())
    }
    pub fn selected_text(&self) -> Option<String> {
        self.selection_range()
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
    }
    fn save(&mut self) {
        let snapshot = self.snapshot();
        push_history(&mut self.undo, snapshot);
        self.redo.clear();
        self.preferred_column = None;
    }
    /// Undo (or redo) one edit, moving the current state onto the opposite stack.
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
        Token::boundary(&self.tokens(), self.text.len(), offset)
    }
    fn normalize(&mut self) {
        self.cursor = self.boundary(self.cursor);
        self.anchor = self.anchor.map(|a| self.boundary(a));
    }
    pub fn selection_range(&self) -> Option<Range<usize>> {
        self.anchor().map(|a| {
            let a = self.boundary(a);
            let c = self.boundary(self.cursor());
            a.min(c)..a.max(c)
        })
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
        let range = self.selection_range();
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
    pub fn insert(&mut self, text: &str) -> EditOutcome {
        if text.is_empty() {
            return EditOutcome::HANDLED;
        }
        self.normalize();
        let text_changed = self.selection_range().is_none_or(|range| {
            self.text[range.clone()] != *text || self.pastes.range(range).next().is_some()
        });
        if text_changed {
            self.save();
        }
        self.delete_selection();
        self.insert_raw(text);
        // Inserting a combining mark can merge with its neighbor. Never leave
        // the caret in the middle of the newly formed grapheme.
        if let Some(token) = self
            .tokens()
            .iter()
            .find(|t| t.source.start < self.cursor && self.cursor < t.source.end)
        {
            self.cursor = token.source.end;
        }
        EditOutcome::handled(text_changed)
    }
    pub fn insert_paste(&mut self, content: String) -> EditOutcome {
        if content.is_empty() {
            return EditOutcome::HANDLED;
        }
        self.normalize();
        self.save();
        self.delete_selection();
        let offset = self.cursor;
        self.insert_raw(OBJECT);
        let id = self.next_id;
        self.next_id += 1;
        let lines = content.lines().count().max(1);
        self.pastes.insert(offset, Paste { id, content, lines });
        EditOutcome::CHANGED
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
        Submission { text, attachments }
    }
    /// Replace the draft with a submission taken back for editing.
    pub fn set_submission(&mut self, submission: Submission) {
        self.set(submission.text);
        self.attachments = submission.attachments;
    }
    fn previous(&self) -> usize {
        self.tokens()
            .iter()
            .rev()
            .find(|t| t.source.start < self.cursor)
            .map_or(0, |t| t.source.start)
    }
    fn next(&self) -> usize {
        self.tokens()
            .iter()
            .find(|t| t.source.end > self.cursor)
            .map_or(self.text.len(), |t| t.source.end)
    }
    fn word(&self, forward: bool) -> usize {
        let tokens = self.tokens();
        let mut end = self.cursor;
        let mut seen_word = false;
        let iter: Box<dyn Iterator<Item = &Token>> = if forward {
            Box::new(tokens.iter().filter(|t| t.source.start >= self.cursor))
        } else {
            Box::new(tokens.iter().rev().filter(|t| t.source.end <= self.cursor))
        };
        for token in iter {
            let edge = if forward {
                token.source.end
            } else {
                token.source.start
            };
            if token.paste {
                if !seen_word {
                    end = edge;
                }
                break;
            }
            if seen_word && token.whitespace {
                break;
            }
            seen_word |= !token.whitespace;
            end = edge;
        }
        end
    }

    pub fn handle(&mut self, key: KeyEvent) -> EditOutcome {
        self.normalize();
        let ctrl = key.modifiers.contains(M::CONTROL);
        let alt = key.modifiers.contains(M::ALT);
        let shift = key.modifiers.contains(M::SHIFT);
        let start = self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1);
        let end = self.boundary(
            self.text[self.cursor..]
                .find('\n')
                .map_or(self.text.len(), |i| self.cursor + i),
        );
        let vertical = matches!(key.code, KeyCode::Up | KeyCode::Down);
        let movement = match key.code {
            KeyCode::Left => Some(if ctrl || alt {
                self.word(false)
            } else {
                self.previous()
            }),
            KeyCode::Right => Some(if ctrl || alt {
                self.word(true)
            } else {
                self.next()
            }),
            KeyCode::Home => Some(0),
            KeyCode::End => Some(self.text.len()),
            KeyCode::Char('a') if ctrl => Some(start),
            KeyCode::Char('e') if ctrl => Some(end),
            KeyCode::Char('b') if ctrl || alt => Some(if alt {
                self.word(false)
            } else {
                self.previous()
            }),
            KeyCode::Char('f') if ctrl || alt => {
                Some(if alt { self.word(true) } else { self.next() })
            }
            KeyCode::Up | KeyCode::Down => {
                let layout = self.layout(self.width);
                let (row, column) = layout.cursor;
                let column = *self.preferred_column.get_or_insert(column);
                let target = if key.code == KeyCode::Up {
                    row.saturating_sub(1)
                } else {
                    (row + 1).min(layout.rows.len() - 1)
                };
                Some(if target == row {
                    self.cursor
                } else {
                    layout.closest(target, column)
                })
            }
            _ => None,
        };
        if let Some(cursor) = movement {
            if shift {
                self.anchor.get_or_insert(self.cursor);
            } else {
                self.anchor = None;
            }
            self.cursor = self.boundary(cursor);
            if !vertical {
                self.preferred_column = None;
            }
            return EditOutcome::HANDLED;
        }
        let text_changed = match key.code {
            KeyCode::Char('-') if ctrl => self.step_history(false),
            KeyCode::Char('.') if ctrl => self.step_history(true),
            KeyCode::Backspace | KeyCode::Char('w') if key.code == KeyCode::Backspace || ctrl => {
                let range = self
                    .selection_range()
                    .filter(|r| !r.is_empty())
                    .unwrap_or_else(|| {
                        let from = if ctrl || alt {
                            self.word(false)
                        } else {
                            self.previous()
                        };
                        from..self.cursor
                    });
                self.erase(range)
            }
            KeyCode::Delete | KeyCode::Char('d') if key.code == KeyCode::Delete || ctrl || alt => {
                let range = self
                    .selection_range()
                    .filter(|r| !r.is_empty())
                    .unwrap_or_else(|| {
                        let to = if ctrl || alt {
                            self.word(true)
                        } else {
                            self.next()
                        };
                        self.cursor..to
                    });
                self.erase(range)
            }
            KeyCode::Char('u') if ctrl => self.erase(start..self.cursor),
            KeyCode::Char('k') if ctrl => self.erase(
                self.cursor..if self.cursor == end && end < self.text.len() {
                    self.next()
                } else {
                    end
                },
            ),
            KeyCode::Char(c) if !ctrl && !alt => return self.insert(&c.to_string()),
            KeyCode::Enter if shift || alt => return self.insert("\n"),
            _ => return EditOutcome::default(),
        };
        self.normalize();
        EditOutcome::handled(text_changed)
    }

    /// Only actual deletions enter history or invalidate redo.
    fn erase(&mut self, range: Range<usize>) -> bool {
        let changed = !range.is_empty();
        if changed {
            self.save();
        }
        self.anchor = None;
        self.delete_range(range);
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::editor::HISTORY_LIMIT;

    fn key(editor: &mut Composer, code: KeyCode, modifiers: M) {
        assert!(editor.handle(KeyEvent::new(code, modifiers)).handled);
    }

    fn plain(text: &str) -> Composer {
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
        assert!(editor.remove_paste(2).handled);
        assert_eq!(
            (editor.expanded_text().as_str(), editor.cursor),
            (" abcfirst", 1)
        );
        undo(&mut editor);
        assert_eq!(editor.expanded_text(), "second abcfirst");
        assert_eq!(editor.paste(2), Some("second"));
        redo(&mut editor);
        assert_eq!(editor.expanded_text(), " abcfirst");
        assert!(!editor.remove_paste(99).handled);
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
        let mut editor = plain("e\u{301}👩‍💻界");
        key(&mut editor, KeyCode::Backspace, M::NONE);
        assert_eq!(editor.text, "e\u{301}👩‍💻");
        key(&mut editor, KeyCode::Backspace, M::NONE);
        assert_eq!(editor.text, "e\u{301}");
        editor.insert_paste("paste".into());
        editor.insert("\u{301}");
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
        assert!(editor.is_first_visual_row());
        key(&mut editor, KeyCode::Down, M::NONE);
        assert_eq!(editor.layout(20).cursor, (1, 1));
        key(&mut editor, KeyCode::Down, M::SHIFT);
        assert_eq!(editor.layout(20).cursor, (2, 5));
        assert!(editor.is_last_visual_row());
        assert_eq!(editor.selected_text().as_deref(), Some("\nabcde"));
        key(&mut editor, KeyCode::Up, M::NONE);
        step(&mut editor, KeyCode::Up, M::NONE, 5);

        let mut editor = plain("界界 abc def ghi");
        editor.set_width(8);
        editor.set_selection(editor.anchor(), "界".len());
        key(&mut editor, KeyCode::Down, M::NONE);
        assert_eq!(editor.layout(8).cursor, (1, 2));
        assert!(!editor.is_first_visual_row());
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
        assert!(!outcome.handled && !outcome.text_changed);
        let outcome = editor.handle(KeyEvent::new(KeyCode::Backspace, M::NONE));
        assert!(outcome.handled && !outcome.text_changed);
        assert!(!editor.insert("").text_changed);
        assert!(editor.insert("same").text_changed);
        editor.set_selection(Some(0), editor.text().len());
        assert!(!editor.insert("same").text_changed);
        assert!(!editor.set("same".into()).text_changed);
        assert!(editor.insert_paste("paste".into()).text_changed);
        editor.set_selection(Some(4), editor.text().len());
        // Identical raw object character, different rich document.
        assert!(editor.insert(OBJECT).text_changed);
        assert!(!editor.has_pastes());
        let undo = KeyEvent::new(KeyCode::Char('-'), M::CONTROL);
        assert!(editor.handle(undo).text_changed);
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
