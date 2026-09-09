//! Markdown shared by saved responses, reasoning, and live streaming fragments.
//! Soft breaks intentionally remain newlines in chat. Block boundaries add one
//! blank row, while tight list items and code-block lines remain consecutive.
use super::{Palette, wrap_words};
use pulldown_cmark::{Alignment, Event, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub(super) fn options() -> Options {
    Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS
}

#[derive(Default)]
struct Table {
    alignments: Vec<Alignment>,
    rows: Vec<Vec<Line<'static>>>,
    row: Vec<Line<'static>>,
}

impl Table {
    fn finish_row(&mut self) {
        self.rows.push(std::mem::take(&mut self.row));
    }

    fn lines(mut self, width: usize) -> Vec<Line<'static>> {
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

#[derive(Default)]
struct List {
    next: Option<u64>,
    indent: usize,
    loose: bool,
    started: bool,
}

enum Container {
    Quote,
    List(usize),
}

struct Renderer {
    palette: Palette,
    width: usize,
    lines: Vec<Line<'static>>,
    spans: Vec<Span<'static>>,
    gap: bool,
    bold: usize,
    italic: usize,
    strike: usize,
    code: bool,
    containers: Vec<Container>,
    marker_only: bool,
    lists: Vec<List>,
    links: Vec<String>,
    table: Option<Table>,
}

impl Renderer {
    fn style(&self) -> Style {
        let mut style = Style::default();
        if self.bold > 0 {
            style = style.add_modifier(Modifier::BOLD);
        }
        if self.italic > 0 {
            style = style.add_modifier(Modifier::ITALIC);
        }
        if self.strike > 0 {
            style = style.add_modifier(Modifier::CROSSED_OUT);
        }
        if self.code || !self.links.is_empty() {
            style = style.fg(self.palette.accent);
        }
        style
    }

    fn begin(&mut self) {
        if self.gap && !self.lines.is_empty() {
            self.lines.push(Line::default());
        }
        self.gap = false;
    }

    fn flush(&mut self, force: bool) {
        if !force && self.spans.is_empty() {
            return;
        }
        self.begin();
        let mut spans = Vec::new();
        for container in &self.containers {
            match container {
                Container::Quote => {
                    spans.push(Span::styled("│ ", Style::default().fg(self.palette.muted)))
                }
                Container::List(index) => {
                    let indent = self.lists[*index].indent;
                    if indent > 0 {
                        spans.push(Span::raw(" ".repeat(indent)));
                    }
                }
            }
        }
        self.spans.retain(|span| !span.content.is_empty());
        spans.append(&mut self.spans);
        self.lines.push(Line::from(spans));
        self.marker_only = false;
        if let Some(list) = self.lists.last_mut()
            && list.indent == 0
        {
            list.indent = list
                .next
                .map_or(2, |next| format!("{}. ", next.saturating_sub(1)).len());
        }
    }

    fn block_end(&mut self) {
        self.flush(false);
        self.gap = true;
    }

    fn text(&mut self, text: &str, style: Style) {
        if !text.is_empty() {
            self.marker_only = false;
        }
        for (index, part) in text.split('\n').enumerate() {
            if index > 0 {
                self.flush(true);
            }
            if !part.is_empty() {
                self.spans.push(Span::styled(part.to_owned(), style));
            }
        }
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(Tag::Strong) => self.bold += 1,
            Event::Start(Tag::Heading { .. }) => {
                if !self.marker_only {
                    self.flush(false);
                }
                self.bold += 1;
            }
            Event::End(TagEnd::Strong) => self.bold = self.bold.saturating_sub(1),
            Event::End(TagEnd::Heading(_)) => {
                self.bold = self.bold.saturating_sub(1);
                self.block_end();
            }
            Event::Start(Tag::Emphasis) => self.italic += 1,
            Event::End(TagEnd::Emphasis) => self.italic = self.italic.saturating_sub(1),
            Event::Start(Tag::Strikethrough) => self.strike += 1,
            Event::End(TagEnd::Strikethrough) => self.strike = self.strike.saturating_sub(1),
            Event::Start(Tag::CodeBlock(_)) => {
                if !self.marker_only {
                    self.flush(false);
                }
                self.code = true;
            }
            Event::End(TagEnd::CodeBlock) => {
                self.code = false;
                self.block_end();
            }
            Event::Start(Tag::BlockQuote(_)) => {
                self.flush(false);
                self.containers.push(Container::Quote);
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                self.block_end();
                self.containers.pop();
            }
            Event::Start(Tag::List(next)) => {
                self.flush(false);
                // Nested lists attach directly to their parent item.
                if !self.lists.is_empty() {
                    self.gap = false;
                }
                self.containers.push(Container::List(self.lists.len()));
                self.lists.push(List {
                    next,
                    indent: 0,
                    loose: false,
                    started: false,
                });
            }
            Event::End(TagEnd::List(_)) => {
                self.flush(false);
                self.lists.pop();
                self.containers.pop();
                self.gap = true;
            }
            Event::Start(Tag::Item) => {
                if self
                    .lists
                    .last()
                    .is_some_and(|list| list.started && !list.loose)
                {
                    self.gap = false;
                }
                self.begin();
                self.marker_only = true;
                let list = self.lists.last_mut().expect("item inside list");
                list.indent = 0;
                list.started = true;
                let marker = if let Some(next) = &mut list.next {
                    let marker = format!("{next}. ");
                    *next = next.saturating_add(1);
                    marker
                } else {
                    "• ".to_owned()
                };
                self.spans.push(Span::raw(marker));
            }
            Event::End(TagEnd::Item) => self.flush(false),
            Event::Start(Tag::Paragraph) => {
                if matches!(self.containers.last(), Some(Container::List(_))) {
                    self.lists.last_mut().unwrap().loose = true;
                }
                // A loose item's first paragraph follows its marker on the same row.
                if self.spans.is_empty() {
                    self.begin();
                }
            }
            Event::End(TagEnd::Paragraph) => self.block_end(),
            Event::Start(Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. }) => {
                self.links.push(dest_url.into_string());
            }
            Event::End(TagEnd::Link | TagEnd::Image) => {
                if let Some(url) = self.links.pop() {
                    // Autolinks already show the destination.
                    if !url.is_empty() && !self.spans.last().is_some_and(|span| span.content == url)
                    {
                        self.spans.push(Span::styled(
                            format!(" ({url})"),
                            self.style().fg(self.palette.accent),
                        ));
                    }
                }
            }
            Event::Text(value) | Event::Html(value) | Event::InlineHtml(value) => {
                self.text(&value, self.style())
            }
            Event::Code(value) => {
                self.marker_only = false;
                self.spans.push(Span::styled(
                    value.into_string(),
                    self.style().fg(self.palette.accent),
                ));
            }
            Event::SoftBreak | Event::HardBreak => self.flush(true),
            Event::Rule => {
                self.flush(false);
                self.spans.push(Span::raw("—"));
                self.block_end();
            }
            Event::TaskListMarker(checked) => {
                self.spans
                    .push(Span::raw(if checked { "[x] " } else { "[ ] " }))
            }
            Event::Start(Tag::Table(alignments)) => {
                self.flush(false);
                self.table = Some(Table {
                    alignments,
                    ..Table::default()
                });
            }
            Event::End(TagEnd::TableCell) => {
                self.table
                    .as_mut()
                    .unwrap()
                    .row
                    .push(Line::from(std::mem::take(&mut self.spans)));
            }
            Event::End(TagEnd::TableHead | TagEnd::TableRow) => {
                self.table.as_mut().unwrap().finish_row()
            }
            Event::End(TagEnd::Table) => {
                let indent: usize = self
                    .containers
                    .iter()
                    .map(|container| match container {
                        Container::Quote => 2,
                        Container::List(index) => self.lists[*index].indent,
                    })
                    .sum();
                let width = self.width.saturating_sub(indent).max(1);
                for line in self.table.take().unwrap().lines(width) {
                    self.spans = line.spans;
                    self.flush(false);
                }
                self.gap = true;
            }
            _ => {}
        }
    }
}

