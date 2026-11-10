//! Main frame composition and viewport orchestration.

use super::cache::{near_view, prepare_rows, shown_edges, visible_entries};
use super::columns::TreePlane;
use super::composer::draw_composer;
use super::content::{draw_content, draw_scrollbar};
use super::menu::draw_menu;
use super::page;
use super::painting::{fill, r, text};
use super::prompt::{PromptLayout, draw_prompt};
use super::sidebar::{SIDEBAR_MIN_TERMINAL, Sidebar, draw_sidebar, sidebar_width};
use super::tree::draw_tree;
use crate::tui::app::{App, Hit, InputTarget, Pane};
use crate::tui::commands::{self, Command};
use crate::tui::format;
use crate::tui::format::model_label;
use crate::tui::model::Tab;
use crate::tui::theme::theme;
use crate::tui::wrapping::{clipped, wrap_plain};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

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
    if app.snapshot.continuable() {
        hints.push((Command::Retry, "Continue"));
    }
    hints.retain(|(command, _)| app.command_available(*command));
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
        Some((commands::leader_hint(prefix, &leader_hints(app)), None))
    } else if let Some((message, _)) = &app.toast {
        Some((message.clone(), None))
    } else if !app.prompts.is_empty() && !prompt_shown {
        let hint = match Command::Attention.binding() {
            Some(binding) => format!("{binding} reopen"),
            None => "Reopen questions and permissions in the command palette".to_owned(),
        };
        let message = format!("{} pending request(s) · {hint}", app.prompts.len());
        Some((message, Some(Hit::Attention)))
    } else if waiting > 0 {
        let hint = Command::Sessions.binding();
        let hint = hint.unwrap_or("/sessions".into());
        let message = format!("{waiting} other session(s) need attention · {hint}");
        Some((message, Some(Hit::Sessions)))
    } else if !app.queue.is_empty() {
        let paused = if app.paused.is_some() {
            " · paused"
        } else {
            ""
        };
        let message = format!("{} follow-up(s) queued{paused} · /queue", app.queue.len());
        Some((message, Some(Hit::Queue)))
    } else {
        None
    }
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    fill(frame, area, theme().ui.base);
    app.render.hits.clear();
    app.render.animating = false;
    if area.width < 20 || area.height < 9 {
        text(
            frame,
            area,
            "Enlarge terminal",
            theme().ui.warning,
            theme().ui.base,
        );
        return;
    }
    let width = area.width;
    let height = area.height;
    let context = app
        .snapshot
        .views
        .context
        .get(&app.selected)
        .map(|c| (c.tokens, c.capacity));
    let mut usage = skyhook::provider::protocol::UsageTotals::default();
    app.snapshot
        .usage
        .values()
        .for_each(|agent| usage.merge(*agent));
    let stats = format::footer(usage, context);
    let stat_lines = wrap_plain(&stats, width.saturating_sub(2) as usize);
    let footer_height = if width < 70 {
        1 + stat_lines.len() as u16
    } else {
        1
    };
    let prompt_shown = !app.is_view_only() && app.prompt_shown();
    let target = app.input_target();
    let editor_width = width.saturating_sub(4).max(1) as usize;
    app.editor.set_width(editor_width);
    let editor_layout = app.editor.layout(editor_width);
    let viewing_child = !app.selected.path().is_empty();
    // A page view keeps only the tab strip, the footer, what asks for an answer and the
    // live agents.
    let page_frame = app.page_frame();
    let notice = notice(app, prompt_shown);
    let notice_height = u16::from(notice.is_some());
    // The draft grows to at most half the rows between the tab strip and footer.
    let editor_limit = (height.saturating_sub(1 + footer_height + notice_height) / 2)
        .max(MIN_COMPOSER_LIMIT)
        .min(height.saturating_sub(footer_height + 3))
        .max(EMPTY_COMPOSER_HEIGHT);
    let editor_height =
        (editor_layout.rows.len() as u16 + 2 + u16::from(!app.editor.attachments().is_empty()))
            .clamp(EMPTY_COMPOSER_HEIGHT, editor_limit);
    let prompt_layout = prompt_shown.then(|| PromptLayout::new(app, width));
    let composer_height = if let Some(layout) = &prompt_layout {
        (layout.height()).min(height.saturating_sub(footer_height + 1 + notice_height))
    } else if !app.composer_shown() {
        0
    } else {
        editor_height
    };
    let tree_agents = app.tree_agents();
    let composer_y = height.saturating_sub(footer_height + composer_height);
    // A child's tree takes the hidden composer's empty height, whatever the draft's length.
    let tree_capacity = composer_y.saturating_sub(2 + notice_height).min(
        (height / 4).clamp(4, 10)
            + if viewing_child && !prompt_shown {
                EMPTY_COMPOSER_HEIGHT
            } else {
                0
            },
    );
    // Under a conversation the tree shows while children run; beside a page, which views no
    // agent, it holds its place for as long as the session works, so the page keeps its
    // width as children come and go.
    let show_tree = match app.tree_viewed() {
        Some(_) => viewing_child || app.projection.has_active_children(),
        None => app.busy() || tree_agents.len() > 1,
    };
    // A wide page keeps its agents in the sidebar's column, running the page's height.
    let plane = if page_frame && width >= SIDEBAR_MIN_TERMINAL {
        TreePlane::Column
    } else {
        TreePlane::Band
    };
    let tree_rows = if show_tree && plane == TreePlane::Band && tree_capacity >= 3 {
        (tree_agents.len() as u16).min(tree_capacity - 2)
    } else {
        0
    };
    let tree_height = if tree_rows > 0 { tree_rows + 2 } else { 0 };
    let tree_y = composer_y.saturating_sub(tree_height);
    let column_width = if show_tree && plane == TreePlane::Column {
        sidebar_width(width)
    } else {
        0
    };
    app.render.composer_rect = r(0, composer_y, width, composer_height);
    app.render.tree_rect = match plane {
        TreePlane::Band => r(0, tree_y, width, tree_height),
        TreePlane::Column => r(
            width - column_width,
            1,
            column_width,
            composer_y.saturating_sub(1),
        ),
    };
    // Focus cannot stay on a hidden tree or a child's absent composer.
    if app.render.tree_rect.is_empty() && app.focus == Pane::Tree {
        app.focus = if app.composer_shown() {
            Pane::Composer
        } else {
            Pane::Content
        };
    }
    let composer_off = app.input_target() == InputTarget::Pane(Pane::Content);
    if (composer_off || page_frame) && app.focus == Pane::Composer {
        app.focus = Pane::Content;
    }
    let sidebar = if page_frame {
        None
    } else {
        Sidebar::of(app, width)
    };
    let content_width = width - column_width - sidebar.as_ref().map_or(0, Sidebar::width);
    app.render.content_rect = r(
        0,
        1,
        content_width,
        tree_y.saturating_sub(1 + notice_height),
    );
    if let Some(sidebar) = sidebar {
        // The notice row belongs to the conversation; the sidebar runs beside it.
        let rect = r(
            content_width,
            1,
            width - content_width,
            app.render.content_rect.height + notice_height,
        );
        draw_sidebar(frame, app, sidebar, rect);
    }
    draw_header(frame, app, width);
    let navigation_active = matches!(target, InputTarget::Pane(_));
    let latest_width = match app.panel {
        super::super::app::Shown::Transcript => {
            app.read_presented_responses();
            let prompt_fences = prompt_layout.as_ref().map(|layout| &layout.fences);
            prepare_rows(app, content_width, prompt_fences);
            app.reveal_scope();
            app.load_shown_output(visible_entries(app));
            let near = near_view(app);
            app.load_near(&near, shown_edges(app));
            draw_content(frame, app, navigation_active);
            draw_scrollbar(frame, app)
        }
        super::super::app::Shown::Extension(_) => {
            app.refresh_extension();
            page::draw_extension(frame, app, navigation_active);
            0
        }
    };
    // The popup and any notice share the row above the tree and composer.
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
            app.render.hits.push((rect, hit));
        }
        text(frame, rect, message, theme().ui.warning, theme().ui.base);
    }
    draw_tree(frame, app, &tree_agents, plane, navigation_active);
    fill(frame, app.render.composer_rect, theme().ui.input);
    if let Some(layout) = &prompt_layout {
        draw_prompt(frame, app, layout);
    } else if app.composer_shown() {
        draw_composer(frame, app, &editor_layout);
    }
    let footer = r(0, height - footer_height, width, footer_height);
    draw_footer(frame, app, footer, &stats, &stat_lines);
    draw_menu(frame, app);
}

