pub use events::{Hit, Work};
use input::InputTarget;
use lifecycle::PendingStart;
#[allow(unused_imports)] // Preserve the app-facing attachment type alongside menu items.
pub use menus::Attachment;
pub use menus::{ConfirmAction, Item, Menu, MenuKind};
use prompts::{QuestionDraft, SuspendedPrompt};
use queue::QueueDelivery;
pub use queue::QueuedInput;
mod events;
mod input;
mod lifecycle;
mod menus;
mod projection;
mod prompts;
mod queue;
mod session;

use super::{
    Launch,
    composer::Composer,
    editor::Editor,
    keys::{COMMANDS, KeyMap},
    model::{self, Entry, Projection, Tab, View},
    state,
};
use crate::interaction::{ApprovalReply, Prompt, PromptKind, PromptResponse};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers as M, MouseButton, MouseEventKind,
};
use ratatui::layout::Rect;
use serde_json::{Value, json};
use skyhook::{
    agent::{
        AgentActivity, ObservationSnapshot, ObservedEvent, QueuedPromptToken, RuntimeEvent,
        SessionHandle,
    },
    identity::{AgentId, JobId, SessionId},
    job::JobOutputQuery,
    session::{SessionEvent, SessionStore},
};
use std::{
    collections::{HashMap, HashSet, VecDeque},
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
fn draft_root() -> AgentId {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let id = SessionId::generate().unwrap_or_else(|_| {
        SessionId::from_bytes(
            u128::from(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)).to_be_bytes(),
        )
    });
    AgentId::root(id)
}

