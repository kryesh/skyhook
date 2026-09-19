//! Shared foreground-only grammar and theme service for tools and code fences.
use super::super::{ContentTheme, model};
use super::{MAX_LINE, MAX_SECTION};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use std::sync::OnceLock;
use syntect::{
    easy::HighlightLines,
    highlighting::{FontStyle, Theme},
    parsing::SyntaxSet,
    util::LinesWithEndings,
};

pub(super) struct SyntaxResources {
    syntaxes: SyntaxSet,
    theme: Theme,
}
pub(super) fn syntax_resources() -> &'static SyntaxResources {
    static RESOURCES: OnceLock<SyntaxResources> = OnceLock::new();
    RESOURCES.get_or_init(|| SyntaxResources {
        syntaxes: SyntaxSet::load_defaults_newlines(),
        theme: ContentTheme::new().syntax_theme(),
    })
}

/// Shared, foreground-only syntax service for code fences and tools.
/// Returns None for unknown languages, oversized sources/lines, or parse failures;
/// callers should render their usual neutral fallback. Like the tool worker, run
/// this off the UI thread: size limits bound input, not regex execution time.
/// Grammars and the theme are initialized once and shared across workers.
pub fn highlight_code(source: &str, language: &str) -> Option<Vec<Line<'static>>> {
    if source.len() > MAX_SECTION
        || source.split('\n').any(|line| line.len() > MAX_LINE)
        || language.is_empty()
    {
        return None;
    }
    let resources = syntax_resources();
    let syntaxes = &resources.syntaxes;
    let syntax = syntaxes
        .find_syntax_by_extension(language)
        .or_else(|| syntaxes.find_syntax_by_token(language))?;
    let theme = &resources.theme;
    let mut highlighter = HighlightLines::new(syntax, theme);
    let mut lines = Vec::new();
    for source_line in LinesWithEndings::from(source) {
        let Ok(tokens) = highlighter.highlight_line(source_line, syntaxes) else {
            return None;
        };
        let mut spans = Vec::new();
        for (style, text) in tokens {
            let mut rendered = Style::default().fg(Color::Rgb(
                style.foreground.r,
                style.foreground.g,
                style.foreground.b,
            ));
            for (font, modifier) in [
                (FontStyle::BOLD, Modifier::BOLD),
                (FontStyle::ITALIC, Modifier::ITALIC),
                (FontStyle::UNDERLINE, Modifier::UNDERLINED),
            ] {
                if style.font_style.contains(font) {
                    rendered = rendered.add_modifier(modifier);
                }
            }
            spans.push(Span::styled(
                model::clean(text.strip_suffix('\n').unwrap_or(text)),
                rendered,
            ));
        }
        lines.push(Line::from(spans));
    }
    if source.ends_with('\n') || source.is_empty() {
        lines.push(Line::default());
    }
    Some(lines)
}

#[cfg(test)]
mod tests {
    use super::super::super::tests::text;
    use super::*;

    fn has_span(line: &Line<'_>, test: impl Fn(&str, Option<Color>) -> bool) -> bool {
        line.spans
            .iter()
            .any(|span| test(&span.content, span.style.fg))
    }

    #[test]
    fn shared_highlighter_maps_tokens_preserves_source_and_declines_unsupported_input() {
        let source = "let value = (true, 42, \"hello\");  \n\t// comment 界 👩‍💻\n\n";
        let theme = ContentTheme::new();
        let lines = highlight_code(source, "rust").unwrap();
        assert_eq!(text(&lines), model::clean(source));
        let spans: Vec<_> = lines.iter().flat_map(|line| &line.spans).collect();
        assert!(spans.iter().all(|span| span.style.bg.is_none()));
        for (token, color) in [
            ("let", theme.secondary),
            ("true", theme.primary),
            ("42", theme.accent),
            ("hello", theme.success),
            ("comment", theme.muted),
        ] {
            let found = spans
                .iter()
                .any(|span| span.content.contains(token) && span.style.fg == Some(color));
            assert!(found, "{token}: {spans:?}");
        }
        for (source, language) in [
            ("hello".to_owned(), "not-a-real-language"),
            ("hello".to_owned(), ""),
            ("x".repeat(MAX_LINE + 1), "rust"),
            ("x\n".repeat(MAX_SECTION / 2 + 1), "rust"),
        ] {
            assert!(highlight_code(&source, language).is_none());
        }
    }

    /// Assert source ranges, not just scope names: grammars can classify real
    /// tokens differently than a synthetic scope would suggest.
    fn assert_source_color(lines: &[Line<'_>], needle: &str, expected: Color) {
        let source = text(lines);
        let mut colors = Vec::new();
        for line in lines {
            for span in &line.spans {
                colors.extend(std::iter::repeat_n(span.style.fg, span.content.len()));
                assert_eq!(span.style.bg, None);
            }
            colors.push(None); // line separator
        }
        assert!(source.contains(needle), "missing {needle:?}");
        for (start, _) in source.match_indices(needle) {
            let colored = colors[start..start + needle.len()]
                .iter()
                .all(|color| *color == Some(expected));
            assert!(
                colored,
                "{needle:?} at {start} should be {expected:?}: {lines:?}"
            );
        }
    }

    #[test]
    fn javascript_function_names_use_standard_scopes() {
        let source = "async function declared() { const result = called(); return object.method(); }\nconst object = { async method() { return declared(); } };\nconst quoted = \"declared() called() method()\"; // declared() called() method()\n";
        let theme = ContentTheme::new();
        let lines = highlight_code(source, "javascript").unwrap();
        assert_eq!(text(&lines), source);
        for token in ["declared", "called", "method"] {
            assert_source_color(&lines[..2], token, theme.secondary);
        }
        assert_source_color(&lines[..1], "result", theme.fg);
        for (token, color) in [
            ("declared() called() method()", theme.success),
            (" declared() called() method()", theme.muted),
        ] {
            let found = has_span(&lines[2], |content, fg| {
                content == token && fg == Some(color)
            });
            assert!(found, "{token}: {:?}", lines[2]);
        }
    }
}
