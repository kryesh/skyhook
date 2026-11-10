use super::events::Work;
use super::{App, Pane, Phase};
use crate::tui::model::Tab;
use crate::tui::render::PageView;
#[cfg(test)]
use ratatui::layout::Rect;
use skyhook::identity::AgentId;
use skyhook::product::SessionExtension;
use skyhook::product::page::{Disclosed, Page, Toggle};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::task::AbortOnDropHandle;

const REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const HOOK_TIMEOUT: Duration = Duration::from_secs(10);

/// The built-in inspector keeps its own tab/scroll state while a host view is shown.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum Shown {
    #[default]
    Transcript,
    /// An index into the extension's `views()`.
    Extension(usize),
}

/// A view's page as last read, or why it could not be.
pub(crate) type Loaded = Result<Arc<Page>, ViewError>;

/// Why a view could not be read.
#[derive(Clone, Debug, thiserror::Error)]
pub enum ViewError {
    #[error(transparent)]
    Failed(Arc<skyhook::product::Error>),
    #[error("View refresh timed out")]
    TimedOut,
}

/// One refresh at a time; the task ends with the app.
#[derive(Default)]
pub(crate) struct Extensions {
    /// Each view as last loaded, or why it failed.
    cache: HashMap<usize, Loaded>,
    /// What the viewer opened and last chose on each view.
    open: HashMap<usize, Disclosed>,
    selected: HashMap<usize, Toggle>,
    /// The shown view as last laid out and scrolled.
    page: PageView,
    refresh: Option<AbortOnDropHandle<()>>,
    next_refresh: Option<Instant>,
}

impl App {
    pub(super) fn extension(&self) -> Option<&Arc<dyn SessionExtension>> {
        match &self.phase {
            Phase::Open { observation, .. } => Some(&observation.extension),
            Phase::Draft { .. } => None,
        }
    }

    pub fn extension_views(&self) -> &[&'static str] {
        self.extension().map_or(&[], |extension| extension.views())
    }

    /// Show extension view `view`, a page that shows no conversation: the root agent is
    /// viewed again, so the status beside it is the session's own.
    pub fn select_extension(&mut self, view: usize) {
        if view >= self.extension_views().len() {
            return;
        }
        let root = AgentId::root(self.selected.session());
        if self.selected != root {
            self.select(root);
        }
        self.panel = Shown::Extension(view);
        self.selection = None;
        self.focus = Pane::Content;
        self.extensions.next_refresh = None;
        self.extensions.page.reset();
        self.dirty = true;
    }

    /// Whether a page takes the frame below the tab strip.
    pub(crate) fn page_frame(&self) -> bool {
        matches!(self.panel, Shown::Extension(_))
    }

    pub(super) fn scroll_extension(&mut self, delta: isize) {
        self.extensions.page.scroll(delta);
        self.dirty = true;
    }

    /// Move the choice to the next or previous toggle on the page, or scroll a page
    /// that has none.
    pub(super) fn step_page(&mut self, up: bool) {
        let Shown::Extension(view) = self.panel else {
            return;
        };
        // A record's detail rows toggle it too: the record is one stop.
        let mut toggles: Vec<&Toggle> = (self.extensions.page.lines().iter())
            .filter_map(|line| line.toggle.as_ref())
            .collect();
        toggles.dedup();
        if toggles.is_empty() {
            return self.scroll_extension(if up { -1 } else { 1 });
        }
        let current = (self.extensions.selected.get(&view))
            .and_then(|selected| toggles.iter().position(|toggle| *toggle == selected));
        let next = match (current, up) {
            (Some(index), true) => index.saturating_sub(1),
            (Some(index), false) => (index + 1).min(toggles.len() - 1),
            (None, true) => toggles.len() - 1,
            (None, false) => 0,
        };
        let toggle = toggles[next].clone();
        self.extensions.selected.insert(view, toggle);
        self.extensions.page.reveal();
        self.dirty = true;
    }

