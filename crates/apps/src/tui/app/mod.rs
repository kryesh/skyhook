use crate::tui::format;
use events::Gesture;
pub use events::{Hit, Operation, Owner, Work};
pub use input::InputTarget;
use lifecycle::PendingStart;
pub use menus::{Emphasis, ItemRef, Menu, MenuId, MenuKind, MenuLayout, SavedStatus, SessionRef};
use prompts::UiPrompt;
use queue::QueueDelivery;
pub use queue::{Paused, QueuedInput, QueuedInputId};
use skyhook::{config::mode::ModeName, provider::model::ModelRef};
mod commands;
mod events;
mod extensions;
pub use extensions::Shown;
mod input;
mod lifecycle;
mod observation;
use observation::ActiveObservation;
pub use observation::PreparedObservation;
mod menus;
mod output;
mod pages;
mod projection;
pub use prompts::{QuestionPage, TabState};
#[cfg(test)]
pub(super) mod played;
mod prompts;
mod queue;
mod scope;
mod scroll;
mod session;
mod tree;
pub use session::{HostRequest, Peer, PeerSession, SlotKey};

use super::{
    composer::{Composer, Submission},
    model::{self, Entry, OutputStore, Projection, Tab, Ticket, Views},
    viewport::Viewport,
};
use crate::state::Prefs;
use crossterm::event::KeyEvent;
#[cfg(test)]
use ratatui::layout::Rect;
use skyhook::identity::{AgentId, SessionId};
use skyhook::interaction::UiInteraction;
use skyhook::launch::Launch;
use skyhook::{session::ObservationSnapshot, session::SessionTitle};
use std::rc::Rc;
use std::sync::Arc;
use std::{
    collections::{HashSet, VecDeque},
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

/// Characters of text kept for a session title or a menu row.
const TITLE_CHARS: usize = 100;
/// Rows a mouse wheel step scrolls.
const SCROLL_STEP: isize = 3;
/// How long a toast stays up.
const TOAST_TTL: Duration = Duration::from_secs(2);
/// Pasted text with more lines than this becomes one inline paste item.
const PASTE_COLLAPSE_LINES: usize = 12;

/// The pane that takes the keys while no menu or prompt does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pane {
    Composer,
    Tree,
    Content,
    /// The draft's scope panel above the conversation.
    Scope,
}
fn draft_root() -> AgentId {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let id = SessionId::generate().unwrap_or_else(|_| {
        SessionId::from_bytes(
            u128::from(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)).to_be_bytes(),
        )
    });
    AgentId::root(id)
}

/// Recall that started from an empty draft; editing the recalled text ends it.
struct HistoryBrowse {
    index: usize,
    /// Empty, but its undo history comes back with it.
    draft: Composer,
    /// The composer revision the recalled entry was set at.
    revision: u64,
}

/// A draft becomes a session with its first submission or script; the draft's
/// root identity then names what was noticed before the session existed.
enum Phase {
    Draft {
        root: AgentId,
        /// The action a session is being created for: one creation at a time, its
        /// completion the current one.
        start: Option<PendingStart>,
    },
    Open {
        observation: ActiveObservation,
        attached_draft: Option<AgentId>,
        /// Whether a script or continuation this app ran owns the root.
        owns_root: bool,
    },
}

/// Whether the slot runs, is shutting its session down, or has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Running,
    Stopping,
    Stopped,
}

