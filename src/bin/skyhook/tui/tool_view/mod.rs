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
    Line(Vec<Run>),
    /// Prose arguments retain their literal text, but wrap at word boundaries.
    Prose(Vec<Run>),
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
                        text.push_str(&run.text);
                    }
                }
                Section::Code {
                    source,
                    indent,
                    gutters,
                    ..
                } => {
                    for line in source.split('\n') {
                        if !first {
                            text.push('\n');
                        }
                        first = false;
                        text.extend(std::iter::repeat_n(' ', *indent));
                        if let Some(gutter) = gutters.as_deref() {
                            text.push_str(gutter);
                        }
                        text.push_str(line);
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
                    let key = CodeKey::admit(source, language);
                    let highlighted = key
                        .as_ref()
                        .and_then(|key| cache.and_then(|cache| cache.ready(key)))
                        .and_then(Option::as_deref);
                    // Split without trimming: empty lines and trailing whitespace are significant.
                    for (index, text) in source.split('\n').enumerate() {
                        let mut spans = vec![Span::raw(" ".repeat(*indent))];
                        if let Some(gutter) = gutters.as_deref() {
                            spans.push(Span::styled(
                                gutter.to_owned(),
                                if matches!(role, Role::Added | Role::Removed) {
                                    role.style()
                                } else {
                                    Role::Muted.style()
                                },
                            ));
                        }
                        if let Some(line) = highlighted.and_then(|lines| lines.get(index)) {
                            spans.extend(line.spans.iter().cloned());
                        } else {
                            spans.push(Span::styled(text.to_owned(), role.style()));
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
        document.sections.push(Section::Line(vec![]));
        document.sections.push(Section::Line(vec![
            Run::new("a\tb", Role::String),
            Run::new("\u{7}界", Role::Number),
        ]));
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
