//! Main frame composition and viewport orchestration.

use super::*;
use crate::tui::{composer::ComposerLayout, keys::Command};
use skyhook::job::JobState;

/// One input row between the composer's padding rows.
const EMPTY_COMPOSER_HEIGHT: u16 = 3;
/// The composer may grow this tall even where that exceeds half the body.
const MIN_COMPOSER_LIMIT: u16 = 7;

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
    if app
        .snapshot
        .activity
        .values()
        .any(|activity| activity.state.is_retryable())
    {
        hints.push((Command::Retry, "Continue"));
    }
    hints
}

/// Other open sessions waiting on the user.
fn background_attention(app: &App) -> usize {
    let others = app.peers.iter().filter(|peer| !peer.current);
    others.filter(|peer| peer.attention).count()
}

/// The row above the tree and composer, most urgent first, and what clicking it
/// opens; hints and toasts only inform.
fn notice(app: &App, prompt_shown: bool) -> Option<(String, Option<Hit>)> {
    let waiting = background_attention(app);
    if let Some(prefix) = app.leader {
        Some((app.keys.leader_hint(prefix, &leader_hints(app)), None))
    } else if let Some((message, _)) = &app.toast {
        Some((message.clone(), None))
    } else if !app.prompts.is_empty() && !prompt_shown {
        let hint = match app.keys.binding(Command::Attention) {
            Some(binding) => format!("{binding} reopen"),
            None => "Reopen questions and permissions in the command palette".to_owned(),
        };
        let message = format!("{} pending request(s) · {hint}", app.prompts.len());
        Some((message, Some(Hit::Attention)))
    } else if waiting > 0 {
        let hint = app.keys.binding(Command::Sessions);
        let hint = hint.unwrap_or("/sessions".into());
        let message = format!("{waiting} other session(s) need attention · {hint}");
        Some((message, Some(Hit::Sessions)))
    } else if !app.queue.is_empty() {
        let paused = if app.paused { " · paused" } else { "" };
        let message = format!("{} follow-up(s) queued{paused} · /queue", app.queue.len());
        Some((message, Some(Hit::Queue)))
    } else {
        None
    }
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    fill(frame, area, THEME.base);
    app.hits.clear();
    app.animating = false;
    if area.width < 20 || area.height < 9 {
        text(frame, area, "Enlarge terminal", THEME.warning, THEME.base);
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
    let prompt_shown = app.prompt_shown();
    let target = app.input_target();
    let editor_width = width.saturating_sub(4).max(1) as usize;
    app.editor.set_width(editor_width);
    let editor_layout = app.editor.layout(editor_width);
    let viewing_child = !app.selected.path().is_empty();
    let notice = notice(app, prompt_shown);
    let notice_height = u16::from(notice.is_some());
    // The draft grows to at most half the rows between the header and footer.
    let editor_limit = (height.saturating_sub(2 + footer_height + notice_height) / 2)
        .max(MIN_COMPOSER_LIMIT)
        .min(height.saturating_sub(footer_height + 3))
        .max(EMPTY_COMPOSER_HEIGHT);
    let editor_height =
        (editor_layout.rows.len() as u16 + 2 + u16::from(!app.editor.attachments().is_empty()))
            .clamp(EMPTY_COMPOSER_HEIGHT, editor_limit);
    let prompt_layout = prompt_shown.then(|| PromptLayout::new(app, width));
    let composer_height = if let Some(layout) = &prompt_layout {
        (layout.height()).min(height.saturating_sub(footer_height + 2 + notice_height))
    } else if viewing_child {
        0
    } else {
        editor_height
    };
    let tree_agents: Vec<_> = app.projection.visible(&app.selected).collect();
    let composer_y = height.saturating_sub(footer_height + composer_height);
    // A child's tree takes the hidden composer's empty height, whatever the draft's length.
    let tree_capacity = composer_y.saturating_sub(3 + notice_height).min(
        (height / 4).clamp(4, 10)
            + if viewing_child && !prompt_shown {
                EMPTY_COMPOSER_HEIGHT
            } else {
                0
            },
    );
    let show_tree = viewing_child || app.projection.has_active_children();
    let tree_rows = if show_tree && tree_capacity >= 3 {
        (tree_agents.len() as u16).min(tree_capacity - 2)
    } else {
        0
    };
    let tree_height = if tree_rows > 0 { tree_rows + 2 } else { 0 };
    // Focus cannot stay on a hidden tree or a child's absent composer.
    if tree_height == 0 && app.focus == Focus::Tree {
        app.focus = if viewing_child {
            Focus::Content
        } else {
            Focus::Composer
        };
    }
    if matches!(target, InputTarget::None) && app.focus == Focus::Composer {
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
        // The notice row belongs to the conversation; the sidebar runs beside it.
        let rect = r(
            content_width,
            2,
            width - content_width,
            app.content_rect.height + notice_height,
        );
        draw_sidebar(frame, app, rect);
    }
    draw_header(frame, app, width);
    prepare_rows(app, content_width);
    let navigation_active = matches!(target, InputTarget::Composer | InputTarget::None);
    draw_content(frame, app, navigation_active);
    // The popup and any notice share the row above the tree and composer.
    let latest_width = draw_scrollbar(frame, app);
    if let Some(Overlay::Search(search)) = &app.overlay {
        text(
            frame,
            r(2, app.content_rect.y, content_width.saturating_sub(4), 1),
            format!("Find: {}▏", search.text()),
            THEME.fg,
            THEME.input,
        );
    }
    if let Some((message, hit)) = notice {
        let taken = if latest_width > 0 {
            latest_width + 3
        } else {
            2
        };
        let rect = r(
            1,
            tree_y.saturating_sub(1),
            content_width.saturating_sub(taken),
            1,
        );
        if let Some(hit) = hit {
            app.hits.push((rect, hit));
        }
        text(frame, rect, message, THEME.warning, THEME.base);
    }
    draw_tree(frame, app, &tree_agents, navigation_active);
    fill(frame, app.composer_rect, THEME.input);
    if let Some(layout) = &prompt_layout {
        draw_prompt(frame, app, layout);
    } else if !viewing_child {
        draw_composer(frame, app, &editor_layout);
    }
    let footer = r(0, height - footer_height, width, footer_height);
    draw_footer(frame, app, footer, &stats, &stat_lines);
    draw_menu(frame, app);
}

/// The workspace and the tab strip.
fn draw_header(frame: &mut Frame, app: &mut App, width: u16) {
    fill(frame, r(0, 0, width, 2), THEME.panel);
    let workspace = app.launch.workspace.display().to_string();
    let workspace = clipped(Line::from(workspace), width.saturating_sub(4) as usize);
    let workspace_width = workspace.width() as u16;
    text(
        frame,
        r((width - workspace_width) / 2, 0, workspace_width, 1),
        workspace,
        THEME.fg,
        THEME.panel,
    );
    let mut x = 2;
    for tab in [Tab::Conversation, Tab::Requests, Tab::Jobs] {
        let label = match (tab, width < 50) {
            (Tab::Conversation, false) => "Conversation",
            (Tab::Conversation, true) => "Chat",
            (Tab::Requests, false) => "Requests",
            (Tab::Requests, true) => "Calls",
            (Tab::Jobs, _) => "Jobs",
        };
        let n = label.len() as u16 + 2;
        let rect = r(x, 1, n, 1);
        let (fg, bg) = if app.tab == tab {
            (THEME.primary, THEME.selected)
        } else {
            (THEME.muted, THEME.panel)
        };
        text(frame, rect, format!(" {label} "), fg, bg);
        app.hits.push((rect, Hit::Tab(tab)));
        x += n + 1;
    }
}

/// The transcript rows in view, with selection, focus, running spinners and
/// live counters.
fn draw_content(frame: &mut Frame, app: &mut App, navigation_active: bool) {
    let content_width = app.content_rect.width;
    if app.tab == Tab::Requests
        && app.content_rect.height > 1
        && let Some(entry) = app.entries().iter().find(|entry| entry.request().is_some())
    {
        let geometry = EntryGeometry::new(entry, content_width, 0);
        if let Some(header) = app.render.request_columns.header(geometry.body_width) {
            text(
                frame,
                r(geometry.x, app.content_rect.y, geometry.body_width, 1),
                header,
                THEME.muted,
                THEME.base,
            );
            // The fixed header is not an entry: paging, hit testing and selection
            // all use a viewport containing data rows only.
            app.content_rect.y += 1;
            app.content_rect.height -= 1;
        }
    }
    let max = app
        .render
        .rows
        .len()
        .saturating_sub(app.content_rect.height as usize);
    let view = app.view();
    let scroll = view.scroll.map_or(max, |n| n.min(max));
    if view.scroll.is_some() {
        view.scroll = Some(scroll);
    }
    let selected_entry = view.row;
    if app.entries().is_empty() {
        text(
            frame,
            r(3, 4, content_width.saturating_sub(6), 1),
            "Start a conversation, or /sessions to open one.",
            THEME.muted,
            THEME.base,
        );
    }
    // A job card's glyph gives way to the spinner only while it runs; waiting
    // for input keeps its glyph beside the counter.
    let jobs = app.projection.jobs();
    let running = |job| {
        jobs.get(&job)
            .is_none_or(|job| job.state == JobState::Running)
    };
    let mut cursor_drawn = false;
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
        let expandable = entry.is_some_and(model::Entry::expandable);
        let focused = navigation_active
            && app.focus == Focus::Content
            && row.selectable
            && row.entry == selected_entry;
        let hovered =
            navigation_active && app.hover.is_some_and(|point| rect.contains(point.into()));
        let bg = row.background(
            entry.is_some_and(model::Entry::open),
            expandable && (focused || hovered),
            selected.is_some(),
        );
        fill(frame, rect, bg);
        let text_rect = r(row.paragraph_x(), y, row.text_width, 1);
        render_row_line(
            row,
            text_rect,
            frame.buffer_mut(),
            Style::default().fg(foreground(row.surface)).bg(bg),
        );
        if let Some(range) = selected {
            let view = row_text.as_ref().expect("selected range has row text");
            // Source-to-column geometry skips hanging prefixes and code padding.
            // A malformed layout yields no cells; never paint it elsewhere.
            let text_end = text_rect.right() as usize;
            let source_end = row.layout.code().map_or(text_end, |code| {
                (text_rect.x as usize + code.body_end()).min(text_end)
            });
            for (byte, column, width) in view.source_cells() {
                if !range.contains(&byte) {
                    continue;
                }
                let start = text_rect.x as usize + column;
                let end = start + width;
                if start >= source_end {
                    break;
                }
                if end > source_end {
                    continue;
                }
                for x in start..end {
                    frame.buffer_mut()[(x as u16, y)].set_bg(THEME.selected);
                }
            }
        }
        if row.layout.is_spacer() {
            continue;
        }
        // Spinners and live counters are painted over the laid-out header row,
        // into cells its layout reserved, so ticks never rebuild entries.
        if row.layout.header()
            && let Some(entry) = entry.filter(|entry| entry.timing.live())
        {
            app.animating = true;
            if entry.job_id().is_none_or(running) {
                let x = row.x + if expandable { 2 } else { 0 };
                // A card's spinner stands in for its state glyph, in the glyph's accent.
                let color = if entry.header().is_some() {
                    THEME.primary
                } else {
                    THEME.muted
                };
                text(frame, r(x, y, 1, 1), spinner(app.tick_count), color, bg);
            }
            if let Some(counter) = entry.timing.text(app.clock) {
                let width = (counter.width() as u16).min(text_rect.width);
                let x = text_rect.right() - width;
                text(frame, r(x, y, width, 1), counter, THEME.muted, bg);
            }
        }
        if focused && !cursor_drawn {
            focus_cursor(frame, row.x.saturating_sub(1), y, THEME.base);
            cursor_drawn = true;
        }
        if row.selectable {
            let toggles = row.layout.header() || expandable;
            app.hits.push((rect, Hit::Entry(row.entry, toggles)));
        }
    }
}

