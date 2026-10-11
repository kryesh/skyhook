//! Column beside the transcript: the session's title, which opens a rename when
//! clicked, the viewed agent's capabilities and todos, and the MCP servers it
//! can use. A section with nothing to list is left out.

use super::*;
use skyhook::{
    agent::TodoStatus,
    mcp::McpServerStatus,
    tool::policy::{Capability, CapabilitySet, ModeName},
};

const SIDEBAR_MIN_TERMINAL: u16 = 100;

#[derive(Clone)]
enum SidebarLine {
    Title(String),
    /// Text, its colour, and the colour of a leading status marker.
    Item(String, Color, Option<Color>),
    Blank,
}

/// What the sidebar shows from the session, read once per draw.
pub(super) struct Sidebar {
    width: u16,
    capabilities: Vec<Capability>,
    servers: Vec<McpRow>,
}

impl Sidebar {
    /// The sidebar narrows only the transcript. None when it is turned off, or
    /// a cramped terminal or having nothing to list hides it without changing
    /// the setting.
    pub(super) fn of(app: &App, width: u16) -> Option<Self> {
        if !app.sidebar || width < SIDEBAR_MIN_TERMINAL {
            return None;
        }
        let grants = Grants::of(app);
        let shown = grants.shown().map(|granted| granted.iter().collect());
        let capabilities: Vec<_> = shown.unwrap_or_default();
        let servers = mcp_servers(app, &grants);
        let todos = app.projection.todos.get(&app.selected);
        let listed = app.title.is_some()
            || !servers.is_empty()
            || todos.is_some_and(|todos| !todos.is_empty())
            || !capabilities.is_empty();
        listed.then(|| Self {
            // A quarter of a wide terminal, within readable bounds.
            width: (width / 4).clamp(32, 48),
            capabilities,
            servers,
        })
    }

    pub(super) fn width(&self) -> u16 {
        self.width
    }
}