    /// Open or close what the chosen toggle names, when the page as last laid out still
    /// shows it.
    pub(super) fn toggle_page(&mut self, toggle: Option<Toggle>) {
        let Shown::Extension(view) = self.panel else {
            return;
        };
        let Some(toggle) = toggle.or_else(|| self.extensions.selected.get(&view).cloned()) else {
            return;
        };
        if !(self.extensions.page.lines().iter()).any(|line| line.toggle.as_ref() == Some(&toggle))
        {
            return;
        }
        self.extensions.open.entry(view).or_default().apply(&toggle);
        self.extensions.selected.insert(view, toggle);
        self.extensions.next_refresh = None;
        self.dirty = true;
    }

    pub(super) fn cycle_tab(&mut self, backwards: bool) {
        let builtins = [Tab::Conversation, Tab::Requests, Tab::Jobs];
        let views = self.extension_views().len();
        let current = match self.panel {
            Shown::Transcript => builtins.iter().position(|tab| *tab == self.tab).unwrap(),
            Shown::Extension(view) => builtins.len() + view,
        };
        let len = builtins.len() + views;
        let next = (current + if backwards { len - 1 } else { 1 }) % len;
        if let Some(tab) = builtins.get(next) {
            self.set_tab(*tab);
        } else {
            self.select_extension(next - builtins.len());
        }
    }

    pub(super) fn extension_due(&self) -> bool {
        !self.stopping()
            && matches!(self.panel, Shown::Extension(_))
            && self.extensions.refresh.is_none()
            && self
                .extensions
                .next_refresh
                .is_none_or(|next| Instant::now() >= next)
    }

    /// Called before painting, but all host code runs in the owned asynchronous task.
    pub fn refresh_extension(&mut self) {
        if !self.extension_due() {
            return;
        }
        let Shown::Extension(view) = self.panel else {
            return;
        };
        let Phase::Open { observation, .. } = &self.phase else {
            return;
        };
        let (extension, session) = (observation.extension.clone(), observation.session.clone());
        let open = self.extensions.open.get(&view).cloned().unwrap_or_default();
        let tx = self.tx.clone();
        let task = tokio::spawn(async move {
            let refresh = extension.refresh(view, session, open.clone());
            let result = match tokio::time::timeout(HOOK_TIMEOUT, refresh).await {
                Ok(result) => result.map_err(|error| ViewError::Failed(Arc::new(error))),
                Err(_) => Err(ViewError::TimedOut),
            };
            let _ = tx.send(Work::ExtensionView { view, open, result });
        });
        self.extensions.refresh = Some(AbortOnDropHandle::new(task));
    }

    /// A page read for `open`: one opened or closed since is read again at once.
    pub(super) fn extension_loaded(&mut self, view: usize, open: Disclosed, result: Loaded) {
        self.extensions.refresh = None;
        let current = self.extensions.open.get(&view).cloned().unwrap_or_default();
        self.extensions.next_refresh = (self.panel == Shown::Extension(view) && current == open)
            .then(|| Instant::now() + REFRESH_INTERVAL);
        let unchanged = match (self.extensions.cache.get(&view), &result) {
            (Some(Ok(cached)), Ok(page)) => Arc::ptr_eq(cached, page),
            // A failure is told by what it says: the same one again keeps its layout.
            (Some(Err(cached)), Err(error)) => cached.to_string() == error.to_string(),
            _ => false,
        };
        if !unchanged {
            self.extensions.cache.insert(view, result);
            self.extensions.page.stale();
        }
        self.dirty = true;
    }
}

