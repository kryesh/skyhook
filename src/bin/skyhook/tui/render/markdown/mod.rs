//! Markdown shared by saved responses, reasoning, and live streaming fragments.
//! Soft breaks intentionally remain newlines in chat. Block boundaries add one
//! blank row, while tight list items and code-block lines remain consecutive.

mod layout;
mod parser;
mod sections;
mod tables;

use super::super::tool_view::Document;
use ratatui::text::Line;
use std::ops::Range;

pub(super) use layout::layout_highlighted;
#[cfg(test)]
pub(super) use parser::tests::{render, render_highlighted};
pub(super) use sections::title;
pub(in crate::tui) use sections::{Section, sections};

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
    source: RowSource,
}

/// The source a row's text shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum RowSource {
    /// All of the row's text, from `start` in logical line `line`.
    Line { line: LineId, start: usize },
    /// A table row: fragments of its cells. Each cell is a logical line of
    /// its own, so its identity survives other cells and rows rewrapping.
    /// Borders and padding show no source.
    Cells(Vec<SourceSpan>),
}

impl Default for RowSource {
    fn default() -> Self {
        Self::Line {
            line: LineId::default(),
            start: 0,
        }
    }
}

impl RowSource {
    /// The source of `bytes` of a line's `text`, as a row of their own. Every
    /// span keeps all its source: one cut by a row boundary splits there,
    /// mapped through what it shows, and one showing nothing stays where it is.
    pub fn slice(&self, bytes: Range<usize>, text: &str) -> Self {
        match self {
            Self::Line { line, start } => Self::Line {
                line: *line,
                start: start + bytes.start,
            },
            Self::Cells(cells) => Self::Cells(
                (cells.iter())
                    .filter_map(|cell| {
                        let shown =
                            cell.shown.start.max(bytes.start)..cell.shown.end.min(bytes.end);
                        let at = cell.shown.start;
                        let here = if cell.shown.is_empty() {
                            bytes.contains(&at) || at == bytes.end && at == text.len()
                        } else {
                            !shown.is_empty()
                        };
                        if !here {
                            return None;
                        }
                        let source = |byte, edge, whole| {
                            if byte == edge {
                                whole
                            } else {
                                cell.to_source(text, byte)
                            }
                        };
                        Some(SourceSpan {
                            source: source(shown.start, cell.shown.start, cell.source.start)
                                ..source(shown.end, cell.shown.end, cell.source.end),
                            shown: shown.start - bytes.start..shown.end - bytes.start,
                            ..cell.clone()
                        })
                    })
                    .collect(),
            ),
        }
    }
}

/// The one interpretation of what a row shows of its source: bytes `shown`
/// of the row's text show bytes `source` of logical line `line`. Endpoints,
/// copying and highlighting all read text and positions through it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SourceSpan {
    pub line: LineId,
    pub source: Range<usize>,
    pub shown: Range<usize>,
    /// The line's own text, where the row shows it changed: a table cell's
    /// trimmed separators and replaced graphemes. Otherwise the shown bytes
    /// are the source.
    pub text: Option<std::sync::Arc<str>>,
}

impl SourceSpan {
    /// The source this span shows, from the row's text or the line's own.
    pub fn source_text<'a>(&'a self, row: &'a str) -> &'a str {
        match &self.text {
            Some(text) => &text[self.source.clone()],
            None => &row[self.shown.clone()],
        }
    }
    /// Shown and source grapheme boundaries, pairwise from the span's start:
    /// each shown grapheme stands for one source grapheme.
    fn boundaries<'a>(&'a self, row: &'a str) -> impl Iterator<Item = (usize, usize)> + 'a {
        use unicode_segmentation::UnicodeSegmentation;
        let ends = |text: &'a str| {
            let starts = text.grapheme_indices(true).map(|(at, _)| at);
            starts.chain([text.len()])
        };
        ends(&row[self.shown.clone()]).zip(ends(self.source_text(row)))
    }
    /// The source byte at shown byte `byte` of the row.
    pub fn to_source(&self, row: &str, byte: usize) -> usize {
        let shown = byte - self.shown.start;
        let source = match self.text {
            None => shown,
            Some(_) => (self.boundaries(row).find(|&(at, _)| at >= shown)).map_or(0, |(_, at)| at),
        };
        self.source.start + source
    }
    /// The shown byte of the row at source byte `byte`; source the row does
    /// not show (trimmed separators) maps to the end of what it does.
    pub fn to_shown(&self, row: &str, byte: usize) -> usize {
        let source = byte - self.source.start;
        let shown = match self.text {
            None => source,
            Some(_) => (self.boundaries(row).take_while(|&(_, at)| at <= source))
                .last()
                .map_or(0, |(at, _)| at),
        };
        self.shown.start + shown
    }
}

/// The part of an entry a logical line comes from. Each part numbers its
/// lines independently, so one part's lines never renumber another's.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(super) enum Segment {
    /// A header row's line. Its text is the entry's state, such as a tool's
    /// status, so it takes no part in the identities of the lines below it.
    #[default]
    Header,
    /// Document and plain lines after a header, each identified by all the
    /// text before it: a line inserted above gives every line below it a new
    /// identity rather than another line's.
    Lines,
    /// A Markdown body, numbered by source position.
    Markdown,
    Footer,
}

/// A logical line's identity within its entry. Its producer derives it from
/// the source, never from the width, so reflow keeps it: a source position
/// and the line's ordinal among lines from that position. Rows with the same
/// identity are one logical line, wrapped. It is identity only: an origin
/// can come from an event that closed a container earlier in the source, so
/// document order is the layout's, never derived from identities.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(super) struct LineId {
    segment: Segment,
    origin: usize,
    part: usize,
}

