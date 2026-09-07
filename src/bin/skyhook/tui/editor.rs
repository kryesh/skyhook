use crossterm::event::{KeyCode, KeyEvent, KeyModifiers as M};
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Default)]
pub struct Editor {
    pub text: String,
    pub cursor: usize,
    pub anchor: Option<usize>,
    undo: Vec<(String, usize)>,
    redo: Vec<(String, usize)>,
}
impl Drop for Editor {
    fn drop(&mut self) {
        self.clear_sensitive();
    }
}
impl Editor {
    pub fn selected_text(&self) -> Option<&str> {
        self.anchor
            .map(|anchor| &self.text[anchor.min(self.cursor)..anchor.max(self.cursor)])
    }
    pub fn clear_sensitive(&mut self) {
        use zeroize::Zeroize;
        self.text.zeroize();
        for (text, _) in self.undo.iter_mut().chain(self.redo.iter_mut()) {
            text.zeroize();
        }
        self.undo.clear();
        self.redo.clear();
        self.cursor = 0;
        self.anchor = None;
    }
    pub fn set(&mut self, text: String) {
        self.text = text;
        self.cursor = self.text.len();
        self.anchor = None;
    }
    fn save(&mut self) {
        if self.undo.len() >= 100 {
            self.undo.remove(0);
        }
        self.undo.push((self.text.clone(), self.cursor));
        self.redo.clear();
    }
    fn selection(&mut self) {
        if let Some(anchor) = self.anchor.take() {
            let (a, b) = (anchor.min(self.cursor), anchor.max(self.cursor));
            self.text.replace_range(a..b, "");
            self.cursor = a;
        }
    }
    pub fn insert(&mut self, text: &str) {
        self.save();
        self.selection();
        self.text.insert_str(self.cursor, text);
        self.cursor += text.len();
    }
    pub fn take(&mut self) -> String {
        self.save();
        self.cursor = 0;
        self.anchor = None;
        std::mem::take(&mut self.text)
    }
    /// Transfer a secret without making an undo copy, and wipe its editing history.
    pub fn take_sensitive(&mut self) -> skyhook::remote::SecretValue {
        let secret = skyhook::remote::SecretValue::new(std::mem::take(&mut self.text));
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
    pub fn handle(&mut self, key: KeyEvent) -> bool {
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
            return true;
        }
        match key.code {
            KeyCode::Char('-') if ctrl => {
                if let Some((text, cursor)) = self.undo.pop() {
                    self.redo.push((self.text.clone(), self.cursor));
                    self.text = text;
                    self.cursor = cursor;
                    self.anchor = None;
                }
            }
            KeyCode::Char('.') if ctrl => {
                if let Some((text, cursor)) = self.redo.pop() {
                    self.undo.push((self.text.clone(), self.cursor));
                    self.text = text;
                    self.cursor = cursor;
                    self.anchor = None;
                }
            }
            KeyCode::Backspace | KeyCode::Char('w') if key.code == KeyCode::Backspace || ctrl => {
                self.save();
                if self.anchor.is_some() {
                    self.selection();
                } else {
                    let from = if ctrl || alt {
                        self.word(false)
                    } else {
                        self.previous()
                    };
                    self.text.replace_range(from..self.cursor, "");
                    self.cursor = from;
                }
            }
            KeyCode::Delete | KeyCode::Char('d') if key.code == KeyCode::Delete || ctrl || alt => {
                self.save();
                if self.anchor.is_some() {
                    self.selection();
                } else {
                    let to = if ctrl || alt {
                        self.word(true)
                    } else {
                        self.next()
                    };
                    self.text.replace_range(self.cursor..to, "");
                }
            }
            KeyCode::Char('u') if ctrl => {
                self.save();
                self.anchor = None;
                self.text.replace_range(start..self.cursor, "");
                self.cursor = start;
            }
            KeyCode::Char('k') if ctrl => {
                self.save();
                self.anchor = None;
                self.text.replace_range(self.cursor..end, "");
            }
            KeyCode::Char(c) if !ctrl && !alt => self.insert(&c.to_string()),
            _ => return false,
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn line_deletion_clears_selection_before_insert_copy_and_undo() {
        for shortcut in ['u', 'k'] {
            for text in ["abcdef", "a👩‍💻é界xy"] {
                let mut editor = Editor::default();
                editor.insert(text);
                for _ in 0..3 {
                    editor.handle(KeyEvent::new(KeyCode::Left, M::SHIFT));
                }
                editor.handle(KeyEvent::new(KeyCode::Char(shortcut), M::CONTROL));
                assert_eq!(editor.selected_text(), None);
                editor.insert("x");
                editor.handle(KeyEvent::new(KeyCode::Char('-'), M::CONTROL));
                editor.handle(KeyEvent::new(KeyCode::Char('-'), M::CONTROL));
                assert_eq!(editor.text, text);
                assert_eq!(editor.selected_text(), None);
            }
        }
    }
    #[test]
    fn deletion_and_undo_preserve_graphemes() {
        let mut e = Editor::default();
        e.insert("a👩‍💻é");
        e.handle(KeyEvent::new(KeyCode::Backspace, M::NONE));
        assert_eq!(e.text, "a👩‍💻");
        e.handle(KeyEvent::new(KeyCode::Backspace, M::NONE));
        assert_eq!(e.text, "a");
        e.handle(KeyEvent::new(KeyCode::Char('-'), M::CONTROL));
        assert_eq!(e.text, "a👩‍💻");
    }
    #[test]
    fn taking_sensitive_text_moves_allocation_and_clears_history() {
        let mut editor = Editor::default();
        editor.insert("sec");
        editor.insert("ret");
        assert!(!editor.undo.is_empty());
        let allocation = editor.text.as_ptr();
        let secret = editor.take_sensitive();
        assert_eq!(secret.expose(), "secret");
        assert_eq!(secret.expose().as_ptr(), allocation);
        assert!(editor.text.is_empty());
        assert!(editor.undo.is_empty());
        assert!(editor.redo.is_empty());
        assert_eq!(editor.cursor, 0);
        assert!(editor.anchor.is_none());
    }
}
