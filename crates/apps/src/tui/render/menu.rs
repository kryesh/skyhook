//! Menus over the conversation: the command palette and the lists it opens.

use super::columns::{
    AGENT_STATS_HEADERS, AgentColumns, AgentStatsColumns, MENU_GAP, MenuColumns, TreePlane,
};
use super::painting::{fill, focus_cursor, r, text};
use super::tree::{AgentRow, agent_stats, agent_status_color, agent_symbol, draw_agent_row};
use crate::tui::app::{App, Emphasis, Hit, MenuKind, MenuLayout, SavedStatus, SessionRef};
use crate::tui::editor::TextField;
use crate::tui::format;
use crate::tui::theme::theme;
use crate::tui::wrapping::{clipped, wrap_plain};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// A session row's marker, a status cell wide enough for the lock, and a gap.
const SESSION_PREFIX: u16 = 4;
const LOCKED: &str = "🔒";

pub(super) fn draw_menu_item(
    frame: &mut Frame,
    row: Rect,
    item: &super::super::app::ItemRef<'_>,
    columns: &MenuColumns,
    selected: bool,
    bg: Color,
) {
    let fg = if selected {
        theme().ui.emphasis
    } else {
        theme().ui.fg
    };
    // Paint the whole row, including the gap before the columns.
    fill(frame, row, bg);
    let columns_width = columns.width() as u16;
    let text_width = row.width.saturating_sub(columns_width);
    let detail = if item.detail.is_empty() {
        String::new()
    } else {
        format!("   {}", item.detail)
    };
    let style = match item.emphasis {
        Emphasis::Normal => Style::default(),
        Emphasis::Strong => Style::default().add_modifier(Modifier::BOLD),
    };
    let line = Line::from(vec![
        Span::styled(item.label.to_owned(), style.fg(fg)),
        Span::styled(detail, style.fg(theme().ui.muted)),
    ]);
    let line = clipped(line, usize::from(text_width));
    text(frame, r(row.x, row.y, text_width, 1), line, fg, bg);
    if columns_width > 0 {
        let rect = r(row.right() - columns_width, row.y, columns_width, 1);
        let columns = format!("{MENU_GAP}{}", columns.format(item.columns));
        let columns = Line::from(Span::styled(columns, style.fg(theme().ui.muted)));
        text(frame, rect, columns, theme().ui.muted, bg);
    }
}

