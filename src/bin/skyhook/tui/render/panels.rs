//! Interactive prompt and composer panel rendering.

use super::*;

pub(super) fn draw_prompt(frame: &mut Frame, app: &mut App, p: Palette) {
    let rect = app.composer_rect;
    let options = app.prompt_options();
    let body = app.prompt_text();
    let lines = wrap_plain(&model::clean(&body), rect.width.saturating_sub(4) as usize);
    let available = rect.height.saturating_sub(2);
    let body_height = if options.is_empty() {
        available
    } else {
        (available / 2).max(1)
    };
    let option_height = if options.is_empty() {
        0
    } else {
        available.saturating_sub(body_height)
    };
    app.prompt_body_rect = r(2, rect.y, rect.width.saturating_sub(4), body_height);
    app.prompt_options_rect = r(
        2,
        rect.y + body_height,
        rect.width.saturating_sub(4),
        option_height,
    );
    app.prompt_body_rows = lines.len();
    app.prompt_body_scroll = app
        .prompt_body_scroll
        .min(lines.len().saturating_sub(body_height as usize));
    for (offset, line) in lines
        .iter()
        .skip(app.prompt_body_scroll)
        .take(body_height as usize)
        .enumerate()
    {
        text(
            frame,
            r(2, rect.y + offset as u16, rect.width.saturating_sub(4), 1),
            line.clone(),
            p.warning,
            p.input,
        );
    }
    let (option_lines, selected_start) =
        prompt_option_lines(&options, app.prompt_choice, rect.width.saturating_sub(4));
    app.prompt_option_rows = option_lines.len();
    if app.prompt_reveal {
        if selected_start < app.prompt_option_scroll {
            app.prompt_option_scroll = selected_start;
        } else if selected_start >= app.prompt_option_scroll + option_height as usize {
            app.prompt_option_scroll =
                selected_start.saturating_sub(option_height.saturating_sub(1) as usize);
        }
        app.prompt_reveal = false;
    }
    app.prompt_option_scroll = app
        .prompt_option_scroll
        .min(option_lines.len().saturating_sub(option_height as usize));
    let mut cursor_drawn = false;
    for (offset, (index, line)) in option_lines
        .iter()
        .skip(app.prompt_option_scroll)
        .take(option_height as usize)
        .enumerate()
    {
        let row = r(
            2,
            rect.y + body_height + offset as u16,
            rect.width.saturating_sub(4),
            1,
        );
        text(
            frame,
            row,
            line.clone(),
            p.fg,
            if *index == app.prompt_choice {
                p.selected
            } else {
                p.input
            },
        );
        if *index == app.prompt_choice
            && !cursor_drawn
            && app.menu.is_none()
            && !(app.multiple_questions() && app.question_editing)
        {
            focus_cursor(frame, row.x - 1, row.y, p.input);
            cursor_drawn = true;
        }
        app.hits.push((row, Hit::PromptChoice(*index)));
    }
    let secret = app.prompts.front().is_some_and(|prompt| prompt.secret());
    let input = if secret {
        "●".repeat(app.prompt_editor.text.graphemes(true).count())
    } else {
        if app.multiple_questions() {
            if app.question_editing {
                let cursor = app.prompt_editor.cursor;
                format!(
                    "{}▏{}",
                    model::clean(&app.prompt_editor.text[..cursor]),
                    model::clean(&app.prompt_editor.text[cursor..])
                )
            } else {
                model::clean(&app.prompt_editor.text)
            }
        } else {
            format!("{}▏", model::clean(&app.prompt_editor.text))
        }
    };
    let input_label = match app.prompts.front().map(|prompt| &prompt.kind) {
        Some(crate::interaction::PromptKind::Questions { questions, .. }) => {
            match questions.get(app.question_index) {
                Some(question) if app.prompt_choice < question.options.len() => {
                    "Comment (optional): "
                }
                Some(_) => "Answer: ",
                None => "",
            }
        }
        _ => "",
    };
    text(
        frame,
        r(
            2,
            rect.bottom().saturating_sub(2),
            rect.width.saturating_sub(4),
            1,
        ),
        format!("{input_label}{input}{}", if secret { "▏" } else { "" }),
        p.fg,
        p.input,
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
                if questions.len() > 1 && app.question_index < questions.len() =>
            {
                format!(
                    "Question {}/{} · {} · ↑↓ choose · Enter answer · Esc dismiss",
                    app.question_index + 1,
                    questions.len(),
                    if app.question_editing {
                        "←→ cursor · Tab switch questions"
                    } else {
                        "←→ switch · Tab edit"
                    }
                )
            }
            _ => "↑↓ choose · PgUp/PgDn text · Ctrl+PgUp/PgDn choices · Enter submit · Esc dismiss"
                .into(),
        },
        p.muted,
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
    let minimum_name_width = tree_agents
        .iter()
        .map(|agent| {
            let indent = (agent.id.depth() as u16 * 4).min(width / 3);
            indent + 16.max(model::target_suffix(&agent.target).width() as u16 + 8)
        })
        .max()
        .unwrap_or(16);
    let columns = AgentColumnsLayout::new(
        width.saturating_sub(4),
        width,
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
        let (running, status) = app.agent_status(agent);
        app.animating |= running;
        let symbol = agent_symbol(running, &status, agent.terminal, app.tick_count);
        let indent = (agent.id.depth() as u16 * 4).min(width / 3);
        let target = model::target_suffix(&agent.target);
        let stats = stats_columns.format(&agent_stats[index]);
        let name_width = columns.identity_width.saturating_sub(indent);
        let name = agent_identity(
            &agent.name,
            &target,
            if selected { "> " } else { "  " },
            Span::styled(
                symbol,
                Style::default().fg(agent_status_color(running, &status, p)),
            ),
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
                status.clone(),
                agent_status_color(running, &status, p),
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

/// Retain each choice's hit target across wrapped continuation rows.
fn prompt_option_lines(
    options: &[String],
    selected: usize,
    width: u16,
) -> (Vec<(usize, String)>, usize) {
    let mut option_lines = Vec::new();
    let mut selected_start = 0;
    for (index, option) in options.iter().enumerate() {
        if index == selected {
            selected_start = option_lines.len();
        }
        for (offset, line) in wrap_plain(
            &model::clean(option),
            width.saturating_sub(2).max(1) as usize,
        )
        .into_iter()
        .enumerate()
        {
            option_lines.push((
                index,
                format!(
                    "{} {line}",
                    if offset == 0 && index == selected {
                        ">"
                    } else {
                        " "
                    }
                ),
            ));
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
            "abc 界 👩‍💻 def".into(),
            "\u{1b}[31msecond\u{1b}[0m choice".into(),
        ];
        for width in [6, 12, 30] {
            let (rows, selected_start) = prompt_option_lines(&options, 1, width);
            assert_eq!(rows[selected_start].0, 1);
            assert!(rows[selected_start].1.starts_with("> "));
            assert_eq!(
                rows.iter()
                    .filter(|(_, text)| text.starts_with("> "))
                    .count(),
                1
            );
            assert!(rows.iter().all(|(_, text)| text.width() <= width as usize));
            for (index, source) in options.iter().enumerate() {
                let reconstructed = rows
                    .iter()
                    .filter(|(choice, _)| *choice == index)
                    .map(|(_, text)| &text[2..])
                    .collect::<String>();
                assert_eq!(reconstructed, model::clean(source));
            }
        }
    }
}