/// The scrollbar thumb, and a way back to the tail while scrolled. Returns the
/// width the tail link takes from the notice row.
fn draw_scrollbar(frame: &mut Frame, app: &mut App) -> u16 {
    let content = app.content_rect;
    let total = app.render.rows.len();
    if total <= content.height as usize || content.height == 0 {
        return 0;
    }
    let scroll = app.view().scroll;
    let track = r(content.width - 1, content.y, 1, content.height);
    scroll_thumb(frame, track, scroll.unwrap_or(total), total, THEME.base);
    if scroll.is_none() {
        return 0;
    }
    let width = 17.min(content.width.saturating_sub(2));
    let rect = r(
        content.width.saturating_sub(1 + width),
        app.tree_rect.y.saturating_sub(1),
        width,
        1,
    );
    // Text-only overlay: retain every underlying cell's background.
    render_line(
        &Line::from("↓ Latest activity"),
        rect,
        frame.buffer_mut(),
        Style::default().fg(THEME.primary),
    );
    app.hits.push((rect, Hit::Latest));
    width
}

/// The root agent's message editor and its attachments.
fn draw_composer(frame: &mut Frame, app: &mut App, layout: &ComposerLayout) {
    let rect = app.composer_rect;
    let text_width = rect.width.saturating_sub(4);
    let (cursor_line, cursor_column) = layout.cursor;
    let visible = rect.height.saturating_sub(2) as usize;
    let bottom = layout.rows.len().saturating_sub(visible);
    let follow = cursor_line.saturating_sub(visible.saturating_sub(1));
    let top = app.composer_scroll.map_or(follow, |top| top.min(bottom));
    let paste = Style::default()
        .fg(THEME.primary)
        .remove_modifier(Modifier::all());
    let selection = Style::default().bg(THEME.selected);
    for (i, row) in layout.rows.iter().skip(top).take(visible).enumerate() {
        let line = row.line(Style::default(), paste, selection);
        let y = rect.y + 1 + i as u16;
        text(frame, r(2, y, text_width, 1), line, THEME.fg, THEME.input);
    }
    let track = r(rect.width - 1, rect.y + 1, 1, visible as u16);
    scroll_thumb(frame, track, top, layout.rows.len(), THEME.input);
    let attachments = app.editor.attachments();
    if !attachments.is_empty() {
        let names = attachments.iter().map(|attachment| {
            match attachment.file().and_then(|file| file.file_name()) {
                Some(name) => format!("[{}]", name.to_string_lossy()),
                None => match &**attachment {
                    skyhook::media::Attachment::Text { .. } => "[text]".to_owned(),
                    skyhook::media::Attachment::Image { .. } => "[image]".to_owned(),
                },
            }
        });
        let names = names.collect::<Vec<_>>().join(" ");
        let y = rect.bottom() - 1;
        text(
            frame,
            r(2, y, text_width, 1),
            names,
            THEME.muted,
            THEME.input,
        );
        app.hits.push((r(0, y, rect.width, 1), Hit::Attachments));
    }
    let shown = (top..top + visible).contains(&cursor_line);
    if shown && app.focus == Focus::Composer && matches!(app.input_target(), InputTarget::Composer)
    {
        frame.set_cursor_position((
            2 + (cursor_column as u16).min(text_width),
            rect.y + 1 + (cursor_line - top) as u16,
        ));
    }
    app.hits.insert(0, (rect, Hit::Composer));
}

