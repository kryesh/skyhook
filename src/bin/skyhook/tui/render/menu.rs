//! Agent identity and command menu rendering.

use super::*;

pub(super) fn agent_status_color(running: bool, status: &str, p: Palette) -> Color {
    if status == "Failed" {
        p.content.error
    } else if status.starts_with("Waiting") || status.starts_with("Reconnecting") {
        p.content.warning
    } else if running {
        p.content.primary
    } else if status == "Completed" {
        p.content.success
    } else {
        p.muted
    }
}

/// Shared by the inline tree and agent inspector. Selection only changes the
/// neutral surface, not the independent identity, target, and state roles.
pub(super) fn agent_identity(
    name: &str,
    target: &str,
    prefix: &str,
    symbol: Span<'static>,
    width: u16,
    p: Palette,
) -> Line<'static> {
    let available = width.saturating_sub((prefix.width() + symbol.width() + 1) as u16);
    let target = model::clean(target);
    let target_width = (target.width() as u16).min(available / 2);
    Line::from(vec![
        Span::styled(prefix.to_owned(), Style::default().fg(p.accent)),
        symbol,
        Span::raw(" "),
        Span::styled(
            clipped_header(&model::clean(name), available.saturating_sub(target_width)),
            Style::default().fg(p.fg),
        ),
        Span::styled(
            clipped_header(&target, target_width),
            Style::default().fg(p.content.accent),
        ),
    ])
}

pub(super) fn agent_symbol(
    running: bool,
    status: &str,
    terminal: bool,
    tick: usize,
) -> &'static str {
    if running {
        spinner(tick)
    } else if status == "Failed" {
        "✗"
    } else if matches!(status, "Cancelled" | "Interrupted") {
        "■"
    } else if status.contains("permission") {
        "◇"
    } else if status.contains("input") {
        "?"
    } else if status.starts_with("Waiting") {
        "◷"
    } else if terminal {
        "✓"
    } else {
        "·"
    }
}
pub(super) fn draw_menu_item(
    frame: &mut Frame,
    row: Rect,
    item: &super::super::app::Item,
    kind: &MenuKind,
    selected: bool,
    p: Palette,
    bg: Color,
) {
    let label = model::clean(&item.label);
    let detail = model::clean(&item.detail);
    let fg = if selected && !matches!(kind, MenuKind::Info | MenuKind::Output(_)) {
        p.content.primary
    } else {
        p.fg
    };
    if matches!(kind, MenuKind::Commands) {
        // Paint the whole row, including the gap between label and shortcut.
        fill(frame, row, bg);
        // Never sacrifice label space for a shortcut. Visible hints share the
        // right edge; an overlong/custom hint is hidden as a whole.
        let hint_width = detail.width();
        let show_hint = hint_width > 0 && label.width() + 2 + hint_width <= row.width as usize;
        let label_width = if show_hint {
            row.width.saturating_sub(hint_width as u16 + 2)
        } else {
            row.width
        };
        text(frame, r(row.x, row.y, label_width, 1), label, fg, bg);
        if show_hint {
            text(
                frame,
                r(row.right() - hint_width as u16, row.y, hint_width as u16, 1),
                detail,
                p.muted,
                bg,
            );
        }
    } else {
        let line = Line::from(vec![
            Span::styled(label, Style::default().fg(fg)),
            Span::styled(
                if detail.is_empty() {
                    detail
                } else {
                    format!("   {detail}")
                },
                Style::default().fg(p.muted),
            ),
        ]);
        text(frame, row, line, fg, bg);
    }
}

