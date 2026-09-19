//! Visual rows, source coordinates, and text selection.

use super::*;

#[derive(Clone)]
pub struct Row {
    pub(super) line: std::sync::Arc<Line<'static>>,
    pub(super) x: u16,
    pub(super) width: u16,
    pub(super) surface: Surface,
    pub entry: usize,
    pub(super) entry_key: std::sync::Arc<model::EntryKey>,
    pub selectable: bool,
    pub(super) layout: markdown::RowLayout,
    pub(super) inset: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TextPosition {
    pub row: usize,
    pub byte: usize,
}

/// Inline reasoning and activity spinners are not navigable or copyable entries.
pub fn entry_selectable(entry: &model::Entry) -> bool {
    entry.expandable()
        || !(entry.surface == Surface::Reasoning
            || (entry.surface == Surface::Muted && entry.running))
}

impl Row {
    pub fn text(&self) -> String {
        self.line.to_string()
    }
    pub(super) fn paragraph_x(&self) -> u16 {
        self.x + self.inset + u16::from(matches!(self.surface, Surface::User | Surface::Agent)) * 2
    }
    pub(super) fn text_view(&self) -> RowText<'_> {
        RowText {
            row: self,
            text: self.text(),
        }
    }
    pub(super) fn participates_in_source(&self) -> bool {
        self.selectable && self.layout.participates_in_source()
    }
    #[cfg(test)]
    pub(super) fn text_x(&self) -> u16 {
        self.paragraph_x()
            .saturating_add(self.text_view().source_column(0).unwrap() as u16)
    }
    pub fn byte_at_column(&self, column: u16) -> usize {
        let target = column.saturating_sub(self.paragraph_x()) as usize;
        let text = self.text();
        let mut column = self.layout.prefix.width();
        let (prefix_bytes, prefix_width) = self.layout.source_prefix();
        let prefix_end = column + prefix_width;
        for (byte, grapheme) in text.grapheme_indices(true) {
            if byte == prefix_bytes {
                column = self
                    .layout
                    .code()
                    .map_or(prefix_end, |code| code.body_start());
            }
            let clipped_prefix = byte < prefix_bytes && column + grapheme.width() > prefix_end;
            if !clipped_prefix && target < column + grapheme.width() {
                return byte;
            }
            column += grapheme.width();
        }
        text.len()
    }
}

/// Text and source coordinates are derived from this row, never an unrelated
/// caller string. Oversized endpoints clamp to EOF (the select-all sentinel);
/// interior non-UTF-8 boundaries are rejected rather than sliced or rounded.
pub(super) struct RowText<'a> {
    row: &'a Row,
    text: String,
}

impl RowText<'_> {
    pub fn text(&self) -> &str {
        &self.text
    }
    fn endpoint(&self, byte: usize) -> Option<usize> {
        let byte = byte.min(self.text.len());
        self.text.is_char_boundary(byte).then_some(byte)
    }
    pub fn source_column(&self, byte: usize) -> Option<usize> {
        let byte = self.endpoint(byte)?;
        let layout = &self.row.layout;
        let (prefix_bytes, prefix_width) = layout.source_prefix();
        let prefix = self.endpoint(prefix_bytes)?;
        let leading = layout.prefix.width();
        Some(if byte < prefix {
            leading
                + self.text[..byte]
                    .graphemes(true)
                    .map(|g| g.width())
                    .sum::<usize>()
                    .min(prefix_width)
        } else {
            layout
                .code()
                .map_or(leading + prefix_width, |code| code.body_start())
                + self.text[prefix..byte]
                    .graphemes(true)
                    .map(|g| g.width())
                    .sum::<usize>()
        })
    }
    pub fn selection_range(
        &self,
        row: usize,
        selection: Option<(TextPosition, TextPosition)>,
    ) -> Option<std::ops::Range<usize>> {
        let (a, b) = selection?;
        let (start, end) = (a.min(b), a.max(b));
        if !self.row.participates_in_source() || start == end || row < start.row || row > end.row {
            return None;
        }
        let start_byte = self.endpoint(if row == start.row { start.byte } else { 0 })?;
        let end_byte = self.endpoint(if row == end.row {
            end.byte
        } else {
            self.text.len()
        })?;
        (start_byte <= end_byte).then_some(start_byte..end_byte)
    }
}

