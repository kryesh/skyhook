use super::*;

const MAX_FILE_MENU_ITEMS: usize = 10_000;
const OUTPUT_SEARCH_CONTEXT: usize = 2;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MenuId(u64);

// Only asynchronous menu kinds can cross the Work boundary. A completion names
// the menu it was loaded for (and, for output, its job) and only fills that
// menu while it is still the open one of the same kind.
pub enum MenuLoaded {
    Sessions(MenuId, Result<Vec<Item<SessionId>>, String>),
    Files(MenuId, Result<Vec<Item<PathBuf>>, String>),
    Output(MenuId, JobId, Result<Vec<Item<OutputAction>>, String>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ConfirmationChoice {
    KeepWorking,
    Proceed,
}

/// A draft item in the attachments menu: pasted text by id, or an attachment by index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DraftItem {
    Paste(usize),
    Attachment(usize),
}
#[derive(Clone)]
pub enum ConfirmAction {
    Exit,
    NewSession,
    SwitchSession(SessionId),
    CancelJob(JobId),
}
#[derive(Clone)]
pub struct Item<T> {
    pub value: T,
    pub label: String,
    /// Searchable metadata, rendered separately from the label.
    pub detail: String,
}
impl<T> Item<T> {
    pub(super) fn new(value: T, label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            value,
            label: label.into(),
            detail: detail.into(),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutputAction {
    Automatic,
    Field(String),
    Search,
    Next,
}
#[derive(Clone)]
pub enum MenuKind {
    Commands(Vec<Item<Command>>),
    Models(Vec<Item<String>>),
    Agents(Vec<Item<AgentId>>),
    Sessions(Vec<Item<SessionId>>),
    /// Workspace files, plus the composer offset just after the `@` that opened
    /// the picker. A chosen file replaces that `@`; cancelling keeps it.
    Files(Vec<Item<PathBuf>>, Option<usize>),
    Attachments(Vec<Item<DraftItem>>),
    Queue(Vec<Item<QueuedInputId>>),
    Confirm(ConfirmAction, Vec<Item<ConfirmationChoice>>),
    Output(JobId, Vec<Item<OutputAction>>),
    OutputSearch(JobId),
    Info(Vec<Item<()>>),
}
/// Rendering and filtering borrow only presentation data; selection stays typed.
pub struct ItemRef<'a> {
    pub index: usize,
    pub label: &'a str,
    pub detail: &'a str,
}
impl MenuKind {
    pub fn items(&self) -> Vec<ItemRef<'_>> {
        fn rows<T>(items: &[Item<T>]) -> Vec<ItemRef<'_>> {
            items
                .iter()
                .enumerate()
                .map(|(index, item)| ItemRef {
                    index,
                    label: &item.label,
                    detail: &item.detail,
                })
                .collect()
        }
        match self {
            Self::Commands(items) => rows(items),
            Self::Models(items) => rows(items),
            Self::Agents(items) => rows(items),
            Self::Sessions(items) => rows(items),
            Self::Files(items, _) => rows(items),
            Self::Attachments(items) => rows(items),
            Self::Queue(items) => rows(items),
            Self::Confirm(_, items) => rows(items),
            Self::Output(_, items) => rows(items),
            Self::Info(items) => rows(items),
            Self::OutputSearch(_) => vec![],
        }
    }
}
pub struct Menu {
    pub(super) id: MenuId,
    pub title: String,
    pub kind: MenuKind,
    pub input: Editor,
    pub selected: usize,
}
impl Menu {
    /// Replace the rows of an open menu, keeping the selected value selected.
    pub(super) fn replace_items<T: Clone + PartialEq>(
        &mut self,
        kind: MenuKind,
        items: fn(&MenuKind) -> Option<&[Item<T>]>,
    ) {
        let selected = self
            .selected_index()
            .and_then(|index| items(&self.kind).map(|items| items[index].value.clone()));
        self.kind = kind;
        self.selected = selected
            .and_then(|value| {
                let items = items(&self.kind)?;
                self.filtered()
                    .iter()
                    .position(|row| items[row.index].value == value)
            })
            .unwrap_or(0);
    }

    pub fn filtered(&self) -> Vec<ItemRef<'_>> {
        let query = self.input.text().to_lowercase();
        let commands = if let MenuKind::Commands(items) = &self.kind {
            Some(items)
        } else {
            None
        };
        let query = if commands.is_some() {
            query.trim_start_matches('/')
        } else {
            &query
        };
        let query = commands
            .and_then(|_| query.parse::<Command>().ok())
            .map_or(query, |command| command.id());
        let mut items: Vec<_> = self
            .kind
            .items()
            .into_iter()
            .filter(|item| {
                commands.is_some_and(|commands| commands[item.index].value.id().contains(query))
                    || format!("{} {}", item.label, item.detail)
                        .to_lowercase()
                        .contains(query)
            })
            .collect();
        if let Some(commands) = commands {
            // An advertised /resume must select that action, not Resume session.
            items.sort_by_key(|item| commands[item.index].value.id() != query);
        }
        items
    }
    pub(super) fn selected_index(&self) -> Option<usize> {
        self.filtered().get(self.selected).map(|item| item.index)
    }
}

