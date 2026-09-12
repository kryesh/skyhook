use super::*;

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
    pub(super) fn new(
        value: impl Into<String>,
        label: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
            detail: detail.into(),
            attachment: None,
        }
    }
    pub(super) fn attachment(
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
    pub(super) id: u64,
    pub title: String,
    pub kind: MenuKind,
    pub items: Vec<Item>,
    pub input: Editor,
    pub selected: usize,
}
impl Menu {
    pub fn filtered(&self) -> Vec<&Item> {
        let query = self.input.text.to_lowercase();
        let commands = matches!(self.kind, MenuKind::Commands);
        let query = if commands {
            query.trim_start_matches('/')
        } else {
            &query
        };
        let query = if commands && query == "models" {
            "model"
        } else {
            query
        };
        let mut items: Vec<_> = self
            .items
            .iter()
            .filter(|i| {
                (commands && i.value.contains(query))
                    || format!("{} {}", i.label, i.detail)
                        .to_lowercase()
                        .contains(query)
            })
            .collect();
        if commands {
            // An advertised /resume must select that action, not Resume session.
            items.sort_by_key(|item| item.value != query);
        }
        items
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
                                Some(crate::tui::format::brief(text, 100))
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

impl App {
    pub(super) fn agent_items(&self) -> Vec<Item> {
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
    pub(super) fn preview_theme(&mut self) {
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
    pub(super) fn open(&mut self, title: &str, kind: MenuKind, items: Vec<Item>) {
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
    pub(super) fn info(&mut self, title: &str, text: String) {
        self.open(
            title,
            MenuKind::Info,
            text.lines().map(|line| Item::new("", line, "")).collect(),
        );
    }
    pub(super) fn confirm(&mut self, action: ConfirmAction) {
        self.open(
            "Confirm action",
            MenuKind::Confirm(action),
            vec![
                Item::new("no", "Keep working", ""),
                Item::new("yes", "Stop work and continue", ""),
            ],
        );
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
                if self.creating || self.stopping || self.switch_restore.is_some()
                    || !self.snapshot.activity.values().any(|activity|
                        matches!(activity, AgentActivity::Failed(_) | AgentActivity::Interrupted))
                {
                    self.notice("No failed or interrupted turns to retry");
                    return;
                }
                let Some(session) = self.session.clone() else { return; };
                // Only own a new root operation if the old one has already ended.
                // Resuming suspended children must leave a waiting parent alone.
                let owns_operation = !self.operation && matches!(
                    self.snapshot.activity.get(self.root_agent()),
                    Some(AgentActivity::Failed(_) | AgentActivity::Interrupted),
                );
                if owns_operation { self.operation = true; }
                let tx = self.tx.clone();
                let notices = self.root_notifier();
                tokio::spawn(async move {
                    let result = session.continue_turn().await.map(|_| ()).map_err(|e| e.to_string());
                    if owns_operation {
                        let _ = tx.send(Work::Done { session: session.id(), result });
                    } else if let Err(error) = result {
                        notices.send(error);
                    }
                });
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
                    crate::tui::format::brief(text, 60),
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
                "Copy message uses the terminal clipboard (OSC 52).\n\n",
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
    pub(super) fn choose(&mut self) {
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
    pub(super) fn output_menu(&mut self) {
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

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::*;
    async fn wait_for_failures(
        session: &SessionHandle,
        child_job: JobId,
        count: usize,
    ) -> ObservationSnapshot {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let snapshot = session.observe().await.snapshot;
                let failures = snapshot
                    .records
                    .values()
                    .filter(|record| {
                        matches!(record.event, SessionEvent::JobFinished {
                        job, state: skyhook::job::JobState::Failed, ..
                    } if job == child_job)
                    })
                    .count();
                if failures == count {
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
        let session = app.session.clone().unwrap();
        let launched = session
            .run_script(
                "return await tool.agent({prompt:'Fail against the fixture provider', bg:true});",
            )
            .await
            .unwrap();
        let child_job: JobId =
            serde_json::from_value(launched.value["value"]["id"].clone()).unwrap();
        let failed_snapshot = wait_for_failures(&session, child_job, 1).await;
        let root = session.root_agent().clone();
        app.snapshot = failed_snapshot;
        app.snapshot
            .activity
            .insert(root.clone(), AgentActivity::WaitingChildren);
        assert_eq!(app.selected, root);
        app.operation = true;
        let (tx, mut rx) = mpsc::unbounded_channel();
        app.tx = tx;
        assert!(app.busy());

        app.command("retry");
        let snapshot = wait_for_failures(&session, child_job, 2).await;
        assert!(!snapshot.records.values().any(|record| {
            record.agent == root && matches!(record.event, SessionEvent::ModelRequested { .. })
        }));
        assert!(
            app.operation,
            "child retry does not replace the pending root operation"
        );
        assert_eq!(
            app.snapshot.activity.get(&root),
            Some(&AgentActivity::WaitingChildren)
        );
        while let Ok(work) = rx.try_recv() {
            assert!(
                !matches!(work, Work::Done { .. }),
                "child retry must not finish the root operation"
            );
            app.work(work);
        }
        assert!(app.operation);
        session.shutdown().await.unwrap();
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
        for (query, expected) in [
            ("ALT+N", "new"),
            ("new SESSION", "new"),
            ("alt+m", "model"),
            ("attention", "attention"),
            ("/ATTENTION", "attention"),
            ("retry", "retry"),
            ("thinking", "thinking"),
            ("sessions", "sessions"),
            ("agents", "agents"),
            ("themes", "themes"),
            ("exit", "exit"),
        ] {
            menu.input.text = query.into();
            let filtered = menu.filtered();
            assert_eq!(filtered.len(), 1, "query: {query}");
            assert_eq!(filtered[0].value, expected);
        }
        menu.input.text = "/models".into();
        assert_eq!(menu.filtered()[0].value, "model");
        menu.input.text = "ctrl+x n".into();
        assert!(
            menu.filtered().is_empty(),
            "overridden defaults must not remain searchable"
        );
        for (id, _, _) in COMMANDS {
            if hidden.contains(id) {
                continue;
            }
            menu.input.text = format!("/{id}");
            assert_eq!(menu.filtered()[0].value, *id, "exact command: {id}");
        }
        // Typing /resume must run the queue action, not open Resume session.
        app.menu = None;
        app.paused = true;
        key(&mut app, KeyCode::Char('/'), M::NONE);
        for c in "resume".chars() {
            key(&mut app, KeyCode::Char(c), M::NONE);
        }
        key(&mut app, KeyCode::Enter, M::NONE);
        assert!(!app.paused);
        assert!(app.menu.is_none());
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
}
