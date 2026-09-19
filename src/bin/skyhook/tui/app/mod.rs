pub use events::{Hit, Work};
use input::InputTarget;
use lifecycle::{PendingStart, StartState};
pub use menus::{ConfirmAction, Item, ItemRef, Menu, MenuId, MenuKind};
use prompts::UiPrompt;
use queue::QueueDelivery;
pub use queue::{QueuedInput, QueuedInputId};
mod events;
mod input;
mod lifecycle;
mod observation;
use observation::ActiveObservation;
pub use observation::PreparedObservation;
mod menus;
mod output;
mod projection;
use output::OutputAttempt;
pub use output::OutputStore;
mod prompts;
mod queue;
mod session;

use super::{
    Launch,
    composer::{Composer, Submission},
    editor::Editor,
    keys::{COMMANDS, Command, KeyMap},
    model::{self, Entry, Projection, Tab, View},
    state,
};
use crate::interaction::{ApprovalReply, Prompt, PromptKind};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers as M, MouseButton, MouseEventKind,
};
use ratatui::layout::Rect;
use serde_json::Value;
#[cfg(test)]
use serde_json::json;
use skyhook::media::Attachment;
use skyhook::{
    agent::{AgentActivity, ObservationSnapshot, ObservedEvent, RuntimeEvent, SessionHandle},
    identity::{AgentId, JobId, SessionId},
    job::JobOutputQuery,
    provider::protocol::Message,
    session::{EventRecord, SessionEvent, SessionStore},
};
use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Composer,
    Tree,
    Content,
}
/// A root submission blocks queue dispatch until a later root user record is
/// observed, either live or through a replacement snapshot after lag. The gate
/// is `Some((root, last sequence))` while waiting.
fn observe_initial_input(gate: &mut Option<(AgentId, u64)>, record: &EventRecord) -> bool {
    let Some((root, after)) = gate else {
        return false;
    };
    let released = record.agent == *root
        && record.sequence > *after
        && matches!(
            &record.event,
            SessionEvent::MessageCommitted {
                message: Message::User(_)
            }
        );
    if released {
        *gate = None;
    }
    released
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
/// attachment reads, a session creation/switch, or an output refresh. Fresh on
/// `default()`, equal only to its clones, and never reconstructible from a
/// later request with identical content.
#[derive(Clone, Default)]
pub struct Token(std::sync::Arc<()>);
impl Token {
    fn matches(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0)
    }
}

struct HistoryBrowse {
    index: usize,
    draft: Composer,
}

pub struct App {
    observation: Option<ActiveObservation>,
    pub launch: Launch,
    /// UI-only choice, captured by each submitted user message.
    pub model: String,
    remembered_model: Option<String>,
    pub sidebar: bool,
    pub snapshot: ObservationSnapshot,
    pub projection: Projection,
    pub selected: AgentId,
    pub views: HashMap<AgentId, View>,
    pub focus: Focus,
    pub tree_cursor: usize,
    pub tree_scroll: usize,
    pub editor: Composer,
    pub history: Vec<String>,
    history_browse: Option<HistoryBrowse>,
    pub queue: VecDeque<QueuedInput>,
    next_queued_id: QueuedInputId,
    queue_sender: Option<mpsc::UnboundedSender<Vec<QueueDelivery>>>,
    queue_activity_revision: u64,
    /// `Some((root, sequence))` while a root submission blocks queue dispatch.
    initial_input: Option<(AgentId, u64)>,
    /// `Some(paused before the switch)` while a session switch is in flight.
    switching: Option<bool>,
    start: StartState,
    attached_draft: Option<AgentId>,
    deferred_switch: Option<Option<SessionId>>,
    pub paused: bool,
    pub operation: bool,
    pub prompts: VecDeque<UiPrompt>,
    pub prompt_active: bool,
    pub prompt_body_rect: Rect,
    pub prompt_options_rect: Rect,
    pub prompt_body_rows: usize,
    pub prompt_option_rows: usize,
    pub menu: Option<Menu>,
    next_menu_id: MenuId,
    // Pending attachment reads belong to one composer draft, not the next submission/session.
    // Local edits, clear, and composer-only history navigation preserve it.
    draft_ticket: Token,
    pub status: super::status::StatusLog,
    // UI-only notices, including startup diagnostics and failed status writes.
    unsaved_status: Vec<(AgentId, String)>,
    stopping: bool,
    pub animating: bool,
    pub thinking: bool,
    pub details: bool,
    pub outputs: OutputStore,
    last_output: Instant,
    pub tx: mpsc::UnboundedSender<Work>,
    pub keys: KeyMap,
    pub leader: Option<KeyEvent>,
    pub toast: Option<(String, Instant)>,
    pub dirty: bool,
    pub content_dirty: bool,
    content_revision: u64,
    pub(super) content_cache: model::ContentCache,
    pub tick_count: usize,
    pub exit: bool,
    pub clipboard: Option<String>,
    pub hits: Vec<(Rect, Hit)>,
    pub content_rect: Rect,
    pub tree_rect: Rect,
    pub composer_rect: Rect,
    pub content_rows: usize,
    pub selection: Option<(super::render::TextPosition, super::render::TextPosition)>,
    pressed_entry: Option<model::EntryKey>,
    pub search_editor: Option<Editor>,
    pub hover: Option<(u16, u16)>,
    pub render: super::render::RenderState,
}
impl App {
    pub fn entries(&self) -> &[Entry] {
        self.content_cache.entries()
    }