pub(super) fn draw_sidebar(frame: &mut Frame, app: &mut App, sidebar: Sidebar, rect: Rect) {
    let mut sections: Vec<Vec<SidebarLine>> = Vec::new();

    let mut section = vec![SidebarLine::Title("MCP servers".into())];
    for McpRow {
        name,
        status,
        availability,
    } in sidebar.servers
    {
        let (mark, detail, color) = match status {
            Some(McpServerStatus::Connected { tools }) => {
                ("●", format!("{tools} tools"), THEME.primary)
            }
            Some(McpServerStatus::Failed(error)) => {
                ("✗", format!("failed: {error}"), THEME.warning)
            }
            Some(McpServerStatus::Skipped) => ("○", "skipped".into(), THEME.muted),
            None => ("○", "not started".into(), THEME.muted),
        };
        let (mark, color, text) = match availability {
            Availability::Available => (mark, color, THEME.fg),
            Availability::Added => ("+", THEME.success, THEME.fg),
            Availability::Removed => (mark, THEME.muted, THEME.muted),
        };
        section.push(SidebarLine::Item(
            format!("{mark} {name}"),
            text,
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

    let granted = sidebar.capabilities.into_iter().map(capability_label);
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

/// What the viewed agent holds now, and what the root agent's next message
/// would hold when it goes out in another mode. A draft has no agent yet, so
/// only its chosen mode counts.
struct Grants {
    running: Option<CapabilitySet>,
    pending: Option<CapabilitySet>,
}

impl Grants {
    fn of(app: &App) -> Self {
        let agent = app.projection.agents.iter().find(|a| a.id == app.selected);
        // A session also knows when the configuration redefined the root's mode.
        let pending = app.selected.path().is_empty()
            && match app.session() {
                Some(session) => session.changes_mode(&app.mode),
                None => agent.is_none_or(|agent| agent.mode.as_ref() != Some(&app.mode)),
            };
        Self {
            // The journal can overstate a reopened session's agents; ask the session.
            running: (app.session()).and_then(|session| session.agent_capabilities(&app.selected)),
            pending: pending
                .then(|| mode_capabilities(app, &app.mode).unwrap_or(CapabilitySet::empty())),
        }
    }

    /// A pending choice shows what that mode grants instead of what is running.
    fn shown(&self) -> Option<&CapabilitySet> {
        self.pending.as_ref().or(self.running.as_ref())
    }
}

/// What sending the root agent into `mode` would grant it. A session limits
/// what the mode grants; a draft's terminal adds interaction, and its root, like
/// a started one, cannot start children without depth to give them.
fn mode_capabilities(app: &App, mode: &ModeName) -> Option<CapabilitySet> {
    match app.session() {
        Some(session) => session.mode_capabilities(mode),
        None => app.modes().get(mode).map(|mode| {
            let listed = mode.capabilities.iter().copied();
            let granted: CapabilitySet = listed.chain([Capability::Interactive]).collect();
            let depth = app.launch.model.config().config().max_child_depth;
            granted.for_agent(depth)
        }),
    }
}

/// How a pending mode choice would change a server's availability to the
/// running agent. Without both, a listed server is simply available.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Availability {
    Available,
    Added,
    Removed,
}

struct McpRow {
    name: String,
    /// Known once a session has started its servers.
    status: Option<McpServerStatus>,
    availability: Availability,
}

/// Configured servers the shown `grants` permit, plus, while a mode choice is
/// pending for a running agent, those it would lose.
fn mcp_servers(app: &App, grants: &Grants) -> Vec<McpRow> {
    let mut statuses = app
        .session()
        .map(|session| session.mcp_servers())
        .unwrap_or_default();
    let configured = &app.launch.model.config().config().mcp;
    let rows = configured.iter().filter_map(|(name, config)| {
        let permits = |granted: &Option<CapabilitySet>| {
            granted.as_ref().map(|granted| config.permitted_by(granted))
        };
        let availability = match (permits(&grants.running), permits(&grants.pending)) {
            (Some(false), Some(true)) => Availability::Added,
            (Some(true), Some(false)) => Availability::Removed,
            (_, Some(true)) | (Some(true), None) => Availability::Available,
            _ => return None,
        };
        Some(McpRow {
            name: name.clone(),
            status: statuses.remove(name),
            availability,
        })
    });
    rows.collect()
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

    #[tokio::test]
    async fn mcp_servers_follow_the_chosen_mode_and_mark_pending_changes() {
        use crate::{
            launch::Permissions,
            tui::app::tests::{attach, draft_fixture},
        };
        use Availability::*;
        let (_root, mut app) = draft_fixture().await;
        let missing = app.launch.workspace.join("missing-mcp-server");
        let missing = serde_json::to_string(&missing.to_string_lossy()).unwrap();
        let configure = |app: &mut App, general: &str, tools: &str| {
            let mut config = app.launch.model.config().config().clone();
            let yaml = format!(
                "modes:\n  general:\n    capabilities: [{general}]\n  tools:\n    capabilities: [{tools}]\nmcp:\n  reader:\n    transport: stdio\n    start_command: [{missing}]\n    capabilities: [read]\n  writer:\n    transport: stdio\n    start_command: [{missing}]\n    capabilities: [write]\n"
            );
            let parsed = skyhook::config::Config::from_yaml(&yaml).unwrap();
            (config.modes, config.mcp) = (parsed.modes, parsed.mcp);
            app.launch.model = (config.into_runtime().unwrap())
                .select_model(&"test/first".parse().unwrap())
                .unwrap();
        };
        let listed = |app: &mut App, mode: &str| {
            app.mode = mode.parse().unwrap();
            let rows = mcp_servers(app, &Grants::of(app)).into_iter();
            let rows = rows.map(|row| (row.name, row.availability, row.status.is_some()));
            rows.collect::<Vec<_>>()
        };
        let reader = |availability, started| vec![("reader".to_owned(), availability, started)];
        configure(&mut app, "read", "read, mcp");
        // A draft lists what its chosen mode permits; no mode grants `writer`.
        assert!(listed(&mut app, "general").is_empty());
        assert_eq!(listed(&mut app, "tools"), reader(Available, false));
        // A running agent keeps its servers listed; a pending choice marks changes.
        let cases = [
            ("general", vec![], "tools", reader(Added, true)),
            (
                "tools",
                reader(Available, true),
                "general",
                reader(Removed, true),
            ),
        ];
        for (running, kept, pending, changed) in cases {
            app.launch.permissions = Permissions::Mode(running.parse().unwrap());
            let session = app.launch.create(None).await.unwrap();
            attach(&mut app, session);
            assert_eq!(listed(&mut app, running), kept, "{running}");
            assert_eq!(listed(&mut app, pending), changed, "{running}");
            app.session().unwrap().shutdown().await.unwrap();
        }

        // Reopened sessions: one started with exact capabilities records no mode;
        // in the others the configuration redefined `tools` without mcp, while
        // `general` may grant it instead.
        let exact = [Capability::Read, Capability::Mcp].into_iter().collect();
        let tools = || Permissions::Mode("tools".parse().unwrap());
        let cases = [
            (Permissions::Exact(exact), "read", "read, mcp"),
            (tools(), "read", "read"),
            (tools(), "read, mcp", "read"),
        ];
        let mut reopened = Vec::new();
        for (created, general, tools) in cases {
            configure(&mut app, "read", "read, mcp");
            app.launch.permissions = created;
            let session = app.launch.create(None).await.unwrap();
            let id = session.id();
            session.shutdown().await.unwrap();
            session.close().await.unwrap();
            drop(session);
            configure(&mut app, general, tools);
            app.launch.permissions = Permissions::Mode("general".parse().unwrap());
            let session = app.launch.create(Some(id)).await.unwrap();
            attach(&mut app, session);
            reopened.push((listed(&mut app, "tools"), listed(&mut app, "general")));
            app.session().unwrap().shutdown().await.unwrap();
        }
        // Without a recorded mode, any chosen mode is a pending change.
        assert_eq!(
            reopened[0],
            (reader(Available, true), reader(Removed, true))
        );
        // The journal still says `tools` grants mcp, but the session no longer does.
        assert_eq!(reopened[1], (vec![], vec![]));
        // Still able to use mcp, the root would lose it on its next message in the
        // redefined `tools`, whose name it already runs under.
        assert_eq!(
            reopened[2],
            (reader(Removed, true), reader(Available, true))
        );
    }

    #[tokio::test]
    async fn a_draft_lists_only_servers_its_roots_depth_allows() {
        use crate::tui::app::tests::draft_fixture;
        let (_root, mut app) = draft_fixture().await;
        let yaml = "modes:\n  general:\n    capabilities: [read, mcp, agents]\nmcp:\n  reader:\n    transport: stdio\n    start_command: [server]\n    capabilities: [read]\n  delegator:\n    transport: stdio\n    start_command: [server]\n    capabilities: [agents]\n";
        let parsed = skyhook::config::Config::from_yaml(yaml).unwrap();
        // Without depth to give children, the root holds no `agents`.
        for (depth, expected) in [(0, &["reader"][..]), (1, &["delegator", "reader"])] {
            let mut config = app.launch.model.config().config().clone();
            (config.modes, config.mcp) = (parsed.modes.clone(), parsed.mcp.clone());
            config.max_child_depth = depth;
            app.launch.model = (config.into_runtime().unwrap())
                .select_model(&"test/first".parse().unwrap())
                .unwrap();
            let rows = mcp_servers(&app, &Grants::of(&app)).into_iter();
            let listed: Vec<_> = rows.map(|row| row.name).collect();
            assert_eq!(listed, expected, "{depth}");
        }
    }
}
