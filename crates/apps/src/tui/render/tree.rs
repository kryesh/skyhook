//! Agent rows, in the tree beside the conversation and in the agents menu.
use super::columns::{AgentColumns, AgentStatsColumns, TreePlane, right_aligned};
use super::painting::{fill, focus_cursor, r, scroll_thumb, spinner, text};
use crate::tui::app::{App, Hit, Pane};
use crate::tui::theme::theme;
use crate::tui::wrapping::{clipped, truncated};
use crate::tui::{format, model};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use skyhook::job::JobState;
use unicode_width::UnicodeWidthStr;

/// Visible agent hierarchy with shared statistics and navigation hit targets.
pub(super) fn draw_tree(
    frame: &mut Frame,
    app: &mut App,
    tree_agents: &[usize],
    plane: TreePlane,
    navigation_active: bool,
) {
    let area = app.render.tree_rect;
    if area.is_empty() {
        return;
    }
    fill(frame, area, theme().ui.panel);
    // A band pads a row above and below its agents; a column heads them with counts.
    let header = match plane {
        TreePlane::Band => Vec::new(),
        TreePlane::Column => tree_header(
            app,
            tree_agents,
            area.width.saturating_sub(4).into(),
            area.height.into(),
        ),
    };
    for (offset, line) in header.iter().enumerate() {
        let rect = r(
            area.x + 2,
            area.y + offset as u16,
            area.width.saturating_sub(4),
            1,
        );
        text(frame, rect, line.clone(), theme().ui.fg, theme().ui.panel);
    }
    let (tree_y, tree_rows) = match plane {
        TreePlane::Band => (area.y, area.height.saturating_sub(2)),
        TreePlane::Column => {
            let taken = header.len() as u16;
            (area.y + taken - 1, area.height.saturating_sub(taken))
        }
    };
    let width = area.width;
    app.tree_cursor = app.tree_cursor.min(tree_agents.len().saturating_sub(1));
    let cursor = (app.focus == Pane::Tree).then_some(app.tree_cursor);
    let tree_top = app
        .tree_view
        .fit(tree_rows.into(), tree_agents.len(), cursor);
    let agents = tree_agents
        .iter()
        .map(|&index| &app.projection.agents[index]);
    let agent_stats: Vec<_> = agents
        .clone()
        .map(|agent| agent_stats(app, agent))
        .collect();
    let stats = AgentStatsColumns::new(&agent_stats);
    let row_width = width.saturating_sub(4);
    let columns = AgentColumns::new(agents, row_width, stats, plane);
    let track = r(area.x + width - 1, tree_y + 1, 1, tree_rows);
    let shown = tree_top as f64..(tree_top + tree_rows as usize) as f64;
    scroll_thumb(
        frame,
        track,
        shown,
        tree_agents.len() as f64,
        theme().ui.panel,
    );
    let rows = tree_agents.iter().enumerate();
    for (index, &agent_index) in rows.skip(tree_top).take(tree_rows as usize) {
        let agent = &app.projection.agents[agent_index];
        let y = tree_y + 1 + (index - tree_top) as u16;
        let selected = app.tree_viewed() == Some(&agent.id);
        let focused = navigation_active && app.focus == Pane::Tree && app.tree_cursor == index;
        let rect = r(area.x + 2, y, row_width, 1);
        let hover = navigation_active && app.hover.is_some_and(|point| rect.contains(point.into()));
        let bg = if selected || focused || hover {
            theme().ui.selected
        } else {
            theme().ui.panel
        };
        let row = AgentRow {
            agent,
            state: app.agent_status(agent),
            timing: app.projection.timing(agent, &app.snapshot),
            stats: &agent_stats[index],
            marker: if selected { "> " } else { "  " },
        };
        app.render.animating |= row.animates();
        let x = draw_agent_row(frame, rect, row, &columns, app.tick_count, bg);
        if focused {
            focus_cursor(frame, x, y, bg);
        }
        app.render.hits.push((rect, Hit::Agent(agent.id.clone())));
    }
}
/// A column's heading, `width` cells wide: how many agents it lists and run, then a line
/// per mode with how many unfinished agents it has, in tree order. Agents keep the column:
/// modes take at most a third of its `height`, the rest folded into one "+N modes" line.
fn tree_header(
    app: &App,
    tree_agents: &[usize],
    width: usize,
    height: usize,
) -> Vec<Line<'static>> {
    let agents: Vec<_> = (tree_agents.iter())
        .map(|&index| &app.projection.agents[index])
        .collect();
    let running = (agents.iter())
        .filter(|agent| app.agent_status(agent).running())
        .count();
    let mut modes: Vec<(&skyhook::config::mode::ModeName, usize)> = Vec::new();
    for mode in (agents.iter())
        .filter(|agent| !agent.terminal())
        .filter_map(|agent| agent.outline.mode.as_ref())
    {
        match modes.iter_mut().find(|(seen, _)| *seen == mode) {
            Some((_, count)) => *count += 1,
            None => modes.push((mode, 1)),
        }
    }
    let title = Line::from(vec![
        Span::styled("Agents", Style::default().fg(theme().ui.strong)),
        Span::styled(
            format!(" {} · {running} running", agents.len()),
            Style::default().fg(theme().ui.muted),
        ),
    ]);
    // One mode a line: its name at the left edge and its count at the right, so names and
    // counts each line up however wide the column.
    let line = |name: String, count: String| {
        let room = width.saturating_sub(count.len() + 1);
        let name = truncated(Line::from(name), room).to_string();
        Line::from(Span::styled(
            format!("{name}{}", right_aligned(&count, width - name.width())),
            Style::default().fg(theme().ui.muted),
        ))
    };
    let budget = height / 3;
    let listed = if modes.len() > budget {
        budget.saturating_sub(1)
    } else {
        modes.len()
    };
    let mut header = vec![title];
    header.extend(
        (modes[..listed].iter()).map(|(mode, count)| line(mode.to_string(), count.to_string())),
    );
    let folded = &modes[listed..];
    if !folded.is_empty() && budget > 0 {
        let agents: usize = folded.iter().map(|(_, count)| count).sum();
        header.push(line(format!("+{} modes", folded.len()), agents.to_string()));
    }
    header
}
pub(super) fn agent_status_color(state: model::AgentDisplayState) -> Color {
    use model::AgentDisplayState as State;
    match state {
        State::Job(state) => model::state_role(state).color(),
        State::Waiting(_) => theme().ui.warning,
        State::Working | State::Reconnecting { .. } | State::Compacting | State::RunningTools => {
            theme().ui.emphasis
        }
        State::Ready => theme().ui.muted,
    }
}
/// An agent's statistics, ending with how long it has been at it.
pub(super) fn agent_stats(app: &App, agent: &model::AgentInfo) -> [String; 4] {
    let [output, input, context] = app.projection.agent_stats(&app.snapshot, &agent.id);
    let timing = app.projection.timing(agent, &app.snapshot);
    let time = timing.text(app.clock).unwrap_or_else(|| "—".into());
    [output, input, context, time]
}
/// One agent in a tree or agents menu row.
pub(super) struct AgentRow<'a> {
    pub(super) agent: &'a model::AgentInfo,
    pub(super) state: model::AgentDisplayState,
    /// How long the agent has been at it, counting while live.
    pub(super) timing: format::Timing,
    pub(super) stats: &'a [String; 4],
    /// Precedes the status symbol, e.g. the selected agent's `>`.
    pub(super) marker: &'a str,
}
impl AgentRow<'_> {
    /// Whether the row changes as time passes: its spinner or its counter.
    pub(super) fn animates(&self) -> bool {
        self.state.running() || self.timing.live()
    }
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
        ..
    } = row;
    fill(frame, rect, bg);
    let color = agent_status_color(state);
    let symbol = agent_symbol(state, tick);
    let indent = columns.plane.indent(agent, rect.width);
    let name_width = columns.identity_width.saturating_sub(indent);
    let available = name_width.saturating_sub((marker.width() + symbol.width() + 1) as u16);
    let target = format::clean(&model::target_suffix(&agent.outline.target));
    let target_width = (target.width() as u16).min(available / 2);
    // Selection only changes the neutral surface, not the independent
    // identity, target, and state roles.
    let mut identity = Line::from(vec![
        Span::styled(marker.to_owned(), Style::default().fg(theme().ui.emphasis)),
        Span::styled(symbol, Style::default().fg(color)),
        Span::raw(" "),
    ]);
    for (text, fg, width) in [
        (
            format::clean(&agent.name),
            theme().ui.fg,
            available - target_width,
        ),
        (target, theme().ui.accent, target_width),
    ] {
        let text = Line::from(Span::styled(text, Style::default().fg(fg)));
        identity.spans.extend(clipped(text, width as usize).spans);
    }
    let x = rect.x + indent;
    text(
        frame,
        r(x, rect.y, name_width, 1),
        identity,
        theme().ui.fg,
        bg,
    );
    if columns.status_width > 0 {
        let status = r(
            rect.x + columns.identity_width + 2,
            rect.y,
            columns.status_width,
            1,
        );
        let label = Line::from(Span::styled(state.label(), Style::default().fg(color)));
        let label = clipped(label, usize::from(columns.status_width));
        text(frame, status, label, color, bg);
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
            theme().ui.muted,
            bg,
        );
    }
    x
}
pub(super) fn agent_symbol(state: model::AgentDisplayState, tick: usize) -> &'static str {
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