    #[cfg(test)]
    pub(super) fn install_entries(&mut self, entries: Vec<Entry>) {
        let view = View::default();
        let changes = self.content_cache.update(
            &ObservationSnapshot::default(),
            &Projection::default(),
            model::EntryView {
                agent: &self.selected,
                view: &view,
                thinking: false,
                all_details: false,
            },
            &OutputStore::default(),
            0,
            entries,
        );
        self.render.content_changed(changes);
        self.content_dirty = false;
    }

    fn advance_draft(&mut self) {
        self.draft_ticket = Token::default();
    }

    fn replace_draft(&mut self, submission: Submission) {
        self.advance_draft();
        self.editor.set_submission(submission);
        self.history_browse = None;
    }

    fn reset_session_draft(&mut self) {
        self.advance_draft();
        self.editor = Composer::default();
        self.history_browse = None;
    }

    pub fn new(
        observation: Option<PreparedObservation>,
        launch: Launch,
        saved: state::SavedState,
        tx: mpsc::UnboundedSender<Work>,
    ) -> Self {
        let selected = observation
            .as_ref()
            .map(|prepared| prepared.active.session.root_agent().clone())
            .unwrap_or_else(draft_root);
        let mut app = Self {
            model: launch.model.name().to_owned(),
            remembered_model: saved.model,
            sidebar: saved.sidebar,
            observation: None,
            launch,
            snapshot: ObservationSnapshot::default(),
            projection: Projection::default(),
            selected: selected.clone(),
            views: HashMap::new(),
            focus: Focus::Composer,
            tree_cursor: 0,
            tree_scroll: 0,
            editor: Composer::default(),
            history: vec![],
            history_browse: None,
            queue: VecDeque::new(),
            next_queued_id: QueuedInputId::default(),
            queue_sender: None,
            queue_activity_revision: 0,
            initial_input: None,
            switching: None,
            start: StartState::Idle,
            attached_draft: None,
            deferred_switch: None,
            paused: false,
            operation: false,
            prompts: VecDeque::new(),
            prompt_active: false,
            prompt_body_rect: Rect::default(),
            prompt_options_rect: Rect::default(),
            prompt_body_rows: 0,
            prompt_option_rows: 0,
            menu: None,
            next_menu_id: MenuId::default(),
            draft_ticket: Token::default(),
            status: super::status::StatusLog::new(tx.clone()),
            unsaved_status: Vec::new(),
            stopping: false,
            animating: false,
            thinking: false,
            details: false,
            outputs: OutputStore::default(),
            last_output: Instant::now(),
            tx: tx.clone(),
            keys: KeyMap::default(),
            leader: None,
            toast: None,
            dirty: true,
            content_dirty: true,
            content_revision: 0,
            content_cache: model::ContentCache::default(),
            tick_count: 0,
            exit: false,
            clipboard: None,
            hits: vec![],
            content_rect: Rect::default(),
            tree_rect: Rect::default(),
            composer_rect: Rect::default(),
            content_rows: 0,
            selection: None,
            pressed_entry: None,
            search_editor: None,
            hover: None,
            render: super::render::RenderState::new(selected, tx),
        };
        app.install_observation(observation);
        app.refresh();
        if let Some(root) = app.projection.agents.iter().find(|a| a.id == app.selected) {
            app.model.clone_from(&root.model);
        }
        app.show_warnings();
        app
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::interaction::UiInteraction;
    pub(super) use skyhook::agent::{Question, QuestionOption};
    use skyhook::remote::EmbeddedShimCatalog;
    use std::sync::Arc;
    pub(super) use tokio::sync::oneshot;
    pub(super) async fn draft_fixture() -> (tempfile::TempDir, App) {
        let root = tempfile::tempdir().unwrap();
        let config: skyhook::config::Config = toml::from_str("[providers.test]\nkind='openai'\napi='chat_completions'\nbase_url='http://127.0.0.1:1'\n[models.first]\nprovider='test'\nmodel='fixture'\nmax_context=128000\nmax_output=4096\n").unwrap();
        let (interaction, _) = UiInteraction::new();
        let launch = Launch {
            model: config
                .into_runtime()
                .unwrap()
                .select_model("first")
                .unwrap(),
            workspace: root.path().to_path_buf(),
            sessions: root.path().join(".skyhook/sessions"),
            catalog: EmbeddedShimCatalog::default(),
            interaction: Some(Arc::new(interaction)),
            approve_all: false,
        };
        let (tx, _) = mpsc::unbounded_channel();
        let mut app = App::new(None, launch, Default::default(), tx);
        // Unit fixtures must not change the user's global model preference.
        app.remembered_model = Some("first".into());
        (root, app)
    }
    pub(in super::super) async fn fixture() -> (tempfile::TempDir, App) {
        let (root, mut app) = draft_fixture().await;
        let session = app.launch.create(None).await.unwrap();
        app.set_session(Some(PreparedObservation::subscribe(session).await));
        (root, app)
    }
    /// A deterministic terminal failure, unlike a refused connection, which is
    /// transient and now retries until cancellation.
    pub(super) async fn permanent_failure_fixture() -> (tempfile::TempDir, App) {
        use skyhook::provider::protocol::ModelRequest;
        use skyhook::provider::{
            Provider, ProviderContext, ProviderError, ProviderErrorKind, ProviderFuture,
        };
        struct Rejected;
        impl Provider for Rejected {
            fn open_context(&self, _: String) -> Result<Box<dyn ProviderContext>, ProviderError> {
                Ok(Box::new(Self))
            }
        }
        impl ProviderContext for Rejected {
            fn invoke(&mut self, _: ModelRequest) -> ProviderFuture {
                Box::pin(async {
                    Err(ProviderError {
                        kind: ProviderErrorKind::Authentication,
                        message: "fixture credentials rejected".into(),
                        retry_after: None,
                    })
                })
            }
        }
        let (root, mut app) = draft_fixture().await;
        let harness = app
            .launch
            .model
            .harness_builder(&app.launch.workspace)
            .unwrap()
            .provider("test", Arc::new(Rejected))
            .session_root(&app.launch.sessions)
            .shim_catalog(app.launch.catalog.clone())
            .build()
            .await
            .unwrap();
        let session = harness.new_session().await.unwrap();
        app.set_session(Some(PreparedObservation::subscribe(session).await));
        (root, app)
    }
    /// An image attachment whose bytes are a minimal PNG signature.
    pub(super) fn png_attachment(name: &str) -> Attachment {
        Attachment::Image {
            file: Some(name.into()),
            image: skyhook::media::Image::new(b"\x89PNG\r\n\x1a\nfixture".to_vec()).unwrap(),
        }
    }
    pub(super) fn capture_work(app: &mut App) -> mpsc::UnboundedReceiver<Work> {
        let (tx, rx) = mpsc::unbounded_channel();
        app.tx = tx;
        rx
    }
    pub(super) async fn next_lifecycle(rx: &mut mpsc::UnboundedReceiver<Work>) -> Work {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let work = rx.recv().await.expect("work channel open");
                if matches!(
                    work,
                    Work::Started { .. } | Work::SessionReady { .. } | Work::Stopped
                ) {
                    return work;
                }
            }
        })
        .await
        .expect("lifecycle task completed")
    }
    pub(super) fn key(app: &mut App, code: KeyCode, modifiers: M) {
        app.event(Event::Key(KeyEvent::new(code, modifiers)));
    }
    pub(super) fn question(
        app: &mut App,
        prompt: String,
        options: Vec<QuestionOption>,
    ) -> oneshot::Receiver<Result<Value, String>> {
        questions(app, false, vec![("answer", &prompt, options)])
    }
    pub(super) fn questions(
        app: &mut App,
        background: bool,
        questions: Vec<(&str, &str, Vec<QuestionOption>)>,
    ) -> oneshot::Receiver<Result<Value, String>> {
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
    pub(super) fn draw_sized_buffer(
        app: &mut App,
        width: u16,
        height: u16,
    ) -> ratatui::buffer::Buffer {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| crate::tui::render::draw(frame, app))
            .unwrap();
        terminal.backend().buffer().clone()
    }
    pub(super) fn draw(app: &mut App) -> String {
        draw_buffer(app)
            .content
            .chunks(60)
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
    pub(super) async fn recv(rx: &mut mpsc::UnboundedReceiver<Work>) -> Work {
        let work = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await;
        work.expect("work arrives").expect("work channel open")
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
    /// Run a script to completion and install the resulting journal snapshot.
    pub(super) async fn run_script(app: &mut App, source: &str) {
        let session = app.session().unwrap().clone();
        session.run_script(source).await.unwrap();
        app.snapshot = session.observe().await.snapshot;
        app.refresh();
    }
    pub(super) fn job_named(app: &App, tool: &str) -> JobId {
        let mut jobs = app.projection.jobs.values();
        jobs.find(|job| job.tool == tool).unwrap().id
    }
    /// Select the content row that renders `job`.
    pub(super) fn select_job(app: &mut App, job: JobId) {
        let row = app.entries().iter().position(|e| e.job_id() == Some(job));
        app.view().row = row.unwrap();
    }
    /// Reopen a session once the agent loops of its previous handle release it.
    pub(super) async fn reopen(app: &App, id: SessionId) -> SessionHandle {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match app.launch.create(Some(id)).await {
                    Ok(session) => break session,
                    Err(error) if error.contains("already open") => {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                    Err(error) => panic!("could not reopen session: {error}"),
                }
            }
        })
        .await
        .expect("agent loops release the session after shutdown")
    }
    /// Startup warnings are shown once, survive rebuilds and are never journaled.
    async fn assert_startup_warnings_ui_only(app: &mut App) {
        let session = app.session().unwrap().clone();
        let warnings = session.startup_warnings().to_vec();
        assert!(!warnings.is_empty());
        let root = session.root_agent().clone();
        for warning in &warnings {
            let status = (root.clone(), format!("Startup warning: {warning}"));
            assert!(app.unsaved_status.contains(&status));
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
        let records = session.observe().await.snapshot.records;
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
        let server = toml::from_str(&format!(
            "transport = 'stdio'\nstart_command = [{}]\nstartup_timeout_secs = 1",
            serde_json::to_string(&missing_command.to_string_lossy()).unwrap(),
        ))
        .unwrap();
        let mut config = draft.launch.model.config().config().clone();
        config.mcp.insert("unavailable".into(), server);
        let runtime = config.into_runtime().unwrap();
        draft.launch.model = runtime.select_model("first").unwrap();
        let session = draft.launch.create(None).await.unwrap();
        let id = session.id();
        let (tx, _) = mpsc::unbounded_channel();
        let observation = PreparedObservation::subscribe(session.clone()).await;
        let mut initial = App::new(
            Some(observation),
            draft.launch.clone(),
            Default::default(),
            tx,
        );
        assert_startup_warnings_ui_only(&mut initial).await;
        draft.session_started(PreparedObservation::subscribe(session.clone()).await);
        assert_startup_warnings_ui_only(&mut draft).await;
        session.shutdown().await.unwrap();
        drop((initial, session));

        draft.set_session(None);
        draft.rebuild_content();
        assert!(draft.unsaved_status.is_empty());
        let mut entries = draft.entries().iter();
        assert!(!entries.any(|entry| entry.text().starts_with("Status · Startup warning:")));
        let resumed = reopen(&draft, id).await;
        draft.set_session(Some(PreparedObservation::subscribe(resumed).await));
        assert_startup_warnings_ui_only(&mut draft).await;
        draft.session().unwrap().shutdown().await.unwrap();
    }
}
