//! Agent identity and command menu rendering.

use super::*;
use skyhook::job::JobState;

fn agent_status_color(state: model::AgentDisplayState) -> Color {
    use model::AgentDisplayState as State;
    match state {
        State::Job(state) => model::state_role(state).color(),
        State::Waiting(_) => THEME.warning,
        State::Working | State::Reconnecting { .. } | State::Compacting | State::RunningTools => {
            THEME.primary
        }
        State::Ready => THEME.muted,
    }
}

/// One agent in a tree or agents menu row.
pub(super) struct AgentRow<'a> {
    pub(super) agent: &'a model::AgentInfo,
    pub(super) state: model::AgentDisplayState,
    pub(super) stats: &'a [String; 3],
    /// Precedes the status symbol, e.g. the selected agent's `>`.
    pub(super) marker: &'a str,
}

/// Fill `rect` with the row's identity, status and statistics. Returns where
/// the identity starts.
pub(super) fn draw_agent_row(
    frame: &mut Frame,
    rect: Rect,
    row: AgentRow<'_>,
    columns: &AgentColumns,
    tick: usize,
    bg: Color,
) -> u16 {
    let AgentRow {
        agent,
        state,
        stats,
        marker,
    } = row;
    fill(frame, rect, bg);
    let color = agent_status_color(state);
    let symbol = agent_symbol(state, tick);
    let indent = agent_indent(agent, rect.width);
    let name_width = columns.identity_width.saturating_sub(indent);
    let available = name_width.saturating_sub((marker.width() + symbol.width() + 1) as u16);
    let target = model::clean(&model::target_suffix(&agent.target));
    let target_width = (target.width() as u16).min(available / 2);
    // Selection only changes the neutral surface, not the independent
    // identity, target, and state roles.
    let mut identity = Line::from(vec![
        Span::styled(marker.to_owned(), Style::default().fg(THEME.primary)),
        Span::styled(symbol, Style::default().fg(color)),
        Span::raw(" "),
    ]);
    for (text, fg, width) in [
        (
            model::clean(&agent.name),
            THEME.fg,
            available - target_width,
        ),
        (target, THEME.accent, target_width),
    ] {
        let text = Line::from(Span::styled(text, Style::default().fg(fg)));
        identity.spans.extend(clipped(text, width as usize).spans);
    }
    let x = rect.x + indent;
    text(frame, r(x, rect.y, name_width, 1), identity, THEME.fg, bg);
    if columns.status_width > 0 {
        let status = r(
            rect.x + columns.identity_width + 2,
            rect.y,
            columns.status_width,
            1,
        );
        text(frame, status, state.label(), color, bg);
    }
    if columns.stats_width > 0 {
        let stats_rect = r(
            rect.right().saturating_sub(columns.stats_width),
            rect.y,
            columns.stats_width,
            1,
        );
        text(
            frame,
            stats_rect,
            columns.stats.format(stats),
            THEME.muted,
            bg,
        );
    }
    x
}

fn agent_symbol(state: model::AgentDisplayState, tick: usize) -> &'static str {
    use model::{AgentDisplayState as State, WaitReason};
    if state.running() {
        return spinner(tick);
    }
    match state {
        State::Job(state) => model::state_glyph(state),
        State::Waiting(WaitReason::Permission) => model::state_glyph(JobState::AwaitingApproval),
        State::Waiting(WaitReason::Input | WaitReason::ParentInput) => {
            model::state_glyph(JobState::WaitingInput)
        }
        State::Waiting(WaitReason::Child | WaitReason::Event) => "◷",
        _ => "·",
    }
}
pub(super) fn draw_menu_item(
    frame: &mut Frame,
    row: Rect,
    item: &super::super::app::ItemRef<'_>,
    kind: &MenuKind,
    selected: bool,
    bg: Color,
) {
    let label = model::clean(item.label);
    let detail = model::clean(item.detail);
    let fg = if selected && !matches!(kind, MenuKind::Output(_, _)) {
        THEME.primary
    } else {
        THEME.fg
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
                THEME.muted,
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
                Style::default().fg(THEME.muted),
            ),
        ]);
        text(frame, row, line, fg, bg);
    }
}

