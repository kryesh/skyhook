//! Retained entry state and incremental reflow.

use super::*;

/// Retained renderer state. Semantic changes arrive as one value, independently
/// of width/theme reflow; row storage owns its own height index.
pub struct RenderState {
    pub changes: model::ContentChanges,
    pub rows: RowBlocks,
    pub width: u16,
    pub light: bool,
    pub agent: skyhook::identity::AgentId,
    pub highlights: super::super::tool_view::HighlightCache,
    pub entries: std::collections::HashMap<String, CachedEntry>,
    pub(super) request_columns: RequestColumns,
}

impl RenderState {
    pub fn new(
        agent: skyhook::identity::AgentId,
        light: bool,
        notify: tokio::sync::mpsc::UnboundedSender<super::super::app::Work>,
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

#[derive(Default)]
pub struct CachedEntry {
    pub(super) width: u16,
    pub(super) light: bool,
    pub(super) stream: stream::StreamLayout,
    pub(super) fences: code::Fences,
    pub(super) body_offset: usize,
    pub(super) title_rows: usize,
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
/// the original UTF-8 entry text so append positions can be translated safely.
pub(super) fn markdown_body_offset(entry: &model::Entry) -> usize {
    if matches!(entry.surface, Surface::User | Surface::Agent) || entry.expandable {
        entry
            .text
            .find('\n')
            .map_or(entry.text.len(), |end| end + 1)
    } else {
        0
    }
}

pub(super) fn update_markdown_fences(
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

pub(super) fn update_entry_rows(
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

/// Reflow dirty entries, preserve scroll anchors, and validate active selections.
pub(super) fn prepare_rows(app: &mut App, width: u16, p: Palette) {
    let tab = app.view().tab;
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
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn appends_and_resize_match_fresh_layout() {
        let highlights = super::super::super::tool_view::HighlightCache::default();
        let source = "Title\nwords 界 👩‍💻\n\n```rust\nlet n = 4;\n```\n\n| A | B |\n| - | - |\n| x | long words |";
        for surface in [Surface::Reasoning, Surface::User, Surface::Agent] {
            let mut entry = expandable_entry();
            entry.surface = surface;
            entry.default_open = true;
            entry.text.clear();
            let mut cached = CachedEntry::default();
            let mut rows = Vec::new();
            for grapheme in source.graphemes(true) {
                let from = entry.text.len();
                entry.text.push_str(grapheme);
                update_entry_rows(
                    &mut rows,
                    &mut cached,
                    &entry,
                    0,
                    EntryLayout {
                        width: 24,
                        palette: Palette::new(false),
                        highlights: &highlights,
                        request_columns: RequestColumns::default(),
                        expanded: true,
                    },
                    (from > 0).then_some(from),
                );
                let mut fresh = Vec::new();
                update_entry_rows(
                    &mut fresh,
                    &mut CachedEntry::default(),
                    &entry,
                    0,
                    EntryLayout {
                        width: 24,
                        palette: Palette::new(false),
                        highlights: &highlights,
                        request_columns: RequestColumns::default(),
                        expanded: true,
                    },
                    None,
                );
                assert_eq!(rows.len(), fresh.len(), "{surface:?}: {}", entry.text);
                for (actual, expected) in rows.iter().zip(&fresh) {
                    assert_eq!(actual.line, expected.line);
                    assert_eq!(actual.layout, expected.layout);
                    assert_eq!(
                        (actual.x, actual.width, actual.continued),
                        (expected.x, expected.width, expected.continued)
                    );
                }
            }
            // A non-append reflow must reset retained stream state on width/theme changes.
            for width in [8, 40] {
                update_entry_rows(
                    &mut rows,
                    &mut cached,
                    &entry,
                    0,
                    EntryLayout {
                        width,
                        palette: Palette::new(true),
                        highlights: &highlights,
                        request_columns: RequestColumns::default(),
                        expanded: true,
                    },
                    None,
                );
                assert!(rows.iter().any(|row| row.text().contains('界')));
                assert!(rows.iter().all(|row| row.width <= width));
            }
        }
    }
}