pub struct App {
    phase: Phase,
    pub launch: Launch,
    /// The session's own prompt channel, which asks this app.
    pub ui: Arc<UiInteraction>,
    /// UI-only choice, captured by each submitted user message.
    pub model: ModelRef,
    /// UI-only choice like `model`; a new session starts in it.
    pub mode: ModeName,
    /// The state file's settings, as the process last saved them: shared by every
    /// open session.
    pub prefs: Rc<Prefs>,
    pub snapshot: ObservationSnapshot,
    /// Responses the session told of for agents not shown, read once one is.
    unread_responses: HashSet<(AgentId, skyhook::session::RequestSeq)>,
    pub projection: Projection,
    /// The viewed agent's tab, loaded around the view; none while out of view.
    pub window: Option<model::Window>,
    pub selected: AgentId,
    /// Shared across agents; reading and expansion state remain in `views`.
    pub tab: Tab,
    pub panel: Shown,
    pub(crate) extensions: extensions::Extensions,
    pub views: Views,
    pub focus: Pane,
    pub tree_cursor: usize,
    pub tree_view: Viewport,
    pub editor: Composer,
    pub history: Vec<String>,
    history_browse: Option<HistoryBrowse>,
    pub queue: VecDeque<QueuedInput>,
    next_queued_id: QueuedInputId,
    queue_sender: Option<mpsc::UnboundedSender<Vec<QueueDelivery>>>,
    queue_activity_revision: u64,
    /// The root journaled a failed turn the queue has not been held for. Failures
    /// journaled before the session was shown hold nothing: no input queued
    /// behind them.
    root_failed: bool,
    /// Why queue delivery is paused, if it is.
    pub paused: Option<Paused>,
    pub prompts: VecDeque<UiPrompt>,
    /// The factory's form, edited while the draft is written, then frozen in view.
    pub scope: Option<scope::ScopePanel>,
    /// The menu over the conversation, which takes the keys.
    pub menu: Option<Menu>,
    /// Taken by the host after each event.
    pub host: Option<HostRequest>,
    /// Every open session not closing, this one included; kept current by the host.
    pub peers: Vec<Peer>,
    /// This session as the others last saw it, and what that was drawn from.
    peer: Option<(session::PeerKey, Peer)>,
    next_menu_id: MenuId,
    // Pending attachment reads belong to one composer draft, not the next submission/session.
    // Local edits, clear, and composer-only history navigation preserve it.
    draft_ticket: Ticket,
    pub status: super::status::StatusLog,
    // UI-only notices, including startup diagnostics and failed status writes.
    unsaved_status: Vec<model::UnsavedStatus>,
    /// The session's title as the session list shows it.
    pub title: Option<SessionTitle>,
    title_ticket: Ticket,
    pub stage: Stage,
    pub details: bool,
    pub outputs: OutputStore,
    /// Held by each task the session stays open for: an export or the activation of a
    /// picked model.
    tasks: std::sync::Arc<()>,
    pub tx: mpsc::UnboundedSender<Work>,
    pub leader: Option<KeyEvent>,
    pub toast: Option<(String, Instant)>,
    pub dirty: bool,
    pub content_dirty: bool,
    /// The composer's first shown row while scrolled away from the cursor; none
    /// follows the cursor.
    pub composer_scroll: Option<usize>,
    pub(super) content_cache: model::ContentCache,
    pub tick_count: usize,
    /// The wall time timings are shown at, advanced each tick.
    pub clock: format::Clock,
    pub clipboard: Option<String>,
    pub selection: Option<super::render::Selection>,
    gesture: Gesture,
    pub hover: Option<(u16, u16)>,
    pub render: super::render::RenderState,
    /// What the viewed tab's window reads and keeps.
    pub budget: model::Budget,
    /// Every record observed, in order, for tests to read what the session journaled.
    #[cfg(test)]
    pub journal: Vec<std::sync::Arc<skyhook::session::EventRecord>>,
}
impl App {
    pub fn entries(&self) -> &[Entry] {
        self.content_cache.entries()
    }

    fn advance_draft(&mut self) {
        self.draft_ticket = Ticket::default();
    }

    fn replace_draft(&mut self, submission: Submission) {
        self.advance_draft();
        self.editor.set_submission(submission);
        self.history_browse = None;
    }

    /// Save the preferences as they are now; a failure toasts what it saved.
    fn save_prefs(&mut self, what: &'static str) {
        let tx = self.tx.clone();
        self.prefs.save(move |saved| {
            let saved = saved.map_err(|error| format!("Could not save {what}: {error}"));
            let _ = tx.send(Work::PrefsSaved(saved));
        });
    }

