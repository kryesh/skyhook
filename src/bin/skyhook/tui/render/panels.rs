//! Interactive prompt and composer panel rendering.

use super::*;

/// Measure the same wrapped content used for painting, so long questions,
/// choices and typed answers can grow the prompt instead of being clipped.
pub(super) struct PromptLayout {
    body: Vec<String>,
    title_start: Option<usize>,
    options: Vec<PromptOptionLine>,
    selected_start: usize,
    /// Absent for prompts answered by choice alone.
    input: Option<super::super::composer::ComposerLayout>,
    input_label: Vec<String>,
}

impl PromptLayout {
    pub(super) fn new(app: &App, width: u16) -> Self {
        let width = width.saturating_sub(4);
        let body_text = model::clean(&app.prompt_text());
        let body = wrap_plain(&body_text, width as usize);
        let question = match app.prompts.front().map(|prompt| &prompt.kind) {
            Some(crate::interaction::PromptKind::Questions { questions, .. }) => {
                questions.get(app.question_index())
            }
            _ => None,
        };
        let header = question.and_then(|_| body_text.split_once('\n'));
        let title_start = header.map(|(header, _)| wrap_plain(header, width as usize).len());
        let choices = match question {
            Some(question) => question
                .options
                .iter()
                .enumerate()
                .map(|(index, option)| {
                    (
                        format!("{}: {}", index + 1, option.label),
                        option.description.clone(),
                    )
                })
                .chain(std::iter::once(("Write an answer…".into(), String::new())))
                .collect::<Vec<_>>(),
            None => app
                .prompt_options()
                .into_iter()
                .map(|label| (label.to_owned(), String::new()))
                .collect(),
        };
        let (options, selected_start) =
            prompt_option_lines(&choices, app.prompt_input().choice, width);
        let prompt = app.prompts.front();
        let input = if !prompt.is_some_and(|prompt| prompt.takes_text()) {
            None
        } else if prompt.is_some_and(|prompt| prompt.secret()) {
            // Never give a password to the renderer. Map source graphemes onto
            // mask bytes before using the same layout as a regular input field.
            let editor = &app.prompt_input().editor;
            let mask = |byte| editor.text()[..byte].graphemes(true).count() * "●".len();
            let mut masked = super::super::editor::Editor::default();
            masked.set("●".repeat(editor.text().graphemes(true).count()));
            let (anchor, cursor) = (editor.anchor().map(mask), mask(editor.cursor()));
            masked.set_selection(anchor, cursor);
            Some(masked.layout(width as usize))
        } else {
            Some(app.prompt_input().editor.layout(width as usize))
        };
        let input_label = match question {
            Some(question) if app.prompt_input().choice < question.options.len() => {
                "Comment (optional):"
            }
            Some(_) => "Answer:",
            None => "",
        };
        let input_label = if input_label.is_empty() {
            Vec::new()
        } else {
            wrap_plain(input_label, width as usize)
        };
        Self {
            body,
            title_start,
            options,
            selected_start,
            input,
            input_label,
        }
    }

    pub(super) fn height(&self) -> u16 {
        (self.body.len() + self.options.len() + self.input_rows() + 1).min(u16::MAX as usize) as u16
    }

    fn input_rows(&self) -> usize {
        self.input.as_ref().map_or(0, |input| input.rows.len()) + self.input_label.len()
    }

    fn row_heights(&self, height: u16) -> [u16; 3] {
        let wanted = [self.body.len(), self.options.len(), self.input_rows()];
        let mut rows = [0; 3];
        let mut remaining = height.saturating_sub(1);
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
        rows
    }
}

