//! Presentation-only tool documents.
mod document;
mod highlighting;
mod hints;

pub use hints::{Hints, starts_child};

use super::{format::Clean, theme::theme};
pub use highlighting::{CodeKey, CodeSource, HighlightCache};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

pub(super) const MAX_SECTION: usize = 256 * 1024;
const MAX_LINE: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Plain,
    /// What a header says of its call's arguments: the part that gives way when the
    /// header is wider than its row.
    Detail,
    ToolName,
    Indicator,
    Target,
    Success,
    Warning,
    Error,
    Heading,
    Label,
    String,
    Number,
    Constant,
    Muted,
    Removed,
    Added,
}
impl Role {
    /// Data roles follow the code palette so tool output matches the code beside it.
    pub fn color(self) -> Color {
        let theme = theme();
        match self {
            Self::Plain | Self::Detail | Self::ToolName => theme.ui.fg,
            Self::Indicator | Self::Heading => theme.ui.emphasis,
            Self::Target => theme.ui.accent,
            Self::Success => theme.ui.success,
            Self::Warning => theme.ui.warning,
            Self::Error | Self::Removed => theme.ui.error,
            Self::Muted => theme.ui.muted,
            Self::Label => theme.syntax.keyword.into(),
            Self::String | Self::Added => theme.syntax.string.into(),
            Self::Number => theme.syntax.number.into(),
            Self::Constant => theme.syntax.literal.into(),
        }
    }

    fn style(self) -> Style {
        let style = Style::default().fg(self.color());
        if matches!(self, Self::Heading | Self::Label | Self::ToolName) {
            style.add_modifier(Modifier::BOLD)
        } else {
            style
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    text: Clean,
    role: Role,
}
impl Run {
    pub(super) fn text(&self) -> &str {
        &self.text
    }

    pub(super) fn new(text: impl Into<Clean>, role: Role) -> Self {
        Self {
            text: text.into(),
            role,
        }
    }
}
/// Render structured presentation metadata without inferring semantics from its text.
pub fn header_line(runs: &[Run]) -> Line<'static> {
    Line::from(
        runs.iter()
            .map(|run| Span::styled(run.text.to_string(), run.role.style()))
            .collect::<Vec<_>>(),
    )
}

/// A header's runs as a line of `width` cells where cutting its detail makes it one:
/// what follows the detail, how the call went and which job it was, stays whole.
pub fn fitted_header(runs: &[Run], width: usize) -> Line<'static> {
    let mut line = header_line(runs);
    let over = line.width().saturating_sub(width);
    let detail = runs.iter().position(|run| run.role == Role::Detail);
    if let Some(detail) = detail.filter(|_| over > 0) {
        let span = std::mem::take(&mut line.spans[detail]);
        let room = span.width().saturating_sub(over);
        let cut = crate::tui::wrapping::clipped(Line::from(span), room);
        line.spans.splice(detail..=detail, cut.spans);
    }
    line
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Section {
    /// A line of runs. `indent` is presentation, never part of the text; prose
    /// keeps its literal text but wraps at words.
    Text {
        indent: usize,
        runs: Vec<Run>,
        wrap: Wrap,
    },
    Code {
        source: CodeSource,
        language: String,
        indent: usize,
        /// No gutter or one shared marker, never a marker per source line.
        gutters: Option<Clean>,
        role: Role,
    },
    /// Model-written prose, laid out as Markdown `indent` cells in.
    Markdown { source: Clean, indent: usize },
}
/// Layout intent travels with each logical line; it is never inferred from text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wrap {
    Hard,
    Words,
}

/// A styled logical line and its presentation-only layout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentLine {
    /// Decoration painted before the text, such as a code block's indent and
    /// change marker: shown, but never selected or copied with the text.
    pub lead: Line<'static>,
    pub text: Line<'static>,
    pub wrap: Wrap,
}

/// What a document lays out: a logical line, or Markdown that layout wraps
/// at the width it is given.
pub enum DocumentBlock<'a> {
    Line(DocumentLine),
    Markdown { source: &'a str, indent: usize },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Document {
    pub sections: Vec<Section>,
}
/// The text of a logical line, before styling.
enum Piece<'a> {
    Runs {
        runs: &'a [Run],
        wrap: Wrap,
    },
    /// Line `index` of a code section's `source`.
    Code {
        text: &'a str,
        index: usize,
        source: &'a CodeSource,
        language: &'a str,
        role: Role,
    },
    /// A whole Markdown section, whose lines layout decides.
    Markdown(&'a str),
}

/// A logical line: its lead, as `indent` cells and any marker, then its text.
/// Display styles these and plain text reads them as they are, so the two
/// always show the same lines with the same leads.
struct Logical<'a> {
    indent: usize,
    marker: Option<&'a str>,
    piece: Piece<'a>,
}