async fn load_sessions(root: PathBuf) -> Result<Vec<Item<SessionId>>, String> {
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
        let Ok(summary) = SessionStore::summary(&root, id).await else {
            continue;
        };
        let title = summary
            .title
            .or_else(|| {
                summary
                    .preview
                    .map(|text| crate::tui::format::brief(&text, 100))
            })
            .unwrap_or_else(|| id.to_string());
        sessions.push((
            summary.last_millis,
            Item::new(id, title, format!("{} events · {id}", summary.entries)),
        ));
    }
    sessions.sort_by_key(|(timestamp, _)| std::cmp::Reverse(*timestamp));
    Ok(sessions.into_iter().map(|(_, item)| item).collect())
}
fn walk_files(root: &std::path::Path, path: &std::path::Path, items: &mut Vec<Item<PathBuf>>) {
    if items.len() >= MAX_FILE_MENU_ITEMS {
        return;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        if items.len() >= MAX_FILE_MENU_ITEMS {
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
                .to_path_buf();
            let label = relative.display().to_string();
            items.push(Item::new(relative, label, ""));
        }
    }
}

impl App {
    pub(super) fn agent_items(&self) -> Vec<Item<AgentId>> {
        self.projection
            .agents
            .iter()
            .map(|agent| {
                Item::new(
                    agent.id.clone(),
                    format!(
                        "{}{}{}",
                        "    ".repeat(agent.id.depth()),
                        agent.name,
                        model::target_suffix(&agent.target)
                    ),
                    format!(
                        "{}   {}",
                        self.agent_status(agent).label(),
                        model::agent_footer(&self.snapshot, &self.projection, &agent.id)
                    ),
                )
            })
            .collect()
    }
    pub fn refresh_agent_menu(&mut self) {
        if !matches!(
            &self.menu,
            Some(Menu {
                kind: MenuKind::Agents(_),
                ..
            })
        ) {
            return;
        }
        let items = self.agent_items();
        self.menu
            .as_mut()
            .unwrap()
            .replace_items(MenuKind::Agents(items), |kind| match kind {
                MenuKind::Agents(items) => Some(items),
                _ => None,
            });
    }
    pub(super) fn open(&mut self, title: &str, kind: MenuKind) {
        self.next_menu_id.0 = self.next_menu_id.0.wrapping_add(1);
        self.menu = Some(Menu {
            id: self.next_menu_id,
            title: title.into(),
            kind,
            input: Editor::default(),
            selected: 0,
        });
    }
    /// Open the workspace file picker; `at` follows an `@` typed in the composer.
    pub(super) fn open_files(&mut self, at: Option<usize>) {
        self.open("Attach workspace file", MenuKind::Files(vec![], at));
        let id = self.menu.as_ref().unwrap().id;
        let root = self.launch.workspace.clone();
        let tx = self.tx.clone();
        tokio::task::spawn_blocking(move || {
            let mut items = vec![];
            walk_files(&root, &root, &mut items);
            items.sort_by(|a, b| a.label.cmp(&b.label));
            let _ = tx.send(Work::MenuLoaded(MenuLoaded::Files(id, Ok(items))));
        });
    }
    pub(super) fn menu_loaded(&mut self, loaded: MenuLoaded) -> bool {
        let Some(menu) = self.menu.as_mut() else {
            return false;
        };
        let (id, result) = match (loaded, &menu.kind) {
            (MenuLoaded::Sessions(id, result), MenuKind::Sessions(_)) => {
                (id, result.map(MenuKind::Sessions))
            }
            (MenuLoaded::Files(id, result), &MenuKind::Files(_, at)) => {
                (id, result.map(|items| MenuKind::Files(items, at)))
            }
            // The menu id is minted per open, so it already identifies the job.
            (MenuLoaded::Output(id, job, result), MenuKind::Output(..)) => {
                (id, result.map(|items| MenuKind::Output(job, items)))
            }
            _ => return false,
        };
        if id != menu.id {
            return false;
        }
        match result {
            Ok(kind) => menu.kind = kind,
            Err(error) => self.notice(error),
        }
        true
    }