pub(super) fn draw_menu(frame: &mut Frame, app: &mut App) {
    let Some(Overlay::Menu(menu)) = &app.overlay else {
        return;
    };
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
    fill(frame, rect, THEME.input);
    text(
        frame,
        r(rect.x + 1, rect.y, width.saturating_sub(2), 1),
        menu.title.clone(),
        THEME.primary,
        THEME.input,
    );
    text(
        frame,
        r(rect.x + 1, rect.y + 1, width.saturating_sub(2), 1),
        format!("> {}▏", model::clean(menu.input.text())),
        THEME.fg,
        THEME.input,
    );
    let items = menu.filtered();
    let agent_menu = matches!(menu.kind, MenuKind::Agents(_));
    let agents = &app.projection.agents;
    let agent_stats: Vec<_> = if agent_menu {
        let stats = agents
            .iter()
            .map(|agent| app.projection.agent_stats(&app.snapshot, &agent.id));
        stats.collect()
    } else {
        Vec::new()
    };
    let row_width = width.saturating_sub(2);
    let columns = AgentColumns::new(agents, row_width, AgentStatsColumns::menu(&agent_stats));
    let stats_width = columns.stats_width;
    let header_height = u16::from(agent_menu && stats_width > 0);
    if header_height > 0 {
        text(
            frame,
            r(
                rect.right() - 1 - stats_width,
                rect.y + 2,
                stats_width,
                header_height.min(rect.height.saturating_sub(2)),
            ),
            columns.stats.format(&AGENT_STATS_HEADERS.map(String::from)),
            THEME.muted,
            THEME.input,
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
        let bg = if selected {
            THEME.selected
        } else {
            THEME.input
        };
        let row = r(rect.x + 1, y, width.saturating_sub(2), 1);
        if let MenuKind::Agents(choices) = &menu.kind {
            let id = &choices[item.index].value;
            if let Some(agent) = agents.get(item.index).filter(|a| &a.id == id) {
                let agent_row = AgentRow {
                    agent,
                    state: app.agent_status(agent),
                    stats: &agent_stats[item.index],
                    marker: "",
                };
                app.animating |= agent_row.state.running();
                draw_agent_row(frame, row, agent_row, &columns, app.tick_count, bg);
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
                let symbol = agent_symbol(peer.state, app.tick_count);
                let color = agent_status_color(peer.state);
                text(frame, r(row.x, y, 1, 1), symbol, color, bg);
            }
            let row = r(row.x + 2, y, row.width.saturating_sub(2), 1);
            draw_menu_item(frame, row, item, &menu.kind, selected, bg);
        } else {
            draw_menu_item(frame, row, item, &menu.kind, selected, bg);
        }
        if selected {
            focus_cursor(frame, rect.x, y, THEME.input);
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
            THEME.muted,
            THEME.input,
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
        THEME.muted,
        THEME.input,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    #[tokio::test]
    async fn only_pickable_rows_are_highlighted() {
        let (_root, mut app) = crate::tui::app::tests::fixture().await;
        for (command, highlighted) in [
            (crate::tui::keys::Command::Help, false),
            (crate::tui::keys::Command::Commands, true),
        ] {
            app.command(command);
            let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
            app.content_rect = r(0, 0, 80, 20);
            terminal.draw(|frame| draw_menu(frame, &mut app)).unwrap();
            let cells = &terminal.backend().buffer().content;
            let found = cells.iter().any(|cell| cell.bg == THEME.selected);
            assert_eq!(found, highlighted, "{command}");
            app.overlay = None;
        }
    }

    #[tokio::test]
    async fn agents_palette_hides_columns_without_stacking_rows() {
        use crate::tui::app::tests::{fixture, push_child};
        let (_root, mut app) = fixture().await;
        let session = app.session().unwrap().clone();
        app.command(crate::tui::keys::Command::Agents);
        let mut children = Vec::new();
        for index in 0..2 {
            let child = push_child(&mut app, index).await;
            let event = skyhook::session::SessionEvent::AgentCompleted;
            let record = session.store().append(child.clone(), event).await;
            let record = record.unwrap();
            app.snapshot.records.insert(record.sequence, record);
            children.push(format!("agent {}", crate::tui::format::agent_label(&child)));
        }
        app.refresh();
        // An open palette's rows follow agents and their live status, which is searchable.
        app.menu_mut().unwrap().input.set("completed".into());
        assert_eq!(app.menu().unwrap().filtered().len(), 2);
        app.menu_mut().unwrap().input.set(String::new());
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
            terminal.draw(|frame| draw_menu(frame, &mut app)).unwrap();
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
            assert_eq!(rows.len(), 3);
            for (index, row) in rows.iter().enumerate() {
                assert_eq!(
                    (row.height, row.y),
                    (1, 3 + u16::from(stats) + index as u16)
                );
            }
            for (row, child) in rows[1..].iter().zip(&children) {
                let text = line(row.y);
                assert!(text.contains(child.as_str()), "{text}");
                assert_eq!(text.contains("Completed"), status);
            }
        }
    }

    #[test]
    fn menu_hints_are_muted_right_aligned_and_yield_to_labels() {
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
                search: "",
            };
            for width in [0, 2, 8, 12, 21, 80] {
                for selected in [false, true] {
                    let mut terminal = Terminal::new(TestBackend::new(90, 3)).unwrap();
                    let row = r(3, 1, width, 1);
                    let bg = if selected {
                        THEME.selected
                    } else {
                        THEME.input
                    };
                    terminal
                        .draw(|frame| {
                            fill(frame, frame.area(), THEME.base);
                            fill(frame, row, bg);
                            draw_menu_item(frame, row, &item, &kind, selected, bg);
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
                        assert!((start..row.right()).all(|x| cell(x).fg == THEME.muted));
                    } else if !hint.is_empty() {
                        assert!(!actual.contains(hint), "{actual:?}");
                    }
                    if label.width() <= width as usize {
                        assert!(actual.starts_with(label), "{actual:?}");
                    }
                    if width > 0 && !label.contains('界') {
                        let fg = if selected { THEME.primary } else { THEME.fg };
                        assert_eq!(cell(row.x).fg, fg);
                    }
                    let filled = painted.iter().all(|&x| cell(x).bg == bg);
                    assert!(
                        filled,
                        "row background: {label:?}, {hint:?}, width={width}, selected={selected}"
                    );
                    assert_eq!(cell(row.right()).bg, THEME.base);
                }
            }
        }
    }
}
