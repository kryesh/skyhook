//! Retained entry state and incremental reflow.

use super::*;

/// Retained renderer state. Semantic changes arrive as one value, independently
/// of width reflow; row storage owns its own height index.
pub struct RenderState {
    pub changes: model::ContentChanges,
    pub rows: RowBlocks,
    pub width: u16,
    pub agent: skyhook::identity::AgentId,
    pub highlights: super::super::tool_view::HighlightCache,
    /// Markdown fence metadata per entry, keyed independently of row storage.
    pub(super) entries: std::collections::HashMap<model::EntryKey, code::Fences>,
    pub(super) request_columns: RequestColumns,
}

impl RenderState {
    pub fn new(
        agent: skyhook::identity::AgentId,
        notify: tokio::sync::mpsc::UnboundedSender<super::super::app::Work>,
    ) -> Self {
        Self {
            changes: model::ContentChanges {
                reset: true,
                ..Default::default()
            },
            rows: RowBlocks::default(),
            width: 0,
            agent,
            highlights: super::super::tool_view::HighlightCache::with_notify(notify),
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
pub(super) struct EntryLayout<'a> {
    pub(super) width: u16,
    pub(super) palette: Palette,
    pub(super) highlights: &'a super::super::tool_view::HighlightCache,
    pub(super) request_columns: RequestColumns,
    pub(super) expanded: bool,
}

/// Markdown layout and syntax metadata must see the same source: generated
/// message/reasoning titles are not part of the Markdown body. Offsets are in
/// the original UTF-8 entry text; both consumers use this one checked boundary.
pub(super) fn markdown_body_offset(entry: &model::Entry) -> usize {
    if matches!(entry.surface, Surface::User | Surface::Agent) || entry.expandable() {
        entry
            .text()
            .find('\n')
            .map_or(entry.text().len(), |end| end + 1)
    } else {
        0
    }
}

pub(super) fn update_markdown_fences(entry: &model::Entry, fences: &mut code::Fences) {
    let offset = markdown_body_offset(entry);
    fences.update(&entry.text()[offset..]);
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
        let has_title = block || entry.expandable();
        let body_offset = markdown_body_offset(entry);
        rows.clear();
        if block {
            rows.push(blank(entry.surface, x, block_width));
        }
        if has_title {
            let (title, _) = entry.text().split_once('\n').unwrap_or((entry.text(), ""));
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
        let body = &entry.text()[body_offset..];
        let prefix = if entry.surface == Surface::Reasoning && !entry.expandable() && entry.running
        {
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
    let tab = app.view().tab;
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
    let reset =
        app.render.changes.reset || app.render.agent != app.selected || app.render.width != width;
    let mut dirty = std::mem::take(&mut app.render.changes.dirty);
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
    // Only changed documents are prepared on ordinary content updates.
    let selected = app.views.get(&app.selected).map_or(0, |view| view.row);
    if reset || content_changed || !dirty.is_empty() {
        let documents = std::iter::once(selected)
            .chain(dirty.iter().copied())
            .filter_map(|i| app.content_cache.entries().get(i));
        app.render.highlights.prepare(documents.flat_map(|entry| {
            entry.document().into_iter().chain(
                app.render
                    .entries
                    .get(entry.key())
                    .map(|cached| &cached.document),
            )
        }));
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
                .is_some_and(|view| entry.is_expanded(view, app.details)),
        };
        app.render.rows.update_entry(index, sources, |block| {
            update_entry_rows(block, entry, index, settings);
        });
    }
    app.render
        .rows
        .truncate_entries(app.content_cache.entries().len());
    if let (Some(selection), Some((start, before))) = (app.selection, selection_before) {
        let unchanged = selection_unchanged(&before, app.render.rows.iter().skip(start), selection);
        if !unchanged {
            app.selection = None;
        }
    }
    app.render.changes.dirty.clear();
    app.render.changes.reset = false;
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_entry(text: String, surface: Surface, expandable: bool) -> model::Entry {
        let key = model::EntryKey::Record(1);
        if expandable {
            model::Entry::expandable_text(key, text, surface)
        } else {
            model::Entry::new(key, text, surface)
        }
    }

    fn fences(body: &str) -> code::Fences {
        let mut fences = code::Fences::default();
        fences.update(body);
        fences
    }

    // Checks the shared entry-to-body translation at the title boundary.
    #[test]
    fn fence_metadata_uses_body_offsets_for_full_replacements() {
        let body = "2. ```rust\n   let café = 42;\n   ```\n\nTail";
        for (surface, expandable, title) in [
            (Surface::Agent, false, "Agent [worker] 🦀\n"),
            (Surface::User, false, "User\n"),
            (Surface::Reasoning, true, "▾ Thinking\n"),
            (Surface::Reasoning, false, ""),
        ] {
            let full = format!("{title}{body}");
            let mut actual = code::Fences::default();
            let boundary = title.len();
            for end in [
                0,
                boundary.saturating_sub(1),
                boundary,
                boundary + 1,
                full.len(),
            ] {
                let entry = text_entry(full[..end].to_owned(), surface, expandable);
                update_markdown_fences(&entry, &mut actual);
                let expected = &full[boundary.min(end)..end];
                let expected = if title.is_empty() {
                    &full[..end]
                } else {
                    expected
                };
                assert_eq!(
                    actual.document,
                    fences(expected).document,
                    "{title:?}, end={end}"
                );
            }
            assert_eq!(actual.document.sections.len(), 1);
            // Preserve the authoritative equal-length replacement scenario.
            let entry = text_entry(full.replace("42", "43"), surface, expandable);
            update_markdown_fences(&entry, &mut actual);
            assert_eq!(actual.document, fences(&body.replace("42", "43")).document);
        }
    }

    #[test]
    fn full_entry_replacements_preserve_unicode_on_resize() {
        let highlights = super::super::super::tool_view::HighlightCache::default();
        let source = "Title\nwords 界 👩‍💻\n\n```rust\nlet n = 4;\n```\n\n| A | B |\n| - | - |\n| x | long words |";
        for surface in [Surface::Reasoning, Surface::User, Surface::Agent] {
            let mut entry = text_entry(source.to_owned(), surface, true);
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
