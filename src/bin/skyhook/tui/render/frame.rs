//! Main frame composition and viewport orchestration.

use super::*;
use crate::tui::keys::Command;
use skyhook::agent::AgentActivity;

/// Contextual labels for typed commands, rendered only when they are bound.
fn leader_hints(app: &App) -> Vec<(Command, &'static str)> {
    let mut hints = Vec::new();
    if !app.prompts.is_empty() {
        hints.push((Command::Attention, "Questions"));
    }
    if background_attention(app) > 0 {
        hints.push((Command::Sessions, "Sessions"));
    }
    hints.extend([
        (Command::Model, "Model"),
        (Command::Agents, "Inspect agent"),
    ]);
    if !app.queue.is_empty() {
        hints.push((Command::Queue, "Edit queue"));
    }
    // Advertise continuing only while something can be continued, so a refusal
    // names the key that resolves it.
    if app.snapshot.activity.values().any(|activity| {
        matches!(
            activity,
            AgentActivity::Failed(_) | AgentActivity::Interrupted
        )
    }) {
        hints.push((Command::Retry, "Continue"));
    }
    hints
}

/// Other open sessions waiting on the user.
fn background_attention(app: &App) -> usize {
    let others = app.peers.iter().filter(|peer| !peer.current);
    others.filter(|peer| peer.attention).count()
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let p = Palette::new();
    fill(frame, area, p.base);
    app.hits.clear();
    app.animating = false;
    if area.width < 20 || area.height < 9 {
        text(frame, area, "Enlarge terminal", p.warning, p.base);
        return;
    }
    let width = area.width;
    let height = area.height;
    let context = app
        .snapshot
        .context
        .get(&app.selected)
        .map(|c| (c.tokens, c.capacity));
    let stats = model::footer(app.projection.usage, context);
    let stat_lines = wrap_plain(&stats, width.saturating_sub(2) as usize);
    let footer_height = if width < 70 {
        1 + stat_lines.len() as u16
    } else {
        1
    };
    let prompt_active = app.prompt_active && !app.prompts.is_empty();
    let editor_width = width.saturating_sub(4).max(1) as usize;
    app.editor.set_width(editor_width);
    let editor_layout = app.editor.layout(editor_width);
    let editor_lines: Vec<_> = editor_layout
        .rows
        .iter()
        .map(|row| {
            row.line(
                Style::default(),
                Style::default()
                    .fg(p.accent)
                    .remove_modifier(Modifier::all()),
                Style::default().bg(p.selected),
            )
        })
        .collect();
    let viewing_child = !app.selected.path().is_empty();
    let editor_height =
        (editor_lines.len() as u16 + 2 + u16::from(!app.editor.attachments().is_empty()))
            .clamp(3, 7)
            .min(height.saturating_sub(footer_height + 3).max(3));
    let waiting = background_attention(app);
    let notice_height = u16::from(
        !app.queue.is_empty()
            || waiting > 0
            || (!app.prompts.is_empty() && !prompt_active)
            || app.leader.is_some()
            || app.toast.is_some(),
    );
    let prompt_layout = prompt_active.then(|| PromptLayout::new(app, width));
    let composer_height = if let Some(layout) = &prompt_layout {
        layout
            .height()
            .max(7)
            .min(height.saturating_sub(footer_height + 2 + notice_height))
    } else if viewing_child {
        0
    } else {
        editor_height
    };
    let tree_agents = app.projection.visible(&app.selected);
    let composer_y = height.saturating_sub(footer_height + composer_height);
    let tree_capacity = composer_y.saturating_sub(3 + notice_height).min(
        (height / 4).clamp(4, 10)
            + if viewing_child && !prompt_active {
                editor_height
            } else {
                0
            },
    );
    let show_tree = !app.selected.path().is_empty() || app.projection.has_active_children();
    let tree_rows = if show_tree && tree_capacity >= 3 {
        (tree_agents.len() as u16).min(tree_capacity - 2)
    } else {
        0
    };
    let tree_height = if tree_rows > 0 { tree_rows + 2 } else { 0 };
    if tree_height == 0 && app.focus == Focus::Tree {
        app.focus = if viewing_child {
            Focus::Content
        } else {
            Focus::Composer
        };
    }
    if viewing_child && !prompt_active && app.focus == Focus::Composer {
        app.focus = Focus::Content;
    }
    let tree_y = composer_y.saturating_sub(tree_height);
    app.composer_rect = r(0, composer_y, width, composer_height);
    app.tree_rect = r(0, tree_y, width, tree_height);
    let content_width = width - sidebar_width(app, width);
    app.content_rect = r(
        0,
        2,
        content_width,
        tree_y.saturating_sub(2 + notice_height),
    );
    if content_width < width {
        let rect = r(
            content_width,
            2,
            width - content_width,
            app.content_rect.height,
        );
        draw_sidebar(frame, app, rect, p);
    }
    fill(frame, r(0, 0, width, 2), p.panel);
    let workspace = app.launch.workspace.display().to_string();
    let workspace = clipped_header(&workspace, width.saturating_sub(4));
    let workspace_width = workspace.width() as u16;
    text(
        frame,
        r((width - workspace_width) / 2, 0, workspace_width, 1),
        workspace,
        p.fg,
        p.panel,
    );
    let tab = app.view().tab;
    let mut x = 2;
    for (label, value) in [
        ("Conversation", Tab::Conversation),
        ("Requests", Tab::Requests),
        ("Jobs", Tab::Jobs),
    ] {
        let label = if width < 50 {
            match value {
                Tab::Conversation => "Chat",
                Tab::Requests => "Calls",
                Tab::Jobs => "Jobs",
            }
        } else {
            label
        };
        let n = label.len() as u16 + 2;
        let rect = r(x, 1, n, 1);
        text(
            frame,
            rect,
            format!(" {label} "),
            if tab == value { p.accent } else { p.muted },
            if tab == value { p.selected } else { p.panel },
        );
        app.hits.push((rect, Hit::Tab(value)));
        x += n + 1;
    }
    prepare_rows(app, content_width, p);
    if tab == Tab::Requests
        && app.content_rect.height > 1
        && let Some(entry) = app.entries().iter().find(|entry| entry.request().is_some())
    {
        let geometry = EntryGeometry::new(entry, content_width, 0);
        if let Some(header) = app.render.request_columns.header(geometry.body_width, p) {
            text(
                frame,
                r(geometry.x, app.content_rect.y, geometry.body_width, 1),
                header,
                p.content.muted,
                p.base,
            );
            // The fixed header is not an entry: paging, hit testing and selection
            // all use a viewport containing data rows only.
            app.content_rect.y += 1;
            app.content_rect.height -= 1;
        }
    }
    let max = app
        .content_rows
        .saturating_sub(app.content_rect.height as usize);
    let scroll = app.view().scroll.map(|n| n.min(max)).unwrap_or(max);
    if app.view().scroll.is_some() {
        app.view().scroll = Some(scroll);
    }
    let selected_entry = app.view().row;
    let view = app.views.get(&app.selected).expect("selected view");
    let mut expanded_entry = None;
    let navigation_active = app.menu.is_none() && app.search_editor.is_none() && !prompt_active;
    let mut cursor_drawn = false;
    if app.entries().is_empty() {
        text(
            frame,
            r(3, 4, content_width.saturating_sub(6), 1),
            "Start a conversation, or /sessions to open one.",
            p.muted,
            p.base,
        );
    }
    for (offset, row) in app
        .render
        .rows
        .iter_from(scroll)
        .take(app.content_rect.height as usize)
        .enumerate()
    {
        let y = app.content_rect.y + offset as u16;
        let rect = r(row.x, y, row.width, 1);
        // Only rows inside the selection's row span need their text.
        let row_text = app
            .selection
            .filter(|(a, b)| (a.row.min(b.row)..=a.row.max(b.row)).contains(&(scroll + offset)))
            .map(|_| row.text_view());
        let selected = row_text
            .as_ref()
            .and_then(|view| view.selection_range(scroll + offset, app.selection));
        let entry = app.content_cache.entries().get(row.entry);
        let expanded = match expanded_entry {
            Some((index, expanded)) if index == row.entry => expanded,
            _ => {
                let expanded = entry.is_some_and(|entry| entry.is_expanded(view, app.details));
                expanded_entry = Some((row.entry, expanded));
                expanded
            }
        };
        let focused = navigation_active
            && app.focus == Focus::Content
            && row.selectable
            && row.entry == selected_entry;
        let hovered = navigation_active
            && app.hover.is_some_and(|point| rect.contains(point.into()))
            && entry.is_some_and(|e| e.expandable());
        let bg = row.background(
            p,
            expanded,
            (focused && entry.is_some_and(|e| e.expandable())) || hovered,
            selected.is_some(),
        );
        fill(frame, rect, bg);
        let text_rect = r(
            row.paragraph_x(),
            y,
            row.width.saturating_sub(row.inset).saturating_sub(
                if matches!(row.surface, Surface::User | Surface::Agent) {
                    4
                } else {
                    0
                },
            ),
            1,
        );
        render_row_line(
            row,
            text_rect,
            frame.buffer_mut(),
            Style::default().fg(p.foreground(row.surface)).bg(bg),
            p,
        );
        if let Some(range) = selected {
            let view = row_text.as_ref().expect("selected range has row text");
            // Source-to-column geometry skips hanging prefixes and code padding.
            // A malformed layout yields no cells; never paint it elsewhere.
            for (byte, column, width) in view.source_cells() {
                if !range.contains(&byte) {
                    continue;
                }
                let start = row.paragraph_x() as usize + column;
                let end = start + width;
                let source_end = row.layout.code().map_or(content_width as usize, |code| {
                    (row.paragraph_x() as usize + code.body_end()).min(content_width as usize)
                });
                if start >= source_end {
                    break;
                }
                if end > source_end {
                    continue;
                }
                for x in start..end {
                    frame.buffer_mut()[(x as u16, y)].set_bg(p.selected);
                }
            }
        }
        if !row.layout.is_spacer() {
            if row.layout.header() && entry.is_some_and(|entry| entry.running) {
                app.animating = true;
                // Paint only the spinner; cached reasoning rows need no relayout on ticks.
                text(
                    frame,
                    r(
                        row.x
                            + if entry.is_some_and(|entry| entry.expandable()) {
                                2
                            } else {
                                0
                            },
                        y,
                        1,
                        1,
                    ),
                    spinner(app.tick_count),
                    if row.surface == Surface::Tool
                        && entry.is_some_and(|entry| entry.header().is_some())
                    {
                        p.content.primary
                    } else {
                        p.muted
                    },
                    bg,
                );
            }
            if focused && !cursor_drawn {
                focus_cursor(frame, row.x.saturating_sub(1), y, p.base);
                cursor_drawn = true;
            }
            if row.selectable {
                app.hits.push((
                    rect,
                    Hit::Entry(
                        row.entry,
                        row.layout.header() || entry.is_some_and(|entry| entry.expandable()),
                    ),
                ));
            }
        }
    }
    if app.content_rows > app.content_rect.height as usize && app.content_rect.height > 0 {
        let thumb = app.content_rect.y
            + ((scroll as u64 * app.content_rect.height.saturating_sub(1) as u64)
                / max.max(1) as u64) as u16;
        text(
            frame,
            r(content_width - 1, thumb, 1, 1),
            "▐",
            p.muted,
            p.base,
        );
        if app
            .views
            .get(&app.selected)
            .is_some_and(|v| v.scroll.is_some())
        {
            let rect = r(
                content_width.saturating_sub(18),
                app.content_rect.bottom().saturating_sub(1),
                17,
                1,
            );
            // Text-only overlay: retain every underlying cell's background.
            render_line(
                &Line::from("↓ Latest activity"),
                rect,
                frame.buffer_mut(),
                Style::default().fg(p.accent),
            );
            app.hits.push((rect, Hit::Latest));
        }
    }
    if let Some(search) = &app.search_editor {
        text(
            frame,
            r(2, app.content_rect.y, content_width.saturating_sub(4), 1),
            format!("Find: {}▏", search.text()),
            p.fg,
            p.input,
        );
    }
    if notice_height > 0 {
        let message = if let Some(prefix) = app.leader {
            app.keys.leader_hint(prefix, &leader_hints(app))
        } else if let Some((message, _)) = &app.toast {
            message.clone()
        } else if !app.prompts.is_empty() && !prompt_active {
            let hint = match app.keys.binding(Command::Attention) {
                Some(binding) => format!("{binding} reopen"),
                None => "Reopen questions and permissions in the command palette".to_owned(),
            };
            format!("{} pending request(s) · {hint}", app.prompts.len())
        } else if waiting > 0 {
            let hint = app
                .keys
                .binding(Command::Sessions)
                .unwrap_or("/sessions".into());
            format!("{waiting} other session(s) need attention · {hint}")
        } else if !app.queue.is_empty() {
            format!(
                "{} follow-up(s) queued{} · /queue",
                app.queue.len(),
                if app.paused { " · paused" } else { "" }
            )
        } else {
            String::new()
        };
        let rect = r(1, tree_y.saturating_sub(1), width.saturating_sub(2), 1);
        text(frame, rect, model::clean(&message), p.warning, p.base);
        if app.leader.is_none() && app.toast.is_none() {
            app.hits.push((
                rect,
                if !app.prompts.is_empty() && !prompt_active {
                    Hit::Attention
                } else if waiting > 0 {
                    Hit::Sessions
                } else {
                    Hit::Queue
                },
            ));
        }
    }
    draw_tree(frame, app, p, &tree_agents, tree_rows, navigation_active);
    fill(frame, app.composer_rect, p.input);
    if let Some(layout) = &prompt_layout {
        draw_prompt(frame, app, p, layout);
    } else if !viewing_child {
        let (cursor_line, cursor_column) = editor_layout.cursor;
        let visible = composer_height.saturating_sub(2) as usize;
        let top = cursor_line.saturating_sub(visible.saturating_sub(1));
        for (i, line) in editor_lines.iter().skip(top).take(visible).enumerate() {
            text(
                frame,
                r(2, composer_y + 1 + i as u16, width.saturating_sub(4), 1),
                line.clone(),
                p.fg,
                p.input,
            );
        }
        if !app.editor.attachments().is_empty() {
            text(
                frame,
                r(
                    2,
                    composer_y + composer_height - 1,
                    width.saturating_sub(4),
                    1,
                ),
                app.editor
                    .attachments()
                    .iter()
                    .map(
                        |attachment| match attachment.file().and_then(|file| file.file_name()) {
                            Some(name) => format!("[{}]", name.to_string_lossy()),
                            None => match attachment {
                                skyhook::media::Attachment::Text { .. } => "[text]".to_owned(),
                                skyhook::media::Attachment::Image { .. } => "[image]".to_owned(),
                            },
                        },
                    )
                    .collect::<Vec<_>>()
                    .join(" "),
                p.muted,
                p.input,
            );
            app.hits.push((
                r(0, composer_y + composer_height - 1, width, 1),
                Hit::Attachments,
            ));
        }
        if app.focus == Focus::Composer && app.menu.is_none() && app.search_editor.is_none() {
            let column = cursor_column as u16;
            frame.set_cursor_position((
                2 + column.min(width.saturating_sub(4)),
                composer_y + 1 + (cursor_line - top) as u16,
            ));
        }
        app.hits.insert(0, (app.composer_rect, Hit::Composer));
    }
    let fy = height - footer_height;
    fill(frame, r(0, fy, width, footer_height), p.base);
    let agent = app.projection.agents.iter().find(|a| a.id == app.selected);
    let model = if app.selected.path().is_empty() {
        app.model.as_str()
    } else {
        agent.map_or(app.model.as_str(), |a| a.model.as_str())
    };
    let model = app
        .launch
        .model
        .config()
        .config()
        .models
        .get(model)
        .map_or(model, |profile| profile.model.as_str());
    // The root composer's next message goes out in this mode; children have none.
    let root_label = format!("{} · {model}", app.mode);
    let model = if app.selected.path().is_empty() {
        &root_label
    } else {
        model
    };
    let session = app.session().as_ref().map_or_else(
        || "new session".to_owned(),
        |session| session.id().to_string(),
    );
    if footer_height > 1 {
        text(
            frame,
            r(1, fy, width.saturating_sub(2), 1),
            footer_metadata(model, &session, width.saturating_sub(2)),
            p.muted,
            p.base,
        );
        for (index, line) in stat_lines.iter().enumerate() {
            text(
                frame,
                r(1, fy + 1 + index as u16, width.saturating_sub(2), 1),
                line.clone(),
                p.fg,
                p.base,
            );
        }
    } else {
        let stat_width = stats.width() as u16;
        let model_width = width.saturating_sub(stat_width + 4);
        if model_width > 0 {
            text(
                frame,
                r(1, fy, model_width, 1),
                footer_metadata(model, &session, model_width),
                p.muted,
                p.base,
            );
        }
        text(
            frame,
            r(
                width.saturating_sub(stat_width + 1),
                fy,
                stat_width.min(width),
                1,
            ),
            stats,
            p.fg,
            p.base,
        );
    }
    draw_menu(frame, app, p);
}