pub(super) fn draw_menu(frame: &mut Frame, app: &mut App) {
    let Some(menu) = &app.menu else {
        return;
    };
    let area = app.render.content_rect;
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
    let full = area.height - gap * 2;
    let agent_menu = matches!(menu.kind, MenuKind::Agents(_));
    let agents = &app.projection.agents;
    let agent_stats: Vec<_> = if agent_menu {
        agents.iter().map(|agent| agent_stats(app, agent)).collect()
    } else {
        Vec::new()
    };
    let row_width = width.saturating_sub(2);
    let columns = AgentColumns::new(
        agents,
        row_width,
        AgentStatsColumns::menu(&agent_stats),
        TreePlane::Band,
    );
    let prefix = if matches!(menu.kind, MenuKind::Sessions(_)) {
        SESSION_PREFIX
    } else {
        0
    };
    let item_columns = MenuColumns::new(&menu.kind.items(), row_width.saturating_sub(prefix));
    let stats_width = columns.stats_width;
    // Headed columns: the status, then the statistics when they fit.
    let header_height = u16::from(agent_menu && columns.status_width > 0);
    let filter = match menu.layout() {
        MenuLayout::Filtered(input) => Some(input.text()),
        MenuLayout::Compact => None,
    };
    let filter_height = u16::from(filter.is_some());
    let title = wrap_plain(&menu.title, row_width.into());
    let title_lines = title.len() as u16;
    // The title's first line takes the space first, then the rows, the controls
    // and the rest of the title, so a short menu still says what it asks.
    let mut left = full;
    let mut take = |wanted: u16| {
        let taken = wanted.min(left);
        left -= taken;
        taken
    };
    let lead = take(title_lines.min(1));
    take(match menu.layout() {
        MenuLayout::Filtered(_) => 1,
        MenuLayout::Compact => menu.filtered().len() as u16,
    });
    let [filter_height, header_height, help_height] =
        [filter_height, header_height, 1].map(&mut take);
    let title_height = lead + take(title_lines - lead);
    let height = match menu.layout() {
        MenuLayout::Filtered(_) => full,
        MenuLayout::Compact => full - left,
    };
    let rect = r(area.x + margin, area.y + gap, width, height);
    fill(frame, rect, theme().ui.input);
    let [title_area, filter_area, header_area, rows_area, help_area] = Layout::vertical([
        Constraint::Length(title_height),
        Constraint::Length(filter_height),
        Constraint::Length(header_height),
        Constraint::Fill(1),
        Constraint::Length(help_height),
    ])
    .areas(rect.inner(Margin::new(1, 0)));
    for (line_rect, line) in title_area.rows().zip(title) {
        text(
            frame,
            line_rect,
            line,
            theme().ui.emphasis,
            theme().ui.input,
        );
    }
    if let Some(filter) = filter {
        let filter = format!("> {}▏", format::clean(filter));
        text(frame, filter_area, filter, theme().ui.fg, theme().ui.input);
    }
    let (y, header) = (header_area.y, header_area.height);
    let status_x = header_area.x + columns.identity_width + 2;
    let status = r(status_x, y, columns.status_width, header);
    text(frame, status, "Status", theme().ui.muted, theme().ui.input);
    if stats_width > 0 {
        let stats = r(header_area.right() - stats_width, y, stats_width, header);
        let headers = columns.stats.format(&AGENT_STATS_HEADERS.map(String::from));
        text(frame, stats, headers, theme().ui.muted, theme().ui.input);
    }
    let height = rows_area.height as usize;
    let Some(menu) = &mut app.menu else {
        return;
    };
    let top = menu.viewport(height);
    let Some(menu) = &app.menu else {
        return;
    };
    let items = menu.filtered();
    let page = menu.is_text_page();
    for (i, item) in items.iter().enumerate().skip(top).take(height) {
        let y = rows_area.y + (i - top) as u16;
        let selected = i == menu.selected() && !page;
        let bg = if selected {
            theme().ui.selected
        } else {
            theme().ui.input
        };
        let row = r(rows_area.x, y, rows_area.width, 1);
        if let MenuKind::Agents(choices) = &menu.kind {
            let id = &choices[item.index].value;
            if let Some(agent) = agents.get(item.index).filter(|a| &a.id == id) {
                let agent_row = AgentRow {
                    agent,
                    state: app.agent_status(agent),
                    timing: app.projection.timing(agent, &app.snapshot),
                    stats: &agent_stats[item.index],
                    marker: "",
                };
                app.render.animating |= agent_row.animates();
                draw_agent_row(frame, row, agent_row, &columns, app.tick_count, bg);
            }
        } else if let MenuKind::Sessions(sessions) = &menu.kind {
            // Open sessions carry their live status, saved ones how they would
            // resume: the current one is marked, and one held elsewhere is locked.
            fill(frame, row, bg);
            let glyph = r(row.x + 1, y, 2, 1);
            let state = match sessions[item.index].value {
                SessionRef::Live(key) => {
                    let peer = app.peers.iter().find(|peer| peer.key == key);
                    if peer.is_some_and(|peer| peer.current) {
                        text(frame, r(row.x, y, 1, 1), ">", theme().ui.emphasis, bg);
                    }
                    peer.map(|peer| peer.state)
                }
                SessionRef::Saved(_, SavedStatus::Closed(state)) => Some(state),
                SessionRef::Saved(_, SavedStatus::Locked) => {
                    text(frame, glyph, LOCKED, theme().ui.muted, bg);
                    None
                }
                SessionRef::Saved(_, SavedStatus::Unknown) => None,
            };
            if let Some(state) = state {
                app.render.animating |= state.running();
                let symbol = agent_symbol(state, app.tick_count);
                text(frame, glyph, symbol, agent_status_color(state), bg);
            }
            let row = r(
                row.x + SESSION_PREFIX,
                y,
                row.width.saturating_sub(SESSION_PREFIX),
                1,
            );
            draw_menu_item(frame, row, item, &item_columns, selected, bg);
        } else {
            draw_menu_item(frame, row, item, &item_columns, selected, bg);
        }
        if selected {
            focus_cursor(frame, rect.x, y, theme().ui.input);
        }
        app.render.hits.push((row, Hit::Menu(i)));
    }
    if items.is_empty() {
        let empty = "No matching entries";
        text(frame, rows_area, empty, theme().ui.muted, theme().ui.input);
    }
    text(
        frame,
        help_area,
        menu.layout().help(),
        theme().ui.muted,
        theme().ui.input,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::ItemRef;
    use crate::tui::model;
    use crate::tui::render::columns::MENU_LABEL_ROOM;
    use ratatui::{Terminal, backend::TestBackend};
    use unicode_width::UnicodeWidthStr;

    #[tokio::test]
    async fn only_pickable_rows_are_highlighted() {
        let (_root, mut app) = crate::tui::app::tests::fixture().await;
        for (command, highlighted) in [
            (crate::tui::commands::Command::Help, false),
            (crate::tui::commands::Command::Commands, true),
        ] {
            app.command(command);
            let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
            app.render.content_rect = r(0, 0, 80, 20);
            terminal.draw(|frame| draw_menu(frame, &mut app)).unwrap();
            let cells = &terminal.backend().buffer().content;
            let found = cells.iter().any(|cell| cell.bg == theme().ui.selected);
            assert_eq!(found, highlighted, "{command}");
            app.menu = None;
        }
    }

    #[tokio::test]
    async fn a_long_title_leaves_rows_and_controls_inside_the_menu() {
        let (_root, mut app) = crate::tui::app::tests::fixture().await;
        app.command(crate::tui::commands::Command::Help);
        app.menu.as_mut().unwrap().title = format!("/{}", "segment/".repeat(20));
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        app.render.content_rect = r(0, 0, 40, 8);
        terminal.draw(|frame| draw_menu(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let line = |y| (0..40).map(|x| buffer[(x, y)].symbol()).collect::<String>();
        let rows: Vec<String> = (0..12).map(line).collect();
        // Nothing is painted below the menu's area.
        assert!(
            rows[8..].iter().all(|row| row.trim().is_empty()),
            "{rows:#?}"
        );
        // The filter and at least one item row stay visible.
        assert!(
            rows.iter().any(|row| row.trim_start().starts_with('>')),
            "{rows:#?}"
        );
        assert!(
            app.render
                .hits
                .iter()
                .any(|(_, hit)| matches!(hit, Hit::Menu(_))),
            "{rows:#?}"
        );
    }

    #[tokio::test]
    async fn agents_palette_hides_columns_without_stacking_rows() {
        use crate::tui::app::played::{
            count, job_ended, observe_until, played_fixture, spawn_children,
        };
        let (_root, mut app, mut model, _session) = played_fixture().await;
        app.command(crate::tui::commands::Command::Agents);
        let (agents, turns) = spawn_children(&mut app, &mut model, 2).await;
        for turn in turns {
            turn.answer("done");
        }
        observe_until(&mut app, |app| count(&app.journal, job_ended) == 2).await;
        let children: Vec<_> = (agents.iter())
            .map(skyhook::identity::AgentId::default_name)
            .collect();
        // An open palette's rows follow agents and their live status, which is searchable.
        app.menu
            .as_mut()
            .unwrap()
            .filter(|input| input.set("completed".into()));
        assert_eq!(app.menu.as_ref().unwrap().filtered().len(), 2);
        app.menu
            .as_mut()
            .unwrap()
            .filter(|input| input.set(String::new()));
        // Which widths drop which columns is the columns' own rule.
        for (width, stats, status) in [(40, false, false), (120, true, true)] {
            let mut terminal = Terminal::new(TestBackend::new(width, 10)).unwrap();
            app.render.content_rect = r(0, 0, width, 10);
            app.render.hits.clear();
            terminal.draw(|frame| draw_menu(frame, &mut app)).unwrap();
            let buffer = terminal.backend().buffer();
            let line = |y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            };
            // One clear row separates the palette from the bars above and below it.
            assert!(line(0).trim().is_empty() && line(9).trim().is_empty());
            // Headers name the status column, and the statistics when they show.
            assert_eq!(line(3).contains("Status"), status);
            let headed = line(3).contains("Input (uncached)") && line(3).contains("Time");
            assert_eq!(headed, stats);
            let hits = app.render.hits.iter();
            let rows: Vec<_> = hits
                .filter_map(|(rect, hit)| matches!(hit, Hit::Menu(_)).then_some(*rect))
                .collect();
            assert_eq!(rows.len(), 3);
            for (index, row) in rows.iter().enumerate() {
                assert_eq!(
                    (row.height, row.y),
                    (1, 3 + u16::from(status) + index as u16)
                );
            }
            for (row, child) in rows[1..].iter().zip(&children) {
                let text = line(row.y);
                assert!(text.contains(child.as_str()), "{text}");
                assert_eq!(text.contains("Completed"), status);
            }
        }
    }

    /// The state fills its column whole and the time sits right-aligned with the
    /// other statistics.
    #[tokio::test]
    async fn an_agents_time_is_a_statistic_beside_its_state() {
        let (_root, mut app) = crate::tui::app::tests::fixture().await;
        app.refresh();
        let agent = &app.projection.agents[0];
        let stats = ["1", "2", "3", "1h 02m 03s"].map(String::from);
        let columns = AgentColumns::new(
            [agent],
            90,
            AgentStatsColumns::new([&stats]),
            TreePlane::Band,
        );
        let state = model::AgentDisplayState::Waiting(model::WaitReason::ParentInput);
        let mut terminal = Terminal::new(TestBackend::new(90, 1)).unwrap();
        terminal
            .draw(|frame| {
                let row = AgentRow {
                    agent,
                    state,
                    timing: format::Timing::Untimed,
                    stats: &stats,
                    marker: "",
                };
                draw_agent_row(frame, frame.area(), row, &columns, 0, theme().ui.panel);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let line: String = (0..90).map(|x| buffer[(x, 0)].symbol()).collect();
        assert!(line.contains(&state.label()), "{line}");
        assert!(line.ends_with("1 · 2 · 3 · 1h 02m 03s"), "{line}");
    }

    /// Columns stay whole and right-aligned; the label keeps its room, so the
    /// leftmost columns go first, and the detail gives way to both. A strong row is
    /// bold throughout, label and columns alike; a normal one is not.
    #[test]
    fn menu_columns_are_muted_right_aligned_and_yield_to_labels() {
        let owned = |values: &[&str]| -> Vec<String> {
            values.iter().map(|value| value.to_string()).collect()
        };
        for (label, detail, columns) in [
            ("New session", "", owned(&["ctrl+x n"])),
            ("界面", "", owned(&["ctrl+shift+p"])),
            ("Unbound", "", owned(&[""])),
            (
                "A long session title",
                "detail",
                owned(&["0123456789abcdef", "9 events", "14:02"]),
            ),
        ] {
            let item = ItemRef {
                index: 0,
                label,
                detail,
                columns: &columns,
                emphasis: Emphasis::Normal,
                search: "",
            };
            for width in [0, 2, 8, 12, 21, 30, 44, 80] {
                let layout = MenuColumns::new(std::slice::from_ref(&item), width);
                for selected in [false, true] {
                    let mut terminal = Terminal::new(TestBackend::new(90, 3)).unwrap();
                    let row = r(3, 1, width, 1);
                    let bg = if selected {
                        theme().ui.selected
                    } else {
                        theme().ui.input
                    };
                    terminal
                        .draw(|frame| {
                            fill(frame, frame.area(), theme().ui.base);
                            fill(frame, row, bg);
                            draw_menu_item(frame, row, &item, &layout, selected, bg);
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
                    let room = usize::from(width) - layout.width();
                    let kept = label.width().min(MENU_LABEL_ROOM).min(usize::from(width));
                    assert!(room >= kept, "{actual:?}");
                    // Shown columns are a suffix, whole and muted at the right edge.
                    let shown: Vec<_> = columns
                        .iter()
                        .map(|value| !value.is_empty() && actual.contains(value.as_str()))
                        .collect();
                    assert!(shown.is_sorted(), "{actual:?}");
                    if let Some(last) = columns.last().filter(|_| layout.width() > 0) {
                        assert!(actual.ends_with(last.as_str()), "{actual:?}");
                        let start = row.right() - last.width() as u16;
                        assert!((start..row.right()).all(|x| cell(x).fg == theme().ui.muted));
                    }
                    if label.width() <= room {
                        assert!(actual.starts_with(label), "{actual:?}");
                    } else if room > 0 {
                        // A cut title ends in an ellipsis, just before any columns.
                        let text: String = actual.chars().take(room).collect();
                        assert!(text.trim_end().ends_with('…'), "{actual:?}");
                    }
                    if !detail.is_empty() {
                        let fits = label.width() + 3 + detail.width() <= room;
                        assert_eq!(actual.contains(detail), fits, "{actual:?}");
                    }
                    if width > 0 && !label.contains('界') {
                        let fg = if selected {
                            theme().ui.emphasis
                        } else {
                            theme().ui.fg
                        };
                        assert_eq!(cell(row.x).fg, fg);
                    }
                    let filled = painted.iter().all(|&x| cell(x).bg == bg);
                    assert!(filled, "row background: {label:?}, width={width}");
                    assert_eq!(cell(row.right()).bg, theme().ui.base);
                }
            }
        }
        let columns = ["0123", "14:02"].map(String::from);
        for (emphasis, bold) in [(Emphasis::Normal, false), (Emphasis::Strong, true)] {
            let item = ItemRef {
                index: 0,
                label: "Session",
                detail: "",
                columns: &columns,
                emphasis,
                search: "",
            };
            let layout = MenuColumns::new(std::slice::from_ref(&item), 40);
            let mut terminal = Terminal::new(TestBackend::new(40, 1)).unwrap();
            terminal
                .draw(|frame| {
                    let row = frame.area();
                    draw_menu_item(frame, row, &item, &layout, false, theme().ui.input);
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            let text = (0..40).filter(|&x| buffer[(x, 0)].symbol() != " ");
            let cells: Vec<_> = text.map(|x| buffer[(x, 0)].modifier).collect();
            assert!(cells.len() > "Session".len() + "14:02".len());
            assert!(cells.iter().all(|m| m.contains(Modifier::BOLD) == bold));
        }
    }
}
