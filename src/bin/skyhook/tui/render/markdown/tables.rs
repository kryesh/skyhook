//! Table sizing, cell wrapping, and bounded grid rendering.
use super::super::{cells, cells_width, wrap_words};
use super::{LineId, SourceSpan, text_len};
use pulldown_cmark::Alignment;
use ratatui::{
    style::Modifier,
    text::{Line, Span},
};

struct Column {
    alignment: Alignment,
    minimum: usize,
    width: usize,
}

/// A cell's rendered text and the source position that identifies it.
type Cell = (Line<'static>, usize);

#[derive(Default)]
pub(super) struct Table {
    columns: Vec<Column>,
    rows: Vec<Vec<Cell>>,
    row: Vec<Cell>,
}

/// A rendered table line and the cell fragments it shows. Each cell is
/// identified by its source position until layout numbers its lines.
pub(super) struct TableLine {
    pub(super) line: Line<'static>,
    pub(super) cells: Vec<SourceSpan>,
}

impl From<Line<'static>> for TableLine {
    fn from(line: Line<'static>) -> Self {
        Self {
            line,
            cells: Vec::new(),
        }
    }
}

impl Table {
    pub(super) fn new(alignments: Vec<Alignment>) -> Self {
        Self {
            columns: alignments
                .into_iter()
                .map(|alignment| Column {
                    alignment,
                    minimum: 1,
                    width: 3,
                })
                .collect(),
            ..Self::default()
        }
    }

    pub(super) fn push_cell(&mut self, cell: Line<'static>, origin: usize) {
        self.row.push((cell, origin));
    }

    /// pulldown-cmark pads and truncates every GFM row to the header width.
    pub(super) fn finish_row(&mut self) {
        self.rows.push(std::mem::take(&mut self.row));
    }

    pub(super) fn lines(mut self, width: usize) -> Vec<TableLine> {
        if !self.row.is_empty() {
            self.finish_row();
        }
        let mut rows = self.rows;
        if self.columns.is_empty() || rows.is_empty() {
            return Vec::new();
        }
        for row in &mut rows {
            for (column, (cell, _)) in self.columns.iter_mut().zip(row) {
                // The TUI paints one grapheme at a time. Some strings (e.g.
                // Arabic lam-alef) measure narrower as a whole than the sum of
                // their graphemes. Split only those spans so width measurement
                // and word wrapping agree with the actual painted cell count.
                let spans = std::mem::take(&mut cell.spans);
                for span in spans {
                    if cells_width(&span.content) != span.width() {
                        cell.spans.extend(
                            cells(&span.content)
                                .map(|(_, g, _)| Span::styled(g.to_owned(), span.style)),
                        );
                    } else {
                        cell.spans.push(span);
                    }
                }
                column.width = column.width.max(cell.width());
                // Never assign less room than a single grapheme needs.
                for span in &cell.spans {
                    for (_, _, width) in cells(&span.content) {
                        column.minimum = column.minimum.max(width);
                    }
                }
            }
        }
        // Each column has two padding cells and a border; include the left edge.
        let budget = width.saturating_sub(3 * self.columns.len() + 1);
        if budget < self.columns.iter().map(|column| column.minimum).sum() {
            return borderless(rows, width);
        }

        if self
            .columns
            .iter()
            .map(|column| column.width)
            .sum::<usize>()
            > budget
        {
            // Cap the widest columns first, leaving short columns at their
            // natural width. Binary search avoids work proportional to the
            // length of an oversized cell (e.g. a URL or tool output).
            let mut low = 1;
            let mut high = self
                .columns
                .iter()
                .map(|column| column.width)
                .max()
                .unwrap_or(1);
            while low < high {
                let cap = low + (high - low).div_ceil(2);
                let used: usize = self
                    .columns
                    .iter()
                    .map(|column| column.width.min(cap).max(column.minimum))
                    .sum();
                if used <= budget {
                    low = cap;
                } else {
                    high = cap - 1;
                }
            }
            let mut remaining = budget
                - self
                    .columns
                    .iter()
                    .map(|column| column.width.min(low).max(column.minimum))
                    .sum::<usize>();
            for column in &mut self.columns {
                let natural = column.width;
                column.width = natural.min(low).max(column.minimum);
                if remaining > 0 && column.width < natural {
                    column.width += 1;
                    remaining -= 1;
                }
            }
        }

        let border = |left: char, junction: &str, right: char| {
            Line::from(format!(
                "{left}{}{right}",
                self.columns
                    .iter()
                    .map(|column| "─".repeat(column.width + 2))
                    .collect::<Vec<_>>()
                    .join(junction)
            ))
        };
        let mut lines = vec![border('┌', "┬", '┐').into()];
        for (index, row) in rows.into_iter().enumerate() {
            if index > 0 {
                lines.push(border('├', "┼", '┤').into());
            }
            let mut cells: Vec<_> = row
                .into_iter()
                .zip(&self.columns)
                .enumerate()
                .map(|(position, ((mut cell, origin), column))| {
                    if index == 0 {
                        bold(&mut cell);
                    }
                    wrap_cell(cell, LineId::cell(origin, position), column.width).into_iter()
                })
                .collect();
            let height = cells.iter().map(ExactSizeIterator::len).max().unwrap_or(1);
            for _ in 0..height {
                let mut line = TableLine::from(Line::from("│"));
                for (index, column) in self.columns.iter().enumerate() {
                    let width = column.width;
                    let fragment = cells.get_mut(index).and_then(Iterator::next);
                    let cell_width = fragment.as_ref().map_or(0, |(cell, _)| cell.width());
                    let padding = width.saturating_sub(cell_width);
                    let left = match column.alignment {
                        Alignment::Right => padding,
                        Alignment::Center => padding / 2,
                        _ => 0,
                    };
                    line.push(Span::raw(" ".repeat(left + 1)));
                    if let Some((cell, span)) = fragment {
                        line.push_cell(cell, span);
                    }
                    line.push(Span::raw(" ".repeat(padding - left + 1)));
                    line.push(Span::raw("│"));
                }
                lines.push(line);
            }
        }
        lines.push(border('└', "┴", '┘').into());
        lines
    }
}

