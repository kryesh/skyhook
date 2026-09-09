mod markdown;
mod rows;
mod stream;
pub use rows::RowBlocks;

use super::{
    app::{App, Focus, Hit, MenuKind},
    model::{self, Surface, Tab},
};
use ratatui::{
    Frame,
    buffer::Buffer,
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Retained renderer state. Semantic changes arrive as one value, independently
/// of width/theme reflow; row storage owns its own height index.
pub struct RenderState {
    pub changes: model::ContentChanges,
    pub rows: RowBlocks,
    pub width: u16,
    pub light: bool,
    pub agent: skyhook::identity::AgentId,
    pub highlights: super::tool_view::HighlightCache,
    pub entries: std::collections::HashMap<String, CachedEntry>,
    request_columns: RequestColumns,
}

impl RenderState {
    pub fn new(
        agent: skyhook::identity::AgentId,
        light: bool,
        notify: tokio::sync::mpsc::UnboundedSender<super::app::Work>,
    ) -> Self {
        Self {
            changes: model::ContentChanges {
                reset: true,
                ..Default::default()
            },
            rows: RowBlocks::default(),
            width: 0,
            light,
            agent,
            highlights: super::tool_view::HighlightCache::with_notify(notify),
            entries: std::collections::HashMap::new(),
            request_columns: RequestColumns::default(),
        }
    }

    pub fn content_changed(&mut self, mut changes: model::ContentChanges) {
        changes.reset |= self.changes.reset;
        self.changes = changes;
    }

    pub fn reset_session(&mut self) {
        self.entries.clear();
        self.changes = model::ContentChanges {
            reset: true,
            ..Default::default()
        };
        self.highlights.clear();
    }
}

#[derive(Clone, Copy)]
pub struct Palette {
    pub base: Color,
    pub panel: Color,
    pub input: Color,
    pub agent: Color,
    pub user: Color,
    pub fg: Color,
    pub muted: Color,
    pub selected: Color,
    pub accent: Color,
    pub warning: Color,
    pub error: Color,
}
impl Palette {
    pub fn new(light: bool) -> Self {
        if light {
            Self {
                base: Color::Rgb(245, 245, 245),
                panel: Color::Rgb(234, 234, 234),
                input: Color::Rgb(224, 224, 224),
                agent: Color::Rgb(232, 232, 232),
                user: Color::Rgb(218, 218, 218),
                fg: Color::Rgb(28, 31, 35),
                muted: Color::Rgb(85, 90, 98),
                selected: Color::Rgb(192, 192, 192),
                accent: Color::Rgb(0, 94, 115),
                warning: Color::Rgb(135, 86, 0),
                error: Color::Rgb(175, 35, 35),
            }
        } else {
            Self {
                base: Color::Rgb(0, 0, 0),
                panel: Color::Rgb(28, 28, 28),
                input: Color::Rgb(40, 40, 40),
                agent: Color::Rgb(34, 34, 34),
                user: Color::Rgb(44, 44, 44),
                fg: Color::Rgb(222, 225, 230),
                muted: Color::Rgb(146, 153, 163),
                selected: Color::Rgb(64, 64, 64),
                accent: Color::Rgb(115, 207, 220),
                warning: Color::Rgb(235, 181, 79),
                error: Color::Rgb(245, 116, 116),
            }
        }
    }
    fn background(self, surface: Surface) -> Color {
        match surface {
            Surface::User => self.user,
            Surface::Agent => self.agent,
            _ => self.base,
        }
    }
    fn foreground(self, surface: Surface) -> Color {
        match surface {
            Surface::Muted | Surface::Status | Surface::Reasoning => self.muted,
            Surface::Error => self.error,
            _ => self.fg,
        }
    }
}
#[derive(Clone)]
pub struct Row {
    line: std::sync::Arc<Line<'static>>,
    header: bool,
    x: u16,
    width: u16,
    surface: Surface,
    pub entry: usize,
    pub selectable: bool,
    blank: bool,
    continued: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TextPosition {
    pub row: usize,
    pub byte: usize,
}

/// Inline reasoning and activity spinners are not navigable or copyable entries.
pub fn entry_selectable(entry: &model::Entry) -> bool {
    entry.expandable
        || !(entry.surface == Surface::Reasoning
            || (entry.surface == Surface::Muted && entry.running))
}

impl Row {
    pub fn text(&self) -> String {
        self.line.to_string()
    }
    fn text_x(&self) -> u16 {
        self.x + u16::from(matches!(self.surface, Surface::User | Surface::Agent)) * 2
    }
    pub fn byte_at_column(&self, column: u16) -> usize {
        let mut remaining = column.saturating_sub(self.text_x()) as usize;
        let text = self.text();
        for (byte, grapheme) in text.grapheme_indices(true) {
            if remaining < grapheme.width() {
                return byte;
            }
            remaining = remaining.saturating_sub(grapheme.width());
        }
        text.len()
    }
    fn selection_range(
        &self,
        row: usize,
        selection: Option<(TextPosition, TextPosition)>,
    ) -> Option<std::ops::Range<usize>> {
        let (a, b) = selection?;
        let (start, end) = (a.min(b), a.max(b));
        if !self.selectable || self.blank || start == end || row < start.row || row > end.row {
            return None;
        }
        let length = self.line.spans.iter().map(|span| span.content.len()).sum();
        let start_byte = if row == start.row {
            start.byte.min(length)
        } else {
            0
        };
        let end_byte = if row == end.row {
            end.byte.min(length)
        } else {
            length
        };
        Some(start_byte..end_byte)
    }
}

pub fn selected_text(rows: &RowBlocks, selection: (TextPosition, TextPosition)) -> String {
    let mut text = String::new();
    let mut first = true;
    let start = selection.0.row.min(selection.1.row);
    let end = selection.0.row.max(selection.1.row);
    for index in start..=end {
        let Some(row) = rows.get(index) else { break };
        if let Some(range) = row.selection_range(index, Some(selection)) {
            if !first && !row.continued {
                text.push('\n');
            }
            text.push_str(&row.text()[range]);
            first = false;
        }
    }
    text
}

/// Compare only selected rows, sharing the same fast path as retained rendering.
fn selection_unchanged<'a>(
    previous: &[Row],
    current: impl IntoIterator<Item = &'a Row>,
    selection: (TextPosition, TextPosition),
) -> bool {
    let (start, end) = (selection.0.min(selection.1), selection.0.max(selection.1));
    let mut current = current.into_iter();
    previous.len() == end.row - start.row + 1
        && previous.iter().enumerate().all(|(offset, before)| {
            let Some(after) = current.next() else {
                return false;
            };
            if before.entry != after.entry
                || before.continued != after.continued
                || before.selectable != after.selectable
            {
                return false;
            }
            if std::sync::Arc::ptr_eq(&before.line, &after.line) {
                return true;
            }
            let before = before.text();
            let after = after.text();
            if start.row + offset == end.row {
                before
                    .get(..end.byte)
                    .is_some_and(|prefix| after.get(..end.byte) == Some(prefix))
            } else {
                before == after
            }
        })
}
#[derive(Default)]
pub struct CachedEntry {
    width: u16,
    light: bool,
    stream: stream::StreamLayout,
    body_offset: usize,
    title_rows: usize,
}

fn spinner(tick: usize) -> &'static str {
    ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"][tick % 10]
}

