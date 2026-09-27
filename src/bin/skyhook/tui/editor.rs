use super::composer::ComposerLayout;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers as M};
use unicode_segmentation::GraphemeCursor;

use std::{collections::VecDeque, ops::Range};
use zeroize::Zeroize;

pub(super) const HISTORY_LIMIT: usize = 100;

/// Push onto a bounded undo/redo stack, forgetting the oldest entry.
pub(super) fn push_history<T>(history: &mut VecDeque<T>, entry: T) {
    if history.len() >= HISTORY_LIMIT {
        history.pop_front();
    }
    history.push_back(entry);
}

/// The grapheme of `text` holding `offset`; empty at the end of the text.
pub(super) fn grapheme(text: &str, offset: usize) -> Range<usize> {
    // The text is one whole chunk, so the cursor never asks for context.
    let mut cursor = GraphemeCursor::new(offset, text.len(), true);
    if cursor.is_boundary(text, 0) == Ok(false) {
        let _ = cursor.prev_boundary(text, 0);
    }
    let from = cursor.cur_cursor();
    let to = cursor.next_boundary(text, 0).ok().flatten();
    from..to.unwrap_or(text.len())
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum EditOutcome {
    #[default]
    Ignored,
    /// Handled without changing text: navigation or a boundary no-op.
    Handled,
    Changed,
}
impl EditOutcome {
    pub const fn handled(text_changed: bool) -> Self {
        if text_changed {
            Self::Changed
        } else {
            Self::Handled
        }
    }
    pub fn text_changed(self) -> bool {
        self == Self::Changed
    }
}

/// Where a movement or deletion reaches from the cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Motion {
    Previous,
    Next,
    WordBack,
    WordForward,
    LineStart,
    LineEnd,
    Start,
    End,
    /// The visual row above or below, keeping the column.
    Up,
    Down,
}

/// One editing action, read from a key the same way for every text field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edit {
    Move {
        motion: Motion,
        extend: bool,
    },
    /// The selection, or else the text up to where `motion` reaches.
    Delete(Motion),
    /// The text to the line boundary, whatever is selected. Forward at the end
    /// of a line joins the next one.
    DeleteLine {
        forward: bool,
    },
    Insert(char),
    Undo,
    Redo,
}
impl Edit {
    pub fn parse(key: KeyEvent) -> Option<Self> {
        let ctrl = key.modifiers.contains(M::CONTROL);
        let alt = key.modifiers.contains(M::ALT);
        let word = ctrl || alt;
        let motion = match key.code {
            KeyCode::Left if word => Motion::WordBack,
            KeyCode::Left => Motion::Previous,
            KeyCode::Right if word => Motion::WordForward,
            KeyCode::Right => Motion::Next,
            KeyCode::Home => Motion::Start,
            KeyCode::End => Motion::End,
            KeyCode::Up => Motion::Up,
            KeyCode::Down => Motion::Down,
            KeyCode::Char('a') if ctrl => Motion::LineStart,
            KeyCode::Char('e') if ctrl => Motion::LineEnd,
            KeyCode::Char('b') if alt => Motion::WordBack,
            KeyCode::Char('b') if ctrl => Motion::Previous,
            KeyCode::Char('f') if alt => Motion::WordForward,
            KeyCode::Char('f') if ctrl => Motion::Next,
            KeyCode::Char('-') if ctrl => return Some(Self::Undo),
            KeyCode::Char('.') if ctrl => return Some(Self::Redo),
            KeyCode::Char('u') if ctrl => return Some(Self::DeleteLine { forward: false }),
            KeyCode::Char('k') if ctrl => return Some(Self::DeleteLine { forward: true }),
            KeyCode::Backspace if word => return Some(Self::Delete(Motion::WordBack)),
            KeyCode::Char('w') if ctrl => return Some(Self::Delete(Motion::WordBack)),
            KeyCode::Backspace => return Some(Self::Delete(Motion::Previous)),
            KeyCode::Delete | KeyCode::Char('d') if word => {
                return Some(Self::Delete(Motion::WordForward));
            }
            KeyCode::Delete => return Some(Self::Delete(Motion::Next)),
            KeyCode::Char(c) if !word => return Some(Self::Insert(c)),
            _ => return None,
        };
        let extend = key.modifiers.contains(M::SHIFT);
        Some(Self::Move { motion, extend })
    }
}

