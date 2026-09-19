//! Terminal rendering: cached layout, selection geometry, and frame painting.
mod cache;
mod code;
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
pub use painting::Palette;
pub use rows::RowBlocks;
pub use selection::{Row, TextPosition, entry_selectable, selected_text};
pub use wrapping::wrap_plain;

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
    app::{App, Focus, Hit, MenuKind},
    model::{self, Surface, Tab},
};
use ratatui::{
    Frame,
    buffer::Buffer,
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
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
            surface: Surface::Tool,
            entry,
            entry_key: std::sync::Arc::new(model::EntryKey::Record(entry as u64)),
            selectable: true,
            layout,
            inset: 0,
        }
    }

    pub(super) fn expandable_entry() -> model::Entry {
        model::Entry::expandable_text(
            model::EntryKey::Record(1),
            "first header with many wrapped fragments\nbody with many wrapped fragments\n  \n"
                .into(),
            Surface::Tool,
        )
    }
}