pub(super) fn draw_prompt(frame: &mut Frame, app: &mut App, layout: &PromptLayout) {
    let rect = app.composer_rect;
    let lines = &layout.body;
    let option_lines = &layout.options;
    let selected_start = layout.selected_start;
    let [body_height, option_height, input_height] = layout.row_heights(rect.height);
    app.prompt_body_rect = r(2, rect.y, rect.width.saturating_sub(4), body_height);
    app.prompt_options_rect = r(
        2,
        rect.y + body_height,
        rect.width.saturating_sub(4),
        option_height,
    );
    app.prompt_body_rows = lines.len();
    app.prompt_input_mut().body_scroll = app
        .prompt_input()
        .body_scroll
        .min(lines.len().saturating_sub(body_height as usize));
    for (offset, line) in lines
        .iter()
        .skip(app.prompt_input().body_scroll)
        .take(body_height as usize)
        .enumerate()
    {
        let title = layout
            .title_start
            .is_some_and(|start| app.prompt_input().body_scroll + offset >= start);
        let style = if title {
            Style::default()
                .fg(THEME.primary)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(THEME.warning)
        };
        render_line(
            &Line::from(line.as_str()),
            r(2, rect.y + offset as u16, rect.width.saturating_sub(4), 1),
            frame.buffer_mut(),
            style.bg(THEME.input),
        );
    }
    app.prompt_option_rows = option_lines.len();
    let input = app.prompt_input_mut();
    if !input.options_scrolled {
        input.option_scroll = scroll_to(input.option_scroll, selected_start, option_height);
        input.options_scrolled = true;
    }
    input.option_scroll = input
        .option_scroll
        .min(option_lines.len().saturating_sub(option_height as usize));
    let mut cursor_drawn = false;
    for (offset, line) in option_lines
        .iter()
        .skip(app.prompt_input().option_scroll)
        .take(option_height as usize)
        .enumerate()
    {
        let row = r(
            2,
            rect.y + body_height + offset as u16,
            rect.width.saturating_sub(4),
            1,
        );
        render_line(
            &Line::from(line.text.as_str()),
            row,
            frame.buffer_mut(),
            line.style(app.prompt_input().choice),
        );
        if line.index == app.prompt_input().choice
            && !cursor_drawn
            && app.overlay.is_none()
            && !(app.multiple_questions() && app.question_editing())
        {
            focus_cursor(frame, row.x - 1, row.y, THEME.input);
            cursor_drawn = true;
        }
        app.hits.push((row, Hit::PromptChoice(line.index)));
    }
    let label_height = layout
        .input_label
        .len()
        .min(input_height.saturating_sub(1) as usize) as u16;
    let input_y = rect.bottom().saturating_sub(input_height + 1);
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
            THEME.muted,
            THEME.input,
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
                    Style::default().bg(THEME.selected),
                ),
                THEME.fg,
                THEME.input,
            );
        }
        if visible > 0
            && (!app.multiple_questions() || app.question_editing())
            && app.overlay.is_none()
        {
            frame.set_cursor_position((
                2 + (cursor_column as u16).min(rect.width.saturating_sub(4)),
                input_y + label_height + (cursor_row - input_top) as u16,
            ));
        }
    }
    let (hint, color) = match app.prompts.front().map(|prompt| &prompt.kind) {
        Some(crate::interaction::PromptKind::Questions { questions, .. })
            if questions.len() > 1 && app.question_index() < questions.len() =>
        {
            let keys = if app.question_editing() {
                "←→ cursor · Tab switch questions"
            } else {
                "←→ switch · Tab edit"
            };
            let (index, count) = (app.question_index() + 1, questions.len());
            let hint = format!(
                "Question {index}/{count} · {keys} · ↑↓ choose · Enter answer · Esc cancel"
            );
            (hint, THEME.warning)
        }
        kind => {
            let escape = match kind {
                Some(crate::interaction::PromptKind::Approval { .. }) => "dismiss",
                _ => "cancel",
            };
            let hint = format!(
                "↑↓ choose · PgUp/PgDn text · Ctrl+PgUp/PgDn choices · Enter submit · Esc {escape}"
            );
            (hint, THEME.muted)
        }
    };
    let footer = r(
        2,
        rect.bottom().saturating_sub(1),
        rect.width.saturating_sub(4),
        1,
    );
    text(frame, footer, hint, color, THEME.input);
}

/// Move a `height`-row window's first row as little as possible to show `cursor`.
fn scroll_to(top: usize, cursor: usize, height: u16) -> usize {
    top.min(cursor)
        .max((cursor + 1).saturating_sub(height as usize))
}

