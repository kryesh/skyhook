//! Markdown event interpretation, styles, and source container metadata.
use super::super::super::tool_view::{self, Document, Section};
use super::super::Palette;
use super::tables::Table;
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use std::ops::Range;

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct LineInfo {
    pub(super) prefix_spans: usize,
    pub(super) continuation: Line<'static>,
    pub(super) code: Option<usize>,
}

/// A rendered logical line travels with the metadata that describes it.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct ParsedLine {
    pub(super) line: Line<'static>,
    pub(super) info: LineInfo,
}

impl From<Line<'static>> for ParsedLine {
    fn from(line: Line<'static>) -> Self {
        Self {
            line,
            info: LineInfo::default(),
        }
    }
}

pub(super) struct Parsed {
    pub(super) lines: Vec<ParsedLine>,
}

pub(in super::super) fn options() -> Options {
    Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS
}

/// Pulldown emits an end event even for an unfinished fence. Its source range
/// includes a real closing marker, but text events consume invalid candidates.
/// Track the un-emitted tail so EOF/container closure cannot enable highlighting.
pub(in super::super) struct Fence {
    marker: u8,
    tail: Range<usize>,
}

impl Fence {
    pub(in super::super) fn new(text: &str, range: Range<usize>) -> Self {
        let body = text[range.clone()]
            .find('\n')
            .map_or(range.end, |end| range.start + end + 1);
        Self {
            marker: text.as_bytes()[range.start],
            tail: body..range.end,
        }
    }

    pub(in super::super) fn text(&mut self, end: usize) {
        self.tail.start = end;
    }

    pub(in super::super) fn closed(self, text: &str) -> bool {
        text.as_bytes()[self.tail].contains(&self.marker)
    }
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
    List(List),
}

enum CodeSource {
    Literal,
    Named {
        language: String,
        source: String,
        fence: Fence,
    },
}

struct ActiveCodeBlock {
    id: usize,
    rows: usize,
    source: CodeSource,
}

struct Renderer<'a> {
    input: &'a str,
    palette: Palette,
    width: usize,
    lines: Vec<ParsedLine>,
    marker_spans: usize,
    next_code_id: usize,
    spans: Vec<Span<'static>>,
    gap: bool,
    heading: usize,
    bold: usize,
    italic: usize,
    strike: usize,
    code: Option<ActiveCodeBlock>,
    highlights: Option<&'a tool_view::HighlightCache>,
    containers: Vec<Container>,
    marker_only: bool,
    links: Vec<String>,
    table: Option<Table>,
}

