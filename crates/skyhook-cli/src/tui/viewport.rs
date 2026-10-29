//! The part of a scrolled list that is on screen.

/// The first row in view, and how many rows the last draw showed: a page is
/// always the height on screen.
#[derive(Default)]
pub struct Viewport {
    pub top: usize,
    rows: usize,
}

impl Viewport {
    /// Record a draw of `rows` rows of a list of `len`, moving the view only as
    /// far as it must to show `cursor`. Returns the first row in view.
    pub fn fit(&mut self, rows: usize, len: usize, cursor: Option<usize>) -> usize {
        self.rows = rows;
        if let Some(cursor) = cursor {
            self.top = self.top.min(cursor).max((cursor + 1).saturating_sub(rows));
        }
        self.scroll(0, len);
        self.top
    }

    /// Scroll `delta` rows, stopping where the last of `len` rows shows.
    pub fn scroll(&mut self, delta: isize, len: usize) {
        self.top = (self.top.saturating_add_signed(delta)).min(len.saturating_sub(self.rows));
    }

    /// Scroll a page. Returns the step, by which a highlight moves to keep its
    /// row on screen.
    pub fn page(&mut self, down: bool, len: usize) -> isize {
        let step = self.rows as isize * if down { 1 } else { -1 };
        self.scroll(step, len);
        step
    }
}
