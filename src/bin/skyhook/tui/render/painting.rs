//! Theme palette, terminal cell painting, and row decoration.

use super::*;

#[derive(Clone, Copy)]
pub struct Palette {
    pub content: super::super::theme::ContentTheme,
    pub base: Color,
    pub panel: Color,
    pub input: Color,
    pub agent: Color,
    pub user: Color,
    pub fg: Color,
    pub muted: Color,
    pub selected: Color,
    pub accent: Color,
    pub warning: Color,
}
impl Palette {
    pub fn new(light: bool) -> Self {
        if light {
            Self {
                content: super::super::theme::ContentTheme::new(light),
                base: Color::Rgb(245, 245, 245),
                panel: Color::Rgb(234, 234, 234),
                input: Color::Rgb(224, 224, 224),
                agent: Color::Rgb(232, 232, 232),
                user: Color::Rgb(218, 218, 218),
                fg: Color::Rgb(28, 31, 35),
                muted: Color::Rgb(85, 90, 98),
                selected: Color::Rgb(192, 192, 192),
                accent: super::super::theme::ContentTheme::new(light).primary,
                warning: super::super::theme::ContentTheme::new(light).warning,
            }
        } else {
            Self {
                content: super::super::theme::ContentTheme::new(light),
                base: Color::Rgb(0, 0, 0),
                panel: Color::Rgb(28, 28, 28),
                input: Color::Rgb(40, 40, 40),
                agent: Color::Rgb(34, 34, 34),
                user: Color::Rgb(44, 44, 44),
                fg: Color::Rgb(222, 225, 230),
                muted: Color::Rgb(146, 153, 163),
                selected: Color::Rgb(64, 64, 64),
                accent: super::super::theme::ContentTheme::new(light).primary,
                warning: super::super::theme::ContentTheme::new(light).warning,
            }
        }
    }
    pub(super) fn background(self, surface: Surface) -> Color {
        match surface {
            Surface::User => self.user,
            Surface::Agent => self.agent,
            _ => self.base,
        }
    }
    pub(super) fn foreground(self, surface: Surface) -> Color {
        match surface {
            Surface::Muted | Surface::Status | Surface::Reasoning => self.content.muted,
            Surface::Error => self.content.error,
            _ => self.content.fg,
        }
    }
}
pub(super) fn spinner(tick: usize) -> &'static str {
    ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"][tick % 10]
}

pub(super) fn fill(frame: &mut Frame, rect: Rect, color: Color) {
    // Clear and background used to walk this rectangle separately through
    // per-cell coordinate lookups. Reset contiguous buffer slices in one pass.
    let buffer = frame.buffer_mut();
    let rect = rect.intersection(buffer.area);
    let stride = buffer.area.width as usize;
    let x = rect.x.saturating_sub(buffer.area.x) as usize;
    for y in rect.y..rect.bottom() {
        let start = (y - buffer.area.y) as usize * stride + x;
        for cell in &mut buffer.content[start..start + rect.width as usize] {
            cell.reset();
            cell.bg = color;
        }
    }
}
pub(super) fn focus_cursor(frame: &mut Frame, x: u16, y: u16, bg: Color) {
    text(frame, r(x, y, 1, 1), "▌", Color::Rgb(255, 255, 255), bg);
}
pub(super) fn text(
    frame: &mut Frame,
    rect: Rect,
    text: impl Into<Line<'static>>,
    fg: Color,
    bg: Color,
) {
    let mut text = text.into();
    for span in &mut text.spans {
        span.content = super::super::model::clean(&span.content).into();
    }
    frame.render_widget(
        Paragraph::new(text).style(Style::default().fg(fg).bg(bg)),
        rect,
    );
}
pub(super) fn r(x: u16, y: u16, width: u16, height: u16) -> Rect {
    Rect::new(x, y, width, height)
}

