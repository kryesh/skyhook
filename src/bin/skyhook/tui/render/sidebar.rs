//! Column beside the transcript: the session's title, which opens a rename when
//! clicked, the viewed agent's capabilities and todos, and the MCP servers. A
//! section with nothing to list is left out.

use super::*;
use skyhook::{agent::TodoStatus, mcp::McpServerStatus, tool::policy::Capability};

const SIDEBAR_MIN_TERMINAL: u16 = 100;

#[derive(Clone)]
enum SidebarLine {
    Title(String),
    /// Text, its colour, and the colour of a leading status marker.
    Item(String, Color, Option<Color>),
    Blank,
}

/// The sidebar narrows only the transcript. A cramped terminal or having
/// nothing to list hides it without changing the setting.
pub(super) fn sidebar_width(app: &App, width: u16) -> u16 {
    let servers = !app.launch.model.config().config().mcp.is_empty();
    let todos = app.projection.todos.get(&app.selected);
    let listed = app.title.is_some()
        || servers
        || todos.is_some_and(|todos| !todos.is_empty())
        || !capabilities(app).is_empty();
    if app.sidebar && listed && width >= SIDEBAR_MIN_TERMINAL {
        // A quarter of a wide terminal, within readable bounds.
        (width / 4).clamp(32, 48)
    } else {
        0
    }
}

pub(super) fn draw_sidebar(frame: &mut Frame, app: &mut App, rect: Rect) {
    let mut sections: Vec<Vec<SidebarLine>> = Vec::new();

    let mut section = vec![SidebarLine::Title("MCP servers".into())];
    let configured = &app.launch.model.config().config().mcp;
    let statuses = app.session().map(|session| session.mcp_servers());
    for name in configured.keys() {
        let (mark, detail, color) = match statuses.as_ref().and_then(|statuses| statuses.get(name))
        {
            Some(McpServerStatus::Connected { tools }) => {
                ("●", format!("{tools} tools"), THEME.primary)
            }
            Some(McpServerStatus::Failed(error)) => {
                ("✗", format!("failed: {error}"), THEME.warning)
            }
            Some(McpServerStatus::Skipped) => ("○", "skipped".into(), THEME.muted),
            None => ("○", "not started".into(), THEME.muted),
        };
        section.push(SidebarLine::Item(
            format!("{mark} {name}"),
            THEME.fg,
            Some(color),
        ));
        section.push(SidebarLine::Item(format!("  {detail}"), THEME.muted, None));
    }
    sections.push(section);

    let todos = app.projection.todos.get(&app.selected);
    let todos = todos.map(Vec::as_slice).unwrap_or_default();
    let done = todos
        .iter()
        .filter(|item| item.status == TodoStatus::Completed);
    let title = format!("Todos {}/{}", done.count(), todos.len());
    let mut section = vec![SidebarLine::Title(title)];
    for item in todos {
        // The agent tree's status vocabulary.
        let (mark, mark_color, color) = match item.status {
            TodoStatus::Pending => ("·", THEME.muted, THEME.muted),
            TodoStatus::InProgress => ("●", THEME.primary, THEME.fg),
            TodoStatus::Completed => ("✓", THEME.success, THEME.muted),
        };
        let text = format!("{mark} {}", item.text);
        section.push(SidebarLine::Item(text, color, Some(mark_color)));
    }
    sections.push(section);

    // A title alone lists nothing.
    sections.retain(|section| section.len() > 1);
    for section in &mut sections {
        section.insert(1, SidebarLine::Blank);
    }
    let lines = sections.join(&SidebarLine::Blank);

    let granted = capabilities(app).into_iter().map(capability_label);
    let mut pinned = vec![SidebarLine::Title("Capabilities".into())];
    pinned.extend(
        granted.map(|label| SidebarLine::Item(format!("· {label}"), THEME.fg, Some(THEME.muted))),
    );
    if pinned.len() == 1 {
        pinned.clear();
    } else {
        pinned.insert(1, SidebarLine::Blank);
    }

    fill(frame, rect, THEME.panel);
    // One row of padding above and below.
    let rect = r(
        rect.x,
        rect.y + 1,
        rect.width,
        rect.height.saturating_sub(2),
    );
    // The title stands alone on the first row, above a blank one.
    let rect = match app.title.as_ref().filter(|_| rect.height > 0) {
        Some(title) => {
            let row = r(rect.x + 1, rect.y, rect.width.saturating_sub(3), 1);
            let title = model::clean(&title.text).replace('\n', " ");
            let title = Line::styled(title, Style::default().add_modifier(Modifier::BOLD));
            let title = clipped(title, row.width as usize);
            text(frame, row, title, THEME.primary, THEME.panel);
            app.hits.push((row, Hit::SessionTitle));
            r(
                rect.x,
                rect.y + 2,
                rect.width,
                rect.height.saturating_sub(2),
            )
        }
        None => rect,
    };
    // Capabilities keep the bottom rows; the rest scrolls off above a blank row.
    let pinned = wrapped(&pinned, rect.width);
    let pinned_height = (pinned.len() as u16).min(rect.height);
    let bottom = r(
        rect.x,
        rect.bottom() - pinned_height,
        rect.width,
        pinned_height,
    );
    draw_rows(frame, bottom, &pinned);
    let above = rect
        .height
        .saturating_sub(pinned_height + u16::from(pinned_height > 0));
    draw_rows(
        frame,
        r(rect.x, rect.y, rect.width, above),
        &wrapped(&lines, rect.width),
    );
}