pub fn selected_text(rows: &RowBlocks, selection: (TextPosition, TextPosition)) -> String {
    // Reject nonexistent/wrong-row and malformed endpoints before extracting
    // any part; partial copied selections would conceal stale coordinates.
    for endpoint in [selection.0, selection.1] {
        let Some(row) = rows.get(endpoint.row) else {
            return String::new();
        };
        if row.text_view().endpoint(endpoint.byte).is_none() {
            return String::new();
        }
    }
    let mut text = String::new();
    let mut first = true;
    let start = selection.0.row.min(selection.1.row);
    let end = selection.0.row.max(selection.1.row);
    for index in start..=end {
        let Some(row) = rows.get(index) else { break };
        let view = row.text_view();
        if let Some(range) = view.selection_range(index, Some(selection)) {
            if !first && !row.layout.continued() {
                text.push('\n');
            }
            text.push_str(&view.text()[range]);
            first = false;
        }
    }
    text
}

/// Compare only selected rows, sharing the same fast path as retained rendering.
pub(super) fn selection_unchanged<'a>(
    previous: &[Row],
    current: impl IntoIterator<Item = &'a Row>,
    selection: (TextPosition, TextPosition),
) -> bool {
    let (start, end) = (selection.0.min(selection.1), selection.0.max(selection.1));
    let mut current = current.into_iter();
    let Some(selected_rows) = end
        .row
        .checked_sub(start.row)
        .and_then(|n| n.checked_add(1))
    else {
        return false;
    };
    previous.len() == selected_rows
        && previous.iter().enumerate().all(|(offset, before)| {
            let Some(after) = current.next() else {
                return false;
            };
            if before.entry != after.entry
                || before.entry_key != after.entry_key
                || before.selectable != after.selectable
                || before.layout != after.layout
            {
                return false;
            }
            for endpoint in [start, end] {
                if endpoint.row == start.row + offset
                    && (before.text_view().endpoint(endpoint.byte).is_none()
                        || after.text_view().endpoint(endpoint.byte).is_none())
                {
                    return false;
                }
            }
            if std::sync::Arc::ptr_eq(&before.line, &after.line) {
                return true;
            }
            let before = before.text();
            let after = after.text();
            if start.row + offset == end.row {
                before
                    .get(..end.byte)
                    .is_some_and(|prefix| after.get(..end.byte) == Some(prefix))
            } else {
                before == after
            }
        })
}

#[cfg(test)]
mod tests {
    use super::super::super::tool_view::HighlightCache;
    use super::super::tests::expandable_entry;
    use super::*;

    fn layout(entry: &model::Entry, width: u16) -> Vec<Row> {
        let highlights = HighlightCache::default();
        let mut rows = Vec::new();
        let options = EntryLayout {
            width,
            palette: Palette::new(),
            highlights: &highlights,
            request_columns: RequestColumns::default(),
            expanded: entry.default_open,
        };
        update_entry_rows(&mut rows, entry, 0, options);
        rows
    }

    fn source_row(text: &str) -> Row {
        let entry = model::Entry::new(model::EntryKey::Record(777), text.to_owned(), Surface::Tool);
        EntryGeometry::new(&entry, 30, 0).row(Line::from(text.to_owned()), false, false)
    }

    #[test]
    fn expansion_matches_toggle_defaults_and_explicit_overrides() {
        let mut entry = expandable_entry();
        let mut view = model::View::default();
        assert!(!entry.is_expanded(&view, false));
        assert!(entry.is_expanded(&view, true));
        view.tab = Tab::Jobs;
        assert!(!entry.is_expanded(&view, true));
        let job = model::EntryKey::Job(skyhook::identity::JobId::new(1).unwrap());
        entry = model::Entry::expandable_text(job, entry.text().to_owned(), Surface::Tool);
        assert!(entry.is_expanded(&view, true));
        entry = expandable_entry();
        entry.surface = Surface::Reasoning;
        entry.default_open = true;
        assert!(entry.is_expanded(&view, false));
        view.set_expanded(entry.key().clone(), false);
        assert!(!entry.is_expanded(&view, true));
        // A single override cannot represent simultaneous collapsed/expanded
        // membership. Explicitly reopening replaces the collapsed override.
        view.set_expanded(entry.key().clone(), true);
        entry.default_open = false;
        assert!(entry.is_expanded(&view, false));
        entry = model::Entry::new(entry.key().clone(), entry.text().to_owned(), entry.surface);
        assert!(!entry.is_expanded(&view, true));
    }

