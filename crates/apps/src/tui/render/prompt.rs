//! The prompt shown in place of the composer: its questions, tabs, options and previews.

use super::painting::{focus_cursor, r, render_line, render_row_line, text};
use crate::tui::app::{App, Hit};
use crate::tui::editor::TextField;
use crate::tui::theme::theme;
use crate::tui::tool_view::Document;
use crate::tui::wrapping::wrap_plain;
use crate::tui::{format, markdown};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

/// Measure the same wrapped content used for painting, so long questions,
/// choices and typed answers can grow the prompt instead of being clipped.
pub(super) struct PromptLayout {
    body: Vec<markdown::LayoutLine>,
    /// The code fences the Markdown highlights.
    pub(super) fences: Document,
    title_start: Option<usize>,
    options: Vec<PromptOptionLine>,
    selected_start: usize,
    /// The focused suggestion's preview beside the options, when the panel is wide
    /// enough to split; otherwise it follows the suggestion.
    side: Option<SidePreview>,
    /// Absent for prompts answered by choice alone.
    input: Option<super::super::composer::ComposerLayout>,
    input_label: Vec<String>,
    /// The height of the batch's tallest question page, which every page keeps so
    /// switching between them leaves the panel still.
    floor: u16,
}

struct SidePreview {
    lines: Vec<markdown::LayoutLine>,
    /// The rows of the tallest preview, which every one reserves.
    rows: usize,
    /// The columns the suggestions keep.
    options: u16,
}

struct PromptAreas {
    body: Rect,
    options: Rect,
    input: Rect,
    help: Rect,
}

impl PromptLayout {
    pub(super) fn new(app: &App, width: u16) -> Self {
        let mut layout = Self::page(app, app.question_page(), &app.prompt_text(), width);
        if let Some(header) = app.question_header() {
            let pages = app.question_pages().into_iter();
            let heights = pages.map(|page| Self::page(app, Some(page), &header, width).height());
            layout.floor = heights.max().unwrap_or_default();
        }
        layout
    }

    /// The layout of `page`, or of the front prompt when it has none, under `body_text`.
    fn page(
        app: &App,
        page: Option<crate::tui::app::QuestionPage<'_>>,
        body_text: &str,
        width: u16,
    ) -> Self {
        let width = width.saturating_sub(4);
        let (state, takes_text) = match &page {
            Some(crate::tui::app::QuestionPage::Question(_, input, _)) => (*input, true),
            Some(crate::tui::app::QuestionPage::Review(_)) => (app.prompt_input(), false),
            None => (app.prompt_input(), app.prompt_takes_text()),
        };
        let body_text = format::clean(body_text);
        let mut body: Vec<_> = (wrap_plain(&body_text, width as usize).into_iter())
            .map(plain_line)
            .collect();
        body.extend(tab_lines(&app.question_tabs(), width.into()));
        let page_prompt = page.as_ref().map(|page| page.prompt());
        let title_start = page_prompt.map(|_| body.len());
        let mut fences = Document::default();
        let mut lay_out = |source: &str, width: usize| {
            let highlights = Some(&app.render.highlights);
            let laid = markdown::layout_highlighted(source, false, width, highlights);
            fences.sections.extend(laid.fences.sections);
            laid.lines
        };
        if let Some(prompt) = page_prompt {
            body.extend(lay_out(&format::clean(prompt), width.into()));
        }
        // A question with previews shows the focused one's beside its suggestions when the
        // panel is wide enough, otherwise under it; a suggestion without one says so. Every
        // preview takes the room of the tallest, its code boxes the whole width, so moving
        // between suggestions changes neither.
        let previews = page.as_ref().and_then(|page| page.previews());
        let side_width = (width >= SIDE_PREVIEW_WIDTH).then_some(width / 2);
        let preview_width = side_width.map_or(width.saturating_sub(MARKER_WIDTH), |options| {
            width - options - PREVIEW_GAP
        });
        let preview = previews.map(|(previews, focused)| {
            let mut laid: Vec<_> = (previews.into_iter())
                .map(|preview| {
                    let preview = format::clean(preview.unwrap_or("*No preview*"));
                    let mut lines = lay_out(&preview, preview_width.into());
                    for line in &mut lines {
                        line.layout.fill_code(preview_width.into());
                    }
                    lines
                })
                .collect();
            let tallest = laid.iter().map(Vec::len).max().unwrap_or_default();
            let lines = (focused < laid.len()).then(|| laid.swap_remove(focused));
            (lines.unwrap_or_default(), tallest)
        });
        let (stacked, side) = match (preview, side_width) {
            (Some((lines, rows)), Some(options)) => {
                let side = SidePreview {
                    lines,
                    rows,
                    options,
                };
                (None, Some(side))
            }
            (preview, _) => {
                let padded = preview.map(|(mut lines, rows)| {
                    lines.resize_with(rows, markdown::LayoutLine::default);
                    lines
                });
                (padded, None)
            }
        };
        let option_width = side.as_ref().map_or(width, |side| side.options);
        let choices = match &page {
            Some(page) => page.choices(),
            _ => app
                .prompt_options()
                .into_iter()
                .map(|label| (label.to_owned(), String::new()))
                .collect(),
        };
        let (options, selected_start) =
            prompt_option_lines(&choices, state.choice, option_width, stacked, &mut lay_out);
        let input = takes_text.then(|| state.editor.layout(width as usize));
        let input_label = page
            .as_ref()
            .map(|page| page.input_label())
            .unwrap_or_default();
        let input_label = if input_label.is_empty() {
            Vec::new()
        } else {
            wrap_plain(input_label, width as usize)
        };
        Self {
            body,
            fences,
            title_start,
            options,
            selected_start,
            side,
            input,
            input_label,
            floor: 0,
        }
    }

