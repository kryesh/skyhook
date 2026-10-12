//! Visual rows, source coordinates, and text selection.

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TextPosition {
    pub row: usize,
    pub byte: usize,
}

/// A place in an entry's selectable source: a byte within one of its logical
/// lines, as the layout identifies them. Unlike a [`TextPosition`], it is
/// independent of width and of the rows before the entry, so it survives
/// reflow, resizes, and entries above it changing height.
#[derive(Clone, Debug)]
struct SourcePoint {
    key: std::sync::Arc<model::EntryKey>,
    /// The entry's index when last resolved; `key` is authoritative.
    entry: usize,
    line: markdown::LineId,
    byte: usize,
}

/// A source point placed in the current layout. Bounds order as the
/// document reads: by entry, then line, then byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct SourceBound {
    entry: usize,
    /// The line's rank in its entry's [`LineOrder`].
    rank: usize,
    byte: usize,
}

/// Every source span of an entry's rows, as `(row offset, span)`, in order.
fn entry_sources(
    rows: &[Row],
) -> impl Iterator<Item = (usize, std::borrow::Cow<'_, markdown::SourceSpan>)> {
    (rows.iter().enumerate())
        .flat_map(|(offset, row)| row.sources().map(move |span| (offset, span)))
}

/// A logical line of an entry.
type LineKey = (usize, markdown::LineId);

/// An entry's logical lines in reading order: the order the layout first
/// shows them in. A wrapped table cell's later fragments interleave with
/// other cells, and a line's identity says nothing of its place, so this is
/// the only order. Each layout of an entry computes it once, when first
/// needed, so dragging across a long entry does not repeat it.
#[derive(Debug)]
pub(super) struct LineOrder {
    /// Each line, and the entry's rows from its first fragment through its last.
    lines: Vec<(markdown::LineId, std::ops::Range<usize>)>,
    ranks: std::collections::HashMap<markdown::LineId, usize>,
}

impl LineOrder {
    pub(super) fn of(rows: &[Row]) -> Self {
        let mut lines = Vec::new();
        let mut ranks = std::collections::HashMap::new();
        for (offset, span) in entry_sources(rows) {
            let rank = *ranks.entry(span.line).or_insert_with(|| {
                lines.push((span.line, offset..offset));
                lines.len() - 1
            });
            lines[rank].1.end = offset + 1;
        }
        Self { lines, ranks }
    }

    fn rank(&self, line: markdown::LineId) -> Option<usize> {
        self.ranks.get(&line).copied()
    }
}

impl SourcePoint {
    /// The source at a laid-out position. A position outside the source (an
    /// edge, a separator, a table border, past a row's end) snaps to the end
    /// of the line latest in reading order among those shown before it, else
    /// the start of the earliest shown after it.
    fn at(rows: &RowBlocks, at: TextPosition) -> Option<Self> {
        let row = rows.get(at.row)?;
        let offset = at.row - rows.entry_start(row.entry)?;
        let text = row.text();
        let byte = row.text_view().endpoint(at.byte)?;
        let point = |line, byte| Self {
            key: row.entry_key.clone(),
            entry: row.entry,
            line,
            byte,
        };
        let hit = (row.sources()).find(|span| span.shown.start <= byte && byte <= span.shown.end);
        if let Some(span) = hit {
            return Some(point(span.line, span.to_source(&text, byte)));
        }
        let order = rows.line_order(row.entry)?;
        let mut before: Option<(usize, markdown::LineId, usize)> = None;
        let mut after: Option<(usize, markdown::LineId, usize)> = None;
        for (span_row, span) in entry_sources(rows.entry_rows(row.entry)) {
            let rank = order.rank(span.line)?;
            if (span_row, span.shown.end) < (offset, byte) {
                if before.is_none_or(|(latest, ..)| rank >= latest) {
                    before = Some((rank, span.line, span.source.end));
                }
            } else if after.is_none_or(|(earliest, ..)| rank < earliest) {
                after = Some((rank, span.line, span.source.start));
            }
        }
        let (_, line, byte) = before.or(after)?;
        Some(point(line, byte))
    }

