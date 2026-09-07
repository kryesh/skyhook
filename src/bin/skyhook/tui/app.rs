use super::{
    Launch,
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
    agent::{AgentActivity, ObservationSnapshot, ObservedEvent, RuntimeEvent, SessionHandle},
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
    Profiles,
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
    Sessions(Result<Vec<Item>, String>),
    Files(Vec<Item>),
    File(Result<(PathBuf, String), String>),
    SessionReady(Result<SessionHandle, String>),
    StatusFailed {
        session: SessionId,
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
}

pub struct QueuedInput {
    id: u64,
    text: String,
    images: Vec<PathBuf>,
    model: String,
}

pub struct App {
    pub session: SessionHandle,
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
    pub editor: Editor,
    pub images: Vec<PathBuf>,
    pub pastes: Vec<String>,
    pub history: Vec<String>,
    history_index: Option<usize>,
    history_draft: String,
    pub queue: VecDeque<QueuedInput>,
    next_queued_id: u64,
    switch_restore: Option<bool>,
    pub paused: bool,
    pub operation: bool,
    pub prompts: VecDeque<Prompt>,
    pub prompt_active: bool,
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
    pub answers: serde_json::Map<String, Value>,
    pub menu: Option<Menu>,
    pub status: super::status::StatusLog,
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
    prewarmed: HashSet<(JobId, bool)>,
}
impl App {
    pub fn new(
        session: SessionHandle,
        launch: Launch,
        snapshot: ObservationSnapshot,
        remembered_model: Option<String>,
        tx: mpsc::UnboundedSender<Work>,
        keys: KeyMap,
        light: bool,
    ) -> Self {
        let selected = session.root_agent().clone();
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
            editor: Editor::default(),
            images: vec![],
            pastes: vec![],
            history: vec![],
            history_index: None,
            history_draft: String::new(),
            queue: VecDeque::new(),
            next_queued_id: 0,
            switch_restore: None,
            paused: false,
            operation: false,
            prompts: VecDeque::new(),
            prompt_active: false,
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
            answers: serde_json::Map::new(),
            menu: None,
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
            prewarmed: HashSet::new(),
        };
        app.refresh();
        if let Some(root) = app.projection.agents.iter().find(|a| a.id == app.selected) {
            app.model.clone_from(&root.model);
        }
        app.show_warnings();
        app
    }
    fn show_warnings(&mut self) {
        if !self.session.warnings().is_empty() {
            self.notice(format!(
                "{} startup warning(s) · /diagnostics",
                self.session.warnings().len()
            ));
        }
    }
    fn notifier(&self) -> super::status::StatusSender {
        self.status.sender(&self.session, &self.selected)
    }
    fn root_notifier(&self) -> super::status::StatusSender {
        self.status.sender(&self.session, self.session.root_agent())
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
        let (records, repaint, content) = match &event.event {
            RuntimeEvent::Record(_) => (true, true, true),
            RuntimeEvent::TextDelta { agent, .. }
            | RuntimeEvent::ReasoningDelta { agent, .. }
            | RuntimeEvent::ResponseSettled { agent, .. } => {
                (false, agent == &self.selected, agent == &self.selected)
            }
            RuntimeEvent::Activity { agent, .. } => (false, true, agent == &self.selected),
            RuntimeEvent::Context { agent, .. } => (false, agent == &self.selected, false),
            RuntimeEvent::TurnCompleted { .. } => (false, true, false),
        };
        // Only deltas guarantee an unchanged prefix. All other content changes may
        // replace entries or alter their presentation and advance the cache revision.
        let append_only = match &event.event {
            RuntimeEvent::TextDelta { agent, request, .. }
            | RuntimeEvent::ReasoningDelta { agent, request, .. } => {
                self.content_cache.observe_response(agent, *request);
                true
            }
            _ => false,
        };
        self.snapshot.apply(event);
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
                    indent: 0,
                    job: None,
                    document: None,
                });
            }
            self.content_dirty = false;
        }
    }
    pub fn prewarm_entry(&mut self, index: usize) {
        if let Some(job) = self.entries.get(index).and_then(|entry| entry.job)
            && self.prewarmed.insert((job, self.light))
            && let Some(job) = self.projection.jobs.get(&job)
        {
            let mut document = super::tool_view::Document::default();
            document.arguments(&job.tool, &job.args);
            self.render.highlights.warm(&document, self.light);
        }
    }
    pub fn busy(&self) -> bool {
        self.switch_restore.is_some()
            || self.operation
            || matches!(
                self.snapshot.activity.get(self.session.root_agent()),
                Some(
                    AgentActivity::Working
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
    pub fn set_session(&mut self, session: SessionHandle, snapshot: ObservationSnapshot) {
        self.session = session;
        self.snapshot = snapshot;
        self.selected = self.session.root_agent().clone();
        self.views.clear();
        self.content_cache = model::ContentCache::default();
        self.render.reset_session();
        self.prewarmed.clear();
        self.outputs.clear();
        self.pending_outputs.clear();
        self.final_outputs.clear();
        self.output_versions.clear();
        self.output_queries.clear();
        self.prompts.clear();
        self.reset_prompt();
        self.prompt_active = false;
        self.editor = Editor::default();
        self.images.clear();
        self.pastes.clear();
        self.queue.clear();
        self.switch_restore = None;
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
    }
    pub fn submit(&mut self, text: String, images: Vec<PathBuf>) {
        if text.trim().is_empty() && images.is_empty() {
            return;
        }
        let queued = self.queued_input(text, images);
        if self.busy() {
            self.queue.push_back(queued);
            self.refresh_queue_menu();
            self.dirty = true;
            return;
        }
        self.send_input(queued);
    }
    fn send_input(&mut self, queued: QueuedInput) {
        let QueuedInput {
            text,
            images,
            model,
            ..
        } = queued;
        self.launch.model.clone_from(&model);
        self.operation = true;
        self.history.push(text.clone());
        self.history_index = None;
        let session = self.session.clone();
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
    fn set_title(&self, title: &str) {
        let path = self.session.directory().join("ui.json");
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
        self.operation = true;
        self.set_title(&path.display().to_string());
        let session = self.session.clone();
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
        self.stopping = true;
        let session = self.session.clone();
        let tx = self.tx.clone();
        let notices = self.root_notifier();
        let status = self.status.clone();
        tokio::spawn(async move {
            status.flush().await;
            let result = session.shutdown().await;
            if let Err(e) = result {
                notices.send(e.to_string());
            }
            status.flush().await;
            let _ = tx.send(Work::Stopped);
        });
        self.dirty = true;
    }
    pub fn work(&mut self, work: Work) {
        match work {
            Work::Done { session, result } if session == self.session.id() => {
                self.operation = false;
                if let Err(error) = result {
                    self.root_notifier().send(error);
                    self.paused = true;
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
            } if session == self.session.id() => {
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
            Work::Sessions(result) => match result {
                Ok(items) => self.open("Resume session", MenuKind::Sessions, items),
                Err(e) => self.notice(e),
            },
            Work::Files(items) => {
                if let Some(menu) = &mut self.menu
                    && matches!(menu.kind, MenuKind::Files)
                {
                    menu.items = items;
                }
            }
            Work::File(result) => match result {
                Ok((path, content)) => {
                    self.pastes
                        .push(format!("File: {}\n{content}", path.display()));
                    self.notice(format!("Attached {}", path.display()));
                }
                Err(e) => self.notice(e),
            },
            Work::SessionReady(Err(error)) => {
                if let Some(paused) = self.switch_restore.take() {
                    self.paused = paused;
                }
                self.notice(error);
            }
            Work::StatusFailed {
                session,
                agent,
                message,
            } if session == self.session.id() => {
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
        if !self.busy() && !self.paused && !self.queue.is_empty() {
            let queued = self.queue.pop_front().unwrap();
            self.refresh_queue_menu();
            self.send_input(queued);
        }
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
        if !self.pending_outputs.insert(job) {
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
        let session = self.session.clone();
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
        let show = self.prompts.is_empty() && self.focus == Focus::Composer && self.menu.is_none();
        self.prompts.push_back(prompt);
        if show {
            self.prompt_active = true;
        }
        self.dirty = true;
    }
    fn reset_prompt(&mut self) {
        self.prompt_editor.clear_sensitive();
        self.prompt_editor = Editor::default();
        self.prompt_choice = 0;
        self.reset_prompt_view();
        self.question_index = 0;
        self.answers.clear();
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
                    let answer = if let Some(option) = question.options.get(self.prompt_choice) {
                        option.label.clone()
                    } else {
                        self.prompt_editor.text.clone()
                    };
                    if answer.trim().is_empty() {
                        return;
                    }
                    self.answers
                        .insert(question.id.clone(), Value::String(answer));
                    self.question_index += 1;
                    self.prompt_body_scroll = 0;
                    self.prompt_option_scroll = 0;
                    self.prompt_reveal = true;
                    self.prompt_choice = 0;
                    self.prompt_editor = Editor::default();
                    if questions.len() == 1 {
                        Some(PromptResponse::Questions(
                            self.answers.values().next().cloned().unwrap_or(Value::Null),
                        ))
                    } else {
                        None
                    }
                } else if self.prompt_choice == 1 {
                    self.question_index = 0;
                    self.prompt_body_scroll = 0;
                    self.prompt_option_scroll = 0;
                    self.prompt_reveal = true;
                    self.prompt_choice = 0;
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
                        self.select_written_answer();
                    }
                    InputTarget::Composer if text.lines().count() > 12 => self.pastes.push(text),
                    InputTarget::Composer => self.editor.insert(&text),
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
                                Hit::Attention => {
                                    self.prompt_active = true;
                                    self.focus = Focus::Composer;
                                }
                                Hit::PromptChoice(index) => {
                                    self.prompt_choice = index;
                                    self.prompt_reveal = true;
                                }
                                Hit::Latest => {
                                    self.view().scroll = None;
                                }
                            }
                        }
                        if self.content_rect.contains(point.into()) {
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
                            self.selection = self
                                .text_position(point)
                                .map(|position| (position, position));
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
        } else {
            InputTarget::Composer
        }
    }
    fn select_written_answer(&mut self) {
        if matches!(
            self.prompts.front().map(|p| &p.kind),
            Some(PromptKind::Questions { .. })
        ) {
            self.prompt_choice = self.prompt_options().len().saturating_sub(1);
            self.prompt_reveal = true;
        }
    }
    fn key(&mut self, key: KeyEvent) {
        let target = self.input_target();
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
                    }
                    self.prompt_active = false;
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
                KeyCode::Up | KeyCode::BackTab => {
                    self.prompt_choice = self.prompt_choice.saturating_sub(1);
                    self.prompt_reveal = true;
                }
                KeyCode::Down | KeyCode::Tab => {
                    if !options.is_empty() {
                        self.prompt_choice = (self.prompt_choice + 1) % options.len();
                        self.prompt_reveal = true;
                    }
                }
                KeyCode::Enter => self.answer(),
                _ => {
                    self.prompt_editor.handle(key);
                    if matches!(key.code, KeyCode::Char(_))
                        && !key.modifiers.intersects(M::CONTROL | M::ALT)
                    {
                        self.select_written_answer();
                    }
                }
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
                } else if self.focus != Focus::Composer {
                    self.focus = Focus::Composer;
                }
                return;
            }
            KeyCode::Char('c') if key.modifiers.contains(M::CONTROL) => {
                if self.focus == Focus::Composer && !self.editor.text.is_empty() {
                    self.editor.take();
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
                    let mut text = self.editor.take();
                    if text.starts_with('/') && !text.contains('\n') {
                        let command = text.trim_start_matches('/').trim().to_owned();
                        self.command(&command);
                    } else {
                        for paste in self.pastes.drain(..) {
                            text.push_str(&format!("\n\n{paste}"));
                        }
                        let images = std::mem::take(&mut self.images);
                        self.paused = false;
                        self.submit(text, images);
                    }
                }
                KeyCode::Up if !self.editor.text[..self.editor.cursor].contains('\n') => {
                    self.prompt_history(false)
                }
                KeyCode::Down if !self.editor.text[self.editor.cursor..].contains('\n') => {
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
                KeyCode::Up => {
                    self.view().row = self.view().row.saturating_sub(1);
                    self.reveal_row();
                }
                KeyCode::Down => {
                    self.view().row =
                        (self.view().row + 1).min(self.entries.len().saturating_sub(1));
                    self.reveal_row();
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
            if self.entries[index].text.to_lowercase().contains(&query) {
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
        let session = self.session.clone();
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
            self.history_draft = self.editor.text.clone();
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
                self.editor.set(self.history_draft.clone());
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
        self.queue.remove(index)
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
        self.menu = Some(Menu {
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
        if self.switch_restore.is_some() {
            return;
        }
        if id == Some(self.session.id()) {
            self.select(self.session.root_agent().clone());
            return;
        }
        self.switch_restore = Some(self.paused);
        self.paused = true;
        self.notice("Opening session…");
        let old = self.session.clone();
        let launch = self.launch.clone();
        let tx = self.tx.clone();
        let status = self.status.clone();
        tokio::spawn(async move {
            status.flush().await;
            let result = async {
                let destination = launch.create(id).await?;
                if let Err(error) = old.shutdown().await {
                    let _ = destination.shutdown().await;
                    return Err(error.to_string());
                }
                Ok(destination)
            }
            .await;
            let _ = tx.send(Work::SessionReady(result));
        });
    }
    pub fn command(&mut self, command: &str) {
        match command {
            "commands" => self.open(
                "Commands", MenuKind::Commands,
                COMMANDS.iter().map(|(id, label, _)| Item::new(*id, *label, self.keys.binding(id))).collect(),
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
            "profiles" => {
                let mut items = vec![Item::new("", "Default instructions", "")];
                items.extend(self.launch.config.agents.keys().map(|name| Item::new(name, name, "")));
                self.open("Instruction profile · new sessions", MenuKind::Profiles, items);
            }
            "agents" => self.open(
                "Agents", MenuKind::Agents,
                self.projection.agents.iter().map(|agent| Item::new(
                    agent.id.to_string(),
                    format!("{}{}", "    ".repeat(agent.id.depth()), agent.name),
                    self.projection.status(agent, &self.snapshot).1,
                )).collect(),
            ),
            "themes" => self.open("Theme", MenuKind::Themes, vec![
                Item::new("dark", "Dark", ""), Item::new("light", "Light", ""),
            ]),
            "inspect" => {
                self.focus = Focus::Content;
                self.view().tab = Tab::Conversation;
            }
            "jobs" | "requests" | "state" => {
                self.focus = Focus::Content;
                self.view().tab = match command {
                    "jobs" => Tab::Jobs,
                    "requests" => Tab::Requests,
                    _ => Tab::State,
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
            "attention" => {
                self.prompt_active = !self.prompts.is_empty();
                self.focus = Focus::Composer;
            }
            "resume" => {
                self.paused = false;
                self.notice("Queued input resumed");
            }
            "retry" => {
                if !self.busy() && matches!(
                    self.snapshot.activity.get(self.session.root_agent()),
                    Some(AgentActivity::Failed(_) | AgentActivity::Interrupted),
                ) {
                    self.operation = true;
                    let session = self.session.clone();
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
                let mut items: Vec<_> = self.pastes.iter().enumerate().map(|(i, text)| Item::attachment(
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
                self.notice("Loading sessions…");
                tokio::spawn(async move {
                    let result = load_sessions(root).await;
                    let _ = tx.send(Work::Sessions(result));
                });
            }
            "files" => {
                self.open("Attach workspace file", MenuKind::Files, vec![]);
                let root = self.launch.workspace.clone();
                let tx = self.tx.clone();
                tokio::task::spawn_blocking(move || {
                    let mut items = vec![];
                    walk_files(&root, &root, &mut items);
                    items.sort_by(|a, b| a.label.cmp(&b.label));
                    let _ = tx.send(Work::Files(items));
                });
            }
            "export" => {
                let entries = model::entries(
                    &self.snapshot, &self.projection, &self.selected,
                    &View::default(), &self.outputs, self.thinking, true,
                );
                let text = entries.iter().map(|entry| entry.text.as_str()).collect::<Vec<_>>().join("\n\n");
                let path = self.session.directory().join(format!(
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
            "diagnostics" => self.info("Startup diagnostics", self.session.warnings().join("\n")),
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
            "inspect" | "jobs" | "requests" | "state" | "thinking" | "details"
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
                                self.pastes.remove(index);
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
            MenuKind::Profiles => {
                self.launch.profile = if value.is_empty() { None } else { Some(value) };
                self.notice("Instruction profile applies to new sessions");
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
                    let _ = tx.send(Work::File(result));
                });
            }
            MenuKind::Attachments => match attachment {
                Some(Attachment::Paste(index)) => {
                    if let Some(text) = self.pastes.get(index) {
                        self.info("Attachment", text.clone());
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
                            let session = self.session.clone();
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
                    Item::new("field:/console", "console", ""),
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
        remote::EmbeddedShimCatalog,
    };
    use std::sync::Arc;
    use tokio::sync::oneshot;

    async fn fixture() -> (tempfile::TempDir, App) {
        let root = tempfile::tempdir().unwrap();
        let config = toml::from_str("[providers.test]\nkind='openai_compatible'\napi='chat_completions'\nbase_url='http://127.0.0.1:1'\n[models.first]\nprovider='test'\nmodel='fixture'\nmax_context=128000\nmax_output=4096\n").unwrap();
        let (interaction, _) = UiInteraction::new();
        let launch = Launch {
            config: Arc::new(config),
            model: "first".into(),
            profile: None,
            workspace: root.path().to_path_buf(),
            sessions: root.path().join(".skyhook/sessions"),
            catalog: EmbeddedShimCatalog::from_assets(&[]).unwrap(),
            interaction: Arc::new(interaction),
            approve_all: false,
        };
        let session = launch.create(None).await.unwrap();
        let snapshot = session.observe().await.snapshot;
        let (tx, _) = mpsc::unbounded_channel();
        let mut app = App::new(
            session,
            launch,
            snapshot,
            None,
            tx,
            KeyMap::new(&Default::default()).unwrap(),
            false,
        );
        // Unit fixtures must not change the user's global model preference.
        app.remembered_model = Some("first".into());
        (root, app)
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
                agent: app.session.root_agent().clone(),
                questions: vec![Question {
                    id: "answer".into(),
                    prompt,
                    options,
                }],
            },
            reply,
        });
        app.prompt_active = true;
        receiver
    }
    fn draw_buffer(app: &mut App) -> ratatui::buffer::Buffer {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 24)).unwrap();
        terminal
            .draw(|frame| super::super::render::draw(frame, app))
            .unwrap();
        terminal.backend().buffer().clone()
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
    fn tool_hits(app: &App) -> Vec<Rect> {
        app.hits
            .iter()
            .filter_map(|(rect, hit)| matches!(hit, Hit::Entry(_, true)).then_some(*rect))
            .collect()
    }

    #[tokio::test]
    async fn silent_requests_animate_and_single_line_reasoning_stays_inline_with_markdown() {
        let (_root, mut app) = fixture().await;
        let agent = app.selected.clone();
        app.observe(ObservedEvent {
            revision: app.snapshot.revision + 1,
            event: RuntimeEvent::Activity {
                agent: agent.clone(),
                activity: AgentActivity::Working,
            },
        });
        assert!(draw(&mut app).contains("⠋ Working"));
        let cached = app.render.rows.line_identities();
        app.tick();
        assert!(draw(&mut app).contains("⠙ Working"));
        let current = app.render.rows.line_identities();
        assert_eq!(cached.len(), current.len());
        assert!(cached.iter().zip(&current).all(|(a, b)| Arc::ptr_eq(a, b)));
        app.observe(ObservedEvent {
            revision: app.snapshot.revision + 1,
            event: RuntimeEvent::ReasoningDelta {
                agent: agent.clone(),
                request: 42,
                text: "**Check** `file.rs`".into(),
            },
        });
        let inline = draw(&mut app);
        assert!(inline.contains("⠙ Check file.rs"), "{inline}");
        assert!(!inline.contains("Reasoning"));
        assert!(!inline.contains("Working"));
        assert!(!app.entries[0].expandable);
        let hit = tool_hits(&app)[0];
        click(&mut app, hit);
        key(&mut app, KeyCode::Enter, M::NONE);
        assert!(draw(&mut app).contains("⠙ Check file.rs"));
        assert!(!app.entries[0].expandable);
        app.observe(ObservedEvent {
            revision: app.snapshot.revision + 1,
            event: RuntimeEvent::ReasoningDelta {
                agent: agent.clone(),
                request: 42,
                text: "\nThen **continue**.".into(),
            },
        });
        let multi = draw(&mut app);
        assert!(app.entries[0].expandable);
        assert!(multi.contains("Then continue."));
        app.observe(ObservedEvent {
            revision: app.snapshot.revision + 1,
            event: RuntimeEvent::TextDelta {
                agent: agent.clone(),
                request: 42,
                text: "Answer".into(),
            },
        });
        let answering = draw(&mut app);
        assert!(answering.contains("▸ Reasoning"));
        assert!(answering.contains("Working"));
        app.observe(ObservedEvent {
            revision: app.snapshot.revision + 1,
            event: RuntimeEvent::Activity {
                agent,
                activity: AgentActivity::Interrupted,
            },
        });
        assert!(!draw(&mut app).contains("Working"));
        assert!(!app.animating);
        app.session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn live_reasoning_can_be_collapsed_by_body_click_and_reopened_with_enter() {
        let (_root, mut app) = fixture().await;
        let agent = app.selected.clone();
        let delta = |text: &str| RuntimeEvent::ReasoningDelta {
            agent: agent.clone(),
            request: 42,
            text: text.into(),
        };
        app.observe(ObservedEvent {
            revision: app.snapshot.revision + 1,
            event: delta("First step\nMore detail"),
        });
        let first = draw(&mut app);
        assert!(first.contains("First step"));
        assert!(first.contains("⠋ Reasoning"));
        assert!(!first.contains("streaming"));
        let cached = app.render.rows.line_identities();
        app.tick();
        assert!(draw(&mut app).contains("⠙ Reasoning"));
        let current = app.render.rows.line_identities();
        assert_eq!(cached.len(), current.len());
        assert!(cached.iter().zip(&current).all(|(a, b)| Arc::ptr_eq(a, b)));
        let body = *tool_hits(&app).last().unwrap();
        click(&mut app, body);
        assert!(!draw(&mut app).contains("First step"));
        app.observe(ObservedEvent {
            revision: app.snapshot.revision + 1,
            event: delta("\nSecond step"),
        });
        assert!(!draw(&mut app).contains("Second step"));
        key(&mut app, KeyCode::Enter, M::NONE);
        assert!(draw(&mut app).contains("Second step"));

        app.observe(ObservedEvent {
            revision: app.snapshot.revision + 1,
            event: RuntimeEvent::TextDelta {
                agent: agent.clone(),
                request: 42,
                text: "Answer starts".into(),
            },
        });
        let answering = draw(&mut app);
        assert!(answering.contains("▸ Reasoning"));
        assert!(answering.contains("Answer starts"));
        assert!(!answering.contains("Second step"));
        assert!(!app.animating);

        // Commit the same reasoning as the runtime does at successful completion.
        let sequence = app.snapshot.records.last_key_value().unwrap().0 + 1;
        app.observe(ObservedEvent {
            revision: app.snapshot.revision + 1,
            event: RuntimeEvent::ResponseSettled {
                agent: agent.clone(),
                request: 42,
                message: Some(sequence),
                error: None,
            },
        });
        app.observe(ObservedEvent {
            revision: app.snapshot.revision + 1,
            event: RuntimeEvent::Record(Box::new(skyhook::session::EventRecord {
                version: 1,
                sequence,
                timestamp_millis: 0,
                agent,
                event: SessionEvent::MessageCommitted {
                    message: skyhook::provider::protocol::Message::Assistant(vec![
                        skyhook::provider::protocol::AssistantContent::Reasoning {
                            text: "First step\nSecond step".into(),
                            opaque: None,
                        },
                    ]),
                },
            })),
        });
        app.refresh();
        let screen = draw(&mut app);
        assert!(screen.contains("▸ Reasoning"), "{screen}");
        assert!(!screen.contains("Second step"));
        key(&mut app, KeyCode::Enter, M::NONE);
        assert!(draw(&mut app).contains("Second step"));
        key(&mut app, KeyCode::Enter, M::NONE);
        draw(&mut app);
        app.view().collapsed.clear();
        app.command("thinking");
        assert!(draw(&mut app).contains("Second step"));
        let body = *tool_hits(&app).last().unwrap();
        click(&mut app, body);
        assert!(!draw(&mut app).contains("Second step"));
        app.session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn statuses_are_durable_agent_scoped_rows_without_reserving_input_space() {
        let (_root, mut app) = fixture().await;
        draw(&mut app);
        let content_height = app.content_rect.height;
        let root = app.selected.clone();
        let child = root.child(1);
        let delayed = app.notifier();
        app.notice("Interrupted");
        app.select(child.clone());
        app.notice("Child status");
        delayed.send("Root follow-up status");
        app.status.flush().await;
        app.snapshot = app.session.observe().await.snapshot;
        app.refresh();
        let child_screen = draw(&mut app);
        assert!(child_screen.contains("Status · Child status"));
        assert!(!child_screen.contains("Interrupted"));
        app.select(root.clone());
        let root_screen = draw(&mut app);
        assert!(root_screen.contains("Status · Interrupted"));
        assert!(root_screen.contains("Status · Root follow-up status"));
        assert!(!root_screen.contains("Child status"));
        assert_eq!(app.content_rect.height, content_height);
        assert!(
            app.entries
                .iter()
                .all(|entry| entry.surface == model::Surface::Status
                    && !entry.expandable
                    && entry.job.is_none())
        );
        let records = SessionStore::read_records(&app.launch.sessions, app.session.id())
            .await
            .unwrap();
        let messages: Vec<_> = records
            .iter()
            .filter_map(|record| match &record.event {
                SessionEvent::Status { message } => Some(message.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            messages,
            ["Interrupted", "Child status", "Root follow-up status"]
        );
        assert!(
            skyhook::session::project_history(&records, &root)
                .unwrap()
                .is_empty()
        );
        app.work(Work::StatusFailed {
            session: app.session.id(),
            agent: root,
            message: "Unsaved status\nCould not save this status: disk full".into(),
        });
        assert!(draw(&mut app).contains("Could not save this status: disk full"));
        app.session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn model_selection_is_ui_only_and_queued_messages_capture_their_model() {
        let (_root, mut app) = fixture().await;
        let mut second = app.launch.config.models["first"].clone();
        second.model = "model-b".into();
        Arc::make_mut(&mut app.launch.config)
            .models
            .insert("second".into(), second);
        app.editor.set("Draft stays".into());
        let journal = app.session.directory().join("events.jsonl");
        let before = std::fs::read(&journal).unwrap();
        app.operation = true;
        app.submit("Queued A".into(), vec![]);
        app.command("model");
        assert_eq!(app.menu.as_ref().unwrap().title, "Model");
        key(&mut app, KeyCode::Down, M::NONE);
        assert_eq!(app.model, "first");
        key(&mut app, KeyCode::Esc, M::NONE);
        assert_eq!(app.model, "first");
        app.command("models");
        key(&mut app, KeyCode::Down, M::NONE);
        key(&mut app, KeyCode::Enter, M::NONE);
        assert_eq!(app.model, "second");
        assert_eq!(
            app.launch.model, "first",
            "unsent selection must not change new-session defaults"
        );
        assert_eq!(app.editor.text, "Draft stays");
        assert!(draw(&mut app).lines().nth(22).unwrap().contains("model-b"));
        assert_eq!(app.projection.agents[0].model, "first");
        app.submit("Queued B".into(), vec![]);
        app.command("model");
        assert_eq!(app.menu.as_ref().unwrap().selected, 1);
        key(&mut app, KeyCode::Up, M::NONE);
        key(&mut app, KeyCode::Enter, M::NONE);
        assert_eq!(
            app.queue
                .iter()
                .map(|q| q.model.as_str())
                .collect::<Vec<_>>(),
            ["first", "second"]
        );
        app.status.flush().await;
        assert_eq!(std::fs::read(&journal).unwrap(), before);
        app.session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn header_is_plain_text_and_footer_only_shows_model_id() {
        let (_root, mut app) = fixture().await;
        app.launch.workspace = PathBuf::from("/workspace/project");
        app.projection.agents[0].profile = Some("hidden-instruction-profile".into());
        let screen = draw(&mut app);
        let header = screen.lines().next().unwrap();
        assert!(header.contains("/workspace/project"));
        assert!(header.contains(&app.session.id().to_string()));
        assert!(!app.hits.iter().any(|(rect, _)| rect.y == 0));
        click(&mut app, Rect::new(2, 0, 1, 1));
        assert!(app.focus == Focus::Composer);
        assert!(app.clipboard.is_none());
        assert_eq!(draw(&mut app).lines().next().unwrap(), header);
        assert!(screen.lines().nth(22).unwrap().contains("fixture"));
        assert!(!screen.contains("hidden-instruction-profile"));
        assert!(!screen.lines().nth(22).unwrap().contains("first"));
        app.session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn theme_navigation_previews_without_rebuilding_content_and_cancel_restores() {
        let (_root, mut app) = fixture().await;
        draw(&mut app);
        app.command("themes");
        assert!(!app.light);
        key(&mut app, KeyCode::Down, M::NONE);
        assert!(app.light);
        assert!(!app.content_dirty);
        key(&mut app, KeyCode::Esc, M::NONE);
        assert!(!app.light);
        app.light = true;
        app.command("themes");
        assert_eq!(app.menu.as_ref().unwrap().selected, 1);
        assert!(app.light);
        mouse(&mut app, Rect::default(), MouseEventKind::ScrollUp);
        assert!(!app.light);
        mouse(&mut app, Rect::default(), MouseEventKind::ScrollDown);
        assert!(app.light);
        key(&mut app, KeyCode::Home, M::NONE);
        assert!(!app.light);
        key(&mut app, KeyCode::Esc, M::NONE);
        assert!(app.light);
        app.session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn idle_ticks_and_hidden_streams_do_not_invalidate_the_visible_conversation() {
        let (_root, mut app) = fixture().await;
        draw(&mut app);
        app.dirty = false;
        app.tick();
        assert!(!app.dirty);
        mouse(&mut app, Rect::new(0, 2, 1, 1), MouseEventKind::Moved);
        assert!(!app.dirty);
        let child = app.selected.child(1);
        let event = RuntimeEvent::TextDelta {
            agent: child.clone(),
            request: 42,
            text: "child stream".into(),
        };
        assert!(!app.observe(ObservedEvent {
            revision: app.snapshot.revision + 1,
            event
        }));
        assert_eq!(app.snapshot.responses[&(child, 42)].text, "child stream");
        assert!(!app.dirty);
        assert!(!app.content_dirty);
        let event = RuntimeEvent::TextDelta {
            agent: app.selected.clone(),
            request: 43,
            text: "visible stream".into(),
        };
        app.observe(ObservedEvent {
            revision: app.snapshot.revision + 1,
            event,
        });
        assert!(app.dirty);
        assert!(app.content_dirty);
        app.session.shutdown().await.unwrap();
    }

    /// Full frame timings include Ratatui buffer diffing but no terminal I/O.
    /// Run optimized, with one benchmark thread, to compare history-independent work.
    #[tokio::test]
    #[ignore = "manual optimized UI latency benchmark"]
    async fn incremental_rendering_benchmark() {
        use skyhook::{
            provider::protocol::{AssistantContent, Message},
            session::EventRecord,
        };
        for count in [100, 10_000] {
            let (_root, mut app) = fixture().await;
            let first = app.snapshot.records.last_key_value().unwrap().0 + 1;
            for offset in 0..count {
                let sequence = first + offset;
                app.snapshot.records.insert(sequence, EventRecord {
                    version: 1, sequence, timestamp_millis: 0, agent: app.selected.clone(),
                    event: SessionEvent::MessageCommitted { message: Message::Assistant(vec![AssistantContent::Text {
                        text: format!("Message {offset}: **important** detail with `code` and ordinary text."),
                    }]) },
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
                    let request = first + count + u64::from(reasoning) + u64::from(markdown);
                    let body = if markdown {
                        "# Heading\n\nA **stable** reasoning paragraph with `code`.\n\n```rust\nlet n = 1;\n```\n\n".repeat(14_000)
                    } else {
                        "A stable reasoning paragraph with ordinary text.\n\n".repeat(22_000)
                    };
                    let event = if reasoning {
                        RuntimeEvent::ReasoningDelta {
                            agent: app.selected.clone(),
                            request,
                            text: body,
                        }
                    } else {
                        RuntimeEvent::TextDelta {
                            agent: app.selected.clone(),
                            request,
                            text: body,
                        }
                    };
                    app.thinking = true;
                    app.invalidate_content();
                    app.observe(ObservedEvent {
                        revision: app.snapshot.revision + 1,
                        event,
                    });
                    terminal
                        .draw(|frame| super::super::render::draw(frame, &mut app))
                        .unwrap();
                    let mut elapsed = Vec::new();
                    for _ in 0..100 {
                        let start = Instant::now();
                        let event = if reasoning {
                            RuntimeEvent::ReasoningDelta {
                                agent: app.selected.clone(),
                                request,
                                text: "next word ".into(),
                            }
                        } else {
                            RuntimeEvent::TextDelta {
                                agent: app.selected.clone(),
                                request,
                                text: "next word ".into(),
                            }
                        };
                        app.observe(ObservedEvent {
                            revision: app.snapshot.revision + 1,
                            event,
                        });
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
            app.session.shutdown().await.unwrap();
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
            .run_script("return await tool.read({path:'example.rs'});")
            .await
            .unwrap();
        app.snapshot = app.session.observe().await.snapshot;
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
        let output = app.session.inspect_output(query).await.unwrap();
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
        let page = app.session.inspect_output(query).await.unwrap();
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
        app.session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn highlighting_real_job_survives_resize_and_preserves_session_bytes() {
        let (_root, mut app) = fixture().await;
        let source = "const value = {answer: 42};  \n\treturn value;\n";
        app.session.run_script(source.to_owned()).await.unwrap();
        app.snapshot = app.session.observe().await.snapshot;
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
            .inspect_output(JobOutputQuery::new(job))
            .await
            .unwrap();
        app.outputs.insert(job, output.clone());
        let journal = app
            .launch
            .sessions
            .join(app.session.id().to_string())
            .join("events.jsonl");
        let before = std::fs::read(&journal).unwrap();
        let records = serde_json::to_vec(&app.snapshot.records).unwrap();
        let mut arguments = super::super::tool_view::Document::default();
        arguments.arguments("script", &app.projection.jobs[&job].args);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            draw(&mut app);
            assert!(app.entries.iter().all(|entry| entry.document.is_none()));
            if app.render.highlights.is_highlighted(&arguments, app.light) {
                break;
            }
            assert!(Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let highlighted_arguments = arguments.lines(Some(&app.render.highlights), app.light);
        app.view().scroll = Some(0);
        draw(&mut app);
        let header = tool_hits(&app)[0];
        click(&mut app, header);
        let buffer = draw_buffer(&mut app);
        // The first expanded frame has its cached argument styles, without a timer tick.
        assert!(app.render.highlights.is_highlighted(&arguments, app.light));
        assert_eq!(
            arguments.lines(Some(&app.render.highlights), app.light),
            highlighted_arguments
        );
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
        let body = tool_hits(&app)[2];
        click(&mut app, body);
        draw(&mut app);
        assert!(app.entries.iter().all(|entry| entry.document.is_none()));
        assert_eq!(app.outputs[&job], output);
        assert_eq!(serde_json::to_vec(&app.snapshot.records).unwrap(), records);
        assert_eq!(std::fs::read(journal).unwrap(), before);
        app.session.shutdown().await.unwrap();
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
                indent: 0,
                job: None,
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
        app.session.shutdown().await.unwrap();
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
                    indent: 0,
                    job: None,
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
                app.session.shutdown().await.unwrap();
            }
        }
    }

    #[tokio::test]
    async fn tool_body_collapses_on_click_preserves_drag_and_never_highlights_spacer() {
        use skyhook::{
            provider::protocol::{AssistantContent, Message, ToolCall},
            session::{EventRecord, SessionEvent},
        };
        let (_root, mut app) = fixture().await;
        let sequence = app
            .snapshot
            .records
            .last_key_value()
            .map_or(1, |(seq, _)| seq + 1);
        app.snapshot.records.insert(
            sequence,
            EventRecord {
                version: 1,
                sequence,
                timestamp_millis: 0,
                agent: app.selected.clone(),
                event: SessionEvent::MessageCommitted {
                    message: Message::Assistant(vec![AssistantContent::ToolCall(ToolCall {
                        id: "fixture".into(),
                        name: "exec".into(),
                        arguments: serde_json::json!({"argv": ["echo", "hello"]}),
                    })]),
                },
            },
        );
        app.refresh();
        draw(&mut app);
        let header = tool_hits(&app)[0];
        click(&mut app, header);
        let buffer = draw_buffer(&mut app);
        let hits = tool_hits(&app);
        assert!(hits.len() > 2);
        assert!(app.entries[0].text.contains("1.  echo"));
        let spacer = hits.last().unwrap().y + 1;
        let base = super::super::render::Palette::new(false).base;
        assert!((0..60).all(|x| buffer[(x, spacer)].bg == base));
        // Releasing a click on any body row closes it; dragging selects instead.
        mouse(&mut app, hits[1], MouseEventKind::Down(MouseButton::Left));
        mouse(&mut app, hits[2], MouseEventKind::Drag(MouseButton::Left));
        mouse(&mut app, hits[2], MouseEventKind::Up(MouseButton::Left));
        draw(&mut app);
        assert_eq!(tool_hits(&app).len(), hits.len());
        click(&mut app, hits[2]);
        let buffer = draw_buffer(&mut app);
        assert_eq!(tool_hits(&app).len(), 1);
        assert!((0..60).all(|x| buffer[(x, header.y + 1)].bg == base));
        // Individual collapse also works after expanding everything via /details.
        app.command("details");
        draw(&mut app);
        let body = tool_hits(&app)[1];
        click(&mut app, body);
        draw(&mut app);
        assert_eq!(tool_hits(&app).len(), 1);
        app.session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn remote_tree_targets_survive_long_names_and_terminal_states() {
        let (_root, mut app) = fixture().await;
        let mut child = app.projection.agents[0].clone();
        child.id = child.id.child(1);
        child.name = "a very long worker name that must leave room for its target".into();
        child.target = "lab-monitoring".into();
        let id = child.id.clone();
        app.projection.agents.push(child);
        app.selected = id.clone();
        for terminal_state in [false, true] {
            app.projection.agents.last_mut().unwrap().terminal = terminal_state;
            for width in [40, 60, 120] {
                let mut terminal =
                    ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 24)).unwrap();
                terminal
                    .draw(|frame| super::super::render::draw(frame, &mut app))
                    .unwrap();
                let buffer = terminal.backend().buffer();
                for (rect, hit) in &app.hits {
                    if let Hit::Agent(agent) = hit {
                        let row = (0..width)
                            .map(|x| buffer[(x, rect.y)].symbol())
                            .collect::<String>();
                        if *agent == id {
                            assert!(row.contains("@lab-monitoring"), "{row}");
                        } else {
                            assert!(!row.contains('@'), "{row}");
                        }
                    }
                }
            }
        }
        app.session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn agent_tree_shows_individual_tokens_and_context_without_changing_session_totals() {
        use skyhook::{
            agent::{AgentActivity, ContextUsage},
            provider::protocol::Usage,
        };
        let (_root, mut app) = fixture().await;
        let root = app.selected.clone();
        let mut child = app.projection.agents[0].clone();
        child.id = root.child(1);
        child.name = "worker with a long descriptive name that needs to be clipped".into();
        child.terminal = false;
        let child_id = child.id.clone();
        app.projection.agents.push(child);
        app.projection.agent_usage.insert(
            root.clone(),
            Usage {
                output_tokens: 1000,
                input_tokens: 2000,
                cached_input_tokens: 3000,
            },
        );
        app.projection.agent_usage.insert(
            child_id.clone(),
            Usage {
                output_tokens: 4000,
                input_tokens: 5000,
                cached_input_tokens: 6000,
            },
        );
        app.projection.usage = Usage {
            output_tokens: 5000,
            input_tokens: 7000,
            cached_input_tokens: 9000,
        };
        app.snapshot.context.insert(
            root.clone(),
            ContextUsage {
                tokens: 10000,
                capacity: 100000,
            },
        );
        app.snapshot.context.insert(
            child_id.clone(),
            ContextUsage {
                tokens: 40000,
                capacity: 200000,
            },
        );
        app.snapshot
            .activity
            .insert(root.clone(), AgentActivity::Working);
        app.snapshot
            .activity
            .insert(child_id.clone(), AgentActivity::WaitingChildren);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 24)).unwrap();
        terminal
            .draw(|frame| super::super::render::draw(frame, &mut app))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let row_text = |y| {
            (0..120)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        };
        let mut status_columns = Vec::new();
        for (agent, expected, status) in [
            (&root, "1k · 5k(2k) · 10% (10k/100k)", "Working"),
            (
                &child_id,
                "4k · 11k(5k) · 20% (40k/200k)",
                "Waiting for child",
            ),
        ] {
            let rect = app
                .hits
                .iter()
                .find_map(|(rect, hit)| match hit {
                    Hit::Agent(id) if id == agent => Some(*rect),
                    _ => None,
                })
                .unwrap();
            let row = row_text(rect.y);
            assert!(row.contains(expected));
            let status_start = row.find(status).unwrap();
            let column = row[..status_start].chars().count() as u16;
            assert_eq!(buffer[(column - 1, rect.y)].symbol(), " ");
            assert_eq!(buffer[(column - 2, rect.y)].symbol(), " ");
            status_columns.push(column);
        }
        assert_eq!(status_columns[0], status_columns[1]);
        assert!(row_text(23).contains("5k · 16k(7k) · 10% (10k/100k)"));
        let state = model::entries(
            &app.snapshot,
            &app.projection,
            &child_id,
            &model::View {
                tab: model::Tab::State,
                ..Default::default()
            },
            &HashMap::new(),
            false,
            false,
        );
        assert!(state[0].text.contains("4k · 11k(5k) · 20% (40k/200k)"));
        assert_eq!(
            model::agent_footer(&app.snapshot, &app.projection, &root.child(2)),
            "0 · 0(0) · —"
        );
        app.session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn composer_has_empty_border_and_tree_stays_visible_in_child_views() {
        let (_root, mut app) = fixture().await;
        app.editor.set("draft".into());
        let buffer = draw_buffer(&mut app);
        assert_eq!(app.tree_rect.height, 0);
        let full_content_height = app.content_rect.height;
        assert!((0..60).all(|x| buffer[(x, app.composer_rect.y)].symbol() == " "));
        assert!(!draw(&mut app).contains("Message skyhook"));
        key(&mut app, KeyCode::Tab, M::NONE);
        assert!(app.focus == Focus::Content);
        key(&mut app, KeyCode::BackTab, M::SHIFT);
        assert!(app.focus == Focus::Composer);

        let mut child = app.projection.agents[0].clone();
        child.id = child.id.child(1);
        child.name = "worker".into();
        child.terminal = false;
        app.projection.agents.push(child.clone());
        let buffer = draw_buffer(&mut app);
        assert_eq!(app.tree_rect.height, 4);
        assert_eq!(app.content_rect.height, full_content_height - 4);
        for y in [app.tree_rect.y, app.tree_rect.bottom() - 1] {
            assert!((0..60).all(|x| buffer[(x, y)].symbol() == " "));
        }
        for y in app.tree_rect.y..app.tree_rect.bottom() {
            assert!(
                [0, 1, 58, 59]
                    .iter()
                    .all(|&x| buffer[(x, y)].symbol() == " ")
            );
        }
        let child_hit = app
            .hits
            .iter()
            .find_map(|(rect, hit)| match hit {
                Hit::Agent(id) if id == &child.id => Some(*rect),
                _ => None,
            })
            .unwrap();
        click(&mut app, child_hit);
        assert_eq!(app.selected, child.id);
        app.projection.agents.last_mut().unwrap().terminal = true;
        app.projection
            .completed
            .insert(child.id.clone(), Instant::now() - Duration::from_secs(3));
        app.focus = Focus::Tree;
        draw(&mut app);
        assert_eq!(app.tree_rect.height, 4);
        assert_eq!(app.content_rect.height, full_content_height - 4);
        assert!(app.focus == Focus::Tree);
        assert!(
            app.hits
                .iter()
                .any(|(_, hit)| matches!(hit, Hit::Agent(id) if id == &child.id))
        );
        let root_hit = app
            .hits
            .iter()
            .find_map(|(rect, hit)| match hit {
                Hit::Agent(id) if id.path().is_empty() => Some(*rect),
                _ => None,
            })
            .unwrap();
        click(&mut app, root_hit);
        assert!(app.selected.path().is_empty());
        app.focus = Focus::Tree;
        draw(&mut app);
        assert_eq!(app.tree_rect.height, 0);
        assert_eq!(app.content_rect.height, full_content_height);
        assert!(app.focus == Focus::Composer);
        assert!(!app.hits.iter().any(|(_, hit)| matches!(hit, Hit::Agent(_))));
        app.session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn paste_targets_search_and_overlay_without_changing_hidden_editors() {
        let (_root, mut app) = fixture().await;
        app.editor.set("preserved draft".into());
        app.focus = Focus::Content;
        key(&mut app, KeyCode::Char('/'), M::NONE);
        app.event(Event::Paste("needle".into()));
        assert_eq!(app.search_editor.as_ref().unwrap().text, "needle");
        assert_eq!(app.editor.text, "preserved draft");
        key(&mut app, KeyCode::Enter, M::NONE);
        assert_eq!(app.view().query, "needle");
        let _answer = question(&mut app, "Question".into(), vec![]);
        app.info("Prompt details", "detail needle".into());
        app.event(Event::Paste("detail".into()));
        assert_eq!(app.menu.as_ref().unwrap().input.text, "detail");
        assert!(app.prompt_editor.text.is_empty());
        key(&mut app, KeyCode::Esc, M::NONE);
        app.event(Event::Paste("custom answer".into()));
        assert_eq!(app.prompt_editor.text, "custom answer");
        assert_eq!(app.editor.text, "preserved draft");
        app.session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn queue_menu_tracks_dispatch_and_stale_actions_cannot_remove_another_message() {
        let (_root, mut app) = fixture().await;
        app.operation = true;
        app.submit("message A".into(), vec![]);
        app.submit("message B".into(), vec![]);
        app.command("queue");
        let stale_items = app.menu.as_ref().unwrap().items.clone();
        app.menu.as_mut().unwrap().selected = 1;
        app.operation = false;
        app.tick(); // Dispatch A while the queue menu remains open.
        assert_eq!(app.queue.len(), 1);
        let menu = app.menu.as_ref().unwrap();
        assert_eq!(menu.items.len(), 1);
        assert_eq!(menu.items[0].label, "message B");
        assert_eq!(menu.items[0].value, stale_items[1].value);
        // A stale rendered action still names A, never the shifted index of B.
        app.menu.as_mut().unwrap().items = stale_items;
        app.menu.as_mut().unwrap().selected = 0;
        key(&mut app, KeyCode::Delete, M::NONE);
        assert_eq!(app.queue.front().unwrap().text, "message B");
        key(&mut app, KeyCode::Enter, M::NONE);
        assert_eq!(app.editor.text, "message B");
        assert!(app.queue.is_empty());
        app.session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn long_questions_and_late_choices_are_readable_and_submittable() {
        let (_root, mut app) = fixture().await;
        app.editor.set("preserved draft".into());
        let body = (0..25)
            .map(|i| format!("Question line {i:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        let options = (0..10)
            .map(|i| QuestionOption {
                label: format!("Choice {i}"),
                description: format!("{} END-{i}", "Long description ".repeat(12)),
            })
            .collect();
        let answer = question(&mut app, body, options);
        assert!(draw(&mut app).contains("Question line 00"));
        for _ in 0..20 {
            key(&mut app, KeyCode::PageDown, M::NONE);
        }
        assert!(draw(&mut app).contains("Question line 24"));
        for _ in 0..9 {
            key(&mut app, KeyCode::Down, M::NONE);
        }
        assert!(draw(&mut app).contains("> Choice 9"));
        key(&mut app, KeyCode::Down, M::NONE);
        assert!(draw(&mut app).contains("> Write an answer"));
        key(&mut app, KeyCode::Up, M::NONE);
        draw(&mut app);
        for _ in 0..10 {
            key(&mut app, KeyCode::PageDown, M::CONTROL);
        }
        assert!(draw(&mut app).contains("END-9"));
        key(&mut app, KeyCode::Enter, M::NONE);
        assert!(matches!(answer.await.unwrap().unwrap(),
            PromptResponse::Questions(value) if value == Value::String("Choice 9".into())));
        assert_eq!(app.editor.text, "preserved draft");
        app.session.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn resize_preserves_semantic_entries_and_reflows_rows() {
        let (_root, mut app) = fixture().await;
        use skyhook::{
            provider::protocol::{AssistantContent, Message},
            session::EventRecord,
        };
        let sequence = app.snapshot.records.last_key_value().unwrap().0 + 1;
        app.snapshot.records.insert(
            sequence,
            EventRecord {
                version: 1,
                sequence,
                timestamp_millis: 0,
                agent: app.selected.clone(),
                event: SessionEvent::MessageCommitted {
                    message: Message::Assistant(vec![AssistantContent::Text {
                        text: "A visible message wrapping differently at narrow widths. ".repeat(8),
                    }]),
                },
            },
        );
        app.refresh();
        draw(&mut app);
        let revision = app.content_revision;
        let entries = app.entries.as_ptr();
        let original_rows = app.content_rows;
        app.event(Event::Resize(30, 24));
        assert!(!app.content_dirty);
        assert_eq!(app.content_revision, revision);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(30, 24)).unwrap();
        terminal
            .draw(|frame| super::super::render::draw(frame, &mut app))
            .unwrap();
        assert_eq!(app.entries.as_ptr(), entries);
        assert!(app.content_rows > original_rows);
        app.session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn command_palette_uses_effective_bindings() {
        let (_root, mut app) = fixture().await;
        app.keys = KeyMap::new(
            &[
                ("models".into(), "ctrl+g".into()),
                ("exit".into(), String::new()),
            ]
            .into(),
        )
        .unwrap();
        app.command("commands");
        let items = &app.menu.as_ref().unwrap().items;
        assert_eq!(
            items
                .iter()
                .find(|item| item.value == "model")
                .unwrap()
                .detail,
            app.keys.binding("model")
        );
        assert_eq!(app.keys.binding("model"), "Ctrl+G");
        assert!(
            items
                .iter()
                .find(|item| item.value == "exit")
                .unwrap()
                .detail
                .is_empty()
        );
        app.session.shutdown().await.unwrap();
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
        app.session.shutdown().await.unwrap();
    }
}
