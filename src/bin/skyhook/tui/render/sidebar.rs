//! Read-only column beside the transcript: MCP servers and the viewed agent's todos.

use super::*;
use skyhook::{agent::TodoStatus, mcp::McpServerStatus};

const SIDEBAR_MIN_TERMINAL: u16 = 100;

/// The sidebar narrows only the transcript. A cramped terminal or having
/// nothing to list hides it without changing the setting.
pub(super) fn sidebar_width(app: &App, width: u16) -> u16 {
    let servers = !app.launch.model.config().config().mcp.is_empty();
    let todos = app.projection.todos.get(&app.selected);
    let listed = servers || todos.is_some_and(|todos| !todos.is_empty());
    if app.sidebar && listed && width >= SIDEBAR_MIN_TERMINAL {
        // A quarter of a wide terminal, within readable bounds.
        (width / 4).clamp(32, 48)
    } else {
        0
    }
}

pub(super) fn draw_sidebar(frame: &mut Frame, app: &App, rect: Rect, p: Palette) {
    // Text, its colour, and the colour of a leading status marker.
    let mut lines = vec![("MCP servers".to_owned(), p.fg, None)];
    let configured = &app.launch.model.config().config().mcp;
    let statuses = app.session().map(|session| session.mcp_servers());
    for name in configured.keys() {
        let (mark, detail, color) = match statuses.and_then(|statuses| statuses.get(name)) {
            Some(McpServerStatus::Connected { tools }) => ("●", format!("{tools} tools"), p.accent),
            Some(McpServerStatus::Failed(error)) => ("✗", format!("failed: {error}"), p.warning),
            Some(McpServerStatus::Skipped) => ("○", "skipped".into(), p.muted),
            None => ("○", "not started".into(), p.muted),
        };
        lines.push((format!("{mark} {name}"), p.fg, Some(color)));
        lines.push((format!("  {detail}"), p.muted, None));
    }
    if configured.is_empty() {
        lines.push(("No servers configured".into(), p.muted, None));
    }
    let todos = app.projection.todos.get(&app.selected);
    let todos = todos.map(Vec::as_slice).unwrap_or_default();
    let done = todos
        .iter()
        .filter(|item| item.status == TodoStatus::Completed);
    lines.push((String::new(), p.muted, None));
    lines.push((
        format!("Todos {}/{}", done.count(), todos.len()),
        p.fg,
        None,
    ));
    for item in todos {
        // The agent tree's status vocabulary.
        let (mark, mark_color, color) = match item.status {
            TodoStatus::Pending => ("·", p.muted, p.fg),
            TodoStatus::InProgress => ("●", p.content.primary, p.fg),
            TodoStatus::Completed => ("✓", p.content.success, p.muted),
        };
        lines.push((format!("{mark} {}", item.text), color, Some(mark_color)));
    }
    if todos.is_empty() {
        lines.push(("No todos".into(), p.muted, None));
    }

    fill(frame, rect, p.panel);
    let width = rect.width.saturating_sub(3);
    // Continuation rows hang under the text, past the two-cell marker.
    let rows = lines.iter().flat_map(|(line, color, mark)| {
        let wrapped = wrap_words(
            Line::from(model::clean(line).replace('\n', " ")),
            width.saturating_sub(2) as usize,
        );
        let rows = wrapped.into_iter().enumerate();
        rows.map(move |(index, row)| {
            (
                u16::from(index > 0) * 2,
                row,
                *color,
                mark.filter(|_| index == 0),
            )
        })
    });
    let rows: Vec<_> = rows.collect();
    let capacity = rect.height as usize;
    let hidden = rows.len().saturating_sub(capacity);
    for (y, (indent, row, color, mark)) in rows.iter().take(capacity).enumerate() {
        let y = rect.y + y as u16;
        if hidden > 0 && y + 1 == rect.bottom() {
            let more = format!("+{} more", hidden + 1);
            text(frame, r(rect.x + 2, y, width, 1), more, p.muted, p.panel);
        } else {
            let rect = r(rect.x + 2 + indent, y, width.saturating_sub(*indent), 1);
            text(frame, rect, row.to_string(), *color, p.panel);
            if let Some(mark) = mark {
                frame.buffer_mut()[(rect.x, y)].set_fg(*mark);
            }
        }
    }
}
