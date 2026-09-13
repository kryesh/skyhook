//! Presentation-only tool documents. Nothing here writes to session or tool state.
mod document;
mod highlighting;

use super::{format::push_clean, model, theme::ContentTheme};
use highlighting::CodeKey;
#[allow(unused_imports)] // Keep the existing direct-highlighting API available.
pub use highlighting::highlight_code;
pub use highlighting::{CodeSource, HighlightCache};
use ratatui::{
    style::{Modifier, Style},
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
    fn style(self, theme: ContentTheme) -> Style {
        let color = match self {
            Self::Plain | Self::ToolName => theme.fg,
            Self::Indicator => theme.primary,
            Self::Target => theme.accent,
            Self::Success => theme.success,
            Self::Warning => theme.warning,
            Self::Error => theme.error,
            Self::Heading => theme.heading,
            Self::Label => theme.secondary,
            Self::String | Self::Added => theme.success,
            Self::Number => theme.accent,
            Self::Constant => theme.primary,
            Self::Muted => theme.muted,
            Self::Removed => theme.error,
        };
        let style = Style::default().fg(color);
        if matches!(self, Self::Heading | Self::Label | Self::ToolName) {
            style.add_modifier(Modifier::BOLD)
        } else {
            style
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    text: String,
    role: Role,
}
impl Run {
    pub(super) fn text(&self) -> &str {
        &self.text
    }

    pub(super) fn new(text: impl Into<String>, role: Role) -> Self {
        Self {
            text: text.into(),
            role,
        }
    }
}
/// Render structured presentation metadata without inferring semantics from its text.
pub fn header_line(runs: &[Run]) -> Line<'static> {
    let theme = ContentTheme::new();
    Line::from(
        runs.iter()
            .map(|run| Span::styled(model::clean(&run.text), run.role.style(theme)))
            .collect::<Vec<_>>(),
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Section {
    Line(Vec<Run>),
    /// Prose arguments retain their literal text, but wrap at word boundaries.
    Prose(Vec<Run>),
    Code {
        source: CodeSource,
        language: String,
        indent: usize,
        gutters: Vec<String>,
        role: Role,
    },
}
/// Layout intent travels with each logical line; it is never inferred from text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wrap {
    Hard,
    Words,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Document {
    pub sections: Vec<Section>,
}
impl Document {
    pub fn plain_text(&self) -> String {
        let mut text = String::new();
        let mut first = true;
        for section in &self.sections {
            match section {
                Section::Line(runs) | Section::Prose(runs) => {
                    if !first {
                        text.push('\n');
                    }
                    first = false;
                    for run in runs {
                        push_clean(&mut text, &run.text);
                    }
                }
                Section::Code {
                    source,
                    indent,
                    gutters,
                    ..
                } => {
                    for (index, line) in source.split('\n').enumerate() {
                        if !first {
                            text.push('\n');
                        }
                        first = false;
                        text.extend(std::iter::repeat_n(' ', *indent));
                        if let Some(gutter) = gutters.get(index) {
                            push_clean(&mut text, gutter);
                        }
                        push_clean(&mut text, line);
                    }
                }
            }
        }
        text
    }
    pub fn lines(&self, cache: Option<&HighlightCache>) -> Vec<Line<'static>> {
        self.layout_lines(cache)
            .into_iter()
            .map(|(line, _)| line)
            .collect()
    }
    /// Styled logical lines and their presentation-only reflow policy.
    pub fn layout_lines(&self, cache: Option<&HighlightCache>) -> Vec<(Line<'static>, Wrap)> {
        let p = ContentTheme::new();
        let mut lines = Vec::new();
        for section in &self.sections {
            match section {
                Section::Line(runs) => lines.push((header_line(runs), Wrap::Hard)),
                Section::Prose(runs) => lines.push((header_line(runs), Wrap::Words)),
                Section::Code {
                    source,
                    language,
                    indent,
                    gutters,
                    role,
                } => {
                    let highlighted = cache
                        .and_then(|cache| cache.ready(&CodeKey::new(source.clone(), language)));
                    // Split without trimming: empty lines and trailing whitespace are significant.
                    for (index, text) in source.split('\n').enumerate() {
                        let mut spans = vec![Span::raw(" ".repeat(*indent))];
                        if let Some(gutter) = gutters.get(index) {
                            spans.push(Span::styled(
                                model::clean(gutter),
                                if matches!(role, Role::Added | Role::Removed) {
                                    role.style(p)
                                } else {
                                    Role::Muted.style(p)
                                },
                            ));
                        }
                        if let Some(line) = highlighted.and_then(|lines| lines.get(index)) {
                            spans.extend(line.spans.iter().cloned().map(|mut span| {
                                // Unsupported syntax completes with raw spans. Keep that
                                // cached completion, but inherit this caller's fallback role
                                // rather than turning coloured pending text neutral.
                                if span.style.fg.is_none() {
                                    span.style = role.style(p).patch(span.style);
                                }
                                span
                            }));
                        } else {
                            spans.push(Span::styled(model::clean(text), role.style(p)));
                        }
                        lines.push((Line::from(spans), Wrap::Hard));
                    }
                }
            }
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line<'_>]) -> String {
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
        document.sections.push(Section::Line(vec![]));
        document.sections.push(Section::Line(vec![
            Run::new("a\tb", Role::String),
            Run::new("\u{7}界", Role::Number),
        ]));
        document.code(
            "a  \n\t界\r\n",
            "js",
            2,
            vec!["+\t".into(), "\u{1b}− ".into()],
            Role::Added,
        );
        document.code("", "", 0, vec![], Role::Plain);
        assert_eq!(document.plain_text(), text(&document.lines(None)));
        assert!(document.plain_text().ends_with("\n  \n"));
    }
}
