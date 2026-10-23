use events::Gesture;
pub use events::{Hit, Operation, Work};
pub use input::InputTarget;
use lifecycle::PendingStart;
pub use menus::{
    ConfirmAction, Emphasis, Item, ItemRef, Menu, MenuId, MenuKind, MenuLayout, SavedState,
    SessionRef,
};
use prompts::UiPrompt;
pub use queue::{Delivered, Paused, QueuedInput, QueuedInputId};
use queue::{QueueDelivery, RowState};
use skyhook::{provider::profile::ModelRef, tool::policy::ModeName};
mod events;
mod input;
mod lifecycle;
mod observation;
use observation::ActiveObservation;
pub use observation::PreparedObservation;
mod menus;
mod output;
mod pages;
mod projection;
pub use output::OutputStore;
use output::{LoadedOutput, OutputAttempt};
#[cfg(test)]
pub(super) mod played;
mod prompts;
mod queue;
mod session;
pub use session::{HostRequest, Peer, PeerSession, SlotKey};

use super::{
    Launch,
    composer::{Composer, Submission},
    editor::{Editor, TextField},
    keys::{Command, KeyMap},
    model::{self, Entry, Projection, Tab, View, Views},
    viewport::Viewport,
};
use crate::interaction::{ApprovalReply, Prompt, PromptKind};
use crate::{state, text::brief};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers as M, MouseButton, MouseEventKind,
};
use ratatui::layout::Rect;
use skyhook::media::Attachment;
use skyhook::{
    agent::{
        AgentActivity, ObservationSnapshot, ObservedEvent, RuntimeEvent, SessionHandle, TurnFailure,
    },
    identity::{AgentId, JobId, SessionId},
    job::JobOutputQuery,
    session::{Message, SessionEvent, SessionTitle},
};
use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
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

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Composer,
    Tree,
    Content,
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

/// Pointer identity of one local asynchronous attempt: a composer draft's
/// attachment reads or an output refresh. Fresh on
/// `default()`, equal only to its clones, and never reconstructible from a
/// later request with identical content.
#[derive(Clone, Default)]
pub struct Token(std::sync::Arc<()>);
impl Token {
    fn matches(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0)
    }
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
    },
    Open {
        observation: ActiveObservation,
        attached_draft: Option<AgentId>,
    },
}

pub struct App {
    phase: Phase,
    pub launch: Launch,
    /// UI-only choice, captured by each submitted user message.
    pub model: ModelRef,
    /// UI-only choice like `model`; a new session starts in it.
    pub mode: ModeName,
    remembered_model: Option<ModelRef>,
    remembered_mode: Option<ModeName>,
    pub sidebar: bool,
    pub snapshot: ObservationSnapshot,
    pub projection: Projection,
    /// The viewed agent's tab, loaded around the view; none while out of view.
    pub window: Option<model::Window>,
    pub selected: AgentId,
    /// Shared across agents; reading and expansion state remain in `views`.
    pub tab: Tab,
    pub views: Views,
    pub focus: Focus,
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
    /// Creation owns its pending action until the asynchronous task completes.
    creating: Option<PendingStart>,
    /// Why queue delivery is paused, if it is.
    pub paused: Option<Paused>,
    pub operation: bool,
    pub prompts: VecDeque<UiPrompt>,
    pub prompt_body_rect: Rect,
    pub prompt_options_rect: Rect,
    pub prompt_body_rows: usize,
    pub prompt_option_rows: usize,
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
    draft_ticket: Token,
    pub status: super::status::StatusLog,
    // UI-only notices, including startup diagnostics and failed status writes.
    unsaved_status: Vec<model::UnsavedStatus>,
    /// The session's title as the session list shows it.
    pub title: Option<SessionTitle>,
    title_ticket: Token,
    pub stopping: bool,
    pub animating: bool,
    pub details: bool,
    pub outputs: OutputStore,
    /// Held by each task the session stays open for: an export, or the
    /// activation of a picked model.
    tasks: std::sync::Arc<()>,
    pub tx: mpsc::UnboundedSender<Work>,
    pub keys: KeyMap,
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
    pub clock: model::Clock,
    pub exit: bool,
    pub clipboard: Option<String>,
    pub hits: Vec<(Rect, Hit)>,
    pub content_rect: Rect,
    pub tree_rect: Rect,
    pub composer_rect: Rect,
    pub selection: Option<super::render::Selection>,
    gesture: Gesture,
    pub hover: Option<(u16, u16)>,
    pub render: super::render::RenderState,
    /// Every record observed, in order, for tests to read what the session journaled.
    #[cfg(test)]
    pub journal: Vec<std::sync::Arc<skyhook::session::EventRecord>>,
    /// Page loads asked for, for tests.
    #[cfg(test)]
    pub loads: usize,
}
impl App {
    pub fn entries(&self) -> &[Entry] {
        self.content_cache.entries()
    }

