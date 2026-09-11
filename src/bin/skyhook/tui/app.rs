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
#[derive(Default)]
struct QuestionDraft {
    editor: Editor,
    choice: usize,
}
// Only non-authentication drafts are suspended; secrets never enter this state.
struct SuspendedPrompt {
    id: u64,
    editor: Editor,
    choice: usize,
    body_scroll: usize,
    option_scroll: usize,
    question_index: usize,
    question_drafts: HashMap<usize, QuestionDraft>,
    question_editing: bool,
    answers: serde_json::Map<String, Value>,
    active: bool,
    focus: Focus,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attachment {
    Paste(usize),
    Image(usize),
}
#[derive(Clone)]
pub enum ConfirmAction {
    Exit,
    NewSession,
    SwitchSession(SessionId),
    CancelJob(JobId),
}
#[derive(Clone)]
pub struct Item {
    pub value: String,
    pub label: String,
    /// Secondary metadata; Commands use configured shortcuts, kept searchable
    /// separately from labels so rendering can align and mute the hint.
    pub detail: String,
    pub attachment: Option<Attachment>,
}
impl Item {
    fn new(value: impl Into<String>, label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
            detail: detail.into(),
            attachment: None,
        }
    }
    fn attachment(
        attachment: Attachment,
        label: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            attachment: Some(attachment),
            ..Self::new("", label, detail)
        }
    }
}
#[derive(Clone)]
pub enum MenuKind {
    Commands,
    Models,
    Agents,
    Sessions,
    Themes,
    Files,
    Attach,
    Attachments,
    Queue,
    Confirm(ConfirmAction),
    Output(JobId),
    OutputSearch(JobId),
    Info,
}
pub struct Menu {
    id: u64,
    pub title: String,
    pub kind: MenuKind,
    pub items: Vec<Item>,
    pub input: Editor,
    pub selected: usize,
}
impl Menu {
    pub fn filtered(&self) -> Vec<&Item> {
        let query = self.input.text.to_lowercase();
        self.items
            .iter()
            .filter(|i| {
                format!("{} {}", i.label, i.detail)
                    .to_lowercase()
                    .contains(&query)
            })
            .collect()
    }
}
pub enum Work {
    QueueCommitted {
        session: SessionId,
        id: u64,
        generation: u64,
        revision: u64,
        result: Result<(), String>,
    },
    Done {
        session: SessionId,
        result: Result<(), String>,
    },
    Output {
        session: SessionId,
        job: JobId,
        version: u64,
        finished: bool,
        result: Result<Value, String>,
    },
    MenuLoaded {
        id: u64,
        result: Result<Vec<Item>, String>,
    },
    File {
        draft: u64,
        result: Result<(PathBuf, String), String>,
    },
    SessionReady(Result<Option<SessionHandle>, String>),
    Started(Result<SessionHandle, String>),
    StatusFailed {
        session: Option<SessionId>,
        agent: AgentId,
        message: String,
    },
    Stopped,
    HighlightsReady,
}
#[derive(Clone)]
pub enum Hit {
    Agent(AgentId),
    Entry(usize, bool),
    Menu(usize),
    Tab(Tab),
    Composer,
    Attachments,
    Attention,
    PromptChoice(usize),
    Latest,
}
#[derive(Clone, Copy)]
enum InputTarget {
    Menu,
    Search,
    Prompt,
    Composer,
    None,
}

pub struct QueuedInput {
    id: u64,
    generation: u64,
    delivery: Option<QueuedPromptToken>,
    text: String,
    images: Vec<PathBuf>,
    model: String,
}

struct QueueDelivery {
    id: u64,
    generation: u64,
    text: String,
    images: Vec<PathBuf>,
    model: String,
    token: QueuedPromptToken,
}

/// Register each UI queue snapshot atomically, including attachment preparation.
async fn queue_dispatcher(
    session: SessionHandle,
    mut rx: mpsc::UnboundedReceiver<Vec<QueueDelivery>>,
    tx: mpsc::UnboundedSender<Work>,
) {
    use futures_util::{StreamExt, stream::FuturesOrdered};
    let mut pending = FuturesOrdered::new();
    let mut open = true;
    while open || !pending.is_empty() {
        tokio::select! {
            biased;
            delivery = rx.recv(), if open => match delivery {
                Some(mut deliveries) => {
                    // Coalesce snapshots already waiting on the UI channel too.
                    while let Ok(more) = rx.try_recv() {
                        deliveries.extend(more);
                    }
                    let session = session.clone();
                    pending.push_back(async move {
                        let ids: Vec<_> = deliveries.iter()
                            .map(|input| (input.id, input.generation)).collect();
                        let inputs = deliveries.into_iter().map(|input| {
                            skyhook::agent::QueuedPrompt {
                                text: input.text,
                                paths: input.images,
                                options: skyhook::agent::PromptOptions { model: Some(input.model) },
                                token: input.token,
                            }
                        }).collect();
                        let results = session.enqueue_prompts_with_options(inputs).await;
                        let revision = session.observe().await.snapshot.revision;
                        ids.into_iter().zip(results).map(|((id, generation), result)| {
                            Work::QueueCommitted {
                                session: session.id(), id, generation, revision,
                                result: result.map_err(|error| error.to_string()),
                            }
                        }).collect::<Vec<_>>()
                    });
                }
                None => open = false,
            },
            Some(work) = pending.next(), if !pending.is_empty() => {
                for item in work {
                    let _ = tx.send(item);
                }
            }
        }
    }
}

// A draft identity only scopes UI state/notices; it never names a session directory.
fn draft_root() -> AgentId {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let id = SessionId::generate().unwrap_or_else(|_| {
        SessionId::from_bytes(
            u128::from(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)).to_be_bytes(),
        )
    });
    AgentId::root(id)
}

