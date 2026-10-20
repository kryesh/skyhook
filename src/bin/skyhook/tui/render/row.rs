//! A visual row of a laid-out entry and the geometry of its text.

use super::*;

#[derive(Clone)]
pub struct Row {
    pub(super) line: std::sync::Arc<Line<'static>>,
    /// The row's filled cells.
    pub(super) x: u16,
    pub(super) width: u16,
    /// Text starts `inset` cells into the row and spans `text_width` cells.
    pub(super) inset: u16,
    pub(super) text_width: u16,
    pub(super) surface: Surface,
    pub(super) entry_key: std::sync::Arc<model::EntryKey>,
    pub selectable: bool,
    pub(super) layout: markdown::RowLayout,
    /// Cells from `x` to where a live entry's spinner paints over this row,
    /// in space its layout kept for it; none where none was kept.
    pub(super) spinner: Option<u16>,
}

impl Row {
    pub fn text(&self) -> String {
        let mut text = String::with_capacity(self.text_len());
        text.extend(self.line.spans.iter().map(|span| &*span.content));
        text
    }
    fn text_len(&self) -> usize {
        markdown::text_len(&self.line)
    }
    /// The source a selectable row shows, in order.
    pub(super) fn sources(
        &self,
    ) -> impl Iterator<Item = std::borrow::Cow<'_, markdown::SourceSpan>> {
        (self
            .selectable
            .then(|| self.layout.sources(self.text_len())))
        .into_iter()
        .flatten()
    }
    pub(super) fn paragraph_x(&self) -> u16 {
        self.x + self.inset
    }
    pub(super) fn text_view(&self) -> RowText<'_> {
        RowText {
            row: self,
            text: self.text(),
        }
    }
    pub fn byte_at_column(&self, column: u16) -> usize {
        let target = column.saturating_sub(self.paragraph_x()) as usize;
        let (prefix_bytes, prefix_width) = self.layout.source_prefix();
        let prefix_end = self.layout.prefix.width() + prefix_width;
        let view = self.text_view();
        let hit = view.source_cells().find(|&(byte, column, width)| {
            let clipped_prefix = byte < prefix_bytes && column + width > prefix_end;
            !clipped_prefix && target < column + width
        });
        hit.map_or(view.text().len(), |(byte, _, _)| byte)
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
    pub(super) fn endpoint(&self, byte: usize) -> Option<usize> {
        let byte = byte.min(self.text.len());
        self.text.is_char_boundary(byte).then_some(byte)
    }
    /// Every grapheme as (byte, source column, width), in one pass over the row.
    pub fn source_cells(&self) -> impl Iterator<Item = (usize, usize, usize)> + '_ {
        fn columns(
            text: &str,
            offset: usize,
            start: usize,
            limit: usize,
        ) -> impl Iterator<Item = (usize, usize, usize)> + '_ {
            cells(text).scan(0usize, move |used, (byte, _, width)| {
                let column = start + (*used).min(limit);
                *used += width;
                Some((offset + byte, column, width))
            })
        }
        let layout = &self.row.layout;
        let (prefix_bytes, prefix_width) = layout.source_prefix();
        let leading = layout.prefix.width();
        let body = layout
            .code()
            .map_or(leading + prefix_width, |code| code.body_start());
        self.endpoint(prefix_bytes)
            .into_iter()
            .flat_map(move |prefix| {
                columns(&self.text[..prefix], 0, leading, prefix_width).chain(columns(
                    &self.text[prefix..],
                    prefix,
                    body,
                    usize::MAX,
                ))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::layout;
    use super::*;

    /// The single-pass painter geometry agrees with per-byte lookup: one cell per
    /// grapheme, in code and in nested, wide, and joined text alike.
    #[test]
    fn source_cells_visit_every_grapheme_once() {
        let entry = |key, text: &str| {
            model::Entry::titled(
                model::EntryKey::UnsavedStatus(key),
                model::Title::plain("Agent"),
                text.to_owned(),
                Surface::Agent,
            )
        };
        let code = entry(
            2,
            "```text\n  first line with enough text to wrap\n    second line  \n```",
        );
        let list = entry(
            3,
            "- 界 wide item that wraps onto more rows\n  > quoted 👩‍💻 text",
        );
        for row in layout(&code, 30).iter().chain(&layout(&list, 16)) {
            let cells = row.text_view().source_cells().count();
            assert_eq!(cells, row.text().graphemes(true).count());
        }
    }
}