/// The viewed agent's model and mode, the session, and usage statistics.
fn draw_footer(frame: &mut Frame, app: &App, rect: Rect, stats: &str, stat_lines: &[String]) {
    fill(frame, rect, THEME.base);
    let model = footer_model(app);
    let session = app.session().as_ref().map_or_else(
        || "new session".to_owned(),
        |session| session.id().to_string(),
    );
    let (width, y) = (rect.width, rect.y);
    if rect.height > 1 {
        let metadata = footer_metadata(&model, &session, width.saturating_sub(2));
        text(
            frame,
            r(1, y, width.saturating_sub(2), 1),
            metadata,
            THEME.muted,
            THEME.base,
        );
        for (index, line) in stat_lines.iter().enumerate() {
            let rect = r(1, y + 1 + index as u16, width.saturating_sub(2), 1);
            text(frame, rect, line.clone(), THEME.fg, THEME.base);
        }
    } else {
        let stat_width = stats.width() as u16;
        let model_width = width.saturating_sub(stat_width + 4);
        if model_width > 0 {
            let metadata = footer_metadata(&model, &session, model_width);
            text(
                frame,
                r(1, y, model_width, 1),
                metadata,
                THEME.muted,
                THEME.base,
            );
        }
        let x = width.saturating_sub(stat_width + 1);
        let rect = r(x, y, stat_width.min(width), 1);
        text(frame, rect, stats.to_owned(), THEME.fg, THEME.base);
    }
}