    /// A new session starts in `mode` when the modes on offer have it, and in
    /// the configured default otherwise. An opened session's root keeps its own
    /// mode where the session still offers it.
    pub fn new(
        observation: Option<PreparedObservation>,
        launch: Launch,
        ui: Arc<UiInteraction>,
        mode: Option<&ModeName>,
        prefs: Rc<Prefs>,
        tx: mpsc::UnboundedSender<Work>,
    ) -> Self {
        let draft = observation.is_none();
        let mut scope = None;
        let (phase, snapshot, projection, window, budget) = match observation {
            Some(PreparedObservation {
                active,
                scope: opened,
                budget,
                shown:
                    observation::Shown {
                        snapshot,
                        projection,
                        window,
                        ..
                    },
            }) => (
                {
                    scope = opened.and_then(scope::ScopePanel::sent);
                    Phase::Open {
                        observation: active,
                        attached_draft: None,
                        owns_root: false,
                    }
                },
                *snapshot,
                *projection,
                window.map(|window| *window),
                budget,
            ),
            None => (
                Phase::Draft {
                    root: draft_root(),
                    start: None,
                },
                ObservationSnapshot::default(),
                Projection::empty(&launch.identity().host),
                None,
                model::Budget::DEFAULT,
            ),
        };
        let selected = match &phase {
            Phase::Draft { root, .. } => root.clone(),
            Phase::Open { observation, .. } => observation.session.root_agent().clone(),
        };
        let mut app = Self {
            model: launch.model().name(),
            mode: launch.model().config().default_mode().clone(),
            prefs,
            phase,
            launch,
            ui,
            snapshot,
            projection,
            window,
            selected: selected.clone(),
            tab: Tab::default(),
            panel: Shown::Transcript,
            extensions: Default::default(),
            views: Views::default(),
            focus: Pane::Composer,
            tree_cursor: 0,
            unread_responses: HashSet::new(),
            tree_view: Viewport::default(),
            editor: Composer::default(),
            history: vec![],
            history_browse: None,
            queue: VecDeque::new(),
            next_queued_id: QueuedInputId::default(),
            queue_sender: None,
            queue_activity_revision: 0,
            root_failed: false,
            paused: None,
            prompts: VecDeque::new(),
            scope,
            menu: None,
            host: None,
            peers: Vec::new(),
            peer: None,
            next_menu_id: MenuId::default(),
            draft_ticket: Ticket::default(),
            status: super::status::StatusLog::new(tx.clone()),
            unsaved_status: Vec::new(),
            title: None,
            title_ticket: Ticket::default(),
            stage: Stage::Running,
            details: false,
            outputs: OutputStore::default(),
            tasks: Default::default(),
            tx: tx.clone(),
            leader: None,
            toast: None,
            dirty: true,
            content_dirty: true,
            composer_scroll: None,
            content_cache: model::ContentCache::default(),
            tick_count: 0,
            clock: format::Clock::now(),
            clipboard: None,
            selection: None,
            gesture: Gesture::Released,
            hover: None,
            render: super::render::RenderState::new(selected, tx),
            budget,
            #[cfg(test)]
            journal: Vec::new(),
        };
        app.refresh();
        if let Some(mode) = mode {
            app.select_mode(mode);
        }
        let mut agents = app.projection.agents.iter();
        let root = agents.find(|agent| agent.id == app.selected);
        if let Some(root) = root {
            app.model = root.outline.model.clone();
            if let Some(mode) = root.outline.mode.clone() {
                app.select_mode(&mode);
            }
        }
        app.show_warnings();
        app.load_title();
        if draft {
            app.open_scope_panel();
        }
        app
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::state::{self, SavedState};
    pub(super) use crate::tests::bounded;
    use crate::tests::{CONFIG, Fixture};
    use crate::tui::model::View;
    use crossterm::event::Event;
    use crossterm::event::KeyCode;
    use crossterm::event::KeyModifiers as M;
    use crossterm::event::MouseButton;
    use crossterm::event::MouseEventKind;
    pub(super) use serde_json::json;
    use skyhook::agent::SessionHandle;
    pub(super) use skyhook::agent::{Question, QuestionAnswer, QuestionOption, QuestionReply};
    use skyhook::interaction::Prompt;
    use skyhook::interaction::PromptKind;
    use skyhook::session::RuntimeEvent;
    use skyhook::session::SessionEvent;
    use skyhook_media::Attachment;
    use skyhook_worker::job::JobId;
    use std::sync::Arc;
    pub(super) use tokio::sync::oneshot;

    impl App {
        /// A session opened as this app opens one, its prompts asking this app.
        pub(crate) async fn create(
            &self,
            opening: skyhook::product::Opening<'_>,
        ) -> Result<skyhook::launch::OpenedSession, skyhook::launch::LaunchError> {
            let interface = skyhook::launch::Interface::Attended(self.ui.clone());
            self.launch.create(opening, &interface).await
        }
    }

    /// `product`'s launch in `root` as the terminal starts one, configured by `yaml` with
    /// its root in `permissions`.
    pub(crate) async fn launch_of(
        product: &impl skyhook::product::Product<Options = ()>,
        root: &std::path::Path,
        yaml: &str,
        permissions: skyhook::launch::request::PermissionRequest,
    ) -> Launch {
        use skyhook::launch::{config::HostConfig, request::ExecutionRequest};
        use skyhook::product::Attendance::Attended;
        let config = skyhook::yaml::parse::<HostConfig>(yaml).unwrap();
        let request = ExecutionRequest {
            resume: None,
            model: None,
        };
        let host = skyhook::launch::configure(product, (), config, root, &request, Attended);
        let host = host.await.unwrap();
        let (prefs, _) = Prefs::load(host.workspace(), "state.json");
        let saved = prefs.saved.into_inner();
        let remembered = (saved.model.as_ref(), saved.mode.as_ref());
        let catalog = skyhook::remote::EmbeddedShimCatalog::default();
        skyhook::launch::startup::prepare(host, &request, &permissions, remembered, catalog)
            .unwrap()
    }

    impl App {
        /// Open this app's next sessions as `product`'s, configured by `yaml` with the
        /// root in `permissions`.
        pub(crate) async fn relaunch(
            &mut self,
            product: &impl skyhook::product::Product<Options = ()>,
            yaml: &str,
            permissions: skyhook::launch::request::PermissionRequest,
        ) {
            let root = self.launch.workspace().to_owned();
            self.launch = launch_of(product, &root, yaml, permissions).await;
        }

        /// [`App::relaunch`] as Skyhook, its root in the default mode.
        pub(crate) async fn reconfigure(&mut self, yaml: &str) {
            let permissions = skyhook::launch::request::PermissionRequest::Mode(None);
            self.relaunch(&skyhook::product::Skyhook, yaml, permissions)
                .await;
        }
    }

    pub(in super::super) async fn draft_fixture() -> (tempfile::TempDir, App) {
        let root = tempfile::tempdir().unwrap();
        let permissions = skyhook::launch::request::PermissionRequest::Mode(None);
        let product = skyhook::product::Skyhook;
        let launch = launch_of(&product, root.path(), CONFIG, permissions).await;
        let (tx, _) = mpsc::unbounded_channel();
        let general = "general".parse().unwrap();
        let (ui, _) = crate::tui::shell::prompts();
        let (prefs, _) = Prefs::load(root.path(), "state.json");
        let app = App::new(None, launch, ui, Some(&general), Rc::new(prefs), tx);
        // Unit fixtures must not change the user's global model preference.
        let mut saved = app.prefs.saved.borrow_mut();
        saved.model = Some("test/first".parse().unwrap());
        saved.mode = Some("general".parse().unwrap());
        drop(saved);
        (root, app)
    }
    pub(in super::super) async fn fixture() -> (tempfile::TempDir, App) {
        fixture_within(model::Budget::DEFAULT).await
    }
    /// [`fixture`], its windows reading and keeping `budget`.
    pub(in super::super) async fn fixture_within(
        budget: model::Budget,
    ) -> (tempfile::TempDir, App) {
        let (root, mut app) = draft_fixture().await;
        app.budget = budget;
        let session = app
            .create(skyhook::product::Opening::New(None))
            .await
            .unwrap()
            .session;
        attach(&mut app, session).await;
        (root, app)
    }
    /// Recorded by a different host, reopened without its capabilities or modes.
    pub(crate) async fn foreign_fixture() -> (tempfile::TempDir, App) {
        use skyhook::tool::policy::AllowAll;
        use skyhook::tool::policy::{Capability, HostCapability};
        let (root, mut app) = draft_fixture().await;
        let review = "example.review".parse::<HostCapability>().unwrap();
        let report = "example.report_detailed_findings"
            .parse::<HostCapability>()
            .unwrap();
        let mut other = Fixture::default();
        other.identity.host = "other-host".parse().unwrap();
        let mode = skyhook::launch::request::PermissionRequest::Mode(None);
        let other_host = launch_of(&other, root.path(), CONFIG, mode).await;
        let (owner, mut model) =
            played::played_harness(&other_host, Arc::new(AllowAll), |builder| {
                builder
                    .host_capabilities([review.clone(), report.clone()].map(|name| {
                        skyhook::tool::policy::HostCapabilityDeclaration {
                            name,
                            title: "Host action",
                        }
                    }))
                    .modes(Default::default())
                    .capabilities(
                        [
                            Capability::Read,
                            Capability::Host(review),
                            Capability::Host(report),
                        ]
                        .into_iter()
                        .collect(),
                    )
            })
            .await;
        let session = owner.new_session().await.unwrap();
        let asked = played::ask(&session, "Foreign prompt");
        model.turn().await.answer("Foreign response");
        bounded(asked).await.unwrap().unwrap();
        session.rename(Some("Other session".into())).await.unwrap();
        let id = session.id();
        session.shutdown().await.unwrap();
        session.close().await.unwrap();
        let foreign = app
            .create(skyhook::product::Opening::Resume(id))
            .await
            .unwrap()
            .session;
        let title = foreign.store().title().await.unwrap();
        attach(&mut app, foreign).await;
        app.title = title;
        (root, app)
    }

    /// A session that approves every tool call, attached.
    pub(crate) async fn approving_fixture() -> (tempfile::TempDir, App, SessionHandle) {
        let (root, mut app) = draft_fixture().await;
        app.reconfigure(&format!("{CONFIG}approve_all: true\n"))
            .await;
        let session = app
            .create(skyhook::product::Opening::New(None))
            .await
            .unwrap()
            .session;
        attach(&mut app, session.clone()).await;
        (root, app, session)
    }
    /// Load `job`'s output as the UI does, shown or not.
    pub(crate) async fn fetch_output(app: &mut App, job: JobId) {
        let mut rx = capture_work(app);
        draw(app);
        // A no-op once drawing began this job's load.
        app.fetch_output(job);
        loop {
            let work = recv(&mut rx).await;
            let loaded = matches!(&work, Work::Output { attempt, .. } if attempt.job() == job);
            app.work(work);
            if loaded {
                break;
            }
        }
    }
    /// `session` observed, as opening it reads it.
    pub(crate) async fn prepared(session: SessionHandle) -> PreparedObservation {
        prepared_within(session, model::Budget::DEFAULT).await
    }
    /// [`prepared`], its windows reading and keeping `budget`.
    async fn prepared_within(session: SessionHandle, budget: model::Budget) -> PreparedObservation {
        crate::tests::bounded(PreparedObservation::load(session, Arc::new(()), budget))
            .await
            .unwrap()
    }
    /// Every record `session` journaled, in order, read from its store.
    pub(crate) async fn journal(
        session: &SessionHandle,
    ) -> Vec<std::sync::Arc<skyhook::session::EventRecord>> {
        crate::tests::bounded(async {
            let agents = session.store().agents().await.unwrap();
            let mut records = Vec::new();
            for agent in agents.agents {
                let mut page = Some(skyhook::session::Page::First);
                let size = model::Budget::DEFAULT.page;
                while let Some(read) = page {
                    let store = session.store();
                    let read = store.transcript(&agent.agent, read, size).await;
                    let read = read.unwrap();
                    page = (read.records.len() == size)
                        .then(|| skyhook::session::Page::After(read.first + size as u64 - 1));
                    records.extend(read.records);
                }
            }
            records.sort_unstable_by_key(|record| record.sequence);
            records
        })
        .await
    }
    /// What the app shows of one agent's conversation, read whole from a store.
    pub(crate) struct Whole {
        pub(crate) live: skyhook::session::ObservationSnapshot,
        pub(crate) window: model::Window,
        pub(crate) projection: Projection,
    }

    impl Whole {
        /// The entries a fresh build shows, every detail `open` or none.
        pub(crate) fn entries(&self, outputs: &OutputStore, open: bool) -> Vec<Entry> {
            let view = View::default();
            let presentation = model::EntryView {
                agent: &self.window.agent,
                tab: self.window.tab,
                view: &view,
                all_details: open,
                today: Default::default(),
                scope: None,
            };
            model::entries(self.inputs(presentation, outputs), true)
        }

        pub(crate) fn inputs<'a>(
            &'a self,
            presentation: model::EntryView<'a>,
            outputs: &'a OutputStore,
        ) -> model::Inputs<'a> {
            model::Inputs {
                live: &self.live,
                window: &self.window,
                projection: &self.projection,
                presentation,
                outputs,
                unsaved: &[],
            }
        }
    }

    /// What the app shows of `agent`'s conversation, read whole from `session`'s store:
    /// its live slice, every page of it, and every agent's summary.
    pub(crate) async fn read(session: &SessionHandle, agent: &AgentId) -> Whole {
        crate::tests::bounded(async {
            let prepared = prepared(session.clone()).await;
            Whole {
                live: *prepared.shown.snapshot,
                window: pages::whole(session, agent).await.unwrap(),
                projection: *prepared.shown.projection,
            }
        })
        .await
    }

    /// The viewed conversation as a fresh build shows it once `app` has applied every
    /// record: its pages read whole from the store, beside what `app` holds live.
    pub(crate) async fn whole_entries(app: &mut App, details: bool) -> Vec<Entry> {
        let session = app.session().unwrap().clone();
        let newest = journal(&session).await.last().map(|record| record.sequence);
        let applied = |app: &App| Some(app.snapshot.applied) >= newest;
        played::observe_until(app, applied).await;
        let mut read = read(&session, &app.selected).await;
        read.live = app.snapshot.clone();
        read.entries(&app.outputs, details)
    }
    /// Observe the session again as a lagged app does, installing what it reads.
    pub(crate) async fn resubscribed(app: &mut App) {
        let mut rx = capture_work(app);
        app.resubscribe();
        bounded(async {
            loop {
                let work = rx.recv().await.expect("work channel open");
                let observed = matches!(work, Work::Observed { .. });
                app.work(work);
                if observed {
                    break;
                }
            }
        })
        .await;
    }
    /// Replace a draft fixture with one opened on `session`.
    /// `session` attached to `app`, its windows reading and keeping `app`'s budget.
    pub(crate) async fn attach(app: &mut App, session: SessionHandle) {
        let observation = prepared_within(session, app.budget).await;
        *app = app.sibling(
            Some(observation),
            app.launch.clone(),
            app.ui.clone(),
            app.tx.clone(),
        );
    }
    /// A 1x1 PNG.
    pub(super) fn png() -> Vec<u8> {
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::RgbImage::new(1, 1)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        bytes.into_inner()
    }
    /// An image attachment of [`png`].
    pub(super) async fn png_attachment(name: &str) -> Attachment {
        Attachment::Image {
            file: Some(name.into()),
            image: skyhook_media::Image::decode(png()).await.unwrap(),
        }
    }
    /// The entries a fresh build of `app`'s view shows from what the app holds.
    pub(crate) fn fresh(app: &App) -> Vec<Entry> {
        let view = View::default();
        let presentation = model::EntryView {
            agent: &app.selected,
            tab: app.tab,
            view: app.viewed().unwrap_or(&view),
            all_details: app.details,
            today: app.clock.day(),
            scope: None,
        };
        let inputs = model::Inputs {
            live: &app.snapshot,
            window: app.window.as_ref().expect("a viewed window"),
            projection: &app.projection,
            presentation,
            outputs: &app.outputs,
            unsaved: &app.unsaved_status,
        };
        model::entries(inputs, true)
    }
    /// An app of its own viewing `session`'s `tab`, its first page loaded.
    pub(crate) async fn viewing(session: &SessionHandle, tab: Tab) -> (tempfile::TempDir, App) {
        viewing_within(session, tab, model::Budget::DEFAULT).await
    }
    /// [`viewing`], its windows reading and keeping `budget`.
    pub(crate) async fn viewing_within(
        session: &SessionHandle,
        tab: Tab,
        budget: model::Budget,
    ) -> (tempfile::TempDir, App) {
        let (root, mut app) = draft_fixture().await;
        app.budget = budget;
        attach(&mut app, session.clone()).await;
        app.tab = tab;
        settled(&mut app).await;
        (root, app)
    }
    /// Draw until the viewed tab has loaded what the view shows, applying the work
    /// its loads send.
    pub(crate) async fn settled(app: &mut App) {
        let mut rx = capture_work(app);
        bounded(async {
            loop {
                draw(app);
                if !app.window.as_ref().is_some_and(model::Window::loading) {
                    break;
                }
                let work = rx.recv().await.expect("work channel open");
                app.work(work);
            }
        })
        .await;
    }
    pub(crate) fn capture_work(app: &mut App) -> mpsc::UnboundedReceiver<Work> {
        let (tx, rx) = mpsc::unbounded_channel();
        app.tx = tx;
        rx
    }
    /// Observe records until a title change, which is returned.
    pub(super) async fn next_title_change(app: &mut App) -> SessionEvent {
        bounded(async {
            loop {
                let event = app.recv_observation().await.expect("observation open");
                let title = match &event.event {
                    RuntimeEvent::Record(record) => matches!(
                        record.event,
                        SessionEvent::TitleSet { .. } | SessionEvent::TitleCleared
                    )
                    .then(|| record.event.clone()),
                    _ => None,
                };
                app.observe(event);
                if let Some(title) = title {
                    return title;
                }
            }
        })
        .await
    }
    pub(super) async fn next_lifecycle(rx: &mut mpsc::UnboundedReceiver<Work>) -> Work {
        bounded(async {
            loop {
                let work = rx.recv().await.expect("work channel open");
                if matches!(work, Work::Started { .. } | Work::Stopped) {
                    return work;
                }
            }
        })
        .await
    }
    pub(super) fn key(app: &mut App, code: KeyCode, modifiers: M) {
        app.event(Event::Key(KeyEvent::new(code, modifiers)));
    }
    pub(crate) fn question(
        app: &mut App,
        prompt: String,
        options: Vec<QuestionOption>,
    ) -> QuestionReceiver {
        questions(app, false, vec![("answer", &prompt, options)])
    }
    /// A reply answering each question header with plain text.
    pub(super) fn texts(answers: &[(&str, &str)]) -> QuestionReply {
        let answers = answers.iter();
        let text = |text: &str| QuestionAnswer::Text(text.into());
        answers
            .map(|(header, answer)| (header.parse().unwrap(), text(answer)))
            .collect()
    }
    /// A suggestion labelled `label`, described by `description`.
    pub(crate) fn option(label: &str, description: &str) -> QuestionOption {
        QuestionOption {
            label: label.parse().unwrap(),
            description: description.parse().unwrap(),
            recommended: false,
            preview: None,
        }
    }
    /// A question titled `header` asking `prompt`, taking one answer.
    pub(crate) fn titled(header: &str, prompt: &str, options: Vec<QuestionOption>) -> Question {
        Question {
            header: header.parse().unwrap(),
            prompt: prompt.parse().unwrap(),
            options,
            multiple: false,
        }
    }
    pub(super) fn questions(
        app: &mut App,
        background: bool,
        questions: Vec<(&str, &str, Vec<QuestionOption>)>,
    ) -> QuestionReceiver {
        let questions = questions
            .into_iter()
            .map(|(header, prompt, options)| titled(header, prompt, options));
        asking(app, background, questions.collect())
    }
    /// Have the root ask `questions` as one batch.
    pub(crate) fn asking(
        app: &mut App,
        background: bool,
        questions: Vec<Question>,
    ) -> QuestionReceiver {
        let (reply, receiver) = oneshot::channel();
        app.prompt(Prompt {
            id: 1,
            kind: PromptKind::Questions {
                questions,
                agent: app.session().unwrap().root_agent().clone(),
                background,
                reply,
            },
        });
        receiver
    }
    /// The agent's view of a question batch.
    pub(crate) type QuestionReceiver =
        oneshot::Receiver<Result<QuestionReply, skyhook::interaction::Dismissed>>;
    /// Whether a reply is still outstanding.
    pub(super) trait Pending {
        fn is_pending(&mut self) -> bool;
    }
    impl<T> Pending for oneshot::Receiver<T> {
        fn is_pending(&mut self) -> bool {
            matches!(self.try_recv(), Err(oneshot::error::TryRecvError::Empty))
        }
    }
    pub(super) fn draw_buffer(app: &mut App) -> ratatui::buffer::Buffer {
        draw_sized_buffer(app, 60, 24)
    }
    /// A test terminal kept across frames, which repaints only the cells a frame changes.
    pub(crate) struct Screen(ratatui::Terminal<ratatui::backend::TestBackend>);
    impl Screen {
        pub(crate) fn new(width: u16, height: u16) -> Self {
            let backend = ratatui::backend::TestBackend::new(width, height);
            Self(ratatui::Terminal::new(backend).unwrap())
        }
        pub(crate) fn draw(&mut self, app: &mut App) -> &ratatui::buffer::Buffer {
            self.0
                .draw(|frame| crate::tui::render::draw(frame, app))
                .unwrap();
            self.0.backend().buffer()
        }
    }
    pub(crate) fn draw_sized_buffer(
        app: &mut App,
        width: u16,
        height: u16,
    ) -> ratatui::buffer::Buffer {
        Screen::new(width, height).draw(app).clone()
    }
    pub(in super::super) fn draw(app: &mut App) -> String {
        draw_sized(app, 60, 24)
    }
    pub(crate) fn draw_sized(app: &mut App, width: u16, height: u16) -> String {
        draw_sized_buffer(app, width, height)
            .content
            .chunks(width.into())
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }
    pub(super) fn mouse(app: &mut App, rect: Rect, kind: MouseEventKind) {
        app.event(Event::Mouse(crossterm::event::MouseEvent {
            kind,
            column: rect.x,
            row: rect.y,
            modifiers: M::NONE,
        }));
    }
    pub(super) fn click(app: &mut App, rect: Rect) {
        mouse(app, rect, MouseEventKind::Down(MouseButton::Left));
        mouse(app, rect, MouseEventKind::Up(MouseButton::Left));
    }
    pub(crate) async fn recv(rx: &mut mpsc::UnboundedReceiver<Work>) -> Work {
        bounded(rx.recv()).await.expect("work channel open")
    }
    /// Apply work until every preference save `app` started has finished, returning
    /// the state file as they left it.
    pub(crate) async fn prefs_saved(
        app: &mut App,
        rx: &mut mpsc::UnboundedReceiver<Work>,
    ) -> SavedState {
        while !app.prefs.written() {
            let work = recv(rx).await;
            app.work(work);
        }
        state::load(&app.prefs.path).0
    }
    pub(super) fn pending(response: &mut impl Pending) -> bool {
        response.is_pending()
    }
    /// Press `Ctrl+X` followed by `code`.
    pub(super) fn chord(app: &mut App, code: KeyCode) {
        key(app, KeyCode::Char('x'), M::CONTROL);
        key(app, code, M::NONE);
    }
    /// Press each key without modifiers.
    pub(super) fn press(app: &mut App, codes: &[KeyCode]) {
        for code in codes {
            key(app, *code, M::NONE);
        }
    }
    /// Run a script to completion and observe what the app is told of it.
    pub(crate) async fn run_script(app: &mut App, source: &str) {
        let session = app.session().unwrap().clone();
        let script = session.run_script(source).await.unwrap();
        drop(script);
        let done = |app: &App| {
            app.snapshot
                .jobs
                .values()
                .all(|job| job.state.is_terminal())
        };
        played::observe_until(app, |app| !app.snapshot.jobs.is_empty() && done(app)).await;
    }
    pub(crate) fn job_named(app: &App, tool: &str) -> JobId {
        let mut jobs = app.snapshot.jobs.values();
        jobs.find(|job| job.tool == tool).unwrap().id
    }
    /// Show `message` unsaved in `app`'s root conversation, after its entry `after`.
    pub(crate) fn unsaved_at(app: &mut App, after: Option<u64>, message: &str) {
        let agent = app.root_agent().clone();
        let message = message.to_owned();
        app.unsaved_status.push(model::UnsavedStatus {
            agent,
            after,
            message,
        });
        app.content_dirty = true;
    }
    /// The unsaved statuses shown in `agent`'s conversation.
    pub(crate) fn unsaved(app: &App, agent: &AgentId) -> Vec<String> {
        let statuses = app.unsaved_status.iter();
        let statuses = statuses.filter(|status| &status.agent == agent);
        statuses.map(|status| status.message.clone()).collect()
    }
    /// Startup warnings are shown once and survive rebuilds.
    async fn assert_startup_warnings_ui_only(app: &mut App) {
        let session = app.session().unwrap().clone();
        let warnings = session.warnings().to_vec();
        assert!(!warnings.is_empty());
        let root = session.root_agent().clone();
        for warning in &warnings {
            let status = format!("Startup warning: {warning}");
            assert!(unsaved(app, &root).contains(&status));
        }
        for _ in 0..2 {
            app.refresh();
            app.rebuild_content();
            for warning in &warnings {
                let status = format!("Status · Startup warning: {warning}");
                let entries = app.entries().iter();
                assert_eq!(entries.filter(|entry| entry.text() == status).count(), 1);
            }
        }
    }
    #[tokio::test]
    async fn startup_warnings_are_ui_only_on_initial_started_and_resumed_sessions() {
        let (_root, mut draft) = draft_fixture().await;
        let missing_command = draft.launch.workspace().join("missing-mcp-server");
        draft
            .reconfigure(&format!(
                "{CONFIG}mcp:\n  unavailable:\n    transport: stdio\n    start_command: [{}]\n    startup_timeout_secs: 1",
                serde_json::to_string(&missing_command.to_string_lossy()).unwrap(),
            ))
            .await;
        let session = draft
            .create(skyhook::product::Opening::New(None))
            .await
            .unwrap()
            .session;
        let id = session.id();
        let (tx, _) = mpsc::unbounded_channel();
        let observation = prepared(session.clone()).await;
        let mut initial = App::new(
            Some(observation),
            draft.launch.clone(),
            draft.ui.clone(),
            Some(&"general".parse().unwrap()),
            draft.prefs.clone(),
            tx,
        );
        assert_startup_warnings_ui_only(&mut initial).await;
        draft.session_started(prepared(session.clone()).await);
        assert_startup_warnings_ui_only(&mut draft).await;
        session.shutdown().await.unwrap();
        session.close().await.unwrap();
        let mut resumed = draft.sibling(
            None,
            draft.launch.clone(),
            draft.ui.clone(),
            draft.tx.clone(),
        );
        drop((initial, draft, session));

        let session = resumed
            .create(skyhook::product::Opening::Resume(id))
            .await
            .unwrap()
            .session;
        attach(&mut resumed, session).await;
        assert_startup_warnings_ui_only(&mut resumed).await;
        resumed.session().unwrap().shutdown().await.unwrap();
    }
}