    /// Rows the suggestions want, with a preview beside them as tall as either.
    fn option_rows(&self) -> usize {
        let side = self.side.as_ref().map_or(0, |side| side.rows);
        self.options.len().max(side)
    }

    pub(super) fn height(&self) -> u16 {
        let sections = self.body.len() + self.option_rows() + self.input_rows();
        let padding = self.padding().into_iter().sum::<u16>();
        let own = (sections + usize::from(padding) + 1).min(u16::MAX as usize) as u16;
        own.max(self.floor)
    }

    /// The top margin, gap above any input, and gap above the shortcut help.
    fn padding(&self) -> [u16; 3] {
        [1, u16::from(self.input_rows() > 0), 1]
    }

    fn input_rows(&self) -> usize {
        self.input.as_ref().map_or(0, |input| input.rows.len()) + self.input_label.len()
    }

    fn areas(&self, rect: Rect) -> PromptAreas {
        let wanted = [self.body.len(), self.option_rows(), self.input_rows()];
        let help_height = rect.height.min(1);
        let mut remaining = rect.height - help_height;
        // Padding gives way before clipping content or hiding the editable answer.
        let mut spare = usize::from(remaining).saturating_sub(wanted.iter().sum()) as u16;
        let padding = self.padding().map(|wanted| {
            let rows = wanted.min(spare);
            spare -= rows;
            remaining -= rows;
            rows
        });
        let mut rows = [0; 3];
        // Reserve a row per section, with the editable answer first if not all fit.
        for index in [2, 0, 1] {
            if remaining > 0 && wanted[index] > 0 {
                rows[index] = 1;
                remaining -= 1;
            }
        }
        // Share a screen-limited box between its sections, giving unused rows
        // back to longer sections rather than always splitting it in half.
        while remaining > 0 {
            let mut grew = false;
            for (rows, wanted) in rows.iter_mut().zip(wanted) {
                if remaining > 0 && (*rows as usize) < wanted {
                    *rows += 1;
                    remaining -= 1;
                    grew = true;
                }
            }
            if !grew {
                break;
            }
        }
        let [body_height, option_height, input_height] = rows;
        let top = rect.y + padding[0];
        let help_y = rect.bottom() - help_height;
        let area = |y, height| r(2, y, rect.width.saturating_sub(4), height);
        PromptAreas {
            body: area(top, body_height),
            options: area(top + body_height, option_height),
            input: area(help_y - padding[2] - input_height, input_height),
            help: area(help_y, help_height),
        }
    }
}