impl TableLine {
    fn push(&mut self, span: Span<'static>) {
        self.line.spans.push(span);
    }
    /// Append a cell fragment, shown where it lands in this line.
    fn push_cell(&mut self, cell: Line<'static>, span: SourceSpan) {
        let at = text_len(&self.line);
        self.line.spans.extend(cell.spans);
        let shown = at..text_len(&self.line);
        self.cells.push(SourceSpan { shown, ..span });
    }
}

/// If even the borders and one grapheme per column cannot fit, stack cells
/// instead of emitting a broken grid or discarding their text.
fn borderless(rows: Vec<Vec<Cell>>, width: usize) -> Vec<TableLine> {
    let mut lines = Vec::new();
    for (index, row) in rows.into_iter().enumerate() {
        if index > 0 {
            lines.push(Line::default().into());
        }
        for (position, (mut cell, origin)) in row.into_iter().enumerate() {
            if index == 0 {
                bold(&mut cell);
            }
            for (fragment, span) in wrap_cell(cell, LineId::cell(origin, position), width) {
                let mut line = TableLine::from(Line::default());
                line.push_cell(fragment, span);
                lines.push(line);
            }
        }
    }
    lines
}

fn bold(line: &mut Line<'_>) {
    for span in &mut line.spans {
        span.style = span.style.add_modifier(Modifier::BOLD);
    }
}

