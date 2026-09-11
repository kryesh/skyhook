mod code;
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
    pub content: super::theme::ContentTheme,
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
}
impl Palette {
    pub fn new(light: bool) -> Self {
        if light {
            Self {
                content: super::theme::ContentTheme::new(light),
                base: Color::Rgb(245, 245, 245),
                panel: Color::Rgb(234, 234, 234),
                input: Color::Rgb(224, 224, 224),
                agent: Color::Rgb(232, 232, 232),
                user: Color::Rgb(218, 218, 218),
                fg: Color::Rgb(28, 31, 35),
                muted: Color::Rgb(85, 90, 98),
                selected: Color::Rgb(192, 192, 192),
                accent: super::theme::ContentTheme::new(light).primary,
                warning: super::theme::ContentTheme::new(light).warning,
            }
        } else {
            Self {
                content: super::theme::ContentTheme::new(light),
                base: Color::Rgb(0, 0, 0),
                panel: Color::Rgb(28, 28, 28),
                input: Color::Rgb(40, 40, 40),
                agent: Color::Rgb(34, 34, 34),
                user: Color::Rgb(44, 44, 44),
                fg: Color::Rgb(222, 225, 230),
                muted: Color::Rgb(146, 153, 163),
                selected: Color::Rgb(64, 64, 64),
                accent: super::theme::ContentTheme::new(light).primary,
                warning: super::theme::ContentTheme::new(light).warning,
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
            Surface::Muted | Surface::Status | Surface::Reasoning => self.content.muted,
            Surface::Error => self.content.error,
            _ => self.content.fg,
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
    layout: markdown::RowLayout,
    inset: u16,
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

/// Match App::toggle's defaults, including open reasoning and historical tools.
fn entry_expanded(entry: &model::Entry, view: &model::View, details: bool) -> bool {
    let all = details
        && (entry.job.is_some()
            || (view.tab == Tab::Conversation && entry.surface == Surface::Tool));
    entry.expandable && view.is_expanded(&entry.key, all || entry.default_open)
}

impl Row {
    fn background(
        &self,
        p: Palette,
        expanded: bool,
        content_edge: bool,
        interactive: bool,
        text_selected: bool,
    ) -> Color {
        // Expanded framing is persistent; text selection must not erase an edge.
        // Inside the body only the selected text range may change the background.
        if !self.blank
            && if expanded {
                content_edge
            } else {
                interactive && !text_selected
            }
        {
            p.selected
        } else {
            p.background(self.surface)
        }
    }

    pub fn text(&self) -> String {
        self.line.to_string()
    }
    fn paragraph_x(&self) -> u16 {
        self.x + self.inset + u16::from(matches!(self.surface, Surface::User | Surface::Agent)) * 2
    }
    fn source_column(&self, text: &str, byte: usize) -> usize {
        let prefix = self.layout.source_prefix.min(text.len());
        let leading = self.layout.prefix.width();
        if byte < prefix {
            leading
                + text[..byte]
                    .graphemes(true)
                    .map(|g| g.width())
                    .sum::<usize>()
                    .min(self.layout.source_prefix_width)
        } else {
            self.layout
                .code
                .map_or(leading + self.layout.source_prefix_width, |code| {
                    code.indent + code.padding
                })
                + text[prefix..byte]
                    .graphemes(true)
                    .map(|g| g.width())
                    .sum::<usize>()
        }
    }
    #[cfg(test)]
    fn text_x(&self) -> u16 {
        self.paragraph_x()
            .saturating_add(self.source_column(&self.text(), 0) as u16)
    }
    pub fn byte_at_column(&self, column: u16) -> usize {
        let target = column.saturating_sub(self.paragraph_x()) as usize;
        let text = self.text();
        let mut column = self.layout.prefix.width();
        for (byte, grapheme) in text.grapheme_indices(true) {
            if byte == self.layout.source_prefix {
                column = self.layout.code.map_or(
                    self.layout.prefix.width() + self.layout.source_prefix_width,
                    |code| code.indent + code.padding,
                );
            }
            let clipped_prefix = byte < self.layout.source_prefix
                && column + grapheme.width()
                    > self.layout.prefix.width() + self.layout.source_prefix_width;
            if !clipped_prefix && target < column + grapheme.width() {
                return byte;
            }
            column += grapheme.width();
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
        if !self.selectable
            || self.blank
            || self.layout.decorative
            || start == end
            || row < start.row
            || row > end.row
        {
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
    fences: code::Fences,
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
        app.render.entries.clear();
        dirty = (0..app.entries.len()).collect();
    }
    if tab == Tab::Requests
        && (reset || !dirty.is_empty() || app.render.rows.entry_count() != app.entries.len())
    {
        app.render.request_columns.update(&app.entries, &mut dirty);
    }
    // Fence sources join tool documents in the same bounded asynchronous cache.
    // Retain committed blocks on appends rather than rehashing previous fences.
    for &index in &dirty {
        let Some(entry) = app.entries.get(index) else {
            continue;
        };
        if entry.document.is_none()
            && matches!(
                entry.surface,
                Surface::User | Surface::Agent | Surface::Reasoning
            )
        {
            let cached = app.render.entries.entry(entry.key.clone()).or_default();
            update_markdown_fences(
                entry,
                &mut cached.fences,
                (!reset)
                    .then(|| app.render.changes.appends.get(&index).copied())
                    .flatten(),
            );
        }
    }
    // Only changed documents are prepared on ordinary content updates.
    let selected = app.views.get(&app.selected).map_or(0, |view| view.row);
    if reset || content_changed || !dirty.is_empty() {
        let documents = std::iter::once(selected)
            .chain(dirty.iter().copied())
            .filter_map(|i| app.entries.get(i));
        app.render.highlights.prepare(
            documents.flat_map(|entry| {
                entry.document.iter().chain(
                    app.render
                        .entries
                        .get(&entry.key)
                        .map(|cached| &cached.fences.document),
                )
            }),
            app.light,
        );
    }
    let highlighted_sources = app.render.highlights.take_changed_sources();
    let mut highlighted_entries = Vec::new();
    app.render
        .rows
        .highlight_entries(&highlighted_sources, &mut highlighted_entries);
    dirty.extend(highlighted_entries.iter().copied());
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
    }
    for index in dirty {
        let Some(entry) = app.entries.get(index) else {
            continue;
        };
        let cached = app.render.entries.entry(entry.key.clone()).or_default();
        let append_from = (!reset
            && !highlighted_entries.contains(&index)
            && cached.width == width
            && cached.light == app.light)
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
                expanded: app
                    .views
                    .get(&app.selected)
                    .is_some_and(|view| entry_expanded(entry, view, app.details)),
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
                .iter()
                .flat_map(|doc| doc.highlight_sources())
                .chain(cached.fences.document.highlight_sources())
                .collect(),
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
            expanded && app.render.rows.is_content_edge(row),
            (focused && entry.is_some_and(|e| e.expandable)) || hovered,
            selected.is_some(),
        );
        fill(frame, rect, bg);
        if expanded && row.surface == Surface::Tool && !row.blank && !row.header && row.x < width {
            frame.buffer_mut()[(row.x, y)].set_bg(p.selected);
        }
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
        let hover = navigation_active && app.hover.is_some_and(|point| rect.contains(point.into()));
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
        let name = agent_identity(
            &agent.name,
            &target,
            if selected { "> " } else { "  " },
            Span::styled(
                symbol,
                Style::default().fg(agent_status_color(running, &status, p)),
            ),
            name_width,
            p,
        );
        text(frame, r(2 + indent, y, name_width, 1), name, p.fg, bg);
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
                agent_status_color(running, &status, p),
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
        if *index == app.prompt_choice
            && !cursor_drawn
            && app.menu.is_none()
            && !(app.multiple_questions() && app.question_editing)
        {
            focus_cursor(frame, row.x - 1, row.y, p.input);
            cursor_drawn = true;
        }
        app.hits.push((row, Hit::PromptChoice(*index)));
    }
    let secret = app.prompts.front().is_some_and(|prompt| prompt.secret());
    let input = if secret {
        "●".repeat(app.prompt_editor.text.graphemes(true).count())
    } else {
        if app.multiple_questions() {
            if app.question_editing {
                let cursor = app.prompt_editor.cursor;
                format!(
                    "{}▏{}",
                    model::clean(&app.prompt_editor.text[..cursor]),
                    model::clean(&app.prompt_editor.text[cursor..])
                )
            } else {
                model::clean(&app.prompt_editor.text)
            }
        } else {
            format!("{}▏", model::clean(&app.prompt_editor.text))
        }
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
        format!("{input_label}{input}{}", if secret { "▏" } else { "" }),
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
        match app.prompts.front().map(|prompt| &prompt.kind) {
            Some(crate::interaction::PromptKind::Questions { questions, .. })
                if questions.len() > 1 && app.question_index < questions.len() =>
            {
                format!(
                    "Question {}/{} · {} · ↑↓ choose · Enter answer · Esc dismiss",
                    app.question_index + 1,
                    questions.len(),
                    if app.question_editing {
                        "←→ cursor · Tab switch questions"
                    } else {
                        "←→ switch · Tab edit"
                    }
                )
            }
            _ => "↑↓ choose · PgUp/PgDn text · Ctrl+PgUp/PgDn choices · Enter submit · Esc dismiss"
                .into(),
        },
        p.muted,
        p.input,
    );
}
/// Numeric fields share their own right edge, not just the footer's right edge.
/// Measure all agents, including off-screen rows, to keep columns stable on scroll.
struct AgentStatsColumns([usize; 3]);

const AGENT_STATS_HEADERS: [&str; 3] = ["Output", "Input (uncached)", "Context"];

impl AgentStatsColumns {
    fn menu<'a>(rows: impl IntoIterator<Item = &'a [String; 3]>, width: u16) -> Self {
        let mut columns = Self::new(rows);
        for (column, header) in columns.0.iter_mut().zip(AGENT_STATS_HEADERS) {
            *column = (*column).max(header.width());
        }
        // Keep all three columns visible on narrow terminals. Headers and values
        // wrap within the same columns instead of clipping away token fields.
        let available = usize::from(width.saturating_sub(6));
        while columns.0.iter().sum::<usize>() > available {
            let largest = (0..3).max_by_key(|&index| columns.0[index]).unwrap();
            columns.0[largest] = columns.0[largest].saturating_sub(1);
        }
        columns
    }

    fn wrapped(value: &str, width: usize) -> Vec<String> {
        wrap_words(Line::from(value.to_owned()), width.max(1))
            .into_iter()
            .map(|line| line.to_string().trim_end().to_owned())
            .collect()
    }

    fn wrapped_height(&self, row: &[String; 3]) -> usize {
        row.iter()
            .zip(self.0)
            .map(|(value, width)| Self::wrapped(value, width).len())
            .max()
            .unwrap_or(1)
    }

    fn draw(&self, frame: &mut Frame, rect: Rect, row: &[String; 3], fg: Color, bg: Color) {
        let mut x = rect.x;
        for (value, width) in row.iter().zip(self.0) {
            for (line, value) in Self::wrapped(value, width).into_iter().enumerate() {
                if line >= usize::from(rect.height) {
                    break;
                }
                text(
                    frame,
                    r(x, rect.y + line as u16, width as u16, 1),
                    format!("{}{value}", " ".repeat(width.saturating_sub(value.width()))),
                    fg,
                    bg,
                );
            }
            x += width as u16 + 3;
        }
    }

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
            .collect::<Vec<_>>();
        let status_start = metadata[..3].iter().map(String::len).sum::<usize>() + " · ".len() * 3;
        let title_end = metadata[0].len();
        let metadata = clipped_header(&metadata.join(" · "), left_width);
        let gap = width as usize - metadata.width();
        let mut spans = vec![Span::raw(" ".repeat(gutter as usize))];
        if metadata.is_char_boundary(title_end) && title_end <= metadata.len() {
            spans.push(Span::styled(
                metadata[..title_end].to_owned(),
                Style::default().fg(p.content.primary),
            ));
            if metadata.is_char_boundary(status_start) && status_start <= metadata.len() {
                spans.push(Span::raw(metadata[title_end..status_start].to_owned()));
                let color = match row.status {
                    "Completed" => p.content.success,
                    "Failed" => p.content.error,
                    "Interrupted" => p.content.warning,
                    "Running" => p.content.info,
                    _ => p.content.muted,
                };
                spans.push(Span::styled(
                    metadata[status_start..].to_owned(),
                    Style::default().fg(color),
                ));
            } else {
                spans.push(Span::raw(metadata[title_end..].to_owned()));
            }
        } else {
            spans.push(Span::styled(
                metadata,
                Style::default().fg(p.content.primary),
            ));
        }
        if show_statistics {
            spans.push(Span::raw(" ".repeat(gap - statistics.width())));
            spans.push(Span::styled(
                statistics,
                Style::default().fg(p.content.muted),
            ));
        }
        Line::from(spans)
    }
}

fn agent_status_color(running: bool, status: &str, p: Palette) -> Color {
    if status == "Failed" {
        p.content.error
    } else if status.starts_with("Waiting") || status.starts_with("Reconnecting") {
        p.content.warning
    } else if running {
        p.content.primary
    } else if status == "Completed" {
        p.content.success
    } else {
        p.muted
    }
}

/// Shared by the inline tree and agent inspector. Selection only changes the
/// neutral surface, not the independent identity, target, and state roles.
fn agent_identity(
    name: &str,
    target: &str,
    prefix: &str,
    symbol: Span<'static>,
    width: u16,
    p: Palette,
) -> Line<'static> {
    let available = width.saturating_sub((prefix.width() + symbol.width() + 1) as u16);
    let target = model::clean(target);
    let target_width = (target.width() as u16).min(available / 2);
    Line::from(vec![
        Span::styled(prefix.to_owned(), Style::default().fg(p.accent)),
        symbol,
        Span::raw(" "),
        Span::styled(
            clipped_header(&model::clean(name), available.saturating_sub(target_width)),
            Style::default().fg(p.fg),
        ),
        Span::styled(
            clipped_header(&target, target_width),
            Style::default().fg(p.content.accent),
        ),
    ])
}

fn agent_symbol(running: bool, status: &str, terminal: bool, tick: usize) -> &'static str {
    if running {
        spinner(tick)
    } else if status == "Failed" {
        "✗"
    } else if matches!(status, "Cancelled" | "Interrupted") {
        "■"
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
fn draw_menu_item(
    frame: &mut Frame,
    row: Rect,
    item: &super::app::Item,
    kind: &MenuKind,
    selected: bool,
    p: Palette,
    bg: Color,
) {
    let label = model::clean(&item.label);
    let detail = model::clean(&item.detail);
    let fg = if selected && !matches!(kind, MenuKind::Info | MenuKind::Output(_)) {
        p.content.primary
    } else {
        p.fg
    };
    if matches!(kind, MenuKind::Commands) {
        // Paint the whole row, including the gap between label and shortcut.
        fill(frame, row, bg);
        // Never sacrifice label space for a shortcut. Visible hints share the
        // right edge; an overlong/custom hint is hidden as a whole.
        let hint_width = detail.width();
        let show_hint = hint_width > 0 && label.width() + 2 + hint_width <= row.width as usize;
        let label_width = if show_hint {
            row.width.saturating_sub(hint_width as u16 + 2)
        } else {
            row.width
        };
        text(frame, r(row.x, row.y, label_width, 1), label, fg, bg);
        if show_hint {
            text(
                frame,
                r(row.right() - hint_width as u16, row.y, hint_width as u16, 1),
                detail,
                p.muted,
                bg,
            );
        }
    } else {
        let line = Line::from(vec![
            Span::styled(label, Style::default().fg(fg)),
            Span::styled(
                if detail.is_empty() {
                    detail
                } else {
                    format!("   {detail}")
                },
                Style::default().fg(p.muted),
            ),
        ]);
        text(frame, row, line, fg, bg);
    }
}

fn draw_menu(frame: &mut Frame, app: &mut App, p: Palette) {
    app.refresh_agent_menu();
    let Some(menu) = &app.menu else { return };
    let area = app.content_rect;
    // Leave enough room for whole header words before spending space on margins.
    let minimum_width = if matches!(menu.kind, MenuKind::Agents) {
        31
    } else {
        20
    };
    let margin = 7.min(area.width.saturating_sub(minimum_width) / 2);
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
    let stats_columns = AgentStatsColumns::menu(agent_stats.iter(), width.saturating_sub(2));
    let stats_width = stats_columns.width();
    let headers = AGENT_STATS_HEADERS.map(String::from);
    let header_height = if agent_menu {
        stats_columns.wrapped_height(&headers) as u16
    } else {
        0
    };
    let stats_height = agent_stats
        .iter()
        .map(|row| stats_columns.wrapped_height(row))
        .max()
        .unwrap_or(1);
    // Match the inline tree's aligned status/token columns. On narrow screens,
    // use additional lines so the picker still exposes status and every token field.
    let compact_agents = agent_menu && width.saturating_sub(2) < stats_width + 50;
    let stacked_stats = compact_agents && width.saturating_sub(2) < stats_width + 30;
    let row_height = if stacked_stats {
        2 + stats_height
    } else if compact_agents {
        1 + stats_height
    } else {
        1
    };
    if agent_menu && stats_width <= width.saturating_sub(2) {
        stats_columns.draw(
            frame,
            r(
                rect.right() - 1 - stats_width,
                rect.y + 2,
                stats_width,
                header_height.min(rect.height.saturating_sub(2)),
            ),
            &headers,
            p.muted,
            p.input,
        );
    }
    let height = rect.height.saturating_sub(3 + header_height) as usize / row_height;
    let top = menu.selected.saturating_sub(height.saturating_sub(1));
    for (i, item) in items.iter().enumerate().skip(top).take(height) {
        let y = rect.y + 2 + header_height + ((i - top) * row_height) as u16;
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
                let stats = model::agent_footer_stats(&app.snapshot, &app.projection, &agent.id);
                let target = model::target_suffix(&agent.target);
                let indent = (agent.id.depth() as u16 * 4).min(row.width / 3);
                let name_width = if compact_agents {
                    row.width
                } else {
                    row.width.saturating_sub(stats_width + 32)
                };
                let name = agent_identity(
                    &agent.name,
                    &target,
                    &" ".repeat(indent as usize),
                    Span::styled(
                        symbol,
                        Style::default().fg(agent_status_color(running, &status, p)),
                    ),
                    name_width,
                    p,
                );
                fill(frame, r(row.x, row.y, row.width, row_height as u16), bg);
                text(frame, r(row.x, y, name_width, 1), name, p.fg, bg);
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
                    agent_status_color(running, &status, p),
                    bg,
                );
                if stats_width <= row.width {
                    stats_columns.draw(
                        frame,
                        r(
                            row.right() - stats_width,
                            status_row.y + u16::from(stacked_stats),
                            stats_width,
                            stats_height as u16,
                        ),
                        &stats,
                        p.muted,
                        bg,
                    );
                }
            }
        } else {
            draw_menu_item(frame, row, item, &menu.kind, selected, p, bg);
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
            r(
                rect.x + 1,
                rect.y + 2 + header_height,
                width.saturating_sub(2),
                1,
            ),
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
    expanded: bool,
}

/// Markdown layout and syntax metadata must see the same source: generated
/// message/reasoning titles are not part of the Markdown body. Offsets are in
/// the original UTF-8 entry text so append positions can be translated safely.
fn markdown_body_offset(entry: &model::Entry) -> usize {
    if matches!(entry.surface, Surface::User | Surface::Agent) || entry.expandable {
        entry
            .text
            .find('\n')
            .map_or(entry.text.len(), |end| end + 1)
    } else {
        0
    }
}

fn update_markdown_fences(
    entry: &model::Entry,
    fences: &mut code::Fences,
    append_from: Option<usize>,
) {
    let offset = markdown_body_offset(entry);
    fences.update(
        &entry.text[offset..],
        append_from.and_then(|from| from.checked_sub(offset)),
    );
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
        palette: mut p,
        highlights,
        request_columns,
        expanded,
    } = settings;
    if entry.surface == Surface::Reasoning {
        p.content.fg = p.content.muted;
    }
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
        let body_offset = markdown_body_offset(entry);
        // A growing title (before its first newline) changes the body boundary;
        // it cannot reuse the previously laid-out body as an append.
        let append_from = append_from.filter(|_| cached.body_offset == body_offset);
        if append_from.is_none() {
            rows.clear();
            if block {
                rows.push(blank(entry.surface, x, block_width));
            }
            cached.body_offset = body_offset;
            if has_title {
                let (title, _) = entry.text.split_once('\n').unwrap_or((&entry.text, ""));
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
        let suffix = cached.stream.update_highlighted(
            body,
            body_width as usize,
            p,
            body_append,
            prefix,
            Some(highlights),
        );
        if let Some((truncate, suffix)) = suffix {
            rows.truncate(cached.title_rows + truncate);
            // Expanded reasoning omits the empty body; response blocks retain it.
            if !entry.expandable || !body.is_empty() || block {
                for line in suffix {
                    let header = rows.is_empty();
                    let mut row = make_row(line.line, header, line.continued);
                    row.layout = line.layout;
                    rows.push(row);
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
        *rows =
            layout_document_or_plain_with_expansion(entry, width, p, highlights, index, expanded);
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
    tool_gutter: bool,
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
            tool_gutter: false,
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
            layout: markdown::RowLayout::default(),
            inset: u16::from(self.tool_gutter && !header),
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
            layout: markdown::RowLayout::default(),
            inset: 0,
        }
    }
}

/// Non-streaming fallback is entry-local and handles only documents or plain text.
#[cfg(test)]
fn layout_document_or_plain(
    entry: &model::Entry,
    width: u16,
    p: Palette,
    highlights: &super::tool_view::HighlightCache,
    index: usize,
) -> Vec<Row> {
    layout_document_or_plain_with_expansion(entry, width, p, highlights, index, entry.default_open)
}

fn layout_document_or_plain_with_expansion(
    entry: &model::Entry,
    width: u16,
    p: Palette,
    highlights: &super::tool_view::HighlightCache,
    index: usize,
    expanded: bool,
) -> Vec<Row> {
    let mut geometry = EntryGeometry::new(entry, width, index);
    geometry.tool_gutter = expanded && entry.expandable && entry.surface == Surface::Tool;
    let block = matches!(entry.surface, Surface::User | Surface::Agent);
    let lines = if let Some(document) = &entry.document {
        document.layout_lines(Some(highlights), p.content.light)
    } else {
        debug_assert!(!block && entry.surface != Surface::Reasoning);
        model::clean(&entry.text)
            .split('\n')
            .map(|s| (Line::from(s.to_owned()), super::tool_view::Wrap::Hard))
            .collect()
    };
    let mut rows = Vec::new();
    if block {
        rows.push(geometry.blank(entry.surface, geometry.x, geometry.block_width));
    }
    let mut header = true;
    for (line_index, (mut line, wrapping)) in lines.into_iter().enumerate() {
        if line_index == 0 && entry.surface == Surface::Tool {
            if let Some(header) = &entry.header {
                line = super::tool_view::header_line(header, p.content.light);
            } else {
                for span in &mut line.spans {
                    span.style.fg = Some(p.content.fg);
                }
            }
        }
        let body_width = geometry
            .body_width
            .saturating_sub(u16::from(geometry.tool_gutter))
            .max(1) as usize;
        let first_width = if line_index == 0 {
            geometry.body_width as usize
        } else {
            body_width
        };
        let wrapped = match wrapping {
            super::tool_view::Wrap::Hard => wrap_line_widths(line, first_width, body_width),
            super::tool_view::Wrap::Words => wrap_words(
                line,
                if line_index == 0 {
                    first_width
                } else {
                    body_width
                },
            ),
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
    rows
}

/// Borrow cached spans while matching Paragraph's unwrapped clipping and styling.
/// Unlike Line's Widget implementation, Paragraph leaves wide continuation cells
/// in the surface style and skips individual graphemes wider than the viewport.
/// Paint Markdown geometry without inserting decorative text into the source.
fn render_row_line(row: &Row, area: Rect, buffer: &mut Buffer, style: Style, p: Palette) {
    if row.layout == markdown::RowLayout::default() {
        render_line(&row.line, area, buffer, style);
        return;
    }
    let area = area.intersection(buffer.area);
    if area.is_empty() {
        return;
    }
    buffer.set_style(area, style.patch(row.line.style));
    let prefix_width = row.layout.prefix.width().min(area.width as usize);
    if prefix_width > 0 {
        render_line(
            &row.layout.prefix,
            r(area.x, area.y, prefix_width as u16, 1),
            buffer,
            style,
        );
    }
    if let Some(code) = row.layout.code {
        let offset = code.indent;
        let rect = r(
            area.x.saturating_add(offset as u16),
            area.y,
            code.width as u16,
            1,
        )
        .intersection(area);
        buffer.set_style(rect, Style::default().bg(p.content.code_bg));
    }
    if row.layout.decorative {
        return;
    }
    let mut byte = 0;
    let mut column = prefix_width;
    for grapheme in row.line.styled_graphemes(Style::default()) {
        if byte == row.layout.source_prefix {
            column = row
                .layout
                .code
                .map_or(prefix_width + row.layout.source_prefix_width, |code| {
                    code.indent + code.padding
                });
        }
        let size = grapheme.symbol.width();
        let in_prefix = byte < row.layout.source_prefix;
        let prefix_clipped =
            in_prefix && column + size > prefix_width + row.layout.source_prefix_width;
        let body_end = row.layout.code.map_or(area.width as usize, |code| {
            (code.indent + code.width - code.padding).min(area.width as usize)
        });
        if !in_prefix && column + size > body_end {
            break;
        }
        if size > 0 && !prefix_clipped && column + size <= area.width as usize {
            buffer[(area.x + column as u16, area.y)]
                .set_symbol(grapheme.symbol)
                .set_style(grapheme.style);
        }
        byte += grapheme.symbol.len();
        column += size;
    }
}

fn render_line(line: &Line<'_>, area: Rect, buffer: &mut Buffer, style: Style) {
    let area = area.intersection(buffer.area);
    if area.is_empty() {
        return;
    }
    // Paragraph applies line style to glyphs, not continuation/trailing cells.
    // Markdown block backgrounds are painted separately from explicit geometry.
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
fn wrap_line(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    wrap_line_widths(line, width, width)
}

fn wrap_line_widths(
    line: Line<'static>,
    width: usize,
    continuation_width: usize,
) -> Vec<Line<'static>> {
    let mut width = width.max(1);
    let template = Line {
        style: line.style,
        alignment: line.alignment,
        ..Line::default()
    };
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
                result.push(Line {
                    spans: std::mem::take(&mut spans),
                    ..template.clone()
                });
                width = continuation_width.max(1);
                used = 0;
            }
            chunk.push_str(grapheme);
            used += size;
        }
        if !chunk.is_empty() {
            spans.push(Span::styled(chunk, span.style));
        }
    }
    result.push(Line { spans, ..template });
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
    let template = Line {
        style: line.style,
        alignment: line.alignment,
        ..Line::default()
    };
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
    let mut row = template.clone();
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
            result.push(std::mem::replace(&mut row, template.clone()));
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
fn markdown(text: &str, p: Palette, width: usize) -> Vec<Line<'static>> {
    markdown::render(text, p, true, width)
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

    fn markdown_rows(
        input: &str,
        width: u16,
        p: Palette,
        cache: Option<&super::super::tool_view::HighlightCache>,
    ) -> Vec<Row> {
        markdown::layout_highlighted(input, p, false, width as usize, width as usize, "", cache)
            .into_iter()
            .map(|line| Row {
                line: std::sync::Arc::new(line.line),
                header: false,
                x: 1,
                width,
                surface: Surface::Tool,
                entry: 0,
                selectable: true,
                blank: false,
                continued: line.continued,
                layout: line.layout,
                inset: 0,
            })
            .collect()
    }

    fn copy_rows(rows: &[Row]) -> String {
        let mut blocks = RowBlocks::default();
        *blocks.block_mut(0) = rows.to_vec();
        blocks.finish_update(0);
        selected_text(
            &blocks,
            (
                TextPosition { row: 0, byte: 0 },
                TextPosition {
                    row: rows.len() - 1,
                    byte: usize::MAX,
                },
            ),
        )
    }

    fn assert_code_geometry(rows: &[Row], p: Palette, width: u16) {
        for row in rows {
            let mut buffer = Buffer::empty(Rect::new(0, 0, width + 2, 1));
            buffer.set_style(buffer.area, Style::default().bg(p.agent));
            render_row_line(
                row,
                Rect::new(1, 0, width, 1),
                &mut buffer,
                Style::default().fg(p.fg).bg(p.agent),
                p,
            );
            let left = 1 + row.layout.code.map_or(0, |code| code.indent);
            for x in 0..width + 2 {
                let code = row.layout.code.is_some_and(|code| {
                    (left..(left + code.width).min(width as usize + 1)).contains(&(x as usize))
                });
                assert_eq!(
                    buffer[(x, 0)].bg,
                    if code { p.content.code_bg } else { p.agent },
                    "row={:?}, x={x}",
                    row.text()
                );
            }
            if row.layout.decorative {
                assert!(row.text().is_empty());
                assert!(
                    row.selection_range(
                        0,
                        Some((
                            TextPosition { row: 0, byte: 0 },
                            TextPosition { row: 1, byte: 0 }
                        ))
                    )
                    .is_none()
                );
            }
        }
    }

    #[test]
    fn markdown_code_geometry_fits_pads_wraps_and_preserves_copy() {
        for light in [false, true] {
            let p = Palette::new(light);
            for input in [
                "```\n  alpha beta gamma delta  \n\n    \nlast\n```",
                "```unknown\n  alpha beta gamma delta  \n\n    \nlast\n```",
                "    alpha beta gamma delta  \n\n    last",
                "```\nlast",
                "```\n界 👩‍💻 لا\n```",
                "> ```\n> short  \n> \n> last\n> ```",
                "- ```\n  short\n  last\n  ```",
                "```\na\n```\n\n```\nlonger\n```",
            ] {
                let original = markdown(input, p, 80)
                    .iter()
                    .map(Line::to_string)
                    .collect::<Vec<_>>()
                    .join("\n");
                for width in [1, 2, 3, 7, 80] {
                    let rows = markdown_rows(input, width, p, None);
                    assert_code_geometry(&rows, p, width);
                    assert_eq!(copy_rows(&rows), original, "input {input:?}, width {width}");
                    let code_rows = rows
                        .iter()
                        .filter(|row| row.layout.code.is_some())
                        .collect::<Vec<_>>();
                    assert!(code_rows.len() >= 3);
                    assert!(code_rows[0].layout.decorative);
                    assert!(code_rows.last().unwrap().layout.decorative);
                    for row in code_rows.iter().filter(|row| !row.layout.decorative) {
                        assert!(row.line.style.bg.is_none());
                        if row.layout.source_prefix == 0 && row.layout.prefix.width() == 0 {
                            let pad = row.layout.code.unwrap().padding;
                            assert_eq!(row.text_x(), 1 + pad as u16);
                            assert_eq!(row.byte_at_column(row.text_x()), 0);
                        }
                    }
                }
            }
            let rows = markdown_rows(
                "before `inline` after\n\n```\nabc\n\nx\n```\n\nafter",
                40,
                p,
                None,
            );
            let code = rows
                .iter()
                .filter_map(|row| row.layout.code)
                .collect::<Vec<_>>();
            assert_eq!(code.len(), 5); // three source rows plus top/bottom
            assert!(code.iter().all(|code| code.width == 5 && code.padding == 1));
            assert_code_geometry(&rows, p, 40);
            assert!(rows.first().unwrap().layout.code.is_none());
            assert!(rows.last().unwrap().layout.code.is_none());
            assert_eq!(copy_rows(&rows), "before inline after\n\nabc\n\nx\n\nafter");
        }
    }

    #[test]
    fn markdown_code_geometry_survives_highlight_arrival_without_styling_tools() {
        use super::super::tool_view::{Document, HighlightCache, Role, Section};
        let source = "let answer = 42;  \n\n// a sufficiently long comment to wrap\n";
        let input = format!("```rust\n{source}```");
        let document = Document {
            sections: vec![Section::Code {
                source: source.into(),
                language: "rust".into(),
                indent: 0,
                gutters: Vec::new(),
                role: Role::Constant,
            }],
        };
        for light in [false, true] {
            let p = Palette::new(light);
            let mut cache = HighlightCache::default();
            let pending = markdown_rows(&input, 80, p, Some(&cache));
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                cache.prepare(std::iter::once(&document), light);
                if cache.is_fully_highlighted(&document, light) {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "highlight worker did not finish"
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            let highlighted = markdown_rows(&input, 80, p, Some(&cache));
            assert_eq!(copy_rows(&pending), copy_rows(&highlighted));
            assert_eq!(
                pending.iter().map(|row| &row.layout).collect::<Vec<_>>(),
                highlighted
                    .iter()
                    .map(|row| &row.layout)
                    .collect::<Vec<_>>()
            );
            assert_ne!(pending[1].line.spans, highlighted[1].line.spans);
            for width in [1, 5, 80] {
                assert_code_geometry(&markdown_rows(&input, width, p, Some(&cache)), p, width);
            }
            let tool_lines = document.lines(Some(&cache), light);
            assert!(tool_lines.iter().all(|line| line.style.bg.is_none()));
            assert!(
                tool_lines
                    .iter()
                    .flat_map(|line| &line.spans)
                    .all(|span| span.style.bg.is_none())
            );
            for (markdown, tool) in highlighted
                .iter()
                .filter(|row| !row.layout.decorative)
                .zip(tool_lines)
            {
                assert_eq!(
                    markdown
                        .line
                        .spans
                        .iter()
                        .filter(|span| !span.content.is_empty())
                        .collect::<Vec<_>>(),
                    tool.spans
                        .iter()
                        .filter(|span| !span.content.is_empty())
                        .collect::<Vec<_>>()
                );
            }
        }
    }

    #[test]
    fn markdown_hanging_prefixes_are_decorative_and_preserve_copy() {
        for light in [false, true] {
            let p = Palette::new(light);
            for input in [
                "- alpha bravo charlie delta echo foxtrot",
                "10. alpha bravo charlie delta echo foxtrot",
                "- [ ] alpha bravo charlie delta echo foxtrot",
                "> alpha bravo charlie delta echo foxtrot",
                "> - alpha bravo charlie delta echo foxtrot",
                "1. outer\n   - inner alpha bravo charlie delta echo foxtrot",
            ] {
                let original = markdown(input, p, 80)
                    .iter()
                    .map(Line::to_string)
                    .collect::<Vec<_>>()
                    .join("\n");
                for width in [8, 16, 24] {
                    let rows = markdown_rows(input, width, p, None);
                    assert_eq!(copy_rows(&rows), original, "{input:?}, width {width}");
                    for row in rows.iter().filter(|row| row.continued) {
                        assert!(
                            row.layout.prefix.width() >= 2,
                            "missing hanging prefix for {input:?}"
                        );
                        assert_eq!(row.byte_at_column(row.text_x()), 0);
                        assert!(row.text_x() >= row.x + 2);
                    }
                }
            }
        }
    }

    fn expandable_entry() -> model::Entry {
        model::Entry {
            key: "entry".into(),
            text:
                "first header with many wrapped fragments\nbody with many wrapped fragments\n  \n"
                    .into(),
            surface: Surface::Tool,
            expandable: true,
            default_open: false,
            running: false,
            footer: None,
            request: None,
            indent: 0,
            job: None,
            compact_after: false,
            header: None,
            document: None,
        }
    }

    #[test]
    fn fence_metadata_uses_body_offsets_for_every_stream_append() {
        let body = "2. ```rust\n   let café = 42;\n   ```\n\nTail";
        for (surface, expandable, title) in [
            (Surface::Agent, false, "Agent [worker] 🦀\n"),
            (Surface::User, false, "User\n"),
            (Surface::Reasoning, true, "▾ Thinking\n"),
            (Surface::Reasoning, false, ""),
        ] {
            let full = format!("{title}{body}");
            let mut entry = expandable_entry();
            entry.surface = surface;
            entry.expandable = expandable;
            entry.text.clear();
            let mut fences = code::Fences::default();
            let mut previous = 0;
            for end in std::iter::once(0)
                .chain(full.char_indices().map(|(start, ch)| start + ch.len_utf8()))
            {
                entry.text = full[..end].to_owned();
                update_markdown_fences(&entry, &mut fences, Some(previous));
                let expected_body = if title.is_empty() {
                    &full[..end]
                } else if end < title.len() {
                    ""
                } else {
                    &full[title.len()..end]
                };
                let mut expected = code::Fences::default();
                expected.update(expected_body, None);
                assert_eq!(
                    fences.document, expected.document,
                    "title={title:?}, end={end}"
                );
                previous = end;
            }
            assert_eq!(fences.document.sections.len(), 1);
            // Replacements are not appends, even if the body has the same length.
            entry.text = full.replace("42", "43");
            update_markdown_fences(&entry, &mut fences, None);
            let mut expected = code::Fences::default();
            expected.update(&body.replace("42", "43"), None);
            assert_eq!(fences.document, expected.document);
        }
    }

    /// Check every logical line's policy as well as the copy contract. Reflow
    /// introduces neither copied indentation nor newlines at visual boundaries.
    fn assert_argument_reflow(
        document: &super::super::tool_view::Document,
        cache: &super::super::tool_view::HighlightCache,
        light: bool,
    ) {
        use super::super::tool_view::Wrap;
        let entry = model::Entry {
            key: "tool-wrap".into(),
            text: document.plain_text(),
            surface: Surface::Tool,
            expandable: true,
            default_open: true,
            running: false,
            footer: None,
            request: None,
            indent: 0,
            job: None,
            compact_after: true,
            header: None,
            document: Some(document.clone()),
        };
        for width in [0, 1, 2, 5, 12, 24, 40, 64, 120, 24] {
            let geometry = EntryGeometry::new(&entry, width, 0);
            let rows = layout_document_or_plain(&entry, width, Palette::new(light), cache, 0);
            let expected = document
                .layout_lines(Some(cache), light)
                .into_iter()
                .enumerate()
                .flat_map(|(index, (line, wrap))| {
                    let body = geometry.body_width.saturating_sub(1).max(1) as usize;
                    let first = if index == 0 {
                        geometry.body_width as usize
                    } else {
                        body
                    };
                    match wrap {
                        Wrap::Hard => wrap_line_widths(line, first, body),
                        Wrap::Words => wrap_words(line, first),
                    }
                })
                .collect::<Vec<_>>();
            assert_eq!(
                rows.iter().map(Row::text).collect::<Vec<_>>(),
                expected
                    .iter()
                    .map(|line| line
                        .spans
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect::<String>())
                    .collect::<Vec<_>>(),
                "width {width}, light {light}"
            );
            let selection = (
                TextPosition { row: 0, byte: 0 },
                TextPosition {
                    row: rows.len() - 1,
                    byte: rows.last().unwrap().text().len(),
                },
            );
            let mut blocks = RowBlocks::default();
            *blocks.block_mut(0) = rows;
            blocks.finish_update(0);
            assert_eq!(
                selected_text(&blocks, selection),
                entry.text,
                "width {width}"
            );
            assert_eq!(
                selected_text(&blocks, (selection.1, selection.0)),
                entry.text
            );
        }
    }

    #[test]
    fn tool_argument_prose_wraps_words_without_interpreting_or_losing_text() {
        use super::super::tool_view::{Document, HighlightCache, Role};
        let prompt = "Please inspect the current implementation and explain every relevant change. "
            .repeat(5)
            + "\n\n  Keep **literal** text, café e\u{301}lan 世界 👩‍💻 together.  \n";
        let args = serde_json::json!({
            "prompt": prompt,
            "description": "short inline prose",
            "nested": {"items": ["nested array prose", "first paragraph\n  second paragraph  "]},
            "token": "extraordinarilylongunbrokentoken世界👩‍💻e\u{301}",
        });
        let original = args.clone();
        let cache = HighlightCache::default();
        let mut document = Document::default();
        document.line("agent", Role::ToolName);
        document.arguments("agent", &args);
        for light in [false, true] {
            assert_argument_reflow(&document, &cache, light);
            // A long prompt's short words must never be split, even though its
            // paragraphs are longer than the viewport and include indentation.
            for (line, wrapping) in document.layout_lines(None, light) {
                if wrapping != super::super::tool_view::Wrap::Words {
                    continue;
                }
                let original = line
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>();
                if original.contains("extraordinarily") {
                    continue;
                }
                for width in [24, 40, 64] {
                    let parts = wrap_words(line.clone(), width);
                    let words = parts
                        .iter()
                        .flat_map(|part| {
                            part.spans
                                .iter()
                                .map(|span| span.content.as_ref())
                                .collect::<String>()
                                .split_whitespace()
                                .map(str::to_owned)
                                .collect::<Vec<_>>()
                        })
                        .collect::<Vec<_>>();
                    assert_eq!(words, original.split_whitespace().collect::<Vec<_>>());
                }
            }
        }
        assert_eq!(args, original);
    }

    #[test]
    fn tool_argument_code_keeps_hard_wrap_and_copy_after_highlights_and_resize() {
        use super::super::tool_view::{Document, HighlightCache, Role};
        let source = "  const message = 'long words stay hard wrapped';\t// café 👩‍💻\n\n    console.log(message);  \n";
        let cases = [
            (
                "script",
                serde_json::json!({"source": source, "description": "prose next to source"}),
            ),
            (
                "shell",
                serde_json::json!({"command": "  printf '%s  %s'  first second\t\n\n"}),
            ),
            (
                "exec",
                serde_json::json!({"argv": ["sh", "-c", "  echo long shell words  \n"], "nested": {"source": source}}),
            ),
            (
                "write",
                serde_json::json!({"path": "file.js", "content": source}),
            ),
            (
                "replace",
                serde_json::json!({"path": "file.js", "old": source, "new": "  replacement text  \n"}),
            ),
            (
                "patch",
                serde_json::json!({"patch": "@@ -1 +1 @@\n-  old words\n+  new words  \n"}),
            ),
        ];
        let mut cache = HighlightCache::default();
        for (tool, args) in cases {
            let mut document = Document::default();
            document.line(tool, Role::ToolName);
            document.arguments(tool, &args);
            let original = document.clone();
            for light in [false, true] {
                assert_argument_reflow(&document, &cache, light);
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                loop {
                    cache.prepare(std::iter::once(&document), light);
                    if document.highlight_sources().next().is_none()
                        || cache.is_fully_highlighted(&document, light)
                    {
                        break;
                    }
                    assert!(
                        std::time::Instant::now() < deadline,
                        "highlight worker did not finish"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                assert_argument_reflow(&document, &cache, light);
                assert_eq!(document, original);
            }
        }
    }

    #[test]
    fn prose_word_wrap_preserves_graphemes_styles_and_cross_span_words() {
        let line = Line::from(vec![
            Span::styled("pré", Style::default().fg(Color::Red)),
            Span::styled(
                "fix e\u{301}lan 👩‍💻 世界  fin  ",
                Style::default().fg(Color::Blue),
            ),
        ]);
        let graphemes = |lines: &[Line<'_>]| {
            lines
                .iter()
                .flat_map(|line| {
                    line.spans
                        .iter()
                        .flat_map(|span| {
                            span.content
                                .graphemes(true)
                                .map(|g| (g.to_owned(), span.style))
                                .collect::<Vec<_>>()
                        })
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>()
        };
        for width in [0, 1, 2, 3, 5, 7, 12, 40] {
            let parts = wrap_words(line.clone(), width);
            assert_eq!(graphemes(&parts), graphemes(std::slice::from_ref(&line)));
        }
        let words = wrap_words(line.clone(), 7)
            .iter()
            .map(|part| {
                part.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            words
                .iter()
                .flat_map(|part| part.split_whitespace())
                .collect::<Vec<_>>(),
            ["préfix", "e\u{301}lan", "👩‍💻", "世界", "fin"]
        );
    }

    #[test]
    fn expansion_matches_toggle_defaults_and_explicit_overrides() {
        let mut entry = expandable_entry();
        let mut view = model::View::default();
        assert!(!entry_expanded(&entry, &view, false));
        assert!(entry_expanded(&entry, &view, true));
        view.tab = Tab::Jobs;
        assert!(!entry_expanded(&entry, &view, true));
        entry.job = Some(skyhook::identity::JobId::new(1).unwrap());
        assert!(entry_expanded(&entry, &view, true));
        entry.job = None;
        entry.surface = Surface::Reasoning;
        entry.default_open = true;
        assert!(entry_expanded(&entry, &view, false));
        view.collapsed.insert(entry.key.clone());
        assert!(!entry_expanded(&entry, &view, true));
        view.expanded.insert(entry.key.clone());
        assert!(!entry_expanded(&entry, &view, true));
        view.collapsed.clear();
        entry.default_open = false;
        assert!(entry_expanded(&entry, &view, false));
        entry.expandable = false;
        assert!(!entry_expanded(&entry, &view, true));
    }

    #[test]
    fn expanded_backgrounds_frame_only_true_content_edges_even_with_selection() {
        for light in [false, true] {
            let p = Palette::new(light);
            let entry = expandable_entry();
            let mut rows = RowBlocks::default();
            *rows.block_mut(0) = layout(std::slice::from_ref(&entry), 20, p, None);
            rows.finish_update(0);
            let nonblank = rows
                .iter()
                .filter(|row| !row.text().trim().is_empty())
                .collect::<Vec<_>>();
            assert!(nonblank.len() > 4, "both header and body should wrap");
            for (index, row) in nonblank.iter().enumerate() {
                let edge = index == 0 || index + 1 == nonblank.len();
                assert_eq!(rows.is_content_edge(row), edge);
            }
            for row in rows.iter() {
                let edge = rows.is_content_edge(row);
                for interactive in [false, true] {
                    for text_selected in [false, true] {
                        assert_eq!(
                            row.background(p, true, edge, interactive, text_selected),
                            if edge {
                                p.selected
                            } else {
                                p.background(row.surface)
                            },
                            "row {:?}, interactive={interactive}, selection={text_selected}",
                            row.text()
                        );
                    }
                }
            }
            let row = nonblank[0];
            assert_eq!(row.background(p, false, true, true, false), p.selected);
            assert_eq!(row.background(p, false, true, false, false), p.base);
            assert_eq!(row.background(p, false, true, true, true), p.base);
        }
    }

    #[test]
    fn foreground_only_overlay_preserves_each_underlying_background() {
        let bounds = Rect::new(0, 0, 24, 2);
        let area = Rect::new(3, 1, 17, 1);
        let mut buffer = Buffer::empty(bounds);
        for (index, cell) in buffer.content.iter_mut().enumerate() {
            cell.set_symbol("x")
                .set_bg(if index % 2 == 0 {
                    Color::Red
                } else {
                    Color::Blue
                })
                .set_style(Style::default().add_modifier(Modifier::all()));
        }
        let backgrounds = buffer
            .content
            .iter()
            .map(|cell| cell.bg)
            .collect::<Vec<_>>();
        render_line(
            &Line::from("↓ Latest activity"),
            area,
            &mut buffer,
            Style::default()
                .fg(Color::Yellow)
                .remove_modifier(Modifier::all()),
        );
        assert_eq!(
            buffer
                .content
                .iter()
                .map(|cell| cell.bg)
                .collect::<Vec<_>>(),
            backgrounds
        );
        assert_eq!(
            (area.x..area.right())
                .map(|x| buffer[(x, area.y)].symbol())
                .collect::<String>(),
            "↓ Latest activity"
        );
        assert!((area.x..area.right()).all(|x| buffer[(x, area.y)].modifier.is_empty()));
    }

    #[test]
    fn agent_identity_and_state_roles_are_semantic_in_both_themes() {
        for light in [false, true] {
            let p = Palette::new(light);
            for (running, status, terminal, expected) in [
                (true, "Working", false, p.content.primary),
                (true, "Running tools", false, p.content.primary),
                (
                    true,
                    "Reconnecting · attempt 1 of 3",
                    false,
                    p.content.warning,
                ),
                (false, "Waiting for permission", false, p.content.warning),
                (false, "Waiting for input", false, p.content.warning),
                (false, "Completed", true, p.content.success),
                (false, "Failed", true, p.content.error),
                (false, "Interrupted", true, p.muted),
                (false, "Cancelled", true, p.muted),
                (false, "Queued", false, p.muted),
                (false, "Idle", false, p.muted),
            ] {
                assert_eq!(agent_status_color(running, status, p), expected);
                for prefix in ["> ", "    "] {
                    let symbol = agent_symbol(running, status, terminal, 0);
                    let line = agent_identity(
                        "Researcher",
                        " @remote",
                        prefix,
                        Span::styled(symbol, Style::default().fg(expected)),
                        50,
                        p,
                    );
                    assert_eq!(line.spans[1].style.fg, Some(expected));
                    assert_eq!(line.spans[3].content, "Researcher");
                    assert_eq!(line.spans[3].style.fg, Some(p.fg));
                    assert_eq!(line.spans[4].content, " @remote");
                    assert_eq!(line.spans[4].style.fg, Some(p.content.accent));
                    assert!(line.spans.iter().all(|span| span.style.bg.is_none()));
                }
            }
            assert_eq!(agent_symbol(false, "Failed", true, 0), "✗");
            assert_ne!(agent_symbol(false, "Cancelled", true, 0), "✓");
            assert_ne!(agent_symbol(false, "Interrupted", true, 0), "✓");
        }
    }

    #[test]
    fn menu_hints_are_muted_right_aligned_and_yield_to_labels() {
        for light in [false, true] {
            let p = Palette::new(light);
            for (label, hint) in [
                ("New session", "ctrl+x n"),
                ("Quit", "alt+q"),
                ("界面", "ctrl+shift+p"),
                ("Unbound", ""),
                ("Long custom shortcut", "ctrl+x ctrl+y ctrl+z"),
            ] {
                for width in [0, 1, 6, 12, 20, 38, 80] {
                    for selected in [false, true] {
                        let item = super::super::app::Item {
                            value: String::new(),
                            label: label.into(),
                            detail: hint.into(),
                            attachment: None,
                        };
                        let mut terminal =
                            ratatui::Terminal::new(ratatui::backend::TestBackend::new(90, 3))
                                .unwrap();
                        let row = r(3, 1, width, 1);
                        let bg = if selected { p.selected } else { p.input };
                        terminal
                            .draw(|frame| {
                                fill(frame, frame.area(), p.base);
                                fill(frame, row, bg);
                                draw_menu_item(
                                    frame,
                                    row,
                                    &item,
                                    &MenuKind::Commands,
                                    selected,
                                    p,
                                    bg,
                                );
                            })
                            .unwrap();
                        let buffer = terminal.backend().buffer();
                        let fits =
                            !hint.is_empty() && label.width() + hint.width() + 2 <= width as usize;
                        // Ratatui resets the hidden continuation cells of wide
                        // graphemes. They are not independently painted terminal
                        // cells: the leading cell's style covers the whole glyph.
                        let mut painted = Vec::new();
                        let mut x = row.x;
                        while x < row.right() {
                            painted.push(x);
                            x += buffer[(x, 1)].symbol().width().max(1) as u16;
                        }
                        let actual = painted
                            .iter()
                            .map(|&x| buffer[(x, 1)].symbol())
                            .collect::<String>();
                        if fits {
                            assert!(actual.ends_with(hint), "{actual:?}");
                            let start = row.right() - hint.width() as u16;
                            assert!((start..row.right()).all(|x| buffer[(x, 1)].fg == p.muted));
                        } else if !hint.is_empty() {
                            assert!(!actual.contains(hint), "{actual:?}");
                        }
                        if label.width() <= width as usize {
                            assert!(actual.starts_with(label), "{actual:?}");
                        }
                        if width > 0 && !label.contains('界') {
                            assert_eq!(
                                buffer[(row.x, 1)].fg,
                                if selected { p.content.primary } else { p.fg }
                            );
                        }
                        assert!(
                            painted.iter().all(|&x| buffer[(x, 1)].bg == bg),
                            "row background: label={label:?}, hint={hint:?}, width={width}, selected={selected}, light={light}: {:?}",
                            (row.x..row.right())
                                .map(|x| (x, buffer[(x, 1)].symbol(), buffer[(x, 1)].bg))
                                .collect::<Vec<_>>()
                        );
                        assert_eq!(buffer[(row.right(), 1)].bg, p.base);
                    }
                }
            }
        }
    }

    #[test]
    fn content_titles_request_status_and_ui_accents_use_semantic_roles() {
        for light in [false, true] {
            let p = Palette::new(light);
            let request = model::RequestRow {
                sequence: 1,
                purpose: skyhook::session::ModelPurpose::Agent,
                model: "test".into(),
                status: "Completed",
                usage: None,
                elapsed_tenths: None,
            };
            let columns = RequestColumns::new([&request]);
            let line = columns.line(&request, 100, p);
            assert!(
                line.spans
                    .iter()
                    .any(|span| span.content.contains("Request #1")
                        && span.style.fg == Some(p.content.primary))
            );
            assert!(
                line.spans
                    .iter()
                    .any(|span| span.content.contains("Completed")
                        && span.style.fg == Some(p.content.success))
            );
            let cache = super::super::tool_view::HighlightCache::default();
            let entry = model::Entry {
                key: "tool".into(),
                text: "read file\nbody".into(),
                surface: Surface::Tool,
                expandable: false,
                default_open: false,
                running: false,
                footer: None,
                request: None,
                indent: 0,
                job: None,
                compact_after: false,
                header: None,
                document: None,
            };
            let rows = layout_document_or_plain(&entry, 80, p, &cache, 0);
            assert_eq!(rows[0].line.spans[0].style.fg, Some(p.content.fg));
            assert_eq!(rows[1].line.style.fg, None);
            assert_eq!(p.accent, p.content.primary);
            assert_eq!(p.warning, p.content.warning);
        }
    }

    #[test]
    fn segmented_tool_headers_keep_roles_when_wrapped_or_in_documents() {
        use super::super::tool_view::{Document, Role, Run, Section, header_line};
        for light in [false, true] {
            let p = Palette::new(light);
            let header = vec![
                Run::new("▸", Role::Indicator),
                Run::new(" read ", Role::ToolName),
                Run::new("@remote", Role::Target),
                Run::new(" a long path ", Role::Plain),
                Run::new("· ", Role::Muted),
                Run::new("Completed", Role::Success),
                Run::new(" · #42", Role::Muted),
            ];
            let line = header_line(&header, light);
            let text = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            let entry = model::Entry {
                key: "tool-header".into(),
                text,
                surface: Surface::Tool,
                expandable: true,
                default_open: false,
                running: false,
                footer: None,
                request: None,
                indent: 0,
                job: None,
                document: None,
                header: Some(header.clone()),
                compact_after: false,
            };
            let mut expanded = entry.clone();
            expanded.document = Some(Document {
                sections: vec![Section::Line(header)],
            });
            let cache = super::super::tool_view::HighlightCache::default();
            for width in [8, 25, 100] {
                let rows = layout_document_or_plain(&entry, width, p, &cache, 0);
                let document_rows = layout_document_or_plain(&expanded, width, p, &cache, 0);
                let lines = |rows: &[Row]| {
                    rows.iter()
                        .map(|row| (row.line.clone(), row.continued))
                        .collect::<Vec<_>>()
                };
                assert_eq!(lines(&rows), lines(&document_rows));
                let name = rows
                    .iter()
                    .flat_map(|row| &row.line.spans)
                    .filter(|span| {
                        span.style.fg == Some(p.content.fg)
                            && span.style.add_modifier.contains(Modifier::BOLD)
                    })
                    .map(|span| span.content.as_ref())
                    .collect::<String>();
                assert!(name.contains("read"));
                assert!(
                    rows.iter()
                        .flat_map(|row| &row.line.spans)
                        .any(|span| span.content.contains("▸")
                            && span.style.fg == Some(p.content.primary))
                );
            }
        }
    }

    #[test]
    fn agents_palette_stats_headers_align_and_wrap_with_their_values() {
        let headers = AGENT_STATS_HEADERS.map(String::from);
        let values = ["12345".into(), "56789(12345)".into(), "54321".into()];
        for width in [100, 44, 30, 20] {
            let columns = AgentStatsColumns::menu([&values], width);
            assert!(columns.width() <= width);
            let header_height = columns.wrapped_height(&headers) as u16;
            let value_height = columns.wrapped_height(&values) as u16;
            let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(
                width,
                header_height + value_height,
            ))
            .unwrap();
            let p = Palette::new(false);
            terminal
                .draw(|frame| {
                    columns.draw(
                        frame,
                        r(0, 0, width, header_height),
                        &headers,
                        p.muted,
                        p.input,
                    );
                    columns.draw(
                        frame,
                        r(0, header_height, width, value_height),
                        &values,
                        p.muted,
                        p.input,
                    );
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            let mut x = 0;
            for (index, column_width) in columns.0.iter().enumerate() {
                let read_column = |y, height| {
                    (y..y + height)
                        .map(|y| {
                            (x..x + *column_width as u16)
                                .map(|x| buffer[(x, y)].symbol())
                                .collect::<String>()
                                .trim()
                                .to_owned()
                        })
                        .collect::<String>()
                };
                assert_eq!(
                    read_column(0, header_height).replace(' ', ""),
                    headers[index].replace(' ', "")
                );
                assert_eq!(read_column(header_height, value_height), values[index]);
                x += *column_width as u16 + 3;
            }
            if width >= 44 {
                assert_eq!(header_height, 1);
                assert_eq!(value_height, 1);
                assert!(
                    columns
                        .0
                        .iter()
                        .zip(&headers)
                        .all(|(width, header)| *width >= header.width())
                );
            }
        }
        assert_eq!(
            AgentStatsColumns::wrapped("Input (uncached)", 10),
            ["Input", "(uncached)"]
        );
    }

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
            let mut p = p;
            if entry.surface == Surface::Reasoning {
                p.content.fg = p.content.muted;
            }
            let block = matches!(entry.surface, Surface::User | Surface::Agent);
            if entry.document.is_some() || !(block || entry.surface == Surface::Reasoning) {
                let fallback = super::super::tool_view::HighlightCache::default();
                rows.extend(layout_document_or_plain(
                    entry,
                    width,
                    p,
                    highlights.unwrap_or(&fallback),
                    index,
                ));
                continue;
            }
            let geometry = EntryGeometry::new(entry, width, index);
            if block {
                rows.push(geometry.blank(entry.surface, geometry.x, geometry.block_width));
            }
            let text = model::clean(&entry.text);
            let mut body = text.as_str();
            let mut header = true;
            if block || entry.expandable {
                let (title, rest) = text.split_once('\n').unwrap_or((&text, ""));
                body = rest;
                let line = if block {
                    Line::from(Span::styled(
                        title.to_owned(),
                        Style::default().add_modifier(Modifier::BOLD),
                    ))
                } else {
                    Line::from(title.to_owned())
                };
                for (part, line) in wrap_words(line, geometry.body_width as usize)
                    .into_iter()
                    .enumerate()
                {
                    rows.push(geometry.row(line, header, part > 0));
                    header = false;
                }
            }
            let prefix =
                if entry.surface == Surface::Reasoning && !entry.expandable && entry.running {
                    "  "
                } else {
                    ""
                };
            if !entry.expandable || !body.is_empty() || block {
                for line in markdown::layout_highlighted(
                    body,
                    p,
                    true,
                    geometry.body_width as usize,
                    (geometry.body_width as usize).saturating_sub(prefix.width()),
                    prefix,
                    highlights,
                ) {
                    let mut row = geometry.row(line.line, header, line.continued);
                    row.layout = line.layout;
                    rows.push(row);
                    header = false;
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
                        for (part, line) in wrap_words(line, geometry.body_width as usize)
                            .into_iter()
                            .enumerate()
                        {
                            rows.push(geometry.row(line, false, part > 0));
                        }
                    }
                }
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
            header: None,
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
                    header: None,
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
            "| Name | Description |\n| --- | --- |\n| 界 | **long words** and `code` |\n\nplain tail",
            "> | Name | Description |\n> | --- | --- |\n> | 界 | long words |\n\nplain tail",
            "# Heading\n\n| Name | Description |\n| --- | --- |\n| value | a long table cell |\n\nend",
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
                        header: None,
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
                            expanded: entry.default_open,
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
                                expanded: entry.default_open,
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