pub(super) fn draw_menu(frame: &mut Frame, app: &mut App, p: Palette) {
    app.refresh_agent_menu();
    let Some(menu) = &app.menu else { return };
    let area = app.content_rect;
    // Leave enough room for whole header words before spending space on margins.
    let minimum_width = if matches!(menu.kind, MenuKind::Agents) {
        31
    } else {
        20
    };
    let margin = 7.min(area.width.saturating_sub(minimum_width) / 2);
    let width = area.width.saturating_sub(margin * 2);
    let rect = r(area.x + margin, area.y, width, area.height);
    fill(frame, rect, p.input);
    text(
        frame,
        r(rect.x + 1, rect.y, width.saturating_sub(2), 1),
        menu.title.clone(),
        p.accent,
        p.input,
    );
    text(
        frame,
        r(rect.x + 1, rect.y + 1, width.saturating_sub(2), 1),
        format!("> {}▏", model::clean(&menu.input.text)),
        p.fg,
        p.input,
    );
    let items = menu.filtered();
    let agent_menu = matches!(menu.kind, MenuKind::Agents);
    let agent_stats = if agent_menu {
        app.projection
            .agents
            .iter()
            .map(|agent| model::agent_footer_stats(&app.snapshot, &app.projection, &agent.id))
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let stats_columns = AgentStatsColumns::menu(agent_stats.iter(), width.saturating_sub(2));
    let stats_width = stats_columns.width();
    let headers = AGENT_STATS_HEADERS.map(String::from);
    let header_height = if agent_menu {
        stats_columns.wrapped_height(&headers) as u16
    } else {
        0
    };
    let stats_height = agent_stats
        .iter()
        .map(|row| stats_columns.wrapped_height(row))
        .max()
        .unwrap_or(1);
    // Match the inline tree's aligned status/token columns. On narrow screens,
    // use additional lines so the picker still exposes status and every token field.
    let compact_agents = agent_menu && width.saturating_sub(2) < stats_width + 50;
    let stacked_stats = compact_agents && width.saturating_sub(2) < stats_width + 30;
    let row_height = if stacked_stats {
        2 + stats_height
    } else if compact_agents {
        1 + stats_height
    } else {
        1
    };
    if agent_menu && stats_width <= width.saturating_sub(2) {
        stats_columns.draw(
            frame,
            r(
                rect.right() - 1 - stats_width,
                rect.y + 2,
                stats_width,
                header_height.min(rect.height.saturating_sub(2)),
            ),
            &headers,
            p.muted,
            p.input,
        );
    }
    let height = rect.height.saturating_sub(3 + header_height) as usize / row_height;
    let top = menu.selected.saturating_sub(height.saturating_sub(1));
    for (i, item) in items.iter().enumerate().skip(top).take(height) {
        let y = rect.y + 2 + header_height + ((i - top) * row_height) as u16;
        let selected = i == menu.selected;
        let bg = if selected { p.selected } else { p.input };
        let row = r(rect.x + 1, y, width.saturating_sub(2), 1);
        if agent_menu {
            if let Some(agent) = app
                .projection
                .agents
                .iter()
                .find(|agent| agent.id.to_string() == item.value)
            {
                let (running, status) = app.agent_status(agent);
                app.animating |= running;
                let symbol = agent_symbol(running, &status, agent.terminal, app.tick_count);
                let stats = model::agent_footer_stats(&app.snapshot, &app.projection, &agent.id);
                let target = model::target_suffix(&agent.target);
                let indent = (agent.id.depth() as u16 * 4).min(row.width / 3);
                let name_width = if compact_agents {
                    row.width
                } else {
                    row.width.saturating_sub(stats_width + 32)
                };
                let name = agent_identity(
                    &agent.name,
                    &target,
                    &" ".repeat(indent as usize),
                    Span::styled(
                        symbol,
                        Style::default().fg(agent_status_color(running, &status, p)),
                    ),
                    name_width,
                    p,
                );
                fill(frame, r(row.x, row.y, row.width, row_height as u16), bg);
                text(frame, r(row.x, y, name_width, 1), name, p.fg, bg);
                let status_row = if stacked_stats {
                    r(row.x, y + 1, row.width, 1)
                } else if compact_agents {
                    r(row.x, y + 1, row.width.saturating_sub(stats_width + 2), 1)
                } else {
                    r(row.x + name_width + 2, y, 28, 1)
                };
                text(
                    frame,
                    status_row,
                    status.clone(),
                    agent_status_color(running, &status, p),
                    bg,
                );
                if stats_width <= row.width {
                    stats_columns.draw(
                        frame,
                        r(
                            row.right() - stats_width,
                            status_row.y + u16::from(stacked_stats),
                            stats_width,
                            stats_height as u16,
                        ),
                        &stats,
                        p.muted,
                        bg,
                    );
                }
            }
        } else {
            draw_menu_item(frame, row, item, &menu.kind, selected, p, bg);
        }
        if selected {
            focus_cursor(frame, rect.x, y, p.input);
        }
        app.hits
            .push((r(row.x, row.y, row.width, row_height as u16), Hit::Menu(i)));
    }
    if items.is_empty() && !matches!(menu.kind, MenuKind::Attach | MenuKind::OutputSearch(_)) {
        text(
            frame,
            r(
                rect.x + 1,
                rect.y + 2 + header_height,
                width.saturating_sub(2),
                1,
            ),
            "No matching entries",
            p.muted,
            p.input,
        );
    }
    text(
        frame,
        r(
            rect.x + 1,
            rect.bottom().saturating_sub(1),
            width.saturating_sub(2),
            1,
        ),
        "↑↓ select · Enter open · Esc return",
        p.muted,
        p.input,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_hints_are_muted_right_aligned_and_yield_to_labels() {
        for light in [false, true] {
            let p = Palette::new(light);
            for (label, hint) in [
                ("New session", "ctrl+x n"),
                ("Quit", "alt+q"),
                ("界面", "ctrl+shift+p"),
                ("Unbound", ""),
                ("Long custom shortcut", "ctrl+x ctrl+y ctrl+z"),
            ] {
                for width in [0, 1, 6, 12, 20, 38, 80] {
                    for selected in [false, true] {
                        let item = super::super::super::app::Item {
                            value: String::new(),
                            label: label.into(),
                            detail: hint.into(),
                            attachment: None,
                        };
                        let mut terminal =
                            ratatui::Terminal::new(ratatui::backend::TestBackend::new(90, 3))
                                .unwrap();
                        let row = r(3, 1, width, 1);
                        let bg = if selected { p.selected } else { p.input };
                        terminal
                            .draw(|frame| {
                                fill(frame, frame.area(), p.base);
                                fill(frame, row, bg);
                                draw_menu_item(
                                    frame,
                                    row,
                                    &item,
                                    &MenuKind::Commands,
                                    selected,
                                    p,
                                    bg,
                                );
                            })
                            .unwrap();
                        let buffer = terminal.backend().buffer();
                        let fits =
                            !hint.is_empty() && label.width() + hint.width() + 2 <= width as usize;
                        // Ratatui resets the hidden continuation cells of wide
                        // graphemes. They are not independently painted terminal
                        // cells: the leading cell's style covers the whole glyph.
                        let mut painted = Vec::new();
                        let mut x = row.x;
                        while x < row.right() {
                            painted.push(x);
                            x += buffer[(x, 1)].symbol().width().max(1) as u16;
                        }
                        let actual = painted
                            .iter()
                            .map(|&x| buffer[(x, 1)].symbol())
                            .collect::<String>();
                        if fits {
                            assert!(actual.ends_with(hint), "{actual:?}");
                            let start = row.right() - hint.width() as u16;
                            assert!((start..row.right()).all(|x| buffer[(x, 1)].fg == p.muted));
                        } else if !hint.is_empty() {
                            assert!(!actual.contains(hint), "{actual:?}");
                        }
                        if label.width() <= width as usize {
                            assert!(actual.starts_with(label), "{actual:?}");
                        }
                        if width > 0 && !label.contains('界') {
                            assert_eq!(
                                buffer[(row.x, 1)].fg,
                                if selected { p.content.primary } else { p.fg }
                            );
                        }
                        assert!(
                            painted.iter().all(|&x| buffer[(x, 1)].bg == bg),
                            "row background: label={label:?}, hint={hint:?}, width={width}, selected={selected}, light={light}: {:?}",
                            (row.x..row.right())
                                .map(|x| (x, buffer[(x, 1)].symbol(), buffer[(x, 1)].bg))
                                .collect::<Vec<_>>()
                        );
                        assert_eq!(buffer[(row.right(), 1)].bg, p.base);
                    }
                }
            }
        }
    }
}
