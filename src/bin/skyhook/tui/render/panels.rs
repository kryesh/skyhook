//! Interactive prompt and composer panel rendering.

use super::*;

/// Measure the same wrapped content used for painting, so long questions,
/// choices and typed answers can grow the prompt instead of being clipped.
pub(super) struct PromptLayout {
    body: Vec<String>,
    title_start: Option<usize>,
    options: Vec<PromptOptionLine>,
    selected_start: usize,
    input: super::super::composer::ComposerLayout,
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
                .map(|label| (label, String::new()))
                .collect(),
        };
        let (options, selected_start) =
            prompt_option_lines(&choices, app.prompt_input().choice, width);
        let secret = app.prompts.front().is_some_and(|prompt| prompt.secret());
        let input = if secret {
            // Never give a password to the renderer. Map source graphemes onto
            // mask bytes before using the same layout as a regular input field.
            let editor = &app.prompt_input().editor;
            let mask = |byte| editor.text()[..byte].graphemes(true).count() * "●".len();
            let mut masked = super::super::editor::Editor::default();
            masked.set("●".repeat(editor.text().graphemes(true).count()));
            let (anchor, cursor) = (editor.anchor().map(mask), mask(editor.cursor()));
            masked.set_selection(anchor, cursor);
            masked.layout(width as usize)
        } else {
            app.prompt_input().editor.layout(width as usize)
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
        (self.body.len()
            + self.options.len()
            + (self.input.rows.len() + self.input_label.len())
            + 1)
        .min(u16::MAX as usize) as u16
    }

    fn row_heights(&self, height: u16) -> [u16; 3] {
        let wanted = [
            self.body.len(),
            self.options.len(),
            (self.input.rows.len() + self.input_label.len()),
        ];
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

pub(super) fn draw_prompt(frame: &mut Frame, app: &mut App, p: Palette, layout: &PromptLayout) {
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
            Style::default().fg(p.accent).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(p.warning)
        };
        frame.render_widget(
            Paragraph::new(line.as_str()).style(style.bg(p.input)),
            r(2, rect.y + offset as u16, rect.width.saturating_sub(4), 1),
        );
    }
    app.prompt_option_rows = option_lines.len();
    if !app.prompt_input().options_scrolled {
        if selected_start < app.prompt_input().option_scroll {
            app.prompt_input_mut().option_scroll = selected_start;
        } else if selected_start >= app.prompt_input().option_scroll + option_height as usize {
            app.prompt_input_mut().option_scroll =
                selected_start.saturating_sub(option_height.saturating_sub(1) as usize);
        }
        app.prompt_input_mut().options_scrolled = true;
    }
    app.prompt_input_mut().option_scroll = app
        .prompt_input()
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
        frame.render_widget(
            Paragraph::new(line.text.as_str()).style(line.style(p, app.prompt_input().choice)),
            row,
        );
        if line.index == app.prompt_input().choice
            && !cursor_drawn
            && app.menu.is_none()
            && !(app.multiple_questions() && app.question_editing())
        {
            focus_cursor(frame, row.x - 1, row.y, p.input);
            cursor_drawn = true;
        }
        app.hits.push((row, Hit::PromptChoice(line.index)));
    }
    let label_height = layout
        .input_label
        .len()
        .min(input_height.saturating_sub(1) as usize) as u16;
    let visible = input_height.saturating_sub(label_height) as usize;
    let (cursor_row, cursor_column) = layout.input.cursor;
    let input_top = cursor_row.saturating_sub(visible.saturating_sub(1));
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
            p.muted,
            p.input,
        );
    }
    for (offset, row) in layout
        .input
        .rows
        .iter()
        .skip(input_top)
        .take(visible)
        .enumerate()
    {
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
                Style::default().bg(p.selected),
            ),
            p.fg,
            p.input,
        );
    }
    if visible > 0
        && (!app.multiple_questions() || app.question_editing())
        && app.menu.is_none()
        && app.search_editor.is_none()
    {
        frame.set_cursor_position((
            2 + (cursor_column as u16).min(rect.width.saturating_sub(4)),
            input_y + label_height + (cursor_row - input_top) as u16,
        ));
    }
    let question_counter = matches!(
        app.prompts.front().map(|prompt| &prompt.kind),
        Some(crate::interaction::PromptKind::Questions { questions, .. })
            if questions.len() > 1 && app.question_index() < questions.len()
    );
    text(
        frame,
        r(
            2,
            rect.bottom().saturating_sub(1),
            rect.width.saturating_sub(4),
            1,
        ),
        match app.prompts.front().map(|prompt| &prompt.kind) {
            Some(crate::interaction::PromptKind::Questions { questions, .. })
                if questions.len() > 1 && app.question_index() < questions.len() =>
            {
                format!(
                    "Question {}/{} · {} · ↑↓ choose · Enter answer · Esc cancel",
                    app.question_index() + 1,
                    questions.len(),
                    if app.question_editing() {
                        "←→ cursor · Tab switch questions"
                    } else {
                        "←→ switch · Tab edit"
                    }
                )
            }
            kind => format!(
                "↑↓ choose · PgUp/PgDn text · Ctrl+PgUp/PgDn choices · Enter submit · Esc {}",
                if matches!(kind, Some(crate::interaction::PromptKind::Approval { .. })) {
                    "dismiss"
                } else {
                    "cancel"
                },
            ),
        },
        if question_counter { p.warning } else { p.muted },
        p.input,
    );
}

