//! The viewed agent's history read a page at a time, off the event loop, as `Work`:
//! drawing asks for what the view nears, and the window folds what lands.
use super::*;
use model::{CHILD_PAGE, ChildPage, Direction, Edge, LIST_PAGE, Loaded, Near, PAGE, Read, Want};
use skyhook::agent::HarnessError;
use skyhook::job::{JobEnvelope, JobRole, JobScope};
use skyhook::session::Page;
use std::collections::VecDeque;
use std::ops::RangeInclusive;

/// Child pages a page load reads under the scripts it shows, nearest first.
const FAMILY_PAGES: usize = 32;

/// Read what `want` asks of `agent`'s history.
pub(super) async fn load(
    session: &SessionHandle,
    agent: &AgentId,
    want: Want,
) -> Result<Loaded, HarnessError> {
    let (read, jobs, parents) = match want {
        Want::Transcript(page) => {
            let read = session.store().transcript(agent, page, PAGE).await?;
            return shown(session, page, read, FAMILY_PAGES).await;
        }
        Want::Requests(page) => {
            let requests = session.store().requests(agent, page, LIST_PAGE).await?;
            (Read::Requests(page, requests), Vec::new(), Vec::new())
        }
        Want::Roots(page) => {
            let scope = JobScope::Roots(agent.clone());
            let jobs = session.jobs().page(scope, page, LIST_PAGE).await?;
            (Read::Roots(page), jobs, Vec::new())
        }
        Want::Children(parents) => (Read::Children, Vec::new(), parents),
    };
    let children = families(session, &jobs, parents, FAMILY_PAGES).await?;
    Ok(Loaded {
        read,
        jobs,
        children,
    })
}

/// What a transcript `page` read, the jobs it shows and their first children.
async fn shown(
    session: &SessionHandle,
    page: Page<u64>,
    read: skyhook::session::TranscriptPage,
    budget: usize,
) -> Result<Loaded, HarnessError> {
    let named = read.records.iter().flat_map(|record| model::shown(record));
    let jobs = session.jobs().views(named.collect()).await?;
    let children = families(session, &jobs, Vec::new(), budget).await?;
    Ok(Loaded {
        read: Read::Transcript(page, Box::new(read)),
        jobs,
        children,
    })
}

/// The children of each script among `jobs`, and of each parent in `parents` after
/// its key, with theirs, a page each, `budget` pages at most.
async fn families(
    session: &SessionHandle,
    jobs: &[JobEnvelope],
    parents: Vec<(JobId, Option<JobId>)>,
    budget: usize,
) -> Result<Vec<ChildPage>, HarnessError> {
    let scripts = |jobs: &[JobEnvelope]| -> Vec<(JobId, Option<JobId>)> {
        let scripts = jobs.iter().filter(|job| job.role == JobRole::Script);
        scripts.map(|job| (job.id, None)).collect()
    };
    let mut parents: VecDeque<_> = parents.into_iter().chain(scripts(jobs)).collect();
    let mut pages = Vec::new();
    while pages.len() < budget
        && let Some((parent, after)) = parents.pop_front()
    {
        let page = after.map_or(Page::First, Page::After);
        let read = (session.jobs())
            .page(JobScope::Children(parent), page, CHILD_PAGE)
            .await?;
        let more = read.len() == CHILD_PAGE;
        parents.extend(scripts(&read));
        pages.push(ChildPage {
            parent,
            after,
            jobs: read,
            more,
        });
    }
    Ok(pages)
}

