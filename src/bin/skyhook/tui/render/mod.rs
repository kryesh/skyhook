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
mod stream;
mod wrapping;

// Kept as part of the renderer API even when only inline tests name it directly.
#[allow(unused_imports)]
pub use cache::CachedEntry;
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