/// Visible agent hierarchy with shared statistics and navigation hit targets.
pub(super) fn draw_tree(
    frame: &mut Frame,
    app: &mut App,
    p: Palette,
    tree_agents: &[model::AgentInfo],
    tree_rows: u16,
    navigation_active: bool,
) {
    let width = app.tree_rect.width;
    let tree_y = app.tree_rect.y;
    fill(frame, app.tree_rect, p.panel);
    app.tree_cursor = app.tree_cursor.min(tree_agents.len().saturating_sub(1));
    if app.focus == Focus::Tree {
        if app.tree_cursor < app.tree_scroll {
            app.tree_scroll = app.tree_cursor;
        }
        if app.tree_cursor >= app.tree_scroll + tree_rows as usize {
            app.tree_scroll = app.tree_cursor + 1 - tree_rows as usize;
        }
    }
    app.tree_scroll = app
        .tree_scroll
        .min(tree_agents.len().saturating_sub(tree_rows as usize));
    let agent_stats = tree_agents
        .iter()
        .map(|agent| model::agent_footer_stats(&app.snapshot, &app.projection, &agent.id))
        .collect::<Vec<_>>();
    let stats_columns = AgentStatsColumns::new(agent_stats.iter());
    let minimum_name_width = AgentColumnsLayout::minimum_identity_width(tree_agents, width);
    let columns = AgentColumnsLayout::new(
        width.saturating_sub(4),
        minimum_name_width,
        stats_columns.width(),
    );
    for (index, agent) in tree_agents
        .iter()
        .enumerate()
        .skip(app.tree_scroll)
        .take(tree_rows as usize)
    {
        let y = tree_y + 1 + (index - app.tree_scroll) as u16;
        let selected = agent.id == app.selected;
        let focused = navigation_active && app.focus == Focus::Tree && app.tree_cursor == index;
        let rect = r(2, y, width.saturating_sub(4), 1);
        let hover = navigation_active && app.hover.is_some_and(|point| rect.contains(point.into()));
        let bg = if selected || focused || hover {
            p.selected
        } else {
            p.panel
        };
        fill(frame, rect, bg);
        let status = app.agent_status(agent);
        app.animating |= status.running();
        let symbol = agent_symbol(status, agent.terminal(), app.tick_count);
        let indent = (agent.id.depth() as u16 * 4).min(width / 3);
        let target = model::target_suffix(&agent.target);
        let stats = stats_columns.format(&agent_stats[index]);
        let name_width = columns.identity_width.saturating_sub(indent);
        let name = agent_identity(
            &agent.name,
            &target,
            if selected { "> " } else { "  " },
            Span::styled(symbol, Style::default().fg(agent_status_color(status, p))),
            name_width,
            p,
        );
        text(frame, r(2 + indent, y, name_width, 1), name, p.fg, bg);
        if focused {
            focus_cursor(frame, 2 + indent, y, bg);
        }
        if columns.status_width > 0 {
            text(
                frame,
                r(columns.identity_width + 4, y, columns.status_width, 1),
                status.label(),
                agent_status_color(status, p),
                bg,
            );
        }
        if columns.stats_width > 0 {
            text(
                frame,
                r(width - columns.stats_width - 2, y, columns.stats_width, 1),
                stats,
                p.muted,
                bg,
            );
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
    fn style(&self, p: Palette, selected: usize) -> Style {
        let style = Style::default().bg(if self.index == selected {
            p.selected
        } else {
            p.input
        });
        if self.description {
            style.fg(p.muted)
        } else {
            style.fg(p.fg)
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
            for (offset, line) in wrap_plain(&source, width.saturating_sub(2).max(1) as usize)
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
        let p = Palette::new();
        for width in [6, 12, 30] {
            let (rows, selected_start) = prompt_option_lines(&options, 1, width);
            assert_eq!(rows[selected_start].index, 1);
            assert!(rows[selected_start].text.starts_with("> "));
            assert_eq!(
                rows.iter().filter(|row| row.text.starts_with("> ")).count(),
                1
            );
            assert!(
                rows.iter()
                    .all(|row| row.text.width() <= width as usize && !row.text.contains('—'))
            );
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
                let style = row.style(p, 1);
                assert_eq!(
                    style.bg,
                    Some(if row.index == 1 { p.selected } else { p.input })
                );
                assert_eq!(style.fg, Some(if row.description { p.muted } else { p.fg }));
                assert!(!style.add_modifier.contains(Modifier::BOLD));
            }
        }
    }
}
