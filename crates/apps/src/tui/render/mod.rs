//! Terminal rendering: cached layout, selection geometry, and frame painting.
mod cache;
mod columns;
mod composer;
mod content;
mod document;
mod frame;
mod menu;
mod page;
mod painting;
mod prompt;
mod row;
mod rows;
mod selection;
pub(crate) mod sidebar;
mod track;
mod tree;

pub use cache::RenderState;
#[cfg(test)]
pub(crate) use cache::visible_entries as visible;
pub(crate) use cache::{Top, anchor};
pub use frame::draw;
pub(super) use page::PageView;
pub use row::Row;
pub use selection::{Selection, TextPosition};
pub(crate) use track::{Spot, Track};

#[cfg(test)]
pub(crate) mod tests {
    pub(crate) use super::cache::tests::statuses;
    use super::*;
    use crate::tui::markdown;
    use crate::tui::model;
    use crate::tui::model::Surface;
    use crate::tui::render::columns::RequestColumns;
    use crate::tui::render::document::update_entry_rows;
    use crate::tui::render::rows::RowBlocks;
    use ratatui::text::Line;

    /// One selectable tool row for layout fixtures.
    pub(super) fn fixture_row(
        line: Line<'static>,
        layout: markdown::RowLayout,
        x: u16,
        width: u16,
    ) -> Row {
        Row {
            line: std::sync::Arc::new(line),
            x,
            width,
            inset: 0,
            text_width: width,
            surface: Surface::Tool,
            entry_key: std::sync::Arc::new(model::EntryKey::UnsavedStatus(0)),
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

    /// An entry's rows at `width`, as the transcript lays them out.
    pub(super) fn layout(entry: &model::Entry, width: u16) -> Vec<Row> {
        let mut rows = Vec::new();
        let highlights = super::super::tool_view::HighlightCache::default();
        let columns = RequestColumns::default();
        update_entry_rows(&mut rows, entry, width, &highlights, columns);
        rows
    }
}