/// The viewed agent's capabilities. The root agent's next message goes out in the
/// chosen mode, so a pending choice shows what that mode grants instead.
fn capabilities(app: &App) -> Vec<Capability> {
    let agent = app.projection.agents.iter().find(|a| a.id == app.selected);
    let pending = app.selected.path().is_empty()
        && agent.is_none_or(|agent| agent.mode.as_ref().is_some_and(|mode| *mode != app.mode));
    if pending {
        // A session limits what the mode grants; a draft's terminal adds interaction.
        let granted = match app.session() {
            Some(session) => session.mode_capabilities(&app.mode),
            None => app.modes().get(&app.mode).map(|mode| {
                let listed = mode.capabilities.iter().copied();
                listed.chain([Capability::Interactive]).collect()
            }),
        };
        return granted
            .map(|granted| granted.iter().collect())
            .unwrap_or_default();
    }
    agent
        .map(|agent| agent.capabilities.clone())
        .unwrap_or_default()
}

fn capability_label(capability: Capability) -> &'static str {
    match capability {
        Capability::Read => "Read files",
        Capability::Write => "Write files",
        Capability::Exec => "Run commands",
        Capability::Network => "Fetch from the network",
        Capability::Targets => "Use remote targets",
        Capability::SshAgent => "External SSH agent",
        Capability::Agents => "Start child agents",
        Capability::Interactive => "Ask you questions",
        Capability::Mcp => "Use MCP tools",
    }
}

/// Continuation rows hang under the text, past the two-cell marker. Returns
/// each visual row with its indent.
fn wrapped(lines: &[SidebarLine], width: u16) -> Vec<(u16, SidebarLine)> {
    let width = width.saturating_sub(5) as usize;
    let rows = lines.iter().flat_map(|line| {
        let text = match line {
            SidebarLine::Title(text) | SidebarLine::Item(text, ..) => text.as_str(),
            SidebarLine::Blank => "",
        };
        let wrapped = wrap_words(Line::from(model::clean(text).replace('\n', " ")), width);
        let rows = wrapped.into_iter().enumerate();
        rows.map(move |(index, row)| {
            let row = row.to_string();
            let row = match line {
                SidebarLine::Title(_) => SidebarLine::Title(row),
                SidebarLine::Item(_, color, mark) => {
                    SidebarLine::Item(row, *color, mark.filter(|_| index == 0))
                }
                SidebarLine::Blank => SidebarLine::Blank,
            };
            (u16::from(index > 0) * 2, row)
        })
    });
    rows.collect()
}

fn draw_rows(frame: &mut Frame, rect: Rect, rows: &[(u16, SidebarLine)]) {
    let width = rect.width.saturating_sub(3);
    let capacity = rect.height as usize;
    let hidden = rows.len().saturating_sub(capacity);
    for (y, (indent, row)) in rows.iter().take(capacity).enumerate() {
        let y = rect.y + y as u16;
        if hidden > 0 && y + 1 == rect.bottom() {
            let more = format!("+{} more", hidden + 1);
            text(
                frame,
                r(rect.x + 2, y, width, 1),
                more,
                THEME.muted,
                THEME.panel,
            );
            continue;
        }
        match row {
            // A title is bold and stands one column left of what it lists.
            SidebarLine::Title(title) => {
                let title =
                    Span::styled(title.clone(), Style::default().add_modifier(Modifier::BOLD));
                text(
                    frame,
                    r(rect.x + 1, y, width, 1),
                    title,
                    THEME.fg,
                    THEME.panel,
                );
            }
            SidebarLine::Item(item, color, mark) => {
                let rect = r(rect.x + 2 + indent, y, width.saturating_sub(*indent), 1);
                text(frame, rect, item.clone(), *color, THEME.panel);
                if let Some(mark) = mark {
                    frame.buffer_mut()[(rect.x, y)].set_fg(*mark);
                }
            }
            SidebarLine::Blank => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_labels_fit_one_row_of_the_narrowest_sidebar() {
        // The sidebar is never narrower than 32 columns.
        for capability in Capability::ALL {
            let text = format!("· {}", capability_label(capability));
            let line = SidebarLine::Item(text, Color::Reset, None);
            assert_eq!(wrapped(&[line], 32).len(), 1, "{capability}");
        }
    }
}