    /// This source in the current layout, and where it shows, if its entry
    /// still has it. A byte where a wrapped line breaks shows at the start
    /// of the following row.
    fn locate(&mut self, rows: &RowBlocks) -> Option<(SourceBound, TextPosition)> {
        let has_key = |entry: usize| {
            (rows.entry_rows(entry).first()).is_some_and(|row| row.entry_key == self.key)
        };
        if !has_key(self.entry) {
            self.entry = (0..rows.entry_count()).find(|&entry| has_key(entry))?;
        }
        let entry_rows = rows.entry_rows(self.entry);
        let order = rows.line_order(self.entry)?;
        let rank = order.rank(self.line)?;
        let mut found = None;
        'rows: for offset in order.lines[rank].1.clone() {
            for span in entry_rows[offset].sources() {
                let source = &span.source;
                if span.line == self.line && source.start <= self.byte && self.byte <= source.end {
                    let within = self.byte < source.end;
                    found = Some((offset, span));
                    if within {
                        break 'rows;
                    }
                }
            }
        }
        let (offset, span) = found?;
        let bound = SourceBound {
            entry: self.entry,
            rank,
            byte: self.byte,
        };
        let shown = TextPosition {
            row: rows.entry_start(self.entry)? + offset,
            byte: span.to_shown(&entry_rows[offset].text(), self.byte),
        };
        Some((bound, shown))
    }
}

/// The source bytes of `span`, of a line whose bytes `selected` are selected.
fn selected(
    lines: &std::collections::HashMap<LineKey, std::ops::Range<usize>>,
    entry: usize,
    span: &markdown::SourceSpan,
) -> Option<std::ops::Range<usize>> {
    let selected = lines.get(&(entry, span.line))?;
    let from = selected.start.max(span.source.start);
    let to = selected.end.min(span.source.end);
    (from <= to).then_some(from..to)
}

/// Transcript text selected with the pointer: the source between its anchor
/// and head, in the order the layout shows logical lines. Its endpoints are
/// source points, so it outlives any reflow that keeps the selected text:
/// streaming more text, a reply settling, or a resize. A reflow that changes
/// the text drops it. Copying and highlighting read the same source spans
/// as its endpoints.
#[derive(Clone, Debug)]
pub struct Selection {
    anchor: SourcePoint,
    head: SourcePoint,
    /// The selected source bytes of each logical line it takes in.
    lines: std::collections::HashMap<LineKey, std::ops::Range<usize>>,
    /// The first and last selected bytes as shown.
    rows: (TextPosition, TextPosition),
    text: String,
}

impl Selection {
    /// An empty selection at `at`, unless no source text is there.
    pub fn new(rows: &RowBlocks, at: TextPosition) -> Option<Self> {
        let point = SourcePoint::at(rows, at)?;
        Self::between(rows, point.clone(), point)
    }

    /// Extend the selection from its anchor to `at`. Positions without source
    /// text leave it as it was.
    pub fn extend(&mut self, rows: &RowBlocks, at: TextPosition) {
        if let Some(head) = SourcePoint::at(rows, at)
            && let Some(selection) = Self::between(rows, self.anchor.clone(), head)
        {
            *self = selection;
        }
    }

