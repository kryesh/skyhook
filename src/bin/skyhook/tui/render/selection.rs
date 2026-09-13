//! Visual rows, source coordinates, and text selection.

use super::*;

#[derive(Clone)]
pub struct Row {
    pub(super) line: std::sync::Arc<Line<'static>>,
    pub(super) header: bool,
    pub(super) x: u16,
    pub(super) width: u16,
    pub(super) surface: Surface,
    pub entry: usize,
    pub selectable: bool,
    pub(super) blank: bool,
    pub(super) continued: bool,
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
    entry.expandable
        || !(entry.surface == Surface::Reasoning
            || (entry.surface == Surface::Muted && entry.running))
}

/// Match App::toggle's defaults, including open reasoning and historical tools.
pub(super) fn entry_expanded(entry: &model::Entry, view: &model::View, details: bool) -> bool {
    let all = details
        && (entry.job.is_some()
            || (view.tab == Tab::Conversation && entry.surface == Surface::Tool));
    entry.expandable && view.is_expanded(&entry.key, all || entry.default_open)
}

impl Row {
    pub fn text(&self) -> String {
        self.line.to_string()
    }
    pub(super) fn paragraph_x(&self) -> u16 {
        self.x + self.inset + u16::from(matches!(self.surface, Surface::User | Surface::Agent)) * 2
    }
    pub(super) fn source_column(&self, text: &str, byte: usize) -> usize {
        let prefix = self.layout.source_prefix.min(text.len());
        let leading = self.layout.prefix.width();
        if byte < prefix {
            leading
                + text[..byte]
                    .graphemes(true)
                    .map(|g| g.width())
                    .sum::<usize>()
                    .min(self.layout.source_prefix_width)
        } else {
            self.layout
                .code
                .map_or(leading + self.layout.source_prefix_width, |code| {
                    code.indent + code.padding
                })
                + text[prefix..byte]
                    .graphemes(true)
                    .map(|g| g.width())
                    .sum::<usize>()
        }
    }
    #[cfg(test)]
    pub(super) fn text_x(&self) -> u16 {
        self.paragraph_x()
            .saturating_add(self.source_column(&self.text(), 0) as u16)
    }
    pub fn byte_at_column(&self, column: u16) -> usize {
        let target = column.saturating_sub(self.paragraph_x()) as usize;
        let text = self.text();
        let mut column = self.layout.prefix.width();
        for (byte, grapheme) in text.grapheme_indices(true) {
            if byte == self.layout.source_prefix {
                column = self.layout.code.map_or(
                    self.layout.prefix.width() + self.layout.source_prefix_width,
                    |code| code.indent + code.padding,
                );
            }
            let clipped_prefix = byte < self.layout.source_prefix
                && column + grapheme.width()
                    > self.layout.prefix.width() + self.layout.source_prefix_width;
            if !clipped_prefix && target < column + grapheme.width() {
                return byte;
            }
            column += grapheme.width();
        }
        text.len()
    }
    pub(super) fn selection_range(
        &self,
        row: usize,
        selection: Option<(TextPosition, TextPosition)>,
    ) -> Option<std::ops::Range<usize>> {
        let (a, b) = selection?;
        let (start, end) = (a.min(b), a.max(b));
        if !self.selectable
            || self.blank
            || self.layout.decorative
            || start == end
            || row < start.row
            || row > end.row
        {
            return None;
        }
        let length = self.line.spans.iter().map(|span| span.content.len()).sum();
        let start_byte = if row == start.row {
            start.byte.min(length)
        } else {
            0
        };
        let end_byte = if row == end.row {
            end.byte.min(length)
        } else {
            length
        };
        Some(start_byte..end_byte)
    }
}