/// `agent`'s whole transcript, every page and every child page, for an export.
pub(super) async fn whole(
    session: &SessionHandle,
    agent: &AgentId,
) -> Result<model::Window, HarnessError> {
    let mut pages = Vec::new();
    let mut following = Some(Page::First);
    while let Some(page) = following {
        let read = session.store().transcript(agent, page, PAGE).await?;
        // A page short of full reads the end.
        let last = (read.records.len() == PAGE).then(|| read.first + PAGE as u64 - 1);
        following = last.map(Page::After);
        let mut shown = shown(session, page, read, usize::MAX).await?;
        // An export shows every child, so their pages are read to the end.
        let more = |pages: &[ChildPage]| -> Vec<_> {
            let more = pages.iter().filter(|page| page.more);
            more.filter_map(|page| Some((page.parent, Some(page.jobs.last()?.id))))
                .collect()
        };
        let mut next = more(&shown.children);
        while !next.is_empty() {
            let read = families(session, &[], next, usize::MAX).await?;
            next = more(&read);
            shown.children.extend(read);
        }
        pages.push(shown);
    }
    Ok(model::Window::whole(agent.clone(), window_id(), pages))
}

/// Window ids, unique across sessions, so a load lands only in the window it was read for.
pub(super) fn window_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

impl App {
    /// The viewed agent's tab as loaded; one is made, its first load read around the
    /// view's top, when the view moved to another, or beyond what is loaded.
    pub(crate) fn ensure_window(&mut self) {
        let (agent, tab, focus) = (self.selected.clone(), self.tab, self.focus_of_view());
        let shown = |window: &model::Window| {
            (&window.agent, window.tab) == (&agent, tab) && !window.beyond(focus)
        };
        if self.window.as_ref().is_some_and(shown) {
            return;
        }
        self.window = Some(match self.session() {
            Some(_) => model::Window::new(agent, tab, window_id(), focus),
            // A draft has no history to read.
            None => model::Window::empty(agent, tab, window_id()),
        });
        // What the new view reads, it reads as the session left it.
        self.projection.retain_released(&Default::default());
        self.content_dirty = true;
    }

    /// Where the viewed tab's top sits, for a window made for it.
    pub(crate) fn focus_of_view(&self) -> Edge {
        let top = self.viewed().and_then(|view| view.top.as_ref());
        top.map_or(Edge::End, model::Anchor::focus)
    }

    /// How many entries the viewed agent's transcript holds.
    pub(crate) fn entry_count(&self) -> u64 {
        let entries = &self.snapshot.views.entries;
        entries.get(&self.selected).copied().unwrap_or_default()
    }

    /// Ask for the loads what is not loaded `near` the view needs, keeping what is
    /// loaded around the entries `shown`, from the view's top to its bottom.
    pub(crate) fn load_near(&mut self, near: &[Near], shown: RangeInclusive<Edge>) {
        let (Some(session), Some(window)) = (self.session().cloned(), self.window.as_mut()) else {
            return;
        };
        for (direction, want) in window.plan(near, shown) {
            #[cfg(test)]
            {
                self.loads += 1;
            }
            let (id, agent, tx) = (window.id, window.agent.clone(), self.tx.clone());
            let session = session.clone();
            tokio::spawn(async move {
                let result = load(&session, &agent, want).await;
                let _ = tx.send(Work::Page {
                    window: id,
                    direction,
                    result: result.map(Box::new),
                });
            });
        }
    }

    /// Fold a load into the window it was read for, if it is still shown.
    pub(super) fn page_loaded(
        &mut self,
        window: u64,
        direction: Direction,
        result: Result<Box<Loaded>, HarnessError>,
    ) {
        let Some(shown) = self.window.as_mut().filter(|shown| shown.id == window) else {
            return;
        };
        match result {
            Ok(loaded) => shown.loaded(direction, *loaded),
            Err(error) => {
                shown.fail(direction);
                self.local_notice(format!("Could not read the session's history: {error}"));
            }
        }
        self.prune(false);
        self.content_dirty = true;
    }

    /// Let go of the view: its window, entries, rows and outputs, as a session out of
    /// view keeps only its summary and the outputs chosen.
    pub fn release(&mut self) {
        self.window = None;
        self.content_cache = model::ContentCache::default();
        self.render.release();
        self.outputs.retain(|_| false);
    }
}

#[cfg(test)]
mod tests {
    use super::super::played::*;
    use super::super::tests::*;
    use super::*;
    use crate::tests::bounded;
    use serde_json::json;
    use skyhook::provider::ProviderErrorKind;

