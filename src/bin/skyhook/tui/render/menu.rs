//! Agent identity and command menu rendering.

use super::*;
use skyhook::job::JobState;

pub(super) fn agent_status_color(state: model::AgentDisplayState, p: Palette) -> Color {
    use model::AgentDisplayState as State;
    match state {
        State::Job(JobState::Failed) => p.content.error,
        State::Waiting(_) | State::Job(JobState::AwaitingApproval | JobState::WaitingInput) => {
            p.content.warning
        }
        State::Working | State::Reconnecting { .. } | State::Compacting | State::RunningTools => {
            p.content.primary
        }
        State::Job(JobState::Completed) => p.content.success,
        State::Ready | State::Job(_) => p.muted,
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
    state: model::AgentDisplayState,
    terminal: bool,
    tick: usize,
) -> &'static str {
    use model::{AgentDisplayState as State, WaitReason};
    if state.running() {
        return spinner(tick);
    }
    match state {
        State::Job(JobState::Failed) => "✗",
        State::Job(JobState::Cancelled | JobState::Interrupted) => "■",
        State::Waiting(WaitReason::Permission) | State::Job(JobState::AwaitingApproval) => "◇",
        State::Waiting(WaitReason::Input | WaitReason::ParentInput)
        | State::Job(JobState::WaitingInput) => "?",
        State::Waiting(WaitReason::Child) => "◷",
        _ if terminal => "✓",
        _ => "·",
    }
}
pub(super) fn draw_menu_item(
    frame: &mut Frame,
    row: Rect,
    item: &super::super::app::ItemRef<'_>,
    kind: &MenuKind,
    selected: bool,
    p: Palette,
    bg: Color,
) {
    let label = model::clean(item.label);
    let detail = model::clean(item.detail);
    let fg = if selected && !matches!(kind, MenuKind::Output(_, _)) {
        p.content.primary
    } else {
        p.fg
    };
    if matches!(kind, MenuKind::Commands(_)) {
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
    // Avoid spending scarce identity space on margins in narrow palettes.
    let minimum_width = if matches!(menu.kind, MenuKind::Agents(_)) {
        31
    } else {
        20
    };
    let margin = 7.min(area.width.saturating_sub(minimum_width) / 2);
    let width = area.width.saturating_sub(margin * 2);
    // Likewise a short transcript keeps every row for the palette.
    let gap = 1.min(area.height.saturating_sub(6) / 2);
    let rect = r(area.x + margin, area.y + gap, width, area.height - gap * 2);
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
        format!("> {}▏", model::clean(menu.input.text())),
        p.fg,
        p.input,
    );
    let items = menu.filtered();
    let agent_menu = matches!(menu.kind, MenuKind::Agents(_));
    let agent_stats = if agent_menu {
        app.projection
            .agents
            .iter()
            .map(|agent| model::agent_footer_stats(&app.snapshot, &app.projection, &agent.id))
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let row_width = width.saturating_sub(2);
    let stats_columns = AgentStatsColumns::menu(agent_stats.iter());
    let minimum_identity_width =
        AgentColumnsLayout::minimum_identity_width(&app.projection.agents, row_width);
    let columns = AgentColumnsLayout::new(row_width, minimum_identity_width, stats_columns.width());
    let headers = AGENT_STATS_HEADERS.map(String::from);
    let header_height = u16::from(agent_menu && columns.stats_width > 0);
    if header_height > 0 {
        stats_columns.draw(
            frame,
            r(
                rect.right() - 1 - columns.stats_width,
                rect.y + 2,
                columns.stats_width,
                header_height.min(rect.height.saturating_sub(2)),
            ),
            &headers,
            p.muted,
            p.input,
        );
    }
    let height = rect.height.saturating_sub(3 + header_height) as usize;
    // A text page has nothing to pick: its selection is the first visible line.
    let page = matches!(menu.kind, MenuKind::Info(_));
    let top = if page {
        menu.selected.min(items.len().saturating_sub(height))
    } else {
        menu.selected.saturating_sub(height.saturating_sub(1))
    };
    for (i, item) in items.iter().enumerate().skip(top).take(height) {
        let y = rect.y + 2 + header_height + (i - top) as u16;
        let selected = i == menu.selected && !page;
        let bg = if selected { p.selected } else { p.input };
        let row = r(rect.x + 1, y, width.saturating_sub(2), 1);
        if let MenuKind::Agents(agents) = &menu.kind {
            if let Some(agent) = app
                .projection
                .agents
                .iter()
                .find(|agent| agent.id == agents[item.index].value)
            {
                let state = app.agent_status(agent);
                app.animating |= state.running();
                let symbol = agent_symbol(state, agent.terminal(), app.tick_count);
                let stats = model::agent_footer_stats(&app.snapshot, &app.projection, &agent.id);
                let target = model::target_suffix(&agent.target);
                let indent = (agent.id.depth() as u16 * 4).min(row.width / 3);
                let name_width = columns.identity_width;
                let name = agent_identity(
                    &agent.name,
                    &target,
                    &" ".repeat(indent as usize),
                    Span::styled(symbol, Style::default().fg(agent_status_color(state, p))),
                    name_width,
                    p,
                );
                fill(frame, row, bg);
                text(frame, r(row.x, y, name_width, 1), name, p.fg, bg);
                if columns.status_width > 0 {
                    text(
                        frame,
                        r(row.x + name_width + 2, y, columns.status_width, 1),
                        state.label(),
                        agent_status_color(state, p),
                        bg,
                    );
                }
                if columns.stats_width > 0 {
                    stats_columns.draw(
                        frame,
                        r(row.right() - columns.stats_width, y, columns.stats_width, 1),
                        &stats,
                        p.muted,
                        bg,
                    );
                }
            }
        } else if let MenuKind::Sessions(sessions) = &menu.kind {
            // Open sessions carry their live status; saved ones align beneath them.
            fill(frame, row, bg);
            let peer = match sessions[item.index].value {
                SessionRef::Live(key) => app.peers.iter().find(|peer| peer.key == key),
                SessionRef::Saved(_) => None,
            };
            if let Some(peer) = peer {
                app.animating |= peer.state.running();
                let symbol = agent_symbol(peer.state, false, app.tick_count);
                let color = agent_status_color(peer.state, p);
                text(frame, r(row.x, y, 1, 1), symbol, color, bg);
            }
            let row = r(row.x + 2, y, row.width.saturating_sub(2), 1);
            draw_menu_item(frame, row, item, &menu.kind, selected, p, bg);
        } else {
            draw_menu_item(frame, row, item, &menu.kind, selected, p, bg);
        }
        if selected {
            focus_cursor(frame, rect.x, y, p.input);
        }
        app.hits.push((row, Hit::Menu(i)));
    }
    if items.is_empty() && !matches!(menu.kind, MenuKind::OutputSearch(_)) {
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
    use ratatui::{Terminal, backend::TestBackend};

    #[tokio::test]
    async fn only_pickable_rows_are_highlighted() {
        let (_root, mut app) = crate::tui::app::tests::fixture().await;
        let p = Palette::new();
        for (command, highlighted) in [
            (crate::tui::keys::Command::Help, false),
            (crate::tui::keys::Command::Commands, true),
        ] {
            app.command(command);
            let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
            app.content_rect = r(0, 0, 80, 20);
            terminal
                .draw(|frame| draw_menu(frame, &mut app, p))
                .unwrap();
            let cells = &terminal.backend().buffer().content;
            let found = cells.iter().any(|cell| cell.bg == p.selected);
            assert_eq!(found, highlighted, "{command}");
            app.menu = None;
        }
    }

    #[tokio::test]
    async fn agents_palette_hides_columns_without_stacking_rows() {
        let (_root, mut app) = crate::tui::app::tests::fixture().await;
        let root = app.projection.agents[0].clone();
        let agents: Vec<_> = (0..2)
            .map(|index| {
                let mut agent = root.clone();
                agent.id = root.id.child(index);
                agent.name = format!("agent-{index}");
                agent
            })
            .collect();
        app.projection.agents = agents.clone();
        for agent in agents {
            app.projection.complete_agent(&agent.id);
        }
        app.command(crate::tui::keys::Command::Agents);
        for (width, stats, status) in [
            (40, false, false),
            (65, false, false),
            (66, false, true),
            (80, false, true),
            (120, true, true),
        ] {
            let mut terminal = Terminal::new(TestBackend::new(width, 10)).unwrap();
            app.content_rect = r(0, 0, width, 10);
            app.hits.clear();
            terminal
                .draw(|frame| draw_menu(frame, &mut app, Palette::new()))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let line = |y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            };
            // One clear row separates the palette from the bars above and below it.
            assert!(line(0).trim().is_empty() && line(9).trim().is_empty());
            assert_eq!(line(3).contains("Input (uncached)"), stats);
            let hits = app.hits.iter();
            let rows: Vec<_> = hits
                .filter_map(|(rect, hit)| matches!(hit, Hit::Menu(_)).then_some(*rect))
                .collect();
            assert_eq!(rows.len(), 2);
            for (index, row) in rows.iter().enumerate() {
                assert_eq!(
                    (row.height, row.y),
                    (1, 3 + u16::from(stats) + index as u16)
                );
                let text = line(row.y);
                assert!(text.contains(&format!("agent-{index}")));
                assert_eq!(text.contains("Completed"), status);
            }
        }
    }

    #[test]
    fn menu_hints_are_muted_right_aligned_and_yield_to_labels() {
        let p = Palette::new();
        let kind = MenuKind::Commands(vec![]);
        for (label, hint) in [
            ("New session", "ctrl+x n"),
            ("界面", "ctrl+shift+p"),
            ("Unbound", ""),
        ] {
            let item = super::super::super::app::ItemRef {
                index: 0,
                label,
                detail: hint,
            };
            for width in [0, 2, 8, 12, 21, 80] {
                for selected in [false, true] {
                    let mut terminal = Terminal::new(TestBackend::new(90, 3)).unwrap();
                    let row = r(3, 1, width, 1);
                    let bg = if selected { p.selected } else { p.input };
                    terminal
                        .draw(|frame| {
                            fill(frame, frame.area(), p.base);
                            fill(frame, row, bg);
                            draw_menu_item(frame, row, &item, &kind, selected, p, bg);
                        })
                        .unwrap();
                    let buffer = terminal.backend().buffer();
                    let cell = move |x: u16| &buffer[(x, 1)];
                    // Ratatui resets the hidden continuation cells of wide
                    // graphemes. They are not independently painted terminal
                    // cells: the leading cell's style covers the whole glyph.
                    let mut painted = Vec::new();
                    let mut x = row.x;
                    while x < row.right() {
                        painted.push(x);
                        x += cell(x).symbol().width().max(1) as u16;
                    }
                    let actual: String = painted.iter().map(|&x| cell(x).symbol()).collect();
                    let fits =
                        !hint.is_empty() && label.width() + hint.width() + 2 <= width as usize;
                    if fits {
                        assert!(actual.ends_with(hint), "{actual:?}");
                        let start = row.right() - hint.width() as u16;
                        assert!((start..row.right()).all(|x| cell(x).fg == p.muted));
                    } else if !hint.is_empty() {
                        assert!(!actual.contains(hint), "{actual:?}");
                    }
                    if label.width() <= width as usize {
                        assert!(actual.starts_with(label), "{actual:?}");
                    }
                    if width > 0 && !label.contains('界') {
                        let fg = if selected { p.content.primary } else { p.fg };
                        assert_eq!(cell(row.x).fg, fg);
                    }
                    let filled = painted.iter().all(|&x| cell(x).bg == bg);
                    assert!(
                        filled,
                        "row background: {label:?}, {hint:?}, width={width}, selected={selected}"
                    );
                    assert_eq!(cell(row.right()).bg, p.base);
                }
            }
        }
    }
}