/// A cell's rows as shown, each with the span of the cell's own text it
/// stands for. Positions refer to that text, whatever the row shows for it.
fn wrap_cell(cell: Line<'static>, id: LineId, width: usize) -> Vec<(Line<'static>, SourceSpan)> {
    let width = width.max(1);
    let text: String = cell.spans.iter().map(|span| &*span.content).collect();
    let text = std::sync::Arc::<str>::from(text);
    let mut start = 0;
    let mut lines = Vec::new();
    for mut line in wrap_words(cell, width) {
        let source = start..start + text_len(&line);
        start = source.end;
        for span in &mut line.spans {
            // Only the borderless fallback can be narrower than a grapheme.
            // Show a replacement there, rather than overwriting the next cell.
            if cells(&span.content).any(|(_, _, w)| w > width) {
                span.content = cells(&span.content)
                    .map(|(_, g, w)| if w > width { "�" } else { g })
                    .collect::<String>()
                    .into();
            }
        }
        // Prose wrapping retains trailing separators beyond the viewport for
        // selection. Inside a cell they must not push padding/borders outward;
        // the span still stands for them.
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
        let shown: String = line.spans.iter().map(|span| &*span.content).collect();
        let span = SourceSpan {
            line: id,
            text: (shown != text[source.clone()]).then(|| text.clone()),
            source,
            shown: 0..shown.len(),
        };
        lines.push((line, span));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::super::super::THEME;
    use super::super::parser::tests::{rendered as table, strings};
    use super::*;
    use ratatui::style::Style;
    use unicode_width::UnicodeWidthStr;

    const KEYS: &str = "| Key | Description |\n| --- | --- |\n| x | one two three four |";
    const STYLED: &str =
        "| L | C | R |\n| :--- | :---: | ---: |\n| **abcdef** | *ab cd e* | `abcde` |";

    #[test]
    fn tables_wrap_cells_not_borders_and_keep_alignment_widths_and_styles() {
        for (source, width, expected) in [
            (
                KEYS,
                19,
                &[
                    "┌─────┬───────────┐",
                    "│ Key │ Descripti │",
                    "│     │ on        │",
                    "├─────┼───────────┤",
                    "│ x   │ one two   │",
                    "│     │ three     │",
                    "│     │ four      │",
                    "└─────┴───────────┘",
                ][..],
            ),
            (
                STYLED,
                22,
                &[
                    "┌──────┬──────┬──────┐",
                    "│ L    │  C   │    R │",
                    "├──────┼──────┼──────┤",
                    "│ abcd │  ab  │ abcd │",
                    "│ ef   │ cd e │    e │",
                    "└──────┴──────┴──────┘",
                ],
            ),
            // A fitting table keeps its natural widths.
            (
                "| Left | Center | Right |\n| :--- | :---: | ---: |\n| a | b | c |",
                80,
                &[
                    "┌──────┬────────┬───────┐",
                    "│ Left │ Center │ Right │",
                    "├──────┼────────┼───────┤",
                    "│ a    │   b    │     c │",
                    "└──────┴────────┴───────┘",
                ],
            ),
        ] {
            let lines = table(source, width);
            assert_eq!(strings(&lines), expected);
            assert!(lines.iter().all(|line| line.width() == expected[0].width()));
        }
        let lines = table(KEYS, 19);
        let header = lines[1..3].iter().flat_map(|line| &line.spans);
        for span in header.filter(|span| span.content.chars().any(char::is_alphabetic)) {
            assert!(span.style.add_modifier.contains(Modifier::BOLD));
        }
        let lines = table(STYLED, 22);
        let spans: Vec<_> = lines[3..].iter().flat_map(|line| &line.spans).collect();
        let has = |text: &str, test: &dyn Fn(Style) -> bool| {
            spans
                .iter()
                .any(|span| span.content == text && test(span.style))
        };
        for text in ["abcd", "ef"] {
            assert!(has(text, &|style| style
                .add_modifier
                .contains(Modifier::BOLD)));
        }
        assert!(has("cd", &|style| style
            .add_modifier
            .contains(Modifier::ITALIC)));
        let code = THEME.inline_code;
        assert!(has("e", &|style| style.fg == Some(code)));
    }

    #[test]
    fn unicode_empty_cells_nesting_and_tiny_widths_stay_bounded() {
        let text = "| Name | Value | Empty |\n| --- | --- | --- |\n| 界界界 | 👩‍💻e\u{301}abcdefghijk | |\n| last | | |";
        for width in 1..80 {
            let lines = table(text, width);
            let rendered = strings(&lines).join("\n");
            assert!(
                lines.iter().all(|line| line.width() <= width),
                "overflow at {width}: {rendered}"
            );
            if width >= 2 {
                assert_eq!(rendered.matches('界').count(), 3);
                assert!(rendered.contains("👩‍💻") && rendered.contains("e\u{301}"));
            }
        }
        for text in [
            "> | Key | Description |\n> | --- | --- |\n> | x | one two three four five |",
            "- item\n\n  | Key | Description |\n  | --- | --- |\n  | x | one two three four five |",
            "> - item\n>\n>   | Key | Description |\n>   | --- | --- |\n>   | x | one two three four five |",
        ] {
            let lines = strings(&table(text, 24));
            assert!(lines.iter().all(|line| line.width() <= 24), "{lines:?}");
            assert!(lines.iter().any(|line| line.contains('┼')));
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
                let y = y as u16;
                assert_eq!(line.width(), grid_width);
                let area = Rect::new(0, y, width as u16, 1);
                super::super::super::render_line(line, area, &mut buffer, Style::default());
                let left = buffer[(0, y)].symbol();
                let right = buffer[((grid_width - 1) as u16, y)].symbol();
                let expected = match left {
                    "┌" => "┐",
                    "├" => "┤",
                    "└" => "┘",
                    "│" => "│",
                    _ => panic!("unexpected left border: {left}"),
                };
                assert_eq!(
                    right, expected,
                    "misaligned border at width {width}, row {y}"
                );
            }
        }
    }
}
