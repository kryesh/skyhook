//! Rich editing is deliberately confined to the message composer. `text` contains
//! one object-replacement character per paste; use `expanded_text`/`take` to send
//! or copy it, and `set` rather than assigning to `text` to replace the document.
use std::{collections::BTreeMap, ops::Range};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers as M};
use ratatui::{
    style::Style,
    text::{Line, Span},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
use zeroize::Zeroize;

const OBJECT: &str = "\u{fffc}";
const HISTORY_LIMIT: usize = 100;

#[derive(Clone, Debug)]
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

#[derive(Clone, Default)]
struct Snapshot {
    text: String,
    pastes: BTreeMap<usize, Paste>,
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
    pub text: String,
    pub cursor: usize,
    pub anchor: Option<usize>,
    pastes: BTreeMap<usize, Paste>,
    next_id: usize,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
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
            next_id: 1,
            undo: Vec::new(),
            redo: Vec::new(),
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
#[derive(Clone, Debug)]
pub struct ComposerSpan {
    pub text: String,
    pub paste: bool,
    pub selected: bool,
}
#[derive(Clone, Debug, Default)]
pub struct ComposerRow {
    pub spans: Vec<ComposerSpan>,
    pub width: usize,
}
impl ComposerRow {
    pub fn line(&self, base: Style, paste: Style, selection: Style) -> Line<'static> {
        Line::from(
            self.spans
                .iter()
                .map(|span| {
                    let style = if span.paste { base.patch(paste) } else { base };
                    Span::styled(
                        span.text.clone(),
                        if span.selected {
                            style.patch(selection)
                        } else {
                            style
                        },
                    )
                })
                .collect::<Vec<_>>(),
        )
    }
}
#[derive(Clone, Debug)]
pub struct ComposerLayout {
    pub rows: Vec<ComposerRow>,
    /// (visual row, terminal-cell column). Before a newline terminating an
    /// exactly full row, column equals width; renderers must reserve one padding
    /// cell for that exclusive edge rather than clamp onto the final character.
    pub cursor: (usize, usize),
    positions: BTreeMap<usize, (usize, usize)>,
}
impl ComposerLayout {
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }
    pub fn cursor_position(&self, offset: usize) -> (usize, usize) {
        self.positions
            .range(..=offset)
            .next_back()
            .map_or((0, 0), |(_, pos)| *pos)
    }
    fn closest(&self, row: usize, column: usize) -> usize {
        self.positions
            .iter()
            .filter(|(_, (r, _))| *r == row)
            .min_by_key(|(offset, (_, col))| (col.abs_diff(column), **offset))
            .map_or(0, |(offset, _)| *offset)
    }
}

