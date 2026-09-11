//! Markdown shared by saved responses, reasoning, and live streaming fragments.
//! Soft breaks intentionally remain newlines in chat. Block boundaries add one
//! blank row, while tight list items and code-block lines remain consecutive.
use super::super::tool_view::{self, Document, Section};
use super::{Palette, wrap_words};
use pulldown_cmark::{Alignment, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Source and decoration stay separate: wrapping and code padding must not add
/// bytes/newlines to selection. Prefix boundaries are recorded by the parser,
/// never inferred from colors or whitespace in the rendered spans.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct RowLayout {
    pub prefix: Line<'static>,
    pub source_prefix: usize,
    pub source_prefix_width: usize,
    pub code: Option<CodeRow>,
    pub decorative: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CodeRow {
    pub indent: usize,
    pub width: usize,
    pub padding: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct LayoutLine {
    pub line: Line<'static>,
    pub continued: bool,
    pub layout: RowLayout,
}

impl From<(Line<'static>, bool)> for LayoutLine {
    fn from((line, continued): (Line<'static>, bool)) -> Self {
        Self {
            line,
            continued,
            layout: RowLayout::default(),
        }
    }
}

#[derive(Default)]
struct LineInfo {
    prefix_spans: usize,
    continuation: Line<'static>,
    code: Option<usize>,
}

struct Parsed {
    lines: Vec<Line<'static>>,
    info: std::collections::HashMap<usize, LineInfo>,
}

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
                    let mut lines = document.lines(self.highlights, self.palette.content.light);
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

#[cfg(test)]
pub(super) fn render(
    text: &str,
    palette: Palette,
    placeholder: bool,
    width: usize,
) -> Vec<Line<'static>> {
    render_highlighted(text, palette, placeholder, width, None)
}

#[cfg(test)]
pub(super) fn render_highlighted(
    text: &str,
    palette: Palette,
    placeholder: bool,
    width: usize,
    cache: Option<&tool_view::HighlightCache>,
) -> Vec<Line<'static>> {
    parse(text, palette, placeholder, width, cache).lines
}

fn parse(
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

/// Wrap source inside its paragraph/container, then add geometry-only code
/// padding. Measuring the entire code block keeps all its rows the same width.
pub(super) fn layout_highlighted(
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
        if super::stream::starts_with_table(text) {
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
        super::wrap_line(body, budget)
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
    use super::*;

    /// Exercise the paint path: source strings alone cannot reveal a gap
    /// inserted by source_prefix_width or decorative continuation prefixes.
    fn painted_markdown(text: &str, width: u16, p: Palette) -> Vec<String> {
        use super::super::{Row, RowBlocks, Surface, TextPosition, render_row_line, selected_text};
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
        for light in [false, true] {
            for width in [12, 80] {
                let p = Palette::new(light);
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
                        painted_markdown(text, width, p),
                        expected,
                        "light={light}, width={width}, source={text:?}",
                    );
                }
            }
        }
    }

    #[test]
    fn task_descendant_wrapping_uses_only_its_own_container_prefix() {
        for light in [false, true] {
            for width in [12, 80] {
                let p = Palette::new(light);
                assert_eq!(
                    painted_markdown("- [ ] task\n  - child one", width, p),
                    if width == 12 {
                        vec!["• [ ] task", "  • child", "    one"]
                    } else {
                        vec!["• [ ] task", "  • child one"]
                    },
                );
                assert_eq!(
                    painted_markdown("- [ ] task\n  > quoted one", width, p),
                    if width == 12 {
                        vec!["• [ ] task", "  │ quoted", "  │ one"]
                    } else {
                        vec!["• [ ] task", "  │ quoted one"]
                    },
                );
                assert_eq!(
                    painted_markdown("- [ ] task\n  - [x] one two", width, p),
                    if width == 12 {
                        vec!["• [ ] task", "  • [x] one", "        two"]
                    } else {
                        vec!["• [ ] task", "  • [x] one two"]
                    },
                    "nested tasks keep their own checkbox hanging width",
                );
                assert_eq!(
                    painted_markdown("- [ ] alpha beta\n  gamma", width, p),
                    if width == 12 {
                        vec!["• [ ] alpha", "      beta", "      gamma"]
                    } else {
                        vec!["• [ ] alpha beta", "      gamma"]
                    },
                    "the task paragraph still hangs beneath its checkbox",
                );
            }
        }
    }

    fn table(text: &str, width: usize) -> Vec<Line<'static>> {
        render(text, Palette::new(false), false, width)
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
        for light in [false, true] {
            let p = Palette::new(light);
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
    }

    #[test]
    fn quote_and_list_markers_use_content_roles_without_changing_text() {
        for light in [false, true] {
            let p = Palette::new(light);
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
    }

    #[test]
    fn table_headers_have_heading_precedence_even_in_borderless_fallback() {
        for light in [false, true] {
            let p = Palette::new(light);
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
                    strings(&table(&quote(&labelled), 80)),
                    strings(&table(&quote(&plain), 80))
                );
            }
        }
        let lines = table("- ```rust\n  let x = 1;  \n\n  ```", 80);
        assert_eq!(strings(&lines), ["• let x = 1;  ", "  "]);
    }

    #[test]
    fn oversized_fences_keep_plain_code_text_and_style() {
        let body = format!("small\n{}  \n\nlast\n", "x".repeat(tool_view::MAX_SECTION));
        let named = format!("```rust\n{body}```");
        let unnamed = format!("```\n{body}```");
        let p = Palette::new(false);
        let named = render(&named, p, false, 80);
        let unnamed = render(&unnamed, p, false, 80);
        assert_eq!(named, unnamed);
    }

    #[test]
    fn named_code_fallback_uses_the_code_role() {
        for light in [false, true] {
            let p = Palette::new(light);
            let lines = render("```not-a-language\nplain\n```", p, false, 80);
            assert_eq!(strings(&lines), ["plain"]);
            assert_eq!(span_style(&lines, "plain").fg, Some(p.content.inline_code));
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
        assert!(spans.iter().any(|span| span.content == "e"
            && span.style.fg == Some(Palette::new(false).content.inline_code)));
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