    fn between(rows: &RowBlocks, mut anchor: SourcePoint, mut head: SourcePoint) -> Option<Self> {
        let (anchor_bound, anchor_shown) = anchor.locate(rows)?;
        let (head_bound, _) = head.locate(rows)?;
        let (start, end) = (anchor_bound.min(head_bound), anchor_bound.max(head_bound));
        let mut lines = std::collections::HashMap::new();
        let mut texts = Vec::new();
        let mut shown: Option<(TextPosition, TextPosition)> = None;
        for entry in start.entry..=end.entry {
            let order = rows.line_order(entry)?;
            let first = if entry == start.entry { start.rank } else { 0 };
            let last = if entry == end.entry {
                end.rank + 1
            } else {
                order.lines.len()
            };
            // Lines rank by their first rows, so the first selected line's
            // starts the rows to read, and the latest last row ends them.
            let mut spanned: Option<std::ops::Range<usize>> = None;
            for (rank, (line, line_rows)) in (first..).zip(&order.lines[first..last]) {
                let from = if (entry, rank) == (start.entry, start.rank) {
                    start.byte
                } else {
                    0
                };
                let to = if (entry, rank) == (end.entry, end.rank) {
                    end.byte
                } else {
                    usize::MAX
                };
                lines.insert((entry, *line), from..to);
                spanned = Some(spanned.map_or(line_rows.clone(), |rows| {
                    rows.start..rows.end.max(line_rows.end)
                }));
            }
            let mut entry_texts = vec![String::new(); last - first];
            let entry_rows = rows.entry_rows(entry);
            let entry_start = rows.entry_start(entry).unwrap_or_default();
            for offset in spanned.unwrap_or_default() {
                let row = &entry_rows[offset];
                let mut text = None;
                for span in row.sources() {
                    let Some(bytes) = selected(&lines, entry, &span) else {
                        continue;
                    };
                    let text = text.get_or_insert_with(|| row.text());
                    let source = span.source.start;
                    let part = span
                        .source_text(text)
                        .get(bytes.start - source..bytes.end - source);
                    let rank = order.rank(span.line).expect("selected lines are ordered");
                    entry_texts[rank - first].push_str(part.unwrap_or_default());
                    if !bytes.is_empty() {
                        let at = |byte| TextPosition {
                            row: entry_start + offset,
                            byte: span.to_shown(text, byte),
                        };
                        let (from, to) = (at(bytes.start), at(bytes.end));
                        shown = Some(shown.map_or((from, to), |(start, _)| (start, to)));
                    }
                }
            }
            texts.extend(entry_texts);
        }
        Some(Self {
            rows: shown.unwrap_or((anchor_shown, anchor_shown)),
            text: texts.join("\n"),
            anchor,
            head,
            lines,
        })
    }

    /// The selection in the reflowed `rows`, if they still show its text there.
    pub(super) fn reflow(self, rows: &RowBlocks) -> Option<Self> {
        Self::between(rows, self.anchor, self.head).filter(|reflowed| reflowed.text == self.text)
    }

    /// Whether row `index` shows selected text: rows outside the selection
    /// need no highlighting, nor their text.
    pub(super) fn covers(&self, index: usize) -> bool {
        let (start, end) = self.rows;
        start != end && (start.row..=end.row).contains(&index)
    }

    /// The shown bytes of `row`, whose text is `text`, the selection covers.
    pub(super) fn highlights(&self, row: &Row, text: &str) -> Vec<std::ops::Range<usize>> {
        let spans = row.sources().filter_map(|span| {
            let bytes = selected(&self.lines, row.entry, &span)?;
            Some(span.to_shown(text, bytes.start)..span.to_shown(text, bytes.end))
        });
        spans.filter(|bytes| !bytes.is_empty()).collect()
    }

    /// The first and last selected bytes as laid out now; an empty
    /// selection's place for both.
    pub fn rows(&self) -> (TextPosition, TextPosition) {
        self.rows
    }