impl LineId {
    /// The single line at `origin`.
    pub fn at(origin: usize) -> Self {
        Self {
            origin,
            ..Self::default()
        }
    }

    /// A table cell, until layout numbers its line: the cell at `origin` in
    /// column `column`. Cells padded onto a short row share their origin.
    pub fn cell(origin: usize, column: usize) -> Self {
        Self {
            origin,
            part: column,
            ..Self::default()
        }
    }
}

/// Numbers logical lines by origin: the first line from an origin is its
/// part 0, the next part 1, and so on.
#[derive(Default)]
pub(super) struct LineIds(std::collections::HashMap<usize, usize>);

impl LineIds {
    pub fn next(&mut self, origin: usize) -> LineId {
        let part = self.0.entry(origin).or_default();
        let id = LineId {
            origin,
            part: *part,
            ..LineId::default()
        };
        *part += 1;
        id
    }
}

/// Identifies the lines after a header, in order, by a hash of all the text
/// before each.
#[derive(Default)]
pub(super) struct TextIds(std::hash::DefaultHasher);

impl TextIds {
    pub fn next(&mut self, line: &Line<'_>) -> LineId {
        use std::hash::Hasher;
        let id = LineId {
            segment: Segment::Lines,
            origin: self.0.finish() as usize,
            part: 0,
        };
        for span in &line.spans {
            self.0.write(span.content.as_bytes());
        }
        self.0.write_u8(b'\n');
        id
    }
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
                ..SourceLayout::default()
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
    pub fn with_header(mut self, header: bool) -> Self {
        self.header = header && !self.is_spacer();
        self
    }
    pub fn with_source(mut self, row: RowSource) -> Self {
        if let RowKind::Source(source) = &mut self.kind {
            source.source = row;
        }
        self
    }
    pub fn in_segment(mut self, segment: Segment) -> Self {
        if let RowKind::Source(source) = &mut self.kind {
            match &mut source.source {
                RowSource::Line { line, .. } => line.segment = segment,
                RowSource::Cells(cells) => {
                    cells
                        .iter_mut()
                        .for_each(|cell| cell.line.segment = segment);
                }
            }
        }
        self
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
    /// The source a row with `len` bytes of text shows, in order.
    pub fn sources(&self, len: usize) -> impl Iterator<Item = std::borrow::Cow<'_, SourceSpan>> {
        let (line, cells) = match &self.kind {
            RowKind::Source(SourceLayout { source, .. }) => match source {
                RowSource::Line { line, start } => (Some((*line, *start)), &[][..]),
                RowSource::Cells(cells) => (None, &cells[..]),
            },
            _ => (None, &[][..]),
        };
        let line = line.map(|(line, start)| SourceSpan {
            line,
            source: start..start + len,
            shown: 0..len,
            text: None,
        });
        let cells = cells.iter().map(std::borrow::Cow::Borrowed);
        line.map(std::borrow::Cow::Owned).into_iter().chain(cells)
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
    #[cfg(test)]
    pub fn row_source(&self) -> Option<&RowSource> {
        match &self.kind {
            RowKind::Source(source) => Some(&source.source),
            _ => None,
        }
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
    pub fn body_end(self) -> usize {
        self.indent + self.width - self.padding()
    }
}

/// Laid-out rows and the fences whose highlighting they show.
#[derive(Default)]
pub(super) struct Layout {
    pub lines: Vec<LayoutLine>,
    pub fences: Document,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct LayoutLine {
    pub line: Line<'static>,
    pub layout: RowLayout,
}

impl LayoutLine {
    /// The rows of logical line `id`, each placed after the text before it.
    pub fn wrapped(rows: Vec<Line<'static>>, id: LineId) -> impl Iterator<Item = Self> {
        rows.into_iter().scan(0, move |start, line| {
            let layout = RowLayout::default().with_source(RowSource::Line {
                line: id,
                start: *start,
            });
            *start += text_len(&line);
            Some(Self { line, layout })
        })
    }
}

/// The bytes of a line's text, without assembling it.
pub(super) fn text_len(line: &Line<'_>) -> usize {
    line.spans.iter().map(|span| span.content.len()).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Slicing a line into rows keeps all of every span's source: split at
    /// row boundaries, mapped through what each row shows.
    #[test]
    fn slicing_preserves_source_coverage() {
        // A cell showing "ab�d" for its source "ab😀d  ", then an empty cell.
        let text = "│ ab�d │";
        let cell = |shown: Range<usize>, source: Range<usize>, origin| SourceSpan {
            line: LineId::at(origin),
            source,
            shown,
            text: Some(std::sync::Arc::from("ab😀d  ")),
        };
        let source = RowSource::Cells(vec![
            cell(4..10, 0..9, 1),
            cell(text.len()..text.len(), 0..0, 2),
        ]);
        let rows = [0..6, 6..text.len()];
        let mut covered = Vec::new();
        for row in rows {
            let RowSource::Cells(cells) = source.slice(row.clone(), text) else {
                panic!("cells slice into cells");
            };
            for cell in cells {
                assert!(cell.shown.end <= row.len(), "{cell:?} in {row:?}");
                covered.push((cell.line.origin, cell.source));
            }
        }
        assert_eq!(covered, [(1, 0..2), (1, 2..9), (2, 0..0)]);
    }

    #[test]
    fn code_geometry_validates_zero_narrow_and_extreme_containers() {
        for container in [0, 1, 3, 80, usize::MAX] {
            for indent in [0, 7, usize::MAX] {
                for longest in [0, 2, 90, usize::MAX] {
                    for widest in [0, 2, usize::MAX] {
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
