//! The scrollbar's track: where the rows laid out lie along the whole history.

use super::*;

/// The scrollbar's span and what the view shows of it: a transcript's records, else
/// the rows a list loaded.
pub(crate) struct Track {
    pub shown: std::ops::Range<f64>,
    pub total: f64,
    span: Span,
}

/// What a track spans: a list's rows, or a transcript's records, how many there are,
/// and where the rows laid out lie among them while they hold the view.
enum Span {
    Rows,
    Records(u64, Option<Records>),
}

/// Where a place on the track lies: laid out, before everything, or in a transcript
/// record not loaded.
pub(crate) enum Spot {
    Row(usize),
    Start,
    Ordinal(u64),
}

impl Track {
    pub(crate) fn of(app: &App) -> Self {
        let rows = &app.render.rows;
        let view = view_rows(app);
        let window = app.window.as_ref();
        let transcript = window.and_then(|window| Some((window, window.transcript()?)));
        let Some((window, transcript)) = transcript else {
            return Self {
                shown: view.start as f64..view.end as f64,
                total: rows.len() as f64,
                span: Span::Rows,
            };
        };
        let run = &transcript.run;
        let loaded = run.extent().map(|extent| *extent.start()..extent.end() + 1);
        let records = app
            .entry_count()
            .max(loaded.as_ref().map_or(0, |loaded| loaded.end));
        // What streams after the last record spans one record, as its reply will once
        // committed.
        let live = live_row(app);
        let total = (records + u64::from(live.is_some())) as f64;
        let laid = loaded.filter(|_| app.render.top != Top::Pending && !window.opening());
        let Some(loaded) = laid else {
            let at = match app.focus_of_view() {
                model::Edge::Start => 0.0,
                model::Edge::At(model::Focus::Ordinal(ordinal)) => ordinal as f64,
                _ => total,
            };
            return Self {
                shown: at..at,
                total,
                span: Span::Records(records, None),
            };
        };
        let live = live.filter(|_| !run.later);
        let laid = Records::of(app, loaded, (run.earlier, run.later), records as f64, live);
        Self {
            shown: laid.position(app, view.start)..laid.position(app, view.end),
            total,
            span: Span::Records(records, Some(laid)),
        }
    }

    /// What lies at `at` along the track.
    pub(crate) fn locate(&self, app: &App, at: f64) -> Spot {
        let (records, laid) = match &self.span {
            Span::Rows => return Spot::Row(at as usize),
            Span::Records(records, laid) => (*records, laid),
        };
        let laid = match laid {
            Some(laid) if (laid.start..laid.reach()).contains(&at) => laid,
            _ if at <= 0.0 => return Spot::Start,
            _ => return Spot::Ordinal((at as u64).min(records.saturating_sub(1))),
        };
        // The first row at the last place at or before `at`: rows that show no record
        // lie where the next one starts, ahead of it.
        let position = |row| laid.position(app, row);
        let rows = app.render.rows.len();
        let before = partition(rows, |row| position(row) <= at).saturating_sub(1);
        let place = position(before);
        Spot::Row(partition(before, |row| position(row) < place))
    }
}

/// The first of `0..len` for which `before` fails, it holding for all before that.
fn partition(len: usize, before: impl Fn(usize) -> bool) -> usize {
    let (mut low, mut high) = (0, len);
    while low < high {
        let middle = (low + high) / 2;
        match before(middle) {
            true => low = middle + 1,
            false => high = middle,
        }
    }
    low
}

/// The first row of what streams after the history, while any does.
fn live_row(app: &App) -> Option<usize> {
    let rows = &app.render.rows;
    let start = rows.entry_start(app.content_cache.history().len())?;
    (start < rows.len()).then_some(start)
}

/// Where the transcript's laid-out rows lie on the track. Each entry's rows span its
/// own record, so what loads beside them never moves them, and records that show
/// nothing take no rows; the first and last entries reach the track's ends once
/// nothing lies past them, but for what streams after the last, which spans one more.
struct Records {
    start: f64,
    /// Where the records end.
    end: f64,
    /// The first row of what streams after the last record.
    live: Option<usize>,
    /// The first entries of the first and last places laid out.
    first: Option<usize>,
    last: Option<usize>,
}

impl Records {
    fn of(
        app: &App,
        loaded: std::ops::Range<u64>,
        (earlier, later): (bool, bool),
        total: f64,
        live: Option<usize>,
    ) -> Self {
        let history = app.content_cache.history();
        let entries = app.render.rows.entry_count();
        let own = |entry: usize| match history.place_of(entry) {
            Some(model::Focus::Ordinal(ordinal)) => {
                Some((ordinal, history.place(model::Focus::Ordinal(ordinal))?))
            }
            _ => None,
        };
        let first = (0..entries).find_map(own);
        let last = (0..entries).rev().find_map(own);
        let start = match (earlier, &first) {
            (false, _) => 0.0,
            (true, Some((ordinal, _))) => *ordinal as f64,
            (true, None) => loaded.start as f64,
        };
        let end = match (later, &last) {
            (false, _) => total,
            (true, Some((ordinal, _))) => (ordinal + 1) as f64,
            (true, None) => loaded.end as f64,
        };
        Self {
            start,
            end,
            live,
            first: first.map(|(_, place)| place.start),
            last: last.map(|(_, place)| place.start),
        }
    }

