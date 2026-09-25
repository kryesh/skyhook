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
    /// Markdown fence metadata per entry, keyed independently of row storage.
    pub(super) entries: std::collections::HashMap<model::EntryKey, code::Fences>,
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
            entries: std::collections::HashMap::new(),
            request_columns: RequestColumns::default(),
            prepared: 0..0,
        }
    }

    pub fn reset_session(&mut self) {
        self.entries.clear();
        self.dirty.clear();
        self.reset = true;
        self.highlights.clear();
    }
}

#[derive(Clone, Copy)]
pub(super) struct EntryLayout<'a> {
    pub(super) width: u16,
    pub(super) palette: Palette,
    pub(super) highlights: &'a super::super::tool_view::HighlightCache,
    pub(super) request_columns: RequestColumns,
    pub(super) expanded: bool,
}

/// Markdown layout and syntax metadata see the same source: the body under any
/// generated title.
pub(super) fn update_markdown_fences(entry: &model::Entry, fences: &mut code::Fences) {
    fences.update(entry.body());
}

pub(super) fn update_entry_rows(
    rows: &mut Vec<Row>,
    entry: &model::Entry,
    index: usize,
    settings: EntryLayout<'_>,
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
    if let Some(request) = entry.request() {
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
    if (entry.surface == Surface::Reasoning || block) && entry.document().is_none() {
        let geometry = EntryGeometry::new(entry, width, index);
        let body_width = geometry.body_width;
        let x = geometry.x;
        let block_width = geometry.block_width;
        let make_row = |line, header, continued| geometry.row(line, header, continued);
        let blank = |surface, x, width| geometry.blank(surface, x, width);
        rows.clear();
        if block {
            rows.push(blank(entry.surface, x, block_width));
        }
        // A running entry leaves its first cells to the spinner: after the
        // disclosure glyph of a title, or before an untitled body.
        if let Some(title) = entry.title() {
            let title = model::clean(&title.line(entry.running));
            let title = if block {
                Line::from(Span::styled(
                    title,
                    Style::default().add_modifier(Modifier::BOLD),
                ))
            } else {
                Line::from(title)
            };
            for (part, line) in wrap_words(title, body_width as usize)
                .into_iter()
                .enumerate()
            {
                rows.push(make_row(line, part == 0, part > 0));
            }
        }
        let body = entry.body();
        let prefix = if entry.title().is_none() && entry.running {
            "  "
        } else {
            ""
        };
        let suffix =
            stream::layout_highlighted(body, body_width as usize, p, prefix, Some(highlights));
        {
            // Expanded reasoning omits the empty body; response blocks retain it.
            if !entry.expandable() || !body.is_empty() || block {
                for line in suffix {
                    let header = rows.is_empty();
                    let continued = line.layout.continued();
                    let mut row = make_row(line.line, header, continued);
                    row.layout = line.layout.with_flow(row.layout.header(), continued);
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
        *rows =
            layout_document_or_plain_with_expansion(entry, width, p, highlights, index, expanded);
    }
}

/// Reflow dirty entries, preserve scroll anchors, and validate active selections.
pub(super) fn prepare_rows(app: &mut App, width: u16, p: Palette) {
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
        app.render.entries.clear();
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
    // Fence sources join tool documents in the same bounded asynchronous cache.
    // Only authoritative dirty entries are rescanned; unchanged rows stay cached.
    for &index in &dirty {
        let Some(entry) = app.content_cache.entries().get(index) else {
            continue;
        };
        if entry.document().is_none()
            && matches!(
                entry.surface,
                Surface::User | Surface::Agent | Surface::Reasoning
            )
        {
            let cached = app.render.entries.entry(entry.key().clone()).or_default();
            update_markdown_fences(entry, cached);
        }
    }
    let highlighted_sources = app.render.highlights.take_changed_sources();
    let mut highlighted_entries = Vec::new();
    app.render
        .rows
        .highlight_entries(&highlighted_sources, &mut highlighted_entries);
    dirty.extend(highlighted_entries.iter().copied());
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
        let cached = app.render.entries.entry(entry.key().clone()).or_default();
        let sources = entry
            .document()
            .into_iter()
            .flat_map(|doc| doc.highlight_sources())
            .chain(cached.document.highlight_sources())
            .collect();
        let settings = EntryLayout {
            width,
            palette: p,
            highlights: &app.render.highlights,
            request_columns: app.render.request_columns,
            expanded: app
                .views
                .get(&app.selected)
                .is_some_and(|view| entry.is_expanded(view, app.tab, app.details)),
        };
        app.render.rows.update_entry(index, sources, |block| {
            update_entry_rows(block, entry, index, settings);
        });
    }
    app.render
        .rows
        .truncate_entries(app.content_cache.entries().len());
    // Fence metadata outlives relayouts; drop it once its entries are gone.
    if app.render.entries.len() > app.content_cache.entries().len() {
        let live: std::collections::HashSet<_> = app
            .content_cache
            .entries()
            .iter()
            .map(|entry| entry.key())
            .collect();
        app.render.entries.retain(|key, _| live.contains(key));
    }
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
    app.content_rows = app.render.rows.len();
    // Highlight what is on screen first, then the selected and changed entries,
    // newest first. The budget cannot hold every section of a long transcript.
    let visible = visible_entries(app);
    if reset || content_changed || !dirty.is_empty() || app.render.prepared != visible {
        app.render.prepared = visible.clone();
        let selected = app.views.get(&app.selected).map_or(0, |view| view.row);
        let order = visible.chain([selected]).chain(dirty.iter().rev().copied());
        let entries = order.filter_map(|index| app.content_cache.entries().get(index));
        app.render.highlights.prepare(entries.flat_map(|entry| {
            entry.document().into_iter().chain(
                app.render
                    .entries
                    .get(entry.key())
                    .map(|cached| &cached.document),
            )
        }));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_entry_replacements_preserve_unicode_on_resize() {
        let highlights = super::super::super::tool_view::HighlightCache::default();
        let body =
            "words 界 👩‍💻\n\n```rust\nlet n = 4;\n```\n\n| A | B |\n| - | - |\n| x | long words |";
        for surface in [Surface::Reasoning, Surface::User, Surface::Agent] {
            let mut entry = model::Entry::titled(
                model::EntryKey::UnsavedStatus(1),
                model::Title::disclosed("Title", true),
                body.to_owned(),
                surface,
            );
            entry.default_open = true;
            let mut rows = Vec::new();
            for width in [24, 8, 40] {
                let options = EntryLayout {
                    width,
                    palette: Palette::new(),
                    highlights: &highlights,
                    request_columns: RequestColumns::default(),
                    expanded: true,
                };
                update_entry_rows(&mut rows, &entry, 0, options);
                assert!(rows.iter().any(|row| row.text().contains('界')));
                assert!(rows.iter().all(|row| row.width <= width));
            }
        }
    }
}