    pub fn text(&self) -> &str {
        &self.text
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{layout, selected_text};
    use super::*;

    fn source_row(text: &str) -> Row {
        let entry = model::Entry::new(
            model::EntryKey::UnsavedStatus(777),
            text.to_owned(),
            Surface::Tool,
        );
        let layout = markdown::RowLayout::default();
        EntryGeometry::new(&entry, 30, 0).row(Line::from(text.to_owned()), false, layout)
    }

    #[test]
    fn selection_preserves_soft_wrapped_text_and_code_whitespace() {
        let source = "  first line with enough text to wrap\n    second line  ";
        let text = format!("```text\n{source}\n```");
        let entry = model::Entry::titled(
            model::EntryKey::UnsavedStatus(2),
            model::Title::plain("Agent"),
            text,
            Surface::Agent,
        );
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
    }

    /// Each entry laid out at its index, as the transcript lays them out.
    fn transcript(entries: &[model::Entry], width: u16) -> RowBlocks {
        let highlights = super::super::super::tool_view::HighlightCache::default();
        let mut blocks = RowBlocks::default();
        for (index, entry) in entries.iter().enumerate() {
            blocks.update_entry(index, Vec::new(), |rows| {
                let columns = RequestColumns::default();
                update_entry_rows(rows, entry, index, width, &highlights, columns)
            });
        }
        blocks
    }

    /// The position of `needle`'s first byte, or of its end with `after`.
    fn find(rows: &RowBlocks, needle: &str, after: bool) -> TextPosition {
        let (row, byte) = (0..rows.len())
            .find_map(|row| Some((row, rows[row].text().find(needle)?)))
            .unwrap();
        let byte = byte + if after { needle.len() } else { 0 };
        TextPosition { row, byte }
    }

    fn select(rows: &RowBlocks, from: &str, to: &str) -> Selection {
        let mut selection = Selection::new(rows, find(rows, from, false)).unwrap();
        selection.extend(rows, find(rows, to, true));
        selection
    }

    fn reasoning(live: bool) -> model::Entry {
        let key = model::EntryKey::UnsavedStatus(1);
        let title = model::Title::disclosed("Reasoning", live);
        let body = if live {
            "Thinking it over\nat length"
        } else {
            ""
        };
        let mut entry = model::Entry::titled(key, title, body.into(), Surface::Reasoning);
        if live {
            entry.timing = model::Timing::Since(0);
        }
        entry
    }

    fn answer(body: &str, footer: Option<&str>) -> model::Entry {
        let key = model::EntryKey::UnsavedStatus(2);
        let title = model::Title::plain("Agent");
        let mut entry = model::Entry::titled(key, title, body.into(), Surface::Agent);
        entry.footer = footer.map(Into::into);
        entry
    }

    const ANSWER: &str =
        "The first paragraph has quite a few words to wrap.\n\nThe second one ends here.";

    /// Settling collapses the reasoning above and adds a footer below, moving
    /// every row of the answer; a resize rewraps it. Neither changes the text.
    #[test]
    fn selection_survives_settling_and_resize_reflow() {
        let streaming = transcript(&[reasoning(true), answer(ANSWER, None)], 40);
        let selection = select(&streaming, "few words", "second");
        let expected = "few words to wrap.\n\nThe second";
        assert_eq!(selection.text(), expected);
        let settled = [reasoning(false), answer(ANSWER, Some("model · 3s"))];
        for width in [40, 24, 80] {
            let rows = transcript(&settled, width);
            let reflowed = selection.clone().reflow(&rows).expect("text is unchanged");
            assert_eq!(reflowed.text(), expected, "width {width}");
            assert_eq!(selected_text(&rows, reflowed.rows()), expected);
            let (start, end) = reflowed.rows();
            assert_eq!(start, find(&rows, "few", false), "width {width}");
            assert_eq!(end, find(&rows, "second", true), "width {width}");
            assert_ne!(start.row, selection.rows().0.row);
        }
        // Streaming more text keeps it; a changed or missing source drops it.
        let grown = answer(&format!("{ANSWER} More."), None);
        let grown = transcript(&[reasoning(true), grown], 40);
        assert!(selection.clone().reflow(&grown).is_some());
        let edited = ANSWER.replace("few", "many");
        for rows in [
            transcript(&[reasoning(true), answer(&edited, None)], 40),
            transcript(&[reasoning(true)], 40),
        ] {
            assert!(selection.clone().reflow(&rows).is_none());
        }
    }

    /// A table rewraps its cells into a different number of rows at each
    /// width; the prose after it keeps its identity.
    #[test]
    fn a_table_rewrapping_above_keeps_a_selection_below_it() {
        let body = "| Key | Description |\n| --- | --- |\n| x | one two three four |\n\nAfter";
        let selection = select(&transcript(&[answer(body, None)], 88), "After", "After");
        assert_eq!(selection.text(), "After");
        for width in [27, 20, 88] {
            let rows = transcript(&[answer(body, None)], width);
            let reflowed = selection.clone().reflow(&rows);
            let text = reflowed.as_ref().map(Selection::text);
            assert_eq!(text, Some("After"), "width {width}");
        }
    }

    /// An untitled live body leaves its spinner's cells outside its text, on
    /// the plain and the Markdown layout alike.
    #[test]
    fn untitled_live_text_keeps_its_selection_when_its_spinner_goes() {
        let entry = |body: &str, surface, live: bool| {
            let key = model::EntryKey::UnsavedStatus(1);
            let mut entry = model::Entry::new(key, body.into(), surface);
            if live {
                entry.timing = model::Timing::Until(0);
            }
            entry
        };
        // A retry's countdown: untitled, live and selectable.
        let status = |live| entry("Retrying in a moment", Surface::Status, live);
        let selection = select(&transcript(&[status(true)], 40), "Retrying", "Retrying");
        let settled = transcript(&[status(false)], 40);
        let reflowed = selection
            .reflow(&settled)
            .expect("the status keeps its text");
        assert_eq!(reflowed.text(), "Retrying");
        assert_eq!(reflowed.rows().0, find(&settled, "Retrying", false));
        // Single-line reasoning, through the plain and Markdown layouts: its
        // spinner paints in a gutter before the text, not over it.
        for body in ["Thinking it over", "Thinking *it* over"] {
            let rows = layout(&entry(body, Surface::Reasoning, true), 40);
            assert_eq!(rows[0].text(), "Thinking it over", "{body:?}");
            assert_eq!(rows[0].spinner, Some(rows[0].inset - 2), "{body:?}");
        }
        // A message box has no header row for a spinner, so reserves no gutter.
        let boxed = |live| layout(&entry("Thinking it over", Surface::Agent, live), 40);
        let geometry = |rows: Vec<Row>| {
            rows.iter()
                .map(|row| (row.inset, row.text()))
                .collect::<Vec<_>>()
        };
        assert_eq!(geometry(boxed(true)), geometry(boxed(false)));
    }

    /// Table cells keep their identities when other rows or columns rewrap:
    /// a selected cell stays selected, in place, never moving to an equal one.
    #[test]
    fn table_cells_keep_their_identity_through_rewrapping() {
        let long = "one two three four five six seven";
        for (body, column) in [
            (
                format!("| A | B |\n| - | - |\n| keep | {long} |\n| keep | x |"),
                0,
            ),
            (
                format!("| A | B |\n| - | - |\n| {long} | x |\n| y | keep |"),
                1,
            ),
        ] {
            let rows = transcript(&[answer(&body, None)], 80);
            // The last "keep": in the later data row.
            let last = |rows: &RowBlocks| {
                let row = (0..rows.len()).rfind(|&row| rows[row].text().contains("keep"));
                let row = row.unwrap();
                let byte = rows[row].text().rfind("keep").unwrap();
                (
                    TextPosition { row, byte },
                    TextPosition {
                        row,
                        byte: byte + 4,
                    },
                )
            };
            let (start, end) = last(&rows);
            let mut selection = Selection::new(&rows, start).unwrap();
            selection.extend(&rows, end);
            assert_eq!(selection.text(), "keep");
            for width in [30, 24, 80] {
                let rows = transcript(&[answer(&body, None)], width);
                let wide = transcript(&[answer(&body, None)], 80).len();
                assert!(
                    width == 80 || rows.len() > wide,
                    "the long cell wraps at {width}"
                );
                let reflowed = selection.clone().reflow(&rows);
                let reflowed = reflowed.unwrap_or_else(|| panic!("column {column}, width {width}"));
                assert_eq!(
                    reflowed.rows(),
                    last(&rows),
                    "column {column}, width {width}"
                );
            }
        }
    }

    /// A wrapped cell's fragments are one text: selecting part of one copies
    /// just that part, and a whole cell copies whole however it wraps,
    /// without borders, padding, other columns or visual wraps.
    #[test]
    fn wrapped_table_cells_copy_only_their_text() {
        for (body, width) in [
            ("| H |\n| - |\n| abcdef |", 15),
            ("| H | I |\n| - | - |\n| abcdef | xy |", 20),
        ] {
            let rows = transcript(&[answer(body, None)], width);
            let visible = select(&rows, "abc", "abc");
            assert_eq!(visible.text(), "abc", "{body:?} at {width}");
            let whole = select(&transcript(&[answer(body, None)], 80), "abcdef", "abcdef");
            // Narrow enough to wrap the cell, and to drop the grid.
            for width in [width, 12, 80] {
                let rows = transcript(&[answer(body, None)], width);
                let reflowed = whole.clone().reflow(&rows);
                let text = reflowed.as_ref().map(Selection::text);
                assert_eq!(text, Some("abcdef"), "{body:?} at {width}");
            }
        }
    }

    /// A grapheme too wide for a narrow cell is shown replaced, but positions
    /// still refer to the cell's own text.
    #[test]
    fn replaced_graphemes_keep_cell_positions() {
        let body = "| H |\n| - |\n| 😀aa |";
        let narrow = transcript(&[answer(body, None)], 9);
        let second = (0..narrow.len())
            .rfind(|&row| narrow[row].text() == "a")
            .unwrap();
        let at = |byte| TextPosition { row: second, byte };
        let mut selection = Selection::new(&narrow, at(0)).unwrap();
        selection.extend(&narrow, at(1));
        assert_eq!(selection.text(), "a");
        let wide = transcript(&[answer(body, None)], 84);
        let row = (0..wide.len())
            .find(|&row| wide[row].text().contains("😀aa"))
            .unwrap();
        let byte = wide[row].text().rfind('a').unwrap();
        let reflowed = selection.reflow(&wide).expect("the cell keeps its text");
        assert_eq!(reflowed.rows().0, TextPosition { row, byte });
    }

    /// What the frame highlights of row `index`.
    fn highlights(
        selection: &Selection,
        rows: &RowBlocks,
        index: usize,
    ) -> Vec<std::ops::Range<usize>> {
        let row = &rows[index];
        if selection.covers(index) {
            selection.highlights(row, &row.text())
        } else {
            Vec::new()
        }
    }

    /// Copy and highlight of every row from `from` through `to` (its end).
    fn copy_and_highlights(rows: &RowBlocks, from: &str, to: &str) -> (String, Vec<bool>) {
        let selection = select(rows, from, to);
        let (start, end) = (find(rows, from, false).row, find(rows, to, true).row);
        let lit = (start..=end).map(|row| {
            let len = rows[row].text().len();
            let covered = highlights(&selection, rows, row)
                .iter()
                .map(|bytes| bytes.len())
                .sum::<usize>();
            len == 0 || rows[row].sources().next().is_none() || len == covered
        });
        (selection.text().to_owned(), lit.collect())
    }

    /// Nested containers close with events from earlier in the source; the
    /// lines after them still copy and highlight in document order.
    #[test]
    fn nested_markdown_copies_and_highlights_in_document_order() {
        for body in [
            "- first\n  - nested\n    continuation\n- second",
            "- first\n  - nested\n    continuation\n- second\n\nafter",
            "> - first\n>   - nested\n>     continuation\n> - second\n\nafter",
            "- first\n\n  ```\n  code\n  ```\n\n  - nested\n    continuation\n- second\n\nafter",
        ] {
            let rows = transcript(&[answer(body, None)], 40);
            let last = body.rsplit(['\n', ' ']).next().unwrap();
            let (copied, lit) = copy_and_highlights(&rows, "first", last);
            let expected = markdown::render(body, false, 32);
            let expected: Vec<_> = expected.iter().map(ToString::to_string).collect();
            let expected = expected.join("\n");
            let expected = &expected[expected.find("first").unwrap()..];
            assert_eq!(copied, expected, "{body:?}");
            assert!(lit.iter().skip(1).all(|&lit| lit), "{body:?}: {lit:?}");
        }
    }

    /// A cell with no visible text still holds its source: an empty cell is
    /// an empty copied line however narrow the table, and hidden whitespace
    /// is still copied.
    #[test]
    fn invisible_table_cells_keep_their_source() {
        for (body, expected) in [
            ("| A | B |\n| - | - |\n| a | |\n| b | c |", "a\n\nb\nc"),
            ("| A | B |\n| - | - |\n| a | ` ` |\n| b | c |", "a\n \nb\nc"),
        ] {
            let wide = transcript(&[answer(body, None)], 80);
            let selection = select(&wide, "a", "c");
            assert_eq!(selection.text(), expected, "{body:?}");
            // Markdown layout width 6: borderless.
            let narrow = transcript(&[answer(body, None)], 14);
            let reflowed = selection.reflow(&narrow).expect(body);
            assert_eq!(reflowed.text(), expected);
            for cell in ["a", "b", "c"] {
                let row = find(&narrow, cell, false).row;
                assert!(!highlights(&reflowed, &narrow, row).is_empty(), "{cell}");
            }
        }
    }

    /// A table in a task item keeps the item's checkbox indent in its width,
    /// so its cells stay selectable when narrow.
    #[test]
    fn tables_in_task_items_stay_selectable_when_narrow() {
        let body = "- [ ] task\n\n  | H |\n  | - |\n  | abcdef |";
        let selection = select(&transcript(&[answer(body, None)], 80), "abcdef", "abcdef");
        let narrow = transcript(&[answer(body, None)], 14);
        let reflowed = selection.reflow(&narrow).expect("the cell keeps its text");
        assert_eq!(reflowed.text(), "abcdef");
        let (start, end) = reflowed.rows();
        let lit =
            (start.row..=end.row).filter(|&row| !highlights(&reflowed, &narrow, row).is_empty());
        let shown: String = lit
            .map(|row| narrow[row].text())
            .collect::<Vec<_>>()
            .concat();
        let letters: String = shown.split_whitespace().collect();
        assert_eq!(
            letters, "abcdef",
            "every letter shown is selected: {shown:?}"
        );
    }

    /// Dragging past a table, onto a row without source or past a row's end,
    /// takes in every cell before the pointer in reading order, however the
    /// columns wrap, whichever way the drag runs.
    #[test]
    fn dragging_past_a_table_keeps_its_last_cells() {
        let mut checked = 0;
        for (body, widths) in [
            ("| A | B |\n| - | - |\n| aa bb | cc |", 20..=20),
            ("> | A | B |\n> | - | - |\n> | aa bb | cc |", 19..=24),
            ("- | A | B |\n  | - | - |\n  | aa bb | cc |", 19..=24),
        ] {
            for width in widths {
                let rows = transcript(&[answer(body, None)], width);
                // The first column must wrap onto a row the last does not reach.
                let past_bb = (0..rows.len()).find(|&row| {
                    let text = rows[row].text();
                    text.contains("bb") && !text.contains('c')
                });
                let Some(past_bb) = past_bb else {
                    continue;
                };
                checked += 1;
                let ends = [
                    TextPosition {
                        row: rows.len() - 1,
                        byte: 0,
                    },
                    TextPosition {
                        row: past_bb,
                        byte: usize::MAX,
                    },
                ];
                for end in ends {
                    let start = find(&rows, "A ", false);
                    for (from, to) in [(start, end), (end, start)] {
                        let mut selection = Selection::new(&rows, from).unwrap();
                        selection.extend(&rows, to);
                        assert_eq!(
                            selection.text(),
                            "A\nB\naa bb\ncc",
                            "{body:?} at {width}, {end:?}"
                        );
                        let cc = find(&rows, "cc", false).row;
                        assert!(!highlights(&selection, &rows, cc).is_empty());
                    }
                }
            }
        }
        assert!(
            checked >= 3,
            "{checked} layouts wrapped the first column alone"
        );
    }

    /// Cells padded onto a short row are distinct, if empty: each copies as
    /// a line of its own.
    #[test]
    fn padded_cells_copy_as_lines_of_their_own() {
        let body = "| A | B | C |\n| - | - | - |\n| x |\n\nafter";
        let rows = transcript(&[answer(body, None)], 80);
        assert_eq!(select(&rows, "x", "after").text(), "x\n\n\n\nafter");
    }

    /// The spinner's gutter is decoration: a selected title outlives it.
    #[test]
    fn a_selected_title_survives_its_spinner_gutter_going_away() {
        let selection = select(
            &transcript(&[reasoning(true)], 40),
            "Reasoning",
            "Reasoning",
        );
        assert_eq!(selection.text(), "Reasoning");
        let settled = transcript(&[reasoning(false)], 40);
        let reflowed = selection.reflow(&settled);
        assert_eq!(reflowed.as_ref().map(Selection::text), Some("Reasoning"));
    }

    /// Rows without source (box edges, separators) snap to the entry's
    /// preceding source, or its first when nothing precedes them.
    #[test]
    fn selections_from_rows_without_source_snap_to_adjacent_text() {
        let rows = transcript(&[answer("Only line", None)], 40);
        let edges: Vec<_> = (0..rows.len())
            .filter(|&row| rows[row].sources().next().is_none())
            .collect();
        let (top, bottom) = (edges[0], *edges.last().unwrap());
        let at = |row| TextPosition { row, byte: 0 };
        let mut selection = Selection::new(&rows, at(top)).unwrap();
        selection.extend(&rows, at(bottom));
        assert_eq!(selection.text(), "Agent\nOnly line");
    }

    #[test]
    fn checked_row_text_rejects_interior_utf8_and_wrong_row_endpoints() {
        let row = source_row("é界👩‍💻é");
        let mut rows = RowBlocks::default();
        rows.replace_entry(0, vec![row.clone()], Vec::new());
        let start = TextPosition { row: 0, byte: 0 };
        for byte in [1, 3, 6] {
            let end = TextPosition { row: 0, byte };
            assert!(Selection::new(&rows, end).is_none());
            assert!(selected_text(&rows, (start, end)).is_empty());
        }
        let end = TextPosition {
            row: 0,
            byte: usize::MAX,
        };
        assert_eq!(selected_text(&rows, (end, start)), row.text());
        assert!(selected_text(&rows, (start, TextPosition { row: 1, byte: 0 })).is_empty());
    }

    fn card(state: &'static str, output: &[String]) -> model::Entry {
        use super::super::super::tool_view::{Document, Role, Run};
        let mut document = Document::default();
        for line in output {
            document.line(line.as_str(), Role::Plain);
        }
        let header = vec![
            Run::new("exec · ", Role::ToolName),
            Run::new(state, Role::Muted),
        ];
        model::Entry::card(model::EntryKey::UnsavedStatus(3), header, Some(document))
    }

    fn lines(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|&line| line.to_owned()).collect()
    }