    /// Where the rows laid out end.
    fn reach(&self) -> f64 {
        self.end + f64::from(u8::from(self.live.is_some()))
    }

    /// Where `row` of those laid out lies: through its entry's span in proportion to
    /// its rows, or through the live span in proportion to its rows; rows of no entry
    /// lie where the next entry starts, or at the end.
    fn position(&self, app: &App, row: usize) -> f64 {
        let rows = &app.render.rows;
        if let Some(live) = self.live.filter(|live| row >= *live) {
            return self.end + (row - live) as f64 / (rows.len() - live) as f64;
        }
        let history = app.content_cache.history();
        let entries = rows.entry_count();
        let Some((entry, _)) = rows.locate(row) else {
            return self.end;
        };
        let ordinal = |entry: usize| match history.place_of(entry) {
            Some(model::Focus::Ordinal(ordinal)) => Some((entry, ordinal)),
            _ => None,
        };
        let Some((placed, own)) = (entry..entries).find_map(ordinal) else {
            return self.end;
        };
        let place = history.place(model::Focus::Ordinal(own));
        let place = place.unwrap_or(placed..placed + 1);
        let from = match Some(place.start) == self.first {
            true => self.start,
            false => own as f64,
        };
        if placed > entry {
            return from;
        }
        let to = match Some(place.start) == self.last {
            true => self.end,
            false => (own + 1) as f64,
        };
        let start = rows.entry_start(place.start).unwrap_or(row);
        let end = rows.entry_start(place.end).unwrap_or(rows.len());
        from + (row - start) as f64 / (end - start).max(1) as f64 * (to - from)
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::app::{
        played::{ask, observe_until, played_fixture, streamed},
        tests::{Screen, capture_work, draw, recv, viewing},
    };
    use super::super::cache::tests::{mixed_history, settle, statuses, top};
    use super::*;
    use crate::tests::bounded;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};

    /// Stepping through a history of entries of mixed heights and records that show
    /// nothing, longer than a window keeps, on a screen `height` rows tall, moves the
    /// thumb only the way it steps, and pages landing not at all but to the start once
    /// found, from flush with the top to flush with the bottom, where the view follows
    /// the newest entries.
    async fn step_through_mixed_history(
        height: u16,
        step: fn(bool, ratatui::layout::Rect) -> Event,
    ) {
        let (_root, _app, session) = mixed_history().await;
        let (_viewer, mut app) = viewing(&session, Tab::Conversation).await;
        let mut work = capture_work(&mut app);
        let mut screen = Screen::new(80, height);
        screen.draw(&mut app);
        // Step until the view stops with nothing left to load, landing a page only
        // once it stops, which leaves the thumb where it is; the thumb's top along
        // the way.
        let mut scroll = async |app: &mut App, up| {
            let mut thumb = vec![Track::of(app).shown.start];
            loop {
                app.event(step(up, app.content_rect));
                screen.draw(app);
                let at = Track::of(app).shown.start;
                if at == *thumb.last().unwrap() {
                    if !app.window.as_ref().is_some_and(model::Window::loading) {
                        break thumb;
                    }
                    let pinned = app.render.top != Top::Latest;
                    let landed = recv(&mut work).await;
                    app.work(landed);
                    screen.draw(app);
                    // Only the page that finds the start moves a pinned top, there.
                    let landed = Track::of(app).shown.start;
                    let kept = landed == at || up && landed == 0.0;
                    assert!(!pinned || kept, "{at} {landed}");
                }
                thumb.push(Track::of(app).shown.start);
            }
        };
        // The first step that moved the thumb against the scroll.
        let against = |thumb: Vec<f64>, up: bool| {
            let back = |step: &[f64]| {
                if up {
                    step[1] > step[0]
                } else {
                    step[1] < step[0]
                }
            };
            thumb.windows(2).position(back)
        };
        let up = scroll(&mut app, true).await;
        assert_eq!(against(up, true), None);
        assert_eq!(Track::of(&app).shown.start, 0.0);
        let down = scroll(&mut app, false).await;
        assert_eq!(against(down, false), None);
        let track = Track::of(&app);
        assert_eq!(track.shown.end, track.total);
        assert_eq!(app.render.top, Top::Latest);
        let later = model::EntryKey::Gap(model::Gap::Later);
        assert!(app.entries().iter().all(|entry| *entry.key() != later));
    }

