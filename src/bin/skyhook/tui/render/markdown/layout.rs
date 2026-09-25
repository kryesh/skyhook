//! Wrapping and source-versus-decoration geometry for Markdown rows.
use super::super::super::tool_view;
use super::super::{Palette, cells, cells_width, wrap_line, wrap_words};
use super::parser::{LineInfo, parse};
use super::{CodeGeometry, LayoutLine, RowLayout};
use ratatui::text::{Line, Span};

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
            parsed.lines.insert(0, Line::from(prefix.to_owned()).into());
        } else {
            parsed.lines[0]
                .line
                .spans
                .insert(0, Span::raw(prefix.to_owned()));
            let info = &mut parsed.lines[0].info;
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
        .map(|parsed| (parsed.line, parsed.info))
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
            let longest = block
                .iter()
                .map(|(line, info)| {
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
                .flat_map(|(line, info)| line.spans.iter().skip(info.prefix_spans))
                .flat_map(|span| cells(&span.content))
                .map(|(_, _, width)| width)
                .max()
                .unwrap_or(0);
            let code = CodeGeometry::new(width, indent, longest, widest);
            let decoration = LayoutLine {
                layout: RowLayout::decoration(
                    clipped_prefix(block[0].1.continuation.clone(), indent),
                    code,
                ),
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
            let end = cells(&span.content)
                .find(|&(_, _, width)| {
                    let clipped = width > remaining;
                    remaining -= if clipped { 0 } else { width };
                    clipped
                })
                .map_or(span.content.len(), |(byte, _, _)| byte);
            (end > 0).then(|| Span::styled(span.content[..end].to_owned(), span.style))
        })
        .collect();
    line
}

fn layout_source(
    mut line: Line<'static>,
    info: LineInfo,
    width: usize,
    code: Option<CodeGeometry>,
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
        |code| code.body_width().max(1),
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
            layout: RowLayout::source(
                if first {
                    Line::default()
                } else {
                    continuation.clone()
                },
                if first { source_prefix } else { 0 },
                if first { source_prefix_width } else { 0 },
                code,
            )
            .with_flow(false, !first),
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
    fn painted_markdown(text: &str) -> Vec<String> {
        use super::super::super::{
            Row, RowBlocks, TextPosition, render_row_line, selected_text, tests::fixture_row,
        };
        use ratatui::{buffer::Buffer, layout::Rect};
        let (p, width) = (Palette::new(), 12);
        let lines = layout_highlighted(text, p, false, width as usize, width as usize, "", None);
        let rows: Vec<Row> = lines
            .into_iter()
            .map(|line| fixture_row(line.line, line.layout, 0, width, 0))
            .collect();
        let mut blocks = RowBlocks::default();
        blocks.replace_entry(0, rows.clone(), Vec::new());
        let end = TextPosition {
            row: rows.len() - 1,
            byte: usize::MAX,
        };
        let copied = selected_text(&blocks, (TextPosition { row: 0, byte: 0 }, end));
        let source = render(text, p, false, width as usize);
        let source: Vec<_> = source.iter().map(ToString::to_string).collect();
        assert_eq!(
            copied,
            source.join("\n"),
            "wrapping/prefix geometry must not change copied source: {text:?}"
        );
        rows.iter()
            .map(|row| {
                let area = Rect::new(0, 0, width, 1);
                let mut buffer = Buffer::empty(area);
                render_row_line(row, area, &mut buffer, Style::default(), p);
                let painted: String = (0..width).map(|x| buffer[(x, 0)].symbol()).collect();
                painted.trim_end().to_owned()
            })
            .collect()
    }

    #[test]
    fn task_descendants_use_only_their_own_container_prefix_and_hanging_indent() {
        for (text, expected) in [
            ("- [ ] task\n  - child", &["• [ ] task", "  • child"][..]),
            ("- [ ] task\n  > quoted", &["• [ ] task", "  │ quoted"]),
            ("- [ ] task\n\n  10. child", &["• [ ] task", "  10. child"]),
            ("- [ ] task\n  - [x] hi", &["• [ ] task", "  • [x] hi"]),
            (
                "> - [ ] task\n>   - child",
                &["│ • [ ] task", "│   • child"],
            ),
            ("- [ ] task\n  > - child", &["• [ ] task", "  │ • child"]),
            (
                "10. [ ] task\n    - child",
                &["10. [ ] task", "    • child"],
            ),
            // Wrapped descendants hang beneath their own markers only.
            (
                "- [ ] task\n  - child one",
                &["• [ ] task", "  • child", "    one"],
            ),
            (
                "- [ ] task\n  > quoted one",
                &["• [ ] task", "  │ quoted", "  │ one"],
            ),
            // Nested tasks retain their own checkbox's hanging width.
            (
                "- [ ] task\n  - [x] one two",
                &["• [ ] task", "  • [x] one", "        two"],
            ),
            // The task paragraph still hangs beneath its checkbox.
            (
                "- [ ] alpha beta\n  gamma",
                &["• [ ] alpha", "      beta", "      gamma"],
            ),
        ] {
            assert_eq!(painted_markdown(text), expected, "{text:?}");
        }
    }
}