    /// The position of the `n`th row that is exactly `text`, or its end.
    fn nth(rows: &RowBlocks, text: &str, n: usize, after: bool) -> TextPosition {
        let row = (0..rows.len()).filter(|&row| rows[row].text() == text);
        let row = row.clone().nth(n).unwrap();
        let byte = if after { text.len() } else { 0 };
        TextPosition { row, byte }
    }

    /// A tool's output lines are told apart by the output before them: a line
    /// inserted above or inside a selection drops it, even where equal text
    /// moves into its place. Appending below, or the header's state changing,
    /// keeps it where it was.
    #[test]
    fn tool_output_selections_drop_when_lines_shift_under_them() {
        let output = lines(&["same", "same", "same", "last"]);
        let rows = transcript(&[card("Running", &output)], 40);
        let mut selection = Selection::new(&rows, nth(&rows, "same", 1, false)).unwrap();
        selection.extend(&rows, nth(&rows, "same", 2, true));
        assert_eq!(selection.text(), "same\nsame");
        for shifted in [
            ["cut", "same", "same", "same", "last"],
            ["same", "cut", "same", "same", "last"],
            ["same", "same", "cut", "same", "last"],
        ] {
            let rows = transcript(&[card("Running", &lines(&shifted))], 40);
            assert!(selection.clone().reflow(&rows).is_none(), "{shifted:?}");
        }
        let appended = lines(&["same", "same", "same", "last", "same"]);
        for rows in [
            transcript(&[card("Running", &appended)], 40),
            transcript(&[card("Completed", &output)], 40),
        ] {
            let reflowed = selection.clone().reflow(&rows).expect("its lines stay put");
            assert_eq!(reflowed.text(), "same\nsame");
            let shown = (nth(&rows, "same", 1, false), nth(&rows, "same", 2, true));
            assert_eq!(reflowed.rows(), shown);
        }
    }

