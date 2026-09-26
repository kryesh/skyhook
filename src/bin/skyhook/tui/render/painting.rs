//! Theme palette, terminal cell painting, and row decoration.

use super::*;

pub(super) fn background(surface: Surface) -> Color {
    match surface {
        Surface::User => THEME.user,
        Surface::Agent => THEME.agent,
        _ => THEME.base,
    }
}
pub(super) fn foreground(surface: Surface) -> Color {
    match surface {
        Surface::Muted | Surface::Status | Surface::Reasoning => THEME.muted,
        Surface::Error => THEME.error,
        _ => THEME.fg,
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
    let mut line = text.into();
    for span in &mut line.spans {
        if span.content.contains(char::is_control) {
            span.content = super::super::model::clean(&span.content).into();
        }
    }
    render_line(
        &line,
        rect,
        frame.buffer_mut(),
        Style::default().fg(fg).bg(bg),
    );
}
pub(super) fn r(x: u16, y: u16, width: u16, height: u16) -> Rect {
    Rect::new(x, y, width, height)
}

/// Paint Markdown geometry without inserting decorative text into the source.
pub(super) fn render_row_line(row: &Row, area: Rect, buffer: &mut Buffer, style: Style) {
    if !row.layout.has_geometry() {
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
    if let Some(code) = row.layout.code() {
        let offset = code.indent();
        let rect = r(
            area.x.saturating_add(offset as u16),
            area.y,
            code.width() as u16,
            1,
        )
        .intersection(area);
        buffer.set_style(rect, Style::default().bg(THEME.code_bg));
    }
    if row.layout.decorative() {
        return;
    }
    let mut byte = 0;
    let mut column = prefix_width;
    let (source_prefix, source_prefix_width) = row.layout.source_prefix();
    let body_end = row.layout.code().map_or(area.width as usize, |code| {
        code.body_end().min(area.width as usize)
    });
    for grapheme in styled_cells(&row.line) {
        if byte == source_prefix {
            column = row
                .layout
                .code()
                .map_or(prefix_width + source_prefix_width, |code| code.body_start());
        }
        let size = grapheme.width;
        let in_prefix = byte < source_prefix;
        let prefix_clipped = in_prefix && column + size > prefix_width + source_prefix_width;
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

struct StyledCell<'a> {
    symbol: &'a str,
    width: usize,
    style: Style,
}

/// `Line::styled_graphemes` with widths, over the cell primitive's ASCII fast path.
fn styled_cells<'a>(line: &'a Line<'_>) -> impl Iterator<Item = StyledCell<'a>> {
    line.spans.iter().flat_map(move |span| {
        let style = line.style.patch(span.style);
        cells(&span.content)
            .filter(|(_, symbol, _)| !symbol.contains(char::is_control))
            .map(move |(_, symbol, width)| StyledCell {
                symbol,
                width,
                style,
            })
    })
}

/// Paint borrowed spans with an unwrapped Paragraph's clipping and styling:
/// wide continuation cells keep the area style, and graphemes wider than the
/// area are skipped.
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
        styled_cells(line)
            .filter(move |g| g.width <= area.width as usize)
            .take_while(move |g| {
                used += g.width;
                used <= area.width as usize
            })
    };
    let width = graphemes().map(|g| g.width as u16).sum::<u16>();
    let mut x = area.x
        + match line.alignment.unwrap_or(Alignment::Left) {
            Alignment::Left => 0,
            Alignment::Center => area.width / 2 - width / 2,
            Alignment::Right => area.width - width,
        };
    for grapheme in graphemes() {
        let width = grapheme.width as u16;
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
        expanded: bool,
        interactive: bool,
        text_selected: bool,
    ) -> Color {
        // Fill expanded items and collapsed hover/focus highlights uniformly.
        // Text selection is painted separately over the persistent expanded fill.
        if !self.layout.is_spacer() && (expanded || (interactive && !text_selected)) {
            THEME.code_bg
        } else {
            background(self.surface)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::tool_view::{Document, HighlightCache, Role, Section};
    use super::super::tests::{expandable_entry, fixture_row, layout};
    use super::*;
    use ratatui::widgets::{Paragraph, Widget};

    fn markdown_rows(input: &str, width: u16, cache: Option<&HighlightCache>) -> Vec<Row> {
        let columns = width as usize;
        let layout = markdown::layout_highlighted(input, false, columns, columns, "", cache);
        let rows = layout.lines.into_iter();
        rows.map(|line| fixture_row(line.line, line.layout, 1, width, 0))
            .collect()
    }

    fn copy_rows(rows: &[Row]) -> String {
        let mut blocks = RowBlocks::default();
        blocks.replace_entry(0, rows.to_vec(), Vec::new());
        let end = TextPosition {
            row: rows.len() - 1,
            byte: usize::MAX,
        };
        selected_text(&blocks, (TextPosition { row: 0, byte: 0 }, end))
    }

    /// The copyable text of unwrapped markdown.
    fn original(input: &str) -> String {
        let lines = markdown::render(input, true, 80);
        lines
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn assert_code_geometry(rows: &[Row], width: u16) {
        for row in rows {
            let mut buffer = Buffer::empty(Rect::new(0, 0, width + 2, 1));
            buffer.set_style(buffer.area, Style::default().bg(THEME.agent));
            let style = Style::default().fg(THEME.fg).bg(THEME.agent);
            render_row_line(row, Rect::new(1, 0, width, 1), &mut buffer, style);
            let left = 1 + row.layout.code().map_or(0, |code| code.indent());
            for x in 0..width + 2 {
                let code = row.layout.code().is_some_and(|code| {
                    (left..(left + code.width()).min(width as usize + 1)).contains(&(x as usize))
                });
                let expected = if code { THEME.code_bg } else { THEME.agent };
                assert_eq!(buffer[(x, 0)].bg, expected, "row={:?}, x={x}", row.text());
            }
            if row.layout.decorative() {
                assert!(row.text().is_empty());
                let all = (
                    TextPosition { row: 0, byte: 0 },
                    TextPosition { row: 1, byte: 0 },
                );
                assert!(row.text_view().selection_range(0, Some(all)).is_none());
            }
        }
    }

    #[test]
    fn markdown_code_geometry_fits_pads_wraps_and_preserves_copy() {
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
            let original = original(input);
            for width in [1, 2, 3, 7, 80] {
                let rows = markdown_rows(input, width, None);
                assert_code_geometry(&rows, width);
                assert_eq!(copy_rows(&rows), original, "input {input:?}, width {width}");
                let code_rows: Vec<_> = rows
                    .iter()
                    .filter(|row| row.layout.code().is_some())
                    .collect();
                assert!(code_rows.len() >= 3);
                assert!(code_rows[0].layout.decorative());
                assert!(code_rows.last().unwrap().layout.decorative());
                for row in code_rows.iter().filter(|row| !row.layout.decorative()) {
                    assert!(row.line.style.bg.is_none());
                    if row.layout.source_prefix().0 == 0 && row.layout.prefix.width() == 0 {
                        let pad = row.layout.code().unwrap().padding();
                        assert_eq!(row.text_x(), 1 + pad as u16);
                        assert_eq!(row.byte_at_column(row.text_x()), 0);
                    }
                }
            }
        }
        let input = "before `inline` after\n\n```\nabc\n\nx\n```\n\nafter";
        let rows = markdown_rows(input, 40, None);
        let code: Vec<_> = rows.iter().filter_map(|row| row.layout.code()).collect();
        assert_eq!(code.len(), 5); // three source rows plus top/bottom
        assert!(
            code.iter()
                .all(|code| code.width() == 5 && code.padding() == 1)
        );
        assert_code_geometry(&rows, 40);
        assert!(rows.first().unwrap().layout.code().is_none());
        assert!(rows.last().unwrap().layout.code().is_none());
        assert_eq!(copy_rows(&rows), "before inline after\n\nabc\n\nx\n\nafter");
    }

    #[test]
    fn markdown_code_geometry_survives_highlight_arrival_without_styling_tools() {
        let source = "let answer = 42;  \n\n// a sufficiently long comment to wrap\n";
        let input = format!("```rust\n{source}```");
        let document = Document {
            sections: vec![Section::Code {
                source: source.into(),
                language: "rust".into(),
                indent: 0,
                gutters: Default::default(),
                role: Role::Constant,
            }],
        };
        let mut cache = HighlightCache::default();
        let pending = markdown_rows(&input, 80, Some(&cache));
        cache.wait(&document);
        let highlighted = markdown_rows(&input, 80, Some(&cache));
        assert_eq!(copy_rows(&pending), copy_rows(&highlighted));
        let layouts = |rows: &[Row]| {
            rows.iter()
                .map(|row| row.layout.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(layouts(&pending), layouts(&highlighted));
        assert_ne!(pending[1].line.spans, highlighted[1].line.spans);
        for width in [1, 5, 80] {
            assert_code_geometry(&markdown_rows(&input, width, Some(&cache)), width);
        }
        let tool_lines = document.lines(Some(&cache));
        assert!(tool_lines.iter().all(|line| line.style.bg.is_none()));
        let mut spans = tool_lines.iter().flat_map(|line| &line.spans);
        assert!(spans.all(|span| span.style.bg.is_none()));
        let visible = |spans: &[Span<'static>]| {
            let spans = spans.iter().filter(|span| !span.content.is_empty());
            spans.cloned().collect::<Vec<_>>()
        };
        let source_rows = highlighted.iter().filter(|row| !row.layout.decorative());
        for (markdown, tool) in source_rows.zip(tool_lines) {
            assert_eq!(visible(&markdown.line.spans), visible(&tool.spans));
        }
    }

    #[test]
    fn markdown_hanging_prefixes_are_decorative_and_preserve_copy() {
        for input in [
            "- alpha bravo charlie delta echo foxtrot",
            "10. alpha bravo charlie delta echo foxtrot",
            "- [ ] alpha bravo charlie delta echo foxtrot",
            "> alpha bravo charlie delta echo foxtrot",
            "> - alpha bravo charlie delta echo foxtrot",
            "1. outer\n   - inner alpha bravo charlie delta echo foxtrot",
        ] {
            let original = original(input);
            for width in [8, 16, 24] {
                let rows = markdown_rows(input, width, None);
                assert_eq!(copy_rows(&rows), original, "{input:?}, width {width}");
                for row in rows.iter().filter(|row| row.layout.continued()) {
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

    #[test]
    fn expanded_backgrounds_fill_all_content_rows_even_with_selection() {
        let mut rows = RowBlocks::default();
        rows.replace_entry(0, layout(&expandable_entry(), 20), Vec::new());
        let nonblank: Vec<_> = rows
            .iter()
            .filter(|row| !row.text().trim().is_empty())
            .collect();
        assert!(nonblank.len() > 4, "both header and body should wrap");
        for row in rows.iter() {
            let expected = if row.layout.is_spacer() {
                background(row.surface)
            } else {
                THEME.code_bg
            };
            for interactive in [false, true] {
                for selected in [false, true] {
                    let background = row.background(true, interactive, selected);
                    assert_eq!(
                        background,
                        expected,
                        "row {:?}, {interactive}/{selected}",
                        row.text()
                    );
                }
            }
        }
        let row = nonblank[0];
        assert_eq!(row.background(false, true, false), THEME.code_bg);
        assert_eq!(row.background(false, false, false), THEME.base);
        assert_eq!(row.background(false, true, true), THEME.base);
    }

    #[test]
    fn foreground_only_overlay_preserves_each_underlying_background() {
        let area = Rect::new(3, 1, 17, 1);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 24, 2));
        for (index, cell) in buffer.content.iter_mut().enumerate() {
            let bg = if index % 2 == 0 {
                Color::Red
            } else {
                Color::Blue
            };
            cell.set_symbol("x")
                .set_bg(bg)
                .set_style(Style::default().add_modifier(Modifier::all()));
        }
        let backgrounds = |buffer: &Buffer| {
            buffer
                .content
                .iter()
                .map(|cell| cell.bg)
                .collect::<Vec<_>>()
        };
        let before = backgrounds(&buffer);
        let style = Style::default()
            .fg(Color::Yellow)
            .remove_modifier(Modifier::all());
        render_line(&Line::from("↓ Latest activity"), area, &mut buffer, style);
        assert_eq!(backgrounds(&buffer), before);
        let cells = (area.x..area.right()).map(|x| &buffer[(x, area.y)]);
        assert_eq!(
            cells.clone().map(|cell| cell.symbol()).collect::<String>(),
            "↓ Latest activity"
        );
        assert!(cells.clone().all(|cell| cell.modifier.is_empty()));
    }

    #[test]
    fn borrowed_lines_match_paragraph_styles_and_clipping() {
        let bold = Style::default().fg(Color::Red).add_modifier(Modifier::BOLD);
        let lines = [
            Line::from("plain text"),
            Line::from("  界 👩‍💻 wide  "),
            Line::from(vec![
                Span::styled("bold ", bold),
                Span::styled("界 tail", Style::default().bg(Color::Blue)),
            ])
            .style(Style::default().add_modifier(Modifier::ITALIC)),
            Line::default(),
        ];
        let style = Style::default().fg(Color::White).bg(Color::DarkGray);
        for line in lines {
            for width in 0..20 {
                let area = Rect::new(2, 1, width, 1);
                let mut expected = Buffer::empty(Rect::new(0, 0, 25, 3));
                let mut actual = expected.clone();
                Paragraph::new(line.clone())
                    .style(style)
                    .render(area, &mut expected);
                render_line(&line, area, &mut actual, style);
                assert_eq!(actual, expected, "width {width}: {line:?}");
            }
        }
    }
}