fn fill(frame: &mut Frame, rect: Rect, color: Color) {
    // Clear and background used to walk this rectangle separately through
    // per-cell coordinate lookups. Reset contiguous buffer slices in one pass.
    let buffer = frame.buffer_mut();
    let rect = rect.intersection(buffer.area);
    let stride = buffer.area.width as usize;
    let x = rect.x.saturating_sub(buffer.area.x) as usize;
    for y in rect.y..rect.bottom() {
        let start = (y - buffer.area.y) as usize * stride + x;
        for cell in &mut buffer.content[start..start + rect.width as usize] {
            cell.reset();
            cell.bg = color;
        }
    }
}
fn focus_cursor(frame: &mut Frame, x: u16, y: u16, bg: Color) {
    text(frame, r(x, y, 1, 1), "▌", Color::Rgb(255, 255, 255), bg);
}
fn text(frame: &mut Frame, rect: Rect, text: impl Into<Line<'static>>, fg: Color, bg: Color) {
    let mut text = text.into();
    for span in &mut text.spans {
        span.content = super::model::clean(&span.content).into();
    }
    frame.render_widget(
        Paragraph::new(text).style(Style::default().fg(fg).bg(bg)),
        rect,
    );
}
fn r(x: u16, y: u16, width: u16, height: u16) -> Rect {
    Rect::new(x, y, width, height)
}

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
    let editor_lines = draft_lines(&app.editor, width.saturating_sub(4).max(1) as usize, p);
    let viewing_child = !app.selected.path().is_empty();
    let editor_height = (editor_lines.len() as u16
        + 2
        + u16::from(!app.images.is_empty() || !app.pastes.is_empty()))
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
    let content_changed = app.content_dirty;
    let anchor = if app.render.agent == app.selected
        && (content_changed || app.render.width != width || app.render.light != app.light)
    {
        app.views
            .get(&app.selected)
            .and_then(|view| view.scroll)
            .and_then(|scroll| {
                let row = app.render.rows.get(scroll)?;
                Some((
                    row.entry,
                    app.entries.get(row.entry)?.key.clone(),
                    scroll - app.render.rows.entry_start(row.entry)?,
                ))
            })
    } else {
        None
    };
    app.rebuild_content();
    app.render.highlights.poll();
    let reset = app.render.changes.reset
        || app.render.agent != app.selected
        || app.render.width != width
        || app.render.light != app.light;
    let mut dirty = std::mem::take(&mut app.render.changes.dirty);
    if reset {
        dirty = (0..app.entries.len()).collect();
    }
    if tab == Tab::Requests
        && (reset || !dirty.is_empty() || app.render.rows.entry_count() != app.entries.len())
    {
        app.render.request_columns.update(&app.entries, &mut dirty);
    }
    // Only changed documents are prepared on ordinary content updates.
    let selected = app.views.get(&app.selected).map_or(0, |view| view.row);
    if reset || content_changed || !dirty.is_empty() {
        app.render.highlights.prepare(
            app.entries
                .get(selected)
                .and_then(|entry| entry.document.as_ref())
                .into_iter()
                .chain(
                    dirty
                        .iter()
                        .filter_map(|&i| app.entries.get(i)?.document.as_ref()),
                ),
            app.light,
        );
    }
    let highlighted_sources = app.render.highlights.take_changed_sources();
    app.render
        .rows
        .highlight_entries(&highlighted_sources, &mut dirty);
    dirty.sort_unstable();
    dirty.dedup();
    let truncated = app.render.rows.entry_count() > app.entries.len();
    let changed = reset || !dirty.is_empty() || truncated;
    // Selection validation visits just selected rows, never concatenates history.
    let selection_before = app
        .selection
        .filter(|selection| {
            changed
                && (reset
                    || truncated
                    || dirty
                        .first()
                        .and_then(|&i| app.render.rows.entry_start(i))
                        .is_some_and(|first| first <= selection.0.row.max(selection.1.row)))
        })
        .map(|selection| {
            let start = selection.0.row.min(selection.1.row);
            let end = selection.0.row.max(selection.1.row);
            (
                start,
                (start..=end)
                    .filter_map(|i| app.render.rows.get(i).cloned())
                    .collect::<Vec<_>>(),
            )
        });
    if reset {
        app.render.rows.clear();
        app.render.entries.clear();
    }
    for index in dirty {
        let Some(entry) = app.entries.get(index) else {
            continue;
        };
        let cached = app.render.entries.entry(entry.key.clone()).or_default();
        let append_from = (!reset && cached.width == width && cached.light == app.light)
            .then(|| app.render.changes.appends.get(&index).copied())
            .flatten();
        let block = app.render.rows.block_mut(index);
        update_entry_rows(
            block,
            cached,
            entry,
            index,
            EntryLayout {
                width,
                palette: p,
                highlights: &app.render.highlights,
                request_columns: app.render.request_columns,
            },
            append_from,
        );
        cached.width = width;
        cached.light = app.light;

        app.render.rows.finish_update(index);
        app.render.rows.register_sources(
            index,
            entry
                .document
                .as_ref()
                .map(|doc| doc.highlight_sources().collect())
                .unwrap_or_default(),
        );
    }
    app.render.rows.truncate_entries(app.entries.len());
    if let (Some(selection), Some((start, before))) = (app.selection, selection_before) {
        let unchanged = selection_unchanged(&before, app.render.rows.iter().skip(start), selection);
        if !unchanged {
            app.selection = None;
        }
    }
    app.render.changes.dirty.clear();
    app.render.changes.appends.clear();
    app.render.changes.reset = false;
    if changed {
        app.render.agent = app.selected.clone();
        if let Some((old_index, key, offset)) = anchor
            && let Some(index) = app
                .entries
                .get(old_index)
                .filter(|entry| entry.key == key)
                .map(|_| old_index)
                .or_else(|| app.entries.iter().position(|entry| entry.key == key))
            && let Some(first) = app.render.rows.entry_start(index)
        {
            let count = app
                .render
                .rows
                .entry_start(index + 1)
                .unwrap_or(app.render.rows.len())
                - first;
            app.view().scroll = Some(first + offset.min(count.saturating_sub(1)));
        }
        app.render.width = width;
        app.render.light = app.light;
    }
    app.content_rows = app.render.rows.len();
    let max = app
        .content_rows
        .saturating_sub(app.content_rect.height as usize);
    let scroll = app.view().scroll.map(|n| n.min(max)).unwrap_or(max);
    if app.view().scroll.is_some() {
        app.view().scroll = Some(scroll);
    }
    let selected_entry = app.view().row;
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
        let focused = navigation_active
            && app.focus == Focus::Content
            && row.selectable
            && row.entry == selected_entry;
        let hovered = app.hover.is_some_and(|point| rect.contains(point.into()))
            && entry.is_some_and(|e| e.expandable);
        let bg = if !row.blank
            && selected.is_none()
            && ((focused && entry.is_some_and(|e| e.expandable)) || hovered)
        {
            p.selected
        } else {
            p.background(row.surface)
        };
        fill(frame, rect, bg);
        let text_rect = r(
            row.text_x(),
            y,
            row.width
                .saturating_sub(if matches!(row.surface, Surface::User | Surface::Agent) {
                    4
                } else {
                    0
                }),
            1,
        );
        render_line(
            row.line.as_ref(),
            text_rect,
            frame.buffer_mut(),
            Style::default().fg(p.foreground(row.surface)).bg(bg),
        );
        if let Some(range) = selected {
            let value = row.text();
            let start = row.text_x() as usize + value[..range.start].width();
            let end = row.text_x() as usize + value[..range.end].width();
            // Include continuation cells of wide graphemes, which Paragraph resets.
            for x in start.min(width as usize)..end.min(width as usize) {
                frame.buffer_mut()[(x as u16, y)].set_bg(p.selected);
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
                    p.muted,
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
            text(frame, rect, "↓ Latest activity", p.accent, p.panel);
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
    fill(frame, app.tree_rect, p.panel);
    app.tree_cursor = app.tree_cursor.min(tree_agents.len().saturating_sub(1));
    if app.focus == Focus::Tree {
        if app.tree_cursor < app.tree_scroll {
            app.tree_scroll = app.tree_cursor;
        }
        if app.tree_cursor >= app.tree_scroll + tree_rows as usize {
            app.tree_scroll = app.tree_cursor + 1 - tree_rows as usize;
        }
    }
    app.tree_scroll = app
        .tree_scroll
        .min(tree_agents.len().saturating_sub(tree_rows as usize));
    let agent_stats = tree_agents
        .iter()
        .map(|agent| model::agent_footer_stats(&app.snapshot, &app.projection, &agent.id))
        .collect::<Vec<_>>();
    let stats_columns = AgentStatsColumns::new(agent_stats.iter());
    let stats_column_width = stats_columns.width();
    let minimum_name_width = tree_agents
        .iter()
        .map(|agent| {
            let indent = (agent.id.depth() as u16 * 4).min(width / 3);
            indent + 16.max(model::target_suffix(&agent.target).width() as u16 + 8)
        })
        .max()
        .unwrap_or(16);
    let tree_width = width.saturating_sub(4);
    // Shared columns keep statuses aligned across agent depths and summary lengths.
    // Reserve two blank cells between the name, status, and token columns.
    let stats_reserved = if tree_width >= stats_column_width + minimum_name_width + 2 {
        stats_column_width + 2
    } else {
        0
    };
    let status_width =
        if width >= 70 && tree_width.saturating_sub(stats_reserved) >= minimum_name_width + 30 {
            28
        } else {
            0
        };
    let status_reserved = if status_width > 0 {
        status_width + 2
    } else {
        0
    };
    for (index, agent) in tree_agents
        .iter()
        .enumerate()
        .skip(app.tree_scroll)
        .take(tree_rows as usize)
    {
        let y = tree_y + 1 + (index - app.tree_scroll) as u16;
        let selected = agent.id == app.selected;
        let focused = navigation_active && app.focus == Focus::Tree && app.tree_cursor == index;
        let rect = r(2, y, width.saturating_sub(4), 1);
        let hover = app.hover.is_some_and(|point| rect.contains(point.into()));
        let bg = if selected || focused || hover {
            p.selected
        } else {
            p.panel
        };
        fill(frame, rect, bg);
        let (running, status) = app.agent_status(agent);
        app.animating |= running;
        let symbol = agent_symbol(running, &status, agent.terminal, app.tick_count);
        let indent = (agent.id.depth() as u16 * 4).min(width / 3);
        let target = model::target_suffix(&agent.target);
        let stats = stats_columns.format(&agent_stats[index]);
        let available = width.saturating_sub(indent + 4);
        let name_width = available.saturating_sub(status_reserved + stats_reserved);
        let name = format!(
            "{} {symbol} {}{}",
            if selected { ">" } else { " " },
            clipped_header(
                &model::clean(&agent.name),
                name_width.saturating_sub(target.width() as u16 + 4)
            ),
            target,
        );
        text(
            frame,
            r(2 + indent, y, name_width, 1),
            model::clean(&name),
            p.fg,
            bg,
        );
        if focused {
            focus_cursor(frame, 2 + indent, y, bg);
        }
        if status_width > 0 {
            text(
                frame,
                r(
                    width - status_width - stats_reserved - 2,
                    y,
                    status_width,
                    1,
                ),
                status.clone(),
                if status.starts_with("Waiting") {
                    p.warning
                } else {
                    p.muted
                },
                bg,
            );
        }
        if stats_reserved > 0 {
            text(
                frame,
                r(width - stats_column_width - 2, y, stats_column_width, 1),
                stats,
                p.muted,
                bg,
            );
        }
        app.hits.push((rect, Hit::Agent(agent.id.clone())));
    }
    fill(frame, app.composer_rect, p.input);
    if prompt_active {
        draw_prompt(frame, app, p);
    } else if !viewing_child {
        let cursor_prefix = &app.editor.text[..app.editor.cursor];
        let prefix_lines = wrap_plain(cursor_prefix, width.saturating_sub(4) as usize);
        let cursor_line = prefix_lines.len().saturating_sub(1);
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
        if !app.images.is_empty() || !app.pastes.is_empty() {
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
                    .chain(app.pastes.iter().enumerate().map(|(i, paste)| {
                        format!("[Paste {} · {} lines]", i + 1, paste.lines().count())
                    }))
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
            let column = prefix_lines.last().map_or(0, |s| s.width()) as u16;
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

fn draw_prompt(frame: &mut Frame, app: &mut App, p: Palette) {
    let rect = app.composer_rect;
    let options = app.prompt_options();
    let body = app.prompt_text();
    let lines = wrap_plain(&model::clean(&body), rect.width.saturating_sub(4) as usize);
    let available = rect.height.saturating_sub(2);
    let body_height = if options.is_empty() {
        available
    } else {
        (available / 2).max(1)
    };
    let option_height = if options.is_empty() {
        0
    } else {
        available.saturating_sub(body_height)
    };
    app.prompt_body_rect = r(2, rect.y, rect.width.saturating_sub(4), body_height);
    app.prompt_options_rect = r(
        2,
        rect.y + body_height,
        rect.width.saturating_sub(4),
        option_height,
    );
    app.prompt_body_rows = lines.len();
    app.prompt_body_scroll = app
        .prompt_body_scroll
        .min(lines.len().saturating_sub(body_height as usize));
    for (offset, line) in lines
        .iter()
        .skip(app.prompt_body_scroll)
        .take(body_height as usize)
        .enumerate()
    {
        text(
            frame,
            r(2, rect.y + offset as u16, rect.width.saturating_sub(4), 1),
            line.clone(),
            p.warning,
            p.input,
        );
    }
    let mut option_lines = Vec::new();
    let mut selected_start = 0;
    for (index, option) in options.iter().enumerate() {
        if index == app.prompt_choice {
            selected_start = option_lines.len();
        }
        for (offset, line) in wrap_plain(
            &model::clean(option),
            rect.width.saturating_sub(6).max(1) as usize,
        )
        .into_iter()
        .enumerate()
        {
            option_lines.push((
                index,
                format!(
                    "{} {line}",
                    if offset == 0 && index == app.prompt_choice {
                        ">"
                    } else {
                        " "
                    }
                ),
            ));
        }
    }
    app.prompt_option_rows = option_lines.len();
    if app.prompt_reveal {
        if selected_start < app.prompt_option_scroll {
            app.prompt_option_scroll = selected_start;
        } else if selected_start >= app.prompt_option_scroll + option_height as usize {
            app.prompt_option_scroll =
                selected_start.saturating_sub(option_height.saturating_sub(1) as usize);
        }
        app.prompt_reveal = false;
    }
    app.prompt_option_scroll = app
        .prompt_option_scroll
        .min(option_lines.len().saturating_sub(option_height as usize));
    let mut cursor_drawn = false;
    for (offset, (index, line)) in option_lines
        .iter()
        .skip(app.prompt_option_scroll)
        .take(option_height as usize)
        .enumerate()
    {
        let row = r(
            2,
            rect.y + body_height + offset as u16,
            rect.width.saturating_sub(4),
            1,
        );
        text(
            frame,
            row,
            line.clone(),
            p.fg,
            if *index == app.prompt_choice {
                p.selected
            } else {
                p.input
            },
        );
        if *index == app.prompt_choice && !cursor_drawn && app.menu.is_none() {
            focus_cursor(frame, row.x - 1, row.y, p.input);
            cursor_drawn = true;
        }
        app.hits.push((row, Hit::PromptChoice(*index)));
    }
    let secret = app.prompts.front().is_some_and(|prompt| prompt.secret());
    let input = if secret {
        "●".repeat(app.prompt_editor.text.graphemes(true).count())
    } else {
        model::clean(&app.prompt_editor.text)
    };
    let input_label = match app.prompts.front().map(|prompt| &prompt.kind) {
        Some(crate::interaction::PromptKind::Questions { questions, .. }) => {
            match questions.get(app.question_index) {
                Some(question) if app.prompt_choice < question.options.len() => {
                    "Comment (optional): "
                }
                Some(_) => "Answer: ",
                None => "",
            }
        }
        _ => "",
    };
    text(
        frame,
        r(
            2,
            rect.bottom().saturating_sub(2),
            rect.width.saturating_sub(4),
            1,
        ),
        format!("{input_label}{input}▏"),
        p.fg,
        p.input,
    );
    text(
        frame,
        r(
            2,
            rect.bottom().saturating_sub(1),
            rect.width.saturating_sub(4),
            1,
        ),
        "↑↓ choose · PgUp/PgDn text · Ctrl+PgUp/PgDn choices · Enter submit · Esc dismiss",
        p.muted,
        p.input,
    );
}
/// Numeric fields share their own right edge, not just the footer's right edge.
/// Measure all agents, including off-screen rows, to keep columns stable on scroll.
struct AgentStatsColumns([usize; 3]);

impl AgentStatsColumns {
    fn new<'a>(rows: impl IntoIterator<Item = &'a [String; 3]>) -> Self {
        let mut widths = [0; 3];
        for row in rows {
            for (width, value) in widths.iter_mut().zip(row) {
                *width = (*width).max(value.width());
            }
        }
        Self(widths)
    }

    fn width(&self) -> u16 {
        (self.0.iter().sum::<usize>() + 6) as u16
    }

    fn format(&self, row: &[String; 3]) -> String {
        row.iter()
            .zip(self.0)
            .map(|(value, width)| format!("{}{value}", " ".repeat(width - value.width())))
            .collect::<Vec<_>>()
            .join(" · ")
    }
}

/// Like agent statistics, request fields use widths measured across the complete
/// list, not the viewport. Metadata is left aligned and numeric fields are right aligned.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct RequestColumns {
    metadata: [usize; 4],
    statistics: [usize; 4],
}

impl RequestColumns {
    fn new<'a>(rows: impl IntoIterator<Item = &'a model::RequestRow>) -> Self {
        let mut columns = Self::default();
        for row in rows {
            for (width, value) in columns.metadata.iter_mut().zip(row.metadata()) {
                *width = (*width).max(value.width());
            }
            for (width, value) in columns.statistics.iter_mut().zip(row.statistics()) {
                *width = (*width).max(value.width());
            }
        }
        columns
    }

    fn update(&mut self, entries: &[model::Entry], dirty: &mut Vec<usize>) {
        let columns = Self::new(entries.iter().filter_map(|entry| entry.request.as_ref()));
        if *self != columns {
            *self = columns;
            // A wider token count or elapsed time changes even unchanged rows.
            dirty.extend(
                entries
                    .iter()
                    .enumerate()
                    .filter_map(|(index, entry)| entry.request.as_ref().map(|_| index)),
            );
        }
    }

    fn line(&self, row: &model::RequestRow, width: u16, p: Palette) -> Line<'static> {
        // The animation overlay paints at column zero. Reserve its cell and a
        // separator on every row so running/completed requests stay aligned.
        let gutter = width.min(2);
        let width = width - gutter;
        let statistics = row
            .statistics()
            .iter()
            .zip(self.statistics)
            .map(|(value, width)| format!("{}{value}", " ".repeat(width - value.width())))
            .collect::<Vec<_>>()
            .join(" · ");
        let show_statistics = statistics.width() + 18 <= width as usize;
        let left_width = if show_statistics {
            width - statistics.width() as u16 - 2
        } else {
            width
        };
        let mut widths = self.metadata;
        // Clip the model column first so IDs, purpose and state remain visible.
        let excess = (widths.iter().sum::<usize>() + 9).saturating_sub(left_width as usize);
        widths[2] = widths[2].saturating_sub(excess);
        let metadata = row
            .metadata()
            .iter()
            .zip(widths)
            .map(|(value, width)| {
                let value = clipped_header(value, width.min(u16::MAX as usize) as u16);
                format!("{value}{}", " ".repeat(width - value.width()))
            })
            .collect::<Vec<_>>()
            .join(" · ");
        let metadata = clipped_header(&metadata, left_width);
        let gap = width as usize - metadata.width();
        let mut spans = vec![Span::raw(" ".repeat(gutter as usize)), Span::raw(metadata)];
        if show_statistics {
            spans.push(Span::raw(" ".repeat(gap - statistics.width())));
            spans.push(Span::styled(statistics, Style::default().fg(p.muted)));
        }
        Line::from(spans)
    }
}