    /// Wheeling, a few rows at a time.
    #[tokio::test]
    async fn wheeling_through_the_history_moves_the_thumb_one_way_to_each_end() {
        step_through_mixed_history(40, |up, rect| {
            Event::Mouse(MouseEvent {
                kind: match up {
                    true => MouseEventKind::ScrollUp,
                    false => MouseEventKind::ScrollDown,
                },
                column: rect.x,
                row: rect.y,
                modifiers: KeyModifiers::NONE,
            })
        })
        .await;
    }

    /// Paging a screen showing more records than a window keeps beside it.
    #[tokio::test]
    async fn paging_a_screen_taller_than_the_window_moves_the_thumb_one_way_to_each_end() {
        step_through_mixed_history(700, |up, _| {
            let code = if up {
                KeyCode::PageUp
            } else {
                KeyCode::PageDown
            };
            Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
        })
        .await;
    }

    /// Dragging the thumb down the track moves the view down with it, and the pointer
    /// held at a row keeps the view where it is, whatever heights the entries it passes.
    #[tokio::test]
    async fn dragging_the_thumb_moves_the_view_with_the_pointer() {
        use crossterm::event::{Event, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        let (_root, _app, session) = mixed_history().await;
        let (_viewer, mut app) = viewing(&session, Tab::Conversation).await;
        let mut work = capture_work(&mut app);
        draw(&mut app);
        let track = app.content_rect;
        let mut mouse = async |app: &mut App, kind, row| {
            let (column, modifiers) = (track.right() - 1, KeyModifiers::NONE);
            app.event(Event::Mouse(MouseEvent {
                kind,
                column,
                row,
                modifiers,
            }));
            settle(app, &mut work).await;
            Track::of(app).shown.start
        };
        let mut last = mouse(&mut app, MouseEventKind::Down(MouseButton::Left), track.y).await;
        for row in track.y..track.bottom() - 1 {
            let dragged = mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), row).await;
            assert!(dragged >= last, "row {row}: {dragged} after {last}");
            let held = mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), row).await;
            let again = mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), row).await;
            assert!(
                held >= dragged && again == held,
                "row {row}: {dragged} {held} {again}"
            );
            last = again;
        }
    }

    /// A view scrolled onto a notice posted before the transcript had any entry, at the
    /// start of a long history, stays there, and is read there again once let go.
    #[tokio::test]
    async fn a_view_on_a_notice_before_every_entry_stays_at_the_start() {
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
        let (_root, _app, session) = statuses(model::KEPT + 3 * model::PAGE).await;
        let (_viewer, mut app) = viewing(&session, Tab::Conversation).await;
        crate::tui::app::tests::unsaved_at(&mut app, None, "drafted");
        let mut work = capture_work(&mut app);
        app.focus = crate::tui::app::Focus::Content;
        app.event(Event::Key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)));
        settle(&mut app, &mut work).await;
        assert!(top(&app).contains("drafted"), "{}", top(&app));
        for _ in 0..3 {
            settle(&mut app, &mut work).await;
            assert!(top(&app).contains("drafted"), "{}", top(&app));
        }
        app.release();
        settle(&mut app, &mut work).await;
        settle(&mut app, &mut work).await;
        assert!(top(&app).contains("drafted"), "{}", top(&app));
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
            model::Edge::At(model::Focus::Ordinal(ordinal)) => ordinal as usize,
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

    /// A long reply still streaming after the last record spans a record of its own:
    /// the thumb moves up through it with the view, and a place along it lies in it.
    #[tokio::test]
    async fn the_thumb_moves_through_a_streaming_reply() {
        use crossterm::event::{Event, KeyModifiers, MouseEvent, MouseEventKind};
        use skyhook::provider::protocol::ItemKind;
        let (_root, mut app, mut model, session) = played_fixture().await;
        for status in 0..20 {
            let agent = session.root_agent().clone();
            bounded(session.record_status(agent, format!("status {status}")))
                .await
                .unwrap();
        }
        let text: String = (0..100).map(|line| format!("line {line}\n\n")).collect();
        let _asked = ask(&session, "question");
        let turn = model.turn().await;
        turn.delta("text", ItemKind::Text, &text);
        observe_until(&mut app, |app| streamed(&app.snapshot, &text)).await;
        draw(&mut app);
        let live = live_row(&app).unwrap();
        let track = Track::of(&app);
        assert_eq!(track.shown.end, track.total);
        assert!(track.shown.start > track.total - 1.0, "{:?}", track.shown);
        let wheel = Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: app.content_rect.x,
            row: app.content_rect.y,
            modifiers: KeyModifiers::NONE,
        });
        let mut thumb = track.shown.start;
        while app.scroll_offset() > live {
            app.event(wheel.clone());
            draw(&mut app);
            let at = Track::of(&app).shown.start;
            assert!(at < thumb, "{at} after {thumb}");
            thumb = at;
        }
        let track = Track::of(&app);
        let along = track.locate(&app, track.total - 0.5);
        assert!(matches!(along, Spot::Row(row) if row > live));
    }
}