    /// A view following a transcript as it grows keeps only its newest entries
    /// loaded: what lies far above is let go, to be read again when the view nears it.
    #[tokio::test]
    async fn a_followed_transcript_keeps_only_its_newest_entries_loaded() {
        let (_root, mut app) = fixture().await;
        let session = app.session().unwrap().clone();
        let root = session.root_agent().clone();
        let total = model::KEPT + 2 * PAGE;
        let shown = |event: &skyhook::session::SessionEvent| {
            matches!(event, skyhook::session::SessionEvent::Status { .. })
        };
        // A batch at a time, so the app keeps up with the updates.
        for batch in (0..total).collect::<Vec<_>>().chunks(PAGE) {
            let statuses = batch.iter().map(|status| {
                let message = format!("status {status}");
                session.record_status(root.clone(), message)
            });
            bounded(futures_util::future::try_join_all(statuses))
                .await
                .unwrap();
            let seen = batch.last().unwrap() + 1;
            observe_until(&mut app, |app| count(&app.journal, shown) == seen).await;
            app.rebuild_content();
        }
        app.rebuild_content();
        let statuses = app
            .entries()
            .iter()
            .filter(|entry| entry.text().starts_with("Status"));
        let loaded = statuses.count();
        assert!(
            (model::KEPT / 2..=model::KEPT / 2 + PAGE).contains(&loaded),
            "{loaded}"
        );
        let last = app.journal.last().unwrap();
        let newest = format!(
            "Status · {}",
            match &last.event {
                skyhook::session::SessionEvent::Status { message } => message,
                event => panic!("{event:?}"),
            }
        );
        assert!(app.entries().iter().any(|entry| entry.text() == newest));
        let earlier = model::EntryKey::Gap(model::Gap::Earlier);
        assert_eq!(app.entries()[0].key(), &earlier);
    }

    /// A list shows its latest page, and `Home` reads the page before each time until
    /// its start shows, keeping no more than is kept however long the list is; shown
    /// again, it reads there.
    #[tokio::test(flavor = "multi_thread")]
    async fn home_reads_a_long_list_back_to_its_start() {
        use futures_util::StreamExt;
        let (_root, app) = fixture().await;
        let session = app.session().unwrap().clone();
        let total = model::KEPT + LIST_PAGE + 2;
        // Run together: nothing below depends on the order the scripts ran in.
        let scripts = futures_util::stream::repeat_with(|| session.run_script("return 1;"));
        let scripts = scripts.take(total).buffer_unordered(64);
        let ran = bounded(scripts.collect::<Vec<_>>()).await;
        assert!(ran.into_iter().all(|ran| ran.is_ok()));
        let (_viewer, mut app) = viewing(&session, Tab::Jobs).await;
        let jobs = |app: &App| {
            app.entries()
                .iter()
                .filter_map(model::Entry::job_id)
                .count()
        };
        assert_eq!(jobs(&app), LIST_PAGE);
        let earlier = model::EntryKey::Gap(model::Gap::Earlier);
        assert_eq!(app.entries()[0].key(), &earlier);
        app.focus = super::super::Focus::Content;
        for _ in 0..total.div_ceil(LIST_PAGE) {
            key(&mut app, KeyCode::Home, M::NONE);
            settled(&mut app).await;
            assert!(jobs(&app) <= model::KEPT);
        }
        let roots = JobScope::Roots(app.selected.clone());
        let first = bounded(session.jobs().page(roots, Page::First, 1)).await;
        let first = first.unwrap()[0].id;
        assert_eq!(app.entries()[0].job_id(), Some(first));
        // Let go of and shown again, the list reads where its view was.
        app.release();
        settled(&mut app).await;
        assert_eq!(app.focus_of_view(), Edge::At(model::Focus::Job(first)));
    }

