//! Markdown shared by saved responses, reasoning, and live streaming fragments.
//! Soft breaks intentionally remain newlines in chat. Block boundaries add one
//! blank row, while tight list items and code-block lines remain consecutive.
use super::Palette;
use pulldown_cmark::{Alignment, Event, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};

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

    fn lines(self) -> Vec<Line<'static>> {
        let mut widths = vec![3; self.alignments.len()];
        for row in &self.rows {
            for (width, cell) in widths.iter_mut().zip(row) {
                *width = (*width).max(cell.width());
            }
        }
        let mut lines = Vec::new();
        for (index, row) in self.rows.into_iter().enumerate() {
            let mut spans = vec![Span::raw("│ ")];
            for (column, mut cell) in row.into_iter().enumerate() {
                let width = widths[column];
                let padding = width.saturating_sub(cell.width());
                let left = match self.alignments[column] {
                    Alignment::Right => padding,
                    Alignment::Center => padding / 2,
                    _ => 0,
                };
                spans.push(Span::raw(" ".repeat(left)));
                if index == 0 {
                    for span in &mut cell.spans {
                        span.style = span.style.add_modifier(Modifier::BOLD);
                    }
                }
                spans.extend(cell.spans);
                spans.push(Span::raw(" ".repeat(padding - left)));
                spans.push(Span::raw(" │ "));
            }
            // Avoid a trailing space beyond the final border.
            if let Some(last) = spans.last_mut() {
                *last = Span::raw(" │");
            }
            lines.push(Line::from(spans));
            if index == 0 {
                lines.push(Line::from(format!(
                    "├{}┤",
                    widths
                        .iter()
                        .map(|width| "─".repeat(width + 2))
                        .collect::<Vec<_>>()
                        .join("┼")
                )));
            }
        }
        lines
    }
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
                for line in self.table.take().unwrap().lines() {
                    self.spans = line.spans;
                    self.flush(false);
                }
                self.gap = true;
            }
            _ => {}
        }
    }
}

pub(super) fn render(text: &str, palette: Palette, placeholder: bool) -> Vec<Line<'static>> {
    let mut renderer = Renderer {
        palette,
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

    fn lines(text: &str) -> Vec<String> {
        render(text, Palette::new(false), true)
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    #[test]
    fn keeps_line_breaks_and_paragraph_spacing_without_trailing_blank_rows() {
        assert_eq!(
            lines("first\nsecond\n\nthird\n\n\nlast\n"),
            ["first", "second", "", "third", "", "last"]
        );
        assert_eq!(lines("one  \ntwo\\\nthree"), ["one", "two", "three"]);
        assert_eq!(
            lines("# Heading\n\nbody\n\n---\n\nend"),
            ["Heading", "", "body", "", "—", "", "end"]
        );
    }

    #[test]
    fn composes_inline_styles_and_does_not_leak_them() {
        for light in [false, true] {
            let rows = render(
                "**bold *both* `code`** *italic* ~~gone~~ plain",
                Palette::new(light),
                true,
            );
            let style = |text: &str| {
                rows[0]
                    .spans
                    .iter()
                    .find(|span| span.content == text)
                    .unwrap()
                    .style
            };
            assert!(style("bold ").add_modifier.contains(Modifier::BOLD));
            assert!(
                style("both")
                    .add_modifier
                    .contains(Modifier::BOLD | Modifier::ITALIC)
            );
            assert!(style("code").add_modifier.contains(Modifier::BOLD));
            assert!(style("italic").add_modifier.contains(Modifier::ITALIC));
            assert!(style("gone").add_modifier.contains(Modifier::CROSSED_OUT));
            assert_eq!(style(" plain"), Style::default());
        }
    }

    #[test]
    fn renders_lists_quotes_links_and_literal_code() {
        assert_eq!(
            lines("3. first\n4. second\n   - nested\n     continuation\n5. final"),
            [
                "3. first",
                "4. second",
                "   • nested",
                "     continuation",
                "5. final"
            ]
        );
        assert_eq!(
            lines("- [x] done\n- [ ] todo"),
            ["• [x] done", "• [ ] todo"]
        );
        assert_eq!(
            lines("> quote\n> next\n\nend"),
            ["│ quote", "│ next", "", "end"]
        );
        assert_eq!(
            lines("[label](https://example.com) <https://example.com>"),
            ["label (https://example.com) https://example.com"]
        );
        assert_eq!(
            lines("before\n\n```rust\na **literal**\n\n  b\n```\n\nafter"),
            ["before", "", "a **literal**", "", "  b", "", "after"]
        );
    }

    #[test]
    fn loose_items_keep_paragraphs_and_nested_quotes_keep_their_prefix() {
        assert_eq!(
            lines("- first\n\n  second\n\n- third"),
            ["• first", "", "  second", "", "• third"]
        );
        assert_eq!(
            lines("- first\n  - nested\n\n    paragraph\n- last"),
            ["• first", "  • nested", "", "    paragraph", "• last"]
        );
        assert_eq!(lines("> outer\n>\n> > inner"), ["│ outer", "", "│ │ inner"]);
        assert_eq!(
            lines("    indented\n      code\n\nafter"),
            ["indented", "  code", "", "after"]
        );
    }

    #[test]
    fn tight_list_blocks_do_not_merge_and_container_order_is_preserved() {
        assert_eq!(
            lines("- foo\n  ```\n  bar\n  ```\n- baz"),
            ["• foo", "  bar", "• baz"]
        );
        assert_eq!(
            lines("- foo\n  # head\n- baz"),
            ["• foo", "  head", "• baz"]
        );
        assert_eq!(lines("- # heading\n- next"), ["• heading", "• next"]);
        assert_eq!(
            lines("- parent\n  - child\n\n  more\n\n- next"),
            ["• parent", "  • child", "", "  more", "", "• next"]
        );
        assert_eq!(
            lines("- foo\n  > quote\n- next"),
            ["• foo", "  │ quote", "• next"]
        );
        assert_eq!(lines("> - foo\n>   > quote"), ["│ • foo", "│   │ quote"]);
        assert_eq!(lines("paragraph\n\n- item"), ["paragraph", "", "• item"]);
    }

    #[test]
    fn tables_handle_empty_cells_alignment_and_escaped_pipes() {
        assert_eq!(
            lines("| A | B | C |\n| --- | :---: | ---: |\n| a\\|b | x | |"),
            [
                "│ A   │  B  │   C │",
                "├─────┼─────┼─────┤",
                "│ a|b │  x  │     │"
            ]
        );
    }

    #[test]
    fn tables_have_distinct_headers_aligned_unicode_cells_and_inline_styles() {
        let text = "| Name | Value |\n| :--- | ---: |\n| **界** | 2 |\n| longer | *123* |";
        assert_eq!(
            lines(text),
            [
                "│ Name   │ Value │",
                "├────────┼───────┤",
                "│ 界     │     2 │",
                "│ longer │   123 │"
            ]
        );
        let rows = render(text, Palette::new(false), true);
        assert!(
            rows[0]
                .spans
                .iter()
                .find(|span| span.content == "Name")
                .unwrap()
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert!(
            rows[2]
                .spans
                .iter()
                .find(|span| span.content == "界")
                .unwrap()
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert!(
            rows[3]
                .spans
                .iter()
                .find(|span| span.content == "123")
                .unwrap()
                .style
                .add_modifier
                .contains(Modifier::ITALIC)
        );
    }
}
