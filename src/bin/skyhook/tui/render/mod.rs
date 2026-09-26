//! Terminal rendering: cached layout, selection geometry, and frame painting.
mod cache;
mod columns;
mod document;
mod frame;
mod markdown;
mod menu;
mod painting;
mod panels;
mod rows;
mod selection;
mod sidebar;
mod stream;
mod wrapping;

pub use cache::RenderState;
pub use frame::draw;
pub use rows::RowBlocks;
pub use selection::{Row, TextPosition, selected_text};

use cache::*;
use columns::*;
use document::*;
use menu::*;
use painting::*;
use panels::*;
use selection::*;
use sidebar::*;
use wrapping::*;

use super::{
    app::{App, Focus, Hit, MenuKind, SessionRef},
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
        }
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