impl Document {
    /// Every logical line, in order.
    fn logical_lines(&self) -> impl Iterator<Item = Logical<'_>> {
        self.sections
            .iter()
            .flat_map(|section| -> Box<dyn Iterator<Item = Logical<'_>> + '_> {
                match section {
                    Section::Text { indent, runs, wrap } => Box::new(std::iter::once(Logical {
                        indent: *indent,
                        marker: None,
                        piece: Piece::Runs { runs, wrap: *wrap },
                    })),
                    Section::Markdown { source, indent } => Box::new(std::iter::once(Logical {
                        indent: *indent,
                        marker: None,
                        piece: Piece::Markdown(source),
                    })),
                    Section::Code {
                        source,
                        language,
                        indent,
                        gutters,
                        role,
                    } => {
                        // Split without trimming: empty lines and trailing whitespace are
                        // significant.
                        Box::new(
                            source
                                .split('\n')
                                .enumerate()
                                .map(move |(index, text)| Logical {
                                    indent: *indent,
                                    marker: gutters.as_deref(),
                                    piece: Piece::Code {
                                        text,
                                        index,
                                        source,
                                        language,
                                        role: *role,
                                    },
                                }),
                        )
                    }
                }
            })
    }

    /// The document's text, leads included and Markdown as its source: a whole
    /// entry's plain text. Selection copies the displayed lines without their leads.
    pub fn plain_text(&self) -> String {
        let mut text = String::new();
        for (index, line) in self.logical_lines().enumerate() {
            if index > 0 {
                text.push('\n');
            }
            text.extend(std::iter::repeat_n(' ', line.indent));
            text.push_str(line.marker.unwrap_or_default());
            match line.piece {
                Piece::Runs { runs, .. } => text.extend(runs.iter().map(Run::text)),
                Piece::Code { text: code, .. } => text.push_str(code),
                Piece::Markdown(source) => {
                    let lead = format!("\n{}", " ".repeat(line.indent));
                    text.push_str(&source.replace('\n', &lead));
                }
            }
        }
        text
    }
    /// Each logical line as painted, its lead included; Markdown as its source.
    pub fn lines(&self, cache: Option<&HighlightCache>) -> Vec<Line<'static>> {
        let blocks = self.layout_lines(cache).into_iter();
        blocks
            .flat_map(|block| match block {
                DocumentBlock::Line(DocumentLine { mut lead, text, .. }) => {
                    lead.spans.extend(text.spans);
                    vec![lead]
                }
                DocumentBlock::Markdown { source, indent } => (source.split('\n'))
                    .map(|line| Line::from(format!("{}{line}", " ".repeat(indent))))
                    .collect(),
            })
            .collect()
    }
    pub fn layout_lines(&self, cache: Option<&HighlightCache>) -> Vec<DocumentBlock<'_>> {
        // A code section's highlighting, looked up once for all its lines.
        let mut highlighted: Option<(&CodeSource, Option<&[Line<'static>]>)> = None;
        let lines = self.logical_lines().map(|line| match line.piece {
            Piece::Runs { runs, wrap } => DocumentBlock::Line(DocumentLine {
                lead: lead(line.indent, None),
                text: header_line(runs),
                wrap,
            }),
            Piece::Markdown(source) => DocumentBlock::Markdown {
                source,
                indent: line.indent,
            },
            Piece::Code {
                text,
                index,
                source,
                language,
                role,
            } => {
                if !highlighted.is_some_and(|(section, _)| std::ptr::eq(section, source)) {
                    let lines = cache
                        .zip(CodeKey::admit(source, language))
                        .and_then(|(cache, key)| cache.ready(&key))
                        .and_then(Option::as_deref);
                    highlighted = Some((source, lines));
                }
                let gutter_style = if matches!(role, Role::Added | Role::Removed) {
                    role.style()
                } else {
                    Role::Muted.style()
                };
                let marker = line
                    .marker
                    .map(|marker| Span::styled(marker.to_owned(), gutter_style));
                let styled = highlighted.and_then(|(_, lines)| lines?.get(index));
                let text = match styled {
                    Some(styled) => Line::from(styled.spans.clone()),
                    None => Line::from(Span::styled(text.to_owned(), role.style())),
                };
                DocumentBlock::Line(DocumentLine {
                    lead: lead(line.indent, marker),
                    text,
                    wrap: Wrap::Hard,
                })
            }
        });
        lines.collect()
    }
}

/// A section's presentation before its text: `indent` cells, then any marker.
fn lead(indent: usize, marker: Option<Span<'static>>) -> Line<'static> {
    let indent = (indent > 0).then(|| Span::raw(" ".repeat(indent)));
    Line::from(indent.into_iter().chain(marker).collect::<Vec<_>>())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::Value;
    use skyhook::job::JobView;

    pub(crate) fn view(fields: Value) -> JobView {
        serde_json::from_value(fields).unwrap()
    }

    pub(super) fn text(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn plain_text_matches_styled_output_without_losing_empty_lines_or_controls() {
        let mut document = Document::default();
        assert_eq!(document.plain_text(), "");
        document.line("\nheading\t\u{1b}\r", Role::Heading);
        document.sections.push(Section::Text {
            indent: 0,
            runs: vec![],
            wrap: Wrap::Hard,
        });
        document.sections.push(Section::Text {
            indent: 2,
            wrap: Wrap::Hard,
            runs: vec![
                Run::new("a\tb", Role::String),
                Run::new("\u{7}界", Role::Number),
            ],
        });
        document.code(
            "a  \n\t界\r\n",
            "js",
            2,
            Some("\u{1b}− ".into()),
            Role::Added,
        );
        document.code("x\n", "", 2, None, Role::Plain);
        document.code("", "", 0, None, Role::Plain);
        assert_eq!(document.plain_text(), text(&document.lines(None)));
        assert!(document.plain_text().ends_with("\n  \n"));
    }
}
