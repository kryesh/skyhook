//! The composer: the draft, its attachments and its cursor.
use super::painting::{r, scroll_thumb, text};
use crate::tui::app::{App, Hit, InputTarget, Pane};
use crate::tui::composer::ComposerLayout;
use crate::tui::theme::theme;
use ratatui::Frame;
use ratatui::style::{Modifier, Style};

/// The root agent's message editor and its attachments.
pub(super) fn draw_composer(frame: &mut Frame, app: &mut App, layout: &ComposerLayout) {
    let rect = app.render.composer_rect;
    let text_width = rect.width.saturating_sub(4);
    let (cursor_line, cursor_column) = layout.cursor;
    let visible = rect.height.saturating_sub(2) as usize;
    let bottom = layout.rows.len().saturating_sub(visible);
    let follow = cursor_line.saturating_sub(visible.saturating_sub(1));
    let top = app.composer_scroll.map_or(follow, |top| top.min(bottom));
    let paste = Style::default()
        .fg(theme().ui.emphasis)
        .remove_modifier(Modifier::all());
    let selection = Style::default().bg(theme().ui.selected);
    for (i, row) in layout.rows.iter().skip(top).take(visible).enumerate() {
        let line = row.line(Style::default(), paste, selection);
        let y = rect.y + 1 + i as u16;
        text(
            frame,
            r(2, y, text_width, 1),
            line,
            theme().ui.fg,
            theme().ui.input,
        );
    }
    let track = r(rect.width - 1, rect.y + 1, 1, visible as u16);
    let shown = top as f64..(top + visible) as f64;
    scroll_thumb(
        frame,
        track,
        shown,
        layout.rows.len() as f64,
        theme().ui.input,
    );
    let attachments = app.editor.attachments();
    if !attachments.is_empty() {
        let names = attachments.iter().map(|attachment| {
            match attachment.file().and_then(|file| file.file_name()) {
                Some(name) => format!("[{}]", name.to_string_lossy()),
                None => match &**attachment {
                    skyhook_media::Attachment::Text { .. } => "[text]".to_owned(),
                    skyhook_media::Attachment::Image { .. } => "[image]".to_owned(),
                },
            }
        });
        let names = names.collect::<Vec<_>>().join(" ");
        let y = rect.bottom() - 1;
        text(
            frame,
            r(2, y, text_width, 1),
            names,
            theme().ui.muted,
            theme().ui.input,
        );
        app.render
            .hits
            .push((r(0, y, rect.width, 1), Hit::Attachments));
    }
    let shown = (top..top + visible).contains(&cursor_line);
    if shown
        && app.focus == Pane::Composer
        && app.input_target() == InputTarget::Pane(Pane::Composer)
    {
        frame.set_cursor_position((
            2 + (cursor_column as u16).min(text_width),
            rect.y + 1 + (cursor_line - top) as u16,
        ));
    }
    app.render.hits.insert(0, (rect, Hit::Composer));
}