/// Paint Markdown geometry without inserting decorative text into the source.
pub(super) fn render_row_line(
    row: &Row,
    area: Rect,
    buffer: &mut Buffer,
    style: Style,
    p: Palette,
) {
    if row.layout == markdown::RowLayout::default() {
        render_line(&row.line, area, buffer, style);
        return;
    }
    let area = area.intersection(buffer.area);
    if area.is_empty() {
        return;
    }
    buffer.set_style(area, style.patch(row.line.style));
    let prefix_width = row.layout.prefix.width().min(area.width as usize);
    if prefix_width > 0 {
        render_line(
            &row.layout.prefix,
            r(area.x, area.y, prefix_width as u16, 1),
            buffer,
            style,
        );
    }
    if let Some(code) = row.layout.code {
        let offset = code.indent;
        let rect = r(
            area.x.saturating_add(offset as u16),
            area.y,
            code.width as u16,
            1,
        )
        .intersection(area);
        buffer.set_style(rect, Style::default().bg(p.content.code_bg));
    }
    if row.layout.decorative {
        return;
    }
    let mut byte = 0;
    let mut column = prefix_width;
    for grapheme in row.line.styled_graphemes(Style::default()) {
        if byte == row.layout.source_prefix {
            column = row
                .layout
                .code
                .map_or(prefix_width + row.layout.source_prefix_width, |code| {
                    code.indent + code.padding
                });
        }
        let size = grapheme.symbol.width();
        let in_prefix = byte < row.layout.source_prefix;
        let prefix_clipped =
            in_prefix && column + size > prefix_width + row.layout.source_prefix_width;
        let body_end = row.layout.code.map_or(area.width as usize, |code| {
            (code.indent + code.width - code.padding).min(area.width as usize)
        });
        if !in_prefix && column + size > body_end {
            break;
        }
        if size > 0 && !prefix_clipped && column + size <= area.width as usize {
            buffer[(area.x + column as u16, area.y)]
                .set_symbol(grapheme.symbol)
                .set_style(grapheme.style);
        }
        byte += grapheme.symbol.len();
        column += size;
    }
}

pub(super) fn render_line(line: &Line<'_>, area: Rect, buffer: &mut Buffer, style: Style) {
    let area = area.intersection(buffer.area);
    if area.is_empty() {
        return;
    }
    // Paragraph applies line style to glyphs, not continuation/trailing cells.
    // Markdown block backgrounds are painted separately from explicit geometry.
    buffer.set_style(area, style);
    let graphemes = || {
        let mut used = 0;
        line.styled_graphemes(Style::default())
            .filter(move |g| g.symbol.width() <= area.width as usize)
            .take_while(move |g| {
                used += g.symbol.width();
                used <= area.width as usize
            })
    };
    let width = graphemes().map(|g| g.symbol.width() as u16).sum::<u16>();
    let mut x = area.x
        + match line.alignment.unwrap_or(Alignment::Left) {
            Alignment::Left => 0,
            Alignment::Center => area.width / 2 - width / 2,
            Alignment::Right => area.width - width,
        };
    for grapheme in graphemes() {
        let width = grapheme.symbol.width() as u16;
        if width != 0 {
            buffer[(x, area.y)]
                .set_symbol(grapheme.symbol)
                .set_style(grapheme.style);
            x += width;
        }
    }
}

