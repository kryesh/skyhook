use super::*;
use crate::SessionExtension;
use crate::page::{Disclosed, Page, Toggle, ViewFrame};
use crate::tui::render::{PageDraw, PageLine, draw_page, lay_out, page_area};
use std::sync::Arc;
use tokio_util::task::AbortOnDropHandle;

const REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const HOOK_TIMEOUT: Duration = Duration::from_secs(10);

/// The built-in inspector keeps its own tab/scroll state while a host view is shown.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum Panel {
    #[default]
    Transcript,
    /// An index into the extension's `views()`.
    Extension(usize),
}

/// One refresh at a time; the task ends with the app.
#[derive(Default)]
pub(super) struct Extensions {
    /// Each view as last loaded, or why it failed.
    cache: HashMap<usize, Result<Arc<Page>, String>>,
    /// What the viewer opened and last chose on each view.
    open: HashMap<usize, Disclosed>,
    selected: HashMap<usize, Toggle>,
    /// The shown view's scroll position, the rows it was last laid out as, and the
    /// view, disclosure and width they were laid out for.
    view: Viewport,
    lines: Vec<PageLine>,
    laid: Option<(usize, Disclosed, usize)>,
    /// The choice moved since the last draw, which scrolls to show it.
    reveal: bool,
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

    pub fn extension_views(&self) -> &[crate::ExtensionView] {
        self.extension().map_or(&[], |extension| extension.views())
    }

    pub fn select_extension(&mut self, view: usize) {
        if view >= self.extension_views().len() {
            return;
        }
        self.panel = Panel::Extension(view);
        self.selection = None;
        self.focus = Focus::Content;
        self.extensions.next_refresh = None;
        self.extensions.view = Viewport::default();
        self.dirty = true;
    }

    /// How the shown panel shares the frame.
    pub(in crate::tui) fn frame(&self) -> ViewFrame {
        match self.panel {
            Panel::Extension(view) => {
                (self.extension_views().get(view)).map_or(ViewFrame::Session, |view| view.frame)
            }
            Panel::Transcript => ViewFrame::Session,
        }
    }

    pub(super) fn page_frame(&self) -> bool {
        matches!(self.frame(), ViewFrame::Page { .. })
    }

    pub(super) fn scroll_extension(&mut self, delta: isize) {
        self.extensions
            .view
            .scroll(delta, self.extensions.lines.len());
        self.dirty = true;
    }

    /// Move the choice to the next or previous toggle on the page, or scroll a page
    /// that has none.
    pub(super) fn step_page(&mut self, up: bool) {
        let Panel::Extension(view) = self.panel else {
            return;
        };
        // A record's detail rows toggle it too: the record is one stop.
        let mut toggles: Vec<&Toggle> = (self.extensions.lines.iter())
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
        self.extensions.reveal = true;
        self.dirty = true;
    }