/// The tab strip.
fn draw_header(frame: &mut Frame, app: &mut App, width: u16) {
    fill(frame, r(0, 0, width, 1), theme().ui.panel);
    let mut tabs: Vec<_> = [Tab::Conversation, Tab::Requests, Tab::Jobs]
        .into_iter()
        .map(|tab| {
            let label = match (tab, width < 50) {
                (Tab::Conversation, false) => "Conversation",
                (Tab::Conversation, true) => "Chat",
                (Tab::Requests, false) => "Requests",
                (Tab::Requests, true) => "Calls",
                (Tab::Jobs, _) => "Jobs",
            };
            let selected = app.panel == super::super::app::Shown::Transcript && app.tab == tab;
            (label.to_owned(), Hit::Tab(tab), selected)
        })
        .collect();
    tabs.extend(
        app.extension_views()
            .iter()
            .enumerate()
            .map(|(index, view)| {
                let selected = app.panel == super::super::app::Shown::Extension(index);
                ((*view).to_owned(), Hit::ExtensionTab(index), selected)
            }),
    );
    let total: usize = tabs.iter().map(|(label, ..)| label.width() + 3).sum();
    let available = width.saturating_sub(4) as usize;
    let limit = if total > available {
        available / tabs.len()
    } else {
        available
    };
    let mut x = 2;
    for (label, hit, selected) in tabs {
        let n = (label.width() + 2).min(limit.saturating_sub(1)) as u16;
        if n == 0 {
            continue;
        }
        let rect = r(x, 0, n, 1);
        let (fg, bg) = if selected {
            (theme().ui.emphasis, theme().ui.selected)
        } else {
            (theme().ui.muted, theme().ui.panel)
        };
        text(
            frame,
            rect,
            clipped(Line::from(format!(" {label} ")), n as usize),
            fg,
            bg,
        );
        app.render.hits.push((rect, hit));
        x += n + 1;
    }
}