fn agent_symbol(running: bool, status: &str, terminal: bool, tick: usize) -> &'static str {
    if running {
        spinner(tick)
    } else if status.contains("permission") {
        "◇"
    } else if status.contains("input") {
        "?"
    } else if status.starts_with("Waiting") {
        "◷"
    } else if terminal {
        "✓"
    } else {
        "·"
    }
}
fn draw_menu(frame: &mut Frame, app: &mut App, p: Palette) {
    app.refresh_agent_menu();
    let Some(menu) = &app.menu else { return };
    let area = app.content_rect;
    let margin = 7.min(area.width.saturating_sub(20) / 2);
    let width = area.width.saturating_sub(margin * 2);
    let rect = r(area.x + margin, area.y, width, area.height);
    fill(frame, rect, p.input);
    text(
        frame,
        r(rect.x + 1, rect.y, width.saturating_sub(2), 1),
        menu.title.clone(),
        p.accent,
        p.input,
    );
    text(
        frame,
        r(rect.x + 1, rect.y + 1, width.saturating_sub(2), 1),
        format!("> {}▏", model::clean(&menu.input.text)),
        p.fg,
        p.input,
    );
    let items = menu.filtered();
    let agent_menu = matches!(menu.kind, MenuKind::Agents);
    let agent_stats = if agent_menu {
        app.projection
            .agents
            .iter()
            .map(|agent| model::agent_footer_stats(&app.snapshot, &app.projection, &agent.id))
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let stats_columns = AgentStatsColumns::new(agent_stats.iter());
    let stats_width = stats_columns.width();
    // Match the inline tree's aligned status/token columns. On narrow screens,
    // use a second line so the picker still exposes both status and token usage.
    let compact_agents = agent_menu && width.saturating_sub(2) < stats_width + 50;
    let stacked_stats = compact_agents && width.saturating_sub(2) < stats_width + 30;
    let row_height = if stacked_stats {
        3
    } else if compact_agents {
        2
    } else {
        1
    };
    let height = rect.height.saturating_sub(3) as usize / row_height;
    let top = menu.selected.saturating_sub(height.saturating_sub(1));
    for (i, item) in items.iter().enumerate().skip(top).take(height) {
        let y = rect.y + 2 + ((i - top) * row_height) as u16;
        let selected = i == menu.selected;
        let bg = if selected { p.selected } else { p.input };
        let row = r(rect.x + 1, y, width.saturating_sub(2), 1);
        if agent_menu {
            if let Some(agent) = app
                .projection
                .agents
                .iter()
                .find(|agent| agent.id.to_string() == item.value)
            {
                let (running, status) = app.agent_status(agent);
                app.animating |= running;
                let symbol = agent_symbol(running, &status, agent.terminal, app.tick_count);
                let stats = stats_columns.format(&model::agent_footer_stats(
                    &app.snapshot,
                    &app.projection,
                    &agent.id,
                ));
                let target = model::target_suffix(&agent.target);
                let indent = (agent.id.depth() as u16 * 4).min(row.width / 3);
                let name_width = if compact_agents {
                    row.width
                } else {
                    row.width.saturating_sub(stats_width + 32)
                };
                let name = format!(
                    "{}{symbol} {}{}",
                    " ".repeat(indent as usize),
                    clipped_header(
                        &model::clean(&agent.name),
                        name_width.saturating_sub(indent + 2 + target.width() as u16)
                    ),
                    target
                );
                fill(frame, r(row.x, row.y, row.width, row_height as u16), bg);
                text(
                    frame,
                    r(row.x, y, name_width, 1),
                    model::clean(&name),
                    p.fg,
                    bg,
                );
                let status_row = if stacked_stats {
                    r(row.x, y + 1, row.width, 1)
                } else if compact_agents {
                    r(row.x, y + 1, row.width.saturating_sub(stats_width + 2), 1)
                } else {
                    r(row.x + name_width + 2, y, 28, 1)
                };
                text(
                    frame,
                    status_row,
                    status.clone(),
                    if status.starts_with("Waiting") {
                        p.warning
                    } else {
                        p.muted
                    },
                    bg,
                );
                if stats_width <= row.width {
                    text(
                        frame,
                        r(
                            row.right() - stats_width,
                            status_row.y + u16::from(stacked_stats),
                            stats_width,
                            1,
                        ),
                        stats,
                        p.muted,
                        bg,
                    );
                }
            }
        } else {
            let text_value = if item.detail.is_empty() {
                item.label.clone()
            } else {
                format!("{}   {}", item.label, item.detail)
            };
            text(frame, row, model::clean(&text_value), p.fg, bg);
        }
        if selected {
            focus_cursor(frame, rect.x, y, p.input);
        }
        app.hits
            .push((r(row.x, row.y, row.width, row_height as u16), Hit::Menu(i)));
    }
    if items.is_empty() && !matches!(menu.kind, MenuKind::Attach | MenuKind::OutputSearch(_)) {
        text(
            frame,
            r(rect.x + 1, rect.y + 2, width.saturating_sub(2), 1),
            "No matching entries",
            p.muted,
            p.input,
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
        p.muted,
        p.input,
    );
}

#[derive(Clone, Copy)]
struct EntryLayout<'a> {
    width: u16,
    palette: Palette,
    highlights: &'a super::tool_view::HighlightCache,
    request_columns: RequestColumns,
}

fn update_entry_rows(
    rows: &mut Vec<Row>,
    cached: &mut CachedEntry,
    entry: &model::Entry,
    index: usize,
    settings: EntryLayout<'_>,
    append_from: Option<usize>,
) {
    let EntryLayout {
        width,
        palette: p,
        highlights,
        request_columns,
    } = settings;
    if let Some(request) = &entry.request {
        let geometry = EntryGeometry::new(entry, width, index);
        rows.clear();
        rows.push(geometry.row(
            request_columns.line(request, geometry.body_width, p),
            true,
            false,
        ));
        return;
    }
    let block = matches!(entry.surface, Surface::User | Surface::Agent);
    if (entry.surface == Surface::Reasoning || block) && entry.document.is_none() {
        let geometry = EntryGeometry::new(entry, width, index);
        let body_width = geometry.body_width;
        let x = geometry.x;
        let block_width = geometry.block_width;
        let make_row = |line, header, continued| geometry.row(line, header, continued);
        let blank = |surface, x, width| geometry.blank(surface, x, width);
        let has_title = block || entry.expandable;
        if append_from.is_none() {
            rows.clear();
            if block {
                rows.push(blank(entry.surface, x, block_width));
            }
            cached.body_offset = 0;
            if has_title {
                let (title, _) = entry.text.split_once('\n').unwrap_or((&entry.text, ""));
                cached.body_offset = (title.len() + 1).min(entry.text.len());
                let title = if block {
                    Line::from(Span::styled(
                        model::clean(title),
                        Style::default().add_modifier(Modifier::BOLD),
                    ))
                } else {
                    Line::from(model::clean(title))
                };
                for (part, line) in wrap_words(title, body_width as usize)
                    .into_iter()
                    .enumerate()
                {
                    rows.push(make_row(line, part == 0, part > 0));
                }
            }
            cached.title_rows = rows.len();
        }
        let body = &entry.text[cached.body_offset..];
        let prefix = if entry.surface == Surface::Reasoning && !entry.expandable && entry.running {
            "  "
        } else {
            ""
        };
        let body_append = append_from.and_then(|from| from.checked_sub(cached.body_offset));
        let suffix =
            cached
                .stream
                .update_prefixed(body, body_width as usize, p, body_append, prefix);
        if let Some((truncate, suffix)) = suffix {
            rows.truncate(cached.title_rows + truncate);
            // Expanded reasoning omits the empty body; response blocks retain it.
            if !entry.expandable || !body.is_empty() || block {
                for (line, continued) in suffix {
                    let header = rows.is_empty();
                    rows.push(make_row(line, header, continued));
                }
            }
            if block {
                if let Some(footer) = &entry.footer {
                    for line in [
                        Line::default(),
                        Line::from(Span::styled(
                            model::clean(footer),
                            Style::default().fg(p.muted),
                        )),
                    ] {
                        for (part, line) in wrap_words(line, body_width as usize)
                            .into_iter()
                            .enumerate()
                        {
                            rows.push(make_row(line, false, part > 0));
                        }
                    }
                }
                rows.push(blank(entry.surface, x, block_width));
            }
            rows.push(blank(Surface::Muted, 0, width));
        }
    } else {
        cached.stream = stream::StreamLayout::default();
        *rows = layout_document_or_plain(entry, width, p, highlights, index);
    }
}

/// Geometry and framing shared by the streaming and document/plain paths.
struct EntryGeometry {
    x: u16,
    block_width: u16,
    row_width: u16,
    body_width: u16,
    surface: Surface,
    entry: usize,
    selectable: bool,
}
impl EntryGeometry {
    fn new(entry: &model::Entry, width: u16, index: usize) -> Self {
        let block = matches!(entry.surface, Surface::User | Surface::Agent);
        let available = width.saturating_sub(5).max(1);
        // Message boxes keep one column on the sender's side and three on the
        // opposite side, leaving all remaining width available for content.
        let block_width = if block {
            width.saturating_sub(4).max(1)
        } else {
            available
        };
        let indent = entry.indent.min(available / 3);
        Self {
            x: if entry.surface == Surface::User {
                width.saturating_sub(block_width + 1)
            } else if block {
                1
            } else {
                2 + indent
            },
            block_width,
            row_width: block_width.saturating_sub(if block { 0 } else { indent }),
            body_width: block_width
                .saturating_sub(if block { 4 } else { indent })
                .max(1),
            surface: entry.surface,
            entry: index,
            selectable: entry_selectable(entry),
        }
    }
    fn row(&self, line: Line<'static>, header: bool, continued: bool) -> Row {
        Row {
            line: std::sync::Arc::new(line),
            header,
            x: self.x,
            width: self.row_width,
            surface: self.surface,
            entry: self.entry,
            selectable: self.selectable,
            blank: false,
            continued,
        }
    }
    fn blank(&self, surface: Surface, x: u16, width: u16) -> Row {
        Row {
            line: std::sync::Arc::new(Line::default()),
            header: false,
            x,
            width,
            surface,
            entry: self.entry,
            selectable: self.selectable,
            blank: true,
            continued: false,
        }
    }
}

/// Non-streaming fallback is entry-local and handles only documents or plain text.
fn layout_document_or_plain(
    entry: &model::Entry,
    width: u16,
    p: Palette,
    highlights: &super::tool_view::HighlightCache,
    index: usize,
) -> Vec<Row> {
    let geometry = EntryGeometry::new(entry, width, index);
    let block = matches!(entry.surface, Surface::User | Surface::Agent);
    let lines = if let Some(document) = &entry.document {
        document.lines(Some(highlights), p.fg == Palette::new(true).fg)
    } else {
        debug_assert!(!block && entry.surface != Surface::Reasoning);
        model::clean(&entry.text)
            .split('\n')
            .map(|s| Line::from(s.to_owned()))
            .collect()
    };
    let mut rows = Vec::new();
    if block {
        rows.push(geometry.blank(entry.surface, geometry.x, geometry.block_width));
    }
    let mut header = true;
    for line in lines {
        for (part, line) in wrap_line(line, geometry.body_width as usize)
            .into_iter()
            .enumerate()
        {
            rows.push(geometry.row(line, header, part > 0));
            header = false;
        }
    }
    if block {
        rows.push(geometry.blank(entry.surface, geometry.x, geometry.block_width));
    }
    if !entry.compact_after {
        rows.push(geometry.blank(Surface::Muted, 0, width));
    }
    rows
}

/// Borrow cached spans while matching Paragraph's unwrapped clipping and styling.
/// Unlike Line's Widget implementation, Paragraph leaves wide continuation cells
/// in the surface style and skips individual graphemes wider than the viewport.
fn render_line(line: &Line<'_>, area: Rect, buffer: &mut Buffer, style: Style) {
    let area = area.intersection(buffer.area);
    if area.is_empty() {
        return;
    }
    buffer.set_style(area, style);
    let graphemes = || {
        let mut used = 0;
        line.styled_graphemes(Style::default())
            .filter(move |g| g.symbol.width() <= area.width as usize)
            .take_while(move |g| {
                used += g.symbol.width();
                used <= area.width as usize
            })
    };
    let width = graphemes().map(|g| g.symbol.width() as u16).sum::<u16>();
    let mut x = area.x
        + match line.alignment.unwrap_or(Alignment::Left) {
            Alignment::Left => 0,
            Alignment::Center => area.width / 2 - width / 2,
            Alignment::Right => area.width - width,
        };
    for grapheme in graphemes() {
        let width = grapheme.symbol.width() as u16;
        if width != 0 {
            buffer[(x, area.y)]
                .set_symbol(grapheme.symbol)
                .set_style(grapheme.style);
            x += width;
        }
    }
}

pub fn wrap_plain(text: &str, width: usize) -> Vec<String> {
    text.split('\n')
        .flat_map(|line| wrap_line(Line::from(line.to_owned()), width.max(1)))
        .map(|line| line.to_string())
        .collect()
}
fn draft_lines(editor: &super::editor::Editor, width: usize, p: Palette) -> Vec<Line<'static>> {
    let selection = editor
        .anchor
        .map(|anchor| anchor.min(editor.cursor)..anchor.max(editor.cursor));
    let mut lines = Vec::new();
    let mut spans = Vec::new();
    for (offset, grapheme) in editor.text.grapheme_indices(true) {
        if grapheme == "\n" {
            lines.extend(wrap_line(Line::from(std::mem::take(&mut spans)), width));
            continue;
        }
        let style = if selection
            .as_ref()
            .is_some_and(|range| range.contains(&offset))
        {
            Style::default().bg(p.selected)
        } else {
            Style::default()
        };
        spans.push(Span::styled(grapheme.to_owned(), style));
    }
    lines.extend(wrap_line(Line::from(spans), width));
    lines
}
fn wrap_line(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    let mut result = Vec::new();
    let mut spans = Vec::new();
    let mut used = 0;
    for span in line.spans {
        let mut chunk = String::new();
        for grapheme in span.content.graphemes(true) {
            let size = grapheme.width();
            if used + size > width && used > 0 {
                if !chunk.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut chunk), span.style));
                }
                result.push(Line::from(std::mem::take(&mut spans)));
                used = 0;
            }
            chunk.push_str(grapheme);
            used += size;
        }
        if !chunk.is_empty() {
            spans.push(Span::styled(chunk, span.style));
        }
    }
    result.push(Line::from(spans));
    result
}
/// Wrap prose at whitespace without losing source bytes or inline styles.
/// Only an unbroken token wider than the viewport may be split. Keeping the
/// separating whitespace also preserves copy/selection and streaming offsets.
fn wrap_words(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    if line.width() <= width {
        return vec![line];
    }
    let mut tokens: Vec<(bool, Vec<Span<'static>>)> = Vec::new();
    for span in line.spans {
        let mut start = 0;
        let mut whitespace = None;
        for (offset, ch) in span.content.char_indices() {
            let next = ch.is_whitespace();
            if whitespace.is_some_and(|previous| previous != next) {
                let previous = whitespace.unwrap();
                if tokens.last().is_none_or(|token| token.0 != previous) {
                    tokens.push((previous, Vec::new()));
                }
                tokens.last_mut().unwrap().1.push(Span::styled(
                    span.content[start..offset].to_owned(),
                    span.style,
                ));
                start = offset;
            }
            whitespace = Some(next);
        }
        if let Some(whitespace) = whitespace {
            if tokens.last().is_none_or(|token| token.0 != whitespace) {
                tokens.push((whitespace, Vec::new()));
            }
            tokens
                .last_mut()
                .unwrap()
                .1
                .push(Span::styled(span.content[start..].to_owned(), span.style));
        }
    }
    let mut result = Vec::new();
    let mut row = Line::default();
    let mut used = 0;
    for (whitespace, spans) in tokens {
        let token_width = spans.iter().map(Span::width).sum::<usize>();
        if whitespace {
            // Keep separators on the preceding row, even when they lie beyond
            // its visible edge. They are clipped by the viewport, not discarded
            // from selection text or rendered as a blank-only continuation row.
            used += token_width;
            row.spans.extend(spans);
            continue;
        }
        if token_width > 0
            && used > 0
            && (used >= width || token_width <= width && used + token_width > width)
        {
            result.push(std::mem::take(&mut row));
            used = 0;
        }
        if used + token_width <= width {
            used += token_width;
            row.spans.extend(spans);
        } else {
            // Whitespace and overlong tokens retain every grapheme; the latter
            // cannot fit on any row even if moved to its own line.
            row.spans.extend(spans);
            let mut wrapped = wrap_line(row, width);
            row = wrapped.pop().unwrap_or_default();
            used = row.width();
            result.extend(wrapped);
        }
    }
    result.push(row);
    result
}

#[cfg(test)]
fn markdown(text: &str, p: Palette) -> Vec<Line<'static>> {
    markdown::render(text, p, true)
}