pub struct App {
    pub session: Option<SessionHandle>,
    pub launch: Launch,
    /// UI-only choice, captured by each submitted user message.
    pub model: String,
    remembered_model: Option<String>,
    pub snapshot: ObservationSnapshot,
    pub projection: Projection,
    pub selected: AgentId,
    pub views: HashMap<AgentId, View>,
    pub focus: Focus,
    pub tree_cursor: usize,
    pub tree_scroll: usize,
    pub editor: Composer,
    pub images: Vec<PathBuf>,
    pub history: Vec<String>,
    history_index: Option<usize>,
    history_draft: Composer,
    pub queue: VecDeque<QueuedInput>,
    next_queued_id: u64,
    queue_sender: Option<mpsc::UnboundedSender<Vec<QueueDelivery>>>,
    queue_activity_revision: u64,
    awaiting_initial_input: bool,
    initial_input_after: u64,
    switch_restore: Option<bool>,
    creating: bool,
    pending_start: Option<PendingStart>,
    attached_draft: Option<AgentId>,
    deferred_switch: Option<Option<SessionId>>,
    pub paused: bool,
    pub operation: bool,
    pub prompts: VecDeque<Prompt>,
    pub prompt_active: bool,
    suspended_prompt: Option<SuspendedPrompt>,
    pub prompt_editor: Editor,
    pub prompt_choice: usize,
    pub prompt_body_scroll: usize,
    pub prompt_option_scroll: usize,
    pub prompt_reveal: bool,
    pub prompt_body_rect: Rect,
    pub prompt_options_rect: Rect,
    pub prompt_body_rows: usize,
    pub prompt_option_rows: usize,
    pub question_index: usize,
    question_drafts: HashMap<usize, QuestionDraft>,
    pub question_editing: bool,
    pub answers: serde_json::Map<String, Value>,
    pub menu: Option<Menu>,
    next_menu_id: u64,
    // Pending attachment reads belong to one composer draft, not the next submission/session.
    draft_revision: u64,
    pub status: super::status::StatusLog,
    // UI-only notices, including startup diagnostics and failed status writes.
    unsaved_status: Vec<(AgentId, String)>,
    stopping: bool,
    pub light: bool,
    theme_preview: Option<bool>,
    pub animating: bool,
    pub thinking: bool,
    pub details: bool,
    pub entries: Vec<Entry>,
    pub outputs: HashMap<JobId, Value>,
    pending_outputs: HashSet<JobId>,
    final_outputs: HashSet<JobId>,
    output_versions: HashMap<JobId, u64>,
    pub output_queries: HashMap<JobId, JobOutputQuery>,
    last_output: Instant,
    pub tx: mpsc::UnboundedSender<Work>,
    pub keys: KeyMap,
    pub leader: Option<(KeyEvent, Instant)>,
    pub dirty: bool,
    pub content_dirty: bool,
    content_revision: u64,
    content_cache: model::ContentCache,
    pub tick_count: usize,
    pub exit: bool,
    pub external_editor: bool,
    pub clipboard: Option<String>,
    pub hits: Vec<(Rect, Hit)>,
    pub content_rect: Rect,
    pub tree_rect: Rect,
    pub composer_rect: Rect,
    pub content_rows: usize,
    pub selection: Option<(super::render::TextPosition, super::render::TextPosition)>,
    pressed_entry: Option<String>,
    pub search_editor: Option<Editor>,
    pub hover: Option<(u16, u16)>,
    pub render: super::render::RenderState,
}
impl App {
    pub fn new(
        session: Option<SessionHandle>,
        launch: Launch,
        snapshot: ObservationSnapshot,
        remembered_model: Option<String>,
        tx: mpsc::UnboundedSender<Work>,
        keys: KeyMap,
        light: bool,
    ) -> Self {
        let selected = session
            .as_ref()
            .map(|s| s.root_agent().clone())
            .unwrap_or_else(draft_root);
        let mut app = Self {
            model: launch.model.clone(),
            remembered_model,
            session,
            launch,
            snapshot,
            projection: Projection::default(),
            selected: selected.clone(),
            views: HashMap::new(),
            focus: Focus::Composer,
            tree_cursor: 0,
            tree_scroll: 0,
            editor: Composer::default(),
            images: vec![],
            history: vec![],
            history_index: None,
            history_draft: Composer::default(),
            queue: VecDeque::new(),
            next_queued_id: 0,
            queue_sender: None,
            queue_activity_revision: 0,
            awaiting_initial_input: false,
            initial_input_after: 0,
            switch_restore: None,
            creating: false,
            pending_start: None,
            attached_draft: None,
            deferred_switch: None,
            paused: false,
            operation: false,
            prompts: VecDeque::new(),
            prompt_active: false,
            suspended_prompt: None,
            prompt_editor: Editor::default(),
            prompt_choice: 0,
            prompt_body_scroll: 0,
            prompt_option_scroll: 0,
            prompt_reveal: true,
            prompt_body_rect: Rect::default(),
            prompt_options_rect: Rect::default(),
            prompt_body_rows: 0,
            prompt_option_rows: 0,
            question_index: 0,
            question_drafts: HashMap::new(),
            question_editing: false,
            answers: serde_json::Map::new(),
            menu: None,
            next_menu_id: 0,
            draft_revision: 0,
            status: super::status::StatusLog::new(tx.clone()),
            unsaved_status: Vec::new(),
            stopping: false,
            light,
            theme_preview: None,
            animating: false,
            thinking: false,
            details: false,
            entries: vec![],
            outputs: HashMap::new(),
            pending_outputs: HashSet::new(),
            final_outputs: HashSet::new(),
            output_versions: HashMap::new(),
            output_queries: HashMap::new(),
            last_output: Instant::now(),
            tx: tx.clone(),
            keys,
            leader: None,
            dirty: true,
            content_dirty: true,
            content_revision: 0,
            content_cache: model::ContentCache::default(),
            tick_count: 0,
            exit: false,
            external_editor: false,
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
            render: super::render::RenderState::new(selected, light, tx),
        };
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
        let config = toml::from_str("[providers.test]\nkind='openai'\napi='chat_completions'\nbase_url='http://127.0.0.1:1'\n[models.first]\nprovider='test'\nmodel='fixture'\nmax_context=128000\nmax_output=4096\n").unwrap();
        let (interaction, _) = UiInteraction::new();
        let launch = Launch {
            config: Arc::new(config),
            model: "first".into(),
            workspace: root.path().to_path_buf(),
            sessions: root.path().join(".skyhook/sessions"),
            catalog: EmbeddedShimCatalog::from_assets(&[]).unwrap(),
            interaction: Some(Arc::new(interaction)),
            approve_all: false,
        };
        let (tx, _) = mpsc::unbounded_channel();
        let mut app = App::new(
            None,
            launch,
            ObservationSnapshot::default(),
            None,
            tx,
            KeyMap::new(&Default::default()).unwrap(),
            false,
        );
        // Unit fixtures must not change the user's global model preference.
        app.remembered_model = Some("first".into());
        (root, app)
    }
    pub(in super::super) async fn fixture() -> (tempfile::TempDir, App) {
        let (root, mut app) = draft_fixture().await;
        let session = app.launch.create(None).await.unwrap();
        let snapshot = session.observe().await.snapshot;
        app.set_session(Some(session), snapshot);
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
            .config
            .harness_builder(&app.launch.workspace, &app.launch.model)
            .unwrap()
            .provider("test", Arc::new(Rejected))
            .session_root(&app.launch.sessions)
            .shim_catalog(app.launch.catalog.clone())
            .build()
            .await
            .unwrap();
        let session = harness.new_session().await.unwrap();
        let snapshot = session.observe().await.snapshot;
        app.set_session(Some(session), snapshot);
        (root, app)
    }
    pub(super) async fn next_lifecycle(rx: &mut mpsc::UnboundedReceiver<Work>) -> Work {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let work = rx.recv().await.expect("work channel open");
                if matches!(
                    work,
                    Work::Started(_) | Work::SessionReady(_) | Work::Stopped
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
    ) -> oneshot::Receiver<Result<PromptResponse, String>> {
        let (reply, receiver) = oneshot::channel();
        app.prompt(Prompt {
            id: 1,
            kind: PromptKind::Questions {
                agent: app.session.as_ref().unwrap().root_agent().clone(),
                questions: vec![Question {
                    id: "answer".into(),
                    prompt,
                    options,
                }],
            },
            reply,
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
    fn assert_startup_warnings_visible(app: &mut App) {
        let session = app.session.as_ref().unwrap();
        let warnings = session.startup_warnings().to_vec();
        assert!(!warnings.is_empty());
        let root = session.root_agent().clone();
        for warning in &warnings {
            assert!(
                app.unsaved_status
                    .contains(&(root.clone(), format!("Startup warning: {warning}"),))
            );
        }
        // Rebuilding must retain the notices without duplicating them.
        for _ in 0..2 {
            app.refresh();
            app.rebuild_content();
            for warning in &warnings {
                assert_eq!(
                    app.entries
                        .iter()
                        .filter(|entry| {
                            entry.text == format!("Status · Startup warning: {warning}")
                        })
                        .count(),
                    1,
                );
            }
        }
    }
    async fn assert_startup_warnings_not_recorded(app: &App) {
        app.status.flush().await;
        let session = app.session.as_ref().unwrap();
        let snapshot = session.observe().await.snapshot;
        for warning in session.startup_warnings() {
            let encoded = serde_json::to_string(warning).unwrap();
            let fragment = &encoded[1..encoded.len() - 1];
            assert!(
                !snapshot
                    .records
                    .values()
                    .any(|record| { serde_json::to_string(record).unwrap().contains(fragment) })
            );
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
        Arc::make_mut(&mut draft.launch.config)
            .mcp
            .insert("unavailable".into(), server);
        let session = draft.launch.create(None).await.unwrap();
        let id = session.id();
        let snapshot = session.observe().await.snapshot;
        let (tx, _) = mpsc::unbounded_channel();
        let mut initial = App::new(
            Some(session.clone()),
            draft.launch.clone(),
            snapshot.clone(),
            None,
            tx,
            KeyMap::new(&Default::default()).unwrap(),
            false,
        );
        assert_startup_warnings_visible(&mut initial);
        assert_startup_warnings_not_recorded(&initial).await;

        draft.session_started(session.clone(), snapshot);
        assert_startup_warnings_visible(&mut draft);
        assert_startup_warnings_not_recorded(&draft).await;
        session.shutdown().await.unwrap();
        drop(initial);
        drop(session);

        draft.set_session(None, ObservationSnapshot::default());
        draft.rebuild_content();
        assert!(draft.unsaved_status.is_empty());
        assert!(
            !draft
                .entries
                .iter()
                .any(|entry| { entry.text.starts_with("Status · Startup warning:") })
        );

        // Shutdown queues termination of the agent loops. Their runtime/store
        // references may outlive the handle until those commands are consumed.
        let resumed = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match draft.launch.create(Some(id)).await {
                    Ok(session) => break session,
                    Err(error) if error.contains("already open") => {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                    Err(error) => panic!("could not resume session: {error}"),
                }
            }
        })
        .await
        .expect("agent loops release the session after shutdown");
        let snapshot = resumed.observe().await.snapshot;
        draft.set_session(Some(resumed), snapshot);
        assert_startup_warnings_visible(&mut draft);
        assert_startup_warnings_not_recorded(&draft).await;
        draft.session.as_ref().unwrap().shutdown().await.unwrap();
    }
}
