//! Table sizing, cell wrapping, and bounded grid rendering.
use super::super::wrap_words;
use pulldown_cmark::Alignment;
use ratatui::{
    style::Modifier,
    text::{Line, Span},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Default)]
pub(super) struct Table {
    alignments: Vec<Alignment>,
    rows: Vec<Vec<Line<'static>>>,
    row: Vec<Line<'static>>,
}

impl Table {
    pub(super) fn new(alignments: Vec<Alignment>) -> Self {
        Self {
            alignments,
            ..Self::default()
        }
    }

    pub(super) fn push_cell(&mut self, cell: Line<'static>) {
        self.row.push(cell);
    }

    pub(super) fn finish_row(&mut self) {
        self.rows.push(std::mem::take(&mut self.row));
    }

    pub(super) fn lines(mut self, width: usize) -> Vec<Line<'static>> {
        let mut widths = vec![3; self.alignments.len()];
        let mut minimums = vec![1; widths.len()];
        for row in &mut self.rows {
            for (column, cell) in row.iter_mut().enumerate() {
                // The TUI paints one grapheme at a time. Some strings (e.g.
                // Arabic lam-alef) measure narrower as a whole than the sum of
                // their graphemes. Split only those spans so width measurement
                // and word wrapping agree with the actual painted cell count.
                let spans = std::mem::take(&mut cell.spans);
                for span in spans {
                    let painted: usize = span.content.graphemes(true).map(|g| g.width()).sum();
                    if painted != span.width() {
                        cell.spans.extend(
                            span.content
                                .graphemes(true)
                                .map(|g| Span::styled(g.to_owned(), span.style)),
                        );
                    } else {
                        cell.spans.push(span);
                    }
                }
                widths[column] = widths[column].max(cell.width());
                // Never assign less room than a single grapheme needs.
                for span in &cell.spans {
                    for grapheme in span.content.graphemes(true) {
                        minimums[column] = minimums[column].max(grapheme.width());
                    }
                }
            }
        }
        // Each column has two padding cells and a border; include the left edge.
        let budget = width.saturating_sub(3 * widths.len() + 1);
        if budget < minimums.iter().sum() {
            // If even the borders and one grapheme per column cannot fit, stack
            // cells instead of emitting a broken grid or discarding their text.
            let mut lines = Vec::new();
            for (index, row) in self.rows.into_iter().enumerate() {
                if index > 0 {
                    lines.push(Line::default());
                }
                for mut cell in row {
                    if index == 0 {
                        bold(&mut cell);
                    }
                    lines.extend(wrap_cell(cell, width));
                }
            }
            return lines;
        }
        if widths.iter().sum::<usize>() > budget {
            // Cap the widest columns first, leaving short columns at their
            // natural width. Binary search avoids work proportional to the
            // length of an oversized cell (e.g. a URL or tool output).
            let mut low = 1;
            let mut high = widths.iter().copied().max().unwrap_or(1);
            while low < high {
                let cap = low + (high - low).div_ceil(2);
                let used: usize = widths
                    .iter()
                    .zip(&minimums)
                    .map(|(&natural, &min)| natural.min(cap).max(min))
                    .sum();
                if used <= budget {
                    low = cap;
                } else {
                    high = cap - 1;
                }
            }
            let natural = widths.clone();
            for (width, &min) in widths.iter_mut().zip(&minimums) {
                *width = (*width).min(low).max(min);
            }
            let mut remaining = budget - widths.iter().sum::<usize>();
            for (width, natural) in widths.iter_mut().zip(natural) {
                if remaining > 0 && *width < natural {
                    *width += 1;
                    remaining -= 1;
                }
            }
        }
        let border = |left: char, junction: &str, right: char| {
            Line::from(format!(
                "{left}{}{right}",
                widths
                    .iter()
                    .map(|width| "─".repeat(width + 2))
                    .collect::<Vec<_>>()
                    .join(junction)
            ))
        };
        let mut lines = vec![border('┌', "┬", '┐')];
        for (index, row) in self.rows.into_iter().enumerate() {
            if index > 0 {
                lines.push(border('├', "┼", '┤'));
            }
            let mut cells: Vec<_> = row
                .into_iter()
                .zip(&widths)
                .map(|(mut cell, &width)| {
                    if index == 0 {
                        bold(&mut cell);
                    }
                    wrap_cell(cell, width).into_iter()
                })
                .collect();
            let height = cells.iter().map(ExactSizeIterator::len).max().unwrap_or(1);
            for _ in 0..height {
                let mut spans = vec![Span::raw("│")];
                for (column, &width) in widths.iter().enumerate() {
                    let cell = cells
                        .get_mut(column)
                        .and_then(Iterator::next)
                        .unwrap_or_default();
                    let padding = width.saturating_sub(cell.width());
                    let left = match self.alignments[column] {
                        Alignment::Right => padding,
                        Alignment::Center => padding / 2,
                        _ => 0,
                    };
                    spans.push(Span::raw(" ".repeat(left + 1)));
                    spans.extend(cell.spans);
                    spans.push(Span::raw(" ".repeat(padding - left + 1)));
                    spans.push(Span::raw("│"));
                }
                lines.push(Line::from(spans));
            }
        }
        lines.push(border('└', "┴", '┘'));
        lines
    }
}