pub(super) fn draw_prompt(frame: &mut Frame, app: &mut App, layout: &PromptLayout) {
    let rect = app.render.composer_rect;
    let lines = &layout.body;
    let option_lines = &layout.options;
    let selected_start = layout.selected_start;
    let areas = layout.areas(rect);
    let (body_height, option_height, input_height) =
        (areas.body.height, areas.options.height, areas.input.height);
    app.render.prompt_body_rect = areas.body;
    app.render.prompt_options_rect = areas.options;
    app.render.prompt_body_rows = lines.len();
    let body_top = (app.prompt_input_mut().body).fit(body_height.into(), lines.len(), None);
    for (offset, line) in lines
        .iter()
        .skip(body_top)
        .take(body_height as usize)
        .enumerate()
    {
        let title = layout
            .title_start
            .is_some_and(|start| body_top + offset >= start);
        let style = if title {
            Style::default()
                .fg(theme().ui.emphasis)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme().ui.warning)
        };
        render_row_line(
            &line.line,
            &line.layout,
            r(
                2,
                areas.body.y + offset as u16,
                rect.width.saturating_sub(4),
                1,
            ),
            frame.buffer_mut(),
            style.bg(theme().ui.input),
        );
    }
    let option_rows = layout.option_rows();
    app.render.prompt_option_rows = option_rows;
    let input = app.prompt_input_mut();
    let reveal = (!input.options_scrolled).then_some(selected_start);
    input.options_scrolled = true;
    let options_top = (input.options).fit(option_height.into(), option_rows, reveal);
    let mut cursor_drawn = false;
    let option_width = (layout.side.as_ref()).map_or(areas.options.width, |side| side.options);
    for (offset, line) in option_lines
        .iter()
        .skip(options_top)
        .take(option_height as usize)
        .enumerate()
    {
        let row = r(2, areas.options.y + offset as u16, option_width, 1);
        let style = line.style(app.prompt_input().choice);
        render_line(&Line::from(line.marker), row, frame.buffer_mut(), style);
        let text = r(
            row.x + MARKER_WIDTH,
            row.y,
            row.width.saturating_sub(MARKER_WIDTH),
            1,
        );
        let (text_line, layout) = (&line.text.line, &line.text.layout);
        render_row_line(text_line, layout, text, frame.buffer_mut(), style);
        if line.index == app.prompt_input().choice
            && !cursor_drawn
            && app.menu.is_none()
            && !(app.multiple_pages() && app.page_editing())
        {
            focus_cursor(frame, row.x - 1, row.y, theme().ui.input);
            cursor_drawn = true;
        }
        app.render.hits.push((row, Hit::PromptChoice(line.index)));
    }
    if let Some(side) = &layout.side {
        let x = areas.options.x + side.options + PREVIEW_GAP;
        let width = (areas.options.width).saturating_sub(side.options + PREVIEW_GAP);
        let style = Style::default().fg(theme().ui.fg).bg(theme().ui.input);
        // The view scrolls over the suggestions and the tallest preview, so the focused
        // preview follows it only as far as its own last row.
        let overflow = side.lines.len().saturating_sub(option_height.into());
        let shown = side.lines.iter().skip(options_top.min(overflow));
        let shown = shown.take(option_height.into());
        for (offset, line) in shown.enumerate() {
            let row = r(x, areas.options.y + offset as u16, width, 1);
            render_row_line(&line.line, &line.layout, row, frame.buffer_mut(), style);
        }
    }
    let label_height = layout
        .input_label
        .len()
        .min(input_height.saturating_sub(1) as usize) as u16;
    let input_y = areas.input.y;
    for (offset, label) in layout
        .input_label
        .iter()
        .take(label_height as usize)
        .enumerate()
    {
        text(
            frame,
            r(2, input_y + offset as u16, rect.width.saturating_sub(4), 1),
            label.clone(),
            theme().ui.muted,
            theme().ui.input,
        );
    }
    if let Some(input) = &layout.input {
        let visible = input_height.saturating_sub(label_height) as usize;
        let (cursor_row, cursor_column) = input.cursor;
        let input_top = cursor_row.saturating_sub(visible.saturating_sub(1));
        for (offset, row) in input.rows.iter().skip(input_top).take(visible).enumerate() {
            text(
                frame,
                r(
                    2,
                    input_y + label_height + offset as u16,
                    rect.width.saturating_sub(4),
                    1,
                ),
                row.line(
                    Style::default(),
                    Style::default(),
                    Style::default().bg(theme().ui.selected),
                ),
                theme().ui.fg,
                theme().ui.input,
            );
        }
        if visible > 0 && (!app.multiple_pages() || app.page_editing()) && app.menu.is_none() {
            frame.set_cursor_position((
                2 + (cursor_column as u16).min(rect.width.saturating_sub(4)),
                input_y + label_height + (cursor_row - input_top) as u16,
            ));
        }
    }
    let (hint, color) = match app.prompts.front().map(|prompt| &prompt.kind) {
        Some(skyhook::interaction::PromptKind::Questions { .. })
            if app.multiple_pages() && app.page_index() < app.question_count() =>
        {
            let (page, pages) = ("Question", "questions");
            let keys = if app.page_editing() {
                format!("←→ cursor · Tab switch {pages}")
            } else {
                "←→ switch · Tab edit".to_owned()
            };
            let (index, count) = (app.page_index() + 1, app.question_count());
            let tick = tick_hint(app);
            let hint = format!(
                "{page} {index}/{count} · {keys} · ↑↓ choose{tick} · Enter answer · Esc cancel"
            );
            (hint, theme().ui.warning)
        }
        kind => {
            let escape = match kind {
                Some(skyhook::interaction::PromptKind::Approval { .. }) => "dismiss",
                _ => "cancel",
            };
            let tick = tick_hint(app);
            let hint = format!(
                "↑↓ choose{tick} · PgUp/PgDn text · Ctrl+PgUp/PgDn choices · Enter submit · Esc {escape}"
            );
            (hint, theme().ui.muted)
        }
    };
    text(frame, areas.help, hint, color, theme().ui.input);
}