    pub(super) fn info(&mut self, title: &str, text: String) {
        self.open(
            title,
            MenuKind::Info(text.lines().map(|line| Item::new((), line, "")).collect()),
        );
    }
    pub(super) fn confirm(&mut self, action: ConfirmAction) {
        self.open(
            "Confirm action",
            MenuKind::Confirm(
                action,
                vec![
                    Item::new(ConfirmationChoice::KeepWorking, "Keep working", ""),
                    Item::new(ConfirmationChoice::Proceed, "Stop work and continue", ""),
                ],
            ),
        );
    }
    pub fn command(&mut self, command: Command) {
        match command {
            Command::Commands => self.open(
                "Commands", MenuKind::Commands(COMMANDS.iter().filter(|spec| spec.palette)
                    .map(|spec| Item::new(spec.command, spec.label, self.keys.binding(spec.command).unwrap_or_default())).collect()),
            ),
            Command::Model => {
                self.open(
                "Model", MenuKind::Models(
                self.launch.model.config().config().models.iter().map(|(name, profile)| {
                    Item::new(name.clone(), name, format!("{} · {}", profile.provider, profile.model))
                }).collect()),
                );
                if let Some(menu) = &mut self.menu
                    && let MenuKind::Models(items) = &menu.kind
                {
                    menu.selected = items.iter().position(|item| item.value == self.model).unwrap_or(0);
                }
            },
            Command::Agents => self.open("Agents", MenuKind::Agents(self.agent_items())),
            Command::Inspect => {
                self.focus = Focus::Content;
                self.view().tab = Tab::Conversation;
            }
            Command::Jobs | Command::Requests => {
                self.focus = Focus::Content;
                self.view().tab = match command {
                    Command::Jobs => Tab::Jobs,
                    _ => Tab::Requests,
                };
                self.view().scroll = None;
            }
            Command::Thinking => self.thinking = !self.thinking,
            Command::Details => {
                self.details = !self.details;
                if self.details { for view in self.views.values_mut() { view.clear_collapsed(); } }
            }
            Command::Copy => self.copy(),
            Command::Attention => self.activate_prompt(),
            Command::Resume => {
                if self.queue_requires_recovery() || self.queue_scan == queue::QueueScan::Failed {
                    // Delivery stays blocked until the journal resolves each row.
                    self.paused = false;
                    match self.session().cloned() {
                        Some(session) => {
                            self.request_queue_recovery(&session);
                            self.notice("Reconciling queued input with the session journal…");
                        }
                        None => {
                            self.paused = true;
                            self.notice("Queued input requires recovery; it has not been retried");
                        }
                    }
                    return;
                }
                self.paused = false;
                // Not busy means no creation is in flight, so only a parked retry can be taken.
                if !self.busy()
                    && let StartState::RetryScript(path) = std::mem::take(&mut self.start)
                {
                    self.start_script(path);
                }
                self.notice("Queued input resumed");
            }
            Command::Retry => {
                if self.start.is_creating() || self.stopping || self.switching.is_some()
                    || !self.snapshot.activity.values().any(|activity|
                        matches!(activity, AgentActivity::Failed(_) | AgentActivity::Interrupted))
                {
                    self.notice("No failed or interrupted turns to retry");
                    return;
                }
                let Some(session) = self.session().cloned() else { return; };
                // Only own a new root operation if the old one has already ended.
                // Resuming suspended children must leave a waiting parent alone.
                let owns_operation = !self.operation && matches!(
                    self.snapshot.activity.get(self.root_agent()),
                    Some(AgentActivity::Failed(_) | AgentActivity::Interrupted),
                );
                if owns_operation { self.operation = true; }
                // A pending model choice is otherwise captured only by a submitted
                // message. Forward it here too: a refusal repeats deterministically
                // on the same model, so "swap then continue" must actually swap.
                let active = self
                    .projection
                    .agents
                    .iter()
                    .find(|agent| &agent.id == self.root_agent())
                    .map(|agent| agent.model.clone());
                let model = (active.as_ref() != Some(&self.model)).then(|| self.model.clone());
                let tx = self.tx.clone();
                let notices = self.root_notifier();
                let requested_model = model.clone();
                tokio::spawn(async move {
                    let outcome = session
                        .continue_turn_with(skyhook::agent::ContinueOptions { model })
                        .await;
                    let result = match &outcome {
                        // Only a continued root turn adopts a model change, and the
                        // gate above admits historical failures too, so report what
                        // actually happened rather than appearing to have retried.
                        Ok(outcome) if outcome.is_empty() => {
                            notices.send(
                                "Nothing to continue: no retained turn is failed or interrupted"
                                    .to_owned(),
                            );
                            Ok(())
                        }
                        Ok(outcome) => {
                            if requested_model.is_some() && !outcome.model_applied {
                                notices.send(format!(
                                    "Continued {} child agent(s) on their own model; a model change applies to the root turn only",
                                    outcome.children_resumed,
                                ));
                            }
                            Ok(())
                        }
                        Err(error) => Err(error.to_string()),
                    };
                    if owns_operation {
                        let _ = tx.send(Work::Done { session: session.id(), result });
                    } else if let Err(error) = result {
                        notices.send(error);
                    }
                });
            }
            Command::Queue => self.open(
                "Queued follow-ups · Enter edit · Delete remove", MenuKind::Queue(self.queue_items()),
            ),
            Command::Attachments => {
                let mut items: Vec<_> = self.editor.pastes().map(|(id, text)| Item::new(
                    DraftItem::Paste(id),
                    format!("Pasted text · {} lines", text.lines().count()),
                    crate::tui::format::brief(text, 60),
                )).collect();
                items.extend(self.editor.attachments().iter().enumerate().map(|(i, attachment)| {
                    let kind = match attachment {
                        Attachment::Text { .. } => "Text",
                        Attachment::Image { .. } => "Image",
                    };
                    let source = attachment.file().map_or_else(|| kind.to_owned(), |file| file.display().to_string());
                    Item::new(DraftItem::Attachment(i), source, kind)
                }));
                self.open("Attachments · Enter inspect · Delete remove", MenuKind::Attachments(items));
            }
            Command::New => {
                if self.active_work() { self.confirm(ConfirmAction::NewSession); }
                else { self.switch(None); }
            }
            Command::Exit => {
                if self.active_work() { self.confirm(ConfirmAction::Exit); }
                else { self.shutdown(); }
            }
            Command::Child => {
                if let Some(agent) = self.projection.agents.iter().find(|agent| {
                    agent.id.parent().as_ref() == Some(&self.selected)
                }) {
                    self.select(agent.id.clone());
                }
            }
            Command::Parent => {
                if let Some(parent) = self.selected.parent() { self.select(parent); }
            }
            Command::Sessions => {
                let root = self.launch.sessions.clone();
                let tx = self.tx.clone();
                self.open("Resume session", MenuKind::Sessions(vec![]));
                let id = self.menu.as_ref().unwrap().id;
                tokio::spawn(async move {
                    let result = load_sessions(root).await;
                    let _ = tx.send(Work::MenuLoaded(MenuLoaded::Sessions(id, result)));
                });
            }
            Command::Files => self.open_files(None),
            Command::Export => {
                let entries = model::entries(
                    &self.snapshot, &self.projection, &self.selected,
                    &View::default(), &self.outputs, self.thinking, true,
                );
                let text = entries.iter().map(|entry| entry.text()).collect::<Vec<_>>().join("\n\n");
                let Some(session) = self.session() else {
                    self.notice("No session to export yet");
                    return;
                };
                let path = session.directory().join(format!(
                    "conversation-{}.md", crate::tui::format::agent_label(&self.selected).replace(':', "-"),
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
            Command::Help => self.info("Skyhook help", format!(concat!(
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
                "Model changes apply from the next submitted message, or from continuing a failed root turn. Instruction changes apply to new sessions.\n",
                "Mouse: click agent or tool, scroll, drag text then copy.\n",
                "The workspace and session ID are plain text; use terminal selection to copy them.\n",
                "Copy message uses the terminal clipboard (OSC 52).",
            ), self.keys.help())),
        }
        if matches!(
            command,
            Command::Inspect
                | Command::Jobs
                | Command::Requests
                | Command::Thinking
                | Command::Details
        ) {
            self.invalidate_content();
        }
        self.dirty = true;
    }
    pub(super) fn menu_key(&mut self, mut key: KeyEvent) {
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
                if let Some(menu) = &self.menu {
                    match &menu.kind {
                        MenuKind::Attachments(items) => {
                            if let Some(index) = menu.selected_index() {
                                match items[index].value {
                                    DraftItem::Paste(id) => {
                                        self.editor.remove_paste(id);
                                    }
                                    DraftItem::Attachment(index) => {
                                        self.editor.remove_attachment(index);
                                    }
                                }
                            }
                            self.command(Command::Attachments);
                        }
                        MenuKind::Queue(items) => {
                            if let Some(index) = menu.selected_index() {
                                self.remove_queued(items[index].value);
                                self.refresh_queue_menu();
                            }
                        }
                        _ => {}
                    }
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
    pub(super) fn choose(&mut self) {
        let Some(menu) = self.menu.take() else { return };
        let selected = menu.selected_index();
        match menu.kind {
            MenuKind::Commands(items) => {
                if let Some(index) = selected {
                    self.command(items[index].value);
                }
            }
            MenuKind::Models(items) => {
                if let Some(index) = selected {
                    match self.launch.model.config().select_model(&items[index].value) {
                        Ok(model) => {
                            self.model = model.name().to_owned();
                            self.launch.model = model;
                        }
                        Err(error) => self.notice(error.to_string()),
                    }
                }
            }
            MenuKind::Agents(items) => {
                if let Some(index) = selected {
                    self.select(items[index].value.clone());
                }
            }
            MenuKind::Sessions(items) => {
                if let Some(index) = selected {
                    let id = items[index].value;
                    if self.active_work() {
                        self.confirm(ConfirmAction::SwitchSession(id));
                    } else {
                        self.switch(Some(id));
                    }
                }
            }
            MenuKind::Files(items, at) => {
                let Some(index) = selected else { return };
                let root = self.launch.workspace.clone();
                let path = root.join(&items[index].value);
                let tx = self.tx.clone();
                let draft = self.draft_ticket.clone();
                tokio::spawn(async move {
                    let result = crate::launch::read_attachment(&root, &path).await;
                    let _ = tx.send(Work::File { draft, at, result });
                });
            }
            MenuKind::Attachments(items) => {
                if let Some(index) = selected {
                    match items[index].value {
                        DraftItem::Paste(id) => {
                            if let Some(text) = self.editor.paste(id) {
                                self.info("Pasted text", text.to_owned());
                            }
                        }
                        DraftItem::Attachment(index) => {
                            match self.editor.attachments().get(index).cloned() {
                                Some(Attachment::Text { file, content }) => {
                                    let title = file.map_or_else(
                                        || "Text attachment".to_owned(),
                                        |file| file.display().to_string(),
                                    );
                                    self.info(&title, content)
                                }
                                Some(Attachment::Image { file, image }) => {
                                    let source = file.map_or_else(
                                        || "pasted image".to_owned(),
                                        |file| file.display().to_string(),
                                    );
                                    let format = image.format().media_type();
                                    self.info("Image attachment", format!("{source} · {format}"))
                                }
                                None => {}
                            }
                        }
                    }
                }
            }
            MenuKind::Queue(items) => {
                if let Some(index) = selected
                    && let Some(queued) = self.remove_queued(items[index].value)
                {
                    self.pause_queue();
                    if !self.editor.is_empty() {
                        let draft = self.editor.take();
                        let draft = self.queued_input(draft);
                        self.queue.push_front(draft);
                    }
                    self.replace_draft(queued.submission);
                }
            }
            MenuKind::Confirm(action, items) => {
                if selected.is_some_and(|index| items[index].value == ConfirmationChoice::Proceed) {
                    match action {
                        ConfirmAction::Exit => self.shutdown(),
                        ConfirmAction::NewSession => self.switch(None),
                        ConfirmAction::SwitchSession(id) => self.switch(Some(id)),
                        ConfirmAction::CancelJob(id) => {
                            let Some(session) = self.session().cloned() else {
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
            MenuKind::Output(job, items) => {
                let Some(index) = selected else { return };
                match &items[index].value {
                    OutputAction::Automatic => {
                        self.outputs.clear_query(job);
                        self.fetch_output(job);
                    }
                    OutputAction::Search => {
                        self.open("Search saved output (regex)", MenuKind::OutputSearch(job));
                    }
                    OutputAction::Next => {
                        if let Some((field, start, offset)) =
                            self.outputs.get(&job).and_then(|view| view.continuation())
                        {
                            let mut query = self
                                .outputs
                                .query(job)
                                .cloned()
                                .unwrap_or_else(|| JobOutputQuery::new(job));
                            query.field = Some(field.to_owned());
                            query.start = Some(start);
                            query.offset = (offset != 0).then_some(offset);
                            self.set_output_query(query);
                        }
                    }
                    OutputAction::Field(field) => {
                        let mut query = JobOutputQuery::new(job);
                        query.field = Some(field.clone());
                        self.set_output_query(query);
                    }
                }
            }
            MenuKind::OutputSearch(job) => {
                let field = self
                    .outputs
                    .query(job)
                    .and_then(|q| q.field.clone())
                    .unwrap_or_default();
                let mut query = JobOutputQuery::new(job);
                query.field = Some(field);
                query.pattern = Some(menu.input.text().to_owned());
                query.context = Some(OUTPUT_SEARCH_CONTEXT);
                self.set_output_query(query);
            }
            MenuKind::Info(_) => {
                self.menu = Some(menu);
            }
        }
    }
    pub(super) fn output_menu(&mut self) {
        let row = self.view().row;
        if let Some(job) = self.entries().get(row).and_then(|e| e.job_id()) {
            self.open(
                "Saved output",
                MenuKind::Output(job, output_items(Vec::new())),
            );
            let Some(session) = self.session().cloned() else {
                return;
            };
            let id = self.menu.as_ref().unwrap().id;
            let tx = self.tx.clone();
            tokio::spawn(async move {
                // Discover saved pointers, not paths through presentation-only
                // wrappers or a currently selected single-field page.
                let result = session
                    .inspect_output_fields(job)
                    .await
                    .map(output_items)
                    .map_err(|error| error.to_string());
                let _ = tx.send(Work::MenuLoaded(MenuLoaded::Output(id, job, result)));
            });
        }
    }
}

fn output_items(fields: Vec<String>) -> Vec<Item<OutputAction>> {
    let mut items: Vec<_> = fields
        .into_iter()
        .map(|field| Item::new(OutputAction::Field(field.clone()), field, ""))
        .collect();
    items.extend([
        Item::new(
            OutputAction::Automatic,
            "automatic output",
            "structured result and live captures",
        ),
        Item::new(OutputAction::Field(String::new()), "complete result", ""),
        Item::new(OutputAction::Search, "Search this field", "regex"),
        Item::new(OutputAction::Next, "Next page", ""),
    ]);
    items
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::*;
    use crate::tui::tool_view::OutputView;

    #[cfg(unix)]
    #[tokio::test]
    async fn file_menu_preserves_non_unicode_paths_and_attaches_images() {
        use std::os::unix::ffi::OsStringExt;
        let (_root, mut app) = draft_fixture().await;
        let workspace = app.launch.workspace.clone();
        let relative = PathBuf::from(std::ffi::OsString::from_vec(b"file-\xff.txt".to_vec()));
        std::fs::write(workspace.join(&relative), "file contents").unwrap();
        let bytes = b"\x89PNG\r\n\x1a\nfixture";
        std::fs::write(workspace.join("shot.png"), bytes).unwrap();
        let mut items = vec![];
        walk_files(&workspace, &workspace, &mut items);
        let mut rx = capture_work(&mut app);
        let canonical = |path: &PathBuf| Some(workspace.join(path).canonicalize().unwrap());
        let text = Attachment::Text {
            file: canonical(&relative),
            content: "file contents".into(),
        };
        let image = PathBuf::from("shot.png");
        let png = Attachment::Image {
            file: canonical(&image),
            image: skyhook::media::Image::new(bytes.to_vec()).unwrap(),
        };
        for (path, expected) in [(relative, text), (image, png)] {
            app.open("Files", MenuKind::Files(items.clone(), None));
            let selected = items.iter().position(|item| item.value == path);
            app.menu.as_mut().unwrap().selected = selected.unwrap();
            app.choose();
            let Work::File { result, .. } = recv(&mut rx).await else {
                panic!("file read")
            };
            assert_eq!(result.unwrap(), expected);
        }
    }

    #[tokio::test]
    async fn typed_at_sign_is_kept_unless_a_file_replaces_it() {
        let (_root, mut app) = draft_fixture().await;
        std::fs::write(app.launch.workspace.join("notes.txt"), "notes").unwrap();
        let mut rx = capture_work(&mut app);
        app.editor.set("mail user".into());
        key(&mut app, KeyCode::Char('@'), M::SHIFT);
        assert!(matches!(
            app.menu.as_ref().unwrap().kind,
            MenuKind::Files(..)
        ));
        key(&mut app, KeyCode::Esc, M::NONE);
        assert!(app.menu.is_none());
        assert_eq!(app.editor.text(), "mail user@");
        key(&mut app, KeyCode::Char('@'), M::SHIFT);
        app.menu.as_mut().unwrap().input.set("notes".into());
        while app.menu.as_ref().unwrap().filtered().is_empty() {
            let work = recv(&mut rx).await;
            app.work(work);
        }
        key(&mut app, KeyCode::Enter, M::NONE);
        while app.editor.attachments().is_empty() {
            let work = recv(&mut rx).await;
            app.work(work);
        }
        // The chosen file consumes only the `@` that opened its picker.
        assert_eq!(app.editor.text(), "mail user@");
        assert!(matches!(
            app.editor.attachments(),
            [Attachment::Text { file: Some(_), content }] if content == "notes"
        ));
        assert!(!app.editor.has_pastes());
    }

    #[tokio::test]
    async fn filtered_queue_selection_pauses_and_preserves_the_current_draft() {
        let (_root, mut app) = draft_fixture().await;
        for text in ["first", "second"] {
            let attachments = vec![png_attachment(text)];
            let text = text.into();
            let queued = app.queued_input(Submission { text, attachments });
            app.queue.push_back(queued);
        }
        app.editor.set("draft".into());
        app.editor.attach(png_attachment("draft.png"));
        app.command(Command::Queue);
        app.menu.as_mut().unwrap().input.set("second".into());
        app.choose();
        assert!(app.paused);
        assert_eq!(app.editor.text(), "second");
        assert_eq!(app.editor.attachments(), [png_attachment("second")]);
        let texts: Vec<_> = app
            .queue
            .iter()
            .map(|input| &input.submission.text)
            .collect();
        assert_eq!(texts, ["draft", "first"]);
        assert_eq!(
            app.queue[0].submission.attachments,
            [png_attachment("draft.png")]
        );
        app.command(Command::Queue);
        app.menu.as_mut().unwrap().input.set("first".into());
        key(&mut app, KeyCode::Delete, M::NONE);
        assert_eq!(app.queue.len(), 1);
        assert!(app.paused);
        app.command(Command::Resume);
        assert!(!app.paused);
    }

    #[tokio::test]
    async fn output_menu_uses_saved_pointers_from_a_paged_script_view() {
        let (_root, mut app) = fixture().await;
        std::fs::write(app.launch.workspace.join("child.txt"), "child data").unwrap();
        run_script(&mut app, "console.log('hello'); return {custom: {'a/b~c': [42]}, child: await tool.read({path: 'child.txt'})};").await;
        app.command(Command::Details);
        draw(&mut app);
        let job = job_named(&app, "script");
        select_job(&mut app, job);
        let mut query = JobOutputQuery::new(job);
        query.field = Some("/result/console".into());
        let session = app.session().unwrap().clone();
        let page = session.inspect_output(query.clone()).await.unwrap();
        assert!(page.get("result").is_none());
        app.outputs
            .insert_product(job, OutputView::historical(page.clone()));
        app.outputs.set_query(query);
        let mut rx = capture_work(&mut app);
        app.output_menu();
        let work = recv(&mut rx).await;
        assert!(matches!(
            work,
            Work::MenuLoaded(MenuLoaded::Output(_, _, Ok(_)))
        ));
        app.work(work);
        let menu = app.menu.as_mut().unwrap();
        let MenuKind::Output(_, items) = &menu.kind else {
            panic!("output menu")
        };
        let fields: Vec<_> = items
            .iter()
            .filter_map(|item| match &item.value {
                OutputAction::Field(field) => Some(field.as_str()),
                _ => None,
            })
            .collect();
        assert!(fields.contains(&"/result/value/child/content"));
        assert!(!fields.contains(&"/result/value/child/result/content"));
        assert!(!fields.contains(&"/result/value/child/id"));
        // Discovery must not replace the displayed page.
        assert_eq!(app.outputs.get(&job).unwrap().value(), &page);
        assert_eq!(
            app.outputs.query(job).unwrap().field.as_deref(),
            Some("/result/console")
        );
        let custom = "/result/value/custom/a~1b~0c/0";
        let selected = items
            .iter()
            .position(|item| item.value == OutputAction::Field(custom.into()));
        menu.selected = selected.unwrap();
        app.choose();
        assert_eq!(
            app.outputs.query(job).unwrap().field.as_deref(),
            Some(custom)
        );
        let query = app.outputs.query(job).unwrap().clone();
        let output = session.inspect_output(query).await.unwrap();
        assert_eq!(output["preview"]["lines"], json!(["42"]));
    }

    async fn wait_for_failures(
        session: &SessionHandle,
        child_job: JobId,
        count: usize,
    ) -> ObservationSnapshot {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let snapshot = session.observe().await.snapshot;
                let failures = snapshot.records.values().filter(|record| {
                    matches!(record.event, SessionEvent::JobFinished {
                        job, state: skyhook::job::JobState::Failed, ..
                    } if job == child_job)
                });
                // The live job settles just after its journal commit; a retry
                // decided before then would find the child still running.
                let settled = session
                    .inspect_jobs(session.root_agent())
                    .await
                    .iter()
                    .any(|job| job.id == child_job && job.state == skyhook::job::JobState::Failed);
                if failures.count() == count && settled {
                    break snapshot;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("child failures observed within ten seconds")
    }

    #[tokio::test]
    async fn retry_children_without_selection_preserves_the_waiting_root_operation() {
        let (_root, mut app) = permanent_failure_fixture().await;
        let session = app.session().cloned().unwrap();
        let script =
            "return await tool.agent({prompt:'Fail against the fixture provider', bg:true});";
        let launched = session.run_script(script).await.unwrap();
        let child: JobId = serde_json::from_value(launched.value["value"]["id"].clone()).unwrap();
        app.snapshot = wait_for_failures(&session, child, 1).await;
        let root = session.root_agent().clone();
        let waiting = AgentActivity::WaitingChildren;
        app.snapshot.activity.insert(root.clone(), waiting.clone());
        assert_eq!(app.selected, root);
        app.operation = true;
        let mut rx = capture_work(&mut app);
        assert!(app.busy());

        app.command(Command::Retry);
        let snapshot = wait_for_failures(&session, child, 2).await;
        assert!(!snapshot.records.values().any(|record| {
            record.agent == root && matches!(record.event, SessionEvent::ModelRequested { .. })
        }));
        // Child retry neither replaces nor finishes the pending root operation.
        assert!(app.operation);
        assert_eq!(app.snapshot.activity.get(&root), Some(&waiting));
        while let Ok(work) = rx.try_recv() {
            assert!(!matches!(work, Work::Done { .. }));
            app.work(work);
        }
        assert!(app.operation);
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn command_menu_shows_default_shortcuts_separately_and_searchably() {
        let (_root, mut app) = draft_fixture().await;
        app.command(Command::Commands);
        let menu = app.menu.as_mut().unwrap();
        let MenuKind::Commands(items) = &menu.kind else {
            panic!("command menu")
        };
        let palette = COMMANDS.iter().filter(|spec| spec.palette);
        let expected: Vec<_> = palette
            .map(|spec| {
                let binding = app.keys.binding(spec.command).unwrap_or_default();
                (spec.command, spec.label.to_string(), binding)
            })
            .collect();
        let shown: Vec<_> = items
            .iter()
            .map(|item| (item.value, item.label.clone(), item.detail.clone()))
            .collect();
        assert_eq!(shown, expected);
        for (command, label, shortcut) in [
            (Command::New, "New session", "Ctrl+X N"),
            (Command::Jobs, "Agent jobs", ""),
            (Command::Model, "Model", "Ctrl+X M"),
        ] {
            let item = items.iter().find(|item| item.value == command).unwrap();
            assert_eq!(
                (item.label.as_str(), item.detail.as_str()),
                (label, shortcut)
            );
        }
        for (query, expected) in [
            ("CTRL+X N", Command::New),
            ("new SESSION", Command::New),
            ("ctrl+x m", Command::Model),
            ("attention", Command::Attention),
            ("/ATTENTION", Command::Attention),
            ("retry", Command::Retry),
            ("thinking", Command::Thinking),
            ("sessions", Command::Sessions),
            ("agents", Command::Agents),
            ("exit", Command::Exit),
        ] {
            menu.input.set(query.into());
            let filtered = menu.filtered();
            assert_eq!(filtered.len(), 1, "query: {query}");
            assert_eq!(items[filtered[0].index].value, expected);
        }
        menu.input.set("/models".into());
        assert_eq!(items[menu.filtered()[0].index].value, Command::Model);
        for spec in COMMANDS.iter().filter(|spec| spec.palette) {
            menu.input.set(format!("/{}", spec.command));
            assert_eq!(items[menu.filtered()[0].index].value, spec.command);
        }
        // Typing /resume must run the queue action, not open Resume session.
        app.menu = None;
        app.paused = true;
        key(&mut app, KeyCode::Char('/'), M::NONE);
        let typed: Vec<_> = "resume".chars().map(KeyCode::Char).collect();
        press(&mut app, &typed);
        key(&mut app, KeyCode::Enter, M::NONE);
        assert!(!app.paused);
        assert!(app.menu.is_none());
    }

    fn menu_completion(menu: &Menu, error: Option<&str>) -> Work {
        fn result<T>(value: T, error: Option<&str>) -> Result<Vec<Item<T>>, String> {
            match error {
                Some(error) => Err(error.into()),
                None => Ok(vec![Item::new(value, "current", "loaded detail")]),
            }
        }
        Work::MenuLoaded(match &menu.kind {
            MenuKind::Sessions(_) => {
                MenuLoaded::Sessions(menu.id, result(SessionId::from_bytes([1; 16]), error))
            }
            MenuKind::Files(..) => {
                MenuLoaded::Files(menu.id, result(PathBuf::from("current"), error))
            }
            MenuKind::Output(job, _) => {
                MenuLoaded::Output(menu.id, *job, result(OutputAction::Automatic, error))
            }
            _ => panic!("not an asynchronous menu"),
        })
    }

    #[tokio::test]
    async fn menu_loads_only_fill_the_originating_open_menu() {
        let (_root, mut app) = draft_fixture().await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        app.status = crate::tui::status::StatusLog::new(tx);
        for kind in [
            MenuKind::Sessions(vec![]),
            MenuKind::Files(vec![], None),
            MenuKind::Output(JobId::new(42).unwrap(), vec![]),
        ] {
            app.open("Loading", kind.clone());
            let complete = |app: &App, error| menu_completion(app.menu.as_ref().unwrap(), error);
            let closed_success = complete(&app, None);
            let closed_failure = complete(&app, Some("obsolete failure"));
            let stale_success = complete(&app, None);
            key(&mut app, KeyCode::Esc, M::NONE);
            app.dirty = false;
            app.work(closed_success);
            assert!(app.menu.is_none());
            assert!(!app.dirty);

            app.open("Newer request", kind);
            app.menu.as_mut().unwrap().input.insert("query");
            app.work(closed_failure);
            app.work(stale_success);
            assert!(app.menu.as_ref().unwrap().kind.items().is_empty());
            assert!(!app.dirty);
            // A current failure keeps the menu and reports a notice.
            app.work(complete(&app, Some("current failure")));
            assert_eq!(app.menu.as_ref().unwrap().title, "Newer request");
            assert!(app.menu.as_ref().unwrap().kind.items().is_empty());
            app.status.flush().await;
            assert!(
                matches!(rx.try_recv().unwrap(), Work::StatusFailed { message, .. } if message == "current failure")
            );
            let (current, replaced) = (complete(&app, None), complete(&app, None));
            app.work(current);
            let menu = app.menu.as_ref().unwrap();
            assert_eq!(menu.kind.items()[0].label, "current");
            assert_eq!(menu.input.text(), "query");

            app.info("Replacement overlay", "Keep me".into());
            app.work(replaced);
            assert_eq!(app.menu.as_ref().unwrap().title, "Replacement overlay");
        }
        app.status.flush().await;
        assert!(
            rx.try_recv().is_err(),
            "stale failures must not emit notices"
        );
    }

    #[tokio::test]
    async fn palette_hover_owns_selection_without_background_or_stationary_updates() {
        let (_root, mut app) = fixture().await;
        // The palette's selected value must belong to the admitted catalog.
        let mut config = app.launch.model.config().config().clone();
        for name in ["second", "third"] {
            config
                .models
                .insert(name.into(), config.models["first"].clone());
        }
        app.launch.model = config
            .into_runtime()
            .unwrap()
            .select_model("first")
            .unwrap();
        let models = |labels: [(&str, &str); 3]| {
            let items = labels.map(|(value, label)| Item::new(value.into(), label, ""));
            MenuKind::Models(items.into())
        };
        let row = |app: &mut App, index| {
            draw(app);
            let mut hits = app.hits.iter();
            hits.find_map(|(rect, hit)| matches!(hit, Hit::Menu(i) if *i == index).then_some(*rect))
                .unwrap()
        };
        app.open(
            "Models",
            models([("first", "First"), ("second", "Second"), ("third", "Third")]),
        );
        let second = row(&mut app, 1);
        let selected = |app: &App| app.menu.as_ref().unwrap().selected;
        mouse(&mut app, second, MouseEventKind::Moved);
        assert_eq!(selected(&app), 1);
        key(&mut app, KeyCode::Down, M::NONE);
        assert_eq!(selected(&app), 2);
        app.dirty = false;
        mouse(&mut app, second, MouseEventKind::Moved);
        assert_eq!(selected(&app), 2);
        assert!(!app.dirty);
        // A physical move inside the same row can take over from the keyboard.
        let moved = Rect {
            x: second.x + 1,
            ..second
        };
        mouse(&mut app, moved, MouseEventKind::Moved);
        assert_eq!(selected(&app), 1);
        let selected_agent = app.selected.clone();
        app.dirty = false;
        let tree_rect = app.tree_rect;
        mouse(&mut app, tree_rect, MouseEventKind::Moved);
        assert_eq!(selected(&app), 1);
        assert_eq!(app.selected, selected_agent);
        assert!(!app.dirty);
        key(&mut app, KeyCode::Enter, M::NONE);
        assert!(app.menu.is_none());
        assert_eq!(app.model, "second");
        let filtered = [
            ("hidden", "Hidden"),
            ("first", "Visible first"),
            ("second", "Visible second"),
        ];
        app.open("Models", models(filtered));
        app.menu.as_mut().unwrap().input.set("Visible".into());
        let visible_second = row(&mut app, 1);
        mouse(&mut app, visible_second, MouseEventKind::Moved);
        key(&mut app, KeyCode::Enter, M::NONE);
        assert_eq!(app.model, "second");
        app.session().unwrap().shutdown().await.unwrap();
    }
}