fn bold(line: &mut Line<'_>) {
    for span in &mut line.spans {
        span.style = span.style.add_modifier(Modifier::BOLD);
    }
}

fn wrap_cell(mut cell: Line<'static>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    // Only the borderless fallback can be narrower than a grapheme. Use a
    // visible replacement there, rather than letting it overwrite the next cell.
    for span in &mut cell.spans {
        if span.content.graphemes(true).any(|g| g.width() > width) {
            span.content = span
                .content
                .graphemes(true)
                .map(|g| if g.width() > width { "�" } else { g })
                .collect::<String>()
                .into();
        }
    }
    let mut lines = wrap_words(cell, width);
    for line in &mut lines {
        // Prose wrapping retains trailing separators beyond the viewport for
        // selection. Inside a cell they must not push padding/borders outward.
        while let Some(span) = line.spans.last_mut() {
            let trimmed = span.content.trim_end();
            if trimmed.is_empty() {
                line.spans.pop();
            } else {
                if trimmed.len() != span.content.len() {
                    span.content = trimmed.to_owned().into();
                }
                break;
            }
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::super::super::Palette;
    use super::super::render;
    use super::*;
    use ratatui::style::Style;

    fn table(text: &str, width: usize) -> Vec<Line<'static>> {
        render(text, Palette::new(), false, width)
    }

    fn strings(lines: &[Line<'_>]) -> Vec<String> {
        lines.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn wraps_headers_and_cells_without_wrapping_borders() {
        let lines = table(
            "| Key | Description |\n| --- | --- |\n| x | one two three four |",
            19,
        );
        assert_eq!(
            strings(&lines),
            [
                "┌─────┬───────────┐",
                "│ Key │ Descripti │",
                "│     │ on        │",
                "├─────┼───────────┤",
                "│ x   │ one two   │",
                "│     │ three     │",
                "│     │ four      │",
                "└─────┴───────────┘",
            ]
        );
        assert!(lines.iter().all(|line| line.width() == 19));
        for line in &lines[1..3] {
            for span in &line.spans {
                if span.content.chars().any(char::is_alphabetic) {
                    assert!(span.style.add_modifier.contains(Modifier::BOLD));
                }
            }
        }
    }

    #[test]
    fn wrapped_cells_keep_inline_styles_and_alignment() {
        let lines = table(
            "| L | C | R |\n| :--- | :---: | ---: |\n| **abcdef** | *ab cd e* | `abcde` |",
            22,
        );
        assert_eq!(
            strings(&lines),
            [
                "┌──────┬──────┬──────┐",
                "│ L    │  C   │    R │",
                "├──────┼──────┼──────┤",
                "│ abcd │  ab  │ abcd │",
                "│ ef   │ cd e │    e │",
                "└──────┴──────┴──────┘",
            ]
        );
        let spans: Vec<_> = lines[3..].iter().flat_map(|line| &line.spans).collect();
        for text in ["abcd", "ef"] {
            assert!(spans.iter().any(
                |span| span.content == text && span.style.add_modifier.contains(Modifier::BOLD)
            ));
        }
        assert!(
            spans
                .iter()
                .any(|span| span.content == "cd"
                    && span.style.add_modifier.contains(Modifier::ITALIC))
        );
        assert!(
            spans.iter().any(|span| span.content == "e"
                && span.style.fg == Some(Palette::new().content.inline_code))
        );
    }

    #[test]
    fn unicode_empty_cells_and_tiny_widths_stay_bounded() {
        let text = "| Name | Value | Empty |\n| --- | --- | --- |\n| 界界界 | 👩‍💻e\u{301}abcdefghijk | |\n| last | | |";
        for width in 1..80 {
            let lines = table(text, width);
            assert!(
                lines.iter().all(|line| line.width() <= width),
                "overflow at {width}: {:?}",
                strings(&lines)
            );
            if width >= 2 {
                let rendered = strings(&lines).join("\n");
                assert_eq!(rendered.matches('界').count(), 3);
                assert!(rendered.contains("👩‍💻"));
                assert!(rendered.contains("e\u{301}"));
            }
        }
    }

    #[test]
    fn table_borders_use_the_same_unicode_width_as_the_screen() {
        use ratatui::{buffer::Buffer, layout::Rect};

        let source = "| H |\n| --- |\n| **لالالا** |\n| 界👩‍💻e\u{301} |";
        for width in 6..30 {
            let lines = table(source, width);
            let grid_width = lines[0].width();
            let mut buffer = Buffer::empty(Rect::new(0, 0, width as u16, lines.len() as u16));
            for (y, line) in lines.iter().enumerate() {
                assert_eq!(line.width(), grid_width);
                super::super::super::render_line(
                    line,
                    Rect::new(0, y as u16, width as u16, 1),
                    &mut buffer,
                    Style::default(),
                );
                let left = buffer[(0, y as u16)].symbol();
                let right = buffer[((grid_width - 1) as u16, y as u16)].symbol();
                let expected_right = match left {
                    "┌" => "┐",
                    "├" => "┤",
                    "└" => "┘",
                    "│" => "│",
                    _ => panic!("unexpected left border: {left}"),
                };
                assert_eq!(
                    right, expected_right,
                    "misaligned border at width {width}, row {y}"
                );
            }
        }
    }

    #[test]
    fn nested_tables_budget_quote_and_list_indentation() {
        for text in [
            "> | Key | Description |\n> | --- | --- |\n> | x | one two three four five |",
            "- item\n\n  | Key | Description |\n  | --- | --- |\n  | x | one two three four five |",
            "> - item\n>\n>   | Key | Description |\n>   | --- | --- |\n>   | x | one two three four five |",
        ] {
            let lines = table(text, 24);
            assert!(
                lines.iter().all(|line| line.width() <= 24),
                "{:?}",
                strings(&lines)
            );
            assert!(strings(&lines).iter().any(|line| line.contains('┼')));
        }
    }
    #[test]
    fn fitting_table_keeps_natural_widths_and_alignment() {
        let lines = table(
            "| Left | Center | Right |\n| :--- | :---: | ---: |\n| a | b | c |",
            80,
        );
        assert_eq!(
            strings(&lines),
            [
                "┌──────┬────────┬───────┐",
                "│ Left │ Center │ Right │",
                "├──────┼────────┼───────┤",
                "│ a    │   b    │     c │",
                "└──────┴────────┴───────┘",
            ]
        );
    }
}