/// The viewed agent's model. The root composer's next message goes out in the
/// chosen mode; a child's mode is fixed.
fn footer_model(app: &App) -> String {
    let root = app.selected.path().is_empty();
    let model = if root {
        Some(&app.model)
    } else {
        let agent = app.projection.agents.iter().find(|a| a.id == app.selected);
        agent.map_or(Some(&app.model), |a| a.model.as_ref())
    };
    let config = app.launch.model.config();
    let model = model.map_or_else(
        || "-".to_owned(),
        |name| {
            config
                .model(name)
                .map_or_else(|| name.to_string(), |profile| profile.model.to_string())
        },
    );
    if root {
        format!("{} · {model}", app.mode)
    } else {
        model
    }
}

/// Keep both model and session identifiable when the footer is compact.
fn footer_metadata(model: &str, session: &str, width: u16) -> Line<'static> {
    let clip = |text: &str, width: u16| clipped(Line::from(text.to_owned()), width as usize);
    if width < 5 {
        return clip(model, width);
    }
    let available = width - 3; // Separator between model and session.
    let session_width = session
        .width()
        .min((available - (available / 2).min(16)) as usize);
    let model_width = model.width().min(available as usize - session_width) as u16;
    let session_width = available - model_width;
    let mut line = clip(model, model_width);
    line.spans.push(Span::raw(" · "));
    line.spans.extend(clip(session, session_width).spans);
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::{
        app::{
            Work,
            tests::{
                fetch_output, fixture, job_named, launch_context, push_record, requested,
                run_script,
            },
        },
        keys::Command,
        theme::THEME,
    };
    use crossterm::event::{Event, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::{Terminal, backend::TestBackend};
    use skyhook::provider::protocol::AssistantItem;
    use skyhook::session::{Message, SessionEvent};
    use std::time::Duration;

    async fn commit_message(app: &mut App, text: &str) {
        let message = Message::Assistant(vec![AssistantItem::text("frame-test", 0, text)]);
        push_record(app, SessionEvent::MessageCommitted { message }).await;
        app.refresh();
    }

    /// A live job card paints its counter at its header row's right edge, so the
    /// clock moves it without changing the card. A running card spins in place of
    /// its glyph; one waiting for input keeps its glyph.
    #[tokio::test]
    async fn live_job_cards_paint_counters_and_only_running_ones_spin() {
        use skyhook::{identity::JobId, job::JobTransition};
        let (_root, mut app) = fixture().await;
        let job = JobId::new(1).unwrap();
        let created = SessionEvent::JobCreated {
            job,
            parent: None,
            origin: None,
            tool: "exec".into(),
            role: skyhook::job::JobRole::Tool,
            name: None,
            arguments: serde_json::json!({"command": ["true"]}),
            output_schema: None,
            accepts_input: true,
            background: false,
            location: skyhook::execution::ExecutionLocation::root("/workspace".into()),
        };
        push_record(&mut app, created).await;
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        let mut screen = |app: &mut App| {
            terminal.draw(|frame| draw(frame, app)).unwrap();
            let buffer = terminal.backend().buffer();
            let row = |y| {
                (0..60)
                    .map(|x| buffer[(x, y)].symbol().to_owned())
                    .collect()
            };
            (0..12).map(row).collect::<Vec<Vec<_>>>()
        };
        for (state, header) in [
            (
                JobTransition::Running,
                format!("{} exec true · Running · #1", spinner(0)),
            ),
            (
                JobTransition::WaitingInput,
                "? exec true · Waiting for input · #1".into(),
            ),
        ] {
            push_record(&mut app, SessionEvent::JobStateChanged { job, state }).await;
            app.refresh();
            app.tick_count = 0;
            let model::Timing::Since(since) = app.projection.jobs()[&job].timing else {
                panic!("a live job counts from when it started");
            };
            app.clock = model::Clock::at(since + 12_400);
            let before = screen(&mut app);
            let y = (before.iter())
                .position(|line| line.concat().contains(&header))
                .unwrap();
            let row = app.render.rows.get(0).unwrap();
            let right = usize::from(row.paragraph_x() + row.text_width);
            assert_eq!(before[y][right - 5..right].concat(), "12.4s");
            let entries = app.content_cache.entries().to_vec();
            app.clock = model::Clock::at(since + 75_000);
            app.refresh();
            let after = screen(&mut app);
            assert!(app.content_cache.entries() == entries);
            assert_eq!(after[y][right - 6..right].concat(), "1m 15s");
            for (line, (old, new)) in before.iter().zip(&after).enumerate() {
                let changed = (old.iter().zip(new).enumerate()).filter(|(_, (a, b))| a != b);
                for (x, _) in changed {
                    assert!(
                        line == y && (right - 6..right).contains(&x),
                        "({x}, {line})"
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn sidebar_narrows_only_the_transcript_and_persists_its_toggle() {
        use skyhook::agent::{TodoItem, TodoStatus};
        let (root, mut app) = fixture().await;
        let text = "right-aligned user text ".repeat(12);
        let message = Message::User(vec![skyhook::session::UserPart::Text { text }]);
        push_record(&mut app, SessionEvent::MessageCommitted { message }).await;
        let items = [
            ("parse", TodoStatus::Completed),
            (
                "render the wrapped todo text wholeword",
                TodoStatus::InProgress,
            ),
        ];
        let items = items.map(|(text, status)| TodoItem {
            text: text.parse().unwrap(),
            status,
        });
        push_record(
            &mut app,
            SessionEvent::TodosReplaced {
                items: items.into(),
            },
        )
        .await;
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
        // A notice spans the conversation only; the sidebar runs on beside it.
        app.toast = Some(("notice ".repeat(30), std::time::Instant::now()));
        let noticed = screen(&mut app, 120);
        let rows: Vec<_> = noticed.lines().collect();
        let notice = rows.iter().position(|row| row.contains("notice notice"));
        let beside: String = rows[notice.unwrap()].chars().skip(88).collect();
        assert!(!beside.contains("notice") && noticed.contains("· Use MCP tools"));
        app.toast = None;
        // A pending mode shows what the next message will be granted.
        app.mode = "missing".parse().unwrap();
        assert!(!screen(&mut app, 120).contains("Capabilities"));
        app.mode = "general".parse().unwrap();
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
        let saved = || crate::state::load(root.path()).0.sidebar;
        // The toggle is saved in the background, with nothing to await.
        crate::tests::bounded(async {
            while saved() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
    }

    #[tokio::test]
    async fn request_headers_stay_outside_the_scrolling_rows() {
        let (_root, mut app) = fixture().await;
        let context = launch_context(&app);
        let context = push_record(&mut app, context).await;
        for _ in 0..30 {
            push_record(&mut app, requested(context, Vec::new())).await;
        }
        app.tab = Tab::Requests;
        app.refresh();
        for (width, header) in [(140, true), (40, false)] {
            let mut terminal = Terminal::new(TestBackend::new(width, 25)).unwrap();
            for scroll in [0, 5] {
                app.view().scroll = Some(scroll);
                terminal.draw(|frame| draw(frame, &mut app)).unwrap();
                let buffer = terminal.backend().buffer();
                let line: String = (0..width).map(|x| buffer[(x, 2)].symbol()).collect();
                assert_eq!(line.contains("Input (uncached)"), header);
                assert_eq!(
                    (app.content_rect.y, app.render.rows.len()),
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

    /// `source` is on screen with its trailing `42;` number painted in `color`.
    fn number_is_painted(buffer: &Buffer, source: &str, color: Color) -> bool {
        let number = source.len() - 3..source.len() - 1;
        buffer
            .content
            .chunks(buffer.area.width as usize)
            .any(|row| {
                row.windows(source.len()).any(|cells| {
                    cells.iter().map(|cell| cell.symbol()).collect::<String>() == source
                        && cells[number.clone()].iter().all(|cell| cell.fg == color)
                })
            })
    }

    #[tokio::test]
    async fn expanded_items_paint_solid_code_backgrounds_across_clipped_rows() {
        let (_root, mut app) = fixture().await;
        std::fs::write(app.launch.workspace.join("body.txt"), "body\n\n".repeat(20)).unwrap();
        run_script(&mut app, "return await tool.read({path:'body.txt'});").await;
        let job = job_named(&app, "read");
        fetch_output(&mut app, job).await;
        app.command(Command::Details);
        for width in [30, 60] {
            let mut terminal = Terminal::new(TestBackend::new(width, 18)).unwrap();
            terminal.draw(|frame| draw(frame, &mut app)).unwrap();
            // Halfway down the card, the viewport is clipped inside the body.
            let scroll = (app.render.rows.len() - app.content_rect.height as usize) / 2;
            assert!(scroll > 0, "exercise a viewport clipped inside the body");
            app.view().scroll = Some(scroll);
            terminal.draw(|frame| draw(frame, &mut app)).unwrap();
            let buffer = terminal.backend().buffer();
            let mut empty_body_rows = 0;
            let rows = app.render.rows.iter().skip(scroll);
            for (offset, row) in rows.take(app.content_rect.height.into()).enumerate() {
                let y = app.content_rect.y + offset as u16;
                let spacer = row.layout.is_spacer();
                let expected = if spacer { THEME.base } else { THEME.code_bg };
                empty_body_rows += usize::from(!spacer && row.text().trim().is_empty());
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
        )
        .await;
        let records = serde_json::to_vec(&app.snapshot.records).unwrap();
        let accent = THEME.accent;
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
                assert!(!number_is_painted(
                    terminal.backend().buffer(),
                    "let answer = 42;",
                    accent
                ));
                assert!(!app.render.reset && app.render.dirty.is_empty());
                assert!(!app.content_dirty);
                let work = crate::tests::bounded(ready.recv()).await.unwrap();
                assert!(matches!(work, Work::HighlightsReady));
                app.work(work);
                terminal.draw(|frame| draw(frame, &mut app)).unwrap();
            }
            assert!(
                number_is_painted(terminal.backend().buffer(), "let answer = 42;", accent),
                "completed highlights did not reach the frame: width={width}, reset={reset}"
            );
            assert_eq!(serde_json::to_vec(&app.snapshot.records).unwrap(), records);
        }
    }

    #[tokio::test]
    async fn highlighting_follows_the_viewport_through_a_transcript_beyond_the_cache() {
        let (_root, mut app) = fixture().await;
        let (notify, mut ready) = tokio::sync::mpsc::unbounded_channel();
        app.render = RenderState::new(app.selected.clone(), notify);
        // More distinct fenced sections than the highlight cache admits at once.
        for index in 0..150 {
            let name = match index {
                0 => "first".to_owned(),
                149 => "last".to_owned(),
                _ => format!("other{index}"),
            };
            let text = format!("```rust\nlet {name} = 42;\n```");
            let message = Message::Assistant(vec![AssistantItem::text("m", 0, text)]);
            push_record(&mut app, SessionEvent::MessageCommitted { message }).await;
        }
        app.refresh();
        let accent = THEME.accent;
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        for (scroll, source) in [(None, "let last = 42;"), (Some(0), "let first = 42;")] {
            app.view().scroll = scroll;
            crate::tests::bounded(async {
                loop {
                    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
                    if number_is_painted(terminal.backend().buffer(), source, accent) {
                        break;
                    }
                    app.work(ready.recv().await.expect("highlight worker is running"));
                }
            })
            .await;
        }
    }

    #[tokio::test]
    async fn composer_grows_to_half_the_body_without_shaping_a_childs_tree() {
        let (_root, mut app) = fixture().await;
        let mut children = Vec::new();
        for index in 1..=20 {
            children.push(crate::tui::app::tests::push_child(&mut app, index).await);
        }
        app.editor.set("line\n".repeat(40));
        let mut terminal = Terminal::new(TestBackend::new(60, 40)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let body = app.content_rect.height + app.tree_rect.height + app.composer_rect.height;
        assert!(app.composer_rect.height > MIN_COMPOSER_LIMIT);
        assert!(app.composer_rect.height <= body / 2);

        app.selected = children[0].clone();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let tree = app.tree_rect;
        app.editor.clear();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert_eq!(app.tree_rect, tree);
    }

    #[tokio::test]
    async fn latest_activity_hit_restores_follow_tail_and_disappears() {
        let (_root, mut app) = fixture().await;
        commit_message(&mut app, &"ordinary prose\n".repeat(100)).await;
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
        // A notice shares that row without moving the popup, and gives way to it.
        app.toast = Some(("notice ".repeat(30), std::time::Instant::now()));
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert_eq!(latest(&app), Some(rect));
        let buffer = terminal.backend().buffer();
        let row: String = (0..80).map(|x| buffer[(x, rect.y)].symbol()).collect();
        assert!(row.starts_with(" notice notice") && row.contains("noti ↓ Latest activity"));
        app.toast = None;
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

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
