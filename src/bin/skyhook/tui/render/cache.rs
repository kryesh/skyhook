//! Retained entry state and incremental reflow.

use super::*;

/// Screens above and below the view within which what is not loaded is loaded.
pub(super) const PREFETCH_SCREENS: usize = 3;

/// Retained renderer state. Content changes arrive from the content cache,
/// independently of width reflow; row storage owns its own height index.
pub struct RenderState {
    /// Lay out every entry on the next frame.
    pub reset: bool,
    pub rows: RowBlocks,
    pub width: u16,
    pub agent: skyhook::identity::AgentId,
    /// The window the rows were laid out from.
    pub window: Option<u64>,
    /// Where the view's top resolved to in these rows.
    pub top: Top,
    /// Where the selected entry was last found.
    pub selected: usize,
    pub highlights: super::super::tool_view::HighlightCache,
    pub(super) request_columns: RequestColumns,
    /// The entries on screen when highlighting was last prepared.
    prepared: std::ops::Range<usize>,
    /// The sidebar's grants, and what they were read for.
    pub(super) grants: Option<(GrantsKey, Grants)>,
    /// Entries laid out since the counter was last read, for tests.
    #[cfg(test)]
    pub laid_out: usize,
}

impl RenderState {
    pub fn new(
        agent: skyhook::identity::AgentId,
        notify: tokio::sync::mpsc::UnboundedSender<super::super::app::Work>,
    ) -> Self {
        Self {
            reset: true,
            rows: RowBlocks::default(),
            width: 0,
            agent,
            window: None,
            top: Top::Latest,
            selected: 0,
            highlights: super::super::tool_view::HighlightCache::with_notify(notify),
            request_columns: RequestColumns::default(),
            prepared: 0..0,
            grants: None,
            #[cfg(test)]
            laid_out: 0,
        }
    }

    pub fn reset_session(&mut self) {
        self.reset = true;
        self.highlights.clear();
        self.grants = None;
    }

    /// Let go of every laid-out row, as a session out of view keeps none.
    pub fn release(&mut self) {
        self.rows = RowBlocks::default();
        self.reset = true;
        self.window = None;
        self.highlights.clear();
    }

    /// The row the view's top sits at; none while it follows the newest entries.
    pub fn scroll(&self) -> Option<usize> {
        match self.top {
            Top::Latest => None,
            Top::Row(row) | Top::Near(row, _) => Some(row),
            Top::Pending => Some(0),
        }
    }
}

/// Where the view's top resolved to in the rows laid out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Top {
    /// Following the newest entries.
    Latest,
    /// At what holds its anchor.
    Row(usize),
    /// In the entries its anchor's place shows, which still load what holds it.
    Near(usize, std::ops::Range<usize>),
    /// Nothing laid out holds its anchor yet: the view waits at the top, and scrolling
    /// moves the anchor.
    Pending,
}

/// The reading position at `row` of the rows laid out now. A row showing what is not
/// loaded past an end is held by the entry it borders, so what loads there lands
/// beyond the view.
pub(crate) fn anchor(app: &App, row: usize) -> Option<model::Anchor> {
    let rows = &app.render.rows;
    let (entry, offset) = rows.locate(row)?;
    // The key as laid out: the entries may have moved on since.
    let key = (*rows.get(row)?.entry_key).clone();
    match key {
        model::EntryKey::Gap(model::Gap::Earlier) if row + 1 < rows.len() => {
            return anchor(app, row + 1);
        }
        model::EntryKey::Gap(model::Gap::Later) if row > 0 => return anchor(app, row - 1),
        _ => {}
    }
    let history = app.content_cache.history();
    let place = history.place_of(entry).and_then(|focus| {
        let shown = history.place(focus)?;
        let start = rows.entry_start(shown.start)?;
        Some((focus, rows.entry_start(entry)? - start + offset))
    });
    Some(model::Anchor {
        key: Some(key),
        offset,
        place,
    })
}

