//! Markdown shared by saved responses, reasoning, and live streaming fragments.
//! Soft breaks intentionally remain newlines in chat. Block boundaries add one
//! blank row, while tight list items and code-block lines remain consecutive.

mod layout;
mod parser;
mod tables;

use ratatui::text::Line;

pub(super) use layout::layout_highlighted;
pub(super) use parser::options;
#[cfg(test)]
pub(super) use parser::tests::{render, render_highlighted};

/// Source and decoration stay separate: wrapping and code padding must not add
/// bytes/newlines to selection. Prefix boundaries are recorded by the parser,
/// never inferred from colors or whitespace in the rendered spans.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct RowLayout {
    pub prefix: Line<'static>,
    pub source_prefix: usize,
    pub source_prefix_width: usize,
    pub code: Option<CodeRow>,
    pub decorative: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CodeRow {
    pub indent: usize,
    pub width: usize,
    pub padding: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct LayoutLine {
    pub line: Line<'static>,
    pub continued: bool,
    pub layout: RowLayout,
}

impl From<(Line<'static>, bool)> for LayoutLine {
    fn from((line, continued): (Line<'static>, bool)) -> Self {
        Self {
            line,
            continued,
            layout: RowLayout::default(),
        }
    }
}
