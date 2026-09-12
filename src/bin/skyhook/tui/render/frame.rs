//! Main frame composition and viewport orchestration.

use super::*;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let p = Palette::new(app.light);
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
    let editor_height = (editor_lines.len() as u16 + 2 + u16::from(!app.images.is_empty()))
        .clamp(3, 7)
        .min(height.saturating_sub(footer_height + 3).max(3));
    let composer_height = if prompt_active {
        (app.prompt_options().len() as u16 + 6)
            .clamp(7, 12)
            .min(height / 2)
    } else if viewing_child {
        0
    } else {
        editor_height
    };
    let tree_agents = app.projection.visible(&app.selected);
    let notice_height = u16::from(
        !app.queue.is_empty()
            || (!app.prompts.is_empty() && !prompt_active)
            || app.leader.is_some(),
    );
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
    app.content_rect = r(0, 2, width, tree_y.saturating_sub(2 + notice_height));
    fill(frame, r(0, 0, width, 2), p.panel);
    let name = app
        .projection
        .agents
        .iter()
        .find(|a| a.id == app.selected)
        .map_or("skyhook", |a| a.name.as_str());
    // Keep the session ID visible alongside the workspace on narrow terminals.
    let workspace = app.launch.workspace.display().to_string();
    let session = app.session.as_ref().map_or_else(
        || "new session".to_owned(),
        |session| session.id().to_string(),
    );
    let available = width.saturating_sub(4);
    let name_width = if available >= 60 {
        (name.width() as u16).min(20)
    } else {
        0
    };
    let mut x = 2;
    if name_width > 0 {
        text(
            frame,
            r(x, 0, name_width, 1),
            name.to_owned(),
            p.fg,
            p.panel,
        );
        x += name_width;
        text(frame, r(x, 0, 3, 1), " · ", p.muted, p.panel);
        x += 3;
    }
    let remaining = width.saturating_sub(x + 2).saturating_sub(3);
    let session_width = (session.width() as u16).min(remaining - (remaining / 2).min(16));
    let workspace_width = (workspace.width() as u16).min(remaining.saturating_sub(session_width));
    let session_width = (session.width() as u16).min(remaining - workspace_width);
    text(
        frame,
        r(x, 0, workspace_width, 1),
        clipped_header(&workspace, workspace_width),
        p.fg,
        p.panel,
    );
    x += workspace_width;
    text(frame, r(x, 0, 3, 1), " · ", p.muted, p.panel);
    x += 3;
    text(
        frame,
        r(x, 0, session_width, 1),
        clipped_header(&session, session_width),
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
    prepare_rows(app, width, p);
    if tab == Tab::Requests
        && app.content_rect.height > 1
        && let Some(entry) = app.entries.iter().find(|entry| entry.request.is_some())
    {
        let geometry = EntryGeometry::new(entry, width, 0);
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
    if app.entries.is_empty() {
        text(
            frame,
            r(3, 4, width.saturating_sub(6), 1),
            "Start a conversation, or /sessions to resume one.",
            p.muted,
            p.base,
        );
    }
    for (offset, row) in app
        .render
        .rows
        .iter()
        .skip(scroll)
        .take(app.content_rect.height as usize)
        .enumerate()
    {
        let y = app.content_rect.y + offset as u16;
        let rect = r(row.x, y, row.width, 1);
        let selected = row.selection_range(scroll + offset, app.selection);
        let entry = app.entries.get(row.entry);
        let expanded = match expanded_entry {
            Some((index, expanded)) if index == row.entry => expanded,
            _ => {
                let expanded = entry.is_some_and(|entry| entry_expanded(entry, view, app.details));
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
            && entry.is_some_and(|e| e.expandable);
        let bg = row.background(
            p,
            expanded,
            (focused && entry.is_some_and(|e| e.expandable)) || hovered,
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
            let value = row.text();
            // Source-to-column geometry skips hanging prefixes and code padding.
            for (byte, grapheme) in value.grapheme_indices(true) {
                if !range.contains(&byte) {
                    continue;
                }
                let start = row.paragraph_x() as usize + row.source_column(&value, byte);
                let end = start + grapheme.width();
                let source_end = row.layout.code.map_or(width as usize, |code| {
                    (row.paragraph_x() as usize + code.indent + code.width - code.padding)
                        .min(width as usize)
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
        if !row.blank {
            if row.header && entry.is_some_and(|entry| entry.running) {
                app.animating = true;
                // Paint only the spinner; cached reasoning rows need no relayout on ticks.
                text(
                    frame,
                    r(
                        row.x
                            + if entry.is_some_and(|entry| entry.expandable) {
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
                        && entry.is_some_and(|entry| entry.header.is_some())
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
                        row.header || entry.is_some_and(|entry| entry.expandable),
                    ),
                ));
            }
        }
    }
    if app.content_rows > app.content_rect.height as usize && app.content_rect.height > 0 {
        let thumb = app.content_rect.y
            + ((scroll as u64 * app.content_rect.height.saturating_sub(1) as u64)
                / max.max(1) as u64) as u16;
        text(frame, r(width - 1, thumb, 1, 1), "▐", p.muted, p.base);
        if app
            .views
            .get(&app.selected)
            .is_some_and(|v| v.scroll.is_some())
        {
            let rect = r(
                width.saturating_sub(18),
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
            r(2, app.content_rect.y, width.saturating_sub(4), 1),
            format!("Find: {}▏", search.text),
            p.fg,
            p.input,
        );
    }
    if notice_height > 0 {
        let message = if app.leader.is_some() {
            "Ctrl+X: N new · L sessions · M model · A agents · I inspect · E editor · Q quit".into()
        } else if !app.prompts.is_empty() && !prompt_active {
            format!(
                "{} request(s) need attention · /attention",
                app.prompts.len()
            )
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
        app.hits.push((rect, Hit::Attention));
    }
    draw_tree(frame, app, p, &tree_agents, tree_rows, navigation_active);
    fill(frame, app.composer_rect, p.input);
    if prompt_active {
        draw_prompt(frame, app, p);
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
        if !app.images.is_empty() {
            text(
                frame,
                r(
                    2,
                    composer_y + composer_height - 1,
                    width.saturating_sub(4),
                    1,
                ),
                app.images
                    .iter()
                    .map(|path| {
                        format!(
                            "[{}]",
                            path.file_name().unwrap_or_default().to_string_lossy()
                        )
                    })
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
    let metadata = app
        .launch
        .config
        .models
        .get(model)
        .map_or(model, |profile| profile.model.as_str())
        .to_owned();
    if footer_height > 1 {
        text(
            frame,
            r(1, fy, width.saturating_sub(2), 1),
            metadata,
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
            text(frame, r(1, fy, model_width, 1), metadata, p.muted, p.base);
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

    fn commit_message(app: &mut App, text: &str) {
        let sequence = app
            .snapshot
            .records
            .last_key_value()
            .map_or(1, |(sequence, _)| sequence + 1);
        app.snapshot.records.insert(
            sequence,
            EventRecord {
                version: 1,
                sequence,
                timestamp_millis: 0,
                agent: app.selected.clone(),
                event: SessionEvent::MessageCommitted {
                    message: Message::Assistant(vec![AssistantItem::text("frame-test", 0, text)]),
                },
            },
        );
        app.refresh();
    }

    #[tokio::test]
    async fn request_headers_stay_outside_the_scrolling_rows() {
        let (_root, mut app) = fixture().await;
        let first = app
            .snapshot
            .records
            .last_key_value()
            .map_or(1, |(seq, _)| seq + 1);
        for sequence in first..first + 30 {
            app.snapshot.records.insert(
                sequence,
                EventRecord {
                    version: 1,
                    sequence,
                    timestamp_millis: sequence as i64 * 1000,
                    agent: app.selected.clone(),
                    event: SessionEvent::ModelRequested {
                        context: 0,
                        messages: Vec::new(),
                        purpose: skyhook::session::ModelPurpose::Agent,
                    },
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
                let line = (0..width)
                    .map(|x| terminal.backend().buffer()[(x, 2)].symbol())
                    .collect::<String>();
                assert_eq!(line.contains("Input (uncached)"), header);
                assert_eq!(app.content_rect.y, 2 + u16::from(header));
                assert_eq!(app.content_rows, 30);
                let (rect, index) = app
                    .hits
                    .iter()
                    .find_map(|(rect, hit)| match hit {
                        Hit::Entry(index, _) => Some((rect, *index)),
                        _ => None,
                    })
                    .unwrap();
                assert_eq!(rect.y, app.content_rect.y);
                assert_eq!(index, scroll);
            }
        }
        app.session.as_ref().unwrap().shutdown().await.unwrap();
    }

    fn number_is_painted(buffer: &Buffer, color: Color) -> bool {
        buffer
            .content
            .chunks(buffer.area.width as usize)
            .any(|row| {
                row.windows("let answer = 42;".len()).any(|cells| {
                    cells.iter().map(|cell| cell.symbol()).collect::<String>() == "let answer = 42;"
                        && cells[13..15].iter().all(|cell| cell.fg == color)
                })
            })
    }

    #[tokio::test]
    async fn expanded_items_paint_solid_code_backgrounds_across_clipped_rows() {
        let (_root, mut app) = fixture().await;
        app.content_dirty = false;
        app.entries = vec![model::Entry {
            key: "solid-background".into(),
            text: format!("Expandable tool\n{}", "body\n\n".repeat(20)),
            surface: Surface::Tool,
            expandable: true,
            default_open: true,
            running: false,
            footer: None,
            request: None,
            indent: 0,
            job: None,
            compact_after: false,
            header: None,
            document: None,
        }];
        for light in [false, true] {
            app.light = light;
            let p = Palette::new(light);
            for width in [30, 60] {
                let mut terminal = Terminal::new(TestBackend::new(width, 18)).unwrap();
                terminal.draw(|frame| draw(frame, &mut app)).unwrap();
                let scroll = app.content_rows - app.content_rect.height as usize;
                assert!(scroll > 0, "exercise a viewport clipped inside the body");
                let buffer = terminal.backend().buffer();
                let mut empty_body_rows = 0;
                for (offset, row) in app.render.rows.iter().skip(scroll).enumerate() {
                    let y = app.content_rect.y + offset as u16;
                    let expected = if row.blank { p.base } else { p.content.code_bg };
                    if !row.blank && row.text().is_empty() {
                        empty_body_rows += 1;
                    }
                    for x in row.x..row.x + row.width {
                        assert_eq!(
                            buffer[(x, y)].bg,
                            expected,
                            "light={light}, width={width}, row={offset}, x={x}"
                        );
                    }
                }
                assert!(empty_body_rows > 0, "blank body lines must also be filled");
            }
        }
    }

    #[tokio::test]
    async fn completed_highlights_repaint_retained_frames_after_reset_resize_and_theme_changes() {
        let (_root, mut app) = fixture().await;
        let (notify, mut ready) = tokio::sync::mpsc::unbounded_channel();
        app.render = RenderState::new(app.selected.clone(), app.light, notify);
        commit_message(
            &mut app,
            "# Example\n\n```rust\nlet answer = 42;\n```\n\n**Done**",
        );
        let records = serde_json::to_vec(&app.snapshot.records).unwrap();

        // Exercise a cold frame, an explicit reset at unchanged dimensions, a
        // resize with warm highlights, a new theme, and the cached original theme.
        for (light, width, reset, completion) in [
            (false, 60, false, true),
            (false, 60, true, true),
            (false, 90, false, false),
            (true, 90, false, true),
            (false, 40, false, false),
        ] {
            app.light = light;
            if reset {
                app.render.reset_session();
            }
            let mut terminal = Terminal::new(TestBackend::new(width, 30)).unwrap();
            terminal.draw(|frame| draw(frame, &mut app)).unwrap();
            let accent = ContentTheme::new(light).accent;
            if completion {
                // The first frame paints fallback text and schedules the worker.
                // Its retained rows must repaint when the worker wakes the UI,
                // without manually invalidating content or rebuilding layout.
                assert!(!number_is_painted(terminal.backend().buffer(), accent));
                assert!(!app.render.changes.reset);
                assert!(app.render.changes.dirty.is_empty());
                assert!(!app.content_dirty);
                let work = tokio::time::timeout(Duration::from_secs(5), ready.recv())
                    .await
                    .expect("highlight worker did not wake the UI")
                    .expect("highlight notification channel closed");
                assert!(matches!(work, Work::HighlightsReady));
                app.work(work);
                terminal.draw(|frame| draw(frame, &mut app)).unwrap();
            }
            assert!(
                number_is_painted(terminal.backend().buffer(), accent),
                "completed highlights did not reach the frame: light={light}, width={width}, reset={reset}"
            );
            assert_eq!(serde_json::to_vec(&app.snapshot.records).unwrap(), records);
        }
        app.session.as_ref().unwrap().shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn latest_activity_hit_restores_follow_tail_and_disappears() {
        let (_root, mut app) = fixture().await;
        commit_message(&mut app, &"ordinary prose\n".repeat(100));
        for light in [false, true] {
            app.light = light;
            let mut terminal = Terminal::new(TestBackend::new(80, 25)).unwrap();
            app.view().scroll = None;
            terminal.draw(|frame| draw(frame, &mut app)).unwrap();
            assert!(!app.hits.iter().any(|(_, hit)| matches!(hit, Hit::Latest)));

            app.view().scroll = Some(0);
            terminal.draw(|frame| draw(frame, &mut app)).unwrap();
            let rect = app
                .hits
                .iter()
                .find_map(|(rect, hit)| matches!(hit, Hit::Latest).then_some(*rect))
                .expect("scrolled history should offer Latest activity");
            let label: String = (rect.x..rect.right())
                .map(|x| terminal.backend().buffer()[(x, rect.y)].symbol())
                .collect();
            assert_eq!(label, "↓ Latest activity");
            assert_eq!(app.view().scroll, Some(0));

            // Use the rendered hit coordinates and the real event handler. The
            // overlay overlaps selectable text, which must not repin the view.
            for kind in [
                MouseEventKind::Down(MouseButton::Left),
                MouseEventKind::Up(MouseButton::Left),
            ] {
                app.event(Event::Mouse(MouseEvent {
                    kind,
                    column: rect.x,
                    row: rect.y,
                    modifiers: KeyModifiers::NONE,
                }));
            }
            assert!(app.view().scroll.is_none());
            assert!(app.selection.is_none());
            terminal.draw(|frame| draw(frame, &mut app)).unwrap();
            assert!(app.view().scroll.is_none());
            assert!(!app.hits.iter().any(|(_, hit)| matches!(hit, Hit::Latest)));
        }
        app.session.as_ref().unwrap().shutdown().await.unwrap();
    }
}