/// Truncate metadata at grapheme boundaries.
fn clipped_header(value: &str, width: u16) -> String {
    let budget = width as usize;
    if budget == 0 {
        return String::new();
    }
    if value.width() <= budget {
        return value.to_owned();
    }
    let mut result = String::new();
    let mut used = 0;
    for grapheme in value.graphemes(true) {
        if used + grapheme.width() > budget - 1 {
            break;
        }
        result.push_str(grapheme);
        used += grapheme.width();
    }
    result.push('…');
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_rows_reserve_spinner_gutter_and_show_unlabelled_statistics() {
        let running = model::RequestRow {
            sequence: 7,
            purpose: skyhook::session::ModelPurpose::Agent,
            model: "qwen".into(),
            status: "Running",
            usage: Some(skyhook::provider::protocol::Usage {
                input_tokens: 80,
                cached_input_tokens: 20,
                output_tokens: 84,
            }),
            elapsed_tenths: Some(12),
        };
        let mut completed = running.clone();
        completed.sequence = 123;
        completed.status = "Completed";
        let columns = RequestColumns::new([&running, &completed]);
        for row in [&running, &completed] {
            let line = columns.line(row, 100, Palette::new(false));
            let text: String = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect();
            assert!(text.starts_with(&format!("  Request #{}", row.sequence)));
            assert!(text.ends_with("84 · 80 · 20 · 1.2s"));
            for label in ["Out ", "In ", "Cached ", "Time "] {
                assert!(!text.contains(label));
            }
            let area = Rect::new(0, 0, 100, 1);
            let mut buffer = Buffer::empty(area);
            Paragraph::new(line).render(area, &mut buffer);
            // This is the same overlay column used by the animated requests view.
            buffer[(0, 0)].set_symbol(spinner(0));
            assert_eq!(buffer[(0, 0)].symbol(), spinner(0));
            assert_eq!(buffer[(1, 0)].symbol(), " ");
            assert_eq!(buffer[(2, 0)].symbol(), "R");
            assert_eq!(buffer[(3, 0)].symbol(), "e");
        }
    }

    #[test]
    fn request_rows_keep_the_gutter_within_narrow_viewports() {
        let row = model::RequestRow {
            sequence: 12345,
            purpose: skyhook::session::ModelPurpose::Compaction,
            model: "long model 界界 😀".into(),
            status: "Running",
            usage: None,
            elapsed_tenths: None,
        };
        let columns = RequestColumns::new([&row]);
        for width in 0..100 {
            let line = columns.line(&row, width, Palette::new(false));
            assert!(line.width() <= width as usize, "overflow at width {width}");
            let text: String = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect();
            assert!(text.starts_with(&" ".repeat(width.min(2) as usize)));
            if width == 99 {
                assert!(text.ends_with("— · — · — · —"));
            }
        }
    }

    // Full-render oracle shares static geometry, but independently parses and wraps
    // the complete source instead of reusing any incremental streaming state.
    fn layout(
        entries: &[model::Entry],
        width: u16,
        p: Palette,
        highlights: Option<&super::super::tool_view::HighlightCache>,
    ) -> Vec<Row> {
        let mut rows = Vec::new();
        for (index, entry) in entries.iter().enumerate() {
            let geometry = EntryGeometry::new(entry, width, index);
            let block = matches!(entry.surface, Surface::User | Surface::Agent);
            let lines = if let Some(document) = &entry.document {
                document.lines(highlights, p.fg == Palette::new(true).fg)
            } else if entry.surface == Surface::Reasoning {
                let text = model::clean(&entry.text);
                if entry.expandable {
                    let (title, body) = text.split_once('\n').unwrap_or((&text, ""));
                    let mut lines = vec![Line::from(title.to_owned())];
                    if !body.is_empty() {
                        lines.extend(markdown(body, p));
                    }
                    lines
                } else {
                    let mut lines = markdown(&text, p);
                    if entry.running
                        && let Some(first) = lines.first_mut()
                    {
                        first.spans.insert(0, Span::raw("  "));
                    }
                    lines
                }
            } else if block {
                let text = model::clean(&entry.text);
                let (sender, body) = text.split_once('\n').unwrap_or((&text, ""));
                let mut lines = vec![Line::from(Span::styled(
                    sender.to_owned(),
                    Style::default().add_modifier(Modifier::BOLD),
                ))];
                lines.extend(markdown(body, p));
                if let Some(model) = &entry.footer {
                    lines.push(Line::default());
                    lines.push(Line::from(Span::styled(
                        model::clean(model),
                        Style::default().fg(p.muted),
                    )));
                }
                lines
            } else {
                model::clean(&entry.text)
                    .split('\n')
                    .map(|s| Line::from(s.to_owned()))
                    .collect()
            };
            if block {
                rows.push(geometry.blank(entry.surface, geometry.x, geometry.block_width));
            }
            let mut header = true;
            for line in lines {
                let wrapped =
                    if entry.document.is_none() && (block || entry.surface == Surface::Reasoning) {
                        wrap_words(line, geometry.body_width as usize)
                    } else {
                        wrap_line(line, geometry.body_width as usize)
                    };
                for (part, line) in wrapped.into_iter().enumerate() {
                    rows.push(geometry.row(line, header, part > 0));
                    header = false;
                }
            }
            if block {
                rows.push(geometry.blank(entry.surface, geometry.x, geometry.block_width));
            }
            if !entry.compact_after {
                rows.push(geometry.blank(Surface::Muted, 0, width));
            }
        }
        rows
    }
    use ratatui::widgets::Widget;

    #[test]
    fn borrowed_lines_match_paragraph_styles_and_clipping() {
        let lines = [
            Line::from("plain text"),
            Line::from("  界 👩‍💻 wide  "),
            Line::from(vec![
                Span::styled(
                    "bold ",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::styled("界 tail", Style::default().bg(Color::Blue)),
            ])
            .style(Style::default().add_modifier(Modifier::ITALIC)),
            Line::default(),
        ];
        for line in lines {
            for width in 0..20 {
                let bounds = Rect::new(0, 0, 25, 3);
                let area = Rect::new(2, 1, width, 1);
                let mut expected = Buffer::empty(bounds);
                let mut actual = expected.clone();
                let style = Style::default().fg(Color::White).bg(Color::DarkGray);
                Paragraph::new(line.clone())
                    .style(style)
                    .render(area, &mut expected);
                render_line(&line, area, &mut actual, style);
                assert_eq!(actual, expected, "width {width}: {line:?}");
            }
        }
    }

    #[test]
    fn selection_preserves_soft_wrapped_text_and_code_whitespace() {
        let source = "  first line with enough text to wrap\n    second line  ";
        let entry = model::Entry {
            key: "message".into(),
            text: format!("Agent\n```text\n{source}\n```"),
            surface: Surface::Agent,
            expandable: false,
            default_open: false,
            running: false,
            footer: None,
            request: None,
            indent: 0,
            job: None,
            compact_after: false,
            document: None,
        };
        let rows = layout(std::slice::from_ref(&entry), 30, Palette::new(false), None);
        let start = rows
            .iter()
            .position(|row| row.text().starts_with("  first"))
            .unwrap();
        let end = rows
            .iter()
            .rposition(|row| row.text().ends_with("second line  "))
            .unwrap();
        let selection = (
            TextPosition {
                row: start,
                byte: 2,
            },
            TextPosition {
                row: end,
                byte: rows[end].text().len(),
            },
        );
        let mut blocks = RowBlocks::default();
        *blocks.block_mut(0) = rows.clone();
        blocks.finish_update(0);
        assert_eq!(
            selected_text(&blocks, selection),
            source.strip_prefix("  ").unwrap()
        );
        assert_eq!(
            selected_text(&blocks, (selection.1, selection.0)),
            source.strip_prefix("  ").unwrap()
        );
        let mut appended = entry.clone();
        appended.text.push_str("\nLater streaming text");
        let updated = layout(&[appended], 30, Palette::new(false), None);
        assert!(selection_unchanged(
            &rows[start..=end],
            updated.iter().skip(start),
            selection
        ));
        let mut changed = entry;
        changed.text = changed.text.replace("first", "other");
        let updated = layout(&[changed], 30, Palette::new(false), None);
        assert!(!selection_unchanged(
            &rows[start..=end],
            updated.iter().skip(start),
            selection
        ));
    }

    #[test]
    fn document_and_plain_fallback_match_reference_layout() {
        let highlights = super::super::tool_view::HighlightCache::default();
        let mut document = super::super::tool_view::Document::default();
        document.arguments("exec", &serde_json::json!({"argv": ["echo", "界 hello"]}));
        for surface in [
            Surface::Tool,
            Surface::Muted,
            Surface::User,
            Surface::Agent,
            Surface::Reasoning,
        ] {
            for document in [None, Some(document.clone())] {
                if document.is_none()
                    && matches!(surface, Surface::User | Surface::Agent | Surface::Reasoning)
                {
                    continue;
                }
                let entry = model::Entry {
                    key: "fallback".into(),
                    text: "plain 界 text\nnext\n".into(),
                    surface,
                    indent: 5,
                    expandable: true,
                    default_open: true,
                    running: false,
                    footer: None,
                    request: None,
                    document,
                    job: None,
                    compact_after: false,
                };
                for light in [false, true] {
                    for width in [0, 1, 6, 30, 80] {
                        let p = Palette::new(light);
                        let actual = layout_document_or_plain(&entry, width, p, &highlights, 0);
                        let expected =
                            layout(std::slice::from_ref(&entry), width, p, Some(&highlights));
                        assert_eq!(actual.len(), expected.len());
                        for (actual, expected) in actual.iter().zip(&expected) {
                            assert_eq!(actual.line, expected.line);
                            assert_eq!(
                                (
                                    actual.header,
                                    actual.x,
                                    actual.width,
                                    actual.surface,
                                    actual.entry,
                                    actual.blank,
                                    actual.continued
                                ),
                                (
                                    expected.header,
                                    expected.x,
                                    expected.width,
                                    expected.surface,
                                    expected.entry,
                                    expected.blank,
                                    expected.continued
                                )
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn incremental_entry_rows_match_reference_layout() {
        let corpus = [
            "A plain sentence with Unicode 界 and emoji 👩‍💻.\nAnother line.\n\nNext paragraph.",
            "# Title\n\n**bold** and *italic* then `code`.\n\n```rust\nlet n = 4;\n```\n\n- list\n- next",
            "Text [reference][id].\n\n[id]: https://example.com\n",
            "a\u{1b}[31mred\u{1b}[0m\nnext\rline\twide",
            "hello  \nworld\n\nparagraph",
            "```\n```\n\nhello",
            "```\n```\n\n# Heading",
        ];
        let highlights = super::super::tool_view::HighlightCache::default();
        for text in corpus {
            for width in [6, 7, 30, 80] {
                for (surface, expandable) in [
                    (Surface::Reasoning, false),
                    (Surface::Reasoning, true),
                    (Surface::Agent, false),
                    (Surface::User, false),
                ] {
                    let mut entry = model::Entry {
                        key: "stream".into(),
                        text: if expandable || surface != Surface::Reasoning {
                            "Title\n".into()
                        } else {
                            String::new()
                        },
                        surface,
                        indent: 0,
                        expandable,
                        default_open: true,
                        running: true,
                        footer: None,
                        request: None,
                        document: None,
                        job: None,
                        compact_after: false,
                    };
                    let mut rows = Vec::new();
                    let mut cache = CachedEntry::default();
                    let p = Palette::new(false);
                    update_entry_rows(
                        &mut rows,
                        &mut cache,
                        &entry,
                        0,
                        EntryLayout {
                            width,
                            palette: p,
                            highlights: &highlights,
                            request_columns: RequestColumns::default(),
                        },
                        None,
                    );
                    for ch in text.chars() {
                        let old_len = entry.text.len();
                        entry.text.push(ch);
                        update_entry_rows(
                            &mut rows,
                            &mut cache,
                            &entry,
                            0,
                            EntryLayout {
                                width,
                                palette: p,
                                highlights: &highlights,
                                request_columns: RequestColumns::default(),
                            },
                            Some(old_len),
                        );
                        let expected = layout(std::slice::from_ref(&entry), width, p, None);
                        assert_eq!(
                            rows.len(),
                            expected.len(),
                            "text={:?}, width={width}, expanded={expandable}",
                            entry.text
                        );
                        for (a, b) in rows.iter().zip(&expected) {
                            let styled = |line: &Line| {
                                line.spans
                                    .iter()
                                    .flat_map(|span| {
                                        span.content.chars().map(move |ch| (ch, span.style))
                                    })
                                    .collect::<Vec<_>>()
                            };
                            assert_eq!(
                                styled(&a.line),
                                styled(&b.line),
                                "text={:?}, width={width}, expanded={expandable}",
                                entry.text
                            );
                            assert_eq!(
                                (a.header, a.x, a.width, a.blank, a.continued),
                                (b.header, b.x, b.width, b.blank, b.continued)
                            );
                        }
                    }
                }
            }
        }
    }
}