/// The viewed agent's model and mode, the workspace, the session, and usage statistics.
fn draw_footer(frame: &mut Frame, app: &App, rect: Rect, stats: &str, stat_lines: &[String]) {
    fill(frame, rect, theme().ui.base);
    let model = footer_model(app);
    let access = if app.is_view_only() {
        "view-only · "
    } else {
        ""
    };
    let session = app.session().as_ref().map_or_else(
        || "new session".to_owned(),
        |session| session.id().to_string(),
    );
    let workspace = format!("{access}{}", app.launch.workspace().display());
    let (width, y) = (rect.width, rect.y);
    if rect.height > 1 {
        let metadata = footer_metadata(&model, &workspace, &session, width.saturating_sub(2));
        text(
            frame,
            r(1, y, width.saturating_sub(2), 1),
            metadata,
            theme().ui.muted,
            theme().ui.base,
        );
        for (index, line) in stat_lines.iter().enumerate() {
            let rect = r(1, y + 1 + index as u16, width.saturating_sub(2), 1);
            text(frame, rect, line.clone(), theme().ui.fg, theme().ui.base);
        }
    } else {
        let stat_width = stats.width() as u16;
        let model_width = width.saturating_sub(stat_width + 4);
        if model_width > 0 {
            let metadata = footer_metadata(&model, &workspace, &session, model_width);
            text(
                frame,
                r(1, y, model_width, 1),
                metadata,
                theme().ui.muted,
                theme().ui.base,
            );
        }
        let x = width.saturating_sub(stat_width + 1);
        let rect = r(x, y, stat_width.min(width), 1);
        text(
            frame,
            rect,
            stats.to_owned(),
            theme().ui.fg,
            theme().ui.base,
        );
    }
}

/// The viewed agent's model. The root composer's next message goes out in the
/// chosen mode; a child's mode is fixed.
fn footer_model(app: &App) -> String {
    let root = app.selected.path().is_empty();
    let pending = root && !app.is_view_only();
    let agent = app.projection.agents.iter().find(|a| a.id == app.selected);
    let model = if pending {
        &app.model
    } else {
        agent.map_or(&app.model, |a| &a.outline.model)
    };
    let profile = app.launch.model().config().model(model);
    let model = model_label(model, profile.map(|profile| &profile.model));
    let mode = if pending {
        Some(&app.mode)
    } else {
        agent.and_then(|agent| agent.outline.mode.as_ref())
    };
    if root && let Some(mode) = mode {
        format!("{mode} · {model}")
    } else {
        model
    }
}