/// A text field that carries out every [`Edit`] through its own primitives.
pub trait TextField {
    fn text(&self) -> &str;
    fn cursor(&self) -> usize;
    fn anchor(&self) -> Option<usize>;
    /// The atomic unit holding `offset`; empty at the end of the text.
    fn unit(&self, offset: usize) -> Range<usize>;
    /// An indivisible object, which is a word by itself.
    fn is_object(&self, _unit: &Range<usize>) -> bool {
        false
    }
    /// Where Up or Down lands, keeping a column across shorter visual rows.
    /// A field drawn on a single row stays put.
    fn vertical(&mut self, _down: bool) -> usize {
        self.cursor()
    }
    /// Forget the column vertical movement keeps.
    fn forget_column(&mut self) {}
    /// Move the cursor to a unit boundary, selecting from `anchor`.
    fn place(&mut self, cursor: usize, anchor: Option<usize>);
    /// Delete `range` as one undoable edit; only an actual deletion enters history.
    fn erase(&mut self, range: Range<usize>) -> bool;
    fn insert(&mut self, text: &str) -> EditOutcome;
    /// Undo (or redo) one edit, moving the current state onto the opposite stack.
    fn step_history(&mut self, redo: bool) -> bool;

    fn selection(&self) -> Option<Range<usize>> {
        let cursor = self.cursor();
        self.anchor().map(|a| a.min(cursor)..a.max(cursor))
    }

    fn handle(&mut self, key: KeyEvent) -> EditOutcome {
        Edit::parse(key).map_or(EditOutcome::Ignored, |edit| self.apply(edit))
    }

    fn apply(&mut self, edit: Edit) -> EditOutcome {
        let cursor = self.cursor();
        let text_changed = match edit {
            Edit::Move { motion, extend } => {
                let target = self.target(motion);
                let anchor = extend.then(|| self.anchor().unwrap_or(cursor));
                self.place(target, anchor);
                if !matches!(motion, Motion::Up | Motion::Down) {
                    self.forget_column();
                }
                return EditOutcome::Handled;
            }
            Edit::Delete(motion) => {
                let range = self.selection().filter(|range| !range.is_empty());
                let range = range.unwrap_or_else(|| {
                    let target = self.target(motion);
                    target.min(cursor)..target.max(cursor)
                });
                self.erase(range)
            }
            Edit::DeleteLine { forward: false } => {
                let start = self.target(Motion::LineStart);
                self.erase(start..cursor)
            }
            Edit::DeleteLine { forward: true } => {
                let end = match self.target(Motion::LineEnd) {
                    end if end == cursor => self.target(Motion::Next),
                    end => end,
                };
                self.erase(cursor..end)
            }
            Edit::Insert(c) => {
                let mut bytes = [0; 4];
                let changed = self.insert(c.encode_utf8(&mut bytes)).text_changed();
                bytes.zeroize();
                changed
            }
            Edit::Undo => self.step_history(false),
            Edit::Redo => self.step_history(true),
        };
        EditOutcome::handled(text_changed)
    }

    fn target(&mut self, motion: Motion) -> usize {
        let (text, cursor) = (self.text(), self.cursor());
        match motion {
            Motion::Previous => self.step(cursor, false).map_or(0, |unit| unit.start),
            Motion::Next => self.step(cursor, true).map_or(cursor, |unit| unit.end),
            Motion::WordBack => self.word(false),
            Motion::WordForward => self.word(true),
            Motion::LineStart => text[..cursor].rfind('\n').map_or(0, |i| i + 1),
            Motion::LineEnd => {
                let end = text[cursor..].find('\n').map_or(text.len(), |i| cursor + i);
                self.unit(end).start
            }
            Motion::Start => 0,
            Motion::End => text.len(),
            Motion::Up | Motion::Down => self.vertical(motion == Motion::Down),
        }
    }

    /// The unit beside `offset`, before or after it.
    fn step(&self, offset: usize, forward: bool) -> Option<Range<usize>> {
        if forward {
            (offset < self.text().len()).then(|| self.unit(offset))
        } else {
            let before = self.text()[..offset].chars().next_back()?;
            Some(self.unit(offset - before.len_utf8()))
        }
    }

    fn word(&self, forward: bool) -> usize {
        let mut end = self.cursor();
        let mut seen_word = false;
        while let Some(unit) = self.step(end, forward) {
            let edge = if forward { unit.end } else { unit.start };
            if self.is_object(&unit) {
                if !seen_word {
                    end = edge;
                }
                break;
            }
            let whitespace = self.text()[unit].chars().all(char::is_whitespace);
            if seen_word && whitespace {
                break;
            }
            seen_word |= !whitespace;
            end = edge;
        }
        end
    }
}