impl Renderer<'_> {
    fn list(&self) -> Option<&List> {
        self.containers
            .iter()
            .rev()
            .find_map(|container| match container {
                Container::List(list) => Some(list),
                Container::Quote => None,
            })
    }

    fn list_mut(&mut self) -> Option<&mut List> {
        self.containers
            .iter_mut()
            .rev()
            .find_map(|container| match container {
                Container::List(list) => Some(list),
                Container::Quote => None,
            })
    }

    fn style(&self) -> Style {
        let content = self.palette.content;
        // Foreground precedence is independent of modifiers: code/link wins
        // over headings (including table headers), then strong, then prose.
        let foreground = if self.code.is_some() {
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
            self.lines.push(ParsedLine::default());
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
                Container::List(list) => {
                    let indent = list.indent;
                    if indent > 0 {
                        spans.push(Span::raw(" ".repeat(indent)));
                    }
                }
            }
        }
        let prefix_spans = spans.len() + self.marker_spans;
        spans.append(&mut self.spans);
        self.marker_spans = 0;
        self.marker_only = false;
        if let Some(list) = self.list_mut()
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
                Container::List(list) => {
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
        self.lines.push(ParsedLine {
            line: Line::from(spans),
            info: LineInfo {
                prefix_spans,
                continuation: Line::from(continuation),
                code: self.code.as_ref().map(|code| code.id),
            },
        });
        if let Some(code) = &mut self.code {
            code.rows += 1;
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

    fn event(&mut self, event: Event<'_>, range: Range<usize>) {
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
                self.next_code_id += 1;
                let source = match kind {
                    CodeBlockKind::Fenced(info) => {
                        info.split_whitespace()
                            .next()
                            .map_or(CodeSource::Literal, |language| CodeSource::Named {
                                language: language.to_owned(),
                                source: String::new(),
                                fence: Fence::new(self.input, range),
                            })
                    }
                    CodeBlockKind::Indented => CodeSource::Literal,
                };
                self.code = Some(ActiveCodeBlock {
                    id: self.next_code_id,
                    rows: 0,
                    source,
                });
            }
            Event::End(TagEnd::CodeBlock) => {
                let code = self.code.as_mut().expect("end inside code block");
                if let CodeSource::Named {
                    language,
                    source,
                    fence,
                } = std::mem::replace(&mut code.source, CodeSource::Literal)
                    && !source.is_empty()
                {
                    let trailing_newline = source.ends_with('\n');
                    let document = Document {
                        sections: vec![Section::Code {
                            source: source.as_str().into(),
                            language,
                            indent: 0,
                            gutters: Default::default(),
                            role: tool_view::Role::Constant,
                        }],
                    };
                    let highlights = if fence.closed(self.input) {
                        self.highlights
                    } else {
                        None
                    };
                    let mut lines = document.lines(highlights);
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
                if self.code.as_ref().unwrap().rows == 0 && self.spans.is_empty() {
                    self.flush(true);
                }
                self.block_end();
                self.code = None;
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
                if self.list().is_some() {
                    self.gap = false;
                }
                self.containers.push(Container::List(List {
                    next,
                    indent: 0,
                    task_indent: 0,
                    loose: false,
                    started: false,
                }));
            }
            Event::End(TagEnd::List(_)) => {
                self.flush(false);
                self.containers.pop();
                self.gap = true;
            }
            Event::Start(Tag::Item) => {
                if self.list().is_some_and(|list| list.started && !list.loose) {
                    self.gap = false;
                }
                self.begin();
                self.marker_only = true;
                let list = self.list_mut().expect("item inside list");
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
                    self.list_mut().unwrap().loose = true;
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
                let overflow = self.code.as_ref().is_some_and(|code| match &code.source {
                    CodeSource::Named { source, .. } => {
                        value.len() > tool_view::MAX_SECTION.saturating_sub(source.len())
                    }
                    CodeSource::Literal => false,
                });
                if overflow {
                    // Oversized fences stay on the original streaming text
                    // path, without allocating/hashing a second huge source.
                    let code = self.code.as_mut().unwrap();
                    if let CodeSource::Named { source, .. } =
                        std::mem::replace(&mut code.source, CodeSource::Literal)
                    {
                        self.text(&source, self.style());
                    }
                }
                if let Some(ActiveCodeBlock {
                    source: CodeSource::Named { source, fence, .. },
                    ..
                }) = &mut self.code
                {
                    source.push_str(&value);
                    fence.text(range.end);
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
                if let Some(list) = self.list_mut() {
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
                        Container::List(list) => list.indent,
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
        input: text,
        palette,
        width,
        lines: Vec::new(),
        marker_spans: 0,
        next_code_id: 0,
        spans: Vec::new(),
        gap: false,
        heading: 0,
        bold: 0,
        italic: 0,
        strike: 0,
        code: None,
        highlights: cache,
        containers: Vec::new(),
        marker_only: false,
        links: Vec::new(),
        table: None,
    };
    for (event, range) in Parser::new_ext(text, options()).into_offset_iter() {
        renderer.event(event, range);
    }
    renderer.flush(false);
    if placeholder && renderer.lines.is_empty() {
        renderer.lines.push(ParsedLine::default());
    }
    Parsed {
        lines: renderer.lines,
    }
}

#[cfg(test)]
pub(in super::super) mod tests {
    use super::*;
    use ratatui::style::Color;

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
        let lines = parse(text, palette, placeholder, width, cache).lines;
        lines.into_iter().map(|parsed| parsed.line).collect()
    }

    fn rendered(text: &str, width: usize) -> Vec<Line<'static>> {
        render(text, Palette::new(), false, width)
    }

    fn strings(lines: &[Line<'_>]) -> Vec<String> {
        lines.iter().map(ToString::to_string).collect()
    }

    fn span_style(lines: &[Line<'_>], text: &str) -> Style {
        let mut spans = lines.iter().flat_map(|line| &line.spans);
        let span = spans.find(|span| span.content == text);
        span.unwrap_or_else(|| panic!("missing span {text:?} in {:?}", strings(lines)))
            .style
    }

    /// Assert a span's foreground and that it carries at least `modifiers`.
    fn assert_style(lines: &[Line<'_>], text: &str, fg: Color, modifiers: Modifier) {
        let style = span_style(lines, text);
        assert_eq!(style.fg, Some(fg), "{text}");
        assert!(style.add_modifier.contains(modifiers), "{text}: {style:?}");
    }

    #[test]
    fn empty_and_multiple_code_blocks_keep_distinct_ids() {
        let source = "```rust\n```\n\nprose\n\n```\n\n```\n\n    indented\n\n```rust\nlast\n```";
        let parsed = parse(source, Palette::new(), false, 80, None);
        let lines: Vec<_> = parsed
            .lines
            .iter()
            .map(|parsed| (parsed.info.code, parsed.line.to_string()))
            .collect();
        let code: Vec<_> = lines
            .iter()
            .filter_map(|(id, text)| id.map(|id| (id, text.as_str())))
            .collect();
        assert_eq!(code, [(1, ""), (2, ""), (3, "indented"), (4, "last")]);
        let mut prose = lines.iter().filter(|(_, text)| text == "prose");
        assert!(prose.all(|(id, _)| id.is_none()));
    }

    #[test]
    fn nested_foregrounds_preserve_heading_and_inline_modifiers() {
        let p = Palette::new();
        let lines = rendered(
            "# head **strong** *italic* [link](https://example.com) **[*`code`*](https://code.example)**\n\n**bold *emphasis* [linked](https://strong.example) `inline`**\n\n*neutral* plain",
            200,
        );
        let (bold, italic, underlined) = (Modifier::BOLD, Modifier::ITALIC, Modifier::UNDERLINED);
        let content = p.content;
        for (text, fg, modifiers) in [
            ("strong", content.heading, bold),
            ("italic", content.heading, bold | italic),
            ("link", content.accent, bold | underlined),
            (" (https://example.com)", content.accent, bold | underlined),
            ("linked", content.accent, bold | underlined),
            (
                " (https://strong.example)",
                content.accent,
                bold | underlined,
            ),
            ("code", content.inline_code, bold | italic | underlined),
            ("emphasis", content.strong, bold | italic),
            ("inline", content.inline_code, bold),
        ] {
            assert_style(&lines, text, fg, modifiers);
        }
        let neutral = span_style(&lines, "neutral");
        assert_eq!(
            (neutral.fg, neutral.add_modifier),
            (Some(content.fg), italic)
        );
    }

    #[test]
    fn quote_and_list_markers_use_content_roles_without_changing_text() {
        let p = Palette::new().content;
        let lines = rendered(
            "> quoted *quiet*\n\n- bullet\n- [x] done\n\n3. numbered",
            80,
        );
        let expected = [
            "│ quoted quiet",
            "",
            "• bullet",
            "• [x] done",
            "",
            "3. numbered",
        ];
        assert_eq!(strings(&lines), expected);
        for (text, fg) in [
            ("│ ", p.muted),
            ("quoted ", p.quote),
            ("• ", p.primary),
            ("[x] ", p.primary),
            ("3. ", p.primary),
            ("bullet", p.fg),
        ] {
            assert_eq!(span_style(&lines, text).fg, Some(fg), "{text}");
        }
        let quiet = span_style(&lines, "quiet");
        assert_eq!(
            (quiet.fg, quiet.add_modifier),
            (Some(p.quote), Modifier::ITALIC)
        );
    }

    #[test]
    fn fenced_code_fallback_preserves_rows_whitespace_and_container_prefixes() {
        for body in ["", "\n", "one\n", "one  \n\n", "one\n\ntwo  \n"] {
            let plain = format!("before\n\n```\n{body}```\n\nafter");
            let labelled = plain.replacen("```\n", "```rust extra-info\n", 1);
            for prefix in ["", "> "] {
                let quote = |source: &str| {
                    let lines = source.lines().map(|line| format!("{prefix}{line}"));
                    strings(&rendered(&lines.collect::<Vec<_>>().join("\n"), 80))
                };
                assert_eq!(quote(&labelled), quote(&plain));
            }
        }
        let lines = rendered("- ```rust\n  let x = 1;  \n\n  ```", 80);
        assert_eq!(strings(&lines), ["• let x = 1;  ", "  "]);
    }

    #[test]
    fn open_fences_ignore_cached_highlights_until_closed() {
        let open = "```rust\nlet answer = 42;\n";
        let closed = format!("{open}```");
        let mut fences = crate::tui::render::code::Fences::default();
        fences.update(&closed);
        let mut cache = tool_view::HighlightCache::default();
        cache.wait(&fences.document);
        let render = |text| render_highlighted(text, Palette::new(), false, 80, Some(&cache));
        let plain = rendered(open, 80);
        assert_eq!(render(open), plain);
        assert_ne!(render(&closed), plain);
    }

    #[test]
    fn named_code_fallback_keeps_plain_text_and_the_code_role() {
        let body = format!("small\n{}  \n\nlast\n", "x".repeat(tool_view::MAX_SECTION));
        let oversized = rendered(&format!("```rust\n{body}```"), 80);
        assert_eq!(oversized, rendered(&format!("```\n{body}```"), 80));
        let lines = rendered("```not-a-language\nplain\n```", 80);
        assert_eq!(strings(&lines), ["plain"]);
        assert_eq!(
            span_style(&lines, "plain").fg,
            Some(Palette::new().content.inline_code)
        );
    }

    #[test]
    fn table_headers_have_heading_precedence_even_in_borderless_fallback() {
        let p = Palette::new().content;
        let (bold, underlined) = (Modifier::BOLD, Modifier::UNDERLINED);
        for width in [1, 80] {
            let lines = rendered(
                "| **H** | *I* | [L](u) | `C` |\n| --- | --- | --- | --- |\n| body | **B** | plain | plain |",
                width,
            );
            for (text, fg, modifiers) in [
                ("H", p.heading, bold),
                ("I", p.heading, bold),
                ("L", p.accent, bold | underlined),
                ("C", p.inline_code, bold),
                ("B", p.strong, Modifier::empty()),
            ] {
                assert_style(&lines, text, fg, modifiers);
            }
        }
    }
}