/// Visible agent hierarchy with shared statistics and navigation hit targets.
pub(super) fn draw_tree(
    frame: &mut Frame,
    app: &mut App,
    tree_agents: &[usize],
    navigation_active: bool,
) {
    let width = app.tree_rect.width;
    let tree_rows = app.tree_rect.height.saturating_sub(2);
    let tree_y = app.tree_rect.y;
    fill(frame, app.tree_rect, THEME.panel);
    app.tree_cursor = app.tree_cursor.min(tree_agents.len().saturating_sub(1));
    if app.focus == Focus::Tree {
        app.tree_scroll = scroll_to(app.tree_scroll, app.tree_cursor, tree_rows);
    }
    app.tree_scroll = app
        .tree_scroll
        .min(tree_agents.len().saturating_sub(tree_rows as usize));
    let agents = tree_agents
        .iter()
        .map(|&index| &app.projection.agents[index]);
    let agent_stats: Vec<_> = agents
        .clone()
        .map(|agent| app.projection.agent_stats(&app.snapshot, &agent.id))
        .collect();
    let stats = AgentStatsColumns::new(&agent_stats);
    let row_width = width.saturating_sub(4);
    let columns = AgentColumns::new(agents, row_width, stats);
    let track = r(width - 1, tree_y + 1, 1, tree_rows);
    scroll_thumb(
        frame,
        track,
        app.tree_scroll,
        tree_agents.len(),
        THEME.panel,
    );
    let rows = tree_agents.iter().enumerate();
    for (index, &agent_index) in rows.skip(app.tree_scroll).take(tree_rows as usize) {
        let agent = &app.projection.agents[agent_index];
        let y = tree_y + 1 + (index - app.tree_scroll) as u16;
        let selected = agent.id == app.selected;
        let focused = navigation_active && app.focus == Focus::Tree && app.tree_cursor == index;
        let rect = r(2, y, row_width, 1);
        let hover = navigation_active && app.hover.is_some_and(|point| rect.contains(point.into()));
        let bg = if selected || focused || hover {
            THEME.selected
        } else {
            THEME.panel
        };
        let row = AgentRow {
            agent,
            state: app.agent_status(agent),
            stats: &agent_stats[index],
            marker: if selected { "> " } else { "  " },
        };
        app.animating |= row.state.running();
        let x = draw_agent_row(frame, rect, row, &columns, app.tick_count, bg);
        if focused {
            focus_cursor(frame, x, y, bg);
        }
        app.hits.push((rect, Hit::Agent(agent.id.clone())));
    }
}

/// Every label and description row retains the choice's selection and hit target.
struct PromptOptionLine {
    index: usize,
    text: String,
    description: bool,
}

impl PromptOptionLine {
    fn style(&self, selected: usize) -> Style {
        let style = Style::default().bg(if self.index == selected {
            THEME.selected
        } else {
            THEME.input
        });
        if self.description {
            style.fg(THEME.muted)
        } else {
            style.fg(THEME.fg)
        }
    }
}

/// Wrap labels and descriptions independently so descriptions always start on
/// a new line, and measure exactly the rows that draw_prompt will paint.
fn prompt_option_lines(
    options: &[(String, String)],
    selected: usize,
    width: u16,
) -> (Vec<PromptOptionLine>, usize) {
    let mut option_lines = Vec::new();
    let mut selected_start = 0;
    for (index, (label, description)) in options.iter().enumerate() {
        if index == selected {
            selected_start = option_lines.len();
        }
        for (description, source) in [(false, label), (true, description)] {
            let source = model::clean(source);
            if description && source.is_empty() {
                continue;
            }
            for (offset, line) in wrap_plain(&source, width.saturating_sub(2) as usize)
                .into_iter()
                .enumerate()
            {
                option_lines.push(PromptOptionLine {
                    index,
                    text: format!(
                        "{} {line}",
                        if !description && offset == 0 && index == selected {
                            ">"
                        } else {
                            " "
                        }
                    ),
                    description,
                });
            }
        }
    }
    (option_lines, selected_start)
}

#[cfg(test)]
mod tests {
    use super::*;

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
            let (rows, selected_start) = prompt_option_lines(&options, 1, width);
            assert_eq!(rows[selected_start].index, 1);
            assert!(rows[selected_start].text.starts_with("> "));
            assert_eq!(
                rows.iter().filter(|row| row.text.starts_with("> ")).count(),
                1
            );
            // Separators stay on the row they end, past its visible edge.
            assert!(rows.iter().all(|row| {
                row.text.trim_end().width() <= width as usize && !row.text.contains('—')
            }));
            for (index, (label, description)) in options.iter().enumerate() {
                let choice: Vec<_> = rows.iter().filter(|row| row.index == index).collect();
                assert!(!choice[0].description);
                for (is_description, source) in [(false, label), (true, description)] {
                    let parts = choice
                        .iter()
                        .filter(|row| row.description == is_description);
                    let reconstructed: String = parts.map(|row| &row.text[2..]).collect();
                    assert_eq!(reconstructed, model::clean(source));
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
                        THEME.selected
                    } else {
                        THEME.input
                    })
                );
                assert_eq!(
                    style.fg,
                    Some(if row.description {
                        THEME.muted
                    } else {
                        THEME.fg
                    })
                );
                assert!(!style.add_modifier.contains(Modifier::BOLD));
            }
        }
    }
}
