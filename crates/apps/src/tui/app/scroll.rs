//! Scrolling the conversation and the draft, and the entry selected in it.
use super::App;
use super::extensions::Shown;
use crate::tui::model;

impl App {
    /// Whether the draft is taller than its box, so scrolling moves through it.
    pub(super) fn composer_overflows(&self) -> bool {
        self.editor.rows() > self.composer_rows()
    }
    /// Draft rows the composer box shows.
    pub(super) fn composer_rows(&self) -> usize {
        self.render.composer_rect.height.saturating_sub(2) as usize
    }
    /// Scroll the draft's view without moving its cursor.
    pub(super) fn scroll_composer(&mut self, step: isize) {
        let visible = self.composer_rows();
        let bottom = self.editor.rows().saturating_sub(visible);
        // From the rows shown, which a resize may have clamped below the stored top.
        let top = (self.composer_scroll)
            .map_or_else(|| self.editor.cursor_top(visible), |top| top.min(bottom));
        self.composer_scroll = Some(top.saturating_add_signed(step).min(bottom));
        self.dirty = true;
    }
    fn latest_row(&self) -> usize {
        let rows = self.render.rows.len();
        rows.saturating_sub(self.render.content_rect.height as usize)
    }
    /// The first transcript row in view: the pinned one, else the latest page.
    pub(crate) fn scroll_offset(&self) -> usize {
        let latest = self.latest_row();
        (self.render.scroll()).map_or(latest, |scroll| scroll.min(latest))
    }
    /// Pin the view's top at `row` of the rows laid out, or follow the newest entries.
    pub(crate) fn scroll_to(&mut self, row: Option<usize>) {
        let top = row.and_then(|row| crate::tui::render::anchor(self, row));
        self.render.top = match (&top, row) {
            (Some(_), Some(row)) => crate::tui::render::Top::Row(row),
            _ => crate::tui::render::Top::Latest,
        };
        self.view().top = top;
    }
    /// Pin the view's top at `at` along the whole history's span, read there when it is
    /// not loaded, or follow the newest entries.
    pub(crate) fn scroll_to_span(&mut self, at: Option<f64>) {
        match at.map(|at| crate::tui::render::Track::of(self).locate(self, at)) {
            Some(crate::tui::render::Spot::Ordinal(ordinal)) => {
                self.view().top = Some(model::Anchor::ordinal(ordinal));
                self.render.top = crate::tui::render::Top::Pending;
            }
            Some(crate::tui::render::Spot::Row(row)) => self.scroll_to(Some(row)),
            Some(crate::tui::render::Spot::Start) => {
                self.view().top = Some(model::Anchor::start());
                self.render.top = crate::tui::render::Top::Pending;
            }
            None => self.scroll_to(None),
        }
    }
    pub(super) fn scroll(&mut self, delta: isize) {
        if let Shown::Extension(_) = self.panel {
            self.scroll_extension(delta);
            return;
        }
        // Until the view's anchor is laid out, or while only a window's loading rows
        // are, the rows shown are not where it is: one read by ordinal moves by entries,
        // a row each, as the track spans what is not loaded; any other stays. Only a
        // page read there finds the end, and gives up a place past it for the newest.
        if self.render.top == crate::tui::render::Top::Pending
            || self.window.as_ref().is_some_and(model::Window::opening)
        {
            if let Some(top) = self.view().top.as_mut()
                && let model::Edge::At((model::Focus::Ordinal(ordinal), _)) = top.place
            {
                *top = model::Anchor::ordinal(ordinal.saturating_add_signed(delta as i64));
            }
            return;
        }
        let latest = self.latest_row();
        let next = self
            .scroll_offset()
            .saturating_add_signed(delta)
            .min(latest);
        // The newest rows follow the newest entries once those are loaded.
        let later = self.window.as_ref().is_some_and(|w| w.content.ends().1);
        self.scroll_to((next != latest || later).then_some(next));
    }
    /// The selected entry, as the content shows it now.
    pub(crate) fn selected_entry(&self) -> Option<usize> {
        let key = self.viewed()?.selected.as_ref()?;
        let entries = self.entries();
        let hint = self.render.selected;
        if entries.get(hint).is_some_and(|entry| entry.key() == key) {
            return Some(hint);
        }
        entries.iter().position(|entry| entry.key() == key)
    }
    pub(crate) fn select_entry(&mut self, index: usize) {
        let key = self.entries().get(index).map(|entry| entry.key().clone());
        self.render.selected = index;
        self.view().selected = key;
    }
    pub(super) fn reveal_row(&mut self) {
        let line = (self.selected_entry()).and_then(|row| self.render.rows.entry_start(row));
        if line.is_some() {
            self.scroll_to(line);
        }
    }
    /// Drop any text selection and follow the viewed tab's latest activity.
    pub(super) fn follow_latest(&mut self) {
        self.selection = None;
        self.view().top = None;
    }
}