enum PendingStart {
    Input(QueuedInput),
    QueuedInput(QueuedInput),
    Script(PathBuf),
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
    pub fn session_id(&self) -> Option<SessionId> {
        self.session.as_ref().map(SessionHandle::id)
    }
    pub fn root_agent(&self) -> &AgentId {
        self.session
            .as_ref()
            .map_or(&self.selected, SessionHandle::root_agent)
    }
    fn show_warnings(&mut self) {
        if let Some(session) = &self.session {
            for warning in session.warnings() {
                self.notice(format!("Startup warning: {warning}"));
            }
            // Connection diagnostics belong only to this host installation, not
            // the journal or the agent's context. Reuse the UI-only status tail
            // rather than notice(), which persists statuses for active sessions.
            self.unsaved_status
                .extend(session.startup_warnings().iter().map(|warning| {
                    (
                        session.root_agent().clone(),
                        format!("Startup warning: {warning}"),
                    )
                }));
            if !session.startup_warnings().is_empty() {
                self.dirty = true;
                self.invalidate_content();
            }
        }
    }
    fn notifier(&self) -> super::status::StatusSender {
        self.status.sender(self.session.as_ref(), &self.selected)
    }
    fn root_notifier(&self) -> super::status::StatusSender {
        self.status.sender(self.session.as_ref(), self.root_agent())
    }
    pub fn notice(&self, message: impl Into<String>) {
        self.notifier().send(message);
    }
    pub fn view(&mut self) -> &mut View {
        self.views.entry(self.selected.clone()).or_default()
    }
    /// Reduce every event, but only invalidate the visible conversation when it changes.
    /// The caller batches projection updates for journal records.
    pub fn observe(&mut self, event: ObservedEvent) -> bool {
        if let RuntimeEvent::Record(record) = &event.event
            && &record.agent == self.root_agent()
            && record.sequence > self.initial_input_after
            && matches!(
                &record.event,
                SessionEvent::MessageCommitted {
                    message: skyhook::provider::protocol::Message::User(_)
                }
            )
        {
            self.awaiting_initial_input = false;
        }
        let (records, repaint, content) = match &event.event {
            RuntimeEvent::Record(_) => (true, true, true),
            RuntimeEvent::ResponseEvent { agent, .. }
            | RuntimeEvent::ResponseSettled { agent, .. } => {
                (false, agent == &self.selected, agent == &self.selected)
            }
            RuntimeEvent::Activity { agent, .. } => (false, true, agent == &self.selected),
            RuntimeEvent::Context { agent, .. } => (false, agent == &self.selected, false),
            RuntimeEvent::TurnCompleted { .. } => (false, true, false),
        };
        // Lifecycle updates can replace the live response, but do not change
        // recorded history. Invalidate that response without rebuilding history.
        let append_only = if let RuntimeEvent::ResponseEvent { agent, request, .. } = &event.event {
            self.content_cache.observe_response(agent, *request);
            true
        } else {
            false
        };
        self.snapshot.apply(event);
        self.deliver_queue();
        self.dirty |= repaint;
        if content {
            if append_only && self.unsaved_status.is_empty() {
                self.content_dirty = true;
            } else {
                self.invalidate_content();
            }
        }
        records
    }
    fn invalidate_content(&mut self) {
        self.content_dirty = true;
        self.content_revision = self.content_revision.wrapping_add(1);
    }
    pub fn refresh(&mut self) {
        self.projection.rebuild(&self.snapshot);
        self.dirty = true;
        self.invalidate_content();
    }
    pub fn reset_projection(&mut self) {
        self.projection = Projection::default();
        self.refresh();
    }
    pub fn rebuild_content(&mut self) {
        if self.content_dirty {
            let view = self.views.entry(self.selected.clone()).or_default();
            let changes = self.content_cache.update(
                &mut self.entries,
                &self.snapshot,
                &self.projection,
                model::EntryView {
                    agent: &self.selected,
                    view,
                    thinking: self.thinking,
                    all_details: self.details,
                },
                &self.outputs,
                self.content_revision,
            );
            self.render.content_changed(changes);
            // Unsaved statuses are an exceptional external tail. A conservative
            // model rebuild can drop it even if entry ordering stays compatible.
            for (index, (agent, message)) in self.unsaved_status.iter().enumerate() {
                if agent != &self.selected {
                    continue;
                }
                let key = format!("unsaved-status-{index}");
                if self
                    .entries
                    .iter()
                    .rev()
                    .take(self.unsaved_status.len())
                    .any(|entry| entry.key == key)
                {
                    continue;
                }
                self.render.changes.dirty.push(self.entries.len());
                self.entries.push(Entry {
                    key,
                    text: format!("Status · {message}"),
                    surface: model::Surface::Status,
                    expandable: false,
                    default_open: false,
                    running: false,
                    footer: None,
                    request: None,
                    indent: 0,
                    job: None,
                    compact_after: false,
                    header: None,
                    document: None,
                });
            }
            self.content_dirty = false;
        }
    }
    pub fn busy(&self) -> bool {
        self.switch_restore.is_some()
            || self.creating
            || self.stopping
            || self.operation
            || self.snapshot.revision < self.queue_activity_revision
            || self.queue.iter().any(|input| input.delivery.is_some())
            || matches!(
                self.snapshot.activity.get(self.root_agent()),
                Some(
                    AgentActivity::Working
                        | AgentActivity::Reconnecting { .. }
                        | AgentActivity::Tools
                        | AgentActivity::Compacting
                        | AgentActivity::WaitingChildren
                )
            )
    }
    fn active_work(&self) -> bool {
        self.busy()
            || self
                .projection
                .jobs
                .values()
                .any(|j| !j.state.is_terminal())
    }
    pub fn set_session(&mut self, session: Option<SessionHandle>, snapshot: ObservationSnapshot) {
        self.cancel_queue_delivery();
        self.queue_sender = None;
        self.queue_activity_revision = 0;
        self.awaiting_initial_input = false;
        self.attached_draft = None;
        self.session = session;
        self.snapshot = snapshot;
        self.selected = self
            .session
            .as_ref()
            .map(|s| s.root_agent().clone())
            .unwrap_or_else(draft_root);
        self.views.clear();
        self.content_cache = model::ContentCache::default();
        self.render.reset_session();
        self.outputs.clear();
        self.pending_outputs.clear();
        self.final_outputs.clear();
        self.output_versions.clear();
        self.output_queries.clear();
        self.prompts.clear();
        self.suspended_prompt = None;
        self.reset_prompt();
        self.prompt_active = false;
        self.editor = Composer::default();
        self.draft_revision = self.draft_revision.wrapping_add(1);
        self.images.clear();
        self.queue.clear();
        self.switch_restore = None;
        self.creating = false;
        self.pending_start = None;
        self.deferred_switch = None;
        self.paused = false;
        self.operation = false;
        self.menu = None;
        self.preview_theme();
        self.unsaved_status.clear();
        self.tree_cursor = 0;
        self.tree_scroll = 0;
        self.selection = None;
        self.reset_projection();
        if let Some(root) = self
            .projection
            .agents
            .iter()
            .find(|a| a.id == self.selected)
        {
            self.model.clone_from(&root.model);
        }
        self.show_warnings();
        if self.stopping {
            self.finish_shutdown();
        }
    }
    fn begin_session(&mut self, action: PendingStart) {
        if matches!(self.pending_start, Some(PendingStart::Script(_))) {
            self.notice("Cancelled the pending script in favor of the new action");
        }
        self.pending_start = Some(action);
        self.creating = true;
        let launch = self.launch.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = launch.create(None).await;
            if let Err(error) = tx.send(Work::Started(result))
                && let Work::Started(Ok(session)) = error.0
            {
                let _ = session.shutdown().await;
            }
        });
        self.dirty = true;
    }
    /// Attach the observation before dispatching the first action. Unlike a switch,
    /// this preserves composer edits, queued model choices, and draft UI state.
    pub fn session_started(&mut self, session: SessionHandle, snapshot: ObservationSnapshot) {
        self.creating = false;
        let draft = self.selected.clone();
        self.attached_draft = Some(draft.clone());
        self.selected = session.root_agent().clone();
        self.session = Some(session);
        self.snapshot = snapshot;
        if let Some(view) = self.views.remove(&draft) {
            self.views.insert(self.selected.clone(), view);
        }
        for (agent, _) in &mut self.unsaved_status {
            if *agent == draft {
                *agent = self.selected.clone();
            }
        }
        self.content_cache = model::ContentCache::default();
        self.render.reset_session();
        self.reset_projection();
        self.show_warnings();
        if self.stopping {
            self.pending_start = None;
            self.finish_shutdown();
            return;
        }
        if let Some(id) = self.deferred_switch.take() {
            self.park_pending_input();
            self.switch(id);
            return;
        }
        if self.paused {
            self.park_pending_input();
            return;
        }
        match self.pending_start.take() {
            Some(PendingStart::Input(input)) => self.send_input(input),
            Some(PendingStart::QueuedInput(input)) => {
                self.queue.push_front(input);
                self.deliver_queue();
            }
            Some(PendingStart::Script(path)) => self.start_script(path),
            None => {}
        }
    }
    fn park_pending_input(&mut self) {
        match self.pending_start.take() {
            Some(PendingStart::Input(input) | PendingStart::QueuedInput(input)) => {
                self.queue.push_front(input);
                self.refresh_queue_menu();
            }
            other => self.pending_start = other,
        }
    }
    fn start_failed(&mut self, error: String) {
        self.creating = false;
        if let Some(action) = self.pending_start.take() {
            match action {
                PendingStart::Input(input) | PendingStart::QueuedInput(input) => {
                    self.queue.push_front(input)
                }
                PendingStart::Script(path) => {
                    // Keep an explicit script retry separate from composer input.
                    self.pending_start = Some(PendingStart::Script(path));
                }
            }
        }
        self.paused = true;
        self.refresh_queue_menu();
        self.notice(error);
        if self.stopping {
            self.finish_shutdown();
        } else if let Some(id) = self.deferred_switch.take() {
            self.switch(id);
        }
    }
    pub fn submit(&mut self, text: String, images: Vec<PathBuf>) {
        if text.trim().is_empty() && images.is_empty() {
            return;
        }
        self.draft_revision = self.draft_revision.wrapping_add(1);
        let queued = self.queued_input(text, images);
        if self.busy() || self.paused || !self.queue.is_empty() {
            self.queue.push_back(queued);
            self.refresh_queue_menu();
            self.deliver_queue();
            self.dirty = true;
            return;
        }
        self.send_input(queued);
    }
    fn send_input(&mut self, queued: QueuedInput) {
        let Some(session) = self.session.clone() else {
            self.launch.model.clone_from(&queued.model);
            self.begin_session(PendingStart::Input(queued));
            return;
        };
        let QueuedInput {
            text,
            images,
            model,
            ..
        } = queued;
        self.launch.model.clone_from(&model);
        self.operation = true;
        self.awaiting_initial_input = true;
        self.initial_input_after = self
            .snapshot
            .records
            .keys()
            .next_back()
            .copied()
            .unwrap_or(0);
        self.history.push(text.clone());
        self.history_index = None;
        let tx = self.tx.clone();
        self.set_title(&text);
        let status = self.status.clone();
        if self.remembered_model.as_deref() != Some(model.as_str()) {
            self.remembered_model = Some(model.clone());
            let remembered = model.clone();
            let notices = self.root_notifier();
            tokio::task::spawn_blocking(move || {
                if let Err(error) = state::remember(&remembered) {
                    notices.send(format!("Could not save model selection: {error}"));
                }
            });
        }
        tokio::spawn(async move {
            status.flush().await;
            let result = session
                .prompt_with_options(
                    text,
                    &images,
                    skyhook::agent::PromptOptions { model: Some(model) },
                )
                .await;
            let _ = tx.send(Work::Done {
                session: session.id(),
                result: result.map(|_| ()).map_err(|e| e.to_string()),
            });
        });
        self.dirty = true;
    }
    /// Cancel only messages that have not crossed the runtime claim boundary.
    /// A claimed row stays until its commit acknowledgement arrives.
    fn cancel_queue_delivery(&mut self) {
        for input in &mut self.queue {
            if input.delivery.as_ref().is_some_and(|token| token.cancel()) {
                input.delivery = None;
            }
        }
    }
    fn deliver_queue(&mut self) {
        // Lag recovery replaces the snapshot without replaying each event.
        if self.awaiting_initial_input
            && self.snapshot.records.values().any(|record| {
                record.sequence > self.initial_input_after
                    && &record.agent == self.root_agent()
                    && matches!(
                        &record.event,
                        SessionEvent::MessageCommitted {
                            message: skyhook::provider::protocol::Message::User(_)
                        }
                    )
            })
        {
            self.awaiting_initial_input = false;
        }
        if self.paused
            || self.creating
            || self.stopping
            || self.switch_restore.is_some()
            || self.awaiting_initial_input
            || self.queue.is_empty()
        {
            return;
        }
        let Some(session) = self.session.clone() else {
            // Retain the head across creation, then register the entire queue.
            let input = self.queue.pop_front().unwrap();
            self.refresh_queue_menu();
            self.launch.model.clone_from(&input.model);
            self.begin_session(PendingStart::QueuedInput(input));
            return;
        };
        let sender = self.queue_sender.get_or_insert_with(|| {
            let (tx, rx) = mpsc::unbounded_channel();
            tokio::spawn(queue_dispatcher(session, rx, self.tx.clone()));
            tx
        });
        let mut deliveries = Vec::new();
        for input in &mut self.queue {
            if input.delivery.is_some() {
                continue;
            }
            input.generation = input.generation.wrapping_add(1);
            let token = QueuedPromptToken::new();
            input.delivery = Some(token.clone());
            let delivery = QueueDelivery {
                id: input.id,
                generation: input.generation,
                text: input.text.clone(),
                images: input.images.clone(),
                model: input.model.clone(),
                token,
            };
            deliveries.push(delivery);
        }
        if !deliveries.is_empty() && sender.send(deliveries).is_err() {
            self.queue_sender = None;
            self.paused = true;
            self.cancel_queue_delivery();
            self.notice("Could not deliver queued messages; resume to retry");
        }
    }
    fn queue_committed(
        &mut self,
        id: u64,
        generation: u64,
        revision: u64,
        result: Result<(), String>,
    ) {
        let Some(index) = self.queue.iter().position(|input| {
            input.id == id && input.generation == generation && input.delivery.is_some()
        }) else {
            // Editing, cancellation and retry invalidate old acknowledgements.
            return;
        };
        match result {
            Ok(()) => {
                let input = self.queue.remove(index).unwrap();
                self.queue_activity_revision = self.queue_activity_revision.max(revision);
                self.launch.model.clone_from(&input.model);
                self.history.push(input.text.clone());
                self.history_index = None;
                self.set_title(&input.text);
                if self.remembered_model.as_deref() != Some(input.model.as_str()) {
                    self.remembered_model = Some(input.model.clone());
                    let notices = self.root_notifier();
                    tokio::task::spawn_blocking(move || {
                        if let Err(error) = state::remember(&input.model) {
                            notices.send(format!("Could not save model selection: {error}"));
                        }
                    });
                }
            }
            Err(error) => {
                self.queue[index].delivery = None;
                self.paused = true;
                self.cancel_queue_delivery();
                if let Some(paused) = &mut self.switch_restore {
                    *paused = true;
                }
                self.notice(format!(
                    "Queued message was not submitted: {error}. Resume to retry."
                ));
            }
        }
        self.refresh_queue_menu();
    }
    fn set_title(&self, title: &str) {
        let Some(session) = &self.session else {
            return;
        };
        let path = session.directory().join("ui.json");
        let title = super::format::brief(title, 100);
        if !path.exists() {
            let notices = self.root_notifier();
            tokio::task::spawn_blocking(move || {
                if let Err(e) = state::atomic_write(
                    &path,
                    &serde_json::to_vec(&json!({"title":title})).unwrap_or_default(),
                ) {
                    notices.send(format!("Could not save session title: {e}"));
                }
            });
        }
    }
    pub fn start_script(&mut self, path: PathBuf) {
        if self.busy() {
            self.notice("Wait for the current operation before starting a script");
            return;
        }
        let Some(session) = self.session.clone() else {
            self.launch.model.clone_from(&self.model);
            self.begin_session(PendingStart::Script(path));
            return;
        };
        self.operation = true;
        self.set_title(&path.display().to_string());
        let tx = self.tx.clone();
        let status = self.status.clone();
        tokio::spawn(async move {
            status.flush().await;
            let result = async {
                let source = tokio::fs::read_to_string(path)
                    .await
                    .map_err(|e| e.to_string())?;
                session
                    .run_script(source)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(())
            }
            .await;
            let _ = tx.send(Work::Done {
                session: session.id(),
                result,
            });
        });
    }
    pub fn shutdown(&mut self) {
        if self.stopping {
            return;
        }
        self.paused = true;
        self.cancel_queue_delivery();
        self.stopping = true;
        // Creation/switch tasks own handles which must arrive before teardown.
        if !self.creating && self.switch_restore.is_none() {
            self.finish_shutdown();
        }
        self.dirty = true;
    }
    fn finish_shutdown(&self) {
        let session = self.session.clone();
        let tx = self.tx.clone();
        let notices = self.root_notifier();
        let status = self.status.clone();
        tokio::spawn(async move {
            status.flush().await;
            if let Some(session) = session
                && let Err(e) = session.shutdown().await
            {
                notices.send(e.to_string());
            }
            status.flush().await;
            let _ = tx.send(Work::Stopped);
        });
    }
    pub fn work(&mut self, work: Work) {
        match work {
            Work::Started(Err(error)) => self.start_failed(error),
            Work::QueueCommitted {
                session,
                id,
                generation,
                revision,
                result,
            } if Some(session) == self.session_id() => {
                self.queue_committed(id, generation, revision, result);
            }
            Work::Done { session, result } if Some(session) == self.session_id() => {
                self.operation = false;
                self.awaiting_initial_input = false;
                if let Err(error) = result {
                    self.root_notifier().send(error);
                    self.paused = true;
                    self.cancel_queue_delivery();
                    if let Some(paused) = &mut self.switch_restore {
                        *paused = true;
                    }
                }
            }
            Work::Output {
                session,
                job,
                version,
                finished,
                result,
            } if Some(session) == self.session_id() => {
                self.pending_outputs.remove(&job);
                if version != *self.output_versions.get(&job).unwrap_or(&0) {
                    return;
                }
                if finished {
                    self.final_outputs.insert(job);
                }
                let value = result.unwrap_or_else(|error| json!({"error": error}));
                if self.outputs.get(&job) == Some(&value) {
                    return;
                }
                self.outputs.insert(job, value);
                self.content_cache.invalidate_job(job);
                self.content_dirty = true;
            }
            Work::MenuLoaded { id, result } => {
                let Some(menu) = self.menu.as_mut().filter(|menu| menu.id == id) else {
                    return;
                };
                match result {
                    Ok(items) => menu.items = items,
                    Err(error) => self.notice(error),
                }
            }
            Work::File { draft, result } => {
                if draft != self.draft_revision {
                    return;
                }
                match result {
                    Ok((path, content)) => {
                        self.editor
                            .insert_paste(format!("File: {}\n{content}", path.display()));
                        self.notice(format!("Attached {}", path.display()));
                    }
                    Err(error) => self.notice(error),
                }
            }
            Work::SessionReady(Err(error)) => {
                if let Some(paused) = self.switch_restore.take() {
                    self.paused = paused;
                }
                self.notice(error);
                if self.stopping {
                    self.finish_shutdown();
                }
            }
            Work::StatusFailed {
                session,
                agent,
                message,
            } if (session == self.session_id()
                && (session.is_some() || agent == self.selected))
                || (session.is_none() && self.attached_draft.as_ref() == Some(&agent)) =>
            {
                let agent = if self.attached_draft.as_ref() == Some(&agent) {
                    self.root_agent().clone()
                } else {
                    agent
                };
                self.unsaved_status.push((agent, message));
                self.invalidate_content();
            }
            Work::Stopped => self.exit = true,
            _ => {}
        }
        self.dirty = true;
    }
    pub fn tick(&mut self) {
        self.tick_count = self.tick_count.wrapping_add(1);
        if self
            .leader
            .is_some_and(|(_, t)| t.elapsed() > Duration::from_secs(2))
        {
            self.leader = None;
            self.dirty = true;
        }
        let previous = self.prompts.front().map(|p| p.id);
        self.prompts.retain(|p| !p.reply.is_closed());
        if previous != self.prompts.front().map(|p| p.id) {
            self.dirty = true;
            self.reset_prompt();
        }
        if self.prompts.is_empty() {
            self.prompt_active = false;
        }
        self.deliver_queue();
        if self.last_output.elapsed() >= Duration::from_millis(500) {
            self.last_output = Instant::now();
            let view = self.views.entry(self.selected.clone()).or_default();
            let jobs: Vec<_> = self
                .projection
                .jobs
                .values()
                .filter(|j| {
                    j.agent == self.selected
                        && view.is_expanded(&format!("j{}", j.id), self.details)
                        && (!j.state.is_terminal() || !self.final_outputs.contains(&j.id))
                })
                .map(|j| j.id)
                .collect();
            for id in jobs {
                self.fetch_output(id);
            }
        }
        self.dirty |= self.animating;
        // Retire completed child rows once, rather than redrawing forever while idle.
        let before = self.projection.completed.len();
        self.projection
            .completed
            .retain(|_, finished| finished.elapsed() < Duration::from_secs(2));
        self.dirty |= before != self.projection.completed.len();
    }
    fn set_output_query(&mut self, job: JobId, query: JobOutputQuery) {
        self.output_queries.insert(job, query);
        *self.output_versions.entry(job).or_default() += 1;
        self.final_outputs.remove(&job);
        self.fetch_output(job);
    }
    fn fetch_output(&mut self, job: JobId) {
        if self.session.is_none() || !self.pending_outputs.insert(job) {
            return;
        }
        let query = self
            .output_queries
            .entry(job)
            .or_insert_with(|| {
                let mut q = JobOutputQuery::new(job);
                q.field = match self.projection.jobs.get(&job).map(|job| job.tool.as_str()) {
                    Some("exec" | "shell") => Some("/result/stdout".into()),
                    // These schemas bound long fields in the structured projection.
                    Some("read" | "write" | "replace" | "patch") => None,
                    _ => Some(String::new()),
                };
                q
            })
            .clone();
        let version = *self.output_versions.get(&job).unwrap_or(&0);
        let finished = self
            .projection
            .jobs
            .get(&job)
            .is_some_and(|job| job.state.is_terminal());
        let Some(session) = self.session.clone() else {
            return;
        };
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = session
                .inspect_output(query)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(Work::Output {
                session: session.id(),
                job,
                version,
                finished,
                result,
            });
        });
    }
    pub fn prompt(&mut self, prompt: Prompt) {
        if matches!(prompt.kind, PromptKind::Authentication(_)) {
            // SSH may be blocking other work. Keep authentication FIFO, but put
            // it ahead of ordinary questions/permissions, even dismissed ones.
            let index = self
                .prompts
                .iter()
                .take_while(|p| matches!(p.kind, PromptKind::Authentication(_)))
                .count();
            if index == 0 {
                if let Some(previous) = self.prompts.front() {
                    self.suspended_prompt = Some(SuspendedPrompt {
                        id: previous.id,
                        editor: std::mem::take(&mut self.prompt_editor),
                        choice: self.prompt_choice,
                        body_scroll: self.prompt_body_scroll,
                        option_scroll: self.prompt_option_scroll,
                        question_index: self.question_index,
                        question_drafts: std::mem::take(&mut self.question_drafts),
                        question_editing: self.question_editing,
                        answers: std::mem::take(&mut self.answers),
                        active: self.prompt_active,
                        focus: self.focus,
                    });
                }
                self.prompts.push_front(prompt);
                self.reset_prompt();
            } else {
                self.prompts.insert(index, prompt);
            }
            self.activate_prompt();
        } else {
            // Ordinary requests do not depend on pane focus, but don't steal
            // input from an open menu/search or reopen dismissed requests.
            let show = self.prompts.is_empty();
            self.prompts.push_back(prompt);
            if show {
                self.prompt_active = true;
                self.leader = None;
            }
        }
        self.dirty = true;
    }
    fn activate_prompt(&mut self) {
        if self.prompts.is_empty() {
            return;
        }
        self.prompt_active = true;
        self.focus = Focus::Composer;
        self.leader = None;
        self.menu = None;
        self.search_editor = None;
        self.preview_theme();
    }
    fn reset_prompt(&mut self) {
        self.prompt_editor.clear_sensitive();
        self.prompt_editor = Editor::default();
        self.prompt_choice = 0;
        self.reset_prompt_view();
        self.question_index = 0;
        self.question_drafts.clear();
        self.question_editing = false;
        self.answers.clear();
        if self
            .suspended_prompt
            .as_ref()
            .is_some_and(|saved| self.prompts.front().is_some_and(|p| p.id == saved.id))
        {
            let saved = self.suspended_prompt.take().unwrap();
            self.prompt_editor = saved.editor;
            self.prompt_choice = saved.choice;
            self.prompt_body_scroll = saved.body_scroll;
            self.prompt_option_scroll = saved.option_scroll;
            self.question_index = saved.question_index;
            self.question_drafts = saved.question_drafts;
            self.question_editing = saved.question_editing;
            self.answers = saved.answers;
            self.prompt_active = saved.active;
            self.focus = saved.focus;
        } else if self
            .suspended_prompt
            .as_ref()
            .is_some_and(|saved| !self.prompts.iter().any(|p| p.id == saved.id))
        {
            self.suspended_prompt = None;
        }
    }
    fn reset_prompt_view(&mut self) {
        self.prompt_body_scroll = 0;
        self.prompt_option_scroll = 0;
        self.prompt_reveal = true;
    }
    fn scroll_prompt(&mut self, options: bool, delta: isize) {
        if options {
            let max = self
                .prompt_option_rows
                .saturating_sub(self.prompt_options_rect.height as usize);
            self.prompt_option_scroll = self
                .prompt_option_scroll
                .saturating_add_signed(delta)
                .min(max);
            self.prompt_reveal = false;
        } else {
            let max = self
                .prompt_body_rows
                .saturating_sub(self.prompt_body_rect.height as usize);
            self.prompt_body_scroll = self
                .prompt_body_scroll
                .saturating_add_signed(delta)
                .min(max);
        }
    }
    pub fn prompt_options(&self) -> Vec<String> {
        let Some(prompt) = self.prompts.front() else {
            return vec![];
        };
        match &prompt.kind {
            PromptKind::Approval(request) => {
                let mut options = vec!["Allow once".into(), "Deny".into(), "Details".into()];
                if request
                    .permissions
                    .iter()
                    .any(|p| p.proposed_grant.is_some())
                {
                    options.push("Allow proposed scope".into());
                }
                options
            }
            PromptKind::Questions { questions, .. } => {
                if let Some(question) = questions.get(self.question_index) {
                    question
                        .options
                        .iter()
                        .map(|o| format!("{} — {}", o.label, o.description))
                        .chain(std::iter::once("Write an answer…".into()))
                        .collect()
                } else {
                    vec!["Submit answers".into(), "Review again".into()]
                }
            }
            PromptKind::Authentication(_) => vec![],
        }
    }
    pub fn prompt_text(&self) -> String {
        let Some(prompt) = self.prompts.front() else {
            return String::new();
        };
        match &prompt.kind {
            PromptKind::Approval(r) => format!(
                "Permission · agent {} · {}\n{}",
                super::format::agent_label(&r.agent),
                r.tool,
                super::format::brief(&model::pretty(&r.arguments), 240)
            ),
            PromptKind::Questions { agent, questions } => {
                questions.get(self.question_index).map_or_else(
                    || format!("Review answers\n{}", model::pretty(&self.answers)),
                    |q| {
                        format!(
                            "Agent {} · question {}/{}\n{}",
                            super::format::agent_label(agent),
                            self.question_index + 1,
                            questions.len(),
                            q.prompt
                        )
                    },
                )
            }
            PromptKind::Authentication(p) => format!("Authentication\n{}", p.message),
        }
    }
    pub fn multiple_questions(&self) -> bool {
        matches!(self.prompts.front().map(|p| &p.kind),
            Some(PromptKind::Questions { questions, .. }) if questions.len() > 1)
    }
    fn select_question(&mut self, index: usize) {
        if index == self.question_index {
            return;
        }
        // Drafts are independent of confirmed answers, including untouched and
        // unanswered questions. Moving never confirms an answer.
        if matches!(self.prompts.front().map(|p| &p.kind),
            Some(PromptKind::Questions { questions, .. }) if self.question_index < questions.len())
        {
            self.question_drafts.insert(
                self.question_index,
                QuestionDraft {
                    editor: std::mem::take(&mut self.prompt_editor),
                    choice: self.prompt_choice,
                },
            );
        } else {
            self.prompt_editor = Editor::default();
        }
        self.question_index = index;
        self.prompt_choice = 0;
        self.question_editing = false;
        self.reset_prompt_view();
        self.restore_question_answer();
    }
    fn switch_question(&mut self, delta: isize) {
        let Some(PromptKind::Questions { questions, .. }) = self.prompts.front().map(|p| &p.kind)
        else {
            return;
        };
        if questions.len() > 1 {
            if self.question_index >= questions.len() && delta > 0 {
                return;
            }
            let index = self
                .question_index
                .saturating_add_signed(delta)
                .min(questions.len() - 1);
            self.select_question(index);
        }
    }
    fn invalidate_question_answer(&mut self) {
        if let Some(PromptKind::Questions { questions, .. }) = self.prompts.front().map(|p| &p.kind)
            && let Some(question) = questions.get(self.question_index)
        {
            self.answers.remove(&question.id);
        }
    }
    fn restore_question_answer(&mut self) {
        if let Some(draft) = self.question_drafts.remove(&self.question_index) {
            self.prompt_editor = draft.editor;
            self.prompt_choice = draft.choice;
            return;
        }
        let Some(PromptKind::Questions { questions, .. }) = self.prompts.front().map(|p| &p.kind)
        else {
            return;
        };
        let Some(question) = questions.get(self.question_index) else {
            return;
        };
        let Some(answer) = self.answers.get(&question.id) else {
            return;
        };
        let (label, comment) = if let Some(text) = answer.as_str() {
            (text, "")
        } else {
            (
                answer
                    .get("answer")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                answer
                    .get("comment")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            )
        };
        self.prompt_choice = question
            .options
            .iter()
            .position(|o| o.label == label)
            .unwrap_or(question.options.len());
        self.prompt_editor
            .set(if self.prompt_choice < question.options.len() {
                comment.to_owned()
            } else {
                label.to_owned()
            });
    }
    fn answer(&mut self) {
        let Some(prompt) = self.prompts.front() else {
            return;
        };
        let value = match &prompt.kind {
            PromptKind::Approval(request) => match self.prompt_choice {
                0 => Some(PromptResponse::Approval(ApprovalReply::Allow)),
                1 => Some(PromptResponse::Approval(ApprovalReply::Deny)),
                2 => {
                    let text = format!(
                        "Agent {}\nTool {}\nArguments\n{}\nPermissions\n{:#?}",
                        request.agent,
                        request.tool,
                        model::pretty(&request.arguments),
                        request.permissions
                    );
                    self.info("Permission details", text);
                    return;
                }
                _ => Some(PromptResponse::Approval(ApprovalReply::Grant)),
            },
            PromptKind::Authentication(_) => Some(PromptResponse::Authentication(
                self.prompt_editor.take_sensitive(),
            )),
            PromptKind::Questions { questions, .. } => {
                if let Some(question) = questions.get(self.question_index) {
                    let text = &self.prompt_editor.text;
                    let answer = if let Some(option) = question.options.get(self.prompt_choice) {
                        if text.trim().is_empty() {
                            Value::String(option.label.clone())
                        } else {
                            serde_json::json!({"answer": option.label, "comment": text})
                        }
                    } else {
                        if text.trim().is_empty() {
                            return;
                        }
                        Value::String(text.clone())
                    };
                    self.answers.insert(question.id.clone(), answer);
                    if questions.len() == 1 {
                        Some(PromptResponse::Questions(
                            self.answers.values().next().cloned().unwrap_or(Value::Null),
                        ))
                    } else {
                        // Keep sequential review, but wrap to skipped questions
                        // before offering submission at the end of the batch.
                        let next = if self.question_index + 1 < questions.len() {
                            self.question_index + 1
                        } else {
                            questions
                                .iter()
                                .position(|question| !self.answers.contains_key(&question.id))
                                .unwrap_or(questions.len())
                        };
                        self.select_question(next);
                        None
                    }
                } else if self.prompt_choice == 1 {
                    self.select_question(0);
                    None
                } else if let Some(index) = questions
                    .iter()
                    .position(|question| !self.answers.contains_key(&question.id))
                {
                    self.select_question(index);
                    None
                } else {
                    Some(PromptResponse::Questions(Value::Object(
                        self.answers.clone(),
                    )))
                }
            }
        };
        if let Some(value) = value {
            if let Some(prompt) = self.prompts.pop_front() {
                let _ = prompt.reply.send(Ok(value));
            }
            self.reset_prompt();
            if self.prompts.is_empty() {
                self.prompt_active = false;
            }
        }
    }
    pub fn event(&mut self, event: Event) {
        if matches!(&event, Event::Key(key) if key.kind == KeyEventKind::Release) {
            return;
        }
        if matches!(&event, Event::Mouse(mouse) if mouse.kind == MouseEventKind::Moved && self.hover == Some((mouse.column, mouse.row)))
        {
            return;
        }
        if let Event::Mouse(mouse) = &event
            && mouse.kind == MouseEventKind::Moved
        {
            if let Some(menu) = &mut self.menu {
                let point = (mouse.column, mouse.row);
                self.hover = Some(point);
                // Palettes own hover while open. Only real pointer movement
                // changes the keyboard selection; drawing never re-applies it.
                if let Some(index) = self.hits.iter().rev().find_map(|(rect, hit)| match hit {
                    Hit::Menu(index) if rect.contains(point.into()) => Some(*index),
                    _ => None,
                }) && index < menu.filtered().len()
                    && menu.selected != index
                {
                    menu.selected = index;
                    self.dirty = true;
                }
                self.preview_theme();
                return;
            }
            let target = |point: Option<(u16, u16)>| {
                point.and_then(|point| {
                    self.hits.iter().position(|(rect, hit)| {
                        rect.contains(point.into())
                            && match hit {
                                Hit::Agent(_) => true,
                                Hit::Entry(index, _) => self
                                    .entries
                                    .get(*index)
                                    .is_some_and(|entry| entry.expandable),
                                _ => false,
                            }
                    })
                })
            };
            let point = (mouse.column, mouse.row);
            if target(self.hover) == target(Some(point)) {
                self.hover = Some(point);
                return;
            }
        }
        self.dirty = true;
        match event {
            Event::Key(mut key) if key.kind != KeyEventKind::Release => {
                key.kind = KeyEventKind::Press;
                key.state = crossterm::event::KeyEventState::NONE;
                self.key(key);
            }
            Event::Paste(text) => {
                let text = model::clean(&text);
                match self.input_target() {
                    InputTarget::Menu => {
                        if let Some(menu) = &mut self.menu {
                            menu.input.insert(&text);
                            menu.selected = 0;
                        }
                    }
                    InputTarget::Search => {
                        self.search_editor.as_mut().unwrap().insert(&text);
                    }
                    InputTarget::Prompt => {
                        self.prompt_editor.insert(&text);
                        if self.multiple_questions() {
                            self.question_editing = true;
                            self.invalidate_question_answer();
                        }
                    }
                    InputTarget::Composer if text.lines().count() > 12 => {
                        self.editor.insert_paste(text)
                    }
                    InputTarget::Composer => self.editor.insert(&text),
                    InputTarget::None => {}
                }
            }
            Event::Mouse(mouse) => {
                let point = (mouse.column, mouse.row);
                match mouse.kind {
                    MouseEventKind::Moved => self.hover = Some(point),
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                        if self.menu.is_some() =>
                    {
                        self.menu_key(KeyEvent::new(
                            if mouse.kind == MouseEventKind::ScrollUp {
                                KeyCode::Up
                            } else {
                                KeyCode::Down
                            },
                            M::NONE,
                        ));
                    }
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                        if matches!(self.input_target(), InputTarget::Prompt)
                            && self.composer_rect.contains(point.into()) =>
                    {
                        self.scroll_prompt(
                            self.prompt_options_rect.contains(point.into()),
                            if mouse.kind == MouseEventKind::ScrollUp {
                                -3
                            } else {
                                3
                            },
                        );
                    }
                    MouseEventKind::ScrollUp => {
                        if self.tree_rect.contains(point.into()) {
                            self.tree_scroll = self.tree_scroll.saturating_sub(3);
                        } else {
                            self.scroll(-3);
                        }
                    }
                    MouseEventKind::ScrollDown => {
                        if self.tree_rect.contains(point.into()) {
                            self.tree_scroll += 3;
                        } else {
                            self.scroll(3);
                        }
                    }
                    MouseEventKind::Down(MouseButton::Left) => {
                        self.pressed_entry = None;
                        self.selection = None;
                        let mut select_text = true;
                        if let Some((_, hit)) = self
                            .hits
                            .iter()
                            .rev()
                            .find(|(rect, _)| rect.contains(point.into()))
                            .cloned()
                        {
                            match hit {
                                Hit::Agent(agent) => self.select(agent),
                                Hit::Entry(index, toggle) => {
                                    self.focus = Focus::Content;
                                    self.view().row = index;
                                    if toggle {
                                        self.pressed_entry =
                                            self.entries.get(index).map(|entry| entry.key.clone());
                                    }
                                }
                                Hit::Menu(index) => {
                                    if let Some(menu) = &mut self.menu {
                                        menu.selected = index;
                                    }
                                    self.choose();
                                }
                                Hit::Tab(tab) => {
                                    self.selection = None;
                                    self.view().tab = tab;
                                    self.view().scroll = None;
                                    self.invalidate_content();
                                }
                                Hit::Composer => self.focus = Focus::Composer,
                                Hit::Attachments => self.command("attachments"),
                                Hit::Attention => self.activate_prompt(),
                                Hit::PromptChoice(index) => {
                                    self.activate_prompt();
                                    if self.prompt_choice != index && self.multiple_questions() {
                                        self.invalidate_question_answer();
                                    }
                                    self.prompt_choice = index;
                                    self.question_editing = false;
                                    self.prompt_reveal = true;
                                }
                                Hit::Latest => {
                                    self.view().scroll = None;
                                    // The text-only overlay sits over selectable history.
                                    // Do not pin the viewport again by selecting beneath it.
                                    select_text = false;
                                }
                            }
                        }
                        if select_text && let Some(position) = self.text_position(point) {
                            let scroll = self
                                .views
                                .get(&self.selected)
                                .and_then(|v| v.scroll)
                                .unwrap_or(
                                    self.content_rows
                                        .saturating_sub(self.content_rect.height as usize),
                                );
                            // Hold the viewport still while selecting a streaming reply.
                            self.view().scroll = Some(scroll);
                            self.focus = Focus::Content;
                            self.selection = Some((position, position));
                        }
                    }
                    MouseEventKind::Up(MouseButton::Left) => {
                        if self.pressed_entry.is_none()
                            && let Some((anchor, _)) = self.selection
                            && let Some(head) = self.text_position(point)
                        {
                            self.selection = Some((anchor, head));
                        }
                        if let Some(key) = self.pressed_entry.take()
                            && let Some((_, Hit::Entry(index, true))) = self
                                .hits
                                .iter()
                                .rev()
                                .find(|(rect, _)| rect.contains(point.into()))
                            && self
                                .entries
                                .get(*index)
                                .is_some_and(|entry| entry.key == key)
                        {
                            self.view().row = *index;
                            self.toggle();
                        }
                    }
                    MouseEventKind::Drag(MouseButton::Left)
                        if self.content_rect.contains(point.into()) =>
                    {
                        self.pressed_entry = None;
                        if let Some((anchor, _)) = self.selection
                            && let Some(head) = self.text_position(point)
                        {
                            self.selection = Some((anchor, head));
                        }
                    }
                    _ => {}
                }
            }
            Event::Resize(..) => {
                self.selection = None;
                self.prompt_reveal = true;
            }
            _ => {}
        }
        self.preview_theme();
    }
    fn text_position(&self, point: (u16, u16)) -> Option<super::render::TextPosition> {
        if !self.content_rect.contains(point.into()) {
            return None;
        }
        let scroll = self
            .views
            .get(&self.selected)
            .and_then(|view| view.scroll)
            .unwrap_or(
                self.content_rows
                    .saturating_sub(self.content_rect.height as usize),
            );
        let row = (scroll + point.1.saturating_sub(self.content_rect.y) as usize)
            .min(self.render.rows.len().checked_sub(1)?);
        if !self.render.rows[row].selectable {
            return None;
        }
        Some(super::render::TextPosition {
            row,
            byte: self.render.rows[row].byte_at_column(point.0),
        })
    }
    fn input_target(&self) -> InputTarget {
        if self.menu.is_some() {
            InputTarget::Menu
        } else if self.search_editor.is_some() {
            InputTarget::Search
        } else if self.prompt_active && !self.prompts.is_empty() {
            InputTarget::Prompt
        } else if !self.selected.path().is_empty() {
            InputTarget::None
        } else {
            InputTarget::Composer
        }
    }
    fn key(&mut self, key: KeyEvent) {
        let target = self.input_target();
        if matches!(target, InputTarget::None) && self.focus == Focus::Composer {
            self.focus = Focus::Content;
        }
        if matches!(target, InputTarget::Menu) {
            self.menu_key(key);
            return;
        }
        if matches!(target, InputTarget::Search) {
            let editor = self.search_editor.as_mut().unwrap();
            match key.code {
                KeyCode::Esc => self.search_editor = None,
                KeyCode::Enter => {
                    let query = editor.text.clone();
                    self.search_editor = None;
                    self.view().query = query;
                    self.find(false);
                }
                _ => {
                    editor.handle(key);
                }
            }
            return;
        }
        if matches!(target, InputTarget::Prompt) {
            let options = self.prompt_options();
            let multiple = self.multiple_questions();
            let editing_question = multiple
                && matches!(
                self.prompts.front().map(|p| &p.kind),
                Some(PromptKind::Questions { questions, .. }) if self.question_index < questions.len());
            let old_choice = self.prompt_choice;
            // Authentication editors contain secrets: never snapshot them for
            // ordinary question draft change detection.
            let old_text = editing_question.then(|| self.prompt_editor.text.clone());
            let old_index = self.question_index;
            match key.code {
                KeyCode::Esc => {
                    if matches!(
                        self.prompts.front().map(|p| &p.kind),
                        Some(PromptKind::Authentication(_))
                    ) {
                        if let Some(prompt) = self.prompts.pop_front() {
                            let _ = prompt.reply.send(Err("authentication cancelled".into()));
                        }
                        self.reset_prompt();
                        if self.prompts.is_empty() {
                            self.prompt_active = false;
                        }
                    } else {
                        self.prompt_active = false;
                    }
                }
                KeyCode::PageUp | KeyCode::PageDown => {
                    let options = key.modifiers.contains(M::CONTROL);
                    let height = if options {
                        self.prompt_options_rect.height
                    } else {
                        self.prompt_body_rect.height
                    };
                    self.scroll_prompt(
                        options,
                        height.max(1) as isize * if key.code == KeyCode::PageUp { -1 } else { 1 },
                    );
                }
                KeyCode::Left | KeyCode::Right if multiple && !self.question_editing => {
                    self.switch_question(if key.code == KeyCode::Left { -1 } else { 1 });
                }
                KeyCode::Tab | KeyCode::BackTab if editing_question => {
                    self.question_editing = !self.question_editing;
                }
                KeyCode::Up | KeyCode::BackTab => {
                    self.question_editing = false;
                    self.prompt_choice = self.prompt_choice.saturating_sub(1);
                    self.prompt_reveal = true;
                }
                KeyCode::Down | KeyCode::Tab => {
                    self.question_editing = false;
                    if !options.is_empty() {
                        self.prompt_choice = (self.prompt_choice + 1) % options.len();
                        self.prompt_reveal = true;
                    }
                }
                KeyCode::Enter => self.answer(),
                _ => {
                    if editing_question
                        && matches!(
                            key.code,
                            KeyCode::Char(_) | KeyCode::Backspace | KeyCode::Delete
                        )
                    {
                        self.question_editing = true;
                    }
                    self.prompt_editor.handle(key);
                }
            }
            if editing_question
                && key.code != KeyCode::Enter
                && self.question_index == old_index
                && (self.prompt_choice != old_choice
                    || old_text
                        .as_deref()
                        .is_some_and(|text| self.prompt_editor.text != text))
            {
                self.invalidate_question_answer();
            }
            return;
        }
        if let Some((prefix, _)) = self.leader.take() {
            if let Some(action) = self.keys.action(Some(prefix), key) {
                self.command(&action);
            }
            return;
        }
        if let Some(action) = self.keys.action(None, key) {
            self.command(&action);
            return;
        }
        if self.keys.prefix(key) {
            self.leader = Some((key, Instant::now()));
            return;
        }
        match key.code {
            KeyCode::Tab | KeyCode::BackTab => {
                let reverse = key.code == KeyCode::BackTab || key.modifiers.contains(M::SHIFT);
                self.focus = match (self.focus, reverse) {
                    (Focus::Composer, false) | (Focus::Content, true) => Focus::Tree,
                    (Focus::Tree, false) | (Focus::Composer, true) => Focus::Content,
                    _ => Focus::Composer,
                };
                if self.focus == Focus::Tree && self.tree_rect.height == 0 {
                    self.focus = if reverse {
                        Focus::Composer
                    } else {
                        Focus::Content
                    };
                }
                if !self.selected.path().is_empty() && self.focus == Focus::Composer {
                    self.focus = if reverse && self.tree_rect.height > 0 {
                        Focus::Tree
                    } else {
                        Focus::Content
                    };
                }
                if self.focus == Focus::Content {
                    let current = self.view().row;
                    let visible: Vec<_> = self
                        .hits
                        .iter()
                        .filter_map(|(_, hit)| {
                            if let Hit::Entry(index, _) = hit {
                                Some(*index)
                            } else {
                                None
                            }
                        })
                        .collect();
                    if !visible.contains(&current)
                        && let Some(first) = visible.first()
                    {
                        self.view().row = *first;
                    }
                }
                return;
            }
            KeyCode::PageUp => {
                self.scroll(-(self.content_rect.height as isize));
                return;
            }
            KeyCode::PageDown => {
                self.scroll(self.content_rect.height as isize);
                return;
            }
            KeyCode::Char('u' | 'd') if key.modifiers.contains(M::CONTROL | M::ALT) => {
                self.scroll(
                    (self.content_rect.height as isize / 2)
                        * if key.code == KeyCode::Char('u') {
                            -1
                        } else {
                            1
                        },
                );
                return;
            }
            KeyCode::Esc => {
                if self.busy() {
                    self.interrupt();
                } else {
                    self.focus = if self.selected.path().is_empty() {
                        Focus::Composer
                    } else {
                        Focus::Content
                    };
                }
                return;
            }
            KeyCode::Char('c') if key.modifiers.contains(M::CONTROL) => {
                if self.focus == Focus::Composer && !self.editor.text.is_empty() {
                    self.editor.clear();
                } else if self.busy() {
                    self.interrupt();
                } else {
                    self.command("exit");
                }
                return;
            }
            _ => {}
        }
        match self.focus {
            Focus::Composer => match key.code {
                KeyCode::Enter if !key.modifiers.is_empty() => self.editor.insert("\n"),
                KeyCode::Char('j') if key.modifiers.contains(M::CONTROL) => {
                    self.editor.insert("\n")
                }
                KeyCode::Enter => {
                    let has_pastes = self.editor.has_pastes();
                    let text = self.editor.take();
                    if !has_pastes && text.starts_with('/') && !text.contains('\n') {
                        let command = text.trim_start_matches('/').trim().to_owned();
                        self.command(&command);
                    } else {
                        let images = std::mem::take(&mut self.images);
                        self.paused = false;
                        self.submit(text, images);
                    }
                }
                KeyCode::Up if key.modifiers.is_empty() && self.editor.is_first_visual_row() => {
                    self.prompt_history(false)
                }
                KeyCode::Down if key.modifiers.is_empty() && self.editor.is_last_visual_row() => {
                    self.prompt_history(true)
                }
                KeyCode::Char('/') if self.editor.text.is_empty() => self.command("commands"),
                KeyCode::Char('@') => self.command("files"),
                _ => {
                    self.editor.handle(key);
                }
            },
            Focus::Tree => {
                let agents = self.projection.visible(&self.selected);
                match key.code {
                    KeyCode::Up => self.tree_cursor = self.tree_cursor.saturating_sub(1),
                    KeyCode::Down => {
                        self.tree_cursor =
                            (self.tree_cursor + 1).min(agents.len().saturating_sub(1))
                    }
                    KeyCode::Enter => {
                        if let Some(agent) = agents.get(self.tree_cursor) {
                            self.select(agent.id.clone());
                        }
                    }
                    KeyCode::Left => self.command("parent"),
                    KeyCode::Right => self.command("child"),
                    _ => {}
                }
            }
            Focus::Content => match key.code {
                KeyCode::Home => self.view().scroll = Some(0),
                KeyCode::End => self.view().scroll = None,
                KeyCode::Up | KeyCode::Down => {
                    let current = self.view().row;
                    let next = if key.code == KeyCode::Up {
                        (0..current.min(self.entries.len()))
                            .rev()
                            .find(|&index| super::render::entry_selectable(&self.entries[index]))
                    } else {
                        (current.saturating_add(1)..self.entries.len())
                            .find(|&index| super::render::entry_selectable(&self.entries[index]))
                    };
                    if let Some(next) = next {
                        self.view().row = next;
                        self.reveal_row();
                    }
                }
                KeyCode::Enter => self.toggle(),
                KeyCode::Char('[' | ']') => {
                    self.selection = None;
                    let tab = self.view().tab.next(key.code == KeyCode::Char('['));
                    self.view().tab = tab;
                    self.view().scroll = None;
                    self.invalidate_content();
                }
                KeyCode::Char('/') => self.search_editor = Some(Editor::default()),
                KeyCode::Char('n' | 'N') => self.find(key.code == KeyCode::Char('N')),
                KeyCode::Char('y') => self.copy(),
                KeyCode::Char('o') => self.output_menu(),
                KeyCode::Char('c') => {
                    let row = self.view().row;
                    if let Some(job) = self.entries.get(row).and_then(|e| e.job) {
                        self.confirm(ConfirmAction::CancelJob(job));
                    }
                }
                _ => {}
            },
        }
    }
    fn scroll(&mut self, delta: isize) {
        let max = self
            .content_rows
            .saturating_sub(self.content_rect.height as usize);
        let old = self.view().scroll.unwrap_or(max);
        let next = old.saturating_add_signed(delta).min(max);
        self.view().scroll = if next == max { None } else { Some(next) };
    }
    fn reveal_row(&mut self) {
        let row = self.view().row;
        if let Some(line) = self.render.rows.entry_start(row) {
            self.view().scroll = Some(line);
        }
    }
    fn toggle(&mut self) {
        let row = self.view().row;
        if let Some(entry) = self.entries.get(row)
            && entry.expandable
        {
            // Toggling needs metadata, not a clone of the entire trace/document.
            let (key, job, surface, default_open) = (
                entry.key.clone(),
                entry.job,
                entry.surface,
                entry.default_open,
            );
            let all = self.details
                && (job.is_some()
                    || (self.view().tab == Tab::Conversation && surface == model::Surface::Tool));
            let view = self.view();
            let closing = view.is_expanded(&key, all || default_open);
            if closing {
                view.expanded.remove(&key);
                view.collapsed.insert(key);
            } else {
                view.collapsed.remove(&key);
                view.expanded.insert(key);
            }
            self.selection = None;
            self.invalidate_content();
            if !closing && let Some(job) = job {
                self.fetch_output(job);
            }
        }
    }
    fn select(&mut self, agent: AgentId) {
        self.selected = agent;
        self.focus = Focus::Content;
        self.selection = None;
        self.invalidate_content();
    }
    fn find(&mut self, backwards: bool) {
        let query = self.view().query.to_lowercase();
        if query.is_empty() {
            return;
        }
        let start = self.view().row;
        let count = self.entries.len();
        for step in 1..=count {
            let index = if backwards {
                (start + count - step) % count
            } else {
                (start + step) % count
            };
            if super::render::entry_selectable(&self.entries[index])
                && self.entries[index].text.to_lowercase().contains(&query)
            {
                self.view().row = index;
                self.reveal_row();
                return;
            }
        }
        self.notice("No matches");
    }
    fn copy(&mut self) {
        if self.focus == Focus::Composer
            && let Some(text) = self.editor.selected_text()
        {
            self.clipboard = Some(text.to_owned());
            self.notice("Copied selected input");
            return;
        }
        if let Some((a, b)) = self.selection
            && a != b
        {
            self.clipboard = Some(super::render::selected_text(&self.render.rows, (a, b)));
        } else {
            let row = self.view().row;
            self.clipboard = self
                .entries
                .get(row)
                .filter(|entry| super::render::entry_selectable(entry))
                .or_else(|| {
                    self.entries
                        .iter()
                        .rev()
                        .find(|e| e.surface == model::Surface::Agent)
                })
                .map(|e| model::clean(&e.text));
        }
        self.notice("Copied to terminal clipboard");
    }
    fn interrupt(&mut self) {
        self.paused = true;
        self.cancel_queue_delivery();
        let Some(session) = self.session.clone() else {
            return;
        };
        // Record the action immediately, before any subsequent prompt can be submitted.
        self.root_notifier().send("Interrupted");
        tokio::spawn(async move {
            session.interrupt().await;
        });
    }
    fn prompt_history(&mut self, forward: bool) {
        if self.history.is_empty() {
            return;
        }
        let n = self.history.len();
        if self.history_index.is_none() {
            if forward {
                return;
            }
            self.history_draft = self.editor.clone();
            self.history_index = Some(n - 1);
        } else {
            self.history_index = Some(if forward {
                self.history_index.unwrap() + 1
            } else {
                self.history_index.unwrap().saturating_sub(1)
            });
        }
        if let Some(i) = self.history_index {
            if i >= n {
                self.editor = self.history_draft.clone();
                self.history_index = None;
            } else {
                self.editor.set(self.history[i].clone());
            }
        }
    }
    fn queued_input(&mut self, text: String, images: Vec<PathBuf>) -> QueuedInput {
        self.next_queued_id += 1;
        QueuedInput {
            id: self.next_queued_id,
            generation: 0,
            delivery: None,
            text,
            images,
            model: self.model.clone(),
        }
    }
    fn queue_items(&self) -> Vec<Item> {
        self.queue
            .iter()
            .map(|queued| {
                Item::new(
                    queued.id.to_string(),
                    super::format::brief(&queued.text, 100),
                    self.launch
                        .config
                        .models
                        .get(&queued.model)
                        .map_or(queued.model.as_str(), |profile| profile.model.as_str()),
                )
            })
            .collect()
    }
    fn remove_queued(&mut self, id: u64) -> Option<QueuedInput> {
        let index = self.queue.iter().position(|queued| queued.id == id)?;
        if let Some(token) = &self.queue[index].delivery
            && !token.cancel()
        {
            self.notice("This message has already been submitted to the model");
            return None;
        }
        self.queue.remove(index)
    }
    pub fn agent_status(&self, agent: &model::AgentInfo) -> (bool, String) {
        let pending = self.prompts.iter().find(|prompt| match &prompt.kind {
            PromptKind::Approval(request) => request.agent == agent.id,
            PromptKind::Questions { agent: owner, .. } => owner == &agent.id,
            _ => false,
        });
        match pending.map(|prompt| &prompt.kind) {
            Some(PromptKind::Approval(_)) => (false, "Waiting for permission".into()),
            Some(_) => (false, "Waiting for user input".into()),
            None => self.projection.status(agent, &self.snapshot),
        }
    }
    fn agent_items(&self) -> Vec<Item> {
        self.projection
            .agents
            .iter()
            .map(|agent| {
                Item::new(
                    agent.id.to_string(),
                    format!(
                        "{}{}{}",
                        "    ".repeat(agent.id.depth()),
                        agent.name,
                        model::target_suffix(&agent.target)
                    ),
                    format!(
                        "{}   {}",
                        self.agent_status(agent).1,
                        model::agent_footer(&self.snapshot, &self.projection, &agent.id)
                    ),
                )
            })
            .collect()
    }
    pub fn refresh_agent_menu(&mut self) {
        if !self
            .menu
            .as_ref()
            .is_some_and(|menu| matches!(menu.kind, MenuKind::Agents))
        {
            return;
        }
        let items = self.agent_items();
        let menu = self.menu.as_mut().unwrap();
        let selected = menu
            .filtered()
            .get(menu.selected)
            .map(|item| item.value.clone());
        menu.items = items;
        menu.selected = selected
            .and_then(|id| menu.filtered().iter().position(|item| item.value == id))
            .unwrap_or(0);
    }
    fn refresh_queue_menu(&mut self) {
        if !self
            .menu
            .as_ref()
            .is_some_and(|menu| matches!(menu.kind, MenuKind::Queue))
        {
            return;
        }
        let items = self.queue_items();
        let menu = self.menu.as_mut().unwrap();
        let selected = menu
            .filtered()
            .get(menu.selected)
            .map(|item| item.value.clone());
        menu.items = items;
        menu.selected = selected
            .and_then(|id| menu.filtered().iter().position(|item| item.value == id))
            .unwrap_or(0);
    }
    fn preview_theme(&mut self) {
        if let Some(menu) = &self.menu
            && matches!(menu.kind, MenuKind::Themes)
        {
            if let Some(item) = menu.filtered().get(menu.selected) {
                self.light = item.value == "light";
            } else if let Some(previous) = self.theme_preview {
                self.light = previous;
            }
        } else if let Some(previous) = self.theme_preview.take() {
            self.light = previous;
        }
    }
    fn open(&mut self, title: &str, kind: MenuKind, items: Vec<Item>) {
        if let Some(previous) = self.theme_preview.take() {
            self.light = previous;
        }
        let selected = if matches!(kind, MenuKind::Themes) {
            self.theme_preview = Some(self.light);
            usize::from(self.light)
        } else {
            0
        };
        self.next_menu_id = self.next_menu_id.wrapping_add(1);
        self.menu = Some(Menu {
            id: self.next_menu_id,
            title: title.into(),
            kind,
            items,
            input: Editor::default(),
            selected,
        });
        self.preview_theme();
    }
    fn info(&mut self, title: &str, text: String) {
        self.open(
            title,
            MenuKind::Info,
            text.lines().map(|line| Item::new("", line, "")).collect(),
        );
    }
    fn confirm(&mut self, action: ConfirmAction) {
        self.open(
            "Confirm action",
            MenuKind::Confirm(action),
            vec![
                Item::new("no", "Keep working", ""),
                Item::new("yes", "Stop work and continue", ""),
            ],
        );
    }
    fn switch(&mut self, id: Option<SessionId>) {
        if self.stopping || self.switch_restore.is_some() {
            return;
        }
        if self.creating {
            self.deferred_switch = Some(id);
            self.paused = true;
            return;
        }
        if id.is_some() && id == self.session_id() {
            self.select(self.root_agent().clone());
            return;
        }
        self.switch_restore = Some(self.paused);
        self.paused = true;
        self.cancel_queue_delivery();
        self.notice(if id.is_some() {
            "Opening session…"
        } else {
            "New session"
        });
        let old = self.session.clone();
        let launch = self.launch.clone();
        let tx = self.tx.clone();
        let status = self.status.clone();
        tokio::spawn(async move {
            status.flush().await;
            let result = async {
                let destination = match id {
                    Some(id) => Some(launch.create(Some(id)).await?),
                    None => None,
                };
                if let Some(old) = old
                    && let Err(error) = old.shutdown().await
                {
                    if let Some(destination) = destination {
                        let _ = destination.shutdown().await;
                    }
                    return Err(error.to_string());
                }
                Ok(destination)
            }
            .await;
            if let Err(error) = tx.send(Work::SessionReady(result))
                && let Work::SessionReady(Ok(Some(session))) = error.0
            {
                let _ = session.shutdown().await;
            }
        });
    }
    pub fn command(&mut self, command: &str) {
        match command {
            "commands" => self.open(
                "Commands", MenuKind::Commands,
                COMMANDS.iter().filter(|(id, _, _)| !matches!(*id, "commands" | "child" | "parent" | "inspect"))
                    .map(|(id, label, _)| Item::new(*id, *label, self.keys.binding(id))).collect(),
            ),
            "model" | "models" => {
                self.open(
                "Model", MenuKind::Models,
                self.launch.config.models.iter().map(|(name, profile)| {
                    Item::new(name, name, format!("{} · {}", profile.provider, profile.model))
                }).collect(),
                );
                if let Some(menu) = &mut self.menu {
                    menu.selected = menu.items.iter().position(|item| item.value == self.model).unwrap_or(0);
                }
            },
            "agents" => self.open("Agents", MenuKind::Agents, self.agent_items()),
            "themes" => self.open("Theme", MenuKind::Themes, vec![
                Item::new("dark", "Dark", ""), Item::new("light", "Light", ""),
            ]),
            "inspect" => {
                self.focus = Focus::Content;
                self.view().tab = Tab::Conversation;
            }
            "jobs" | "requests" => {
                self.focus = Focus::Content;
                self.view().tab = match command {
                    "jobs" => Tab::Jobs,
                    _ => Tab::Requests,
                };
                self.view().scroll = None;
            }
            "thinking" => self.thinking = !self.thinking,
            "details" => {
                self.details = !self.details;
                if self.details { for view in self.views.values_mut() { view.collapsed.clear(); } }
            }
            "editor" => self.external_editor = true,
            "copy" => self.copy(),
            "attention" => self.activate_prompt(),
            "resume" => {
                self.paused = false;
                if !self.busy()
                    && let Some(PendingStart::Script(path)) = self.pending_start.take()
                {
                    self.start_script(path);
                }
                self.notice("Queued input resumed");
            }
            "retry" => {
                if !self.busy() && matches!(
                    self.snapshot.activity.get(self.root_agent()),
                    Some(AgentActivity::Failed(_) | AgentActivity::Interrupted),
                ) {
                    self.operation = true;
                    let Some(session) = self.session.clone() else { return; };
                    let tx = self.tx.clone();
                    tokio::spawn(async move {
                        let result = session.continue_turn().await.map(|_| ()).map_err(|e| e.to_string());
                        let _ = tx.send(Work::Done { session: session.id(), result });
                    });
                } else {
                    self.notice("No failed or interrupted root turn to retry");
                }
            }
            "queue" => self.open(
                "Queued follow-ups · Enter edit · Delete remove", MenuKind::Queue,
                self.queue_items(),
            ),
            "attach" => self.open("Image path · Enter attach", MenuKind::Attach, vec![]),
            "attachments" => {
                let mut items: Vec<_> = self.editor.pastes().map(|(i, text)| Item::attachment(
                    Attachment::Paste(i),
                    format!("Pasted text / file · {} lines", text.lines().count()),
                    super::format::brief(text, 60),
                )).collect();
                items.extend(self.images.iter().enumerate().map(|(i, path)| {
                    Item::attachment(Attachment::Image(i), path.display().to_string(), "Image")
                }));
                self.open("Attachments · Enter inspect · Delete remove", MenuKind::Attachments, items);
            }
            "new" => {
                if self.active_work() { self.confirm(ConfirmAction::NewSession); }
                else { self.switch(None); }
            }
            "exit" => {
                if self.active_work() { self.confirm(ConfirmAction::Exit); }
                else { self.shutdown(); }
            }
            "child" => {
                if let Some(agent) = self.projection.agents.iter().find(|agent| {
                    agent.id.parent().as_ref() == Some(&self.selected)
                }) {
                    self.select(agent.id.clone());
                }
            }
            "parent" => {
                if let Some(parent) = self.selected.parent() { self.select(parent); }
            }
            "sessions" => {
                let root = self.launch.sessions.clone();
                let tx = self.tx.clone();
                self.open("Resume session", MenuKind::Sessions, vec![]);
                let id = self.next_menu_id;
                tokio::spawn(async move {
                    let result = load_sessions(root).await;
                    let _ = tx.send(Work::MenuLoaded { id, result });
                });
            }
            "files" => {
                self.open("Attach workspace file", MenuKind::Files, vec![]);
                let id = self.next_menu_id;
                let root = self.launch.workspace.clone();
                let tx = self.tx.clone();
                tokio::task::spawn_blocking(move || {
                    let mut items = vec![];
                    walk_files(&root, &root, &mut items);
                    items.sort_by(|a, b| a.label.cmp(&b.label));
                    let _ = tx.send(Work::MenuLoaded { id, result: Ok(items) });
                });
            }
            "export" => {
                let entries = model::entries(
                    &self.snapshot, &self.projection, &self.selected,
                    &View::default(), &self.outputs, self.thinking, true,
                );
                let text = entries.iter().map(|entry| entry.text.as_str()).collect::<Vec<_>>().join("\n\n");
                let Some(session) = &self.session else {
                    self.notice("No session to export yet");
                    return;
                };
                let path = session.directory().join(format!(
                    "conversation-{}.md", super::format::agent_label(&self.selected).replace(':', "-"),
                ));
                let notices = self.notifier();
                tokio::spawn(async move {
                    let notice = match tokio::fs::write(&path, text).await {
                        Ok(_) => format!("Exported {}", path.display()),
                        Err(error) => error.to_string(),
                    };
                    notices.send(notice);
                });
            }
            "help" => self.info("Skyhook help", format!(concat!(
                "{}\n\n",
                "Tab / Shift+Tab: composer, tree, content\n",
                "Enter: send / queue / expand\n",
                "Alt+Enter / Ctrl+J: newline\n",
                "PageUp / PageDown: scroll\n",
                "Content: / search; n/N next/previous; [ ] inspector tabs\n",
                "Ctrl+A/E: line start/end; Ctrl+W: delete word; Ctrl+U/K: delete to line boundary\n",
                "Ctrl+- / Ctrl+.: undo/redo\n\n",
                "Footer: session output · total input(uncached) · estimated context (current/capacity)\n",
                "Context belongs to the selected agent and includes system, tools and runtime state.\n",
                "Messages always go to skyhook, including while viewing a child.\n",
                "Model changes apply from the next submitted message. Instruction changes apply to new sessions.\n",
                "Mouse: click agent or tool, scroll, drag text then copy.\n",
                "The workspace and session ID are plain text; use terminal selection to copy them.\n",
                "Themes preview while navigating; Escape cancels and Enter saves.\n",
                "Ctrl+X Y copies through the terminal clipboard (OSC 52).\n\n",
                "Settings: {}",
            ), self.keys.help(), state::config_path().display())),
            "" => {}
            _ => self.notice(format!("Unknown command: /{command}. Use /help.")),
        }
        if matches!(
            command,
            "inspect" | "jobs" | "requests" | "thinking" | "details"
        ) {
            self.invalidate_content();
        }
        self.dirty = true;
    }
    fn menu_key(&mut self, mut key: KeyEvent) {
        if key.modifiers.contains(M::CONTROL) {
            if key.code == KeyCode::Char('p') {
                key.code = KeyCode::Up;
            }
            if key.code == KeyCode::Char('n') {
                key.code = KeyCode::Down;
            }
        }
        match key.code {
            KeyCode::Home => {
                if let Some(menu) = &mut self.menu {
                    menu.selected = 0;
                }
            }
            KeyCode::End => {
                if let Some(menu) = &mut self.menu {
                    menu.selected = menu.filtered().len().saturating_sub(1);
                }
            }
            KeyCode::Esc => self.menu = None,
            KeyCode::Up => {
                if let Some(menu) = &mut self.menu {
                    menu.selected = menu.selected.saturating_sub(1);
                }
            }
            KeyCode::Down => {
                if let Some(menu) = &mut self.menu {
                    menu.selected =
                        (menu.selected + 1).min(menu.filtered().len().saturating_sub(1));
                }
            }
            KeyCode::PageUp => {
                if let Some(menu) = &mut self.menu {
                    menu.selected = menu.selected.saturating_sub(10);
                }
            }
            KeyCode::PageDown => {
                if let Some(menu) = &mut self.menu {
                    menu.selected =
                        (menu.selected + 10).min(menu.filtered().len().saturating_sub(1));
                }
            }
            KeyCode::Enter | KeyCode::Tab => self.choose(),
            KeyCode::Delete => {
                if let Some(menu) = &self.menu
                    && matches!(menu.kind, MenuKind::Attachments)
                {
                    if let Some(item) = menu.filtered().get(menu.selected) {
                        match item.attachment {
                            Some(Attachment::Paste(index)) => {
                                self.editor.remove_paste(index);
                            }
                            Some(Attachment::Image(index)) => {
                                self.images.remove(index);
                            }
                            None => {}
                        }
                    }
                    self.command("attachments");
                    return;
                }
                if let Some(menu) = &self.menu
                    && matches!(menu.kind, MenuKind::Queue)
                    && let Some(item) = menu.filtered().get(menu.selected)
                    && let Ok(id) = item.value.parse::<u64>()
                {
                    self.remove_queued(id);
                    self.refresh_queue_menu();
                }
            }
            _ => {
                if let Some(menu) = &mut self.menu {
                    menu.input.handle(key);
                    menu.selected = 0;
                }
            }
        }
    }
    fn choose(&mut self) {
        let Some(menu) = self.menu.take() else { return };
        let filtered = menu.filtered();
        let selected = filtered.get(menu.selected);
        let value = selected.map(|i| i.value.clone()).unwrap_or_default();
        let attachment = selected.and_then(|i| i.attachment);
        match menu.kind {
            MenuKind::Commands => self.command(&value),
            MenuKind::Models => {
                if !value.is_empty() {
                    self.model = value;
                }
            }
            MenuKind::Agents => {
                if let Some(agent) = self
                    .projection
                    .agents
                    .iter()
                    .find(|a| a.id.to_string() == value)
                {
                    self.select(agent.id.clone());
                }
            }
            MenuKind::Themes => {
                if !matches!(value.as_str(), "light" | "dark") {
                    self.preview_theme();
                    return;
                }
                self.theme_preview = None;
                self.light = value == "light";
                let notices = self.notifier();
                tokio::task::spawn_blocking(move || {
                    if let Err(e) = state::remember_theme(&value) {
                        notices.send(e.to_string());
                    }
                });
            }
            MenuKind::Sessions => {
                if let Ok(id) = value.parse() {
                    if self.active_work() {
                        self.confirm(ConfirmAction::SwitchSession(id));
                    } else {
                        self.switch(Some(id));
                    }
                }
            }
            MenuKind::Files => {
                if value.is_empty() {
                    return;
                }
                let root = self.launch.workspace.clone();
                let path = root.join(value);
                let tx = self.tx.clone();
                let draft = self.draft_revision;
                tokio::spawn(async move {
                    let result = async {
                        let path = tokio::fs::canonicalize(path)
                            .await
                            .map_err(|e| e.to_string())?;
                        if !path.starts_with(&root) {
                            return Err("File reference leaves the workspace".into());
                        }
                        if tokio::fs::metadata(&path)
                            .await
                            .map_err(|e| e.to_string())?
                            .len()
                            > 1_048_576
                        {
                            return Err(
                                "File is larger than 1 MiB; ask the agent to read it instead"
                                    .into(),
                            );
                        }
                        let text = tokio::fs::read_to_string(&path)
                            .await
                            .map_err(|e| e.to_string())?;
                        Ok((path, text))
                    }
                    .await;
                    let _ = tx.send(Work::File { draft, result });
                });
            }
            MenuKind::Attachments => match attachment {
                Some(Attachment::Paste(index)) => {
                    if let Some(text) = self.editor.paste(index) {
                        self.info("Attachment", text.to_owned());
                    }
                }
                Some(Attachment::Image(index)) => {
                    if let Some(path) = self.images.get(index) {
                        self.info("Image attachment", path.display().to_string());
                    }
                }
                None => {}
            },
            MenuKind::Attach => {
                let text = menu.input.text.trim();
                if !text.is_empty() {
                    self.images.push(self.launch.workspace.join(text));
                }
            }
            MenuKind::Queue => {
                if let Ok(id) = value.parse::<u64>()
                    && let Some(queued) = self.remove_queued(id)
                {
                    if !self.editor.text.is_empty() || !self.images.is_empty() {
                        let text = self.editor.take();
                        let images = std::mem::take(&mut self.images);
                        let draft = self.queued_input(text, images);
                        self.queue.push_front(draft);
                    }
                    self.editor.set(queued.text);
                    self.images = queued.images;
                }
            }
            MenuKind::Confirm(action) => {
                if value == "yes" {
                    match action {
                        ConfirmAction::Exit => self.shutdown(),
                        ConfirmAction::NewSession => self.switch(None),
                        ConfirmAction::SwitchSession(id) => self.switch(Some(id)),
                        ConfirmAction::CancelJob(id) => {
                            let Some(session) = self.session.clone() else {
                                return;
                            };
                            let notices = self.notifier();
                            tokio::spawn(async move {
                                let result = session.cancel_job(id).await;
                                notices.send(match result {
                                    Ok(job) => {
                                        format!("Job {id}: {}", model::state_name(job.state))
                                    }
                                    Err(e) => e.to_string(),
                                });
                            });
                        }
                    }
                }
            }
            MenuKind::Output(job) => {
                if value == "search" {
                    self.open(
                        "Search saved output (regex)",
                        MenuKind::OutputSearch(job),
                        vec![],
                    );
                } else if value == "next" {
                    if let Some(position) = self.outputs.get(&job).and_then(|value| {
                        value
                            .get("preview")
                            .filter(|page| page["next_start"].is_u64())
                            .or_else(|| value["truncated"].as_array()?.first())
                    }) {
                        let mut query = self
                            .output_queries
                            .get(&job)
                            .cloned()
                            .unwrap_or_else(|| JobOutputQuery::new(job));
                        query.field = position["field"].as_str().map(str::to_owned);
                        query.start = position["next_start"].as_u64().map(|n| n as usize);
                        query.offset = position["next_offset"].as_u64().map(|n| n as usize);
                        self.set_output_query(job, query);
                    }
                } else if let Some(field) = value.strip_prefix("field:") {
                    let mut query = JobOutputQuery::new(job);
                    query.field = Some(field.into());
                    self.set_output_query(job, query);
                }
            }
            MenuKind::OutputSearch(job) => {
                let field = self
                    .output_queries
                    .get(&job)
                    .and_then(|q| q.field.clone())
                    .unwrap_or_default();
                let mut query = JobOutputQuery::new(job);
                query.field = Some(field);
                query.pattern = Some(menu.input.text.clone());
                query.context = Some(2);
                self.set_output_query(job, query);
            }
            MenuKind::Info => {
                self.menu = Some(menu);
            }
        }
    }
    fn output_menu(&mut self) {
        let row = self.view().row;
        if let Some(job) = self.entries.get(row).and_then(|e| e.job) {
            self.open(
                "Saved output",
                MenuKind::Output(job),
                vec![
                    Item::new("field:/result/stdout", "stdout", ""),
                    Item::new("field:/result/stderr", "stderr", ""),
                    Item::new("field:/result/console", "script console", ""),
                    Item::new("field:/result/value", "script return value", ""),
                    Item::new("field:/result/content", "file content", ""),
                    Item::new("field:", "complete result", ""),
                    Item::new("search", "Search this field", "regex"),
                    Item::new("next", "Next page", ""),
                ],
            );
        }
    }
}
async fn load_sessions(root: PathBuf) -> Result<Vec<Item>, String> {
    let mut entries = match tokio::fs::read_dir(&root).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e.to_string()),
    };
    let mut sessions = vec![];
    while let Some(entry) = entries.next_entry().await.map_err(|e| e.to_string())? {
        let Some(id) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<SessionId>().ok())
        else {
            continue;
        };
        let records = match SessionStore::read_records(&root, id).await {
            Ok(records) => records,
            Err(_) => continue,
        };
        let title = tokio::fs::read(entry.path().join("ui.json"))
            .await
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|v| v["title"].as_str().map(str::to_owned))
            .or_else(|| {
                records.iter().find_map(|r| {
                    if let SessionEvent::MessageCommitted {
                        message: skyhook::provider::protocol::Message::User(blocks),
                    } = &r.event
                    {
                        blocks.iter().find_map(|b| {
                            if let skyhook::provider::protocol::UserContent::Text { text } = b {
                                Some(super::format::brief(text, 100))
                            } else {
                                None
                            }
                        })
                    } else {
                        None
                    }
                })
            })
            .unwrap_or_else(|| id.to_string());
        let timestamp = records.last().map_or(0, |r| r.timestamp_millis);
        sessions.push((
            timestamp,
            Item::new(
                id.to_string(),
                title,
                format!("{} events · {id}", records.len()),
            ),
        ));
    }
    sessions.sort_by_key(|(timestamp, _)| std::cmp::Reverse(*timestamp));
    Ok(sessions.into_iter().map(|(_, item)| item).collect())
}
fn walk_files(root: &std::path::Path, path: &std::path::Path, items: &mut Vec<Item>) {
    if items.len() >= 10000 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        if items.len() >= 10000 {
            break;
        }
        let name = entry.file_name();
        if matches!(
            name.to_str(),
            Some(".git" | "target" | "node_modules" | ".skyhook")
        ) {
            continue;
        }
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            walk_files(root, &entry.path(), items);
        } else if kind.is_file() {
            let relative = entry
                .path()
                .strip_prefix(root)
                .unwrap_or(&entry.path())
                .display()
                .to_string();
            items.push(Item::new(relative.clone(), relative, ""));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interaction::UiInteraction;
    use skyhook::{
        agent::{Question, QuestionOption},
        provider::protocol::{BlockKind, ContentDelta, ItemKind, ResponseEvent},
        remote::EmbeddedShimCatalog,
    };
    use std::sync::Arc;
    use tokio::sync::oneshot;

    fn response_event(app: &mut App, agent: &AgentId, request: u64, event: ResponseEvent) -> bool {
        app.observe(ObservedEvent {
            revision: app.snapshot.revision + 1,
            event: RuntimeEvent::ResponseEvent {
                agent: agent.clone(),
                request,
                event,
            },
        })
    }

    /// One explicitly started provider item/block; appends never synthesize lifecycle events.
    struct TestBlock {
        agent: AgentId,
        request: u64,
        item: String,
        block: String,
    }

    impl TestBlock {
        fn start(
            app: &mut App,
            agent: &AgentId,
            request: u64,
            item: &str,
            position: usize,
            kind: BlockKind,
        ) -> Self {
            let item_kind = match kind {
                BlockKind::Text => ItemKind::Text,
                BlockKind::Reasoning => ItemKind::Reasoning,
                BlockKind::ToolCallArguments => ItemKind::ToolCall,
            };
            response_event(
                app,
                agent,
                request,
                ResponseEvent::ItemStarted {
                    id: item.into(),
                    position,
                    kind: item_kind,
                },
            );
            let block = format!("{item}:0");
            response_event(
                app,
                agent,
                request,
                ResponseEvent::BlockStarted {
                    item: item.into(),
                    id: block.clone(),
                    position: 0,
                    kind,
                },
            );
            Self {
                agent: agent.clone(),
                request,
                item: item.into(),
                block,
            }
        }

        fn delta(&self, app: &mut App, text: &str) -> bool {
            response_event(
                app,
                &self.agent,
                self.request,
                ResponseEvent::BlockDelta {
                    item: self.item.clone(),
                    block: self.block.clone(),
                    delta: ContentDelta::Text(text.into()),
                },
            )
        }
    }

    async fn draft_fixture() -> (tempfile::TempDir, App) {
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
    #[tokio::test]
    async fn command_menu_keeps_configured_shortcuts_separate_and_searchable() {
        let (_root, mut app) = draft_fixture().await;
        app.command("commands");
        let menu = app.menu.as_ref().unwrap();
        assert!(matches!(menu.kind, MenuKind::Commands));
        let hidden = ["commands", "child", "parent", "inspect"];
        assert_eq!(menu.items.len(), COMMANDS.len() - hidden.len());
        for (id, label, _) in COMMANDS {
            if hidden.contains(id) {
                assert!(!menu.items.iter().any(|item| item.value == *id));
                continue;
            }
            let item = menu.items.iter().find(|item| item.value == *id).unwrap();
            assert_eq!(item.label, *label);
            assert_eq!(item.detail, app.keys.binding(id));
        }
        app.keys = KeyMap::new(&std::collections::BTreeMap::from([
            ("new".into(), "alt+n".into()),
            ("exit".into(), "none".into()),
            ("models".into(), "alt+m".into()),
        ]))
        .unwrap();
        app.command("commands");
        let menu = app.menu.as_mut().unwrap();
        let new = menu.items.iter().find(|item| item.value == "new").unwrap();
        assert_eq!(new.label, "New session");
        assert_eq!(new.detail, "Alt+N");
        assert!(
            menu.items
                .iter()
                .find(|item| item.value == "exit")
                .unwrap()
                .detail
                .is_empty()
        );
        assert_eq!(
            menu.items
                .iter()
                .find(|item| item.value == "model")
                .unwrap()
                .detail,
            "Alt+M"
        );
        for (query, expected) in [("ALT+N", "new"), ("new SESSION", "new"), ("alt+m", "model")] {
            menu.input.text = query.into();
            let filtered = menu.filtered();
            assert_eq!(filtered.len(), 1, "query: {query}");
            assert_eq!(filtered[0].value, expected);
        }
        menu.input.text = "ctrl+x n".into();
        assert!(
            menu.filtered().is_empty(),
            "overridden defaults must not remain searchable"
        );
    }

    async fn fixture() -> (tempfile::TempDir, App) {
        let (root, mut app) = draft_fixture().await;
        let session = app.launch.create(None).await.unwrap();
        let snapshot = session.observe().await.snapshot;
        app.set_session(Some(session), snapshot);
        (root, app)
    }
    async fn next_lifecycle(rx: &mut mpsc::UnboundedReceiver<Work>) -> Work {
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

    #[tokio::test]
    async fn reconnecting_keeps_input_delivery_busy_until_interrupted() {
        let (_root, mut app) = draft_fixture().await;
        let agent = app.root_agent().clone();
        app.snapshot.activity.insert(
            agent.clone(),
            AgentActivity::Reconnecting {
                attempt: 2,
                max_attempts: 3,
            },
        );
        assert!(app.busy());
        app.snapshot
            .activity
            .insert(agent, AgentActivity::Interrupted);
        assert!(!app.busy());
    }

    #[tokio::test]
    async fn menu_loads_only_fill_the_originating_open_menu() {
        let (_root, mut app) = draft_fixture().await;
        for kind in [MenuKind::Sessions, MenuKind::Files] {
            app.open("Loading", kind, vec![]);
            let closed = app.menu.as_ref().unwrap().id;
            key(&mut app, KeyCode::Esc, M::NONE);
            app.dirty = false;
            app.work(Work::MenuLoaded {
                id: closed,
                result: Ok(vec![Item::new("old", "old", "")]),
            });
            assert!(app.menu.is_none());
            assert!(!app.dirty);

            app.open("Newer request", MenuKind::Files, vec![]);
            let current = app.menu.as_ref().unwrap().id;
            app.menu.as_mut().unwrap().input.insert("query");
            app.work(Work::MenuLoaded {
                id: closed,
                result: Err("obsolete failure".into()),
            });
            assert!(app.menu.as_ref().unwrap().items.is_empty());
            assert!(!app.dirty);
            app.work(Work::MenuLoaded {
                id: current,
                result: Ok(vec![Item::new("current", "current", "")]),
            });
            let menu = app.menu.as_ref().unwrap();
            assert_eq!(menu.items[0].value, "current");
            assert_eq!(menu.input.text, "query");

            app.info("Replacement overlay", "Keep me".into());
            app.work(Work::MenuLoaded {
                id: current,
                result: Ok(vec![]),
            });
            assert_eq!(app.menu.as_ref().unwrap().title, "Replacement overlay");
        }
    }

    #[tokio::test]
    async fn composer_pastes_submit_in_place_and_restore_history_drafts() {
        let (_root, mut app) = draft_fixture().await;
        // Keep the submitted message queued without starting a provider/session.
        app.creating = true;
        let first = "first\n".repeat(13);
        let second = "second\n".repeat(14);
        app.editor.insert("before after");
        app.editor.cursor = "before ".len();
        app.event(Event::Paste(first.clone()));
        app.editor.insert(" between ");
        app.event(Event::Paste(second.clone()));
        let expected = format!("before {first} between {second}after");
        assert_eq!(app.editor.expanded_text(), expected);
        assert_eq!(app.editor.pastes().count(), 2);
        app.editor.anchor = Some(0);
        app.editor.cursor = app.editor.text.len();
        app.copy();
        assert_eq!(app.clipboard.as_deref(), Some(expected.as_str()));
        app.editor.anchor = None;

        app.history.push("older prompt".into());
        app.prompt_history(false);
        assert_eq!(app.editor.expanded_text(), "older prompt");
        app.prompt_history(true);
        assert_eq!(app.editor.expanded_text(), expected);
        assert_eq!(app.editor.pastes().count(), 2);

        key(&mut app, KeyCode::Enter, M::NONE);
        assert_eq!(app.queue.len(), 1);
        assert_eq!(app.queue[0].text, expected);
        assert!(app.editor.is_empty());
        assert!(!app.editor.has_pastes());
    }

    #[tokio::test]
    async fn composer_short_pastes_and_attachment_removal_use_editor_history() {
        let (_root, mut app) = draft_fixture().await;
        app.event(Event::Paste("short\npaste".into()));
        assert!(!app.editor.has_pastes());
        assert_eq!(app.editor.text, "short\npaste");
        app.event(Event::Paste("long\n".repeat(13)));
        app.command("attachments");
        assert_eq!(app.menu.as_ref().unwrap().items.len(), 1);
        key(&mut app, KeyCode::Delete, M::NONE);
        assert!(!app.editor.has_pastes());
        assert_eq!(app.editor.expanded_text(), "short\npaste");
        key(&mut app, KeyCode::Esc, M::NONE);
        app.editor
            .handle(KeyEvent::new(KeyCode::Char('-'), M::CONTROL));
        assert_eq!(app.editor.pastes().count(), 1);
    }

    #[tokio::test]
    async fn composer_clear_draft_can_undo_text_and_pastes() {
        let (_root, mut app) = draft_fixture().await;
        app.editor.insert("before ");
        app.event(Event::Paste("payload\n".repeat(13)));
        app.editor.insert(" after");
        let expected = app.editor.expanded_text();
        key(&mut app, KeyCode::Char('c'), M::CONTROL);
        assert!(app.editor.is_empty());
        key(&mut app, KeyCode::Char('-'), M::CONTROL);
        assert_eq!(app.editor.expanded_text(), expected);
        assert_eq!(app.editor.pastes().count(), 1);
    }

    #[tokio::test]
    async fn composer_wraps_words_and_vertical_arrows_do_not_skip_to_history() {
        let (_root, mut app) = draft_fixture().await;
        app.history.push("older prompt".into());
        app.editor.insert(&format!("{}ending", "word ".repeat(16)));
        let expected = app.editor.expanded_text();
        let screen = draw(&mut app);
        assert!(screen.contains("word word word"));
        assert!(!app.editor.is_first_visual_row());
        let cursor = app.editor.cursor;
        key(&mut app, KeyCode::Up, M::NONE);
        assert_eq!(app.editor.expanded_text(), expected);
        assert!(app.editor.cursor < cursor);
        assert!(app.history_index.is_none());
        app.editor.cursor = 0;
        key(&mut app, KeyCode::Up, M::NONE);
        assert_eq!(app.editor.expanded_text(), "older prompt");
    }

    #[tokio::test]
    async fn attachment_reads_do_not_leak_into_the_next_submission_or_session() {
        let (_root, mut app) = draft_fixture().await;
        let attachment = |draft| Work::File {
            draft,
            result: Ok((PathBuf::from("fixture.txt"), "contents".into())),
        };
        let draft = app.draft_revision;
        app.info("Unrelated overlay", "Still the same draft".into());
        app.work(attachment(draft));
        assert_eq!(
            app.editor
                .pastes()
                .map(|(_, text)| text)
                .collect::<Vec<_>>(),
            ["File: fixture.txt\ncontents"]
        );
        app.editor.take();

        // Queue without starting a session: even a queued submission consumes its draft.
        app.paused = true;
        app.submit("submitted".into(), vec![]);
        app.dirty = false;
        app.work(attachment(draft));
        assert!(!app.editor.has_pastes());
        assert!(!app.dirty);
        let next_draft = app.draft_revision;
        app.work(attachment(next_draft));
        assert_eq!(app.editor.pastes().count(), 1);

        app.set_session(None, ObservationSnapshot::default());
        app.dirty = false;
        app.work(attachment(next_draft));
        assert!(!app.editor.has_pastes());
        assert!(!app.dirty);
        app.work(attachment(app.draft_revision));
        assert_eq!(app.editor.pastes().count(), 1);
    }

    #[tokio::test]
    async fn first_submit_creates_once_and_preserves_queue_models_and_draft() {
        let (_root, mut app) = draft_fixture().await;
        let broken_skill = app.launch.workspace.join(".agents/skills/broken");
        std::fs::create_dir_all(&broken_skill).unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        app.tx = tx;
        for command in ["export", "retry"] {
            app.command(command);
        }
        assert!(app.session.is_none() && !app.launch.sessions.exists());
        app.submit("first input".into(), vec![]);
        assert!(app.creating);
        assert!(app.session.is_none());
        app.model = "second".into();
        app.submit("second input".into(), vec![PathBuf::from("queued.png")]);
        app.editor.set("still composing".into());
        app.editor.insert_paste("unsent attachment".into());
        let Work::Started(Ok(session)) = next_lifecycle(&mut rx).await else {
            panic!("first submit should create a session");
        };
        assert_eq!(std::fs::read_dir(&app.launch.sessions).unwrap().count(), 1);
        let snapshot = session.observe().await.snapshot;
        app.session_started(session, snapshot);
        assert!(!app.creating);
        assert!(app.operation);
        assert_eq!(app.history, ["first input"]);
        assert_eq!(app.model, "second");
        assert_eq!(app.queue.len(), 1);
        assert_eq!(app.queue[0].text, "second input");
        assert_eq!(app.queue[0].model, "second");
        assert_eq!(app.queue[0].images, [PathBuf::from("queued.png")]);
        assert_eq!(
            app.editor.expanded_text(),
            "still composingunsent attachment"
        );
        assert_eq!(
            app.editor
                .pastes()
                .map(|(_, text)| text)
                .collect::<Vec<_>>(),
            ["unsent attachment"]
        );
        app.status.flush().await;
        let session = app.session.as_ref().unwrap();
        let snapshot = session.observe().await.snapshot;
        assert!(!session.warnings().is_empty());
        for warning in session.warnings() {
            assert!(snapshot.records.values().any(|record| matches!(
                &record.event, skyhook::session::SessionEvent::Status { message }
                if message == &format!("Startup warning: {warning}")
            )));
        }
        session.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn failed_creation_retains_inputs_and_can_resume_without_duplicate_creation() {
        let (_root, mut app) = draft_fixture().await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        app.tx = tx;
        let config = app.launch.config.clone();
        Arc::make_mut(&mut app.launch.config)
            .models
            .shift_remove("first");
        app.submit("first".into(), vec![PathBuf::from("first.png")]);
        app.model = "second".into();
        app.submit("second".into(), vec![]);
        app.editor.set("new draft".into());
        let work = next_lifecycle(&mut rx).await;
        assert!(matches!(work, Work::Started(Err(_))));
        app.work(work);
        assert!(!app.busy());
        assert!(app.paused);
        assert!(app.session.is_none());
        assert_eq!(app.queue.len(), 2);
        assert_eq!(app.queue[0].text, "first");
        assert_eq!(app.queue[0].model, "first");
        assert_eq!(app.queue[0].images, [PathBuf::from("first.png")]);
        assert_eq!(app.queue[1].model, "second");
        assert_eq!(app.editor.text, "new draft");
        app.launch.config = config;
        app.command("resume");
        app.tick();
        assert!(app.creating);
        app.tick();
        assert_eq!(app.queue.len(), 1);
        let Work::Started(Ok(session)) = next_lifecycle(&mut rx).await else {
            panic!("resume should retry creation");
        };
        app.shutdown();
        let snapshot = session.observe().await.snapshot;
        app.session_started(session, snapshot);
        app.work(next_lifecycle(&mut rx).await);
        assert!(app.exit);
        assert!(
            app.history.is_empty(),
            "shutdown must suppress the pending prompt"
        );
    }
    #[tokio::test]
    async fn script_creation_and_new_wait_for_inflight_creation() {
        let (_root, mut app) = draft_fixture().await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        app.tx = tx;
        app.start_script(PathBuf::from("explicit.js"));
        assert!(app.creating);
        app.switch(None);
        let Work::Started(Ok(session)) = next_lifecycle(&mut rx).await else {
            panic!("script should create on demand");
        };
        let snapshot = session.observe().await.snapshot;
        app.session_started(session, snapshot);
        let Work::SessionReady(Ok(None)) = next_lifecycle(&mut rx).await else {
            panic!("new waits for and shuts down created handle");
        };
        app.set_session(None, ObservationSnapshot::default());
        assert!(app.session.is_none());
        assert!(app.pending_start.is_none());
        assert!(!app.operation);
        assert_eq!(std::fs::read_dir(&app.launch.sessions).unwrap().count(), 1);
    }
    #[tokio::test]
    async fn shutdown_waits_for_failed_creation_and_for_new_session_reset() {
        let (_root, mut app) = draft_fixture().await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        app.tx = tx;
        Arc::make_mut(&mut app.launch.config)
            .models
            .shift_remove("first");
        app.submit("unsent".into(), vec![]);
        app.shutdown();
        assert!(!app.exit);
        let work = next_lifecycle(&mut rx).await;
        assert!(matches!(work, Work::Started(Err(_))));
        app.work(work);
        app.work(next_lifecycle(&mut rx).await);
        assert!(app.exit);
        assert!(!app.launch.sessions.exists());

        let (_root, mut app) = fixture().await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        app.tx = tx;
        app.switch(None);
        app.shutdown();
        assert!(!app.exit);
        let Work::SessionReady(Ok(None)) = next_lifecycle(&mut rx).await else {
            panic!("shutdown waits for the reset task");
        };
        app.set_session(None, ObservationSnapshot::default());
        app.work(next_lifecycle(&mut rx).await);
        assert!(app.exit);
    }
    fn key(app: &mut App, code: KeyCode, modifiers: M) {
        app.event(Event::Key(KeyEvent::new(code, modifiers)));
    }
    fn question(
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
    fn draw_buffer(app: &mut App) -> ratatui::buffer::Buffer {
        draw_sized_buffer(app, 60, 24)
    }
    fn draw_sized_buffer(app: &mut App, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| super::super::render::draw(frame, app))
            .unwrap();
        terminal.backend().buffer().clone()
    }
    #[tokio::test]
    async fn tree_inspector_and_menu_roles_survive_selection_and_narrow_layouts() {
        use super::super::render::Palette;
        use ratatui::{buffer::Buffer, style::Color};
        fn assert_role(buffer: &Buffer, rect: Rect, value: &str, fg: Color) {
            let chars: Vec<_> = value.chars().map(|c| c.to_string()).collect();
            for y in rect.y..rect.bottom() {
                for x in rect.x..rect.right() {
                    if x as usize + chars.len() <= rect.right() as usize
                        && chars
                            .iter()
                            .enumerate()
                            .all(|(i, ch)| buffer[(x + i as u16, y)].symbol() == ch)
                    {
                        assert!(
                            (x..x + chars.len() as u16).all(|x| buffer[(x, y)].fg == fg),
                            "wrong role: {value}"
                        );
                        return;
                    }
                }
            }
            panic!("missing {value:?} in {rect:?}");
        }
        let (_root, mut app) = fixture().await;
        app.projection.agents[0].name = "Researcher".into();
        app.projection.agents[0].target = "remote".into();
        let id = app.projection.agents[0].id.clone();
        // A root-only session intentionally hides the tree. Keep a live child
        // so the fixture exercises the real visible-tree layout in every state.
        let mut child = app.projection.agents[0].clone();
        child.id = id.child(1);
        child.name = "Child".into();
        app.projection.agents.push(child);
        for light in [false, true] {
            app.light = light;
            let p = Palette::new(light);
            for (activity, terminal, status, role) in [
                (AgentActivity::Working, false, "Working", p.content.primary),
                (
                    AgentActivity::WaitingChildren,
                    false,
                    "Waiting for child",
                    p.content.warning,
                ),
                (
                    AgentActivity::Failed("fixture".into()),
                    false,
                    "Failed",
                    p.content.error,
                ),
                (AgentActivity::Interrupted, false, "Interrupted", p.muted),
                (AgentActivity::Idle, true, "Completed", p.content.success),
            ] {
                app.projection.agents[0].terminal = terminal;
                app.snapshot.activity.insert(id.clone(), activity);
                app.menu = None;
                let buffer = draw_sized_buffer(&mut app, 160, 36);
                let row = app
                    .hits
                    .iter()
                    .find_map(|(rect, hit)| {
                        matches!(hit, Hit::Agent(agent) if agent == &id).then_some(*rect)
                    })
                    .unwrap();
                assert_role(&buffer, row, "Researcher", p.content.fg);
                assert_role(&buffer, row, "@remote", p.content.accent);
                assert_role(&buffer, row, status, role);
                for width in [45, 160] {
                    app.command("agents");
                    let buffer = draw_sized_buffer(&mut app, width, 36);
                    let row = app
                        .hits
                        .iter()
                        .find_map(|(rect, hit)| matches!(hit, Hit::Menu(0)).then_some(*rect))
                        .unwrap();
                    assert_role(&buffer, row, "Researcher", p.content.fg);
                    assert_role(&buffer, row, "@remote", p.content.accent);
                    assert_role(&buffer, row, status, role);
                    assert_eq!(buffer[(row.x, row.y)].bg, p.selected);
                }
            }
            app.keys = KeyMap::new(&std::collections::BTreeMap::from([(
                "new".into(),
                "alt+n".into(),
            )]))
            .unwrap();
            app.command("commands");
            let buffer = draw_sized_buffer(&mut app, 90, 36);
            let row = app
                .hits
                .iter()
                .find_map(|(rect, hit)| matches!(hit, Hit::Menu(0)).then_some(*rect))
                .unwrap();
            assert_role(&buffer, buffer.area, "Commands", p.content.primary);
            assert_role(&buffer, row, "New session", p.content.primary);
            assert_role(&buffer, row, "Alt+N", p.muted);
            let hint: String = (row.right() - 5..row.right())
                .map(|x| buffer[(x, row.y)].symbol())
                .collect();
            assert_eq!(hint, "Alt+N");
            for x in row.x..row.right() {
                assert_eq!(
                    buffer[(x, row.y)].bg,
                    p.selected,
                    "selected palette row must have no background gap"
                );
            }
            let buffer = draw_sized_buffer(&mut app, 20, 36);
            let row = app
                .hits
                .iter()
                .find_map(|(rect, hit)| matches!(hit, Hit::Menu(0)).then_some(*rect))
                .unwrap();
            assert_role(&buffer, row, "New session", p.content.primary);
        }
    }

    fn draw(app: &mut App) -> String {
        draw_buffer(app)
            .content
            .chunks(60)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn mouse(app: &mut App, rect: Rect, kind: MouseEventKind) {
        app.event(Event::Mouse(crossterm::event::MouseEvent {
            kind,
            column: rect.x,
            row: rect.y,
            modifiers: M::NONE,
        }));
    }
    fn click(app: &mut App, rect: Rect) {
        mouse(app, rect, MouseEventKind::Down(MouseButton::Left));
        mouse(app, rect, MouseEventKind::Up(MouseButton::Left));
    }
    #[tokio::test]
    async fn palette_hover_owns_selection_without_background_or_stationary_updates() {
        let (_root, mut app) = fixture().await;
        app.open(
            "Models",
            MenuKind::Models,
            vec![
                Item::new("first", "First", ""),
                Item::new("second", "Second", ""),
                Item::new("third", "Third", ""),
            ],
        );
        draw(&mut app);
        let rows: Vec<_> = app
            .hits
            .iter()
            .filter_map(|(rect, hit)| {
                if let Hit::Menu(index) = hit {
                    Some((*rect, *index))
                } else {
                    None
                }
            })
            .collect();
        mouse(&mut app, rows[1].0, MouseEventKind::Moved);
        assert_eq!(app.menu.as_ref().unwrap().selected, 1);
        key(&mut app, KeyCode::Down, M::NONE);
        assert_eq!(app.menu.as_ref().unwrap().selected, 2);
        app.dirty = false;
        mouse(&mut app, rows[1].0, MouseEventKind::Moved);
        assert_eq!(app.menu.as_ref().unwrap().selected, 2);
        assert!(!app.dirty);
        // A physical move inside the same row can take over from the keyboard.
        let mut moved = rows[1].0;
        moved.x += 1;
        mouse(&mut app, moved, MouseEventKind::Moved);
        assert_eq!(app.menu.as_ref().unwrap().selected, 1);
        let selected_agent = app.selected.clone();
        app.dirty = false;
        let tree_rect = app.tree_rect;
        mouse(&mut app, tree_rect, MouseEventKind::Moved);
        assert_eq!(app.menu.as_ref().unwrap().selected, 1);
        assert_eq!(app.selected, selected_agent);
        assert!(!app.dirty);
        key(&mut app, KeyCode::Enter, M::NONE);
        assert!(app.menu.is_none());
        assert_eq!(app.model, "second");
        app.open(
            "Models",
            MenuKind::Models,
            vec![
                Item::new("hidden", "Hidden", ""),
                Item::new("first", "Visible first", ""),
                Item::new("second", "Visible second", ""),
            ],
        );
        app.menu.as_mut().unwrap().input.set("Visible".into());
        draw(&mut app);
        let row = app
            .hits
            .iter()
            .find_map(|(rect, hit)| matches!(hit, Hit::Menu(1)).then_some(*rect))
            .unwrap();
        mouse(&mut app, row, MouseEventKind::Moved);
        key(&mut app, KeyCode::Enter, M::NONE);
        assert_eq!(app.model, "second");
        // Selection-dependent theme previews follow hover just like keyboard input.
        let original_light = app.light;
        app.command("themes");
        draw(&mut app);
        let other = usize::from(!original_light);
        let mut row = app
            .hits
            .iter()
            .find_map(|(rect, hit)| {
                matches!(hit, Hit::Menu(index) if *index == other).then_some(*rect)
            })
            .unwrap();
        row.x += 1;
        mouse(&mut app, row, MouseEventKind::Moved);
        assert_eq!(app.light, !original_light);
        key(&mut app, KeyCode::Esc, M::NONE);
        assert_eq!(app.light, original_light);
        app.session.as_ref().unwrap().shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn palette_background_hover_is_not_rendered() {
        let (_root, mut app) = fixture().await;
        let mut child = app.projection.agents[0].clone();
        child.id = child.id.child(1);
        app.projection.agents.push(child.clone());
        app.open(
            "Models",
            MenuKind::Models,
            vec![Item::new("first", "First", "")],
        );
        let before = draw_buffer(&mut app);
        let row = app
            .hits
            .iter()
            .find_map(|(rect, hit)| {
                matches!(hit, Hit::Agent(id) if *id == child.id).then_some(*rect)
            })
            .unwrap();
        mouse(&mut app, row, MouseEventKind::Moved);
        let after = draw_buffer(&mut app);
        for x in row.x..row.right() {
            assert_eq!(before[(x, row.y)].bg, after[(x, row.y)].bg);
        }
        app.session.as_ref().unwrap().shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn agents_palette_headers_are_fixed_and_hits_cover_only_items() {
        let (_root, mut app) = fixture().await;
        for index in 1..20 {
            let mut child = app.projection.agents[0].clone();
            child.id = child.id.child(index);
            child.name = format!("worker {index}");
            app.projection.agents.push(child);
        }
        app.command("agents");
        for width in [120, 90, 60, 40] {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 40)).unwrap();
            terminal
                .draw(|frame| super::super::render::draw(frame, &mut app))
                .unwrap();
            let first_rows: Vec<_> = app
                .hits
                .iter()
                .filter_map(|(rect, hit)| {
                    if let Hit::Menu(index) = hit {
                        Some((*rect, *index))
                    } else {
                        None
                    }
                })
                .collect();
            assert!(!first_rows.is_empty(), "width {width}");
            let first_y = first_rows[0].0.y;
            assert!(first_y > app.content_rect.y + 2);
            let header = terminal.backend().buffer().clone();
            key(&mut app, KeyCode::End, M::NONE);
            terminal
                .draw(|frame| super::super::render::draw(frame, &mut app))
                .unwrap();
            let rows: Vec<_> = app
                .hits
                .iter()
                .filter_map(|(rect, hit)| {
                    if let Hit::Menu(index) = hit {
                        Some((*rect, *index))
                    } else {
                        None
                    }
                })
                .collect();
            assert_eq!(rows.last().unwrap().1, 19);
            assert_eq!(rows[0].0.y, first_y);
            for y in app.content_rect.y + 2..first_y {
                for x in 0..width {
                    assert_eq!(header[(x, y)], terminal.backend().buffer()[(x, y)]);
                }
            }
            let mut header_point = rows[0].0;
            header_point.y = first_y - 1;
            mouse(&mut app, header_point, MouseEventKind::Moved);
            assert_eq!(app.menu.as_ref().unwrap().selected, 19);
            // Every line of a stacked item resolves to its filtered item index.
            let (row, index) = rows[0];
            let mut bottom = row;
            bottom.y = row.bottom() - 1;
            mouse(&mut app, bottom, MouseEventKind::Moved);
            assert_eq!(app.menu.as_ref().unwrap().selected, index);
            key(&mut app, KeyCode::Home, M::NONE);
        }
        app.session.as_ref().unwrap().shutdown().await.unwrap();
    }

    fn tool_hits(app: &App) -> Vec<Rect> {
        app.hits
            .iter()
            .filter_map(|(rect, hit)| matches!(hit, Hit::Entry(_, true)).then_some(*rect))
            .collect()
    }

    /// Full frame timings include Ratatui buffer diffing but no terminal I/O.
    /// Run optimized, with one benchmark thread, to compare history-independent work.
    #[tokio::test]
    #[ignore = "manual optimized UI latency benchmark"]
    async fn incremental_rendering_benchmark() {
        use skyhook::{
            provider::protocol::{AssistantItem, Message},
            session::EventRecord,
        };
        for count in [100, 10_000] {
            let (_root, mut app) = fixture().await;
            let first = app.snapshot.records.last_key_value().unwrap().0 + 1;
            for offset in 0..count {
                let sequence = first + offset;
                app.snapshot.records.insert(sequence, EventRecord {
                    version: 1, sequence, timestamp_millis: 0, agent: app.selected.clone(),
                    event: SessionEvent::MessageCommitted { message: Message::Assistant(vec![AssistantItem::text(format!("message-{offset}"), 0,
                        format!("Message {offset}: **important** detail with `code` and ordinary text."))]) },
                });
            }
            app.refresh();
            for (width, height) in [(120, 40), (240, 80)] {
                let mut terminal =
                    ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
                        .unwrap();
                terminal
                    .draw(|frame| super::super::render::draw(frame, &mut app))
                    .unwrap();
                let mut elapsed = Vec::new();
                for _ in 0..100 {
                    let start = Instant::now();
                    terminal
                        .draw(|frame| super::super::render::draw(frame, &mut app))
                        .unwrap();
                    elapsed.push(start.elapsed());
                }
                elapsed.sort();
                eprintln!(
                    "history={count} screen={width}x{height} warm-frame median={:?} p95={:?}",
                    elapsed[50], elapsed[95]
                );
                for (reasoning, markdown) in [(false, false), (true, false), (true, true)] {
                    let request = first
                        + count
                        + u64::from(width) * 10
                        + u64::from(reasoning)
                        + u64::from(markdown);
                    let body = if markdown {
                        "# Heading\n\nA **stable** reasoning paragraph with `code`.\n\n```rust\nlet n = 1;\n```\n\n".repeat(14_000)
                    } else {
                        "A stable reasoning paragraph with ordinary text.\n\n".repeat(22_000)
                    };
                    let agent = app.selected.clone();
                    let stream = TestBlock::start(
                        &mut app,
                        &agent,
                        request,
                        "benchmark",
                        0,
                        if reasoning {
                            BlockKind::Reasoning
                        } else {
                            BlockKind::Text
                        },
                    );
                    app.thinking = true;
                    app.invalidate_content();
                    stream.delta(&mut app, &body);
                    terminal
                        .draw(|frame| super::super::render::draw(frame, &mut app))
                        .unwrap();
                    let mut elapsed = Vec::new();
                    for _ in 0..100 {
                        let start = Instant::now();
                        stream.delta(&mut app, "next word ");
                        terminal
                            .draw(|frame| super::super::render::draw(frame, &mut app))
                            .unwrap();
                        elapsed.push(start.elapsed());
                    }
                    elapsed.sort();
                    eprintln!(
                        "history={count} screen={width}x{height} reasoning={reasoning} markdown={markdown} 1MiB-append-frame median={:?} p95={:?}",
                        elapsed[50], elapsed[95]
                    );
                }
            }
            app.session.as_ref().unwrap().shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn file_preview_uses_source_fields_and_can_continue_truncated_output() {
        let (_root, mut app) = fixture().await;
        let source = (0..100)
            .map(|index| format!("// original source line {index:03}\n"))
            .collect::<String>();
        std::fs::write(app.launch.workspace.join("example.rs"), &source).unwrap();
        app.session
            .as_ref()
            .unwrap()
            .run_script("return await tool.read({path:'example.rs'});")
            .await
            .unwrap();
        app.snapshot = app.session.as_ref().unwrap().observe().await.snapshot;
        app.refresh();
        let job = app
            .projection
            .jobs
            .values()
            .find(|job| job.tool == "read")
            .unwrap()
            .id;
        app.fetch_output(job);
        let query = app.output_queries[&job].clone();
        assert!(query.field.is_none());
        let output = app
            .session
            .as_ref()
            .unwrap()
            .inspect_output(query)
            .await
            .unwrap();
        let prefix = output["result"]["content"].as_str().unwrap();
        assert!(source.starts_with(prefix));
        let position = output["truncated"][0].clone();
        app.outputs.insert(job, output);
        app.pending_outputs.clear();
        app.command("details");
        draw(&mut app);
        app.view().row = app
            .entries
            .iter()
            .position(|entry| entry.job == Some(job))
            .unwrap();
        app.output_menu();
        let menu = app.menu.as_mut().unwrap();
        menu.selected = menu
            .items
            .iter()
            .position(|item| item.value == "next")
            .unwrap();
        app.choose();
        let query = app.output_queries[&job].clone();
        assert_eq!(
            query.start,
            Some(position["next_start"].as_u64().unwrap() as usize)
        );
        assert_eq!(
            query.offset,
            position["next_offset"]
                .as_u64()
                .map(|offset| offset as usize)
        );
        let page = app
            .session
            .as_ref()
            .unwrap()
            .inspect_output(query)
            .await
            .unwrap();
        assert_eq!(page["preview"]["field"], "/result/content");
        assert!(
            page["preview"]["lines"]
                .as_array()
                .unwrap()
                .iter()
                .any(|line| line.as_str().unwrap().contains("source line 099"))
        );
        assert_eq!(
            std::fs::read_to_string(app.launch.workspace.join("example.rs")).unwrap(),
            source
        );
        app.session.as_ref().unwrap().shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn markdown_continuations_keep_task_list_quote_and_ordered_indentation() {
        use skyhook::{
            provider::protocol::{AssistantItem, Message},
            session::EventRecord,
        };
        let (_root, mut app) = fixture().await;
        let body = "- [ ] alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo lima\n\n> quoteone quotetwo quotethree quotefour quotefive quotesix quoteseven\n\n10. orderone ordertwo orderthree orderfour orderfive ordersix\n\n> - nestedone nestedtwo nestedthree nestedfour nestedfive nestedsix";
        let sequence = app.snapshot.records.last_key_value().unwrap().0 + 1;
        app.snapshot.records.insert(
            sequence,
            EventRecord {
                version: 1,
                sequence,
                timestamp_millis: 0,
                agent: app.selected.clone(),
                event: SessionEvent::MessageCommitted {
                    message: Message::Assistant(vec![AssistantItem::text("indent", 0, body)]),
                },
            },
        );
        let records = serde_json::to_vec(&app.snapshot.records).unwrap();
        app.refresh();
        for light in [false, true] {
            app.light = light;
            for width in [30, 44] {
                let buffer = draw_sized_buffer(&mut app, width, 60);
                for word in ["alpha", "quoteone", "orderone", "nestedone"] {
                    let (x, y) = (0..buffer.area.height)
                        .find_map(|y| {
                            (0..width.saturating_sub(word.len() as u16)).find_map(|x| {
                                word.chars()
                                    .enumerate()
                                    .all(|(offset, c)| {
                                        buffer[(x + offset as u16, y)].symbol() == c.to_string()
                                    })
                                    .then_some((x, y))
                            })
                        })
                        .unwrap_or_else(|| panic!("missing {word}"));
                    let continued_x = (0..width)
                        .find(|&x| buffer[(x, y + 1)].symbol().chars().any(char::is_alphabetic))
                        .unwrap();
                    assert_eq!(continued_x, x, "{word}, width={width}, light={light}");
                    if word == "quoteone" {
                        assert_eq!(buffer[(x - 2, y + 1)].symbol(), "│");
                    }
                }
            }
        }
        assert_eq!(serde_json::to_vec(&app.snapshot.records).unwrap(), records);
        app.session.as_ref().unwrap().shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn markdown_code_keeps_stock_syntax_and_padded_backgrounds() {
        use super::super::{
            render::Palette,
            theme::ContentTheme,
            tool_view::{CodeSource, Document, Role, Section},
        };
        use skyhook::{
            provider::protocol::{AssistantItem, Message},
            session::EventRecord,
        };
        let (_root, mut app) = fixture().await;
        let source = "const answer = 42;\n\nawait performWork(answer);\n";
        let body = format!("Outside prose\n\n```js\n{source}```\n\nMore prose with `inline`.");
        let sequence = app.snapshot.records.last_key_value().unwrap().0 + 1;
        app.snapshot.records.insert(
            sequence,
            EventRecord {
                version: 1,
                sequence,
                timestamp_millis: 0,
                agent: app.selected.clone(),
                event: SessionEvent::MessageCommitted {
                    message: Message::Assistant(vec![AssistantItem::text("code", 0, &body)]),
                },
            },
        );
        let records = serde_json::to_vec(&app.snapshot.records).unwrap();
        app.refresh();
        let document = Document {
            sections: vec![Section::Code {
                source: CodeSource::from(source),
                language: "js".into(),
                indent: 0,
                gutters: vec![],
                role: Role::Constant,
            }],
        };
        let locate = |buffer: &ratatui::buffer::Buffer, word: &str| {
            let area = buffer.area;
            (area.y..area.bottom())
                .find_map(|y| {
                    (area.x
                        ..area
                            .right()
                            .saturating_sub(word.len() as u16)
                            .saturating_add(1))
                        .find_map(|x| {
                            word.chars()
                                .enumerate()
                                .all(|(offset, c)| {
                                    buffer[(x + offset as u16, y)].symbol() == c.to_string()
                                })
                                .then_some((x, y))
                        })
                })
                .unwrap_or_else(|| panic!("missing {word}"))
        };
        for light in [false, true] {
            app.light = light;
            for width in [36, 64] {
                let deadline = Instant::now() + Duration::from_secs(5);
                let buffer = loop {
                    let buffer = draw_sized_buffer(&mut app, width, 40);
                    if app.render.highlights.is_fully_highlighted(&document, light) {
                        break buffer;
                    }
                    assert!(Instant::now() < deadline);
                    tokio::time::sleep(Duration::from_millis(10)).await;
                };
                let theme = ContentTheme::new(light);
                for (word, foreground) in
                    [("const", theme.secondary), ("performWork", theme.secondary)]
                {
                    let (x, y) = locate(&buffer, word);
                    for offset in 0..word.len() as u16 {
                        assert_eq!(
                            buffer[(x + offset, y)].fg,
                            foreground,
                            "{word}, light={light}"
                        );
                        assert_eq!(buffer[(x + offset, y)].bg, theme.code_bg);
                    }
                }
                let (code_x, y) = locate(&buffer, "const");
                let (paragraph_x, paragraph_y) = locate(&buffer, "Outside");
                let rect = app
                    .hits
                    .iter()
                    .find_map(|(rect, hit)| {
                        (rect.y == paragraph_y && matches!(hit, Hit::Entry(_, _))).then_some(*rect)
                    })
                    .unwrap();
                let available = rect.width.saturating_sub(4);
                let block_width =
                    (source.lines().map(str::len).max().unwrap() as u16 + 2).min(available);
                assert_eq!(
                    code_x,
                    paragraph_x + 1,
                    "code starts after one padding column"
                );
                let (_, last_y) = locate(&buffer, "await");
                for row in [y - 1, y, y + 1, last_y, last_y + 1] {
                    for x in paragraph_x..paragraph_x + block_width {
                        assert_eq!(buffer[(x, row)].bg, theme.code_bg, "code padding/blank row");
                    }
                    if paragraph_x + block_width < width {
                        assert_eq!(
                            buffer[(paragraph_x + block_width, row)].bg,
                            Palette::new(light).agent,
                            "code background stops at its content-sized right edge"
                        );
                    }
                }
                for word in ["Outside", "inline"] {
                    let point = locate(&buffer, word);
                    assert_eq!(buffer[point].bg, Palette::new(light).agent, "{word}");
                }
            }
        }
        assert_eq!(serde_json::to_vec(&app.snapshot.records).unwrap(), records);
        app.session.as_ref().unwrap().shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn expanded_agent_prompts_wrap_on_words_at_multiple_widths() {
        use super::super::theme::ContentTheme;
        use skyhook::{
            provider::protocol::{AssistantItem, Message, ToolCall},
            session::EventRecord,
        };
        let (_root, mut app) = fixture().await;
        let words = "alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mango nectar orange papaya quince rocket sierra tango uniform victor whiskey xray yankee zulu"
            .split_whitespace().collect::<Vec<_>>();
        let prompt = format!(
            "{}\n{}",
            words.join(" "),
            words.iter().rev().copied().collect::<Vec<_>>().join(" ")
        );
        let sequence = app.snapshot.records.last_key_value().unwrap().0 + 1;
        app.snapshot.records.insert(
            sequence,
            EventRecord {
                version: 1,
                sequence,
                timestamp_millis: 0,
                agent: app.selected.clone(),
                event: SessionEvent::MessageCommitted {
                    message: Message::Assistant(vec![AssistantItem::tool_call(
                        "prompt-item",
                        0,
                        ToolCall {
                            id: "prompt-call".into(),
                            name: "agent".into(),
                            arguments: serde_json::json!({"prompt": prompt}),
                        },
                    )]),
                },
            },
        );
        let records = serde_json::to_vec(&app.snapshot.records).unwrap();
        app.refresh();
        draw_sized_buffer(&mut app, 64, 60);
        let index = app
            .entries
            .iter()
            .position(|entry| entry.surface == model::Surface::Tool)
            .unwrap();
        app.view().row = index;
        app.toggle();
        for light in [false, true] {
            app.light = light;
            for width in [24, 40, 64] {
                let buffer = draw_sized_buffer(&mut app, width, 60);
                let mut displayed = Vec::new();
                for (rect, hit) in &app.hits {
                    if !matches!(hit, Hit::Entry(i, true) if *i == index) {
                        continue;
                    }
                    let text = (rect.x..rect.right())
                        .filter_map(|x| {
                            let cell = &buffer[(x, rect.y)];
                            (cell.fg == ContentTheme::new(light).success).then_some(cell.symbol())
                        })
                        .collect::<String>();
                    // Every displayed fragment must contain whole prompt words,
                    // not the prefix/suffix of a word cut at the terminal edge.
                    displayed.extend(text.split_whitespace().map(str::to_owned));
                }
                let expected = words
                    .iter()
                    .chain(words.iter().rev())
                    .map(|word| (*word).to_owned())
                    .collect::<Vec<_>>();
                assert_eq!(displayed, expected, "light={light}, width={width}");
            }
        }
        assert_eq!(serde_json::to_vec(&app.snapshot.records).unwrap(), records);
        app.session.as_ref().unwrap().shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn expanded_edges_persist_without_body_focus_or_hover_backgrounds() {
        use super::super::render::Palette;
        use skyhook::{
            provider::protocol::{AssistantItem, Message, ToolCall, ToolResult},
            session::EventRecord,
        };
        let (_root, mut app) = fixture().await;
        let sequence = app.snapshot.records.last_key_value().unwrap().0 + 1;
        let calls = ["first", "second"].map(|id| ToolCall {
            id: id.into(),
            name: "read".into(),
            arguments: serde_json::json!({"path": format!("{id}.txt")}),
        });
        app.snapshot.records.insert(
            sequence,
            EventRecord {
                version: 1,
                sequence,
                timestamp_millis: 0,
                agent: app.selected.clone(),
                event: SessionEvent::MessageCommitted {
                    message: Message::Assistant(
                        calls
                            .iter()
                            .enumerate()
                            .map(|(index, call)| {
                                AssistantItem::tool_call(call.id.clone(), index, call.clone())
                            })
                            .collect(),
                    ),
                },
            },
        );
        app.snapshot.records.insert(sequence + 1, EventRecord {
            version: 1, sequence: sequence + 1, timestamp_millis: 1, agent: app.selected.clone(),
            event: SessionEvent::MessageCommitted { message: Message::Tool(calls.iter().map(|call| ToolResult {
                call_id: call.id.clone(), name: call.name.clone(),
                result: serde_json::json!({"result": {"content": format!("{} body", call.id)}}),
                images: vec![], is_error: false,
            }).collect()) },
        });
        app.refresh();
        draw_sized_buffer(&mut app, 80, 60);
        let keys = app
            .entries
            .iter()
            .filter(|entry| entry.surface == model::Surface::Tool)
            .map(|entry| entry.key.clone())
            .collect::<Vec<_>>();
        assert_eq!(keys.len(), 2);
        let rects = |app: &App, key: &str| {
            let index = app
                .entries
                .iter()
                .position(|entry| entry.key == key)
                .unwrap();
            app.hits
                .iter()
                .filter_map(|(rect, hit)| {
                    matches!(hit, Hit::Entry(i, true) if *i == index).then_some(*rect)
                })
                .collect::<Vec<_>>()
        };
        let check = |app: &App, buffer: &ratatui::buffer::Buffer, key: &str| {
            let rows = rects(app, key);
            assert!(rows.len() > 2);
            let p = Palette::new(app.light);
            let arrow_x = rows[0].x;
            for (index, rect) in rows.iter().enumerate() {
                assert_eq!(
                    buffer[(arrow_x, rect.y)].bg,
                    p.selected,
                    "connected tool gutter"
                );
                for x in rect.x..rect.right() {
                    let expected = if index == 0 || index + 1 == rows.len() || x == arrow_x {
                        p.selected
                    } else {
                        p.base
                    };
                    assert_eq!(buffer[(x, rect.y)].bg, expected, "{key} row {index}, x={x}");
                }
            }
        };
        for light in [false, true] {
            app.light = light;
            app.details = false;
            app.view().expanded.clear();
            app.view().collapsed.clear();
            app.invalidate_content();
            draw_sized_buffer(&mut app, 80, 60);
            app.focus = Focus::Content;
            app.view().row = app
                .entries
                .iter()
                .position(|entry| entry.key == keys[0])
                .unwrap();
            app.toggle();
            let buffer = draw_sized_buffer(&mut app, 80, 60);
            check(&app, &buffer, &keys[0]);
            let body = rects(&app, &keys[0])[1];
            mouse(&mut app, body, MouseEventKind::Moved);
            let buffer = draw_sized_buffer(&mut app, 80, 60);
            check(&app, &buffer, &keys[0]);
            let composer = app.composer_rect;
            click(&mut app, composer);
            let buffer = draw_sized_buffer(&mut app, 80, 60);
            check(&app, &buffer, &keys[0]);
            app.view().row = app
                .entries
                .iter()
                .position(|entry| entry.key == keys[1])
                .unwrap();
            app.toggle();
            let buffer = draw_sized_buffer(&mut app, 80, 60);
            check(&app, &buffer, &keys[0]);
            check(&app, &buffer, &keys[1]);
            let first = rects(&app, &keys[0])[0];
            click(&mut app, first);
            draw_sized_buffer(&mut app, 80, 60);
            let composer = app.composer_rect;
            click(&mut app, composer);
            app.hover = None;
            let buffer = draw_sized_buffer(&mut app, 80, 60);
            let first = rects(&app, &keys[0]);
            assert_eq!(first.len(), 1);
            assert_eq!(
                buffer[(first[0].x, first[0].y)].bg,
                Palette::new(light).base
            );
            check(&app, &buffer, &keys[1]);
        }
        app.session.as_ref().unwrap().shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn latest_activity_is_text_only_and_still_jumps_to_the_end() {
        use super::super::render::Palette;
        let (_root, mut app) = fixture().await;
        app.entries = vec![model::Entry {
            key: "long-message".into(),
            text: format!("Agent\n{}", "ordinary prose\n".repeat(100)),
            surface: model::Surface::Agent,
            expandable: false,
            default_open: false,
            running: false,
            footer: None,
            request: None,
            indent: 0,
            job: None,
            header: None,
            document: None,
            compact_after: false,
        }];
        app.content_dirty = false;
        for light in [false, true] {
            app.light = light;
            app.view().scroll = Some(0);
            let buffer = draw_buffer(&mut app);
            let popup = app
                .hits
                .iter()
                .find_map(|(rect, hit)| matches!(hit, Hit::Latest).then_some(*rect))
                .unwrap();
            let message = app
                .hits
                .iter()
                .find_map(|(rect, hit)| {
                    (rect.y == popup.y && matches!(hit, Hit::Entry(0, _))).then_some(*rect)
                })
                .unwrap();
            let p = Palette::new(light);
            let mut label = String::new();
            for x in popup.x..popup.right() {
                let expected = if message.contains((x, popup.y).into()) {
                    p.agent
                } else {
                    p.base
                };
                assert_eq!(buffer[(x, popup.y)].bg, expected);
                label.push_str(buffer[(x, popup.y)].symbol());
            }
            assert_eq!(label, "↓ Latest activity");
            click(&mut app, popup);
            draw_buffer(&mut app);
            assert!(app.view().scroll.is_none());
            assert!(!app.hits.iter().any(|(_, hit)| matches!(hit, Hit::Latest)));
        }
        app.session.as_ref().unwrap().shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn pre_job_failure_updates_the_existing_tool_card_and_expands_in_place() {
        use super::super::theme::ContentTheme;
        use skyhook::{
            provider::protocol::{AssistantItem, Message, ToolCall, ToolResult},
            session::EventRecord,
        };
        let (_root, mut app) = fixture().await;
        let sequence = app.snapshot.records.last_key_value().unwrap().0 + 1;
        app.snapshot.records.insert(
            sequence,
            EventRecord {
                version: 1,
                sequence,
                timestamp_millis: 0,
                agent: app.selected.clone(),
                event: SessionEvent::MessageCommitted {
                    message: Message::Assistant(vec![AssistantItem::tool_call(
                        "call-item",
                        0,
                        ToolCall {
                            id: "denied-call".into(),
                            name: "exec".into(),
                            arguments: serde_json::json!({"argv": ["cargo", "test"]}),
                        },
                    )]),
                },
            },
        );
        app.refresh();
        draw(&mut app);
        let key = app
            .entries
            .iter()
            .find(|entry| entry.surface == model::Surface::Tool)
            .unwrap()
            .key
            .clone();
        app.snapshot.records.insert(sequence + 1, EventRecord {
            version: 1, sequence: sequence + 1, timestamp_millis: 1, agent: app.selected.clone(),
            event: SessionEvent::MessageCommitted { message: Message::Tool(vec![ToolResult {
                call_id: "denied-call".into(), name: "exec".into(),
                result: serde_json::json!({"error": "Permission was denied", "code": "permission_denied", "executed": false}),
                images: vec![], is_error: true,
            }]) },
        });
        let records = serde_json::to_vec(&app.snapshot.records).unwrap();
        app.refresh();
        for light in [false, true] {
            app.light = light;
            let buffer = draw_buffer(&mut app);
            let cards = app
                .entries
                .iter()
                .filter(|entry| entry.surface == model::Surface::Tool)
                .collect::<Vec<_>>();
            assert_eq!(cards.len(), 1);
            assert_eq!(cards[0].key, key);
            assert!(cards[0].text.contains("Failed"));
            assert!(!cards[0].text.contains("Permission was denied"));
            assert!(cards[0].job.is_none());
            assert!(cards[0].expandable);
            assert!(buffer.content.windows(6).any(|cells| {
                cells.iter().map(|cell| cell.symbol()).collect::<String>() == "Failed"
                    && cells
                        .iter()
                        .all(|cell| cell.fg == ContentTheme::new(light).error)
            }));
            let index = app
                .entries
                .iter()
                .position(|entry| entry.key == key)
                .unwrap();
            let hit = app
                .hits
                .iter()
                .find_map(|(rect, hit)| {
                    matches!(hit, Hit::Entry(i, true) if *i == index).then_some(*rect)
                })
                .unwrap();
            click(&mut app, hit);
            draw(&mut app);
            let card = app.entries.iter().find(|entry| entry.key == key).unwrap();
            assert!(card.text.contains("Arguments"));
            assert!(card.text.contains("Output"));
            assert!(card.text.contains("Permission was denied"));
            assert!(card.text.contains("permission_denied"));
            assert!(card.document.is_some());
            assert_eq!(
                app.entries
                    .iter()
                    .filter(|entry| entry.surface == model::Surface::Tool)
                    .count(),
                1
            );
            let hit = app
                .hits
                .iter()
                .find_map(|(rect, hit)| {
                    matches!(hit, Hit::Entry(i, true) if *i == index).then_some(*rect)
                })
                .unwrap();
            click(&mut app, hit);
            draw(&mut app);
        }
        assert_eq!(serde_json::to_vec(&app.snapshot.records).unwrap(), records);
        app.session.as_ref().unwrap().shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn ordered_list_fences_highlight_without_parsing_generated_message_titles() {
        use super::super::{
            theme::ContentTheme,
            tool_view::{CodeSource, Document, Role, Section},
        };
        use skyhook::{
            provider::protocol::{AssistantItem, Message, UserContent},
            session::EventRecord,
        };
        // A fresh cache for every case is important: another entry's identical
        // highlighted source must not mask a missing metadata request.
        for user in [false, true] {
            for start in [2, 3] {
                let (_root, mut app) = fixture().await;
                let source = "let answer = 42;\n";
                let body = format!("{start}. ```rust\n   {source}   ```");
                let message = if user {
                    Message::User(vec![UserContent::Text { text: body.clone() }])
                } else {
                    Message::Assistant(vec![AssistantItem::text("ordered-fence", 0, &body)])
                };
                let sequence = app.snapshot.records.last_key_value().unwrap().0 + 1;
                app.snapshot.records.insert(
                    sequence,
                    EventRecord {
                        version: 1,
                        sequence,
                        timestamp_millis: 0,
                        agent: app.selected.clone(),
                        event: SessionEvent::MessageCommitted { message },
                    },
                );
                app.refresh();
                let document = Document {
                    sections: vec![Section::Code {
                        source: CodeSource::from(source),
                        language: "rust".into(),
                        indent: 0,
                        gutters: vec![],
                        role: Role::Constant,
                    }],
                };
                for light in [false, true] {
                    app.light = light;
                    let deadline = Instant::now() + Duration::from_secs(5);
                    loop {
                        let buffer = draw_sized_buffer(&mut app, 60, 40);
                        let painted = buffer.content.windows(2).any(|cells| {
                            cells[0].symbol() == "4"
                                && cells[1].symbol() == "2"
                                && cells
                                    .iter()
                                    .all(|cell| cell.fg == ContentTheme::new(light).accent)
                        });
                        if app.render.highlights.is_fully_highlighted(&document, light) && painted {
                            break;
                        }
                        assert!(
                            Instant::now() < deadline,
                            "missing ordered fence: start={start}, user={user}, light={light}"
                        );
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }
                app.session.as_ref().unwrap().shutdown().await.unwrap();
            }
        }
    }

    #[tokio::test]
    async fn markdown_fence_highlights_arrive_after_reset_resize_and_theme_switch() {
        use super::super::{
            theme::ContentTheme,
            tool_view::{CodeSource, Document, Role, Section},
        };
        use skyhook::{
            provider::protocol::{AssistantItem, Message},
            session::EventRecord,
        };
        let (_root, mut app) = fixture().await;
        let source = "let answer = 42;\n";
        let message = format!("# Example\n\n```rust\n{source}```\n\n**Done**");
        let sequence = app.snapshot.records.last_key_value().unwrap().0 + 1;
        app.snapshot.records.insert(
            sequence,
            EventRecord {
                version: 1,
                sequence,
                timestamp_millis: 0,
                agent: app.selected.clone(),
                event: SessionEvent::MessageCommitted {
                    message: Message::Assistant(vec![AssistantItem::text("fence", 0, &message)]),
                },
            },
        );
        app.refresh();
        let document = Document {
            sections: vec![Section::Code {
                source: CodeSource::from(source),
                language: "rust".into(),
                indent: 0,
                gutters: vec![],
                role: Role::Constant,
            }],
        };
        let records = serde_json::to_vec(&app.snapshot.records).unwrap();
        for (light, width) in [(false, 60), (true, 90), (false, 40)] {
            app.light = light;
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 30)).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                terminal
                    .draw(|frame| super::super::render::draw(frame, &mut app))
                    .unwrap();
                // Inspect painted cells, not just cache readiness: this catches
                // missing source-ID invalidation after an initial reset.
                let painted = terminal.backend().buffer().content.windows(2).any(|cells| {
                    cells[0].symbol() == "4"
                        && cells[1].symbol() == "2"
                        && cells
                            .iter()
                            .all(|cell| cell.fg == ContentTheme::new(light).accent)
                });
                if app.render.highlights.is_fully_highlighted(&document, light) && painted {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "fence did not recolour: light={light}, width={width}"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert!(
                app.entries
                    .iter()
                    .any(|entry| entry.text.ends_with(&message))
            );
            assert_eq!(serde_json::to_vec(&app.snapshot.records).unwrap(), records);
        }
        app.session.as_ref().unwrap().shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn highlighting_real_job_survives_resize_and_preserves_session_bytes() {
        let (_root, mut app) = fixture().await;
        let source = "const value = {answer: 42};  \n\treturn value;\n";
        app.session
            .as_ref()
            .unwrap()
            .run_script(source.to_owned())
            .await
            .unwrap();
        app.snapshot = app.session.as_ref().unwrap().observe().await.snapshot;
        app.refresh();
        let job = app
            .projection
            .jobs
            .values()
            .find(|job| job.tool == "script")
            .unwrap()
            .id;
        let output = app
            .session
            .as_ref()
            .unwrap()
            .inspect_output(JobOutputQuery::new(job))
            .await
            .unwrap();
        app.outputs.insert(job, output.clone());
        let journal = app
            .launch
            .sessions
            .join(app.session.as_ref().unwrap().id().to_string())
            .join("events.jsonl");
        let before = std::fs::read(&journal).unwrap();
        let records = serde_json::to_vec(&app.snapshot.records).unwrap();
        let mut arguments = super::super::tool_view::Document::default();
        arguments.arguments("script", &app.projection.jobs[&job].args);
        app.view().scroll = Some(0);
        draw(&mut app);
        assert!(app.entries.iter().all(|entry| entry.document.is_none()));
        assert!(!app.render.highlights.is_highlighted(&arguments, app.light));
        let header = tool_hits(&app)[0];
        click(&mut app, header);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            draw(&mut app);
            if app.render.highlights.is_highlighted(&arguments, app.light) {
                break;
            }
            assert!(Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let buffer = draw_buffer(&mut app);
        let source_row = buffer
            .content
            .chunks(60)
            .find(|row| {
                row.iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>()
                    .contains("const value")
            })
            .unwrap();
        assert!(
            source_row
                .iter()
                .filter(|cell| cell.symbol() != " ")
                .map(|cell| cell.fg)
                .collect::<HashSet<_>>()
                .len()
                >= 3
        );
        app.view().scroll = Some(0);
        let text = draw(&mut app);
        assert!(!text.contains("@root"), "{text}");
        assert!(text.contains("const value = {answer: 42};"), "{text}");
        let document = app
            .entries
            .iter()
            .find_map(|entry| entry.document.as_ref())
            .unwrap()
            .clone();
        // The arguments can finish before the output's separate code sections.
        // Capture the fully highlighted document, not a timing-dependent mix.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            draw(&mut app);
            if app
                .render
                .highlights
                .is_fully_highlighted(&document, app.light)
            {
                break;
            }
            assert!(Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let highlighted_document = document.lines(Some(&app.render.highlights), app.light);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(90, 30)).unwrap();
        terminal
            .draw(|frame| super::super::render::draw(frame, &mut app))
            .unwrap();
        assert_eq!(
            document.lines(Some(&app.render.highlights), app.light),
            highlighted_document
        );
        // Theme previews recolour cached code without changing its source or
        // document geometry, and cancelling restores the exact original spans.
        let original_light = app.light;
        let plain_document = document.plain_text();
        app.command("themes");
        app.menu.as_mut().unwrap().selected = usize::from(!original_light);
        app.preview_theme();
        assert_eq!(app.light, !original_light);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            draw(&mut app);
            if app
                .render
                .highlights
                .is_fully_highlighted(&document, app.light)
            {
                break;
            }
            assert!(Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let preview_document = document.lines(Some(&app.render.highlights), app.light);
        assert_ne!(preview_document, highlighted_document);
        assert_eq!(preview_document.len(), highlighted_document.len());
        assert_eq!(document.plain_text(), plain_document);
        key(&mut app, KeyCode::Esc, M::NONE);
        draw(&mut app);
        assert_eq!(app.light, original_light);
        assert_eq!(
            document.lines(Some(&app.render.highlights), app.light),
            highlighted_document
        );
        let body = tool_hits(&app)[2];
        click(&mut app, body);
        draw(&mut app);
        assert!(app.entries.iter().all(|entry| entry.document.is_none()));
        assert_eq!(app.outputs[&job], output);
        assert_eq!(serde_json::to_vec(&app.snapshot.records).unwrap(), records);
        assert_eq!(std::fs::read(journal).unwrap(), before);
        app.session.as_ref().unwrap().shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn tab_focus_marks_visible_messages_and_tracks_tree_navigation() {
        let (_root, mut app) = fixture().await;
        app.entries = (0..12)
            .map(|index| model::Entry {
                key: format!("message{index}"),
                text: format!("skyhook\nMessage {index}"),
                surface: model::Surface::Agent,
                expandable: false,
                default_open: false,
                running: false,
                footer: None,
                request: None,
                indent: 0,
                job: None,
                compact_after: false,
                header: None,
                document: None,
            })
            .collect();
        app.content_dirty = false;
        let buffer = draw_buffer(&mut app);
        assert_eq!(buffer[(0, 3)].bg, ratatui::style::Color::Rgb(0, 0, 0));
        assert!(!buffer.content.iter().any(|cell| cell.symbol() == "▌"));
        assert_eq!(app.view().row, 0);
        key(&mut app, KeyCode::Tab, M::NONE);
        let buffer = draw_buffer(&mut app);
        assert!(app.view().row > 0, "Tab should select a visible message");
        let markers: Vec<_> = buffer
            .content
            .iter()
            .filter(|cell| cell.symbol() == "▌")
            .collect();
        assert_eq!(markers.len(), 1);
        assert_eq!(markers[0].fg, ratatui::style::Color::Rgb(255, 255, 255));
        key(&mut app, KeyCode::Tab, M::NONE);
        assert!(!draw(&mut app).contains('▌'));

        let mut child = app.projection.agents[0].clone();
        child.id = child.id.child(1);
        app.projection.agents.push(child);
        draw(&mut app);
        key(&mut app, KeyCode::Tab, M::NONE);
        let buffer = draw_buffer(&mut app);
        let tree_y = app.tree_rect.y;
        assert_eq!(buffer[(2, tree_y + 1)].symbol(), "▌");
        key(&mut app, KeyCode::Down, M::NONE);
        let buffer = draw_buffer(&mut app);
        assert_ne!(buffer[(2, tree_y + 1)].symbol(), "▌");
        assert_eq!(buffer[(6, tree_y + 2)].symbol(), "▌");
        app.command("commands");
        assert!(
            app.menu
                .as_ref()
                .unwrap()
                .items
                .iter()
                .all(|item| !matches!(
                    item.value.as_str(),
                    "commands" | "inspect" | "child" | "parent" | "diagnostics"
                ))
        );
        let buffer = draw_buffer(&mut app);
        assert_ne!(buffer[(6, tree_y + 2)].symbol(), "▌");
        assert_eq!(
            buffer
                .content
                .iter()
                .filter(|cell| cell.symbol() == "▌")
                .count(),
            1
        );
        key(&mut app, KeyCode::Esc, M::NONE);
        key(&mut app, KeyCode::Char('x'), M::CONTROL);
        key(&mut app, KeyCode::Up, M::NONE);
        assert_eq!(app.selected, app.projection.agents[0].id);
        key(&mut app, KeyCode::Char('x'), M::CONTROL);
        key(&mut app, KeyCode::Down, M::NONE);
        assert_eq!(app.selected, app.projection.agents[1].id);
        key(&mut app, KeyCode::Char('x'), M::CONTROL);
        key(&mut app, KeyCode::Char('i'), M::NONE);
        assert!(matches!(app.focus, Focus::Content));
        assert_eq!(app.view().tab, Tab::Conversation);
        app.session.as_ref().unwrap().shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn message_drag_selects_only_the_requested_text_including_unicode() {
        for surface in [model::Surface::User, model::Surface::Agent] {
            for word in ["bravo", "e\u{301}界🙂"] {
                let (_root, mut app) = fixture().await;
                app.entries = vec![Entry {
                    key: "selectable".into(),
                    text: format!("Sender\nAlpha **{word}** omega"),
                    surface,
                    expandable: false,
                    default_open: false,
                    running: false,
                    footer: None,
                    request: None,
                    indent: 0,
                    job: None,
                    compact_after: false,
                    header: None,
                    document: None,
                }];
                app.content_dirty = false;
                let buffer = draw_buffer(&mut app);
                let (x, y) = (0..24)
                    .find_map(|y| {
                        let row = (0..60).map(|x| buffer[(x, y)].symbol()).collect::<String>();
                        row.find("Alpha ").map(|x| (x as u16 + 6, y))
                    })
                    .unwrap();
                // Both fixtures occupy five cells; drag backwards as well as forwards.
                let (start, end) = if surface == model::Surface::User {
                    (x + 5, x)
                } else {
                    (x, x + 5)
                };
                mouse(
                    &mut app,
                    Rect::new(start, y, 1, 1),
                    MouseEventKind::Down(MouseButton::Left),
                );
                mouse(
                    &mut app,
                    Rect::new(end, y, 1, 1),
                    MouseEventKind::Drag(MouseButton::Left),
                );
                mouse(
                    &mut app,
                    Rect::new(end, y, 1, 1),
                    MouseEventKind::Up(MouseButton::Left),
                );
                let selected = draw_buffer(&mut app);
                let color = super::super::render::Palette::new(app.light).selected;
                // Terminals paint wide characters from their leading cell; Ratatui's
                // backend diff skips their continuation cells.
                let mut column = x;
                while column < x + 5 {
                    assert_eq!(selected[(column, y)].bg, color);
                    column += unicode_width::UnicodeWidthStr::width(selected[(column, y)].symbol())
                        .max(1) as u16;
                }
                assert_ne!(selected[(x - 1, y)].bg, color);
                assert_ne!(selected[(x + 5, y)].bg, color);
                key(&mut app, KeyCode::Char('x'), M::CONTROL);
                key(&mut app, KeyCode::Char('y'), M::NONE);
                assert_eq!(app.clipboard.as_deref(), Some(word));
                app.session.as_ref().unwrap().shutdown().await.unwrap();
            }
        }
    }

    #[tokio::test]
    async fn queue_delivery_is_immediate_while_busy_and_cancellation_rejects_stale_acks() {
        let (_root, mut app) = fixture().await;
        app.operation = true;
        app.submit("pending".into(), vec![PathBuf::from("pending.png")]);
        assert_eq!(app.queue.len(), 1);
        assert!(app.history.is_empty());
        let id = app.queue[0].id;
        let generation = app.queue[0].generation;
        let token = app.queue[0].delivery.clone().expect("sent while busy");
        app.paused = true;
        app.cancel_queue_delivery();
        assert!(token.cancel());
        assert!(app.queue[0].delivery.is_none());
        app.queue_committed(id, generation, 0, Err("cancelled".into()));
        assert_eq!(app.queue.len(), 1);
        app.paused = false;
        app.deliver_queue();
        assert!(app.queue[0].generation > generation);
        app.queue_committed(id, generation, 0, Ok(()));
        assert_eq!(app.queue.len(), 1);
        assert!(app.history.is_empty());
        let edited = app.remove_queued(id).unwrap();
        assert_eq!(edited.images, [PathBuf::from("pending.png")]);
        assert!(app.queue.is_empty());
        app.session.as_ref().unwrap().shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn queue_waits_for_initial_input_and_commit_does_not_finish_original_operation() {
        let (_root, mut app) = fixture().await;
        app.operation = true;
        app.awaiting_initial_input = true;
        app.submit("followup".into(), vec![]);
        app.tick();
        assert!(app.queue[0].delivery.is_none());
        // The initial input commit releases follow-ups before its first request,
        // including when that request first needs compaction.
        let mut committed = app.snapshot.records.values().next_back().unwrap().clone();
        committed.sequence += 1;
        committed.event = SessionEvent::MessageCommitted {
            message: skyhook::provider::protocol::Message::User(vec![
                skyhook::provider::protocol::UserContent::Text {
                    text: "initial".into(),
                },
            ]),
        };
        app.observe(ObservedEvent {
            revision: app.snapshot.revision + 1,
            event: RuntimeEvent::Record(Box::new(committed)),
        });
        assert!(!app.awaiting_initial_input);
        let input = &app.queue[0];
        let id = input.id;
        let generation = input.generation;
        // This unit test synthesizes the ack rather than invoking a provider.
        assert!(input.delivery.as_ref().unwrap().cancel());
        app.work(Work::QueueCommitted {
            session: app.session_id().unwrap(),
            id,
            generation,
            revision: app.snapshot.revision + 1,
            result: Ok(()),
        });
        assert!(app.queue.is_empty());
        assert_eq!(app.history, ["followup"]);
        assert!(
            app.operation,
            "commit is not a turn-completion acknowledgement"
        );
        app.operation = false;
        assert!(
            app.busy(),
            "wait for observation activity after an idle enqueue"
        );
        app.snapshot.revision = app.queue_activity_revision;
        assert!(!app.busy());
        app.session.as_ref().unwrap().shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn queue_failure_retains_row_and_pauses_remaining_deliveries() {
        let (_root, mut app) = fixture().await;
        app.operation = true;
        app.submit("first".into(), vec![]);
        app.submit("second".into(), vec![]);
        let id = app.queue[0].id;
        let generation = app.queue[0].generation;
        assert!(app.queue[0].delivery.as_ref().unwrap().cancel());
        app.queue_committed(id, generation, 0, Err("attachment missing".into()));
        assert!(app.paused);
        assert_eq!(app.queue.len(), 2);
        assert!(app.queue.iter().all(|input| input.delivery.is_none()));
        assert!(app.history.is_empty());
        app.session.as_ref().unwrap().shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn questions_open_and_accept_answers_while_inspecting_agents() {
        let (_root, mut app) = fixture().await;
        let mut child = app.projection.agents[0].clone();
        child.id = child.id.child(1);
        child.name = "worker".into();
        child.terminal = false;
        app.projection.agents.push(child.clone());
        app.select(child.id.clone());
        app.editor.set("preserved draft".into());

        for focus in [Focus::Tree, Focus::Content] {
            app.focus = focus;
            let answer = question(
                &mut app,
                "Choose a direction".into(),
                vec![QuestionOption {
                    label: "Continue".into(),
                    description: "Keep working".into(),
                }],
            );
            assert!(app.prompt_active);
            let screen = draw(&mut app);
            assert!(app.tree_rect.height > 0);
            assert!(screen.contains("Choose a direction"));
            assert!(screen.contains("Continue"));
            key(&mut app, KeyCode::Enter, M::NONE);
            assert!(matches!(
                answer.await.unwrap().unwrap(),
                PromptResponse::Questions(Value::String(value)) if value == "Continue"
            ));
            assert_eq!(app.selected, child.id);
            assert!(app.focus == focus);
            assert_eq!(app.editor.text, "preserved draft");
        }
        app.session.as_ref().unwrap().shutdown().await.unwrap();
    }

    fn suggestions() -> Vec<QuestionOption> {
        ["First", "Second"]
            .into_iter()
            .map(|label| QuestionOption {
                label: label.into(),
                description: format!("Use {label}"),
            })
            .collect()
    }

    #[tokio::test]
    async fn question_navigation_preserves_unanswered_drafts_and_clamps() {
        let (_root, mut app) = fixture().await;
        let mut response = question(&mut app, "Choose".into(), suggestions());
        if let PromptKind::Questions { questions, .. } = &mut app.prompts.front_mut().unwrap().kind
        {
            for id in ["middle", "last"] {
                questions.push(Question {
                    id: id.into(),
                    prompt: id.into(),
                    options: vec![],
                });
            }
        }
        assert!(draw(&mut app).contains("Question 1/3 · ←→ switch · Tab edit"));
        key(&mut app, KeyCode::Left, M::NONE);
        assert_eq!(app.question_index, 0);
        key(&mut app, KeyCode::Down, M::NONE);
        app.event(Event::Paste("comment".into()));
        key(&mut app, KeyCode::Left, M::NONE);
        assert_eq!(app.question_index, 0);
        assert_eq!(app.prompt_editor.cursor, 6);
        let screen = draw(&mut app);
        assert!(screen.contains("←→ cursor · Tab switch questions"));
        assert!(screen.contains("commen▏t"));
        key(&mut app, KeyCode::Tab, M::NONE);
        app.prompt_body_scroll = 5;
        app.prompt_option_scroll = 5;
        key(&mut app, KeyCode::Right, M::NONE);
        assert_eq!(app.prompt_body_scroll, 0);
        assert_eq!(app.prompt_option_scroll, 0);
        app.event(Event::Paste("draft".into()));
        key(&mut app, KeyCode::Tab, M::NONE);
        key(&mut app, KeyCode::Right, M::NONE);
        key(&mut app, KeyCode::Right, M::NONE);
        assert_eq!(app.question_index, 2);
        assert!(app.prompt_editor.text.is_empty());
        assert!(app.answers.is_empty());
        assert!(matches!(
            response.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        key(&mut app, KeyCode::Left, M::NONE);
        assert_eq!(app.prompt_editor.text, "draft");
        key(&mut app, KeyCode::Left, M::NONE);
        assert_eq!(app.prompt_choice, 1);
        assert_eq!(app.prompt_editor.text, "comment");
        assert_eq!(app.prompt_editor.cursor, 6);
        let ssh = authentication(&mut app, 100);
        key(&mut app, KeyCode::Esc, M::NONE);
        assert!(ssh.await.unwrap().is_err());
        key(&mut app, KeyCode::Right, M::NONE);
        assert_eq!(app.prompt_editor.text, "draft");
        assert!(app.answers.is_empty());
        app.session.as_ref().unwrap().shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn question_navigation_enter_wraps_skips_before_review_and_submit() {
        let (_root, mut app) = fixture().await;
        let mut response = question(&mut app, "First".into(), vec![]);
        if let PromptKind::Questions { questions, .. } = &mut app.prompts.front_mut().unwrap().kind
        {
            questions.push(Question {
                id: "last".into(),
                prompt: "Last".into(),
                options: vec![],
            });
        }
        key(&mut app, KeyCode::Right, M::NONE);
        app.event(Event::Paste("second".into()));
        key(&mut app, KeyCode::Enter, M::NONE);
        assert_eq!(app.question_index, 0);
        assert_eq!(app.answers.len(), 1);
        key(&mut app, KeyCode::Enter, M::NONE); // Empty free-form stays unanswered.
        assert_eq!(app.question_index, 0);
        assert!(matches!(
            response.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        app.event(Event::Paste("first".into()));
        key(&mut app, KeyCode::Enter, M::NONE);
        assert_eq!(app.prompt_editor.text, "second");
        key(&mut app, KeyCode::Enter, M::NONE);
        assert_eq!(app.question_index, 2); // Review, not submission.
        assert!(matches!(
            response.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        key(&mut app, KeyCode::Right, M::NONE);
        assert_eq!(app.question_index, 2);
        key(&mut app, KeyCode::Left, M::NONE); // Review can return to last question.
        assert_eq!(app.question_index, 1);
        app.event(Event::Paste(" revised".into()));
        assert!(!app.answers.contains_key("last"));
        key(&mut app, KeyCode::Tab, M::NONE);
        key(&mut app, KeyCode::Left, M::NONE);
        key(&mut app, KeyCode::Enter, M::NONE);
        assert_eq!(app.prompt_editor.text, "second revised");
        key(&mut app, KeyCode::Enter, M::NONE);
        key(&mut app, KeyCode::Enter, M::NONE);
        assert!(
            matches!(response.await.unwrap().unwrap(), PromptResponse::Questions(value)
            if value == json!({"answer": "first", "last": "second revised"}))
        );
        app.session.as_ref().unwrap().shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn question_navigation_does_not_change_single_question_or_auth_editing() {
        let (_root, mut app) = fixture().await;
        let response = question(&mut app, "Single".into(), vec![]);
        app.event(Event::Paste("abc".into()));
        key(&mut app, KeyCode::Left, M::NONE);
        assert_eq!(app.prompt_editor.cursor, 2);
        key(&mut app, KeyCode::Enter, M::NONE);
        assert!(
            matches!(response.await.unwrap().unwrap(), PromptResponse::Questions(value) if value == "abc")
        );
        let ssh = authentication(&mut app, 100);
        app.event(Event::Paste("secret".into()));
        key(&mut app, KeyCode::Left, M::NONE);
        assert_eq!(app.prompt_editor.cursor, 5);
        assert!(!draw(&mut app).contains("secret"));
        key(&mut app, KeyCode::Esc, M::NONE);
        assert!(ssh.await.unwrap().is_err());
        app.session.as_ref().unwrap().shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn question_comments_survive_ssh_preemption_and_batch_review() {
        let (_root, mut app) = fixture().await;
        let response = question(&mut app, "Choose".into(), suggestions());
        if let PromptKind::Questions { questions, .. } = &mut app.prompts.front_mut().unwrap().kind
        {
            questions.push(Question {
                id: "next".into(),
                prompt: "Anything else?".into(),
                options: vec![],
            });
        }
        key(&mut app, KeyCode::Down, M::NONE);
        app.event(Event::Paste("my comment".into()));
        let ssh = authentication(&mut app, 100);
        key(&mut app, KeyCode::Esc, M::NONE);
        assert!(ssh.await.unwrap().is_err());
        assert_eq!(app.prompt_choice, 1);
        assert_eq!(app.prompt_editor.text, "my comment");
        key(&mut app, KeyCode::Enter, M::NONE);
        assert!(app.prompt_editor.text.is_empty());
        app.event(Event::Paste("freeform".into()));
        key(&mut app, KeyCode::Enter, M::NONE);
        assert!(app.prompt_text().contains("my comment"));
        key(&mut app, KeyCode::Down, M::NONE); // Review again.
        key(&mut app, KeyCode::Enter, M::NONE);
        assert_eq!(app.question_index, 0);
        assert_eq!(app.prompt_choice, 1);
        assert_eq!(app.prompt_editor.text, "my comment");
        app.event(Event::Paste(" amended".into()));
        key(&mut app, KeyCode::Enter, M::NONE);
        assert_eq!(app.prompt_editor.text, "freeform");
        key(&mut app, KeyCode::Enter, M::NONE);
        key(&mut app, KeyCode::Enter, M::NONE);
        let PromptResponse::Questions(value) = response.await.unwrap().unwrap() else {
            panic!("wrong response")
        };
        assert_eq!(
            value,
            serde_json::json!({"answer": {"answer": "Second", "comment": "my comment amended"}, "next": "freeform"})
        );
        app.session.as_ref().unwrap().shutdown().await.unwrap();
    }

    fn authentication(app: &mut App, id: u64) -> oneshot::Receiver<Result<PromptResponse, String>> {
        let (reply, response) = oneshot::channel();
        app.prompt(Prompt {
            id,
            kind: PromptKind::Authentication(skyhook::remote::SensitivePrompt {
                kind: skyhook::remote::SensitivePromptKind::Password,
                message: format!("SSH password {id}"),
            }),
            reply,
        });
        response
    }

    #[tokio::test]
    async fn authentication_preempts_overlays_and_restores_partial_question_batches() {
        let (_root, mut app) = fixture().await;
        for finish in 0..3 {
            app.editor.set("composer draft".into());
            app.focus = Focus::Tree;
            let answer = question(&mut app, "First question".into(), vec![]);
            if let PromptKind::Questions { questions, .. } =
                &mut app.prompts.front_mut().unwrap().kind
            {
                questions.push(Question {
                    id: "second".into(),
                    prompt: "Second question".into(),
                    options: vec![],
                });
            }
            app.event(Event::Paste("first answer".into()));
            key(&mut app, KeyCode::Enter, M::NONE);
            app.event(Event::Paste("unfinished answer".into()));
            app.prompt_body_scroll = 3;
            app.prompt_option_scroll = 2;
            app.info("Details", "An open menu".into());
            app.search_editor = Some(Editor::default());
            let response = authentication(&mut app, 100);
            assert!(matches!(app.input_target(), InputTarget::Prompt));
            assert!(app.menu.is_none());
            assert!(app.search_editor.is_none());
            assert_eq!(app.prompts.front().unwrap().id, 100);
            assert!(app.prompt_editor.text.is_empty());
            assert!(app.answers.is_empty());
            app.event(Event::Paste("ssh secret".into()));
            let screen = draw(&mut app);
            assert!(screen.contains("SSH password 100"));
            assert!(!screen.contains("ssh secret"));
            match finish {
                0 => {
                    key(&mut app, KeyCode::Enter, M::NONE);
                    let PromptResponse::Authentication(secret) = response.await.unwrap().unwrap()
                    else {
                        panic!("wrong response kind")
                    };
                    assert_eq!(secret.expose(), "ssh secret");
                }
                1 => {
                    key(&mut app, KeyCode::Esc, M::NONE);
                    assert!(response.await.unwrap().is_err());
                }
                _ => {
                    drop(response);
                    app.tick();
                }
            }
            assert!(app.prompt_active);
            assert!(app.focus == Focus::Tree);
            assert_eq!(app.question_index, 1);
            assert_eq!(app.prompt_editor.text, "unfinished answer");
            assert_eq!(app.prompt_body_scroll, 3);
            assert_eq!(app.prompt_option_scroll, 2);
            assert_eq!(app.answers["answer"], "first answer");
            assert_eq!(app.editor.text, "composer draft");
            assert!(app.suspended_prompt.is_none());
            key(&mut app, KeyCode::Enter, M::NONE);
            key(&mut app, KeyCode::Enter, M::NONE);
            let PromptResponse::Questions(value) = answer.await.unwrap().unwrap() else {
                panic!("wrong response kind")
            };
            assert_eq!(value["answer"], "first answer");
            assert_eq!(value["second"], "unfinished answer");
        }
        app.session.as_ref().unwrap().shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn authentication_is_fifo_and_takes_priority_over_dismissed_requests() {
        let (_root, mut app) = fixture().await;
        let answer = question(&mut app, "Question".into(), vec![]);
        app.event(Event::Paste("saved answer".into()));
        key(&mut app, KeyCode::Esc, M::NONE);
        let first = authentication(&mut app, 100);
        app.event(Event::Paste("first secret".into()));
        let second = authentication(&mut app, 101);
        assert!(app.prompt_active);
        assert_eq!(
            app.prompts.iter().map(|p| p.id).collect::<Vec<_>>(),
            vec![100, 101, 1]
        );
        assert_eq!(app.prompt_editor.text, "first secret");
        key(&mut app, KeyCode::Esc, M::NONE);
        assert!(first.await.unwrap().is_err());
        assert!(app.prompt_active);
        assert!(app.prompt_editor.text.is_empty());
        assert_eq!(app.prompts.front().unwrap().id, 101);
        key(&mut app, KeyCode::Esc, M::NONE);
        assert!(second.await.unwrap().is_err());
        assert!(!app.prompt_active);
        assert_eq!(app.prompt_editor.text, "saved answer");
        app.command("attention");
        key(&mut app, KeyCode::Enter, M::NONE);
        assert!(
            matches!(answer.await.unwrap().unwrap(), PromptResponse::Questions(Value::String(value)) if value == "saved answer")
        );
        app.session.as_ref().unwrap().shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cancelled_suspended_questions_do_not_leak_into_later_prompts() {
        let (_root, mut app) = fixture().await;
        let answer = question(&mut app, "Question".into(), vec![]);
        app.event(Event::Paste("abandoned answer".into()));
        let response = authentication(&mut app, 100);
        drop(answer);
        app.tick();
        key(&mut app, KeyCode::Esc, M::NONE);
        assert!(response.await.unwrap().is_err());
        assert!(app.suspended_prompt.is_none());
        assert!(app.prompt_editor.text.is_empty());
        let _next = question(&mut app, "Next question".into(), vec![]);
        assert!(app.prompt_editor.text.is_empty());
        app.session.as_ref().unwrap().shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn authentication_moves_secret_and_clears_editor() {
        let (_root, mut app) = fixture().await;
        let (reply, response) = oneshot::channel();
        app.prompt(Prompt {
            id: 1,
            kind: PromptKind::Authentication(skyhook::remote::SensitivePrompt {
                kind: skyhook::remote::SensitivePromptKind::Password,
                message: "Password".into(),
            }),
            reply,
        });
        app.prompt_editor.insert("secret");
        app.answer();
        let PromptResponse::Authentication(secret) = response.await.unwrap().unwrap() else {
            panic!("wrong response kind")
        };
        assert_eq!(secret.expose(), "secret");
        assert!(app.prompt_editor.text.is_empty());
        assert!(app.prompts.is_empty());
        app.session.as_ref().unwrap().shutdown().await.unwrap();
    }
}