    #[test]
    fn selection_preserves_soft_wrapped_text_and_code_whitespace() {
        let source = "  first line with enough text to wrap\n    second line  ";
        let text = format!("Agent\n```text\n{source}\n```");
        let entry = model::Entry::new(model::EntryKey::Record(2), text, Surface::Agent);
        let rows = layout(&entry, 30);
        let start = rows
            .iter()
            .position(|row| row.text().starts_with("  first"))
            .unwrap();
        let end = rows
            .iter()
            .rposition(|row| row.text().ends_with("second line  "))
            .unwrap();
        let byte = rows[end].text().len();
        let selection = (
            TextPosition {
                row: start,
                byte: 2,
            },
            TextPosition { row: end, byte },
        );
        let mut blocks = RowBlocks::default();
        blocks.replace_entry(0, rows.clone(), Vec::new());
        let expected = source.strip_prefix("  ").unwrap();
        assert_eq!(selected_text(&blocks, selection), expected);
        assert_eq!(selected_text(&blocks, (selection.1, selection.0)), expected);
        for (text, unchanged) in [
            (format!("{}\nLater streaming text", entry.text()), true),
            (entry.text().replace("first", "other"), false),
        ] {
            let updated = layout(
                &model::Entry::new(entry.key().clone(), text, entry.surface),
                30,
            );
            let same =
                selection_unchanged(&rows[start..=end], updated.iter().skip(start), selection);
            assert_eq!(same, unchanged);
        }
    }

    #[test]
    fn source_participation_changes_invalidate_identical_shared_text() {
        let row = source_row("same");
        let selection = (
            TextPosition { row: 0, byte: 0 },
            TextPosition { row: 0, byte: 4 },
        );
        let unchanged =
            |old: &Row, new: &Row| selection_unchanged(std::slice::from_ref(old), [new], selection);
        let mut spacer = row.clone();
        spacer.layout = markdown::RowLayout::spacer();
        assert!(!unchanged(&row, &spacer) && !unchanged(&spacer, &row));
        assert!(
            spacer
                .text_view()
                .selection_range(0, Some(selection))
                .is_none()
        );
        // Reflow and identity changes invalidate the selection as well.
        let mut reflowed = row.clone();
        reflowed.layout =
            markdown::RowLayout::source(Line::from("  "), 0, 0, None).with_flow(false, true);
        let mut rekeyed = row.clone();
        rekeyed.entry_key = std::sync::Arc::new(model::EntryKey::Record(778));
        for changed in [reflowed, rekeyed, source_row("else")] {
            assert!(!unchanged(&row, &changed));
        }
    }

    #[test]
    fn checked_row_text_rejects_interior_utf8_and_wrong_row_endpoints() {
        let row = source_row("é界👩‍💻é");
        let mut rows = RowBlocks::default();
        rows.replace_entry(0, vec![row.clone()], Vec::new());
        let start = TextPosition { row: 0, byte: 0 };
        for byte in [1, 3, 6] {
            let end = TextPosition { row: 0, byte };
            assert!(row.text_view().source_column(byte).is_none());
            assert!(
                row.text_view()
                    .selection_range(0, Some((start, end)))
                    .is_none()
            );
            assert!(selected_text(&rows, (start, end)).is_empty());
        }
        let end = TextPosition {
            row: 0,
            byte: usize::MAX,
        };
        assert_eq!(selected_text(&rows, (end, start)), row.text());
        assert!(selected_text(&rows, (start, TextPosition { row: 1, byte: 0 })).is_empty());
    }
}