/// Keep both model and session identifiable when the footer is compact; the
/// workspace between them takes only what is left.
fn footer_metadata(model: &str, workspace: &str, session: &str, width: u16) -> Line<'static> {
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
    let spare = (available as usize).saturating_sub(model.width() + session.width());
    if spare > 3 + 4 {
        line.spans.push(Span::raw(" · "));
        line.spans.extend(clip(workspace, (spare - 3) as u16).spans);
        let session_width = session.width() as u16;
        line.spans.push(Span::raw(" · "));
        line.spans.extend(clip(session, session_width).spans);
        return line;
    }
    line.spans.push(Span::raw(" · "));
    line.spans.extend(clip(session, session_width).spans);
    line
}

#[cfg(test)]
mod tests {
    use super::super::painting::spinner;
    use super::*;
    use crate::tui::render::cache::RenderState;
    use crate::tui::{
        app::{
            Work,
            played::{
                Model, activity, answered, ask, asking_child, call, committed, exchange,
                failing_call, observe_find, observe_record, observe_until, played_fixture,
                recorded, sleeping_script, spawn_children, user,
            },
            tests::{
                capture_work, draw_sized, draw_sized_buffer, fetch_output, fixture, job_named,
                prefs_saved, run_script, settled,
            },
        },
        commands::Command,
        theme::theme,
    };
    use crossterm::event::{Event, KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::buffer::Buffer;
    use ratatui::style::Color;
    use serde_json::json;
    use skyhook::interaction::{Prompt, PromptKind};
    use skyhook::provider::protocol::AssistantItem;
    use skyhook::session::AgentActivity;
    use skyhook::session::SessionEvent;

    async fn reply(app: &mut App, model: &mut Model, text: &str) {
        let answer = vec![AssistantItem::text("frame-test", 0, text)];
        exchange(app, model, "question", answer).await;
    }

    /// A live job card paints its counter at its header row's right edge. A running
    /// card spins in place of its glyph; one waiting for input keeps its glyph.
    #[tokio::test]
    async fn live_job_cards_paint_counters_and_only_running_ones_spin() {
        use skyhook::job::JobState;
        let (_root, mut app, mut model, session) = played_fixture().await;
        let screen = |app: &mut App| {
            let buffer = draw_sized_buffer(app, 80, 40);
            let row = |y| {
                (0..80)
                    .map(|x| buffer[(x, y)].symbol().to_owned())
                    .collect()
            };
            (0..40).map(row).collect::<Vec<Vec<_>>>()
        };
        let script = sleeping_script(&session);
        for (state, name, glyph) in [
            (JobState::Running, "Running", spinner(0)),
            (JobState::WaitingInput, "Waiting for input", "?"),
        ] {
            if state == JobState::WaitingInput {
                asking_child(&session, &mut model, json!({"prompt": "ask"})).await;
            }
            let live = |app: &App| {
                let mut jobs = app.snapshot.jobs.values();
                jobs.find(|job| job.state == state).map(|job| job.id)
            };
            let idle = |app: &App| {
                let root = activity(&app.snapshot, app.root_agent());
                !root.is_some_and(AgentActivity::is_busy)
            };
            let label = |job| format!(" · {name} · #{job}");
            let shown = |app: &App, job| {
                let mut entries = app.entries().iter();
                entries.any(|entry| entry.text().contains(&label(job)))
            };
            let job = observe_find(&mut app, |app| {
                live(app).filter(|&job| idle(app) && shown(app, job))
            })
            .await;
            app.tick_count = 0;
            let format::Timing::Since(since) = app.snapshot.jobs[&job].timing.into() else {
                panic!("a live job counts from when it started");
            };
            app.clock = format::Clock::at(since + 12_400);
            let before = screen(&mut app);
            let label = label(job);
            let y = (before.iter())
                .position(|line| line.concat().contains(&label))
                .unwrap();
            assert!(before[y].concat().contains(&format!("{glyph} ")));
            let row = (app.render.rows.iter())
                .find(|row| row.text().contains(&label))
                .unwrap();
            let right = usize::from(row.paragraph_x() + row.text_width);
            assert_eq!(before[y][right - 5..right].concat(), "12.4s");
            app.clock = format::Clock::at(since + 75_000);
            app.refresh();
            let after = screen(&mut app);
            assert_eq!(after[y][right - 6..right].concat(), "1m 15s");
            session.cancel_job(job).await.unwrap();
            observe_until(&mut app, |app| app.snapshot.jobs[&job].state.is_terminal()).await;
        }
        let _ = crate::tests::bounded(script).await.unwrap();
    }

    /// A live title's spinner takes only the gap its narrow row kept: when
    /// the row keeps just the disclosure glyph, the title's text stays.
    #[tokio::test]
    async fn spinners_never_cover_narrow_titles() {
        use skyhook::provider::protocol::ItemKind;
        let (_root, mut app, mut model, _session) = played_fixture().await;
        let asked = ask(app.session().unwrap(), "question");
        let turn = model.turn().await;
        let mut thought = String::new();
        for (width, more, shown) in [
            (22, "First\nSecond", "▾ R"),
            (23, "\n\n**界 plan**\n\nThird\nFourth", "▾ 界"),
        ] {
            turn.delta("thinking", ItemKind::Reasoning, more);
            thought.push_str(more);
            observe_until(&mut app, |app| {
                let mut responses = app.snapshot.views.responses.values();
                responses.any(|response| response.blocks().iter().any(|b| b.text == thought))
            })
            .await;
            let screen = draw_sized(&mut app, width, 24);
            assert!(screen.lines().any(|row| row.contains(shown)), "{screen}");
        }
        turn.answer("done");
        crate::tests::bounded(asked).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn sidebar_narrows_only_the_transcript_and_persists_its_toggle() {
        let (_root, mut app, mut model, _session) = played_fixture().await;
        let text = "right-aligned user text ".repeat(12);
        let asked = ask(app.session().unwrap(), &text);
        observe_record(&mut app, committed(user)).await;
        let screen = |app: &mut App, width| draw_sized(app, width, 24);
        // Sections with nothing to list are left out, title included.
        let bare = screen(&mut app, 120);
        assert!(bare.contains("Capabilities") && bare.contains("· Read files"));
        assert!(!bare.contains("MCP servers") && !bare.contains("Todos"));
        let items = json!([
            {"text": "parse", "status": "completed"},
            {"text": "render the wrapped todo text wholeword", "status": "in_progress"},
        ]);
        let todo = call("todo", "todo", json!({"items": items}), 0);
        model.turn().await.end(vec![todo]);
        model.turn().await.answer("done");
        crate::tests::bounded(asked).await.unwrap().unwrap();
        let todos = |event: &SessionEvent| matches!(event, SessionEvent::TodosReplaced { .. });
        observe_record(&mut app, todos).await;
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
        // Capabilities are pinned to the bottom: a gap separates them from the todos.
        let rows: Vec<_> = wide.lines().collect();
        let row = |text: &str| rows.iter().position(|row| row.contains(text)).unwrap();
        assert!(row("Capabilities") > row("text wholeword") + 2);
        // One padding row above the first section and below the last.
        let column = |y: usize| rows[y].chars().skip(88).collect::<String>();
        let bottom = app.render.content_rect.bottom() as usize - 1;
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
        assert_eq!(app.render.content_rect.width, 88);
        assert!(
            app.render
                .hits
                .iter()
                .all(|(rect, hit)| !matches!(hit, Hit::Entry { .. }) || rect.right() <= 88)
        );
        screen(&mut app, 160);
        assert_eq!(app.render.content_rect.width, 120);
        // A cramped terminal hides the column without changing the setting.
        assert!(!screen(&mut app, 99).contains("Todos"));
        assert_eq!(
            (
                app.render.content_rect.width,
                app.prefs.saved.borrow().sidebar
            ),
            (99, true)
        );
        let mut rx = capture_work(&mut app);
        app.command(Command::Sidebar);
        assert!(!screen(&mut app, 120).contains("Todos"));
        assert_eq!(app.render.content_rect.width, 120);
        assert!(!prefs_saved(&mut app, &mut rx).await.sidebar);
    }

    #[tokio::test]
    async fn request_headers_stay_outside_the_scrolling_rows() {
        let (_root, mut app, mut model, session) = played_fixture().await;
        let _asked = ask(&session, "question");
        for index in 0..8 {
            let failing = failing_call(&format!("call-{index}"));
            model.turn().await.end(vec![failing]);
        }
        model.turn().await.answer("done");
        app.tab = Tab::Requests;
        observe_until(&mut app, |app| recorded(&app.journal, answered)).await;
        settled(&mut app).await;
        for (width, header) in [(140, true), (40, false)] {
            for scroll in [0, 5] {
                app.scroll_to(Some(scroll));
                let buffer = draw_sized_buffer(&mut app, width, 10);
                let line: String = (0..width).map(|x| buffer[(x, 1)].symbol()).collect();
                assert_eq!(line.contains("Input (uncached)"), header);
                assert_eq!(
                    (app.render.content_rect.y, app.render.rows.len()),
                    (1 + u16::from(header), 9)
                );
                let first = app.render.hits.iter().find_map(|(rect, hit)| match hit {
                    Hit::Entry { index, .. } => Some((rect.y, *index)),
                    _ => None,
                });
                assert_eq!(first, Some((app.render.content_rect.y, scroll)));
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
        std::fs::write(
            app.launch.workspace().join("body.txt"),
            "body\n\n".repeat(20),
        )
        .unwrap();
        run_script(&mut app, "return await tool.read({path:'body.txt'});").await;
        let job = job_named(&app, "read");
        fetch_output(&mut app, job).await;
        app.command(Command::Details);
        for width in [30, 60] {
            draw_sized_buffer(&mut app, width, 18);
            // Halfway down the card, the viewport is clipped inside the body.
            let scroll = (app.render.rows.len() - app.render.content_rect.height as usize) / 2;
            assert!(scroll > 0, "exercise a viewport clipped inside the body");
            app.scroll_to(Some(scroll));
            let buffer = draw_sized_buffer(&mut app, width, 18);
            let mut empty_body_rows = 0;
            let rows = app.render.rows.iter().skip(scroll);
            for (offset, row) in rows.take(app.render.content_rect.height.into()).enumerate() {
                let y = app.render.content_rect.y + offset as u16;
                let spacer = row.layout.is_spacer();
                let expected = if spacer {
                    theme().ui.base
                } else {
                    theme().ui.code_bg
                };
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
        let (_root, mut app, mut model, _session) = played_fixture().await;
        let (notify, mut ready) = tokio::sync::mpsc::unbounded_channel();
        app.render = RenderState::new(app.selected.clone(), notify);
        reply(
            &mut app,
            &mut model,
            "# Example\n\n```rust\nlet answer = 42;\n```\n\n**Done**",
        )
        .await;
        let accent = theme().ui.accent;
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
            let mut buffer = draw_sized_buffer(&mut app, width, 30);
            if completion {
                // The first frame paints fallback text and schedules the worker.
                // Its retained rows repaint when the worker wakes the UI.
                assert!(!number_is_painted(&buffer, "let answer = 42;", accent));
                let work = crate::tests::bounded(ready.recv()).await.unwrap();
                assert!(matches!(work, Work::HighlightsReady));
                app.work(work);
                buffer = draw_sized_buffer(&mut app, width, 30);
            }
            assert!(
                number_is_painted(&buffer, "let answer = 42;", accent),
                "completed highlights did not reach the frame: width={width}, reset={reset}"
            );
        }
    }

    /// A prompt paints a completed highlight on the frame its completion wakes,
    /// and highlights the same code anew under another fence language.
    #[tokio::test]
    async fn prompt_code_highlights_on_completion_and_per_language() {
        let (_root, mut app) = fixture().await;
        let (notify, mut ready) = tokio::sync::mpsc::unbounded_channel();
        app.render = RenderState::new(app.selected.clone(), notify);
        let source = "let answer = 42;";
        let question = |header: &str, language: &str| {
            let prompt = format!("```{language}\n{source}\n```");
            crate::tui::app::tests::titled(header, &prompt, vec![])
        };
        let (reply, _answer) = tokio::sync::oneshot::channel();
        let questions = vec![question("first", "rust"), question("second", "c")];
        let kind = PromptKind::Questions {
            questions,
            agent: app.root_agent().clone(),
            background: false,
            reply,
        };
        app.prompt(Prompt { id: 1, kind });
        for question in 0..2 {
            if question > 0 {
                app.event(Event::Key(KeyCode::Right.into()));
            }
            assert_eq!(app.page_index(), question);
            let buffer = draw_sized_buffer(&mut app, 60, 30);
            assert!(!number_is_painted(&buffer, source, theme().ui.accent));
            app.work(crate::tests::bounded(ready.recv()).await.unwrap());
            let buffer = draw_sized_buffer(&mut app, 60, 30);
            assert!(
                number_is_painted(&buffer, source, theme().ui.accent),
                "{question}"
            );
        }
    }

    #[tokio::test]
    async fn highlighting_follows_the_viewport_through_a_transcript_beyond_the_cache() {
        let (_root, mut app, mut model, _session) = played_fixture().await;
        let (notify, mut ready) = tokio::sync::mpsc::unbounded_channel();
        app.render = RenderState::new(app.selected.clone(), notify);
        // More distinct fenced sections than the highlight cache admits at once.
        let sections = (0..150).map(|index| {
            let name = match index {
                0 => "first".to_owned(),
                149 => "last".to_owned(),
                _ => format!("other{index}"),
            };
            let text = format!("```rust\nlet {name} = 42;\n```");
            AssistantItem::text(format!("m{index}"), index, text)
        });
        exchange(&mut app, &mut model, "question", sections.collect()).await;
        let accent = theme().ui.accent;
        for (scroll, source) in [(None, "let last = 42;"), (Some(0), "let first = 42;")] {
            app.scroll_to(scroll);
            crate::tests::bounded(async {
                loop {
                    let buffer = draw_sized_buffer(&mut app, 60, 20);
                    if number_is_painted(&buffer, source, accent) {
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
        let (_root, mut app, mut model, _session) = played_fixture().await;
        let (children, _turns) = spawn_children(&mut app, &mut model, 12).await;
        app.editor.set("line\n".repeat(40));
        draw_sized_buffer(&mut app, 60, 40);
        let body = app.render.content_rect.height
            + app.render.tree_rect.height
            + app.render.composer_rect.height;
        assert!(app.render.composer_rect.height > MIN_COMPOSER_LIMIT);
        assert!(app.render.composer_rect.height <= body / 2);

        app.selected = children[0].clone();
        draw_sized_buffer(&mut app, 60, 40);
        let tree = app.render.tree_rect;
        // More agents than rows: the layout, not the tree, sets its height.
        assert!(tree.height - 2 < children.len() as u16 + 1);
        app.editor.clear();
        draw_sized_buffer(&mut app, 60, 40);
        assert_eq!(app.render.tree_rect, tree);
    }

    #[tokio::test]
    async fn latest_activity_hit_restores_follow_tail_and_disappears() {
        let (_root, mut app, mut model, _session) = played_fixture().await;
        reply(&mut app, &mut model, &"ordinary prose\n".repeat(100)).await;
        let latest = |app: &App| {
            let mut hits = app.render.hits.iter();
            hits.find_map(|(rect, hit)| matches!(hit, Hit::Latest).then_some(*rect))
        };
        app.scroll_to(None);
        draw_sized_buffer(&mut app, 80, 25);
        assert!(latest(&app).is_none());

        app.scroll_to(Some(0));
        let buffer = draw_sized_buffer(&mut app, 80, 25);
        let rect = latest(&app).expect("scrolled history should offer Latest activity");
        let buffer = &buffer;
        let label: String = (rect.x..rect.right())
            .map(|x| buffer[(x, rect.y)].symbol())
            .collect();
        assert_eq!(label, "↓ Latest activity");
        assert_eq!(app.render.scroll(), Some(0));
        // A notice shares that row without moving the popup, and gives way to it.
        app.toast = Some(("notice ".repeat(30), std::time::Instant::now()));
        let buffer = draw_sized_buffer(&mut app, 80, 25);
        assert_eq!(latest(&app), Some(rect));
        let buffer = &buffer;
        let row: String = (0..80).map(|x| buffer[(x, rect.y)].symbol()).collect();
        assert!(row.starts_with(" notice notice") && row.contains("noti ↓ Latest activity"));
        app.toast = None;
        draw_sized_buffer(&mut app, 80, 25);

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
        assert!(app.render.scroll().is_none() && app.selection.is_none());
        draw_sized_buffer(&mut app, 80, 25);
        assert!(app.render.scroll().is_none());
        assert!(latest(&app).is_none());
    }
}