/// Every label and description row retains the choice's selection and hit target.
struct PromptOptionLine {
    index: usize,
    /// Painted in the two cells before the text.
    marker: &'static str,
    text: markdown::LayoutLine,
    description: bool,
}

impl PromptOptionLine {
    fn style(&self, selected: usize) -> Style {
        let style = Style::default().bg(if self.index == selected {
            theme().ui.selected
        } else {
            theme().ui.input
        });
        if self.description {
            style.fg(theme().ui.muted)
        } else {
            style.fg(theme().ui.fg)
        }
    }
}

fn plain_line(text: String) -> markdown::LayoutLine {
    markdown::LayoutLine {
        line: Line::from(text),
        ..markdown::LayoutLine::default()
    }
}

/// The narrowest panel that shows a preview beside the suggestions rather than under one.
const SIDE_PREVIEW_WIDTH: u16 = 90;
/// Columns between the suggestions and the preview beside them.
const PREVIEW_GAP: u16 = 2;
/// Columns of a suggestion's `> ` focus marker, which its text and a preview under it follow.
const MARKER_WIDTH: u16 = 2;

/// A question batch's tabs, wrapped between tabs to `width`: the one in view inverted,
/// those answered ticked, those open muted.
fn tab_lines(
    tabs: &[(String, crate::tui::app::TabState)],
    width: usize,
) -> Vec<markdown::LayoutLine> {
    let mut lines: Vec<Vec<Span<'static>>> = Vec::new();
    let mut used = 0;
    for (header, state) in tabs {
        let (text, style) = match state {
            crate::tui::app::TabState::Current => (
                format!(" {header} "),
                Style::default()
                    .fg(theme().ui.input)
                    .bg(theme().ui.emphasis)
                    .add_modifier(Modifier::BOLD),
            ),
            crate::tui::app::TabState::Answered => (
                format!("✓ {header}"),
                Style::default().fg(theme().ui.success),
            ),
            crate::tui::app::TabState::Open => {
                (format!("○ {header}"), Style::default().fg(theme().ui.muted))
            }
        };
        let tab = format::clean(&text);
        let tab_width = tab.width();
        match lines.last_mut() {
            Some(line) if used + 2 + tab_width <= width => {
                line.push(Span::raw("  "));
                line.push(Span::styled(tab, style));
                used += 2 + tab_width;
            }
            _ => {
                lines.push(vec![Span::styled(tab, style)]);
                used = tab_width;
            }
        }
    }
    let line = |spans: Vec<Span<'static>>| markdown::LayoutLine {
        line: Line::from(spans),
        ..markdown::LayoutLine::default()
    };
    lines.into_iter().map(line).collect()
}

/// Space ticks the focused suggestion of a question taking several, until text is typed.
fn tick_hint(app: &App) -> &'static str {
    let tickable = app.question_page().is_some_and(|page| page.tickable());
    if tickable && app.prompt_input().editor.text().is_empty() {
        " · Space tick"
    } else {
        ""
    }
}

