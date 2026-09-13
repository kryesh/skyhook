//! Markdown event interpretation, styles, and source container metadata.
use super::super::super::tool_view::{self, Document, Section};
use super::super::Palette;
use super::tables::Table;
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};

#[derive(Default)]
pub(super) struct LineInfo {
    pub(super) prefix_spans: usize,
    pub(super) continuation: Line<'static>,
    pub(super) code: Option<usize>,
}

pub(super) struct Parsed {
    pub(super) lines: Vec<Line<'static>>,
    pub(super) info: std::collections::HashMap<usize, LineInfo>,
}

pub(in super::super) fn options() -> Options {
    Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS
}

#[derive(Default)]
struct List {
    next: Option<u64>,
    indent: usize,
    task_indent: usize,
    loose: bool,
    started: bool,
}

enum Container {
    Quote,
    List(usize),
}

struct Renderer<'a> {
    palette: Palette,
    width: usize,
    lines: Vec<Line<'static>>,
    info: std::collections::HashMap<usize, LineInfo>,
    marker_spans: usize,
    code_id: usize,
    code_rows: usize,
    spans: Vec<Span<'static>>,
    gap: bool,
    heading: usize,
    bold: usize,
    italic: usize,
    strike: usize,
    code: bool,
    fence: Option<(String, String)>,
    highlights: Option<&'a tool_view::HighlightCache>,
    containers: Vec<Container>,
    marker_only: bool,
    lists: Vec<List>,
    links: Vec<String>,
    table: Option<Table>,
}