/// Find `anchor` in the rows laid out now: its place, then its key among what that
/// shows; one with no place by its key. It is pending until a window opened around it,
/// and lost (none) once an open window, which reaches its place, does not hold it.
fn resolve(app: &App, anchor: &model::Anchor) -> Option<Top> {
    if app.window.as_ref().is_none_or(model::Window::opening) {
        return Some(Top::Pending);
    }
    let rows = &app.render.rows;
    let entries = app.content_cache.entries();
    let at = |entry: usize| {
        let start = rows.entry_start(entry)?;
        let end = rows.entry_start(entry + 1).unwrap_or(rows.len());
        Some(start + anchor.offset.min((end - start).saturating_sub(1)))
    };
    let keyed = |entry: &usize| {
        let key = entries.get(*entry).map(model::Entry::key);
        key.is_some_and(|key| Some(key) == anchor.key.as_ref())
    };
    let Some((focus, into)) = anchor.place else {
        return (0..entries.len()).find(keyed).and_then(at).map(Top::Row);
    };
    let shown = app.content_cache.history().place(focus)?;
    if let Some(row) = shown.clone().find(keyed).and_then(at) {
        return Some(Top::Row(row));
    }
    let start = rows.entry_start(shown.start).unwrap_or(rows.len());
    let end = rows.entry_start(shown.end).unwrap_or(rows.len());
    let row = start + into.min(end.saturating_sub(start + 1));
    let loading = |entry: usize| matches!(entries[entry].key(), model::EntryKey::Gap(_));
    Some(match anchor.key {
        Some(_) if shown.clone().any(loading) => Top::Near(row, shown),
        _ => Top::Row(row),
    })
}

/// Reflow changed entries, preserve scroll anchors, and reflow the text selection.
pub(super) fn prepare_rows(app: &mut App, width: u16) {
    let tab = app.tab;
    app.ensure_window();
    let window = app.window.as_ref().map(|window| window.id);
    let same = app.render.agent == app.selected
        && app.render.window == window
        && app.render.width == width;
    app.rebuild_content();
    app.render.highlights.poll();
    let reset = app.render.reset || !same;
    let changes = app.content_cache.take_changes();
    let changed = !changes.is_empty();
    let model::Changes { splices, dirty } = changes;
    let entries = app.content_cache.entries();
    let render = &mut app.render;
    let mut dirty = if reset {
        render.rows.reset(entries.len());
        (0..entries.len()).collect()
    } else {
        for &splice in &splices {
            render.rows.splice(splice);
        }
        debug_assert_eq!(render.rows.entry_count(), entries.len());
        dirty
    };
    if tab == Tab::Requests && (reset || changed) {
        render.request_columns.update(entries, &mut dirty);
    }
    let highlighted_sources = render.highlights.take_changed_sources();
    render
        .rows
        .highlight_entries(&highlighted_sources, &mut dirty);
    dirty.sort_unstable();
    dirty.dedup();
    #[cfg(test)]
    {
        render.laid_out += dirty.len();
    }
    let (highlights, columns) = (&render.highlights, render.request_columns);
    render.rows.layout(&dirty, |index| {
        let entry = &entries[index];
        let sources = entry.document().into_iter();
        let sources = sources.flat_map(|doc| doc.highlight_sources()).collect();
        let mut rows = Vec::new();
        let fences = update_entry_rows(&mut rows, entry, width, highlights, columns);
        Laid {
            rows,
            sources,
            fences,
        }
    });
    // Rows before the first changed entry keep their place and text, so a
    // selection ending before it needs no reflow.
    let first_changed = match reset {
        true => Some(0),
        false => (splices.first().map(|splice| splice.at))
            .into_iter()
            .chain(dirty.first().copied())
            .min()
            .map(|entry| render.rows.entry_start(entry).unwrap_or(render.rows.len())),
    };
    app.render.reset = false;
    app.render.agent = app.selected.clone();
    app.render.window = window;
    app.render.width = width;
    follow(app);
    if let Some(selected) = app.selected_entry() {
        app.render.selected = selected;
    }
    let reflow_selection =
        app.selection
            .as_ref()
            .zip(first_changed)
            .is_some_and(|(selection, first)| {
                let (anchor, head) = selection.rows();
                first <= anchor.row.max(head.row)
            });
    if reflow_selection {
        app.selection =
            (app.selection.take()).and_then(|selection| selection.reflow(&app.render.rows));
    }
    // Highlight what is on screen first, then the selected and changed entries,
    // newest first. The budget cannot hold every section of a long transcript.
    let visible = visible_entries(app);
    if reset || first_changed.is_some() || app.render.prepared != visible {
        app.render.prepared = visible.clone();
        let selected = app.selected_entry();
        let order = visible.chain(selected).chain(dirty.iter().rev().copied());
        let (entries, rows) = (app.content_cache.entries(), &app.render.rows);
        let documents = order.filter_map(|index| Some((entries.get(index)?, index)));
        app.render.highlights.prepare(
            documents
                .flat_map(|(entry, index)| entry.document().into_iter().chain(rows.fences(index))),
        );
    }
}