/// Keep both model and session identifiable when the footer is compact.
fn footer_metadata(model: &str, session: &str, width: u16) -> String {
    if width < 5 {
        return clipped_header(model, width);
    }
    let available = width - 3; // Separator between model and session.
    let session_width = session
        .width()
        .min((available - (available / 2).min(16)) as usize);
    let model_width = model.width().min(available as usize - session_width) as u16;
    let session_width = available - model_width;
    format!(
        "{} · {}",
        clipped_header(model, model_width),
        clipped_header(session, session_width),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::{
        app::{Work, tests::fixture},
        theme::ContentTheme,
    };
    use crossterm::event::{Event, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::{Terminal, backend::TestBackend};
    use skyhook::{
        provider::protocol::{AssistantItem, Message},
        session::{EventRecord, SessionEvent},
    };
    use std::time::Duration;

    fn push_record(app: &mut App, event: SessionEvent) {
        let records = &app.snapshot.records;
        let sequence = records
            .last_key_value()
            .map_or(1, |(sequence, _)| sequence + 1);
        let record = EventRecord {
            id: skyhook::identity::EventId::generate().unwrap(),
            sequence,
            timestamp_millis: sequence as i64 * 1000,
            agent: app.selected.clone(),
            event,
        };
        app.snapshot.records.insert(sequence, record);
    }

    fn commit_message(app: &mut App, text: &str) {
        let message = Message::Assistant(vec![AssistantItem::text("frame-test", 0, text)]);
        push_record(app, SessionEvent::MessageCommitted { message });
        app.refresh();
    }

    #[tokio::test]
    async fn sidebar_narrows_only_the_transcript_and_persists_its_toggle() {
        use skyhook::agent::{TodoItem, TodoStatus};
        let (root, mut app) = fixture().await;
        let text = "right-aligned user text ".repeat(12);
        let message = Message::User(vec![skyhook::provider::protocol::UserContent::Text {
            text,
        }]);
        push_record(&mut app, SessionEvent::MessageCommitted { message });
        let items = [
            ("parse", TodoStatus::Completed),
            (
                "render the wrapped todo text wholeword",
                TodoStatus::InProgress,
            ),
        ];
        let items = items.map(|(text, status)| TodoItem {
            text: text.into(),
            status,
        });
        push_record(
            &mut app,
            SessionEvent::TodosReplaced {
                items: items.into(),
            },
        );
        let screen = |app: &mut App, width| {
            let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
            terminal.draw(|frame| draw(frame, app)).unwrap();
            let buffer = terminal.backend().buffer();
            let rows = (0..24).map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect());
            rows.collect::<Vec<String>>().join("\n")
        };
        // Sections with nothing to list are left out, title included.
        let bare = screen(&mut app, 120);
        assert!(bare.contains("Capabilities") && bare.contains("· Read files"));
        assert!(!bare.contains("MCP servers") && !bare.contains("Todos"));
        app.refresh();
        let wide = screen(&mut app, 120);
        for expected in [
            "Todos 1/2",
            "✓ parse",
            "● render the wrapped todo   ",
            "    text wholeword",
            "Capabilities",
        ] {
            assert!(wide.contains(expected), "{expected}\n{wide}");
        }
        assert!(!wide.contains("MCP servers"));
        // Capabilities are pinned to the bottom: a gap separates them from the todos.
        let rows: Vec<_> = wide.lines().collect();
        let row = |text: &str| rows.iter().position(|row| row.contains(text)).unwrap();
        assert!(row("Capabilities") > row("text wholeword") + 2);
        // One padding row above the first section and below the last.
        let column = |y: usize| rows[y].chars().skip(88).collect::<String>();
        let bottom = app.content_rect.bottom() as usize - 1;
        assert!(column(2).trim().is_empty() && column(3).starts_with(" Todos 1/2"));
        // A blank row under the title; what it lists sits one column further in.
        assert!(column(4).trim().is_empty() && column(5).starts_with("  ✓ parse"));
        assert!(column(bottom).trim().is_empty() && column(bottom - 1).contains("· Use MCP tools"));
        // A pending mode shows what the next message will be granted.
        app.mode = "missing".into();
        assert!(!screen(&mut app, 120).contains("Capabilities"));
        app.mode = "general".into();
        assert_eq!(app.content_rect.width, 88);
        assert!(
            app.hits
                .iter()
                .all(|(rect, hit)| !matches!(hit, Hit::Entry(..)) || rect.right() <= 88)
        );
        screen(&mut app, 160);
        assert_eq!(app.content_rect.width, 120);
        // A cramped terminal hides the column without changing the setting.
        assert!(!screen(&mut app, 99).contains("Todos"));
        assert_eq!((app.content_rect.width, app.sidebar), (99, true));
        app.command(Command::Sidebar);
        assert!(!screen(&mut app, 120).contains("Todos"));
        assert_eq!(app.content_rect.width, 120);
        let saved = || crate::tui::state::load(root.path()).0.sidebar;
        tokio::time::timeout(Duration::from_secs(5), async {
            while saved() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("toggle is saved");
    }

    #[tokio::test]
    async fn request_headers_stay_outside_the_scrolling_rows() {
        let (_root, mut app) = fixture().await;
        for _ in 0..30 {
            let purpose = skyhook::session::ModelPurpose::Agent;
            push_record(
                &mut app,
                SessionEvent::ModelRequested {
                    context: 0,
                    history: Vec::new(),
                    tail: Vec::new(),
                    history_lifetime: Default::default(),
                    purpose,
                },
            );
        }
        app.view().tab = Tab::Requests;
        app.refresh();
        for (width, header) in [(80, true), (40, false)] {
            let mut terminal = Terminal::new(TestBackend::new(width, 25)).unwrap();
            for scroll in [0, 5] {
                app.view().scroll = Some(scroll);
                terminal.draw(|frame| draw(frame, &mut app)).unwrap();
                let buffer = terminal.backend().buffer();
                let line: String = (0..width).map(|x| buffer[(x, 2)].symbol()).collect();
                assert_eq!(line.contains("Input (uncached)"), header);
                assert_eq!(
                    (app.content_rect.y, app.content_rows),
                    (2 + u16::from(header), 30)
                );
                let first = app.hits.iter().find_map(|(rect, hit)| match hit {
                    Hit::Entry(index, _) => Some((rect.y, *index)),
                    _ => None,
                });
                assert_eq!(first, Some((app.content_rect.y, scroll)));
            }
        }
    }

    fn number_is_painted(buffer: &Buffer, color: Color) -> bool {
        let source = "let answer = 42;";
        buffer
            .content
            .chunks(buffer.area.width as usize)
            .any(|row| {
                row.windows(source.len()).any(|cells| {
                    cells.iter().map(|cell| cell.symbol()).collect::<String>() == source
                        && cells[13..15].iter().all(|cell| cell.fg == color)
                })
            })
    }

    #[tokio::test]
    async fn expanded_items_paint_solid_code_backgrounds_across_clipped_rows() {
        let (_root, mut app) = fixture().await;
        app.content_dirty = false;
        let text = format!("Expandable tool\n{}", "body\n\n".repeat(20));
        let mut entry =
            model::Entry::expandable_text(model::EntryKey::Record(1), text, Surface::Tool);
        entry.default_open = true;
        app.install_entries(vec![entry]);
        let p = Palette::new();
        for width in [30, 60] {
            let mut terminal = Terminal::new(TestBackend::new(width, 18)).unwrap();
            terminal.draw(|frame| draw(frame, &mut app)).unwrap();
            let scroll = app.content_rows - app.content_rect.height as usize;
            assert!(scroll > 0, "exercise a viewport clipped inside the body");
            let buffer = terminal.backend().buffer();
            let mut empty_body_rows = 0;
            for (offset, row) in app.render.rows.iter().skip(scroll).enumerate() {
                let y = app.content_rect.y + offset as u16;
                let spacer = row.layout.is_spacer();
                let expected = if spacer { p.base } else { p.content.code_bg };
                empty_body_rows += usize::from(!spacer && row.text().is_empty());
                for x in row.x..row.x + row.width {
                    assert_eq!(
                        buffer[(x, y)].bg,
                        expected,
                        "width={width}, row={offset}, x={x}"
                    );
                }
            }
            assert!(empty_body_rows > 0, "blank body lines must also be filled");
        }
    }

    #[tokio::test]
    async fn completed_highlights_repaint_retained_frames_after_reset_and_resize() {
        let (_root, mut app) = fixture().await;
        let (notify, mut ready) = tokio::sync::mpsc::unbounded_channel();
        app.render = RenderState::new(app.selected.clone(), notify);
        commit_message(
            &mut app,
            "# Example\n\n```rust\nlet answer = 42;\n```\n\n**Done**",
        );
        let records = serde_json::to_vec(&app.snapshot.records).unwrap();
        let accent = ContentTheme::new().accent;
        // A cold frame, an explicit reset at unchanged dimensions, then resizes
        // with warm highlights at different widths.
        for (width, reset, completion) in [
            (60, false, true),
            (60, true, true),
            (90, false, false),
            (40, false, false),
        ] {
            if reset {
                app.render.reset_session();
            }
            let mut terminal = Terminal::new(TestBackend::new(width, 30)).unwrap();
            terminal.draw(|frame| draw(frame, &mut app)).unwrap();
            if completion {
                // The first frame paints fallback text and schedules the worker.
                // Its retained rows must repaint when the worker wakes the UI,
                // without manually invalidating content or rebuilding layout.
                assert!(!number_is_painted(terminal.backend().buffer(), accent));
                assert!(!app.render.changes.reset && app.render.changes.dirty.is_empty());
                assert!(!app.content_dirty);
                let work = tokio::time::timeout(Duration::from_secs(5), ready.recv()).await;
                let work = work.expect("highlight worker woke the UI").unwrap();
                assert!(matches!(work, Work::HighlightsReady));
                app.work(work);
                terminal.draw(|frame| draw(frame, &mut app)).unwrap();
            }
            assert!(
                number_is_painted(terminal.backend().buffer(), accent),
                "completed highlights did not reach the frame: width={width}, reset={reset}"
            );
            assert_eq!(serde_json::to_vec(&app.snapshot.records).unwrap(), records);
        }
    }

    #[tokio::test]
    async fn latest_activity_hit_restores_follow_tail_and_disappears() {
        let (_root, mut app) = fixture().await;
        commit_message(&mut app, &"ordinary prose\n".repeat(100));
        let latest = |app: &App| {
            let mut hits = app.hits.iter();
            hits.find_map(|(rect, hit)| matches!(hit, Hit::Latest).then_some(*rect))
        };
        let mut terminal = Terminal::new(TestBackend::new(80, 25)).unwrap();
        app.view().scroll = None;
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(latest(&app).is_none());

        app.view().scroll = Some(0);
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let rect = latest(&app).expect("scrolled history should offer Latest activity");
        let buffer = terminal.backend().buffer();
        let label: String = (rect.x..rect.right())
            .map(|x| buffer[(x, rect.y)].symbol())
            .collect();
        assert_eq!(label, "↓ Latest activity");
        assert_eq!(app.view().scroll, Some(0));

        // Use the rendered hit coordinates and the real event handler. The
        // overlay overlaps selectable text, which must not repin the view.
        let left = MouseButton::Left;
        for kind in [MouseEventKind::Down(left), MouseEventKind::Up(left)] {
            let (column, row, modifiers) = (rect.x, rect.y, KeyModifiers::NONE);
            app.event(Event::Mouse(MouseEvent {
                kind,
                column,
                row,
                modifiers,
            }));
        }
        assert!(app.view().scroll.is_none() && app.selection.is_none());
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(app.view().scroll.is_none());
        assert!(latest(&app).is_none());
    }
}
