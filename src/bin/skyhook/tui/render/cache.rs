//! Retained entry state and incremental reflow.

use super::*;

/// Retained renderer state. Content changes arrive as dirty entry indices,
/// independently of width reflow; row storage owns its own height index.
pub struct RenderState {
    /// Entries to lay out again on the next frame.
    pub dirty: Vec<usize>,
    /// Lay out every entry on the next frame.
    pub reset: bool,
    pub rows: RowBlocks,
    pub width: u16,
    pub agent: skyhook::identity::AgentId,
    pub highlights: super::super::tool_view::HighlightCache,
    pub(super) request_columns: RequestColumns,
    /// The entries on screen when highlighting was last prepared.
    prepared: std::ops::Range<usize>,
}

impl RenderState {
    pub fn new(
        agent: skyhook::identity::AgentId,
        notify: tokio::sync::mpsc::UnboundedSender<super::super::app::Work>,
    ) -> Self {
        Self {
            dirty: Vec::new(),
            reset: true,
            rows: RowBlocks::default(),
            width: 0,
            agent,
            highlights: super::super::tool_view::HighlightCache::with_notify(notify),
            request_columns: RequestColumns::default(),
            prepared: 0..0,
        }
    }

    pub fn reset_session(&mut self) {
        self.dirty.clear();
        self.reset = true;
        self.highlights.clear();
    }
}

/// Reflow dirty entries, preserve scroll anchors, and validate active selections.
pub(super) fn prepare_rows(app: &mut App, width: u16) {
    let tab = app.tab;
    let content_changed = app.content_dirty;
    let anchor =
        if app.render.agent == app.selected && (content_changed || app.render.width != width) {
            app.views
                .get(&app.selected)
                .and_then(|view| view.scroll)
                .and_then(|scroll| {
                    let row = app.render.rows.get(scroll)?;
                    Some((
                        row.entry,
                        app.content_cache.entries().get(row.entry)?.key().clone(),
                        scroll - app.render.rows.entry_start(row.entry)?,
                    ))
                })
        } else {
            None
        };
    app.rebuild_content();
    app.render.highlights.poll();
    let reset = app.render.reset || app.render.agent != app.selected || app.render.width != width;
    let mut dirty = std::mem::take(&mut app.render.dirty);
    if reset {
        dirty = (0..app.content_cache.entries().len()).collect();
    }
    if tab == Tab::Requests
        && (reset
            || !dirty.is_empty()
            || app.render.rows.entry_count() != app.content_cache.entries().len())
    {
        app.render
            .request_columns
            .update(app.content_cache.entries(), &mut dirty);
    }
    let highlighted_sources = app.render.highlights.take_changed_sources();
    app.render
        .rows
        .highlight_entries(&highlighted_sources, &mut dirty);
    dirty.sort_unstable();
    dirty.dedup();
    let truncated = app.render.rows.entry_count() > app.content_cache.entries().len();
    let changed = reset || !dirty.is_empty() || truncated;
    // Selection validation visits just the selected rows that can change:
    // those from the first dirty or removed entry onward.
    let rows = &app.render.rows;
    let first_changed = if reset {
        Some(0)
    } else {
        let dirty = dirty.first().and_then(|&i| rows.entry_start(i));
        let removed = rows.entry_start(app.content_cache.entries().len());
        dirty.into_iter().chain(removed).min()
    };
    let selection_before = app
        .selection
        .zip(first_changed)
        .and_then(|((a, b), first)| {
            let first = first.max(a.row.min(b.row));
            let end = a.row.max(b.row);
            (first <= end).then(|| {
                let before = rows.iter_from(first).take(end - first + 1).cloned();
                (first, before.collect::<Vec<_>>())
            })
        });
    if reset {
        app.render.rows.clear();
    }
    for &index in &dirty {
        let Some(entry) = app.content_cache.entries().get(index) else {
            continue;
        };
        let sources = entry
            .document()
            .into_iter()
            .flat_map(|doc| doc.highlight_sources())
            .collect();
        let (highlights, columns) = (&app.render.highlights, app.render.request_columns);
        app.render.rows.update_entry(index, sources, |block| {
            update_entry_rows(block, entry, index, width, highlights, columns)
        });
    }
    app.render
        .rows
        .truncate_entries(app.content_cache.entries().len());
    if let (Some(selection), Some((first, before))) = (app.selection, selection_before) {
        let unchanged =
            selection_unchanged(first, &before, app.render.rows.iter_from(first), selection);
        if !unchanged {
            app.selection = None;
        }
    }
    app.render.reset = false;
    if changed {
        app.render.agent = app.selected.clone();
        if let Some((old_index, key, offset)) = anchor
            && let Some(index) = app
                .entries()
                .get(old_index)
                .filter(|entry| *entry.key() == key)
                .map(|_| old_index)
                .or_else(|| {
                    app.content_cache
                        .entries()
                        .iter()
                        .position(|entry| *entry.key() == key)
                })
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
    }
    // Highlight what is on screen first, then the selected and changed entries,
    // newest first. The budget cannot hold every section of a long transcript.
    let visible = visible_entries(app);
    if reset || content_changed || !dirty.is_empty() || app.render.prepared != visible {
        app.render.prepared = visible.clone();
        let selected = app.views.get(&app.selected).map_or(0, |view| view.row);
        let order = visible.chain([selected]).chain(dirty.iter().rev().copied());
        let (entries, rows) = (app.content_cache.entries(), &app.render.rows);
        let documents = order.filter_map(|index| Some((entries.get(index)?, index)));
        app.render.highlights.prepare(
            documents
                .flat_map(|(entry, index)| entry.document().into_iter().chain(rows.fences(index))),
        );
    }
}

/// Entries with a row inside the viewport.
fn visible_entries(app: &App) -> std::ops::Range<usize> {
    let rows = &app.render.rows;
    let height = app.content_rect.height as usize;
    let max = rows.len().saturating_sub(height);
    let scroll = app.views.get(&app.selected).and_then(|view| view.scroll);
    let top = scroll.map_or(max, |scroll| scroll.min(max));
    let bottom = (top + height).min(rows.len()).saturating_sub(1);
    match (rows.get(top), rows.get(bottom)) {
        (Some(first), Some(last)) => first.entry..last.entry + 1,
        _ => 0..0,
    }
}
