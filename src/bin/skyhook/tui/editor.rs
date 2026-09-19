use super::composer::ComposerLayout;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers as M};
use unicode_segmentation::UnicodeSegmentation;

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

/// An edit can be handled without changing text (navigation or a boundary no-op).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EditOutcome {
    pub handled: bool,
    pub text_changed: bool,
}
impl EditOutcome {
    /// Handled without changing text: navigation or a boundary no-op.
    pub const HANDLED: Self = Self::handled(false);
    pub const CHANGED: Self = Self::handled(true);
    pub const fn handled(text_changed: bool) -> Self {
        Self {
            handled: true,
            text_changed,
        }
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
    pub fn text(&self) -> &str {
        &self.text
    }
    pub fn cursor(&self) -> usize {
        self.cursor
    }
    pub fn anchor(&self) -> Option<usize> {
        self.anchor
    }

    pub fn set_cursor(&mut self, cursor: usize) -> bool {
        if !self.text.is_char_boundary(cursor) {
            return false;
        }
        self.cursor = cursor;
        true
    }

    /// Reject invalid UTF-8 coordinates before changing either position.
    pub fn set_selection(&mut self, anchor: Option<usize>, cursor: usize) -> bool {
        if anchor.is_some_and(|anchor| !self.text.is_char_boundary(anchor))
            || !self.set_cursor(cursor)
        {
            return false;
        }
        self.anchor = anchor;
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
    /// Undo (or redo) one edit, moving the current text onto the opposite stack.
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
    fn selection(&mut self) -> bool {
        if let Some(anchor) = self.anchor.take() {
            let (a, b) = (anchor.min(self.cursor), anchor.max(self.cursor));
            let changed = self.text.replace_range(a..b, "");
            self.cursor = a;
            changed
        } else {
            false
        }
    }
    pub fn insert(&mut self, text: &str) -> EditOutcome {
        self.save();
        let anchor = self.anchor.take().unwrap_or(self.cursor);
        let (a, b) = (anchor.min(self.cursor), anchor.max(self.cursor));
        let text_changed = self.text.replace_range(a..b, text);
        self.cursor = a + text.len();
        EditOutcome::handled(text_changed)
    }
    /// Transfer a secret without making an undo copy, and wipe its editing history.
    pub fn take_sensitive(&mut self) -> skyhook::remote::SecretValue {
        let secret = skyhook::remote::SecretValue::new(std::mem::take(&mut self.text.0));
        self.clear_sensitive();
        secret
    }
    fn previous(&self) -> usize {
        self.text[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(i, _)| i)
    }
    fn next(&self) -> usize {
        self.text[self.cursor..]
            .graphemes(true)
            .next()
            .map_or(self.text.len(), |s| self.cursor + s.len())
    }
    fn word(&self, forward: bool) -> usize {
        if forward {
            let suffix = &self.text[self.cursor..];
            let n = suffix
                .char_indices()
                .skip_while(|(_, c)| c.is_whitespace())
                .find(|(_, c)| c.is_whitespace())
                .map_or(suffix.len(), |(i, _)| i);
            self.cursor + n
        } else {
            let prefix = self.text[..self.cursor].trim_end();
            prefix
                .char_indices()
                .rev()
                .find(|(_, c)| c.is_whitespace())
                .map_or(0, |(i, c)| i + c.len_utf8())
        }
    }
    pub fn handle(&mut self, key: KeyEvent) -> EditOutcome {
        let ctrl = key.modifiers.contains(M::CONTROL);
        let alt = key.modifiers.contains(M::ALT);
        let shift = key.modifiers.contains(M::SHIFT);
        let start = self.text[..self.cursor].rfind('\n').map_or(0, |n| n + 1);
        let end = self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |n| self.cursor + n);
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
            KeyCode::Up if start > 0 => {
                let previous = self.text[..start - 1].rfind('\n').map_or(0, |n| n + 1);
                let column = self.text[start..self.cursor].graphemes(true).count();
                Some(
                    previous
                        + self.text[previous..start - 1]
                            .graphemes(true)
                            .take(column)
                            .map(str::len)
                            .sum::<usize>(),
                )
            }
            KeyCode::Down if end < self.text.len() => {
                let next = end + 1;
                let tail = &self.text[next..];
                let line = tail.split('\n').next().unwrap_or_default();
                let column = self.text[start..self.cursor].graphemes(true).count();
                Some(
                    next + line
                        .graphemes(true)
                        .take(column)
                        .map(str::len)
                        .sum::<usize>(),
                )
            }
            _ => None,
        };
        if let Some(cursor) = movement {
            if shift {
                self.anchor.get_or_insert(self.cursor);
            } else {
                self.anchor = None;
            }
            self.cursor = cursor;
            return EditOutcome::HANDLED;
        }
        let text_changed = match key.code {
            KeyCode::Char('-') if ctrl => self.step_history(false),
            KeyCode::Char('.') if ctrl => self.step_history(true),
            KeyCode::Backspace | KeyCode::Char('w') if key.code == KeyCode::Backspace || ctrl => {
                self.save();
                if self.anchor.is_some() {
                    self.selection()
                } else {
                    let from = if ctrl || alt {
                        self.word(false)
                    } else {
                        self.previous()
                    };
                    let changed = self.text.replace_range(from..self.cursor, "");
                    self.cursor = from;
                    changed
                }
            }
            KeyCode::Delete | KeyCode::Char('d') if key.code == KeyCode::Delete || ctrl || alt => {
                self.save();
                if self.anchor.is_some() {
                    self.selection()
                } else {
                    let to = if ctrl || alt {
                        self.word(true)
                    } else {
                        self.next()
                    };
                    self.text.replace_range(self.cursor..to, "")
                }
            }
            KeyCode::Char('u') if ctrl => {
                self.save();
                self.anchor = None;
                let changed = self.text.replace_range(start..self.cursor, "");
                self.cursor = start;
                changed
            }
            KeyCode::Char('k') if ctrl => {
                self.save();
                self.anchor = None;
                self.text.replace_range(self.cursor..end, "")
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                let mut bytes = [0; 4];
                let changed = self.insert(c.encode_utf8(&mut bytes)).text_changed;
                bytes.zeroize();
                changed
            }
            _ => return EditOutcome::default(),
        };
        EditOutcome::handled(text_changed)
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
            EditOutcome::HANDLED
        );
        assert_eq!(
            press(&mut editor, KeyCode::Esc, M::NONE),
            EditOutcome::default()
        );
        assert!(editor.insert("aé👩‍💻").text_changed);
        press(&mut editor, KeyCode::Left, M::SHIFT);
        let same = editor.insert("👩‍💻");
        assert_eq!(
            same,
            EditOutcome::HANDLED,
            "identical selection replacement is not a text change"
        );
        assert!(press(&mut editor, KeyCode::Backspace, M::NONE).text_changed);
        assert_eq!(editor.text(), "aé");
        // Invalid selections are rejected atomically.
        for (anchor, cursor) in [(Some(0), 2), (Some(4), 0)] {
            assert!(!editor.set_selection(anchor, cursor));
            assert_eq!((editor.cursor(), editor.anchor()), (3, None));
        }
        assert!(editor.set_selection(Some(1), 3));
        assert!(editor.insert("界").text_changed);
        assert_eq!(editor.text(), "a界");
    }
}