/// Put the view's top at its anchor in the rows laid out now, and keep the anchor at
/// what it found there. One not found yet is kept and the view waits near it, or at
/// the top; one the view no longer lists gives way to the newest entries.
fn follow(app: &mut App) {
    let Some(top) = app.viewed().and_then(|view| view.top.clone()) else {
        app.render.top = Top::Latest;
        return;
    };
    match resolve(app, &top) {
        Some(Top::Row(row)) => {
            app.render.top = Top::Row(row);
            if let Some(found) = anchor(app, row) {
                app.view().top = Some(found);
            }
        }
        Some(top) => app.render.top = top,
        None => {
            app.render.top = Top::Latest;
            app.view().top = None;
        }
    }
}

/// The row at the view's top: where it resolved, else the newest rows'.
fn top_row(app: &App) -> usize {
    let rows = &app.render.rows;
    let max = rows.len().saturating_sub(app.content_rect.height as usize);
    app.render.scroll().map_or(max, |scroll| scroll.min(max))
}

/// Entries with a row inside the viewport.
pub(crate) fn visible_entries(app: &App) -> std::ops::Range<usize> {
    let rows = &app.render.rows;
    let height = app.content_rect.height as usize;
    let top = top_row(app);
    let bottom = (top + height).min(rows.len()).saturating_sub(1);
    match (rows.locate(top), rows.locate(bottom)) {
        (Some((first, _)), Some((last, _))) => first..last + 1,
        _ => 0..0,
    }
}

/// What is not loaded within the prefetch distance of the view, or in the place its
/// anchor is sought in, for the window to load.
pub(super) fn near_view(app: &App) -> Vec<model::Near> {
    let rows = &app.render.rows;
    let height = (app.content_rect.height as usize).max(1);
    let top = top_row(app);
    let reach = PREFETCH_SCREENS * height;
    let (from, to) = (
        top.saturating_sub(reach),
        (top + height + reach).min(rows.len()),
    );
    let entries = app.content_cache.entries();
    let first = rows.locate(from).map_or(entries.len(), |(entry, _)| entry);
    let mut near = first..first;
    while rows.entry_start(near.end).is_some_and(|start| start < to) {
        near.end += 1;
    }
    if let Top::Near(_, place) = &app.render.top {
        near = near.start.min(place.start)..near.end.max(place.end);
    }
    let near = near.filter_map(|entry| {
        let model::EntryKey::Gap(gap) = entries.get(entry)?.key() else {
            return None;
        };
        let above = rows.entry_start(entry)? < top;
        Some(model::Near { gap: *gap, above })
    });
    near.collect()
}

/// The whole history as the scrollbar spans it, in rows: those laid out, with the
/// transcript's entries not loaded before and after them at the loaded entries' rows
/// per entry, exact once all are loaded; an entry a row while none are. A list spans
/// what it loaded.
pub(crate) struct Track {
    /// Rows before and after those laid out.
    pub before: usize,
    pub after: usize,
    rows: usize,
    /// The transcript's loaded ordinals, and how many entries it holds.
    transcript: Option<(std::ops::Range<u64>, u64)>,
}

/// Where a row of the track lies: laid out, or in a transcript entry not loaded.
pub(crate) enum Spot {
    Row(usize),
    Ordinal(u64),
}

impl Track {
    pub(crate) fn of(app: &App) -> Self {
        let rows = app.render.rows.len();
        let mut track = Self {
            before: 0,
            after: 0,
            rows,
            transcript: None,
        };
        let Some(transcript) = app.window.as_ref().and_then(model::Window::transcript) else {
            return track;
        };
        let entries = app.entry_count();
        let keys = &transcript.run.keys;
        let loaded = match (keys.first(), keys.last()) {
            (Some(first), Some(last)) => *first..last + 1,
            _ => {
                let focus = match app.focus_of_view() {
                    Some(model::Focus::Ordinal(ordinal)) => ordinal.min(entries),
                    _ => entries,
                };
                focus..focus
            }
        };
        let per = match loaded.end - loaded.start {
            0 => 1.0,
            count => rows as f64 / count as f64,
        };
        let share = |count: u64| (count as f64 * per).ceil() as usize;
        track.before = share(loaded.start);
        track.after = share(entries.saturating_sub(loaded.end));
        track.transcript = Some((loaded, entries));
        track
    }