    fn advance_draft(&mut self) {
        self.draft_ticket = Token::default();
    }

    fn replace_draft(&mut self, submission: Submission) {
        self.advance_draft();
        self.editor.set_submission(submission);
        self.history_browse = None;
    }

    /// A new session starts in `mode` when the modes on offer have it, and in
    /// the configured default otherwise. An opened session's root keeps its own
    /// mode where the session still offers it.
    pub fn new(
        observation: Option<PreparedObservation>,
        launch: Launch,
        mode: &ModeName,
        saved: state::SavedState,
        tx: mpsc::UnboundedSender<Work>,
    ) -> Self {
        let (phase, snapshot, projection, window) = match observation {
            Some(PreparedObservation {
                active,
                snapshot,
                projection,
                window,
                ..
            }) => (
                Phase::Open {
                    observation: active,
                    attached_draft: None,
                },
                *snapshot,
                *projection,
                window.map(|window| *window),
            ),
            None => (
                Phase::Draft { root: draft_root() },
                ObservationSnapshot::default(),
                Projection::default(),
                None,
            ),
        };
        let selected = match &phase {
            Phase::Draft { root } => root.clone(),
            Phase::Open { observation, .. } => observation.session.root_agent().clone(),
        };
        let mut app = Self {
            model: launch.model.name(),
            mode: launch.model.config().default_mode().clone(),
            remembered_model: saved.model,
            remembered_mode: saved.mode,
            sidebar: saved.sidebar,
            phase,
            launch,
            snapshot,
            projection,
            window,
            selected: selected.clone(),
            tab: Tab::default(),
            views: Views::default(),
            focus: Focus::Composer,
            tree_cursor: 0,
            tree_view: Viewport::default(),
            editor: Composer::default(),
            history: vec![],
            history_browse: None,
            queue: VecDeque::new(),
            next_queued_id: QueuedInputId::default(),
            queue_sender: None,
            queue_activity_revision: 0,
            root_failed: false,
            creating: None,
            paused: None,
            operation: false,
            prompts: VecDeque::new(),
            prompt_body_rect: Rect::default(),
            prompt_options_rect: Rect::default(),
            prompt_body_rows: 0,
            prompt_option_rows: 0,
            menu: None,
            host: None,
            peers: Vec::new(),
            peer: None,
            next_menu_id: MenuId::default(),
            draft_ticket: Token::default(),
            status: super::status::StatusLog::new(tx.clone()),
            unsaved_status: Vec::new(),
            title: None,
            title_ticket: Token::default(),
            stopping: false,
            animating: false,
            details: false,
            outputs: OutputStore::default(),
            tasks: Default::default(),
            tx: tx.clone(),
            keys: KeyMap::default(),
            leader: None,
            toast: None,
            dirty: true,
            content_dirty: true,
            composer_scroll: None,
            content_cache: model::ContentCache::default(),
            tick_count: 0,
            clock: model::Clock::now(),
            exit: false,
            clipboard: None,
            hits: vec![],
            content_rect: Rect::default(),
            tree_rect: Rect::default(),
            composer_rect: Rect::default(),
            selection: None,
            gesture: Gesture::Released,
            hover: None,
            render: super::render::RenderState::new(selected, tx),
            #[cfg(test)]
            journal: Vec::new(),
            #[cfg(test)]
            loads: 0,
        };
        app.refresh();
        app.select_mode(mode);
        let mut agents = app.projection.agents.iter();
        let root = agents.find(|agent| agent.id == app.selected);
        if let Some(root) = root {
            app.model = root.model.clone();
            if let Some(mode) = root.mode.clone() {
                app.select_mode(&mode);
            }
        }
        app.show_warnings();
        app.load_title();
        app
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::interaction::UiInteraction;
    pub(super) use crate::tests::bounded;
    pub(super) use serde_json::json;
    pub(super) use skyhook::agent::{Question, QuestionAnswer, QuestionOption, QuestionReply};
    use skyhook::remote::EmbeddedShimCatalog;
    use skyhook::session::SessionEvent;
    use std::sync::Arc;
    pub(super) use tokio::sync::oneshot;

    pub(in super::super) async fn draft_fixture() -> (tempfile::TempDir, App) {
        let root = tempfile::tempdir().unwrap();
        let config = crate::interaction::tests::test_config();
        let (interaction, _) = UiInteraction::new();
        let launch = Launch {
            model: config
                .into_runtime()
                .unwrap()
                .select_model(&"test/first".parse().unwrap())
                .unwrap(),
            permissions: crate::launch::Permissions::Mode("general".parse().unwrap()),
            workspace: root.path().to_path_buf(),
            sessions: root.path().join(".skyhook/sessions"),
            catalog: EmbeddedShimCatalog::default(),
            interaction: Some(Arc::new(interaction)),
        };
        let (tx, _) = mpsc::unbounded_channel();
        let general = "general".parse().unwrap();
        let mut app = App::new(None, launch, &general, Default::default(), tx);
        // Unit fixtures must not change the user's global model preference.
        app.remembered_model = Some("test/first".parse().unwrap());
        app.remembered_mode = Some("general".parse().unwrap());
        (root, app)
    }
    pub(in super::super) async fn fixture() -> (tempfile::TempDir, App) {
        let (root, mut app) = draft_fixture().await;
        let session = app.launch.create(None).await.unwrap();
        attach(&mut app, session).await;
        (root, app)
    }
    /// A session that approves every tool call, attached.
    pub(in crate::tui) async fn approving_fixture() -> (tempfile::TempDir, App, SessionHandle) {
        let (root, mut app) = draft_fixture().await;
        let mut config = app.launch.model.config().config().clone();
        config.approve_all = true;
        app.launch.model = config.into_runtime().unwrap().default_model();
        let session = app.launch.create(None).await.unwrap();
        attach(&mut app, session.clone()).await;
        (root, app, session)
    }
    /// Load `job`'s output as the UI does, shown or not, returning what it shows.
    pub(in crate::tui) async fn fetch_output(app: &mut App, job: JobId) -> skyhook::job::JobView {
        let mut rx = capture_work(app);
        draw(app);
        // A no-op once drawing began this job's load.
        app.fetch_output(job);
        loop {
            let work = recv(&mut rx).await;
            let loaded = matches!(&work, Work::Output { attempt, .. } if attempt.job() == job);
            app.work(work);
            if loaded {
                break app.outputs.get(&job).unwrap().clone().unwrap();
            }
        }
    }
    /// `session` observed, as opening it reads it.
    pub(in crate::tui) async fn prepared(session: SessionHandle) -> PreparedObservation {
        crate::tests::bounded(PreparedObservation::load(session))
            .await
            .unwrap()
    }
    /// Every record `session` journaled, in order, read from its store.
    pub(in crate::tui) async fn journal(
        session: &SessionHandle,
    ) -> Vec<std::sync::Arc<skyhook::session::EventRecord>> {
        crate::tests::bounded(async {
            let agents = session.store().agents().await.unwrap();
            let mut records = Vec::new();
            for agent in agents.agents {
                let mut page = Some(skyhook::session::Page::First);
                while let Some(read) = page {
                    let store = session.store();
                    let read = store.transcript(&agent.agent, read, model::PAGE).await;
                    let read = read.unwrap();
                    page = (read.records.len() == model::PAGE).then(|| {
                        skyhook::session::Page::After(read.first + model::PAGE as u64 - 1)
                    });
                    records.extend(read.records);
                }
            }
            records.sort_unstable_by_key(|record| record.sequence);
            records
        })
        .await
    }
    /// What the app shows of one agent's conversation, read whole from a store.
    pub(in crate::tui) struct Whole {
        pub(in crate::tui) live: skyhook::agent::ObservationSnapshot,
        pub(in crate::tui) window: model::Window,
        pub(in crate::tui) projection: Projection,
    }

    impl Whole {
        /// The entries a fresh build shows, every detail `open` or none.
        pub(in crate::tui) fn entries(&self, outputs: &OutputStore, open: bool) -> Vec<Entry> {
            let view = View::default();
            let presentation = model::EntryView {
                agent: &self.window.agent,
                tab: self.window.tab,
                view: &view,
                all_details: open,
                today: Default::default(),
            };
            model::entries(self.inputs(presentation, outputs), true)
        }

        pub(in crate::tui) fn inputs<'a>(
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
    pub(in crate::tui) async fn read(session: &SessionHandle, agent: &AgentId) -> Whole {
        crate::tests::bounded(async {
            let prepared = PreparedObservation::load(session.clone()).await.unwrap();
            Whole {
                live: *prepared.snapshot,
                window: pages::whole(session, agent).await.unwrap(),
                projection: *prepared.projection,
            }
        })
        .await
    }

    /// The viewed conversation as a fresh build shows it once `app` has applied every
    /// record: its pages read whole from the store, beside what `app` holds live.
    pub(in crate::tui) async fn whole_entries(app: &mut App, details: bool) -> Vec<Entry> {
        let session = app.session().unwrap().clone();
        let newest = journal(&session).await.last().map(|record| record.sequence);
        let applied = |app: &App| Some(app.snapshot.applied) >= newest;
        played::observe_until(app, applied).await;
        let mut read = read(&session, &app.selected).await;
        read.live = app.snapshot.clone();
        read.entries(&app.outputs, details)
    }
    /// Observe the session again as a lagged app does, installing what it reads.
    pub(in crate::tui) async fn resubscribed(app: &mut App) {
        let mut rx = capture_work(app);
        app.resubscribe();
        bounded(async {
            loop {
                let work = rx.recv().await.expect("work channel open");
                let observed = matches!(work, Work::Observed(_));
                app.work(work);
                if observed {
                    break;
                }
            }
        })
        .await;
    }
    /// Replace a draft fixture with one opened on `session`.
    pub(in crate::tui) async fn attach(app: &mut App, session: SessionHandle) {
        let observation = prepared(session).await;
        *app = app.sibling(Some(observation), app.launch.clone(), app.tx.clone());
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
            image: skyhook::media::Image::decode(png()).await.unwrap(),
        }
    }
    /// The entries a fresh build of `app`'s view shows from what the app holds.
    pub(in crate::tui) fn fresh(app: &App) -> Vec<Entry> {
        let view = View::default();
        let presentation = model::EntryView {
            agent: &app.selected,
            tab: app.tab,
            view: app.viewed().unwrap_or(&view),
            all_details: app.details,
            today: app.clock.day(),
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
    pub(in crate::tui) async fn viewing(
        session: &SessionHandle,
        tab: Tab,
    ) -> (tempfile::TempDir, App) {
        let (root, mut app) = draft_fixture().await;
        attach(&mut app, session.clone()).await;
        app.tab = tab;
        settled(&mut app).await;
        (root, app)
    }
    /// Draw until the viewed tab has loaded what the view shows, applying the work
    /// its loads send.
    pub(in crate::tui) async fn settled(app: &mut App) {
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
    pub(in crate::tui) fn capture_work(app: &mut App) -> mpsc::UnboundedReceiver<Work> {
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
    pub(super) fn question(
        app: &mut App,
        prompt: String,
        options: Vec<QuestionOption>,
    ) -> oneshot::Receiver<Result<QuestionReply, String>> {
        questions(app, false, vec![("answer", &prompt, options)])
    }
    /// A reply answering each question id with plain text.
    pub(super) fn texts(answers: &[(&str, &str)]) -> QuestionReply {
        let answers = answers.iter();
        let text = |text: &str| QuestionAnswer::Text(text.into());
        answers
            .map(|(id, answer)| (id.to_string(), text(answer)))
            .collect()
    }
    pub(super) fn questions(
        app: &mut App,
        background: bool,
        questions: Vec<(&str, &str, Vec<QuestionOption>)>,
    ) -> oneshot::Receiver<Result<QuestionReply, String>> {
        let (reply, receiver) = oneshot::channel();
        let questions = questions.into_iter().map(|(id, prompt, options)| Question {
            id: id.into(),
            prompt: prompt.into(),
            options,
        });
        app.prompt(Prompt {
            id: 1,
            kind: PromptKind::Questions {
                agent: app.session().unwrap().root_agent().clone(),
                background,
                questions: questions.collect(),
                reply,
            },
        });
        receiver
    }
    pub(super) fn draw_buffer(app: &mut App) -> ratatui::buffer::Buffer {
        draw_sized_buffer(app, 60, 24)
    }
    /// A test terminal kept across frames, which repaints only the cells a frame changes.
    pub(in crate::tui) struct Screen(ratatui::Terminal<ratatui::backend::TestBackend>);
    impl Screen {
        pub(in crate::tui) fn new(width: u16, height: u16) -> Self {
            let backend = ratatui::backend::TestBackend::new(width, height);
            Self(ratatui::Terminal::new(backend).unwrap())
        }
        pub(in crate::tui) fn draw(&mut self, app: &mut App) -> &ratatui::buffer::Buffer {
            self.0
                .draw(|frame| crate::tui::render::draw(frame, app))
                .unwrap();
            self.0.backend().buffer()
        }
    }
    pub(in crate::tui) fn draw_sized_buffer(
        app: &mut App,
        width: u16,
        height: u16,
    ) -> ratatui::buffer::Buffer {
        Screen::new(width, height).draw(app).clone()
    }
    pub(in super::super) fn draw(app: &mut App) -> String {
        draw_sized(app, 60, 24)
    }
    pub(in crate::tui) fn draw_sized(app: &mut App, width: u16, height: u16) -> String {
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
    pub(in crate::tui) async fn recv(rx: &mut mpsc::UnboundedReceiver<Work>) -> Work {
        bounded(rx.recv()).await.expect("work channel open")
    }
    pub(super) fn pending<T>(response: &mut oneshot::Receiver<T>) -> bool {
        matches!(
            response.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        )
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
    pub(in crate::tui) async fn run_script(app: &mut App, source: &str) {
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
    pub(in crate::tui) fn job_named(app: &App, tool: &str) -> JobId {
        let mut jobs = app.snapshot.jobs.values();
        jobs.find(|job| job.tool == tool).unwrap().id
    }
    /// Select the content row that renders `job`.
    pub(super) fn select_job(app: &mut App, job: JobId) {
        let row = app.entries().iter().position(|e| e.job_id() == Some(job));
        app.select_entry(row.unwrap());
    }
    /// Show `message` unsaved in `app`'s root conversation, after its entry `after`.
    pub(in crate::tui) fn unsaved_at(app: &mut App, after: Option<u64>, message: &str) {
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
    pub(in crate::tui) fn unsaved(app: &App, agent: &AgentId) -> Vec<String> {
        let statuses = app.unsaved_status.iter();
        let statuses = statuses.filter(|status| &status.agent == agent);
        statuses.map(|status| status.message.clone()).collect()
    }
    /// Startup warnings are shown once, survive rebuilds and are never journaled.
    async fn assert_startup_warnings_ui_only(app: &mut App) {
        let session = app.session().unwrap().clone();
        let warnings: Vec<_> = (session.warnings().iter())
            .chain(session.startup_warnings())
            .cloned()
            .collect();
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
        app.status.flush().await;
        let records = journal(&session).await;
        let records = serde_json::to_string(&records).unwrap();
        for warning in &warnings {
            let encoded = serde_json::to_string(warning).unwrap();
            assert!(!records.contains(&encoded[1..encoded.len() - 1]));
        }
    }
    #[tokio::test]
    async fn startup_warnings_are_ui_only_on_initial_started_and_resumed_sessions() {
        let (_root, mut draft) = draft_fixture().await;
        let missing_command = draft.launch.workspace.join("missing-mcp-server");
        let mut config = draft.launch.model.config().config().clone();
        config.mcp = skyhook::config::Config::from_yaml(&format!(
            "mcp:\n  unavailable:\n    transport: stdio\n    start_command: [{}]\n    startup_timeout_secs: 1",
            serde_json::to_string(&missing_command.to_string_lossy()).unwrap(),
        ))
        .unwrap()
        .mcp;
        let runtime = config.into_runtime().unwrap();
        draft.launch.model = runtime
            .select_model(&"test/first".parse().unwrap())
            .unwrap();
        let session = draft.launch.create(None).await.unwrap();
        let id = session.id();
        let (tx, _) = mpsc::unbounded_channel();
        let observation = prepared(session.clone()).await;
        let mut initial = App::new(
            Some(observation),
            draft.launch.clone(),
            &"general".parse().unwrap(),
            Default::default(),
            tx,
        );
        assert_startup_warnings_ui_only(&mut initial).await;
        draft.session_started(prepared(session.clone()).await);
        assert_startup_warnings_ui_only(&mut draft).await;
        session.shutdown().await.unwrap();
        session.close().await.unwrap();
        let mut resumed = draft.sibling(None, draft.launch.clone(), draft.tx.clone());
        drop((initial, draft, session));

        let session = resumed.launch.create(Some(id)).await.unwrap();
        attach(&mut resumed, session).await;
        assert_startup_warnings_ui_only(&mut resumed).await;
        resumed.session().unwrap().shutdown().await.unwrap();
    }
}
