//! Wrapping and source-versus-decoration geometry for Markdown rows.
use super::super::super::tool_view;
use super::super::{Palette, wrap_line, wrap_words};
use super::parser::{LineInfo, parse};
use super::{CodeRow, LayoutLine, RowLayout};
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Wrap source inside its paragraph/container, then add geometry-only code
/// padding. Measuring the entire code block keeps all its rows the same width.
pub(in super::super) fn layout_highlighted(
    text: &str,
    palette: Palette,
    placeholder: bool,
    width: usize,
    markdown_width: usize,
    prefix: &str,
    cache: Option<&tool_view::HighlightCache>,
) -> Vec<LayoutLine> {
    let width = width.max(1);
    let mut parsed = parse(text, palette, placeholder, markdown_width, cache);
    if !prefix.is_empty() && !parsed.lines.is_empty() {
        if super::super::stream::starts_with_table(text) {
            parsed.lines.insert(0, Line::from(prefix.to_owned()));
            parsed.info = parsed
                .info
                .into_iter()
                .map(|(index, info)| (index + 1, info))
                .collect();
        } else {
            parsed.lines[0]
                .spans
                .insert(0, Span::raw(prefix.to_owned()));
            let info = parsed.info.entry(0).or_default();
            if info.prefix_spans > 0 || info.code.is_some() {
                info.prefix_spans += 1;
            }
            // The reasoning spinner is only a first-row decoration, not a
            // Markdown container to repeat on later visual rows.
        }
    }
    let mut input = parsed
        .lines
        .into_iter()
        .enumerate()
        .map(|(index, line)| (line, parsed.info.remove(&index).unwrap_or_default()))
        .peekable();
    let mut output = Vec::new();
    while let Some((line, info)) = input.next() {
        if let Some(id) = info.code {
            let mut block = vec![(line, info)];
            while input.peek().is_some_and(|(_, info)| info.code == Some(id)) {
                block.push(input.next().unwrap());
            }
            let indent = block
                .iter()
                .map(|(line, info)| {
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
            let available = width.saturating_sub(indent).max(1);
            let longest = block
                .iter()
                .map(|(line, info)| {
                    line.spans
                        .iter()
                        .skip(info.prefix_spans)
                        .flat_map(|span| span.content.graphemes(true))
                        .map(|g| g.width())
                        .sum::<usize>()
                })
                .max()
                .unwrap_or(0);
            let block_width = longest.saturating_add(2).min(available);
            // Very narrow containers prioritize a source grapheme over padding.
            let widest = block
                .iter()
                .flat_map(|(line, info)| line.spans.iter().skip(info.prefix_spans))
                .flat_map(|span| span.content.graphemes(true))
                .map(|g| g.width())
                .max()
                .unwrap_or(0);
            let padding = usize::from(block_width >= widest.saturating_add(2));
            let code = CodeRow {
                indent,
                width: block_width,
                padding,
            };
            let decoration = LayoutLine {
                layout: RowLayout {
                    prefix: clipped_prefix(block[0].1.continuation.clone(), indent),
                    code: Some(code),
                    decorative: true,
                    ..RowLayout::default()
                },
                ..LayoutLine::default()
            };
            output.push(decoration.clone());
            for (line, info) in block {
                layout_source(line, info, width, Some(code), &mut output);
            }
            output.push(decoration);
        } else {
            layout_source(line, info, width, None, &mut output);
        }
    }
    output
}

fn clipped_prefix(mut line: Line<'static>, width: usize) -> Line<'static> {
    let mut remaining = width;
    line.spans = line
        .spans
        .into_iter()
        .filter_map(|span| {
            let mut text = String::new();
            for grapheme in span.content.graphemes(true) {
                if grapheme.width() > remaining {
                    break;
                }
                remaining -= grapheme.width();
                text.push_str(grapheme);
            }
            (!text.is_empty()).then(|| Span::styled(text, span.style))
        })
        .collect();
    line
}

fn layout_source(
    mut line: Line<'static>,
    info: LineInfo,
    width: usize,
    code: Option<CodeRow>,
    output: &mut Vec<LayoutLine>,
) {
    let body = line
        .spans
        .split_off(info.prefix_spans.min(line.spans.len()));
    let source_prefix = line.to_string().len();
    let source_prefix_width = line
        .width()
        .max(info.continuation.width())
        .min(width.saturating_sub(1));
    let continuation = clipped_prefix(info.continuation, width.saturating_sub(1));
    let indent = source_prefix_width.max(continuation.width());
    let budget = code.map_or_else(
        || width.saturating_sub(indent).max(1),
        |code| code.width.saturating_sub(code.padding * 2).max(1),
    );
    let body = Line {
        spans: body,
        style: line.style,
        alignment: line.alignment,
    };
    let wrapped = if code.is_some() {
        wrap_line(body, budget)
    } else {
        wrap_words(body, budget)
    };
    for (index, mut body) in wrapped.into_iter().enumerate() {
        let first = index == 0;
        if first {
            let mut spans = line.spans.clone();
            spans.append(&mut body.spans);
            body.spans = spans;
        }
        output.push(LayoutLine {
            line: body,
            continued: !first,
            layout: RowLayout {
                prefix: if first {
                    Line::default()
                } else {
                    continuation.clone()
                },
                source_prefix: if first { source_prefix } else { 0 },
                source_prefix_width: if first { source_prefix_width } else { 0 },
                code,
                decorative: false,
            },
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::render;
    use super::*;
    use ratatui::style::Style;

    /// Exercise the paint path: source strings alone cannot reveal a gap
    /// inserted by source_prefix_width or decorative continuation prefixes.
    fn painted_markdown(text: &str, width: u16, p: Palette) -> Vec<String> {
        use super::super::super::{
            Row, RowBlocks, Surface, TextPosition, render_row_line, selected_text,
        };
        use ratatui::{buffer::Buffer, layout::Rect};

        let rows: Vec<Row> =
            layout_highlighted(text, p, false, width as usize, width as usize, "", None)
                .into_iter()
                .map(|line| Row {
                    line: std::sync::Arc::new(line.line),
                    header: false,
                    x: 0,
                    width,
                    surface: Surface::Tool,
                    entry: 0,
                    selectable: true,
                    blank: false,
                    continued: line.continued,
                    layout: line.layout,
                    inset: 0,
                })
                .collect();
        let mut blocks = RowBlocks::default();
        *blocks.block_mut(0) = rows.clone();
        blocks.finish_update(0);
        assert_eq!(
            selected_text(
                &blocks,
                (
                    TextPosition { row: 0, byte: 0 },
                    TextPosition {
                        row: rows.len() - 1,
                        byte: usize::MAX,
                    },
                ),
            ),
            strings(&render(text, p, false, width as usize)).join("\n"),
            "wrapping/prefix geometry must not change copied source: {text:?}",
        );
        rows.iter()
            .map(|row| {
                let area = Rect::new(0, 0, width, 1);
                let mut buffer = Buffer::empty(area);
                render_row_line(row, area, &mut buffer, Style::default(), p);
                (0..width)
                    .map(|x| buffer[(x, 0)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    #[test]
    fn task_descendant_markers_do_not_inherit_checkbox_hanging_indent() {
        for (text, expected) in [
            ("- [ ] task\n  - child", vec!["• [ ] task", "  • child"]),
            ("- [ ] task\n  > quoted", vec!["• [ ] task", "  │ quoted"]),
            (
                "- [ ] task\n\n  10. child",
                vec!["• [ ] task", "  10. child"],
            ),
            ("- [ ] task\n  - [x] hi", vec!["• [ ] task", "  • [x] hi"]),
            (
                "> - [ ] task\n>   - child",
                vec!["│ • [ ] task", "│   • child"],
            ),
            ("- [ ] task\n  > - child", vec!["• [ ] task", "  │ • child"]),
            (
                "10. [ ] task\n    - child",
                vec!["10. [ ] task", "    • child"],
            ),
        ] {
            assert_eq!(
                painted_markdown(text, 12, Palette::new()),
                expected,
                "{text:?}"
            );
        }
    }

    #[test]
    fn task_descendant_wrapping_uses_only_its_own_container_prefix() {
        for (text, expected) in [
            (
                "- [ ] task\n  - child one",
                vec!["• [ ] task", "  • child", "    one"],
            ),
            (
                "- [ ] task\n  > quoted one",
                vec!["• [ ] task", "  │ quoted", "  │ one"],
            ),
            // Nested tasks retain their own checkbox's hanging width.
            (
                "- [ ] task\n  - [x] one two",
                vec!["• [ ] task", "  • [x] one", "        two"],
            ),
            // The task paragraph still hangs beneath its checkbox.
            (
                "- [ ] alpha beta\n  gamma",
                vec!["• [ ] alpha", "      beta", "      gamma"],
            ),
        ] {
            assert_eq!(
                painted_markdown(text, 12, Palette::new()),
                expected,
                "{text:?}"
            );
        }
    }

    fn strings(lines: &[Line<'_>]) -> Vec<String> {
        lines.iter().map(ToString::to_string).collect()
    }
}
