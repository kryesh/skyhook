//! Wrapping and source-versus-decoration geometry for Markdown rows.
use super::parser::{LineInfo, parse};
use super::{
    CodeGeometry, Layout, LayoutLine, LineIds, RowLayout, RowSource, SourceSpan, text_len,
};
use crate::tui::tool_view;
use crate::tui::wrapping::{cells, cells_width, truncated, wrap_line, wrap_words};
use ratatui::text::{Line, Span};

/// Wrap source inside its paragraph/container, then add geometry-only code
/// padding. Measuring the entire code block keeps all its rows the same width.
pub(crate) fn layout_highlighted(
    text: &str,
    placeholder: bool,
    width: usize,
    cache: Option<&tool_view::HighlightCache>,
) -> Layout {
    let width = width.max(1);
    let parsed = parse(text, placeholder, width, cache);
    // Table lines vary in number with the width; their cells, which do not,
    // identify their source, so only other lines take line identities.
    let mut ids = LineIds::default();
    let mut cell_ids = std::collections::HashMap::new();
    let mut input = parsed
        .lines
        .into_iter()
        .map(|mut parsed| {
            let source = match parsed.info.cells.take() {
                None => RowSource::Line {
                    line: ids.next(parsed.info.origin),
                    start: 0,
                },
                Some(cells) => RowSource::Cells(
                    (cells.into_iter())
                        .map(|cell| SourceSpan {
                            line: *(cell_ids.entry(cell.line))
                                .or_insert_with(|| ids.next(cell.line.origin)),
                            ..cell
                        })
                        .collect(),
                ),
            };
            (parsed.line, source, parsed.info)
        })
        .peekable();
    let mut output = Vec::new();
    while let Some((line, id, info)) = input.next() {
        if let Some(code) = info.code {
            let mut block = vec![(line, id, info)];
            while input
                .peek()
                .is_some_and(|(_, _, info)| info.code == Some(code))
            {
                block.push(input.next().unwrap());
            }
            let indent = block
                .iter()
                .map(|(line, _, info)| {
                    line.spans
                        .iter()
                        .take(info.prefix_spans)
                        .map(Span::width)
                        .sum::<usize>()
                        .max(info.continuation.width())
                })
                .max()
                .unwrap_or(0)
                .min(width.saturating_sub(1));
            let longest = block
                .iter()
                .map(|(line, _, info)| {
                    line.spans
                        .iter()
                        .skip(info.prefix_spans)
                        .map(|span| cells_width(&span.content))
                        .sum::<usize>()
                })
                .max()
                .unwrap_or(0);
            // Very narrow containers prioritize a source grapheme over padding.
            let widest = block
                .iter()
                .flat_map(|(line, _, info)| line.spans.iter().skip(info.prefix_spans))
                .flat_map(|span| cells(&span.content))
                .map(|(_, _, width)| width)
                .max()
                .unwrap_or(0);
            let code = CodeGeometry::new(width, indent, longest, widest);
            let decoration = LayoutLine {
                layout: RowLayout::decoration(
                    truncated(block[0].2.continuation.clone(), indent),
                    code,
                ),
                ..LayoutLine::default()
            };
            output.push(decoration.clone());
            for (line, id, info) in block {
                layout_source(line, id, info, width, Some(code), &mut output);
            }
            output.push(decoration);
        } else {
            layout_source(line, id, info, width, None, &mut output);
        }
    }
    Layout {
        lines: output,
        fences: parsed.fences,
    }
}

fn layout_source(
    mut line: Line<'static>,
    source: RowSource,
    info: LineInfo,
    width: usize,
    code: Option<CodeGeometry>,
    output: &mut Vec<LayoutLine>,
) {
    let text: String = line.spans.iter().map(|span| &*span.content).collect();
    let body = line
        .spans
        .split_off(info.prefix_spans.min(line.spans.len()));
    let source_prefix = text_len(&line);
    let source_prefix_width = line
        .width()
        .max(info.continuation.width())
        .min(width.saturating_sub(1));
    let continuation = truncated(info.continuation, width.saturating_sub(1));
    let indent = source_prefix_width.max(continuation.width());
    let budget = code.map_or_else(
        || width.saturating_sub(indent).max(1),
        |code| code.body_width().max(1),
    );
    let body = Line {
        spans: body,
        style: line.style,
        alignment: line.alignment,
    };
    let wrapped = if code.is_some() {
        wrap_line(body, budget, budget)
    } else {
        wrap_words(body, budget)
    };
    let mut start = 0;
    for (index, mut body) in wrapped.into_iter().enumerate() {
        let first = index == 0;
        if first {
            let mut spans = line.spans.clone();
            spans.append(&mut body.spans);
            body.spans = spans;
        }
        let layout = RowLayout::source(
            if first {
                Line::default()
            } else {
                continuation.clone()
            },
            if first { source_prefix } else { 0 },
            if first { source_prefix_width } else { 0 },
            code,
        );
        let row_start = start;
        start += text_len(&body);
        output.push(LayoutLine {
            layout: layout.with_source(source.slice(row_start..start, &text)),
            line: body,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::continued;
    use super::*;
    use unicode_width::UnicodeWidthStr;

    fn layout(text: &str, width: usize) -> Vec<LayoutLine> {
        layout_highlighted(text, true, width, None).lines
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
                rows = layout(&text, width);
            }
            assert_eq!(rows.iter().filter(|row| row.layout.decorative()).count(), 2);
            let mut copied = String::new();
            let mut first = true;
            for row in rows.iter().filter(|row| !row.layout.decorative()) {
                if !first && !continued(&row.layout) {
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
            for width in [18, 23, 28] {
                let rows = layout(&source, width);
                assert!(rows.len() > 4, "cells should wrap inside the table");
                let start = rows
                    .iter()
                    .position(|row| row.line.to_string().contains('┌'));
                let table_rows = &rows[start.unwrap()..];
                let separator = table_rows
                    .iter()
                    .find(|row| row.line.to_string().contains('├'));
                // Measure actual screen columns, including quote containers.
                let columns = border_columns(&separator.unwrap().line.to_string());
                let mut separators = 0;
                for row in table_rows {
                    let text = row.line.to_string();
                    assert!(row.line.width() <= width, "{text:?}, width {width}");
                    let continued = continued(&row.layout);
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
        // Replacement rendering follows resizes of a growing table.
        let mut source = format!("{NARROW_TABLE}\n\nplain tail");
        let wide_rows = layout(&source, 60).len();
        for width in [17, 8, 3, 35, 60] {
            let rows = layout(&source, width);
            assert!(width != 17 || rows.len() > wide_rows);
            source.push_str(" more");
        }
    }
}
