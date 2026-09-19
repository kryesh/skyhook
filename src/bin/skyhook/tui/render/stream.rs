//! Full-source layout for live and saved prose. Content updates are
//! authoritative replacements: every update renders its complete source, with a
//! stateless plain-prose fast path and a conservative Markdown fallback.

use super::markdown::{self, LayoutLine};
use super::{Palette, wrap_words};
use ratatui::{
    style::Style,
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

pub(super) fn layout_highlighted(
    text: &str,
    width: usize,
    p: Palette,
    prefix: &str,
    highlights: Option<&super::super::tool_view::HighlightCache>,
) -> Vec<LayoutLine> {
    let width = width.max(1);
    if let Some(rows) = plain_layout(text, width, prefix, p) {
        return rows;
    }
    let cleaned = super::model::clean(text);
    markdown::layout_highlighted(
        &cleaned,
        p,
        true,
        width,
        width.saturating_sub(prefix.width()),
        prefix,
        highlights,
    )
}

/// Render a complete plain source once, or decline so Markdown handles it.
fn plain_layout(text: &str, width: usize, prefix: &str, p: Palette) -> Option<Vec<LayoutLine>> {
    // Conservative narrow-prefix admission, including multibyte prefixes.
    if !prefix.is_empty() && width < prefix.len() {
        return None;
    }
    let mut output = Vec::new();
    let mut pending_blank = false;
    for body in text.split('\n') {
        if body
            .char_indices()
            .any(|(offset, ch)| !safe_char(ch, offset == 0))
        {
            return None;
        }
        let visible = body.trim_end_matches(' ');
        if visible.is_empty() {
            pending_blank |= !output.is_empty();
            continue;
        }
        if pending_blank {
            output.push((Line::default(), false).into());
            pending_blank = false;
        }
        let mut spans = Vec::new();
        if output.is_empty() && !prefix.is_empty() {
            spans.push(Span::raw(prefix.to_owned()));
        }
        spans.push(Span::styled(
            visible.to_owned(),
            Style::default().fg(p.content.fg),
        ));
        output.extend(
            wrap_words(Line::from(spans), width)
                .into_iter()
                .enumerate()
                .map(|(part, line)| (line, part > 0).into()),
        );
    }
    if output.is_empty() {
        output.push((Line::from(prefix.to_owned()), false).into());
    }
    Some(output)
}

// Look through containers and empty blocks. A list item already emits a marker
// before its table, whereas a block quote emits no row until content arrives.
pub(super) fn starts_with_table(text: &str) -> bool {
    use pulldown_cmark::{Event, Parser, Tag};
    for event in Parser::new_ext(text, super::markdown::options()) {
        match event {
            Event::Start(Tag::Table(_)) => return true,
            Event::Start(Tag::BlockQuote(_) | Tag::Paragraph | Tag::Heading { .. })
            | Event::End(_) => {}
            _ => return false,
        }
    }
    false
}

fn safe_char(ch: char, line_start: bool) -> bool {
    // Keep the same conservative plain admission as the former replacement
    // fast path. Combining/format sequences go through full Markdown layout.
    if unicode_width::UnicodeWidthChar::width(ch) == Some(0) {
        return false;
    }
    if ch.is_control()
        || matches!(
            ch,
            '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '&' | '~' | '#' | '|'
        )
    {
        return false;
    }
    if line_start && (ch == ' ' || ch.is_ascii_digit() || matches!(ch, '-' | '+' | '=')) {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(text: &str, width: usize, prefix: &str) -> Vec<LayoutLine> {
        layout_highlighted(text, width, Palette::new(), prefix, None)
    }

    #[test]
    fn replacement_code_geometry_partial_input_and_resize() {
        let input = "prose `inline`\n\n```rust\nlet answer = 42;  \n\n    \n// long comment with words for wrapping\n```\n\nafter";
        let mut text = String::new();
        for width in [7, 29, 3] {
            text.clear();
            let mut rows = Vec::new();
            for ch in input.chars() {
                text.push(ch);
                rows = render(&text, width, "");
            }
            assert_eq!(rows.iter().filter(|row| row.layout.decorative()).count(), 2);
            let mut copied = String::new();
            let mut first = true;
            for row in rows.iter().filter(|row| !row.layout.decorative()) {
                if !first && !row.layout.continued() {
                    copied.push('\n');
                }
                copied.push_str(&row.line.to_string());
                first = false;
            }
            assert_eq!(
                copied,
                "prose inline\n\nlet answer = 42;  \n\n    \n// long comment with words for wrapping\n\nafter"
            );
        }
    }

    const NARROW_TABLE: &str = "| Name | Description |\n| :--- | ---: |\n| 界 | **long words** and `code` |\n| e\u{301} | abcdefghijklmnopqrstuvwxyz |";

    /// Screen columns of every table border character in `text`.
    fn border_columns(text: &str) -> Vec<usize> {
        let borders = text
            .char_indices()
            .filter(|(_, ch)| "│├┼┤┌┬┐└┴┘".contains(*ch));
        borders.map(|(at, _)| text[..at].width()).collect()
    }

    #[test]
    fn narrow_tables_keep_borders_on_single_rows_through_resize_and_growth() {
        let quoted: String = NARROW_TABLE
            .lines()
            .map(|line| format!("> > {line}\n"))
            .collect();
        let sources = [
            NARROW_TABLE.to_owned(),
            quoted,
            format!("```\n```\n\n{NARROW_TABLE}"),
        ];
        for source in sources {
            for prefix in ["", "  ", "↳ ", "界 "] {
                for width in [18, 23, 28] {
                    let rows = render(&source, width, prefix);
                    assert!(rows.len() > 4, "cells should wrap inside the table");
                    let start = rows
                        .iter()
                        .position(|row| row.line.to_string().contains('┌'));
                    let table_rows = &rows[start.unwrap()..];
                    let separator = table_rows
                        .iter()
                        .find(|row| row.line.to_string().contains('├'));
                    // Measure actual screen columns, including quote containers;
                    // stripping the spinner would hide first-row misalignment.
                    let columns = border_columns(&separator.unwrap().line.to_string());
                    let mut separators = 0;
                    for row in table_rows {
                        let text = row.line.to_string();
                        assert!(row.line.width() <= width, "{text:?}, width {width}");
                        let continued = row.layout.continued();
                        assert!(!continued, "downstream wrapper split a table row: {text:?}");
                        let actual = border_columns(&text);
                        assert_eq!(actual, columns, "misaligned physical borders: {text:?}");
                        let (end, joint) = if text.contains('├') {
                            separators += 1;
                            ('┤', Some('┼'))
                        } else if text.contains('┌') {
                            ('┐', Some('┬'))
                        } else if text.contains('└') {
                            ('┘', Some('┴'))
                        } else {
                            assert!(text.starts_with('│'), "{text:?}");
                            ('│', None)
                        };
                        assert!(text.ends_with(end), "{text:?}");
                        assert!(joint.is_none_or(|joint| text.matches(joint).count() == 1));
                    }
                    assert_eq!(separators, 2);
                }
            }
        }
        // Replacement rendering follows resizes of a growing table.
        for prefix in ["", "  ", "界 "] {
            let mut source = format!("{NARROW_TABLE}\n\nplain tail");
            let wide_rows = render(&source, 60, prefix).len();
            for width in [17, 8, 3, 35, 60] {
                let rows = render(&source, width, prefix);
                assert!(width != 17 || rows.len() > wide_rows);
                source.push_str(" more");
            }
        }
    }

    /// Plain prose skips Markdown parsing, so it must lay out exactly as the
    /// Markdown render would, including every prefix that could become Markdown.
    #[test]
    fn plain_shortcut_matches_markdown_for_every_prefix() {
        let p = Palette::new();
        let canonical = |rows: Vec<LayoutLine>| {
            rows.into_iter()
                .map(|row| {
                    let mut spans: Vec<(String, Style)> = Vec::new();
                    for span in row.line.spans {
                        match spans.last_mut() {
                            _ if span.content.is_empty() => {}
                            Some((text, style)) if *style == span.style => {
                                text.push_str(&span.content);
                            }
                            _ => spans.push((span.content.into_owned(), span.style)),
                        }
                    }
                    (spans, row.line.style, row.layout)
                })
                .collect::<Vec<_>>()
        };
        let alphabet = [
            "a", " ", "\n", "\n\n", "-", "+", "1", ".", "*", "_", "~", "#", "=", "|", "`", ":",
            "  ", "中",
        ];
        let mut seed = 71u64;
        let random = (0..12).map(|_| {
            (0..30)
                .map(|_| {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                    alphabet[(seed >> 32) as usize % alphabet.len()]
                })
                .collect::<String>()
        });
        let sources = [
            "A quick brown fox jumps over the lazy dog, with   repeated spaces.",
            "Hello abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyz end.",
            "Wide 中文文字没有空格 and e\u{301}e\u{301} with emoji 👩\u{200d}💻❤️ end.",
            "\n\nfirst\nsoft break  \nhard break\n\n\nsecond\n\n",
            "# title\n\nplain\n\nnext\n\n## later\n\nlast",
            "- first\n- second\n\n1. first\n2. second\n\n- [x] done",
            "> quoted\n\nsetext\n===\n\nplain\n---\n\nend",
        ]
        .into_iter()
        .map(str::to_owned)
        .chain(random);
        for source in sources {
            let ends = source.char_indices().map(|(at, _)| at).skip(1);
            for end in ends.chain([source.len()]) {
                let text = &source[..end];
                for width in [1, 3, 13, 80] {
                    let cleaned = super::super::model::clean(text);
                    let reference =
                        markdown::layout_highlighted(&cleaned, p, true, width, width, "", None);
                    assert_eq!(
                        canonical(render(text, width, "")),
                        canonical(reference),
                        "{text:?} at width {width}"
                    );
                }
            }
        }
    }
}