    /// A job made and let go while a window's first load is in flight, after the load
    /// read the jobs, keeps the view the session let go of it with: its card shows
    /// once the load lands.
    #[tokio::test]
    async fn a_job_let_go_while_a_window_opens_keeps_its_card() {
        let (_root, mut app) = fixture().await;
        let session = app.session().unwrap().clone();
        let mut work = capture_work(&mut app);
        app.tab = Tab::Jobs;
        draw(&mut app);
        let page = recv(&mut work).await;
        assert!(matches!(page, Work::Page { .. }));
        bounded(session.run_script("return 1;")).await.unwrap();
        let created = |app: &App| {
            app.journal.iter().find_map(|record| match record.event {
                skyhook::session::SessionEvent::JobCreated { job, .. } => Some(job),
                _ => None,
            })
        };
        let job = observe_find(&mut app, created).await;
        observe_until(&mut app, |app| !app.snapshot.jobs.contains_key(&job)).await;
        app.work(page);
        app.rebuild_content();
        assert!(
            app.entries()
                .iter()
                .any(|entry| entry.job_id() == Some(job))
        );
    }

    /// Each entry read alone, with the facts its page carries, shows what the whole
    /// transcript shows: the requests it belongs to and how they settled, the calls and
    /// results it pairs with, the job a call started, how its turn began, and the
    /// failure a failed turn ended on.
    #[tokio::test]
    async fn an_entry_read_alone_shows_what_the_whole_transcript_shows() {
        let (_root, mut app, mut model, session) = played_fixture().await;
        let root = app.root_agent().clone();
        let asked = ask(&session, "work");
        let exec = call("exec", "exec", json!({"command": ["true"]}), 1);
        model.turn().await.end(vec![failing_call("failing"), exec]);
        let delay = Some(std::time::Duration::from_millis(10));
        let unavailable = ProviderErrorKind::Unavailable.error("HTTP 503");
        model.turn().await.fail(unavailable.with_retry_after(delay));
        model.turn().await.answer("done");
        bounded(asked).await.unwrap().unwrap();
        let failed = ask(&session, "again");
        let rejected = ProviderErrorKind::Authentication.error("rejected");
        model.turn().await.fail(rejected);
        let _ = bounded(failed).await.unwrap();
        let ended = |event: &skyhook::session::SessionEvent| {
            matches!(event, skyhook::session::SessionEvent::AgentFailed { .. })
        };
        observe_record(&mut app, ended).await;
        let whole = read(&session, &root).await;
        let outputs = OutputStore::default();
        // History alone, its spacing between siblings aside: entries read alone have
        // no neighbours to sit tight against.
        let history = |read: &Whole, details| {
            let view = View::default();
            let presentation = model::EntryView {
                agent: &root,
                tab: Tab::Conversation,
                view: &view,
                all_details: details,
                today: Default::default(),
            };
            let entries = model::entries(read.inputs(presentation, &outputs), false);
            let entries = entries
                .into_iter()
                .filter(|entry| !matches!(entry.key(), model::EntryKey::Gap(_)));
            let unspaced = entries.map(|mut entry| {
                entry.compact_after = false;
                entry
            });
            unspaced.collect::<Vec<_>>()
        };
        let mut reads = Vec::new();
        for ordinal in 0..whole.live.views.entries[&root] {
            let page = ordinal.checked_sub(1).map_or(Page::First, Page::After);
            let read = bounded(session.store().transcript(&root, page, 1))
                .await
                .unwrap();
            let loaded = shown(&session, page, read, FAMILY_PAGES);
            let loaded = bounded(loaded).await.unwrap();
            let focus = Edge::At(model::Focus::Ordinal(ordinal));
            let id = window_id();
            let mut window = model::Window::new(root.clone(), Tab::Conversation, id, focus);
            window.plan(&[], focus..=focus);
            window.loaded(Direction::Down, loaded);
            reads.push(Whole {
                live: whole.live.clone(),
                window,
                projection: whole.projection.clone(),
            });
        }
        for details in [false, true] {
            let alone = reads.iter().flat_map(|read| history(read, details));
            assert!(
                alone.collect::<Vec<_>>() == history(&whole, details),
                "details {details}"
            );
        }
    }
}
