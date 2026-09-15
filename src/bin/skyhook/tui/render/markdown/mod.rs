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
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RowLayout {
    pub prefix: Line<'static>,
    /// Header is an entry overlay, not source text: it can mark a decorative
    /// code-surface row (preserving a running reasoning spinner), never a spacer.
    header: bool,
    kind: RowKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RowKind {
    Source(SourceLayout),
    CodeDecoration(CodeGeometry),
    Spacer,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct SourceLayout {
    prefix_bytes: usize,
    prefix_width: usize,
    code: Option<CodeGeometry>,
    continued: bool,
}

impl Default for RowLayout {
    fn default() -> Self {
        Self {
            prefix: Line::default(),
            header: false,
            kind: RowKind::Source(SourceLayout::default()),
        }
    }
}

impl RowLayout {
    pub fn source(
        prefix: Line<'static>,
        prefix_bytes: usize,
        prefix_width: usize,
        code: Option<CodeGeometry>,
    ) -> Self {
        Self {
            prefix,
            header: false,
            kind: RowKind::Source(SourceLayout {
                prefix_bytes,
                prefix_width,
                code,
                continued: false,
            }),
        }
    }
    pub fn decoration(prefix: Line<'static>, code: CodeGeometry) -> Self {
        Self {
            prefix,
            header: false,
            kind: RowKind::CodeDecoration(code),
        }
    }
    pub fn spacer() -> Self {
        Self {
            prefix: Line::default(),
            header: false,
            kind: RowKind::Spacer,
        }
    }
    pub fn with_flow(mut self, header: bool, continued: bool) -> Self {
        self.header = header && !self.is_spacer();
        if let RowKind::Source(source) = &mut self.kind {
            source.continued = continued;
        }
        self
    }
    pub fn participates_in_source(&self) -> bool {
        matches!(self.kind, RowKind::Source(_))
    }
    pub fn is_spacer(&self) -> bool {
        matches!(self.kind, RowKind::Spacer)
    }
    pub fn decorative(&self) -> bool {
        matches!(self.kind, RowKind::CodeDecoration(_))
    }
    pub fn header(&self) -> bool {
        self.header
    }
    pub fn continued(&self) -> bool {
        matches!(&self.kind, RowKind::Source(source) if source.continued)
    }
    /// Source prefix as `(bytes, display width)`; zero for non-source rows.
    pub fn source_prefix(&self) -> (usize, usize) {
        match &self.kind {
            RowKind::Source(source) => (source.prefix_bytes, source.prefix_width),
            _ => (0, 0),
        }
    }
    pub fn code(&self) -> Option<CodeGeometry> {
        match &self.kind {
            RowKind::Source(source) => source.code,
            RowKind::CodeDecoration(code) => Some(*code),
            RowKind::Spacer => None,
        }
    }
    pub fn has_geometry(&self) -> bool {
        !self.prefix.spans.is_empty() || self.source_prefix() != (0, 0) || self.code().is_some()
    }
}

/// Validated code surface: width fits the container, padding is zero or one
/// cell on BOTH sides. Narrow containers preserve source before padding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CodeGeometry {
    indent: usize,
    width: usize,
    padding: bool,
}

impl CodeGeometry {
    pub fn new(container: usize, indent: usize, longest: usize, widest: usize) -> Self {
        let indent = indent.min(container.saturating_sub(1));
        let width = longest
            .saturating_add(2)
            .min(container.saturating_sub(indent));
        let padding = width >= widest.saturating_add(2);
        Self {
            indent,
            width,
            padding,
        }
    }
    pub fn indent(self) -> usize {
        self.indent
    }
    pub fn width(self) -> usize {
        self.width
    }
    pub fn padding(self) -> usize {
        usize::from(self.padding)
    }
    pub fn body_width(self) -> usize {
        self.width - 2 * self.padding()
    }
    pub fn body_start(self) -> usize {
        self.indent + self.padding()
    }
    #[cfg(test)]
    pub fn body_end(self) -> usize {
        self.indent + self.width - self.padding()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct LayoutLine {
    pub line: Line<'static>,
    pub layout: RowLayout,
}

impl From<(Line<'static>, bool)> for LayoutLine {
    fn from((line, continued): (Line<'static>, bool)) -> Self {
        Self {
            line,
            layout: RowLayout::default().with_flow(false, continued),
        }
    }
}

#[cfg(test)]
mod geometry_tests {
    use super::*;
    #[test]
    fn code_geometry_validates_zero_narrow_and_extreme_containers() {
        for container in [0, 1, 2, 3, 8, 80, usize::MAX] {
            for indent in [0, 1, 7, usize::MAX] {
                for longest in [0, 1, 2, 90, usize::MAX] {
                    for widest in [0, 1, 2, usize::MAX] {
                        let code = CodeGeometry::new(container, indent, longest, widest);
                        assert!(code.indent() <= container);
                        assert!(code.width() <= container - code.indent());
                        assert!(code.padding() <= 1);
                        assert!(code.width() >= code.padding() * 2);
                        assert_eq!(code.body_end() - code.body_start(), code.body_width());
                    }
                }
            }
        }
    }
}