impl Renderer<'_> {
    fn style(&self) -> Style {
        let content = self.palette.content;
        // Foreground precedence is independent of modifiers: code/link wins
        // over headings (including table headers), then strong, then prose.
        let foreground = if self.code {
            content.inline_code
        } else if !self.links.is_empty() {
            content.accent
        } else if self.heading > 0 {
            content.heading
        } else if self.bold > 0 {
            content.strong
        } else if self
            .containers
            .iter()
            .any(|c| matches!(c, Container::Quote))
        {
            content.quote
        } else {
            content.fg
        };
        let mut style = Style::default().fg(foreground);
        if self.bold > 0 || self.heading > 0 {
            style = style.add_modifier(Modifier::BOLD);
        }
        if self.italic > 0 {
            style = style.add_modifier(Modifier::ITALIC);
        }
        if self.strike > 0 {
            style = style.add_modifier(Modifier::CROSSED_OUT);
        }
        if !self.links.is_empty() {
            style = style.add_modifier(Modifier::UNDERLINED);
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
                Container::Quote => spans.push(Span::styled(
                    "│ ",
                    Style::default().fg(self.palette.content.muted),
                )),
                Container::List(index) => {
                    let indent = self.lists[*index].indent;
                    if indent > 0 {
                        spans.push(Span::raw(" ".repeat(indent)));
                    }
                }
            }
        }
        let prefix_spans = spans.len() + self.marker_spans;
        spans.append(&mut self.spans);
        let index = self.lines.len();
        self.lines.push(Line::from(spans));
        self.marker_spans = 0;
        self.marker_only = false;
        if let Some(list) = self.lists.last_mut()
            && list.indent == 0
        {
            list.indent = list
                .next
                .map_or(2, |next| format!("{}. ", next.saturating_sub(1)).len());
        }
        let mut continuation = Vec::new();
        for (depth, container) in self.containers.iter().enumerate() {
            match container {
                Container::Quote => continuation.push(Span::styled(
                    "│ ",
                    Style::default().fg(self.palette.content.muted),
                )),
                Container::List(index) => {
                    let list = &self.lists[*index];
                    // The checkbox hangs only the task's own paragraph. A
                    // descendant list/quote starts at the ordinary item indent,
                    // so its continuation must not inherit ancestor checkboxes.
                    let task_indent = if depth + 1 == self.containers.len() {
                        list.task_indent
                    } else {
                        0
                    };
                    continuation.push(Span::raw(" ".repeat(list.indent + task_indent)));
                }
            }
        }
        self.info.insert(
            index,
            LineInfo {
                prefix_spans,
                continuation: Line::from(continuation),
                code: self.code.then_some(self.code_id),
            },
        );
        if self.code {
            self.code_rows += 1;
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
                self.heading += 1;
            }
            Event::End(TagEnd::Strong) => self.bold = self.bold.saturating_sub(1),
            Event::End(TagEnd::Heading(_)) => {
                self.heading = self.heading.saturating_sub(1);
                self.block_end();
            }
            Event::Start(Tag::Emphasis) => self.italic += 1,
            Event::End(TagEnd::Emphasis) => self.italic = self.italic.saturating_sub(1),
            Event::Start(Tag::Strikethrough) => self.strike += 1,
            Event::End(TagEnd::Strikethrough) => self.strike = self.strike.saturating_sub(1),
            Event::Start(Tag::CodeBlock(kind)) => {
                if !self.marker_only {
                    self.flush(false);
                }
                self.code = true;
                self.code_id += 1;
                self.code_rows = 0;
                if let CodeBlockKind::Fenced(info) = kind
                    && let Some(language) = info.split_whitespace().next()
                {
                    self.fence = Some((language.to_owned(), String::new()));
                }
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some((language, source)) = self.fence.take()
                    && !source.is_empty()
                {
                    let trailing_newline = source.ends_with('\n');
                    let document = Document {
                        sections: vec![Section::Code {
                            source: source.as_str().into(),
                            language,
                            indent: 0,
                            gutters: Vec::new(),
                            role: tool_view::Role::Constant,
                        }],
                    };
                    let mut lines = document.lines(self.highlights);
                    // Markdown's text() flushes the preceding row at a final
                    // newline; it does not emit split()'s trailing empty row.
                    if trailing_newline {
                        lines.pop();
                    }
                    for line in lines {
                        self.spans.extend(line.spans);
                        self.flush(true);
                    }
                }
                if self.code_rows == 0 && self.spans.is_empty() {
                    self.flush(true);
                }
                self.block_end();
                self.code = false;
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
                    task_indent: 0,
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
                list.task_indent = 0;
                list.started = true;
                let marker = if let Some(next) = &mut list.next {
                    let marker = format!("{next}. ");
                    *next = next.saturating_add(1);
                    marker
                } else {
                    "• ".to_owned()
                };
                self.spans.push(Span::styled(
                    marker,
                    Style::default().fg(self.palette.content.primary),
                ));
                self.marker_spans = self.spans.len();
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
                let style = self.style();
                if let Some(url) = self.links.pop() {
                    // Autolinks already show the destination.
                    if !url.is_empty() && !self.spans.last().is_some_and(|span| span.content == url)
                    {
                        self.spans.push(Span::styled(format!(" ({url})"), style));
                    }
                }
            }
            Event::Text(value) | Event::Html(value) | Event::InlineHtml(value) => {
                if self.fence.as_ref().is_some_and(|(_, source)| {
                    value.len() > tool_view::MAX_SECTION.saturating_sub(source.len())
                }) {
                    // Oversized fences stay on the original streaming text
                    // path, without allocating/hashing a second huge source.
                    let (_, source) = self.fence.take().unwrap();
                    self.text(&source, self.style());
                }
                if let Some((_, source)) = &mut self.fence {
                    source.push_str(&value);
                } else {
                    self.text(&value, self.style());
                }
            }
            Event::Code(value) => {
                self.marker_only = false;
                self.spans.push(Span::styled(
                    value.into_string(),
                    self.style().fg(self.palette.content.inline_code),
                ));
            }
            Event::SoftBreak | Event::HardBreak => self.flush(true),
            Event::Rule => {
                self.flush(false);
                self.spans.push(Span::raw("—"));
                self.block_end();
            }
            Event::TaskListMarker(checked) => {
                self.spans.push(Span::styled(
                    if checked { "[x] " } else { "[ ] " },
                    Style::default().fg(self.palette.content.primary),
                ));
                self.marker_spans += 1;
                if let Some(list) = self.lists.last_mut() {
                    list.task_indent = 4;
                }
            }
            Event::Start(Tag::Table(alignments)) => {
                self.flush(false);
                self.table = Some(Table::new(alignments));
            }
            Event::End(TagEnd::TableCell) => {
                self.table
                    .as_mut()
                    .unwrap()
                    .push_cell(Line::from(std::mem::take(&mut self.spans)));
            }
            Event::Start(Tag::TableHead) => self.heading += 1,
            Event::End(TagEnd::TableHead) => {
                self.heading = self.heading.saturating_sub(1);
                self.table.as_mut().unwrap().finish_row()
            }
            Event::End(TagEnd::TableRow) => self.table.as_mut().unwrap().finish_row(),
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

