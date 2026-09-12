//! Visual composer rows, source-to-cell cursor mapping, and atomic paste clipping.
use super::{Composer, Token};
use ratatui::{
    style::Style,
    text::{Line, Span},
};
use std::collections::BTreeMap;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

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
    pub(super) fn closest(&self, row: usize, column: usize) -> usize {
        self.positions
            .iter()
            .filter(|(_, (r, _))| *r == row)
            .min_by_key(|(offset, (_, col))| (col.abs_diff(column), **offset))
            .map_or(0, |(offset, _)| *offset)
    }
}

impl Composer {
    /// Word wrapping preserves all source whitespace, splits overlong words only
    /// at grapheme boundaries, and clips a paste label rather than splitting it.
    /// A full last row gets an empty caret row; callers must not wrap these rows
    /// again. Tabs occupy four cells and other terminal controls are made visible.
    pub fn layout(&self, width: usize) -> ComposerLayout {
        ComposerLayout::from_tokens(
            self.tokens(),
            self.text.len(),
            self.cursor,
            self.anchor,
            width,
        )
    }
}

impl ComposerLayout {
    /// Lay out plain text with the composer's whitespace-preserving wrapping and
    /// source-byte cursor/selection mapping. Offsets inside a grapheme snap to
    /// its start. Pass only masked text and masked offsets for secret fields.
    /// Render these rows without further wrapping, reserving a padding cell for
    /// the cursor at the exclusive edge of an exactly full explicit line.
    pub fn plain_text(text: &str, cursor: usize, anchor: Option<usize>, width: usize) -> Self {
        Self::from_tokens(
            Token::plain(text, 0).collect(),
            text.len(),
            cursor,
            anchor,
            width,
        )
    }

    fn from_tokens(
        tokens: Vec<Token>,
        text_len: usize,
        cursor: usize,
        anchor: Option<usize>,
        width: usize,
    ) -> Self {
        let width = width.max(1);
        let cursor = Token::boundary(&tokens, text_len, cursor);
        let selection = anchor.map(|a| {
            let a = Token::boundary(&tokens, text_len, a);
            a.min(cursor)..a.max(cursor)
        });
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
            positions.insert(text_len, (rows.len() - 1, 0));
        }
        let mut layout = ComposerLayout {
            rows,
            cursor: (0, 0),
            positions,
        };
        layout.cursor = layout.cursor_position(cursor);
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
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers as M};

    fn rows(editor: &Composer, width: usize) -> Vec<String> {
        let layout = editor.layout(width);
        if !editor.has_pastes() {
            let mut plain = crate::tui::editor::Editor::default();
            plain.set(editor.text.clone());
            plain.cursor = editor.cursor;
            plain.anchor = editor.anchor;
            let plain_layout = plain.layout(width);
            assert_eq!(plain_layout.cursor, layout.cursor);
            assert_eq!(plain_layout.positions, layout.positions);
            assert_eq!(
                plain_layout
                    .rows
                    .iter()
                    .map(|r| r.width)
                    .collect::<Vec<_>>(),
                layout.rows.iter().map(|r| r.width).collect::<Vec<_>>()
            );
            assert_eq!(
                plain_layout
                    .rows
                    .iter()
                    .map(|r| r.line(Style::default(), Style::default(), Style::default()))
                    .collect::<Vec<_>>(),
                layout
                    .rows
                    .iter()
                    .map(|r| r.line(Style::default(), Style::default(), Style::default()))
                    .collect::<Vec<_>>()
            );
        }
        layout
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
        assert_eq!(rows(&plain("界"), 1), ["�", ""]);
        assert_eq!(rows(&plain("a\r\nb"), 4), ["a", "b"]);
        // A literal object marker stays ordinary text in a plain editor.
        assert!(!ComposerLayout::plain_text("\u{fffc}", 3, None, 4).rows[0].spans[0].paste);
        let layout = ComposerLayout::plain_text("界e\u{301}\tq", 5, Some(3), 8);
        assert_eq!(layout.cursor, (0, 2)); // Inside the combining grapheme.
        assert_eq!(layout.cursor_position(2), (0, 0)); // Inside the wide UTF-8 glyph.
        assert_eq!(layout.cursor_position(6), (0, 3));
        assert_eq!(layout.cursor_position(7), (0, 7));
        assert_eq!(layout.cursor_position(8), (1, 0));
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
        let plain_layout = ComposerLayout::plain_text("a\t界\nx", 6, Some(1), 8);
        assert_eq!(plain_layout.cursor, (1, 0));
        assert_eq!(
            plain_layout.rows[0]
                .spans
                .iter()
                .map(|s| (s.text.as_str(), s.selected))
                .collect::<Vec<_>>(),
            [("a", false), ("    ", true), ("界", true), (" ", true)]
        );
        assert!(!plain_layout.rows[1].spans[0].selected);
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
            assert!(editor.handle(KeyEvent::new(KeyCode::Down, M::NONE)));
            assert!(editor.handle(KeyEvent::new(KeyCode::Up, M::NONE)));
            assert_eq!(
                editor.cursor, newline,
                "vertical movement preserves edge column"
            );
        }
    }
}