pub fn selected_text(rows: &RowBlocks, selection: (TextPosition, TextPosition)) -> String {
    let mut text = String::new();
    let mut first = true;
    let start = selection.0.row.min(selection.1.row);
    let end = selection.0.row.max(selection.1.row);
    for index in start..=end {
        let Some(row) = rows.get(index) else { break };
        if let Some(range) = row.selection_range(index, Some(selection)) {
            if !first && !row.continued {
                text.push('\n');
            }
            text.push_str(&row.text()[range]);
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
    previous.len() == end.row - start.row + 1
        && previous.iter().enumerate().all(|(offset, before)| {
            let Some(after) = current.next() else {
                return false;
            };
            if before.entry != after.entry
                || before.continued != after.continued
                || before.selectable != after.selectable
            {
                return false;
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
    use super::*;

    fn expandable_entry() -> model::Entry {
        model::Entry {
            key: "entry".into(),
            text:
                "first header with many wrapped fragments\nbody with many wrapped fragments\n  \n"
                    .into(),
            surface: Surface::Tool,
            expandable: true,
            default_open: false,
            running: false,
            footer: None,
            request: None,
            indent: 0,
            job: None,
            compact_after: false,
            header: None,
            document: None,
        }
    }

    #[test]
    fn expansion_matches_toggle_defaults_and_explicit_overrides() {
        let mut entry = expandable_entry();
        let mut view = model::View::default();
        assert!(!entry_expanded(&entry, &view, false));
        assert!(entry_expanded(&entry, &view, true));
        view.tab = Tab::Jobs;
        assert!(!entry_expanded(&entry, &view, true));
        entry.job = Some(skyhook::identity::JobId::new(1).unwrap());
        assert!(entry_expanded(&entry, &view, true));
        entry.job = None;
        entry.surface = Surface::Reasoning;
        entry.default_open = true;
        assert!(entry_expanded(&entry, &view, false));
        view.collapsed.insert(entry.key.clone());
        assert!(!entry_expanded(&entry, &view, true));
        view.expanded.insert(entry.key.clone());
        assert!(!entry_expanded(&entry, &view, true));
        view.collapsed.clear();
        entry.default_open = false;
        assert!(entry_expanded(&entry, &view, false));
        entry.expandable = false;
        assert!(!entry_expanded(&entry, &view, true));
    }

    #[test]
    fn selection_preserves_soft_wrapped_text_and_code_whitespace() {
        let source = "  first line with enough text to wrap\n    second line  ";
        let entry = model::Entry {
            key: "message".into(),
            text: format!("Agent\n```text\n{source}\n```"),
            surface: Surface::Agent,
            expandable: false,
            default_open: false,
            running: false,
            footer: None,
            request: None,
            indent: 0,
            job: None,
            compact_after: false,
            header: None,
            document: None,
        };
        let rows = layout(std::slice::from_ref(&entry), 30, Palette::new(), None);
        let start = rows
            .iter()
            .position(|row| row.text().starts_with("  first"))
            .unwrap();
        let end = rows
            .iter()
            .rposition(|row| row.text().ends_with("second line  "))
            .unwrap();
        let selection = (
            TextPosition {
                row: start,
                byte: 2,
            },
            TextPosition {
                row: end,
                byte: rows[end].text().len(),
            },
        );
        let mut blocks = RowBlocks::default();
        *blocks.block_mut(0) = rows.clone();
        blocks.finish_update(0);
        assert_eq!(
            selected_text(&blocks, selection),
            source.strip_prefix("  ").unwrap()
        );
        assert_eq!(
            selected_text(&blocks, (selection.1, selection.0)),
            source.strip_prefix("  ").unwrap()
        );
        let mut appended = entry.clone();
        appended.text.push_str("\nLater streaming text");
        let updated = layout(&[appended], 30, Palette::new(), None);
        assert!(selection_unchanged(
            &rows[start..=end],
            updated.iter().skip(start),
            selection
        ));
        let mut changed = entry;
        changed.text = changed.text.replace("first", "other");
        let updated = layout(&[changed], 30, Palette::new(), None);
        assert!(!selection_unchanged(
            &rows[start..=end],
            updated.iter().skip(start),
            selection
        ));
    }
    fn layout(
        entries: &[model::Entry],
        width: u16,
        p: Palette,
        highlights: Option<&super::super::super::tool_view::HighlightCache>,
    ) -> Vec<Row> {
        let fallback = super::super::super::tool_view::HighlightCache::default();
        let highlights = highlights.unwrap_or(&fallback);
        let mut rows = Vec::new();
        for (index, entry) in entries.iter().enumerate() {
            update_entry_rows(
                &mut rows,
                &mut CachedEntry::default(),
                entry,
                index,
                EntryLayout {
                    width,
                    palette: p,
                    highlights,
                    request_columns: RequestColumns::default(),
                    expanded: entry.default_open,
                },
                None,
            );
        }
        rows
    }
}