pub(super) fn parse(
    text: &str,
    palette: Palette,
    placeholder: bool,
    width: usize,
    cache: Option<&tool_view::HighlightCache>,
) -> Parsed {
    let mut renderer = Renderer {
        palette,
        width,
        lines: Vec::new(),
        info: std::collections::HashMap::new(),
        marker_spans: 0,
        code_id: 0,
        code_rows: 0,
        spans: Vec::new(),
        gap: false,
        heading: 0,
        bold: 0,
        italic: 0,
        strike: 0,
        code: false,
        fence: None,
        highlights: cache,
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
    Parsed {
        lines: renderer.lines,
        info: renderer.info,
    }
}

#[cfg(test)]
pub(in super::super) mod tests {
    use super::*;

    pub(in super::super::super) fn render(
        text: &str,
        palette: Palette,
        placeholder: bool,
        width: usize,
    ) -> Vec<Line<'static>> {
        super::super::render_highlighted(text, palette, placeholder, width, None)
    }

    pub(in super::super::super) fn render_highlighted(
        text: &str,
        palette: Palette,
        placeholder: bool,
        width: usize,
        cache: Option<&tool_view::HighlightCache>,
    ) -> Vec<Line<'static>> {
        parse(text, palette, placeholder, width, cache).lines
    }

    fn rendered(text: &str, width: usize) -> Vec<Line<'static>> {
        render(text, Palette::new(), false, width)
    }

    fn strings(lines: &[Line<'_>]) -> Vec<String> {
        lines.iter().map(ToString::to_string).collect()
    }

    fn span_style(lines: &[Line<'_>], text: &str) -> Style {
        lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.content == text)
            .unwrap_or_else(|| panic!("missing span {text:?} in {:?}", strings(lines)))
            .style
    }
    #[test]
    fn nested_foregrounds_preserve_heading_and_inline_modifiers() {
        let p = Palette::new();
        let lines = render(
            "# head **strong** *italic* [link](https://example.com) **[*`code`*](https://code.example)**\n\n**bold *emphasis* [linked](https://strong.example) `inline`**\n\n*neutral* plain",
            p,
            false,
            200,
        );
        for text in ["strong", "italic"] {
            let style = span_style(&lines, text);
            assert_eq!(style.fg, Some(p.content.heading));
            assert!(style.add_modifier.contains(Modifier::BOLD));
        }
        assert!(
            span_style(&lines, "italic")
                .add_modifier
                .contains(Modifier::ITALIC)
        );
        for text in [
            "link",
            " (https://example.com)",
            "linked",
            " (https://strong.example)",
        ] {
            let style = span_style(&lines, text);
            assert_eq!(style.fg, Some(p.content.accent));
            assert!(
                style
                    .add_modifier
                    .contains(Modifier::BOLD | Modifier::UNDERLINED)
            );
        }
        let code = span_style(&lines, "code");
        assert_eq!(code.fg, Some(p.content.inline_code));
        assert!(
            code.add_modifier
                .contains(Modifier::BOLD | Modifier::ITALIC | Modifier::UNDERLINED)
        );
        let emphasis = span_style(&lines, "emphasis");
        assert_eq!(emphasis.fg, Some(p.content.strong));
        assert!(
            emphasis
                .add_modifier
                .contains(Modifier::BOLD | Modifier::ITALIC)
        );
        let inline = span_style(&lines, "inline");
        assert_eq!(inline.fg, Some(p.content.inline_code));
        assert!(inline.add_modifier.contains(Modifier::BOLD));
        let neutral = span_style(&lines, "neutral");
        assert_eq!(neutral.fg, Some(p.content.fg));
        assert_eq!(neutral.add_modifier, Modifier::ITALIC);
    }

    #[test]
    fn quote_and_list_markers_use_content_roles_without_changing_text() {
        let p = Palette::new();
        let lines = render(
            "> quoted *quiet*\n\n- bullet\n- [x] done\n\n3. numbered",
            p,
            false,
            80,
        );
        assert_eq!(
            strings(&lines),
            [
                "│ quoted quiet",
                "",
                "• bullet",
                "• [x] done",
                "",
                "3. numbered"
            ]
        );
        assert_eq!(span_style(&lines, "│ ").fg, Some(p.content.muted));
        assert_eq!(span_style(&lines, "quoted ").fg, Some(p.content.quote));
        let quiet = span_style(&lines, "quiet");
        assert_eq!(quiet.fg, Some(p.content.quote));
        assert_eq!(quiet.add_modifier, Modifier::ITALIC);
        for marker in ["• ", "[x] ", "3. "] {
            assert_eq!(span_style(&lines, marker).fg, Some(p.content.primary));
        }
        assert_eq!(span_style(&lines, "bullet").fg, Some(p.content.fg));
    }

    #[test]
    fn fenced_code_fallback_preserves_rows_whitespace_and_container_prefixes() {
        for body in ["", "\n", "one\n", "one  \n\n", "one\n\ntwo  \n"] {
            let plain = format!("before\n\n```\n{body}```\n\nafter");
            let labelled = plain.replacen("```\n", "```rust extra-info\n", 1);
            for prefix in ["", "> "] {
                let quote = |source: &str| {
                    source
                        .lines()
                        .map(|line| format!("{prefix}{line}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                assert_eq!(
                    strings(&rendered(&quote(&labelled), 80)),
                    strings(&rendered(&quote(&plain), 80))
                );
            }
        }
        let lines = rendered("- ```rust\n  let x = 1;  \n\n  ```", 80);
        assert_eq!(strings(&lines), ["• let x = 1;  ", "  "]);
    }

    #[test]
    fn oversized_fences_keep_plain_code_text_and_style() {
        let body = format!("small\n{}  \n\nlast\n", "x".repeat(tool_view::MAX_SECTION));
        let named = format!("```rust\n{body}```");
        let unnamed = format!("```\n{body}```");
        let p = Palette::new();
        let named = render(&named, p, false, 80);
        let unnamed = render(&unnamed, p, false, 80);
        assert_eq!(named, unnamed);
    }

    #[test]
    fn named_code_fallback_uses_the_code_role() {
        let p = Palette::new();
        let lines = render("```not-a-language\nplain\n```", p, false, 80);
        assert_eq!(strings(&lines), ["plain"]);
        assert_eq!(span_style(&lines, "plain").fg, Some(p.content.inline_code));
    }
    #[test]
    fn table_headers_have_heading_precedence_even_in_borderless_fallback() {
        let p = Palette::new();
        for width in [1, 80] {
            let lines = render(
                "| **H** | *I* | [L](u) | `C` |\n| --- | --- | --- | --- |\n| body | **B** | plain | plain |",
                p,
                false,
                width,
            );
            for text in ["H", "I"] {
                let style = span_style(&lines, text);
                assert_eq!(style.fg, Some(p.content.heading));
                assert!(style.add_modifier.contains(Modifier::BOLD));
            }
            let link = span_style(&lines, "L");
            assert_eq!(link.fg, Some(p.content.accent));
            assert!(
                link.add_modifier
                    .contains(Modifier::BOLD | Modifier::UNDERLINED)
            );
            let code = span_style(&lines, "C");
            assert_eq!(code.fg, Some(p.content.inline_code));
            assert!(code.add_modifier.contains(Modifier::BOLD));
            assert_eq!(span_style(&lines, "B").fg, Some(p.content.strong));
        }
    }
}
