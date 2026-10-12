//! Terminal rendering: cached layout, selection geometry, and frame painting.
mod cache;
mod columns;
mod document;
mod frame;
mod markdown;
mod menu;
mod painting;
mod panels;
mod row;
mod rows;
mod selection;
mod sidebar;
mod stream;
mod wrapping;

pub use cache::RenderState;
pub use frame::draw;
pub(super) use markdown::{Section, sections};
pub(super) use panels::scroll_to;
pub use row::Row;
pub use rows::RowBlocks;
pub use selection::{Selection, TextPosition};

use cache::*;
use columns::*;
use document::*;
use menu::*;
use painting::*;
use panels::*;
use sidebar::*;
use wrapping::*;

use super::{
    app::{
        App, Emphasis, Focus, Hit, InputTarget, ItemRef, MenuKind, Overlay, SavedState, SessionRef,
    },
    editor::TextField,
    model::{self, Surface, Tab},
    theme::THEME,
};
use ratatui::{
    Frame,
    buffer::Buffer,
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[cfg(test)]
mod tests {
    use super::*;

    /// One selectable tool row for layout fixtures.
    pub(super) fn fixture_row(
        line: Line<'static>,
        layout: markdown::RowLayout,
        x: u16,
        width: u16,
        entry: usize,
    ) -> Row {
        Row {
            line: std::sync::Arc::new(line),
            x,
            width,
            inset: 0,
            text_width: width,
            surface: Surface::Tool,
            entry,
            entry_key: std::sync::Arc::new(model::EntryKey::UnsavedStatus(entry)),
            selectable: true,
            layout,
            spinner: None,
        }
    }

    /// The text a selection from `a` to `b` copies.
    pub(super) fn selected_text(rows: &RowBlocks, (a, b): (TextPosition, TextPosition)) -> String {
        let selection = Selection::new(rows, a).map(|mut selection| {
            selection.extend(rows, b);
            selection
        });
        selection.map_or_else(String::new, |selection| selection.text().to_owned())
    }

    /// Where a row's text starts: the column of its first source cell.
    pub(super) fn text_x(row: &Row) -> u16 {
        let (_, prefix_width) = row.layout.source_prefix();
        let body = row.layout.prefix.width() + prefix_width;
        let body = row.layout.code().map_or(body, |code| code.body_start());
        let first = row.text_view().source_cells().next();
        row.paragraph_x() + first.map_or(body, |(_, column, _)| column) as u16
    }

    /// Whether a row continues a wrapped logical line.
    pub(super) fn continued(layout: &markdown::RowLayout) -> bool {
        matches!(
            layout.row_source(),
            Some(markdown::RowSource::Line { start, .. }) if *start > 0
        )
    }

    /// An entry's rows at `width`, as the transcript lays them out.
    pub(super) fn layout(entry: &model::Entry, width: u16) -> Vec<Row> {
        let mut rows = Vec::new();
        let highlights = super::super::tool_view::HighlightCache::default();
        let columns = RequestColumns::default();
        update_entry_rows(&mut rows, entry, 0, width, &highlights, columns);
        rows
    }

    pub(super) fn expandable_entry() -> model::Entry {
        model::Entry::titled(
            model::EntryKey::UnsavedStatus(1),
            model::Title::disclosed("first header with many wrapped fragments", true),
            "body with many wrapped fragments\n  \n".into(),
            Surface::Tool,
        )
    }
}