    pub(crate) fn len(&self) -> usize {
        self.before + self.rows + self.after
    }

    /// What lies at `row` of the track.
    pub(crate) fn locate(&self, row: usize) -> Spot {
        let Some((loaded, entries)) = &self.transcript else {
            return Spot::Row(row);
        };
        let ordinal = |rows: usize, span: usize, count: u64| {
            (rows as f64 * count as f64 / span.max(1) as f64) as u64
        };
        let last = entries.saturating_sub(1);
        if row < self.before {
            return Spot::Ordinal(ordinal(row, self.before, loaded.start));
        }
        match row - self.before {
            row if row < self.rows && !loaded.is_empty() => Spot::Row(row),
            row => {
                let past = entries.saturating_sub(loaded.end);
                let row = row.saturating_sub(self.rows);
                Spot::Ordinal((loaded.end + ordinal(row, self.after, past)).min(last))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::app::{
        Work,
        played::{
            answered, ask, call, count, exchange, observe_record, observe_until, played_fixture,
        },
        tests::{capture_work, draw, draw_sized, fixture, recv, viewing},
    };
    use super::*;
    use crate::tests::bounded;
    use serde_json::json;
    use skyhook::agent::SessionHandle;
    use skyhook::provider::protocol::AssistantItem;
    use skyhook::session::SessionEvent;
    use std::sync::Arc;

    /// A session whose root journaled `count` statuses, one line each.
    async fn statuses(count: usize) -> (tempfile::TempDir, App, SessionHandle) {
        let (root, app) = fixture().await;
        let session = app.session().unwrap().clone();
        let agent = session.root_agent().clone();
        for status in 0..count {
            let message = format!("status {status}");
            session.record_status(agent.clone(), message).await.unwrap();
        }
        (root, app, session)
    }

    /// The first row in view, as text.
    fn top(app: &App) -> String {
        let scroll = app.render.scroll().unwrap_or_default();
        app.render
            .rows
            .get(scroll)
            .map_or_else(String::new, Row::text)
    }

    /// A fling stops at what is not loaded, asking for it once, read off the event
    /// loop; the drawing never waits on it. Each page lands above the view, which
    /// keeps its entry, and the fling goes on through it to the start.
    #[tokio::test]
    async fn a_fling_stops_at_what_is_not_loaded_and_reads_it_once() {
        let (_root, _app, session) = statuses(400).await;
        let (_viewer, mut app) = viewing(&session, Tab::Conversation).await;
        let mut work = capture_work(&mut app);
        let loaded = app.loads;
        let anchored = |app: &App| match app.focus_of_view() {
            Some(model::Focus::Ordinal(ordinal)) => ordinal,
            focus => panic!("{focus:?}"),
        };
        let pages = bounded(async {
            let mut pages = 0;
            loop {
                // Three rows a frame toward the top, nothing landing meanwhile, until
                // the view stops.
                loop {
                    let before = app.scroll_offset();
                    app.scroll_to(Some(before.saturating_sub(3)));
                    draw(&mut app);
                    if app.scroll_offset() >= before {
                        break;
                    }
                }
                if top(&app).contains("status 0") {
                    break pages;
                }
                let ordinal = anchored(&app);
                let landed = recv(&mut work).await;
                assert!(matches!(landed, Work::Page { .. }));
                app.work(landed);
                draw(&mut app);
                assert_eq!(anchored(&app), ordinal);
                pages += 1;
            }
        })
        .await;
        assert_eq!(app.loads - loaded, pages);
        assert_eq!(pages, (400 - model::PAGE).div_ceil(model::PAGE));
    }

    /// A view scrolled into the transcript keeps its entry whatever replaces the rows
    /// it was found in: a new width, and its session leaving view while it recovers
    /// lost updates, which installs nothing out of view, then showing again.
    #[tokio::test]
    async fn a_scrolled_view_keeps_its_entry_across_layouts() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let (_root, _app, session) = statuses(400).await;
        let (_viewer, mut app) = viewing(&session, Tab::Conversation).await;
        let mut work = capture_work(&mut app);
        let fill =
            async |app: &mut App, work: &mut tokio::sync::mpsc::UnboundedReceiver<Work>, width| {
                bounded(async {
                    loop {
                        draw_sized(app, width, 24);
                        if !app.window.as_ref().is_some_and(model::Window::loading) {
                            break;
                        }
                        let landed = recv(work).await;
                        app.work(landed);
                    }
                })
                .await;
            };
        let anchored = |app: &App| {
            let (entry, offset) = app.render.rows.locate(app.render.scroll()?)?;
            Some((app.entries()[entry].text().to_owned(), offset))
        };
        app.scroll_to(Some(app.render.rows.len() / 2));
        fill(&mut app, &mut work, 80).await;
        let shown = anchored(&app).unwrap();
        assert!(shown.0.starts_with("Status · status"), "{shown:?}");
        fill(&mut app, &mut work, 50).await;
        assert_eq!(anchored(&app).as_ref(), Some(&shown));
        app.resubscribe();
        app.release();
        let observed = recv(&mut work).await;
        assert!(matches!(observed, Work::Observed(Ok(_))));
        app.work(observed);
        assert!(app.window.is_none());
        fill(&mut app, &mut work, 50).await;
        assert_eq!(anchored(&app), Some(shown));
        // A view on entries a page read before the updates reported them keeps them,
        // and pages on past what the updates reported while it reads them.
        let ahead = app.entry_count() + 10;
        for status in 0..60 {
            let agent = session.root_agent().clone();
            bounded(session.record_status(agent, format!("unobserved {status}")))
                .await
                .unwrap();
        }
        app.view().top = Some(model::Anchor::ordinal(ahead));
        app.release();
        draw_sized(&mut app, 50, 24);
        let page = u64::from(app.content_rect.height);
        let key = KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE);
        app.event(crossterm::event::Event::Key(key));
        fill(&mut app, &mut work, 50).await;
        let paged = ahead + page;
        assert_eq!(app.focus_of_view(), Some(model::Focus::Ordinal(paged)));
        let status = format!("unobserved {}", 10 + page);
        assert!(top(&app).ends_with(&status), "{}", top(&app));
    }

    /// Pressing and dragging the scrollbar moves the view through the whole
    /// transcript, the entries there loading as it arrives; paging on before they
    /// land moves on from where it arrived.
    #[tokio::test]
    async fn dragging_the_scrollbar_moves_through_the_whole_transcript() {
        use crossterm::event::{
            Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
        };
        let (_root, _app, session) = statuses(400).await;
        let (_viewer, mut app) = viewing(&session, Tab::Conversation).await;
        let mut work = capture_work(&mut app);
        // The jump comes while the window reads the page above its start.
        app.scroll_to(Some(0));
        draw(&mut app);
        assert!(app.window.as_ref().is_some_and(model::Window::loading));
        let track = app.content_rect;
        let mouse = |app: &mut App, kind, row| {
            let (column, modifiers) = (track.right() - 1, KeyModifiers::NONE);
            app.event(Event::Mouse(MouseEvent {
                kind,
                column,
                row,
                modifiers,
            }));
        };
        let middle = track.y + track.height / 2;
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), track.y);
        mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), middle);
        mouse(&mut app, MouseEventKind::Up(MouseButton::Left), middle);
        let ordinal = |app: &App| match app.focus_of_view() {
            Some(model::Focus::Ordinal(ordinal)) => ordinal as usize,
            focus => panic!("{focus:?}"),
        };
        draw(&mut app);
        let jumped = ordinal(&app);
        assert!((150..250).contains(&jumped), "{jumped}");
        app.event(Event::Key(KeyEvent::new(
            KeyCode::PageDown,
            KeyModifiers::NONE,
        )));
        let paged = ordinal(&app);
        assert_eq!(paged, jumped + track.height as usize);
        // The first status in view.
        let status = |app: &App| {
            let scroll = app.render.scroll()?;
            let mut rows = app
                .render
                .rows
                .iter_from(scroll)
                .take(4)
                .map(|(_, row)| row.text());
            rows.find_map(|text| text.strip_prefix("Status · status ")?.parse::<usize>().ok())
        };
        bounded(async {
            draw(&mut app);
            while app.window.as_ref().is_some_and(model::Window::loading) {
                let landed = recv(&mut work).await;
                app.work(landed);
                draw(&mut app);
            }
        })
        .await;
        assert_eq!(ordinal(&app), paged);
        assert!(status(&app).is_some(), "{}", top(&app));
    }

    /// A new width lays out the entries the window holds, never what it has not loaded.
    #[tokio::test]
    async fn a_resize_lays_out_only_the_loaded_window() {
        let (_root, _app, session) = statuses(400).await;
        let (_viewer, mut app) = viewing(&session, Tab::Conversation).await;
        let statuses = app.entries().iter();
        let statuses = statuses.filter(|entry| entry.text().starts_with("Status"));
        assert!(
            statuses.count() < 400,
            "the window holds part of the transcript"
        );
        app.render.laid_out = 0;
        draw_sized(&mut app, 80, 24);
        assert_eq!(app.render.laid_out, app.entries().len());
    }

    /// A child job arriving under an old parent's card moves the rows after it
    /// without laying them out again, into the places a fresh layout gives them, and
    /// a view after it keeps what it shows.
    #[tokio::test]
    async fn a_child_under_an_old_parent_moves_the_rows_after_it() {
        let (_root, mut app, mut model, session) = played_fixture().await;
        let asked = ask(&session, "start");
        let source = "await receive(); return await tool.exec({command: ['true']});";
        let script = call("script", "script", json!({"source": source, "bg": true}), 1);
        model.turn().await.end(vec![script]);
        model.turn().await.answer("first answer");
        bounded(asked).await.unwrap().unwrap();
        observe_until(&mut app, |app| count(&app.journal, answered) == 1).await;
        let later = vec![AssistantItem::text("text", 0, "later answer")];
        exchange(&mut app, &mut model, "more", later).await;
        let parent = app.snapshot.jobs.values().find(|job| job.tool == "script");
        let parent = parent.unwrap().id;
        draw(&mut app);
        let position = |app: &App, text: &str| {
            let mut entries = app.entries().iter();
            entries
                .position(|entry| entry.text().contains(text))
                .unwrap()
        };
        let later_rows = |app: &App| {
            let rows = app.render.rows.entry_rows(position(app, "later answer"));
            rows.iter().map(|row| row.line.clone()).collect::<Vec<_>>()
        };
        let before = later_rows(&app);
        // A view on the first answer, which follows the parent's card.
        draw_sized(&mut app, 60, 14);
        let first = app.render.rows.entry_start(position(&app, "first answer"));
        app.scroll_to(first);
        let at_first = |app: &mut App| {
            draw_sized(app, 60, 14);
            let scroll = app.render.scroll().unwrap();
            let (entry, offset) = app.render.rows.locate(scroll).unwrap();
            (
                offset == 0 && app.entries()[entry].text().contains("first answer"),
                scroll,
            )
        };
        let (shown, scrolled) = at_first(&mut app);
        assert!(shown);
        let send = format!("await tool.job({parent}).send({{value: 'go'}});");
        let sent = tokio::spawn({
            let session = session.clone();
            async move { session.run_script(send).await }
        });
        let child = |event: &SessionEvent| matches!(event, SessionEvent::JobCreated { parent: Some(owner), .. } if *owner == parent);
        observe_record(&mut app, child).await;
        // The child's card arrives above the view, which keeps its place.
        let (shown, moved) = at_first(&mut app);
        assert!(shown && moved > scrolled);
        app.scroll_to(None);
        draw(&mut app);
        let card = app.entries().iter().position(|entry| {
            entry
                .job_id()
                .is_some_and(|job| app.snapshot.jobs[&job].parent == Some(parent))
        });
        assert!(card.unwrap() < position(&app, "later answer"));
        let after = later_rows(&app);
        assert_eq!(after.len(), before.len());
        assert!(
            after
                .iter()
                .zip(&before)
                .all(|(after, before)| Arc::ptr_eq(after, before))
        );
        let texts = |app: &App| app.render.rows.iter().map(Row::text).collect::<Vec<_>>();
        let retained = texts(&app);
        app.render.reset = true;
        draw(&mut app);
        assert_eq!(retained, texts(&app));
        bounded(sent).await.unwrap().unwrap();
        session.shutdown().await.unwrap();
    }
}