/// Each suggestion's label and description, laid out after its marker within `width`, the
/// selected one followed by `stacked`, its preview, when it is not shown beside them.
fn prompt_option_lines(
    options: &[(String, String)],
    selected: usize,
    width: u16,
    mut stacked: Option<Vec<markdown::LayoutLine>>,
    mut lay_out: impl FnMut(&str, usize) -> Vec<markdown::LayoutLine>,
) -> (Vec<PromptOptionLine>, usize) {
    let width = width.saturating_sub(MARKER_WIDTH) as usize;
    let mut option_lines = Vec::new();
    let mut selected_start = 0;
    for (index, (label, description)) in options.iter().enumerate() {
        if index == selected {
            selected_start = option_lines.len();
        }
        let label = lay_out(&format::clean(label), width).into_iter();
        option_lines.extend(label.enumerate().map(|(offset, text)| PromptOptionLine {
            index,
            marker: if offset == 0 && index == selected {
                "> "
            } else {
                "  "
            },
            text,
            description: false,
        }));
        let mut description = lay_out(&format::clean(description), width);
        if index == selected
            && let Some(preview) = stacked.take()
        {
            description.extend(preview);
        }
        option_lines.extend(description.into_iter().map(|text| PromptOptionLine {
            index,
            marker: "  ",
            text,
            description: true,
        }));
    }
    (option_lines, selected_start)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::tests::draft_fixture;
    use crossterm::event::Event;
    use skyhook::interaction::{Prompt, PromptKind};
    use tokio::sync::oneshot;

    fn assert_prompt_render(app: &mut App, width: u16, height: u16, answer: Option<&str>) {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| crate::tui::render::draw(frame, app))
            .unwrap();
        let cursor = terminal.get_cursor_position().unwrap();
        let buffer = terminal.backend().buffer();
        let row = |y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        };
        let layout = PromptLayout::new(app, width);
        let areas = layout.areas(app.render.composer_rect);
        assert!(areas.body.height > 0);
        if !layout.options.is_empty() {
            assert!(areas.options.height > 0);
            assert!(app.render.hits.iter().any(|(rect, hit)| {
                matches!(hit, Hit::PromptChoice(_))
                    && areas.options.contains((rect.x, rect.y).into())
            }));
        }
        if let Some(answer) = answer {
            assert!(areas.input.contains(cursor), "{width}x{height}: {cursor:?}");
            assert!(row(cursor.y).contains(answer), "{width}x{height}");
            assert_eq!(cursor.x, areas.input.x + answer.width() as u16);
        }
        let content_rows = layout.body.len() + layout.option_rows() + layout.input_rows();
        if usize::from(app.render.composer_rect.height) > content_rows {
            assert_eq!(usize::from(areas.body.height), layout.body.len());
            assert_eq!(usize::from(areas.options.height), layout.option_rows());
            assert_eq!(usize::from(areas.input.height), layout.input_rows());
        }
        if app.render.composer_rect.height == layout.height() {
            assert_eq!(areas.body.y, app.render.composer_rect.y + 1);
            assert!(row(app.render.composer_rect.y).trim().is_empty());
            if layout.input.is_some() {
                assert_eq!(areas.input.y, areas.options.bottom() + 1);
                assert!(row(areas.input.y - 1).trim().is_empty());
            }
            let last = if layout.input.is_some() {
                areas.input.bottom()
            } else {
                areas.options.bottom()
            };
            assert_eq!(areas.help.y, last + 1);
            assert!(row(areas.help.y - 1).trim().is_empty());
        }
    }

    /// The focused suggestion's preview stands beside the suggestions in a wide panel and
    /// under that suggestion in a narrow one, wrapping within the room after its marker;
    /// a suggestion without one says so.
    #[tokio::test]
    async fn previews_stand_beside_wide_suggestions_and_under_narrow_ones() {
        use crate::tui::app::tests::{asking, fixture, option, titled};
        let (_root, mut app) = fixture().await;
        let edge = format!("{} edgemark", "x".repeat(46));
        let previewed = skyhook::agent::QuestionOption {
            preview: Some(format!("`shape-a`\n\n{edge}").parse().unwrap()),
            ..option("a", "First")
        };
        let options = vec![previewed, option("b", "Second")];
        let _asked = asking(
            &mut app,
            false,
            vec![titled("shape", "Which shape?", options)],
        );
        let rows = |app: &mut App, width| {
            let screen = crate::tui::app::tests::draw_sized(app, width, 40);
            screen.lines().map(str::to_owned).collect::<Vec<_>>()
        };
        let wide = rows(&mut app, 120);
        assert!(
            wide.iter()
                .any(|row| row.contains("1: a") && row.contains("shape-a")),
            "{wide:#?}"
        );
        let narrow = rows(&mut app, 60);
        let label = narrow.iter().position(|row| row.contains("1: a")).unwrap();
        let preview = narrow
            .iter()
            .position(|row| row.contains("shape-a"))
            .unwrap();
        assert!(
            preview > label && !narrow[label].contains("shape-a"),
            "{narrow:#?}"
        );
        assert!(
            narrow.iter().any(|row| row.contains("edgemark")),
            "{narrow:#?}"
        );
        app.prompt_input_mut().choice = 1;
        let wide = rows(&mut app, 120);
        assert!(
            wide.iter().any(|row| row.contains("No preview")),
            "{wide:#?}"
        );
        assert!(!wide.iter().any(|row| row.contains("shape-a")), "{wide:#?}");
    }

    /// A preview beside the suggestions taller than the panel scrolls with them; one
    /// fitting it stays in view however far they scroll, by hand or to reach a
    /// suggestion.
    #[tokio::test]
    async fn side_previews_scroll_only_as_far_as_they_overflow() {
        use crate::tui::app::tests::{asking, draw_sized, fixture, option, titled};
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let (_root, mut app) = fixture().await;
        let tall = (0..30)
            .map(|row| format!("row {row}\n\n"))
            .collect::<String>();
        let preview = |index, text: String| skyhook::agent::QuestionOption {
            preview: Some(text.parse().unwrap()),
            ..option(&format!("choice {index}"), "Why")
        };
        let options = std::iter::once(preview(0, format!("{tall}last-row")))
            .chain((1..12).map(|index| preview(index, format!("preview-{index}"))))
            .collect();
        let _asked = asking(&mut app, false, vec![titled("many", "Which?", options)]);
        let press = |app: &mut App, key: KeyEvent, times| {
            for _ in 0..times {
                draw_sized(app, 120, 20);
                app.event(Event::Key(key));
            }
            draw_sized(app, 120, 20)
        };
        let page_down = KeyEvent::new(KeyCode::PageDown, KeyModifiers::CONTROL);
        let screen = press(&mut app, page_down, 30);
        assert!(screen.contains("last-row"), "{screen}");
        let screen = press(&mut app, KeyEvent::from(KeyCode::Down), 1);
        assert!(screen.contains("preview-1"), "{screen}");
        let screen = press(&mut app, KeyEvent::from(KeyCode::Down), 10);
        assert!(screen.contains("preview-11"), "{screen}");
    }

    /// Previews of different sizes take the same room, their code boxes the column's
    /// width, so moving between suggestions moves nothing, beside them or under them.
    #[tokio::test]
    async fn every_preview_takes_the_room_of_the_tallest() {
        use crate::tui::app::tests::{asking, fixture, option, titled};
        let (_root, mut app) = fixture().await;
        let previewed = |label, preview: &str| skyhook::agent::QuestionOption {
            preview: Some(preview.parse().unwrap()),
            ..option(label, "Why")
        };
        let options = vec![
            previewed("short", "```rust\nlet a = 1;\n```"),
            previewed(
                "long",
                "```rust\nlet b = 2;\nlet c = 3;\nlet longer_name = 4;\n```",
            ),
        ];
        let _asked = asking(&mut app, false, vec![titled("shape", "Which?", options)]);
        for width in [120, 60] {
            let shown = |app: &mut App, choice| {
                app.prompt_input_mut().choice = choice;
                let layout = PromptLayout::new(app, width);
                let preview = match &layout.side {
                    Some(side) => side.lines.clone(),
                    None => (layout.options.iter())
                        .filter(|line| line.index == choice && line.description)
                        .map(|line| line.text.clone())
                        .collect(),
                };
                let boxes: Vec<_> = (preview.iter())
                    .filter_map(|line| line.layout.code().map(|code| code.width()))
                    .collect();
                (
                    layout.height(),
                    layout.option_rows(),
                    boxes.first().copied(),
                )
            };
            let (short, long) = (shown(&mut app, 0), shown(&mut app, 1));
            assert_eq!((short.0, short.1), (long.0, long.1), "width {width}");
            assert_eq!(short.2, long.2, "width {width}");
        }
    }

    /// A batch keeps the height of its tallest question on every page, review included,
    /// so switching between them leaves the panel still.
    #[tokio::test]
    async fn a_batch_keeps_its_tallest_pages_height() {
        use crate::tui::app::tests::{asking, fixture, option, titled};
        let (_root, mut app) = fixture().await;
        let many = (0..6)
            .map(|index| option(&format!("choice {index}"), "Why"))
            .collect();
        let long = "Which?\n\n- first point\n- second point\n- third point";
        let batch = vec![titled("short", "Yes?", vec![]), titled("long", long, many)];
        let _asked = asking(&mut app, false, batch);
        let heights: Vec<_> = (0..3)
            .map(|page| {
                app.select_page(page);
                PromptLayout::new(&app, 80).height()
            })
            .collect();
        assert!(
            heights.iter().all(|height| *height == heights[1]),
            "{heights:?}"
        );
    }

    #[tokio::test]
    async fn question_answer_and_cursor_survive_collapsed_prompt_padding() {
        let (_root, mut app) = draft_fixture().await;
        let (reply, _answer) = oneshot::channel();
        let question = crate::tui::app::tests::titled("answer", "What should I call you?", vec![]);
        app.prompt(Prompt {
            id: 1,
            kind: PromptKind::Questions {
                questions: vec![question],
                agent: app.root_agent().clone(),
                background: false,
                reply,
            },
        });
        app.event(Event::Paste("visible answer".into()));
        for (width, height) in [(80, 9), (40, 12), (80, 24)] {
            assert_prompt_render(&mut app, width, height, Some("visible answer"));
        }
        app.event(Event::Paste(format!("{}visible answer", "\n".repeat(20))));
        assert_prompt_render(&mut app, 80, 9, Some("visible answer"));
    }

    #[tokio::test]
    async fn rename_prompts_fit_with_optional_sections() {
        let (_root, mut app) = draft_fixture().await;
        let (reply, _answer) = oneshot::channel();
        app.prompt(Prompt {
            id: 1,
            kind: PromptKind::Rename {
                current: "visible title".into(),
                reply,
            },
        });
        for (width, height) in [(80, 9), (40, 12), (80, 24)] {
            assert_prompt_render(&mut app, width, height, Some("visible title"));
        }
    }

    #[test]
    fn prompt_choices_wrap_without_losing_unicode_or_selection_targets() {
        let options = vec![
            ("abc 界 👩‍💻 def".into(), "Details 界 👩‍💻 on a new line".into()),
            (
                "\u{1b}[31msecond\u{1b}[0m choice".into(),
                "Muted details".into(),
            ),
            ("Write an answer…".into(), String::new()),
        ];
        for width in [6, 12, 30] {
            let lay_out = |source: &str, width| {
                markdown::layout_highlighted(source, false, width, None).lines
            };
            let (rows, selected_start) = prompt_option_lines(&options, 1, width, None, lay_out);
            assert_eq!(rows[selected_start].index, 1);
            assert_eq!(rows[selected_start].marker, "> ");
            assert_eq!(rows.iter().filter(|row| row.marker == "> ").count(), 1);
            // Separators stay on the row they end, past its visible edge.
            assert!(rows.iter().all(|row| {
                let text = row.text.line.to_string();
                row.marker.width() + text.trim_end().width() <= width as usize
            }));
            for (index, (label, description)) in options.iter().enumerate() {
                let choice: Vec<_> = rows.iter().filter(|row| row.index == index).collect();
                assert!(!choice[0].description);
                for (is_description, source) in [(false, label), (true, description)] {
                    let parts = choice
                        .iter()
                        .filter(|row| row.description == is_description);
                    let reconstructed: String =
                        parts.map(|row| row.text.line.to_string()).collect();
                    assert_eq!(reconstructed, format::clean(source));
                }
                // Descriptions follow the label rows contiguously.
                if let Some(first) = choice.iter().position(|row| row.description) {
                    assert!(first > 0 && choice[first..].iter().all(|row| row.description));
                }
            }
            for row in &rows {
                let style = row.style(1);
                assert_eq!(
                    style.bg,
                    Some(if row.index == 1 {
                        theme().ui.selected
                    } else {
                        theme().ui.input
                    })
                );
                assert_eq!(
                    style.fg,
                    Some(if row.description {
                        theme().ui.muted
                    } else {
                        theme().ui.fg
                    })
                );
            }
        }
    }
}
