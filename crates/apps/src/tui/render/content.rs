//! The conversation pane and its scrollbar.
use super::document::EntryGeometry;
use super::painting::{
    fill, focus_cursor, foreground, r, render_line, render_row_line, scroll_thumb, spinner, text,
};
use super::track::Track;
use crate::tui::app::{App, Hit, Pane};
use crate::tui::model;
use crate::tui::model::Tab;
use crate::tui::theme::theme;
use ratatui::Frame;
use ratatui::style::Style;
use ratatui::text::Line;
use skyhook::job::JobState;
use unicode_width::UnicodeWidthStr;

/// The transcript rows in view, with selection, focus, running spinners and
/// live counters.
pub(super) fn draw_content(frame: &mut Frame, app: &mut App, navigation_active: bool) {
    let content_width = app.render.content_rect.width;
    if app.tab == Tab::Requests
        && app.render.content_rect.height > 1
        && let Some(entry) = app.entries().iter().find(|entry| entry.request().is_some())
    {
        let geometry = EntryGeometry::new(entry, content_width);
        if let Some(header) = app.render.request_columns.header(geometry.body_width) {
            text(
                frame,
                r(
                    geometry.x,
                    app.render.content_rect.y,
                    geometry.body_width,
                    1,
                ),
                header,
                theme().ui.muted,
                theme().ui.base,
            );
            // The fixed header is not an entry: paging, hit testing and selection
            // all use a viewport containing data rows only.
            app.render.content_rect.y += 1;
            app.render.content_rect.height -= 1;
        }
    }
    let max = app
        .render
        .rows
        .len()
        .saturating_sub(app.render.content_rect.height as usize);
    let scroll = app.render.scroll().map_or(max, |n| n.min(max));
    let selected_entry = app.selected_entry();
    // An empty conversation invites the first message below the scope panel and any
    // notices that are never journaled, startup warnings among them: neither is part
    // of the conversation.
    let empty = app.entries().iter().all(|entry| {
        matches!(
            entry.key(),
            model::EntryKey::Scope | model::EntryKey::UnsavedStatus(_)
        )
    });
    if empty {
        let rect = app.render.content_rect;
        let below = (app.render.rows.len() as u16).saturating_sub(scroll as u16);
        let y = rect.y + below + 3;
        if y < rect.bottom() {
            text(
                frame,
                r(rect.x + 3, y, content_width.saturating_sub(6), 1),
                crate::tui::theme::look().placeholder,
                theme().ui.muted,
                theme().ui.base,
            );
        }
    }
    // A job card's glyph gives way to the spinner only while it runs; waiting
    // for input keeps its glyph beside the counter.
    let (projection, live, window) = (&app.projection, &app.snapshot, app.window.as_ref());
    let running = |job| {
        let job = projection.job(live, window, &job);
        job.is_none_or(|job| job.state == JobState::Running)
    };
    let mut cursor_drawn = false;
    for (offset, (index, row)) in app
        .render
        .rows
        .iter_from(scroll)
        .take(app.render.content_rect.height as usize)
        .enumerate()
    {
        let y = app.render.content_rect.y + offset as u16;
        let rect = r(row.x, y, row.width, 1);
        let selection = app.selection.as_ref();
        let selection = selection.filter(|selection| selection.covers(scroll + offset));
        let view = selection.map(|_| row.text_view());
        let selected = (selection.zip(view.as_ref()))
            .map(|(selection, view)| selection.highlights(index, row, view.text()))
            .unwrap_or_default();
        let entry = app.content_cache.entries().get(index);
        let expandable = entry.is_some_and(model::Entry::expandable);
        let focused = navigation_active
            && app.focus == Pane::Content
            && row.selectable
            && Some(index) == selected_entry;
        let hovered =
            navigation_active && app.hover.is_some_and(|point| rect.contains(point.into()));
        let bg = row.background(
            entry.is_some_and(model::Entry::open),
            expandable && (focused || hovered),
            !selected.is_empty(),
        );
        fill(frame, rect, bg);
        let text_rect = r(row.paragraph_x(), y, row.text_width, 1);
        render_row_line(
            &row.line,
            &row.layout,
            text_rect,
            frame.buffer_mut(),
            Style::default().fg(foreground(row.surface)).bg(bg),
        );
        if let Some(view) = view.filter(|_| !selected.is_empty()) {
            // Source-to-column geometry skips hanging prefixes and code padding.
            // A malformed layout yields no cells; never paint it elsewhere.
            let text_end = text_rect.right() as usize;
            let source_end = row.layout.code().map_or(text_end, |code| {
                (text_rect.x as usize + code.body_end()).min(text_end)
            });
            for (byte, column, width) in view.source_cells() {
                if !selected.iter().any(|bytes| bytes.contains(&byte)) {
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
                    frame.buffer_mut()[(x as u16, y)].set_bg(theme().ui.selected);
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
            app.render.animating = true;
            if entry.job_id().is_none_or(running)
                && let Some(slot) = row.spinner
            {
                let x = row.x + slot;
                // A card's spinner stands in for its state glyph, in the glyph's accent.
                let color = if entry.header().is_some() {
                    theme().ui.emphasis
                } else {
                    theme().ui.muted
                };
                text(frame, r(x, y, 1, 1), spinner(app.tick_count), color, bg);
            }
            if let Some(counter) = entry.timing.text(app.clock) {
                let width = (counter.width() as u16).min(text_rect.width);
                let x = text_rect.right() - width;
                text(frame, r(x, y, width, 1), counter, theme().ui.muted, bg);
            }
        }
        if focused && !cursor_drawn {
            focus_cursor(frame, row.x.saturating_sub(1), y, theme().ui.base);
            cursor_drawn = true;
        }
        if *row.entry_key == model::EntryKey::Scope {
            let rows = &app.render.rows;
            let field = rows.locate(scroll + offset).and_then(|(entry, at)| {
                let line = (*rows.header_lines(entry).get(at)?)?;
                app.scope.as_ref()?.field_at(line)
            });
            if let Some(field) = field {
                app.render.hits.push((rect, Hit::ScopeField(field)));
            }
        } else if row.selectable {
            let toggles = row.layout.header() || expandable;
            app.render.hits.push((rect, Hit::Entry { index, toggles }));
        }
    }
}
/// The scrollbar thumb, and a way back to the tail while scrolled. Returns the
/// width the tail link takes from the notice row.
pub(super) fn draw_scrollbar(frame: &mut Frame, app: &mut App) -> u16 {
    let content = app.render.content_rect;
    let whole = Track::of(app);
    let track = r(content.width - 1, content.y, 1, content.height);
    if !scroll_thumb(frame, track, whole.shown, whole.total, theme().ui.base) {
        return 0;
    }
    app.render.hits.push((track, Hit::Scrollbar));
    if app.render.scroll().is_none() {
        return 0;
    }
    let width = 17.min(content.width.saturating_sub(2));
    let rect = r(
        content.width.saturating_sub(1 + width),
        app.render.tree_rect.y.saturating_sub(1),
        width,
        1,
    );
    // Text-only overlay: retain every underlying cell's background.
    render_line(
        &Line::from("↓ Latest activity"),
        rect,
        frame.buffer_mut(),
        Style::default().fg(theme().ui.emphasis),
    );
    app.render.hits.push((rect, Hit::Latest));
    width
}