    /// Open or close what the chosen toggle names, when the page as last laid out still
    /// shows it.
    pub(super) fn toggle_page(&mut self, toggle: Option<Toggle>) {
        let Panel::Extension(view) = self.panel else {
            return;
        };
        let Some(toggle) = toggle.or_else(|| self.extensions.selected.get(&view).cloned()) else {
            return;
        };
        if !(self.extensions.lines.iter()).any(|line| line.toggle.as_ref() == Some(&toggle)) {
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
            Panel::Transcript => builtins.iter().position(|tab| *tab == self.tab).unwrap(),
            Panel::Extension(view) => builtins.len() + view,
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
        !self.stopping
            && matches!(self.panel, Panel::Extension(_))
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
        let Panel::Extension(view) = self.panel else {
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
                Ok(result) => result.map_err(|error| error.to_string()),
                Err(_) => Err("View refresh timed out".into()),
            };
            let _ = tx.send(Work::ExtensionView { view, open, result });
        });
        self.extensions.refresh = Some(AbortOnDropHandle::new(task));
    }

    /// A page read for `open`: one opened or closed since is read again at once.
    pub(super) fn extension_loaded(
        &mut self,
        view: usize,
        open: Disclosed,
        result: Result<Arc<Page>, String>,
    ) {
        self.extensions.refresh = None;
        let current = self.extensions.open.get(&view).cloned().unwrap_or_default();
        self.extensions.next_refresh = (self.panel == Panel::Extension(view) && current == open)
            .then(|| Instant::now() + REFRESH_INTERVAL);
        let result = result.map_err(|error| model::clean(&error));
        let unchanged = match (self.extensions.cache.get(&view), &result) {
            (Some(Ok(cached)), Ok(page)) => Arc::ptr_eq(cached, page),
            (Some(Err(cached)), Err(error)) => cached == error,
            _ => false,
        };
        if !unchanged {
            self.extensions.cache.insert(view, result);
            self.extensions.laid = None;
        }
        self.dirty = true;
    }

    /// Draw the shown view; `navigation` says whether the pointer highlights its toggles.
    pub fn draw_extension(&mut self, frame: &mut ratatui::Frame<'_>, navigation: bool) {
        let Panel::Extension(view) = self.panel else {
            return;
        };
        let width = page_area(self.render.content_rect).width.max(1).into();
        // A page is laid out again only when it, what is open, or the width changes;
        // its spinners turn as it is drawn.
        let key = (
            view,
            self.extensions.open.get(&view).cloned().unwrap_or_default(),
            width,
        );
        if self.extensions.laid.as_ref() != Some(&key) {
            self.extensions.lines = match self.extensions.cache.get(&view) {
                Some(Ok(page)) => lay_out(page, &key.1, width),
                Some(Err(error)) => vec![PageLine::text(error.clone())],
                None => vec![PageLine::text("Loading view…".to_owned())],
            };
            self.extensions.laid = self.extensions.cache.contains_key(&view).then_some(key);
        }
        let draw = PageDraw {
            selected: self.extensions.selected.get(&view),
            reveal: std::mem::take(&mut self.extensions.reveal),
            hover: self.hover.filter(|_| navigation),
            hits: &mut self.render.hits,
            animating: &mut self.render.animating,
            tick: self.tick_count,
        };
        draw_page(
            frame,
            self.render.content_rect,
            &self.extensions.lines,
            &mut self.extensions.view,
            draw,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::super::played::{observe_until, played_fixture, spawn_children};
    use super::super::tests::{
        capture_work, click, draw, draw_sized, draw_sized_buffer, fixture, key, mouse, recv,
    };
    use super::*;
    use crate::page::{
        AgentPane, Block, Cell, Column, Detail, Fact, Record, RecordKey, Section, SectionKey,
    };
    use crate::tests::bounded;
    use crate::tui::keys::Command;
    use crate::{ExtensionView, HookFuture};
    use crossterm::event::{KeyCode, KeyModifiers as M, MouseEventKind};
    use tokio::sync::Semaphore;

    const ITEMS: SectionKey = SectionKey("items");

    /// Two views, a page and one inside the session's chrome. Each read reports what it
    /// was asked for and waits for a permit; a section holds `total` records, newest
    /// first, of which it supplies what is open.
    struct MockExtension {
        views: [ExtensionView; 2],
        gate: Semaphore,
        reads: mpsc::UnboundedSender<(usize, Disclosed)>,
        total: std::sync::atomic::AtomicU64,
    }
    impl SessionExtension for MockExtension {
        fn views(&self) -> &[ExtensionView] {
            &self.views
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
        install_with(app, AgentPane::Live)
    }
    /// [`install`], its page keeping or hiding the live agents.
    fn install_with(
        app: &mut App,
        agents: AgentPane,
    ) -> (
        Arc<MockExtension>,
        mpsc::UnboundedReceiver<(usize, Disclosed)>,
    ) {
        let (reads, received) = mpsc::unbounded_channel();
        let extension = Arc::new(MockExtension {
            views: [
                ExtensionView {
                    title: "Overview",
                    frame: ViewFrame::Page { agents },
                },
                ExtensionView {
                    title: "Details",
                    frame: ViewFrame::Session,
                },
            ],
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
        assert!(app.panel == Panel::Extension(0));
        assert!(draw(&mut app).contains("Loading view"));
        assert_eq!(bounded(reads.recv()).await.map(|read| read.0), Some(0));
        assert!(draw_sized(&mut app, 90, 30).contains("Loading view"));
        assert!(reads.try_recv().is_err());
        app.command(Command::NextTab);
        extension.gate.add_permits(1);
        app.work(refreshed(&mut work).await);
        assert!(app.panel == Panel::Extension(1));
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
        assert!(app.extensions.view.top > 0, "the wheel scrolls the view");
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
        assert!(app.panel == Panel::Extension(1));
        app.command(Command::NextTab);
        assert!(app.panel == Panel::Transcript && app.tab == Tab::Conversation);
        app.command(Command::PreviousTab);
        assert!(app.panel == Panel::Extension(1));
        app.session().unwrap().shutdown().await.unwrap();
    }

    /// A page view takes the frame below the tab strip: no sidebar, live agents or
    /// composer, and focus stays on the page; a view inside the session's chrome and
    /// the conversation keep them.
    #[tokio::test]
    async fn page_views_take_the_frame_and_session_views_keep_the_chrome() {
        let (_root, mut app) = fixture().await;
        install(&mut app);
        app.saved.sidebar = true;
        draw_sized(&mut app, 120, 30);
        let conversation = (app.render.content_rect, app.render.composer_rect);
        assert!(conversation.0.width < 120 && conversation.1.height > 0);
        app.select_extension(0);
        app.focus = Focus::Composer;
        draw_sized(&mut app, 120, 30);
        assert_eq!(app.render.content_rect.width, 120);
        assert_eq!(app.render.composer_rect.height, 0);
        assert_eq!(app.focus, Focus::Content);
        app.select_extension(1);
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
        assert!((app.extensions.lines.iter()).any(|line| line.toggle.as_ref() == Some(&newest)));
        app.extensions.selected.insert(0, record);
        app.extensions.reveal = true;
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
        app.extensions.reveal = true;
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
    /// narrow one; one that hides them shows neither.
    #[tokio::test]
    async fn page_agents_sit_beside_wide_pages_and_below_narrow_ones() {
        let (_root, mut app, mut model, _session) = played_fixture().await;
        let (children, _turns) = spawn_children(&mut app, &mut model, 3).await;
        for (child, mode) in children.iter().zip(["component", "question", "validate"]) {
            let agent = (app.projection.agents.iter_mut())
                .find(|agent| &agent.id == child)
                .unwrap();
            agent.mode = Some(mode.parse().unwrap());
        }
        install(&mut app);
        app.select_extension(0);
        let screen = draw_sized(&mut app, 110, 40);
        let (tree, content) = (app.render.tree_rect, app.render.content_rect);
        assert_eq!((tree.y, tree.right()), (1, 110));
        assert_eq!(content.width, tree.x);
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
        install_with(&mut app, AgentPane::Hidden);
        draw_sized(&mut app, 110, 40);
        assert!(app.render.tree_rect.is_empty());
        assert_eq!(app.render.content_rect.width, 110);
    }

    /// Tab moves between a page and its agents, and stepping through them stays on the
    /// page; an agent chosen there or in the Agents palette opens its conversation.
    #[tokio::test]
    async fn agents_chosen_on_a_page_open_their_conversation() {
        let (_root, mut app, mut model, _session) = played_fixture().await;
        let (children, _turns) = spawn_children(&mut app, &mut model, 2).await;
        install(&mut app);
        app.select_extension(0);
        draw_sized(&mut app, 110, 40);
        let viewed = app.selected.clone();
        key(&mut app, KeyCode::Tab, M::NONE);
        assert!(app.focus == Focus::Tree);
        // Arrows move the column's cursor, not the conversation to come back to.
        key(&mut app, KeyCode::Down, M::NONE);
        assert!(matches!(app.panel, Panel::Extension(0)));
        assert_eq!((app.selected.clone(), app.tree_cursor), (viewed.clone(), 1));
        // Left and Right go from the cursor too, keeping the keys in the column.
        key(&mut app, KeyCode::Left, M::NONE);
        assert_eq!((app.selected.clone(), app.tree_cursor), (viewed.clone(), 0));
        key(&mut app, KeyCode::Right, M::NONE);
        assert_eq!((app.selected.clone(), app.tree_cursor), (viewed.clone(), 1));
        assert!(app.focus == Focus::Tree && matches!(app.panel, Panel::Extension(0)));
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
        assert!(app.focus == Focus::Content);

        draw_sized(&mut app, 110, 40);
        let row = (app.render.hits.iter())
            .find_map(|(rect, hit)| {
                matches!(hit, Hit::Agent(id) if *id == children[1]).then_some(*rect)
            })
            .expect("the column lists the child");
        click(&mut app, row);
        assert!(matches!(app.panel, Panel::Transcript) && app.tab == Tab::Conversation);
        assert_eq!(app.selected, children[1]);

        app.select_extension(0);
        app.command(Command::Agents);
        let index = (app.projection.agents.iter()).position(|agent| agent.id == children[0]);
        app.menu.as_mut().unwrap().select(index.unwrap());
        key(&mut app, KeyCode::Enter, M::NONE);
        assert!(matches!(app.panel, Panel::Transcript));
        assert_eq!(app.selected, children[0]);
    }

    /// A page shows no conversation, so its agents column marks no agent as viewed: only
    /// the cursor highlights, and only while the column has the keys.
    #[tokio::test]
    async fn a_page_column_highlights_only_its_focused_cursor() {
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
        key(&mut app, KeyCode::Tab, M::NONE);
        assert_eq!(highlighted(&mut app), 1);
        key(&mut app, KeyCode::Tab, M::NONE);
        assert_eq!(highlighted(&mut app), 0);
    }

    /// Beside a page the agents keep their column while the session works, so the page
    /// keeps its width when the children end.
    #[tokio::test]
    async fn a_page_keeps_its_agents_column_while_the_session_works() {
        let (_root, mut app, mut model, _session) = played_fixture().await;
        let (_children, turns) = spawn_children(&mut app, &mut model, 2).await;
        install(&mut app);
        app.select_extension(0);
        draw_sized(&mut app, 110, 40);
        let content = app.render.content_rect;
        assert!(content.width < 110);
        for turn in turns {
            turn.answer("done");
        }
        observe_until(&mut app, |app| !app.projection.has_active_children()).await;
        draw_sized(&mut app, 110, 40);
        assert!(app.busy());
        assert_eq!(app.render.content_rect, content);
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

    /// A page hides the composer, so neither Esc nor a paste reaches its draft.
    #[tokio::test]
    async fn a_page_keeps_keys_and_pastes_off_the_hidden_composer() {
        let (_root, mut app) = fixture().await;
        app.editor.set("saved draft".into());
        install(&mut app);
        app.select_extension(0);
        draw(&mut app);
        assert!(!app.busy());
        key(&mut app, KeyCode::Esc, M::NONE);
        assert!(app.focus == Focus::Content);
        app.event(crossterm::event::Event::Paste("unexpected paste".into()));
        assert_eq!(app.editor.text(), "saved draft");
    }
}