impl Extensions {
    /// View `view` as last loaded, what is open and chosen on it, and its layout.
    pub(crate) fn shown(
        &mut self,
        view: usize,
    ) -> (Option<&Loaded>, Disclosed, Option<&Toggle>, &mut PageView) {
        let open = self.open.get(&view).cloned().unwrap_or_default();
        (
            self.cache.get(&view),
            open,
            self.selected.get(&view),
            &mut self.page,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::super::played::{observe_until, played_fixture, spawn_children};
    use super::super::tests::{
        capture_work, click, draw, draw_sized, draw_sized_buffer, fixture, key, mouse, recv,
    };
    use super::*;
    use crate::tests::bounded;
    use crate::tui::app::events::Hit;
    use crate::tui::commands::Command;
    use crate::tui::editor::TextField;
    use crossterm::event::{KeyCode, KeyModifiers as M, MouseEventKind};
    use skyhook::product::HookFuture;
    use skyhook::product::page::{
        Block, Cell, Column, Detail, Fact, Record, RecordKey, Section, SectionKey,
    };
    use tokio::sync::Semaphore;
    use tokio::sync::mpsc;

    const ITEMS: SectionKey = SectionKey("items");

    /// Two views. Each read reports what it was asked for and waits for a permit; a
    /// section holds `total` records, newest first, of which it supplies what is open.
    struct MockExtension {
        gate: Semaphore,
        reads: mpsc::UnboundedSender<(usize, Disclosed)>,
        total: std::sync::atomic::AtomicU64,
    }
    impl SessionExtension for MockExtension {
        fn views(&self) -> &[&'static str] {
            &["Overview", "Details"]
        }
        fn refresh(
            &self,
            view: usize,
            _session: skyhook::agent::SessionHandle,
            open: Disclosed,
        ) -> HookFuture<'_, Arc<Page>> {
            Box::pin(async move {
                self.reads.send((view, open.clone())).unwrap();
                self.gate.acquire().await.unwrap().forget();
                let total = self.total.load(std::sync::atomic::Ordering::SeqCst);
                let records = (0..total)
                    .rev()
                    .take(open.limit(ITEMS).unwrap_or(0))
                    .map(|item| {
                        let key = RecordKey(item.to_string());
                        let detail = open.detail(ITEMS, &key).map(|_| {
                            vec![Detail {
                                label: Some("about".to_owned()),
                                cells: vec![Cell::from(format!("detail of {item}"))],
                            }]
                            .into()
                        });
                        Record {
                            key,
                            cells: vec![format!("#{item}").into(), format!("item {item}").into()],
                            detail,
                        }
                    })
                    .collect();
                let mut blocks = vec![Block::Facts(vec![Fact {
                    label: "view".to_owned(),
                    value: view.to_string().into(),
                }])];
                blocks.extend((0..30).map(|row| {
                    Block::Facts(vec![Fact {
                        label: "row".to_owned(),
                        value: row.to_string().into(),
                    }])
                }));
                blocks.push(Block::Section(Section {
                    key: ITEMS,
                    title: "Items".to_owned(),
                    summary: None,
                    total,
                    columns: vec![
                        Column {
                            title: "id".to_owned(),
                            priority: 0,
                        },
                        Column {
                            title: "title".to_owned(),
                            priority: 0,
                        },
                    ],
                    records,
                }));
                Ok(Arc::new(Page { blocks }))
            })
        }
    }
    fn install(
        app: &mut App,
    ) -> (
        Arc<MockExtension>,
        mpsc::UnboundedReceiver<(usize, Disclosed)>,
    ) {
        let (reads, received) = mpsc::unbounded_channel();
        let extension = Arc::new(MockExtension {
            gate: Semaphore::new(0),
            reads,
            total: 60.into(),
        });
        let Phase::Open { observation, .. } = &mut app.phase else {
            panic!("opened fixture")
        };
        observation.extension = extension.clone();
        (extension, received)
    }
    async fn refreshed(rx: &mut mpsc::UnboundedReceiver<Work>) -> Work {
        loop {
            let work = recv(rx).await;
            if matches!(work, Work::ExtensionView { .. }) {
                return work;
            }
        }
    }
    /// Let the next read through, apply it and lay it out.
    async fn load(
        app: &mut App,
        extension: &MockExtension,
        work: &mut mpsc::UnboundedReceiver<Work>,
    ) {
        draw(app);
        extension.gate.add_permits(1);
        app.work(refreshed(work).await);
        draw_sized(app, 90, 30);
    }
    fn hit(app: &App, wanted: &Toggle) -> Rect {
        (app.render.hits.iter())
            .find_map(|(rect, hit)| {
                matches!(hit, Hit::Page(toggle) if toggle == wanted).then_some(*rect)
            })
            .unwrap_or_else(|| panic!("no hit for {wanted:?}"))
    }

    /// Drawing shows the placeholder until the one in-flight read completes, never
    /// waiting for it or starting another; a loaded page is laid out and scrolls; headers
    /// click and cycle with the built-in tabs.
    #[tokio::test]
    async fn extension_views_load_off_render_and_cycle_with_builtin_tabs() {
        let (_root, mut app) = fixture().await;
        let (extension, mut reads) = install(&mut app);
        let mut work = capture_work(&mut app);
        app.set_tab(Tab::Jobs);
        app.command(Command::NextTab);
        assert!(app.panel == Shown::Extension(0));
        assert!(draw(&mut app).contains("Loading view"));
        assert_eq!(bounded(reads.recv()).await.map(|read| read.0), Some(0));
        assert!(draw_sized(&mut app, 90, 30).contains("Loading view"));
        assert!(reads.try_recv().is_err());
        app.command(Command::NextTab);
        extension.gate.add_permits(1);
        app.work(refreshed(&mut work).await);
        assert!(app.panel == Shown::Extension(1));
        assert!(!draw(&mut app).contains("view 0"));
        app.command(Command::PreviousTab);
        let screen = draw(&mut app);
        assert!(screen.contains("view 0") && !screen.contains("Items"));
        // The page sits inside the pane's inset, and the wheel scrolls it.
        let page = app.render.content_rect;
        let first = screen
            .lines()
            .nth(usize::from(page.y) + 1)
            .unwrap_or_default();
        assert!(first.starts_with("  view 0"), "{screen}");
        mouse(&mut app, page, MouseEventKind::ScrollDown);
        assert_ne!(draw(&mut app), screen, "the wheel scrolls the view");
        app.scroll(isize::MAX / 2);
        let scrolled = draw(&mut app);
        assert!(scrolled.contains("Items") && !scrolled.contains("view 0"));
        app.select_extension(0);
        assert!(draw(&mut app).contains("view 0"));

        let header = app
            .render
            .hits
            .iter()
            .find_map(|(rect, hit)| matches!(hit, Hit::ExtensionTab(1)).then_some(*rect))
            .unwrap();
        click(&mut app, header);
        assert!(app.panel == Shown::Extension(1));
        // The focused page's bracket keys step the tabs as the commands do.
        key(&mut app, KeyCode::Char(']'), M::NONE);
        assert!(app.panel == Shown::Transcript && app.tab == Tab::Conversation);
        app.focus = Pane::Content;
        key(&mut app, KeyCode::Char('['), M::NONE);
        assert!(app.panel == Shown::Extension(1));
        app.session().unwrap().shutdown().await.unwrap();
    }

    /// A page view takes the frame below the tab strip: no sidebar or composer, focus
    /// stays on the page, neither Esc nor a paste reaches the hidden draft, and the root
    /// agent is viewed again; the conversation keeps them.
    #[tokio::test]
    async fn page_views_take_the_frame_and_the_conversation_keeps_the_chrome() {
        let (_root, mut app) = fixture().await;
        app.editor.set("saved draft".into());
        install(&mut app);
        app.prefs.saved.borrow_mut().sidebar = true;
        draw_sized(&mut app, 120, 30);
        let conversation = (app.render.content_rect, app.render.composer_rect);
        assert!(conversation.0.width < 120 && conversation.1.height > 0);
        let root = app.selected.clone();
        app.selected = root.child(0);
        app.select_extension(0);
        assert_eq!(app.selected, root, "a page view views the root again");
        app.focus = Pane::Composer;
        draw_sized(&mut app, 120, 30);
        assert_eq!(app.render.content_rect.width, 120);
        assert_eq!(app.render.composer_rect.height, 0);
        assert_eq!(app.focus, Pane::Content);
        assert!(!app.busy());
        key(&mut app, KeyCode::Esc, M::NONE);
        assert!(app.focus == Pane::Content);
        app.event(crossterm::event::Event::Paste("unexpected paste".into()));
        assert_eq!(app.editor.text(), "saved draft");
        app.set_tab(Tab::Conversation);
        draw_sized(&mut app, 120, 30);
        assert_eq!(app.render.content_rect, conversation.0);
        assert!(app.render.composer_rect.height > 0);
        app.session().unwrap().shutdown().await.unwrap();
    }

    /// Clicking a section, Enter on a record and clicking "more" each change what the
    /// next read asks for, which is read at once; what is open and chosen is kept by key
    /// while a refresh adds records above it.
    #[tokio::test]
    async fn page_toggles_open_what_the_next_read_supplies_and_survive_new_records() {
        let (_root, mut app) = fixture().await;
        let (extension, mut reads) = install(&mut app);
        let mut work = capture_work(&mut app);
        app.select_extension(0);
        load(&mut app, &extension, &mut work).await;
        assert_eq!(bounded(reads.recv()).await, Some((0, Disclosed::default())));
        app.scroll(isize::MAX / 2);
        draw_sized(&mut app, 90, 30);
        let section = hit(&app, &Toggle::Section(ITEMS));
        click(&mut app, section);
        assert!(app.extension_due(), "a toggle reads the page again at once");
        load(&mut app, &extension, &mut work).await;
        let (_, open) = bounded(reads.recv()).await.unwrap();
        assert_eq!(open.limit(ITEMS), Some(Disclosed::PAGE));

        let record = Toggle::Record(ITEMS, RecordKey("59".to_owned()));
        app.extensions.selected.insert(0, Toggle::Section(ITEMS));
        key(&mut app, KeyCode::Down, M::NONE);
        assert_eq!(app.extensions.selected.get(&0), Some(&record));
        key(&mut app, KeyCode::Enter, M::NONE);
        load(&mut app, &extension, &mut work).await;
        let (_, open) = bounded(reads.recv()).await.unwrap();
        assert!(open.detail(ITEMS, &RecordKey("59".to_owned())).is_some());
        assert!(draw_sized(&mut app, 90, 30).contains("detail of 59"));

        app.scroll(isize::MAX / 2);
        let screen = draw_sized(&mut app, 90, 30);
        assert!(screen.contains("50 of 60 · more"), "{screen}");
        let more = hit(&app, &Toggle::More(ITEMS));
        click(&mut app, more);
        load(&mut app, &extension, &mut work).await;
        let (_, open) = bounded(reads.recv()).await.unwrap();
        assert_eq!(open.limit(ITEMS), Some(2 * Disclosed::PAGE));

        extension
            .total
            .store(61, std::sync::atomic::Ordering::SeqCst);
        app.extensions.next_refresh = None;
        load(&mut app, &extension, &mut work).await;
        assert_eq!(app.extensions.selected.get(&0), Some(&Toggle::More(ITEMS)));
        let newest = Toggle::Record(ITEMS, RecordKey("60".to_owned()));
        assert!(
            (app.extensions.page.lines().iter()).any(|line| line.toggle.as_ref() == Some(&newest))
        );
        app.extensions.selected.insert(0, record);
        app.extensions.page.reveal();
        let screen = draw_sized(&mut app, 90, 30);
        assert!(
            screen.contains("detail of 59"),
            "the open record stays open: {screen}"
        );
        app.session().unwrap().shutdown().await.unwrap();
    }

    /// An open record's detail rows close it on click, as an open entry's body does in
    /// the conversation, and the next read no longer asks for its detail.
    #[tokio::test]
    async fn clicking_an_open_records_detail_closes_it() {
        let (_root, mut app) = fixture().await;
        let (extension, mut reads) = install(&mut app);
        let mut work = capture_work(&mut app);
        let record = Toggle::Record(ITEMS, RecordKey("59".to_owned()));
        let open = app.extensions.open.entry(0).or_default();
        open.apply(&Toggle::Section(ITEMS));
        open.apply(&record);
        app.select_extension(0);
        load(&mut app, &extension, &mut work).await;
        bounded(reads.recv()).await.unwrap();
        app.extensions.selected.insert(0, record.clone());
        app.extensions.page.reveal();
        draw_sized(&mut app, 90, 30);
        app.scroll_extension(4);
        let screen = draw_sized(&mut app, 90, 30);
        assert!(screen.contains("detail of 59"), "{screen}");
        let detail = (app.render.hits.iter())
            .filter(|(_, hit)| matches!(hit, Hit::Page(toggle) if *toggle == record))
            .map(|(rect, _)| *rect)
            .max_by_key(|rect| rect.y)
            .unwrap();
        assert!(
            detail.y > hit(&app, &record).y,
            "the detail row is a hit of its own"
        );
        click(&mut app, detail);
        load(&mut app, &extension, &mut work).await;
        let (_, open) = bounded(reads.recv()).await.unwrap();
        assert!(open.detail(ITEMS, &RecordKey("59".to_owned())).is_none());
        assert!(!draw_sized(&mut app, 90, 30).contains("detail of 59"));
        app.session().unwrap().shutdown().await.unwrap();
    }

    /// A page that keeps the live agents has them in a column beside it on a wide
    /// terminal, headed by a line per mode with its name at the left and its count at the
    /// right (folded into one line when the column is short), and in the band below it on a
    /// narrow one; one that hides them shows neither. Beside a page the agents keep their
    /// column while the session works, so the page keeps its width when the children end.
    #[tokio::test]
    async fn page_agents_sit_beside_wide_pages_and_below_narrow_ones() {
        let (_root, mut app, mut model, _session) = played_fixture().await;
        let (children, turns) = spawn_children(&mut app, &mut model, 3).await;
        for (child, mode) in children.iter().zip(["component", "question", "validate"]) {
            let agent = (app.projection.agents.iter_mut())
                .find(|agent| &agent.id == child)
                .unwrap();
            agent.outline.mode = Some(mode.parse().unwrap());
        }
        install(&mut app);
        app.select_extension(0);
        let screen = draw_sized(&mut app, 110, 40);
        let (tree, content) = (app.render.tree_rect, app.render.content_rect);
        assert_eq!((tree.y, tree.right()), (1, 110));
        assert_eq!(content.width, tree.x);
        let wide = content;
        assert!(screen.contains("Agents 4"), "{screen}");
        // Each mode's name starts at the column's left edge and its count ends at the right.
        let (left, right) = (usize::from(tree.x) + 2, usize::from(tree.right()) - 3);
        let counts: Vec<_> = (screen.lines())
            .filter_map(|line| {
                let cell = |byte: usize| line[..byte].chars().count();
                let name = line.find("component").or_else(|| line.find("question"))?;
                Some((cell(name), line.rfind('1').map(cell)))
            })
            .collect();
        assert_eq!(counts, [(left, Some(right)); 2], "{screen}");
        // On a short terminal the modes past a third of the column fold into one line, and
        // the agents keep their rows.
        let short = draw_sized(&mut app, 110, 9);
        assert!(short.contains("+3 modes"), "{short}");
        assert!(!short.contains("question"), "{short}");
        let rows = (app.render.hits.iter())
            .filter(|(_, hit)| matches!(hit, crate::tui::app::Hit::Agent(_)))
            .count();
        assert!(rows >= 2, "{short}");
        draw_sized(&mut app, 70, 40);
        let (tree, content) = (app.render.tree_rect, app.render.content_rect);
        assert_eq!((tree.x, tree.width, content.width), (0, 70, 70));
        assert!(tree.y >= content.bottom());
        for turn in turns {
            turn.answer("done");
        }
        observe_until(&mut app, |app| !app.projection.has_active_children()).await;
        draw_sized(&mut app, 110, 40);
        assert!(app.busy());
        assert_eq!(app.render.content_rect, wide);
    }

    /// Tab moves between a page and its agents, and stepping through them stays on the
    /// page; an agent chosen there or in the Agents palette opens its conversation. A page
    /// shows no conversation, so its column marks no agent as viewed: only the cursor
    /// highlights, and only while the column has the keys.
    #[tokio::test]
    async fn agents_chosen_on_a_page_open_their_conversation() {
        let (_root, mut app, mut model, _session) = played_fixture().await;
        let (children, _turns) = spawn_children(&mut app, &mut model, 2).await;
        install(&mut app);
        app.open_agent(children[0].clone());
        app.select_extension(0);
        let highlighted = |app: &mut App| {
            let buffer = draw_sized_buffer(app, 110, 40);
            (app.render.hits.iter())
                .filter(|(rect, hit)| {
                    matches!(hit, Hit::Agent(_))
                        && buffer[(rect.x, rect.y)].bg == crate::tui::theme::theme().ui.selected
                })
                .count()
        };
        assert_eq!(highlighted(&mut app), 0);
        let viewed = app.selected.clone();
        key(&mut app, KeyCode::Tab, M::NONE);
        assert!(app.focus == Pane::Tree);
        assert_eq!(highlighted(&mut app), 1);
        // Arrows move the column's cursor, not the conversation to come back to.
        key(&mut app, KeyCode::Down, M::NONE);
        assert!(matches!(app.panel, Shown::Extension(0)));
        assert_eq!((app.selected.clone(), app.tree_cursor), (viewed.clone(), 1));
        // Left and Right go from the cursor too, keeping the keys in the column.
        key(&mut app, KeyCode::Left, M::NONE);
        assert_eq!((app.selected.clone(), app.tree_cursor), (viewed.clone(), 0));
        key(&mut app, KeyCode::Right, M::NONE);
        assert_eq!((app.selected.clone(), app.tree_cursor), (viewed.clone(), 1));
        assert!(app.focus == Pane::Tree && matches!(app.panel, Shown::Extension(0)));
        // A first child the column no longer lists is passed over for the next listed one.
        key(&mut app, KeyCode::Left, M::NONE);
        let first = (app.projection.agents.iter()).position(|agent| agent.id == children[0]);
        app.projection.agents[first.unwrap()].lifecycle =
            crate::tui::model::AgentLifecycle::Terminal {
                until: None,
                grace_started: None,
            };
        draw_sized(&mut app, 110, 40);
        key(&mut app, KeyCode::Right, M::NONE);
        let cursor = app.projection.agents[app.tree_agents()[app.tree_cursor]]
            .id
            .clone();
        assert_eq!(
            (app.selected.clone(), cursor),
            (viewed, children[1].clone())
        );
        key(&mut app, KeyCode::Tab, M::NONE);
        assert!(app.focus == Pane::Content);
        assert_eq!(highlighted(&mut app), 0);

        draw_sized(&mut app, 110, 40);
        let row = (app.render.hits.iter())
            .find_map(|(rect, hit)| {
                matches!(hit, Hit::Agent(id) if *id == children[1]).then_some(*rect)
            })
            .expect("the column lists the child");
        click(&mut app, row);
        assert!(matches!(app.panel, Shown::Transcript) && app.tab == Tab::Conversation);
        assert_eq!(app.selected, children[1]);

        app.select_extension(0);
        app.command(Command::Agents);
        let index = (app.projection.agents.iter()).position(|agent| agent.id == children[0]);
        app.menu.as_mut().unwrap().select(index.unwrap());
        key(&mut app, KeyCode::Enter, M::NONE);
        assert!(matches!(app.panel, Shown::Transcript));
        assert_eq!(app.selected, children[0]);
    }

    /// The host applies only a toggle its page still shows.
    #[tokio::test]
    async fn a_toggle_the_page_no_longer_shows_opens_nothing() {
        let record = Toggle::Record(ITEMS, RecordKey("1".to_owned()));
        let (_root, mut app) = fixture().await;
        install(&mut app);
        app.select_extension(0);
        draw(&mut app);
        app.extensions.selected.insert(0, record);
        app.toggle_page(None);
        assert_eq!(app.extensions.open.get(&0), None);
    }
}