/// Owns every allocation containing editor text, including history snapshots.
/// There is no `DerefMut`: mutation never lets String retire an unwiped
/// allocation. This covers owned buffers, not caller, allocator, or terminal copies.
#[derive(Clone, Default)]
struct SensitiveText(String);

impl std::ops::Deref for SensitiveText {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl SensitiveText {
    fn replace_range(&mut self, range: Range<usize>, replacement: &str) -> bool {
        if &self.0[range.clone()] == replacement {
            return false;
        }
        let new_len = self.0.len() - range.len() + replacement.len();
        if new_len > self.0.capacity() {
            // Allocate before copying any secret bytes; the old owner wipes on replacement.
            let mut next = Self(String::with_capacity(new_len));
            next.0.push_str(&self.0[..range.start]);
            next.0.push_str(replacement);
            next.0.push_str(&self.0[range.end..]);
            *self = next;
        } else {
            self.0.replace_range(range, replacement);
        }
        true
    }
}

impl Drop for SensitiveText {
    fn drop(&mut self) {
        // Fill spare capacity without growth, so the whole allocation is wiped
        // (including bytes retired by an in-place deletion) and tests can inspect it.
        while self.0.len() < self.0.capacity() {
            self.0.push('\0');
        }
        self.0.as_mut_str().zeroize();
        #[cfg(test)]
        if !self.0.is_empty() {
            assert!(self.0.bytes().all(|byte| byte == 0));
            WIPES.with_borrow_mut(|wipes| wipes.push(self.0.len()));
        }
    }
}

#[cfg(test)]
thread_local! {
    static WIPES: std::cell::RefCell<Vec<usize>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[derive(Clone, Default)]
pub struct Editor {
    text: SensitiveText,
    cursor: usize,
    anchor: Option<usize>,
    undo: VecDeque<(SensitiveText, usize)>,
    redo: VecDeque<(SensitiveText, usize)>,
}
impl Editor {
    /// Reject invalid UTF-8 coordinates before changing either position.
    pub fn set_selection(&mut self, anchor: Option<usize>, cursor: usize) -> bool {
        if [Some(cursor), anchor]
            .into_iter()
            .flatten()
            .any(|offset| !self.text.is_char_boundary(offset))
        {
            return false;
        }
        (self.cursor, self.anchor) = (cursor, anchor);
        true
    }

    /// Use the composer's visual rows without changing plain-text editing.
    /// Secret fields must lay out a masked editor rather than their raw text.
    pub fn layout(&self, width: usize) -> ComposerLayout {
        ComposerLayout::plain_text(&self.text, self.cursor, self.anchor, width)
    }

    pub fn clear_sensitive(&mut self) {
        self.text = SensitiveText::default();
        self.undo.clear();
        self.redo.clear();
        self.cursor = 0;
        self.anchor = None;
    }
    pub fn set(&mut self, text: String) {
        self.text = SensitiveText(text);
        self.cursor = self.text.len();
        self.anchor = None;
    }
    fn save(&mut self) {
        push_history(&mut self.undo, (self.text.clone(), self.cursor));
        self.redo.clear();
    }
    /// Transfer a secret without making an undo copy, and wipe its editing history.
    pub fn take_sensitive(&mut self) -> skyhook::remote::SecretValue {
        let secret = skyhook::remote::SecretValue::new(std::mem::take(&mut self.text.0));
        self.clear_sensitive();
        secret
    }
}
impl TextField for Editor {
    fn text(&self) -> &str {
        &self.text
    }
    fn cursor(&self) -> usize {
        self.cursor
    }
    fn anchor(&self) -> Option<usize> {
        self.anchor
    }
    fn unit(&self, offset: usize) -> Range<usize> {
        grapheme(&self.text, offset)
    }
    fn place(&mut self, cursor: usize, anchor: Option<usize>) {
        (self.cursor, self.anchor) = (cursor, anchor);
    }
    fn erase(&mut self, range: Range<usize>) -> bool {
        let changed = !range.is_empty();
        if changed {
            self.save();
        }
        self.anchor = None;
        self.text.replace_range(range.clone(), "");
        self.cursor = range.start;
        changed
    }
    fn insert(&mut self, text: &str) -> EditOutcome {
        self.save();
        let anchor = self.anchor.take().unwrap_or(self.cursor);
        let (a, b) = (anchor.min(self.cursor), anchor.max(self.cursor));
        let text_changed = self.text.replace_range(a..b, text);
        self.cursor = a + text.len();
        EditOutcome::handled(text_changed)
    }
    fn step_history(&mut self, redo: bool) -> bool {
        let (from, to) = if redo {
            (&mut self.redo, &mut self.undo)
        } else {
            (&mut self.undo, &mut self.redo)
        };
        let Some((text, cursor)) = from.pop_back() else {
            return false;
        };
        let changed = *self.text != *text;
        push_history(to, (std::mem::replace(&mut self.text, text), self.cursor));
        self.cursor = cursor;
        self.anchor = None;
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(editor: &mut Editor, code: KeyCode, modifiers: M) -> EditOutcome {
        editor.handle(KeyEvent::new(code, modifiers))
    }

    fn undo(editor: &mut Editor) {
        press(editor, KeyCode::Char('-'), M::CONTROL);
    }

    #[test]
    fn line_deletion_clears_selection_before_insert_copy_and_undo() {
        for shortcut in ['u', 'k'] {
            for text in ["abcdef", "a👩‍💻é界xy"] {
                let mut editor = Editor::default();
                editor.insert(text);
                for _ in 0..3 {
                    press(&mut editor, KeyCode::Left, M::SHIFT);
                }
                press(&mut editor, KeyCode::Char(shortcut), M::CONTROL);
                assert_eq!(editor.anchor, None);
                editor.insert("x");
                undo(&mut editor);
                undo(&mut editor);
                assert_eq!((editor.text(), editor.anchor), (text, None));
            }
        }
    }

    #[test]
    fn deletion_and_undo_preserve_graphemes() {
        let mut e = Editor::default();
        e.insert("a👩‍💻é");
        assert_eq!(e.layout(3).cursor, (1, 1));
        assert_eq!(e.layout(3).cursor_position("a👩‍💻".len()), (1, 0));
        press(&mut e, KeyCode::Backspace, M::NONE);
        assert_eq!((e.layout(3).cursor, e.text()), ((1, 0), "a👩‍💻"));
        press(&mut e, KeyCode::Backspace, M::NONE);
        assert_eq!(e.text(), "a");
        undo(&mut e);
        assert_eq!(e.text(), "a👩‍💻");
    }

    #[test]
    fn sensitive_text_wipes_every_retired_allocation_and_transfers_without_copying() {
        let take_wipes = || WIPES.with_borrow_mut(std::mem::take);
        let mut editor = Editor::default();
        editor.set("old".into());
        take_wipes();
        editor.set("secret".into());
        assert_eq!(take_wipes(), [3], "replaced text is wiped");
        editor.save();
        assert!(!editor.undo.is_empty());
        let allocation = editor.text.as_ptr();
        let secret = editor.take_sensitive();
        assert_eq!(secret.expose(), "secret");
        assert_eq!(secret.expose().as_ptr(), allocation);
        assert_eq!(take_wipes(), [6], "only history is wiped by transfer");
        assert!(editor.text.is_empty() && editor.undo.is_empty() && editor.redo.is_empty());
        assert_eq!((editor.cursor, editor.anchor), (0, None));

        editor.set("secret that will be deleted".into());
        let capacity = editor.text.0.capacity();
        editor.insert(&"q".repeat(capacity + 1));
        assert_eq!(take_wipes(), [capacity], "growth wipes the old allocation");
        let grown = editor.text.0.capacity();
        editor.clear_sensitive();
        assert!(take_wipes().contains(&grown), "clear wipes full capacity");
    }

    #[test]
    fn editor_effects_distinguish_changes_navigation_boundary_noops_and_invalid_selections() {
        let mut editor = Editor::default();
        assert_eq!(
            press(&mut editor, KeyCode::Backspace, M::NONE),
            EditOutcome::Handled
        );
        assert_eq!(
            press(&mut editor, KeyCode::Esc, M::NONE),
            EditOutcome::Ignored
        );
        assert!(editor.insert("aé👩‍💻").text_changed());
        press(&mut editor, KeyCode::Left, M::SHIFT);
        let same = editor.insert("👩‍💻");
        assert_eq!(
            same,
            EditOutcome::Handled,
            "identical selection replacement is not a text change"
        );
        assert!(press(&mut editor, KeyCode::Backspace, M::NONE).text_changed());
        assert_eq!(editor.text(), "aé");
        // Invalid selections are rejected atomically.
        for (anchor, cursor) in [(Some(0), 2), (Some(4), 0)] {
            assert!(!editor.set_selection(anchor, cursor));
            assert_eq!((editor.cursor(), editor.anchor()), (3, None));
        }
        assert!(editor.set_selection(Some(1), 3));
        assert!(editor.insert("界").text_changed());
        assert_eq!(editor.text(), "a界");
    }
}
