//! Presentation-only tool documents. Nothing here writes to session or tool state.
mod document;
mod highlighting;
mod hints;

pub use hints::{Hints, starts_child};

use super::{format::Clean, theme::THEME};
use highlighting::CodeKey;
pub use highlighting::{CodeSource, HighlightCache};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

pub(super) const MAX_SECTION: usize = 256 * 1024;
const MAX_LINE: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Plain,
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
    pub fn color(self) -> Color {
        match self {
            Self::Plain | Self::ToolName => THEME.fg,
            Self::Indicator => THEME.primary,
            Self::Target => THEME.accent,
            Self::Success => THEME.success,
            Self::Warning => THEME.warning,
            Self::Error => THEME.error,
            Self::Heading => THEME.heading,
            Self::Label => THEME.secondary,
            Self::String | Self::Added => THEME.success,
            Self::Number => THEME.accent,
            Self::Constant => THEME.primary,
            Self::Muted => THEME.muted,
            Self::Removed => THEME.error,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Section {
    /// Hard-wrapped runs. `indent` is presentation, never part of the text.
    Line { indent: usize, runs: Vec<Run> },
    /// Prose arguments retain their literal text, but wrap at word boundaries.
    Prose { indent: usize, runs: Vec<Run> },
    Code {
        source: CodeSource,
        language: String,
        indent: usize,
        /// No gutter or one shared marker, never a marker per source line.
        gutters: Option<Clean>,
        role: Role,
    },
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
impl DocumentLine {
    pub fn new(text: Line<'static>, wrap: Wrap) -> Self {
        Self {
            lead: Line::default(),
            text,
            wrap,
        }
    }
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
                let line = |indent: &usize, runs, wrap| {
                    Box::new(std::iter::once(Logical {
                        indent: *indent,
                        marker: None,
                        piece: Piece::Runs { runs, wrap },
                    }))
                };
                match section {
                    Section::Line { indent, runs } => line(indent, runs, Wrap::Hard),
                    Section::Prose { indent, runs } => line(indent, runs, Wrap::Words),
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

    /// The document as displayed, leads included: a whole entry's plain text.
    /// Selection copies the same lines without their leads.
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
            }
        }
        text
    }
    /// Each logical line as painted, its lead included.
    pub fn lines(&self, cache: Option<&HighlightCache>) -> Vec<Line<'static>> {
        self.layout_lines(cache)
            .into_iter()
            .map(|DocumentLine { mut lead, text, .. }| {
                lead.spans.extend(text.spans);
                lead
            })
            .collect()
    }
    pub fn layout_lines(&self, cache: Option<&HighlightCache>) -> Vec<DocumentLine> {
        // A code section's highlighting, looked up once for all its lines.
        let mut highlighted: Option<(&CodeSource, Option<&[Line<'static>]>)> = None;
        let lines = self.logical_lines().map(|line| match line.piece {
            Piece::Runs { runs, wrap } => DocumentLine {
                lead: lead(line.indent, None),
                text: header_line(runs),
                wrap,
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
                DocumentLine {
                    lead: lead(line.indent, marker),
                    text,
                    wrap: Wrap::Hard,
                }
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
        document.sections.push(Section::Line {
            indent: 0,
            runs: vec![],
        });
        document.sections.push(Section::Line {
            indent: 2,
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