#[derive(Clone)]
struct Token {
    source: Range<usize>,
    text: String,
    paste: bool,
    whitespace: bool,
    newline: bool,
}
impl Composer {
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
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
    pub fn remove_paste(&mut self, id: usize) -> bool {
        let Some(offset) = self
            .pastes
            .iter()
            .find(|(_, p)| p.id == id)
            .map(|(&offset, _)| offset)
        else {
            return false;
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
        true
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
        layout.cursor.0 + 1 == layout.row_count()
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
        self.undo.clear();
        self.redo.clear();
        self.cursor = 0;
        self.anchor = None;
        self.preferred_column = None;
        self.next_id = 1;
    }
    /// Replace the whole document as plain text (history/menu recall).
    pub fn set(&mut self, text: String) {
        self.clear_sensitive();
        self.text = text;
        self.cursor = self.text.len();
    }
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            text: self.text.clone(),
            pastes: self.pastes.clone(),
            cursor: self.cursor,
            anchor: self.anchor,
        }
    }
    fn restore(&mut self, mut snapshot: Snapshot) {
        self.text.zeroize();
        self.text = std::mem::take(&mut snapshot.text);
        self.pastes = std::mem::take(&mut snapshot.pastes);
        self.cursor = snapshot.cursor;
        self.anchor = snapshot.anchor;
        self.preferred_column = None;
    }
    fn save(&mut self) {
        if self.undo.len() >= HISTORY_LIMIT {
            self.undo.remove(0);
        }
        self.undo.push(self.snapshot());
        self.redo.clear();
        self.preferred_column = None;
    }
    fn tokens(&self) -> Vec<Token> {
        let mut tokens = Vec::new();
        let mut offset = 0;
        while offset < self.text.len() {
            if let Some(paste) = self.pastes.get(&offset) {
                tokens.push(Token {
                    source: offset..offset + OBJECT.len(),
                    text: paste.label(),
                    paste: true,
                    whitespace: false,
                    newline: false,
                });
                offset += OBJECT.len();
            } else {
                let end = self
                    .pastes
                    .range(offset..)
                    .next()
                    .map_or(self.text.len(), |(&i, _)| i);
                for (i, g) in self.text[offset..end].grapheme_indices(true) {
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
                    tokens.push(Token {
                        source: offset + i..offset + i + g.len(),
                        text,
                        paste: false,
                        whitespace: g.chars().all(char::is_whitespace),
                        newline,
                    });
                }
                offset = end;
            }
        }
        tokens
    }
    fn boundary(&self, offset: usize) -> usize {
        let offset = offset.min(self.text.len());
        self.tokens()
            .iter()
            .find(|t| t.source.start < offset && offset < t.source.end)
            .map_or(offset, |t| t.source.start)
    }
    fn normalize(&mut self) {
        self.cursor = self.boundary(self.cursor);
        self.anchor = self.anchor.map(|a| self.boundary(a));
    }
    fn selection_range(&self) -> Option<Range<usize>> {
        self.anchor.map(|a| {
            let a = self.boundary(a);
            let c = self.boundary(self.cursor);
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
    pub fn insert(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.normalize();
        self.save();
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
    }
    pub fn insert_paste(&mut self, content: String) {
        if content.is_empty() {
            return;
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
    }
    /// Clear a local draft as one undoable edit (for Ctrl+C). Unlike sending or
    /// clearing sensitive data, this intentionally retains an undo snapshot.
    pub fn clear(&mut self) {
        self.normalize();
        if !self.is_empty() {
            self.save();
        }
        self.text.zeroize();
        self.pastes.clear();
        self.cursor = 0;
        self.anchor = None;
        self.preferred_column = None;
    }

    /// Expand inline items in document order. Sending starts a fresh undo history.
    pub fn take(&mut self) -> String {
        let text = self.expanded_text();
        self.clear_sensitive();
        text
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
            if token.paste {
                if !seen_word {
                    end = if forward {
                        token.source.end
                    } else {
                        token.source.start
                    };
                }
                break;
            }
            if seen_word && token.whitespace {
                break;
            }
            seen_word |= !token.whitespace;
            end = if forward {
                token.source.end
            } else {
                token.source.start
            };
        }
        end
    }

    pub fn handle(&mut self, key: KeyEvent) -> bool {
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
                    (row + 1).min(layout.row_count() - 1)
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
            return true;
        }
        match key.code {
            KeyCode::Char('-') if ctrl => {
                if let Some(snapshot) = self.undo.pop() {
                    self.redo.push(self.snapshot());
                    self.restore(snapshot);
                }
            }
            KeyCode::Char('.') if ctrl => {
                if let Some(snapshot) = self.redo.pop() {
                    self.undo.push(self.snapshot());
                    self.restore(snapshot);
                }
            }
            KeyCode::Backspace | KeyCode::Char('w') if key.code == KeyCode::Backspace || ctrl => {
                self.save();
                if !self.delete_selection() {
                    let from = if ctrl || alt {
                        self.word(false)
                    } else {
                        self.previous()
                    };
                    self.delete_range(from..self.cursor);
                }
            }
            KeyCode::Delete | KeyCode::Char('d') if key.code == KeyCode::Delete || ctrl || alt => {
                self.save();
                if !self.delete_selection() {
                    let to = if ctrl || alt {
                        self.word(true)
                    } else {
                        self.next()
                    };
                    self.delete_range(self.cursor..to);
                }
            }
            KeyCode::Char('u') if ctrl => {
                self.save();
                self.anchor = None;
                self.delete_range(start..self.cursor);
            }
            KeyCode::Char('k') if ctrl => {
                self.save();
                self.anchor = None;
                self.delete_range(
                    self.cursor..if self.cursor == end && end < self.text.len() {
                        self.next()
                    } else {
                        end
                    },
                );
            }
            KeyCode::Char(c) if !ctrl && !alt => self.insert(&c.to_string()),
            KeyCode::Enter if shift || alt => self.insert("\n"),
            _ => return false,
        }
        self.normalize();
        true
    }

    /// Word wrapping preserves all source whitespace, splits overlong words only
    /// at grapheme boundaries, and clips a paste label rather than splitting it.
    /// A full last row gets an empty caret row; callers must not wrap these rows
    /// again. Tabs occupy four cells and other terminal controls are made visible.
    pub fn layout(&self, width: usize) -> ComposerLayout {
        let width = width.max(1);
        let tokens = self.tokens();
        let selection = self.selection_range();
        let mut rows = vec![ComposerRow::default()];
        let mut positions = BTreeMap::new();
        positions.insert(0, (0, 0));
        let mut i = 0;
        while i < tokens.len() {
            let token = &tokens[i];
            if token.newline {
                // Preserve the insertion point after a full row instead of
                // clamping onto its final character or wide continuation cell.
                // Rendering reserves a padding cell for this exclusive edge.
                positions.insert(
                    token.source.start,
                    (rows.len() - 1, rows.last().unwrap().width),
                );
                let row = rows.last_mut().unwrap();
                if selection
                    .as_ref()
                    .is_some_and(|r| r.start <= token.source.start && token.source.start < r.end)
                    && row.width < width
                {
                    row.spans.push(ComposerSpan {
                        text: " ".into(),
                        paste: false,
                        selected: true,
                    });
                }
                rows.push(ComposerRow::default());
                positions.insert(token.source.end, (rows.len() - 1, 0));
                i += 1;
                continue;
            }
            // Wrap a whole word when it fits on a fresh row. Whitespace is not
            // discarded or bundled with the word, unlike Paragraph's trim mode.
            if !token.whitespace
                && !token.paste
                && (i == 0 || tokens[i - 1].whitespace || tokens[i - 1].paste)
            {
                let word_width: usize = tokens[i..]
                    .iter()
                    .take_while(|t| !t.whitespace && !t.paste)
                    .map(|t| UnicodeWidthStr::width(t.text.as_str()))
                    .sum();
                let column = rows.last().unwrap().width;
                if column > 0 && word_width <= width && column + word_width > width {
                    rows.push(ComposerRow::default());
                }
            }
            let raw_width = UnicodeWidthStr::width(token.text.as_str());
            let token_width = raw_width.min(width);
            if rows.last().unwrap().width >= width
                || (rows.last().unwrap().width > 0
                    && rows.last().unwrap().width + token_width > width)
            {
                rows.push(ComposerRow::default());
            }
            let row_index = rows.len() - 1;
            let row = rows.last_mut().unwrap();
            positions.insert(token.source.start, (row_index, row.width));
            let (display, displayed_width) = clip(&token.text, width);
            row.spans.push(ComposerSpan {
                text: display,
                paste: token.paste,
                selected: selection
                    .as_ref()
                    .is_some_and(|r| r.start < token.source.end && token.source.start < r.end),
            });
            row.width += displayed_width;
            positions.insert(token.source.end, (row_index, row.width));
            i += 1;
        }
        if rows.last().unwrap().width >= width {
            rows.push(ComposerRow::default());
            positions.insert(self.text.len(), (rows.len() - 1, 0));
        }
        let mut layout = ComposerLayout {
            rows,
            cursor: (0, 0),
            positions,
        };
        layout.cursor = layout.cursor_position(self.boundary(self.cursor));
        layout
    }
}

fn clip(text: &str, width: usize) -> (String, usize) {
    let mut out = String::new();
    let mut used = 0;
    for g in text.graphemes(true) {
        let n = UnicodeWidthStr::width(g);
        if used + n > width {
            // A wide grapheme cannot be displayed on a one-cell terminal. Keep
            // an atomic, visible replacement with the same source mapping.
            if out.is_empty() {
                out.push('�');
                used = 1;
            }
            break;
        }
        out.push_str(g);
        used += n;
    }
    (out, used)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(editor: &mut Composer, code: KeyCode, modifiers: M) {
        assert!(editor.handle(KeyEvent::new(code, modifiers)));
    }
    fn rows(editor: &Composer, width: usize) -> Vec<String> {
        editor
            .layout(width)
            .rows
            .iter()
            .map(|r| r.spans.iter().map(|s| s.text.as_str()).collect())
            .collect()
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

    #[test]
    fn ordered_verbatim_expansion_and_literal_object_character() {
        let mut editor = plain("before ");
        editor.insert_paste("a\n\n b\r\n".into());
        editor.insert(" middle ");
        editor.insert_paste("末尾\n".into());
        editor.insert(OBJECT);
        assert_eq!(
            editor.expanded_text(),
            "before a\n\n b\r\n middle 末尾\n\u{fffc}"
        );
        assert_eq!(
            editor.pastes().map(|(id, _)| id).collect::<Vec<_>>(),
            [1, 2]
        );
        let text = editor.take();
        assert_eq!(text, "before a\n\n b\r\n middle 末尾\n\u{fffc}");
        assert!(editor.is_empty());
        assert!(!editor.has_pastes());
        undo(&mut editor);
        assert!(editor.is_empty(), "sent documents cannot reappear via undo");
    }

    #[test]
    fn inserting_before_paste_tracks_positions_and_stable_ids() {
        let mut editor = plain("abc");
        editor.insert_paste("first".into());
        editor.cursor = 0;
        editor.insert_paste("second".into());
        editor.insert(" ");
        assert_eq!(editor.expanded_text(), "second abcfirst");
        assert_eq!(
            editor.pastes().collect::<Vec<_>>(),
            [(2, "second"), (1, "first")]
        );
        assert_eq!(editor.paste(1), Some("first"));
        assert!(editor.remove_paste(2));
        assert_eq!(editor.expanded_text(), " abcfirst");
        assert_eq!(editor.cursor, 1);
        undo(&mut editor);
        assert_eq!(editor.expanded_text(), "second abcfirst");
        assert_eq!(editor.paste(2), Some("second"));
        redo(&mut editor);
        assert_eq!(editor.expanded_text(), " abcfirst");
        assert!(!editor.remove_paste(99));
    }

    #[test]
    fn paste_cursor_and_deletion_are_atomic() {
        let mut editor = plain("L");
        editor.insert_paste("not individually editable".into());
        editor.insert("R");
        key(&mut editor, KeyCode::Left, M::NONE);
        assert_eq!(editor.cursor, 1 + OBJECT.len());
        key(&mut editor, KeyCode::Left, M::NONE);
        assert_eq!(editor.cursor, 1);
        key(&mut editor, KeyCode::Right, M::NONE);
        assert_eq!(editor.cursor, 1 + OBJECT.len());
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
    }

    #[test]
    fn paste_selection_copies_expanded_and_replacement_undo_restores_selection() {
        let mut editor = plain("head");
        editor.insert_paste("\nx\ny\n".into());
        editor.insert("tail");
        editor.cursor = 4;
        key(&mut editor, KeyCode::Right, M::SHIFT);
        assert_eq!(editor.selected_text().as_deref(), Some("\nx\ny\n"));
        editor.insert("new");
        assert_eq!(editor.expanded_text(), "headnewtail");
        undo(&mut editor);
        assert_eq!(editor.expanded_text(), "head\nx\ny\ntail");
        assert_eq!(editor.selected_text().as_deref(), Some("\nx\ny\n"));
        redo(&mut editor);
        assert_eq!(editor.expanded_text(), "headnewtail");
    }

    #[test]
    fn backward_selection_across_multiple_pastes() {
        let mut editor = plain("A");
        editor.insert_paste("111".into());
        editor.insert("B");
        editor.insert_paste("222".into());
        editor.insert("C");
        editor.anchor = Some(editor.text.len());
        editor.cursor = 1;
        assert_eq!(editor.selected_text().as_deref(), Some("111B222C"));
        key(&mut editor, KeyCode::Delete, M::NONE);
        assert_eq!(editor.text, "A");
        assert!(editor.pastes.is_empty());
        undo(&mut editor);
        assert_eq!(editor.pastes().count(), 2);
        assert_eq!(editor.expanded_text(), "A111B222C");
    }

    #[test]
    fn unicode_graphemes_are_atomic_even_adjacent_to_pastes() {
        let mut editor = plain("e\u{301}👩‍💻界");
        key(&mut editor, KeyCode::Backspace, M::NONE);
        assert_eq!(editor.text, "e\u{301}👩‍💻");
        key(&mut editor, KeyCode::Backspace, M::NONE);
        assert_eq!(editor.text, "e\u{301}");
        editor.insert_paste("paste".into());
        editor.insert("\u{301}");
        key(&mut editor, KeyCode::Left, M::NONE);
        assert_eq!(editor.cursor, "e\u{301}".len() + OBJECT.len());
        key(&mut editor, KeyCode::Backspace, M::NONE);
        assert!(!editor.has_pastes());
        assert_eq!(editor.text, "e\u{301}\u{301}");
        assert_eq!(editor.cursor, 0, "joining graphemes normalizes the caret");
    }

    #[test]
    fn word_movement_stops_at_atomic_pastes() {
        let mut editor = plain("first");
        editor.insert_paste("paste".into());
        editor.insert("second");
        key(&mut editor, KeyCode::Left, M::CONTROL);
        assert_eq!(editor.cursor, "first".len() + OBJECT.len());
        key(&mut editor, KeyCode::Left, M::CONTROL);
        assert_eq!(editor.cursor, "first".len());
        key(&mut editor, KeyCode::Left, M::CONTROL);
        assert_eq!(editor.cursor, 0);
        key(&mut editor, KeyCode::Right, M::CONTROL);
        assert_eq!(editor.cursor, "first".len());
        key(&mut editor, KeyCode::Delete, M::CONTROL);
        assert_eq!(editor.text, "firstsecond");
    }

    #[test]
    fn clone_preserves_rich_draft_independently() {
        let mut editor = Composer::default();
        editor.insert_paste("original".into());
        let mut draft = editor.clone();
        editor.take();
        assert_eq!(draft.paste(1), Some("original"));
        draft.insert(" suffix");
        assert_eq!(draft.take(), "original suffix");
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
        assert!(editor.text.is_empty());
        assert!(editor.undo.is_empty());
        assert!(editor.redo.is_empty());
        assert_eq!(editor.cursor, 0);
        assert!(editor.anchor.is_none());
    }

    #[test]
    fn word_wrapping_preserves_whitespace_and_explicit_empty_lines() {
        let editor = plain("one two   three\n\nlast\n");
        assert_eq!(rows(&editor, 8), ["one two ", "  three", "", "last", ""]);
        assert_eq!(editor.expanded_text(), "one two   three\n\nlast\n");
        assert_eq!(rows(&plain("hello world"), 8), ["hello ", "world"]);
        assert_eq!(rows(&plain("abcdefghijk"), 4), ["abcd", "efgh", "ijk"]);
    }

    #[test]
    fn cursor_uses_full_word_layout_not_prefix_wrapping() {
        let mut editor = plain("hello world");
        editor.cursor = "hello w".len();
        let layout = editor.layout(8);
        assert_eq!(layout.cursor, (1, 1));
        assert_eq!(layout.cursor_position(6), (1, 0));
    }

    #[test]
    fn full_width_end_gets_visible_caret_row() {
        let editor = plain("abcd");
        let layout = editor.layout(4);
        assert_eq!(rows(&editor, 4), ["abcd", ""]);
        assert_eq!(layout.cursor, (1, 0));
        assert_eq!(rows(&plain("abcd\n"), 4), ["abcd", ""]);
        assert_eq!(rows(&plain(""), 0), [""]);
    }

    #[test]
    fn unicode_width_and_tabs() {
        let editor = plain("界界 e\u{301} 👩‍💻");
        let layout = editor.layout(5);
        assert_eq!(rows(&editor, 5), ["界界 ", "e\u{301} 👩‍💻"]);
        assert_eq!(
            layout.rows.iter().map(|r| r.width).collect::<Vec<_>>(),
            [5, 4]
        );
        assert_eq!(layout.cursor, (1, 4));
        assert_eq!(rows(&plain("a\tb"), 6), ["a    b", ""]);
        assert_eq!(rows(&plain("\u{1b}[31m"), 80), ["�[31m"]);
    }

    #[test]
    fn paste_label_wraps_as_one_item_and_clips_on_narrow_terminal() {
        let mut editor = plain("prefix ");
        editor.insert_paste("one\ntwo\n".into());
        let label = editor.pastes.values().next().unwrap().label();
        assert_eq!(
            rows(&editor, label.len()),
            ["prefix ".to_string(), label.clone()]
        );
        let narrow = editor.layout(5);
        let paste_spans: Vec<_> = narrow
            .rows
            .iter()
            .flat_map(|r| &r.spans)
            .filter(|s| s.paste)
            .collect();
        assert_eq!(paste_spans.len(), 1);
        assert_eq!(paste_spans[0].text, "[Past");
        assert_eq!(narrow.cursor_position(7).1, 0);
        assert_eq!(narrow.cursor_position(10).1, 0);
        assert_eq!(editor.expanded_text(), "prefix one\ntwo\n");
    }

    #[test]
    fn selection_styling_tracks_wrapped_source_and_atomic_paste() {
        use ratatui::style::Color;
        let mut editor = plain("hello ");
        editor.insert_paste("contents".into());
        editor.insert(" end");
        editor.anchor = Some(6);
        editor.cursor = 10;
        let layout = editor.layout(10);
        let spans: Vec<_> = layout.rows.iter().flat_map(|r| &r.spans).collect();
        assert!(spans.iter().find(|s| s.paste).unwrap().selected);
        assert!(!spans[0].selected);
        for row in &layout.rows {
            let line = row.line(
                Style::default().fg(Color::White),
                Style::default().fg(Color::Blue),
                Style::default().bg(Color::Gray),
            );
            for (source, rendered) in row.spans.iter().zip(line.spans) {
                assert_eq!(rendered.style.bg, source.selected.then_some(Color::Gray));
                assert_eq!(
                    rendered.style.fg,
                    Some(if source.paste {
                        Color::Blue
                    } else {
                        Color::White
                    })
                );
            }
        }
    }

    #[test]
    fn vertical_navigation_preserves_display_column_through_short_rows() {
        let mut editor = plain("abcdef\nx\nabcdef");
        editor.set_width(20);
        editor.cursor = 5;
        assert!(editor.is_first_visual_row());
        key(&mut editor, KeyCode::Down, M::NONE);
        assert_eq!(editor.layout(20).cursor, (1, 1));
        key(&mut editor, KeyCode::Down, M::SHIFT);
        assert_eq!(editor.layout(20).cursor, (2, 5));
        assert!(editor.is_last_visual_row());
        assert_eq!(editor.selected_text().as_deref(), Some("\nabcde"));
        key(&mut editor, KeyCode::Up, M::NONE);
        key(&mut editor, KeyCode::Up, M::NONE);
        assert_eq!(editor.cursor, 5);
    }

    #[test]
    fn vertical_navigation_follows_soft_rows_and_wide_characters() {
        let mut editor = plain("界界 abc def ghi");
        editor.set_width(8);
        editor.cursor = "界".len();
        key(&mut editor, KeyCode::Down, M::NONE);
        assert_eq!(editor.layout(8).cursor, (1, 2));
        assert!(!editor.is_first_visual_row());
        key(&mut editor, KeyCode::Up, M::NONE);
        assert_eq!(editor.cursor, "界".len());
    }

    #[test]
    fn cursor_and_rows_stay_in_bounds_on_every_narrow_width() {
        for text in [
            "",
            "界界",
            "👩‍💻e\u{301}\n",
            "abcd\nef gh",
            "\t\n",
            "\u{301}a",
        ] {
            let mut editor = plain(text);
            editor.insert_paste("hidden\nbody\n".into());
            editor.insert("\u{301} end");
            let boundaries: Vec<_> = std::iter::once(0)
                .chain(editor.tokens().iter().map(|t| t.source.end))
                .collect();
            for width in 0..25 {
                for &cursor in &boundaries {
                    editor.cursor = cursor;
                    let layout = editor.layout(width);
                    assert!(layout.cursor.0 < layout.row_count());
                    assert!(
                        layout.cursor.1 <= width.max(1),
                        "{text:?} width={width} offset={cursor} cursor={:?}",
                        layout.cursor
                    );
                    assert!(layout.rows.iter().all(|r| r.width <= width.max(1)));
                }
            }
        }
    }

    #[test]
    fn local_clear_is_undoable_with_paste_ids_positions_and_selection() {
        let mut editor = plain("before ");
        editor.insert_paste("private\ncontents".into());
        editor.insert(" after");
        editor.anchor = Some(7);
        editor.cursor = 7 + OBJECT.len();
        let expected = editor.expanded_text();
        editor.clear();
        assert!(editor.is_empty());
        assert!(!editor.has_pastes());
        undo(&mut editor);
        assert_eq!(editor.expanded_text(), expected);
        assert_eq!(editor.paste(1), Some("private\ncontents"));
        assert_eq!(editor.cursor, 7 + OBJECT.len());
        assert_eq!(editor.anchor, Some(7));
        assert_eq!(editor.selected_text().as_deref(), Some("private\ncontents"));
        redo(&mut editor);
        assert!(editor.is_empty());
        editor.insert_paste("next".into());
        assert_eq!(editor.paste(2), Some("next"));
    }

    #[test]
    fn full_width_newline_caret_uses_exclusive_edge_not_last_character() {
        for (text, last_start, newline) in [("abcd\nx", 3, 4), ("ab界\nx", 2, 5)] {
            let mut editor = plain(text);
            editor.cursor = last_start;
            let before = editor.layout(4).cursor;
            editor.cursor = newline;
            let layout = editor.layout(4);
            assert_eq!(layout.cursor, (0, 4));
            assert_ne!(before, layout.cursor);
            assert_eq!(layout.cursor_position(newline + 1), (1, 0));
            assert_eq!(
                layout.row_count(),
                2,
                "full explicit lines are not double-spaced"
            );
            assert_eq!(rows(&editor, 4), [text.split('\n').next().unwrap(), "x"]);
            editor.set_width(4);
            key(&mut editor, KeyCode::Down, M::NONE);
            key(&mut editor, KeyCode::Up, M::NONE);
            assert_eq!(
                editor.cursor, newline,
                "vertical movement preserves edge column"
            );
        }
    }

    #[test]
    fn crlf_editing_is_grapheme_atomic() {
        let mut editor = plain("a\r\nb");
        editor.cursor = 0;
        key(&mut editor, KeyCode::Char('e'), M::CONTROL);
        assert_eq!(editor.cursor, 1);
        key(&mut editor, KeyCode::Char('k'), M::CONTROL);
        assert_eq!(editor.text, "ab");
        undo(&mut editor);
        key(&mut editor, KeyCode::Right, M::NONE);
        assert_eq!(editor.cursor, 3);
        key(&mut editor, KeyCode::Backspace, M::NONE);
        assert_eq!(editor.text, "ab");
    }
}