impl Row {
    pub(super) fn background(
        &self,
        p: Palette,
        expanded: bool,
        content_edge: bool,
        interactive: bool,
        text_selected: bool,
    ) -> Color {
        // Expanded framing is persistent; text selection must not erase an edge.
        // Inside the body only the selected text range may change the background.
        if !self.blank
            && if expanded {
                content_edge
            } else {
                interactive && !text_selected
            }
        {
            p.selected
        } else {
            p.background(self.surface)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::widgets::Widget;

    fn markdown_rows(
        input: &str,
        width: u16,
        p: Palette,
        cache: Option<&super::super::super::tool_view::HighlightCache>,
    ) -> Vec<Row> {
        markdown::layout_highlighted(input, p, false, width as usize, width as usize, "", cache)
            .into_iter()
            .map(|line| Row {
                line: std::sync::Arc::new(line.line),
                header: false,
                x: 1,
                width,
                surface: Surface::Tool,
                entry: 0,
                selectable: true,
                blank: false,
                continued: line.continued,
                layout: line.layout,
                inset: 0,
            })
            .collect()
    }

    fn copy_rows(rows: &[Row]) -> String {
        let mut blocks = RowBlocks::default();
        *blocks.block_mut(0) = rows.to_vec();
        blocks.finish_update(0);
        selected_text(
            &blocks,
            (
                TextPosition { row: 0, byte: 0 },
                TextPosition {
                    row: rows.len() - 1,
                    byte: usize::MAX,
                },
            ),
        )
    }

    fn assert_code_geometry(rows: &[Row], p: Palette, width: u16) {
        for row in rows {
            let mut buffer = Buffer::empty(Rect::new(0, 0, width + 2, 1));
            buffer.set_style(buffer.area, Style::default().bg(p.agent));
            render_row_line(
                row,
                Rect::new(1, 0, width, 1),
                &mut buffer,
                Style::default().fg(p.fg).bg(p.agent),
                p,
            );
            let left = 1 + row.layout.code.map_or(0, |code| code.indent);
            for x in 0..width + 2 {
                let code = row.layout.code.is_some_and(|code| {
                    (left..(left + code.width).min(width as usize + 1)).contains(&(x as usize))
                });
                assert_eq!(
                    buffer[(x, 0)].bg,
                    if code { p.content.code_bg } else { p.agent },
                    "row={:?}, x={x}",
                    row.text()
                );
            }
            if row.layout.decorative {
                assert!(row.text().is_empty());
                assert!(
                    row.selection_range(
                        0,
                        Some((
                            TextPosition { row: 0, byte: 0 },
                            TextPosition { row: 1, byte: 0 }
                        ))
                    )
                    .is_none()
                );
            }
        }
    }

    #[test]
    fn markdown_code_geometry_fits_pads_wraps_and_preserves_copy() {
        for light in [false, true] {
            let p = Palette::new(light);
            for input in [
                "```\n  alpha beta gamma delta  \n\n    \nlast\n```",
                "```unknown\n  alpha beta gamma delta  \n\n    \nlast\n```",
                "    alpha beta gamma delta  \n\n    last",
                "```\nlast",
                "```\n界 👩‍💻 لا\n```",
                "> ```\n> short  \n> \n> last\n> ```",
                "- ```\n  short\n  last\n  ```",
                "```\na\n```\n\n```\nlonger\n```",
            ] {
                let original = markdown(input, p, 80)
                    .iter()
                    .map(Line::to_string)
                    .collect::<Vec<_>>()
                    .join("\n");
                for width in [1, 2, 3, 7, 80] {
                    let rows = markdown_rows(input, width, p, None);
                    assert_code_geometry(&rows, p, width);
                    assert_eq!(copy_rows(&rows), original, "input {input:?}, width {width}");
                    let code_rows = rows
                        .iter()
                        .filter(|row| row.layout.code.is_some())
                        .collect::<Vec<_>>();
                    assert!(code_rows.len() >= 3);
                    assert!(code_rows[0].layout.decorative);
                    assert!(code_rows.last().unwrap().layout.decorative);
                    for row in code_rows.iter().filter(|row| !row.layout.decorative) {
                        assert!(row.line.style.bg.is_none());
                        if row.layout.source_prefix == 0 && row.layout.prefix.width() == 0 {
                            let pad = row.layout.code.unwrap().padding;
                            assert_eq!(row.text_x(), 1 + pad as u16);
                            assert_eq!(row.byte_at_column(row.text_x()), 0);
                        }
                    }
                }
            }
            let rows = markdown_rows(
                "before `inline` after\n\n```\nabc\n\nx\n```\n\nafter",
                40,
                p,
                None,
            );
            let code = rows
                .iter()
                .filter_map(|row| row.layout.code)
                .collect::<Vec<_>>();
            assert_eq!(code.len(), 5); // three source rows plus top/bottom
            assert!(code.iter().all(|code| code.width == 5 && code.padding == 1));
            assert_code_geometry(&rows, p, 40);
            assert!(rows.first().unwrap().layout.code.is_none());
            assert!(rows.last().unwrap().layout.code.is_none());
            assert_eq!(copy_rows(&rows), "before inline after\n\nabc\n\nx\n\nafter");
        }
    }

    #[test]
    fn markdown_code_geometry_survives_highlight_arrival_without_styling_tools() {
        use super::super::super::tool_view::{Document, HighlightCache, Role, Section};
        let source = "let answer = 42;  \n\n// a sufficiently long comment to wrap\n";
        let input = format!("```rust\n{source}```");
        let document = Document {
            sections: vec![Section::Code {
                source: source.into(),
                language: "rust".into(),
                indent: 0,
                gutters: Vec::new(),
                role: Role::Constant,
            }],
        };
        for light in [false, true] {
            let p = Palette::new(light);
            let mut cache = HighlightCache::default();
            let pending = markdown_rows(&input, 80, p, Some(&cache));
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                cache.prepare(std::iter::once(&document), light);
                if cache.is_fully_highlighted(&document, light) {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "highlight worker did not finish"
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            let highlighted = markdown_rows(&input, 80, p, Some(&cache));
            assert_eq!(copy_rows(&pending), copy_rows(&highlighted));
            assert_eq!(
                pending.iter().map(|row| &row.layout).collect::<Vec<_>>(),
                highlighted
                    .iter()
                    .map(|row| &row.layout)
                    .collect::<Vec<_>>()
            );
            assert_ne!(pending[1].line.spans, highlighted[1].line.spans);
            for width in [1, 5, 80] {
                assert_code_geometry(&markdown_rows(&input, width, p, Some(&cache)), p, width);
            }
            let tool_lines = document.lines(Some(&cache), light);
            assert!(tool_lines.iter().all(|line| line.style.bg.is_none()));
            assert!(
                tool_lines
                    .iter()
                    .flat_map(|line| &line.spans)
                    .all(|span| span.style.bg.is_none())
            );
            for (markdown, tool) in highlighted
                .iter()
                .filter(|row| !row.layout.decorative)
                .zip(tool_lines)
            {
                assert_eq!(
                    markdown
                        .line
                        .spans
                        .iter()
                        .filter(|span| !span.content.is_empty())
                        .collect::<Vec<_>>(),
                    tool.spans
                        .iter()
                        .filter(|span| !span.content.is_empty())
                        .collect::<Vec<_>>()
                );
            }
        }
    }

    #[test]
    fn markdown_hanging_prefixes_are_decorative_and_preserve_copy() {
        for light in [false, true] {
            let p = Palette::new(light);
            for input in [
                "- alpha bravo charlie delta echo foxtrot",
                "10. alpha bravo charlie delta echo foxtrot",
                "- [ ] alpha bravo charlie delta echo foxtrot",
                "> alpha bravo charlie delta echo foxtrot",
                "> - alpha bravo charlie delta echo foxtrot",
                "1. outer\n   - inner alpha bravo charlie delta echo foxtrot",
            ] {
                let original = markdown(input, p, 80)
                    .iter()
                    .map(Line::to_string)
                    .collect::<Vec<_>>()
                    .join("\n");
                for width in [8, 16, 24] {
                    let rows = markdown_rows(input, width, p, None);
                    assert_eq!(copy_rows(&rows), original, "{input:?}, width {width}");
                    for row in rows.iter().filter(|row| row.continued) {
                        assert!(
                            row.layout.prefix.width() >= 2,
                            "missing hanging prefix for {input:?}"
                        );
                        assert_eq!(row.byte_at_column(row.text_x()), 0);
                        assert!(row.text_x() >= row.x + 2);
                    }
                }
            }
        }
    }

    fn expandable_entry() -> model::Entry {
        model::Entry {
            key: "entry".into(),
            text:
                "first header with many wrapped fragments\nbody with many wrapped fragments\n  \n"
                    .into(),
            surface: Surface::Tool,
            expandable: true,
            default_open: false,
            running: false,
            footer: None,
            request: None,
            indent: 0,
            job: None,
            compact_after: false,
            header: None,
            document: None,
        }
    }

    #[test]
    fn expanded_backgrounds_frame_only_true_content_edges_even_with_selection() {
        for light in [false, true] {
            let p = Palette::new(light);
            let entry = expandable_entry();
            let mut rows = RowBlocks::default();
            *rows.block_mut(0) = layout(std::slice::from_ref(&entry), 20, p, None);
            rows.finish_update(0);
            let nonblank = rows
                .iter()
                .filter(|row| !row.text().trim().is_empty())
                .collect::<Vec<_>>();
            assert!(nonblank.len() > 4, "both header and body should wrap");
            for (index, row) in nonblank.iter().enumerate() {
                let edge = index == 0 || index + 1 == nonblank.len();
                assert_eq!(rows.is_content_edge(row), edge);
            }
            for row in rows.iter() {
                let edge = rows.is_content_edge(row);
                for interactive in [false, true] {
                    for text_selected in [false, true] {
                        assert_eq!(
                            row.background(p, true, edge, interactive, text_selected),
                            if edge {
                                p.selected
                            } else {
                                p.background(row.surface)
                            },
                            "row {:?}, interactive={interactive}, selection={text_selected}",
                            row.text()
                        );
                    }
                }
            }
            let row = nonblank[0];
            assert_eq!(row.background(p, false, true, true, false), p.selected);
            assert_eq!(row.background(p, false, true, false, false), p.base);
            assert_eq!(row.background(p, false, true, true, true), p.base);
        }
    }

    #[test]
    fn foreground_only_overlay_preserves_each_underlying_background() {
        let bounds = Rect::new(0, 0, 24, 2);
        let area = Rect::new(3, 1, 17, 1);
        let mut buffer = Buffer::empty(bounds);
        for (index, cell) in buffer.content.iter_mut().enumerate() {
            cell.set_symbol("x")
                .set_bg(if index % 2 == 0 {
                    Color::Red
                } else {
                    Color::Blue
                })
                .set_style(Style::default().add_modifier(Modifier::all()));
        }
        let backgrounds = buffer
            .content
            .iter()
            .map(|cell| cell.bg)
            .collect::<Vec<_>>();
        render_line(
            &Line::from("↓ Latest activity"),
            area,
            &mut buffer,
            Style::default()
                .fg(Color::Yellow)
                .remove_modifier(Modifier::all()),
        );
        assert_eq!(
            buffer
                .content
                .iter()
                .map(|cell| cell.bg)
                .collect::<Vec<_>>(),
            backgrounds
        );
        assert_eq!(
            (area.x..area.right())
                .map(|x| buffer[(x, area.y)].symbol())
                .collect::<String>(),
            "↓ Latest activity"
        );
        assert!((area.x..area.right()).all(|x| buffer[(x, area.y)].modifier.is_empty()));
    }

    #[test]
    fn borrowed_lines_match_paragraph_styles_and_clipping() {
        let lines = [
            Line::from("plain text"),
            Line::from("  界 👩‍💻 wide  "),
            Line::from(vec![
                Span::styled(
                    "bold ",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::styled("界 tail", Style::default().bg(Color::Blue)),
            ])
            .style(Style::default().add_modifier(Modifier::ITALIC)),
            Line::default(),
        ];
        for line in lines {
            for width in 0..20 {
                let bounds = Rect::new(0, 0, 25, 3);
                let area = Rect::new(2, 1, width, 1);
                let mut expected = Buffer::empty(bounds);
                let mut actual = expected.clone();
                let style = Style::default().fg(Color::White).bg(Color::DarkGray);
                Paragraph::new(line.clone())
                    .style(style)
                    .render(area, &mut expected);
                render_line(&line, area, &mut actual, style);
                assert_eq!(actual, expected, "width {width}: {line:?}");
            }
        }
    }
    fn layout(
        entries: &[model::Entry],
        width: u16,
        p: Palette,
        highlights: Option<&super::super::super::tool_view::HighlightCache>,
    ) -> Vec<Row> {
        let fallback = super::super::super::tool_view::HighlightCache::default();
        let highlights = highlights.unwrap_or(&fallback);
        let mut rows = Vec::new();
        for (index, entry) in entries.iter().enumerate() {
            update_entry_rows(
                &mut rows,
                &mut CachedEntry::default(),
                entry,
                index,
                EntryLayout {
                    width,
                    palette: p,
                    highlights,
                    request_columns: RequestColumns::default(),
                    expanded: entry.default_open,
                },
                None,
            );
        }
        rows
    }

    fn markdown(text: &str, p: Palette, width: usize) -> Vec<Line<'static>> {
        markdown::render(text, p, true, width)
    }
}