    /// A drag across a long output reads the entry's current layout at every
    /// motion, including after the entry is laid out again mid-drag.
    #[test]
    fn dragging_across_a_long_output_follows_relayout() {
        let output: Vec<_> = (0..5000)
            .map(|line| format!("output line {line}"))
            .collect();
        let entry = card("Running", &output);
        let mut rows = transcript(std::slice::from_ref(&entry), 80);
        let unwrapped = rows.len();
        let mut selection = select(&rows, "output line 10", "output line 10");
        for line in [4000, 2500, 12] {
            selection.extend(&rows, find(&rows, &format!("output line {line}"), true));
        }
        assert_eq!(selection.text(), output[10..=12].join("\n"));
        // Narrow enough to wrap every line onto two rows.
        let highlights = super::super::super::tool_view::HighlightCache::default();
        rows.update_entry(0, Vec::new(), |rows| {
            update_entry_rows(rows, &entry, 0, 16, &highlights, RequestColumns::default())
        });
        assert!(rows.len() > unwrapped + output.len() / 2);
        let end = find(&rows, "4000", true);
        selection.extend(&rows, end);
        assert_eq!(selection.text(), output[10..=4000].join("\n"));
        assert_eq!(selection.rows().1, end);
        assert_eq!(selected_text(&rows, selection.rows()), selection.text());
    }
}