pub(super) fn render(
    text: &str,
    palette: Palette,
    placeholder: bool,
    width: usize,
) -> Vec<Line<'static>> {
    let mut renderer = Renderer {
        palette,
        width,
        lines: Vec::new(),
        spans: Vec::new(),
        gap: false,
        bold: 0,
        italic: 0,
        strike: 0,
        code: false,
        containers: Vec::new(),
        marker_only: false,
        lists: Vec::new(),
        links: Vec::new(),
        table: None,
    };
    for event in Parser::new_ext(text, options()) {
        renderer.event(event);
    }
    renderer.flush(false);
    if placeholder && renderer.lines.is_empty() {
        renderer.lines.push(Line::default());
    }
    renderer.lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(text: &str, width: usize) -> Vec<Line<'static>> {
        render(text, Palette::new(false), false, width)
    }

    fn strings(lines: &[Line<'_>]) -> Vec<String> {
        lines.iter().map(ToString::to_string).collect()
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

    #[test]
    fn every_row_has_borders_including_empty_cells() {
        let lines = table("| A | B |\n| --- | --- |\n| x | |\n| | y |", 80);
        assert_eq!(
            strings(&lines),
            [
                "┌─────┬─────┐",
                "│ A   │ B   │",
                "├─────┼─────┤",
                "│ x   │     │",
                "├─────┼─────┤",
                "│     │ y   │",
                "└─────┴─────┘",
            ]
        );
    }

    #[test]
    fn header_only_table_has_closed_borders() {
        let lines = table("| H |\n| --- |", 80);
        assert_eq!(strings(&lines), ["┌─────┐", "│ H   │", "└─────┘"]);
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
            spans.iter().any(
                |span| span.content == "e" && span.style.fg == Some(Palette::new(false).accent)
            )
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
                super::super::render_line(
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
}
