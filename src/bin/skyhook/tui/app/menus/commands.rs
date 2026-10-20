//! Commands, from the palette, a shortcut or a typed `/command`.
use super::{ConfirmAction, Item, MenuKind};
use crate::state;
use crate::tui::app::events::load_output;
use crate::tui::app::output::MAX_LOADS;
use crate::tui::app::{App, Focus, HostRequest, Operation, OutputStore, Work};
use crate::tui::keys::{COMMANDS, Command};
use crate::tui::model::{self, Tab, View};
use futures_util::StreamExt;
use skyhook::job::JobOutputQuery;

impl App {
    pub fn command(&mut self, command: Command) {
        match command {
            Command::Commands => {
                let palette = COMMANDS.iter().filter(|spec| spec.palette);
                let items = palette.map(|spec| {
                    let binding = self.keys.binding(spec.command).unwrap_or_default();
                    Item::columned(spec.command, spec.label, "", vec![binding])
                });
                self.open("Commands", MenuKind::Commands(items.collect()));
            }
            Command::Model => {
                let models = self.launch.model.config().models();
                let items: Vec<_> = models
                    .map(|(name, profile)| {
                        Item::new(name.clone(), name.to_string(), profile.model.clone())
                    })
                    .collect();
                let selected = items.iter().position(|item| item.value == self.model);
                self.open("Model", MenuKind::Models(items));
                self.menu.as_mut().unwrap().select(selected.unwrap_or(0));
            }
            Command::Mode => {
                let items: Vec<_> = self
                    .modes()
                    .iter()
                    .map(|(name, mode)| {
                        let capabilities = mode.capabilities.iter().map(|c| c.as_str());
                        let capabilities = capabilities.collect::<Vec<_>>().join(" ");
                        Item::new(name.clone(), name.as_str(), capabilities)
                    })
                    .collect();
                let selected = items.iter().position(|item| item.value == self.mode);
                self.open("Mode", MenuKind::Modes(items));
                self.menu.as_mut().unwrap().select(selected.unwrap_or(0));
            }
            Command::Agents => self.open("Agents", MenuKind::Agents(self.agent_items())),
            Command::Inspect => {
                self.focus = Focus::Content;
                self.tab = Tab::Conversation;
            }
            Command::PreviousTab | Command::NextTab => {
                self.set_tab(self.tab.next(command == Command::PreviousTab));
            }
            Command::PreviousAgent | Command::NextAgent => {
                self.step_agent(command == Command::PreviousAgent);
            }
            Command::Details => {
                self.details = !self.details;
                if self.details {
                    self.views.clear_collapsed();
                }
            }
            Command::Copy => self.copy(),
            Command::Attention => self.activate_prompt(),
            Command::Resume => {
                self.discard_edit();
                self.paused = None;
                self.toast("Queued input resumed");
            }
            Command::Retry => self.retry(),
            Command::Queue => self.open(
                "Queued follow-ups · Enter edit · Delete remove",
                MenuKind::Queue(self.queue_items()),
            ),
            Command::Attachments => self.open(
                "Attachments · Enter inspect · Delete remove",
                MenuKind::Attachments(self.attachment_items()),
            ),
            Command::New => self.host = Some(HostRequest::New),
            Command::Close => {
                if self.stopping {
                    return;
                }
                if self.active_work() {
                    self.confirm(ConfirmAction::CloseSession);
                } else {
                    self.shutdown();
                }
            }
            Command::Exit => {
                if self.active_work() || self.peers.iter().any(|peer| peer.working) {
                    self.confirm(ConfirmAction::Exit);
                } else {
                    self.host = Some(HostRequest::Quit);
                }
            }
            Command::Child => {
                let mut agents = self.projection.agents.iter();
                if let Some(agent) =
                    agents.find(|agent| agent.id.parent().as_ref() == Some(&self.selected))
                {
                    self.select(agent.id.clone());
                }
            }
            Command::Parent => {
                if let Some(parent) = self.selected.parent() {
                    self.select(parent);
                }
            }
            Command::Sessions => self.open_sessions(),
            Command::Rename => self.rename_session(),
            Command::Files => self.open_files(None),
            Command::Sidebar => {
                self.sidebar = !self.sidebar;
                let (sidebar, tx) = (self.sidebar, self.tx.clone());
                let workspace = self.launch.workspace.clone();
                tokio::task::spawn_blocking(move || {
                    if let Err(error) = state::update(&workspace, |state| state.sidebar = sidebar) {
                        let message = format!("Could not save sidebar setting: {error}");
                        let _ = tx.send(Work::Toast(message));
                    }
                });
            }
            Command::Export => self.export(),
            Command::Help => self.info(
                "Skyhook help",
                format!(
                    concat!(
                        "{}\n\n",
                        "Tab / Shift+Tab: next/previous mode in the composer, row in the tree or conversation; click a pane to focus it\n",
                        "Enter: send / queue / expand\n",
                        "Alt+Enter / Ctrl+J: newline\n",
                        "PageUp / PageDown: scroll\n",
                        "Content: [ ] inspector tabs\n",
                        "Ctrl+A/E: line start/end; Ctrl+W: delete word; Ctrl+U/K: delete to line boundary\n",
                        "Ctrl+- / Ctrl+.: undo/redo\n\n",
                        "Footer: session output · total input(uncached) · reported context (current/capacity)\n",
                        "Context belongs to the selected agent and includes system, tools and runtime state.\n",
                        "Messages always go to skyhook, including while viewing a child.\n",
                        "Model and mode changes apply from the next submitted message, or from continuing a failed root turn. Instruction changes apply to new sessions.\n",
                        "Mouse: click agent or tool, scroll, drag text to copy it.\n",
                        "The workspace and session ID are plain text; use terminal selection to copy them.\n",
                        "Copy message uses the terminal clipboard (OSC 52).",
                    ),
                    self.keys.help()
                ),
            ),
        }
        if matches!(command, Command::Inspect | Command::Details) {
            self.invalidate_content();
        }
        self.dirty = true;
    }

    fn retry(&mut self) {
        // A viewed session reads its root as it closed; only activation, which
        // continuing runs first, decides whether it resumes, as it does for
        // deliveries pending at close.
        let viewed = self.snapshot.phase == skyhook::session::SessionPhase::Viewing;
        let retryable = viewed || model::continuable(&self.snapshot);
        if self.creating.is_some() || self.stopping {
            self.toast("No failed or interrupted turns to retry");
            return;
        }
        // An open edit holds every queued and held row: only a turn that holds
        // none goes on.
        let editing = matches!(self.paused, Some(crate::tui::app::Paused::Editing { .. }));
        if editing {
            self.toast("Submit the message you are editing, or resume the queue, to retry it");
            if !retryable || self.holds_rows() {
                return;
            }
        }
        // Rows a failure or an interrupt handed back are sent again too.
        let resumed = self.resume_stopped_queue();
        if !retryable {
            return self.toast(match resumed {
                true => "Queued input resumed",
                false => "No failed or interrupted turns to retry",
            });
        }
        let Some(session) = self.session().cloned() else {
            return;
        };
        // Only own a new root operation if the old one has already ended.
        // Resuming suspended children must leave a waiting parent alone.
        let operation = if !self.operation && (viewed || self.root_interrupted()) {
            self.operation = true;
            Operation::Owned
        } else {
            Operation::Joined
        };
        // A pending model or mode choice is otherwise captured only by a submitted
        // message. Forward it here too: a refusal repeats deterministically
        // on the same model, so "swap then continue" must actually swap. Rows the
        // session holds retry under it whatever the turn runs.
        let held = self.holds_rows();
        let mut agents = self.projection.agents.iter();
        let active = agents.find(|agent| &agent.id == self.root_agent());
        let model = active.map(|agent| &agent.model);
        let model = (held || model != Some(&self.model)).then(|| self.model.clone());
        let mode = active.and_then(|agent| agent.mode.as_ref());
        let mode = (held || mode != Some(&self.mode)).then(|| self.mode.clone());
        let tx = self.tx.clone();
        let notices = self.root_notifier();
        if !editing {
            self.toast("Continue requested");
        }
        let requested = model.is_some() || mode.is_some();
        tokio::spawn(async move {
            let read = session.store().applied();
            let outcome = match session.selection(model.as_ref(), mode.as_ref()) {
                Ok(selection) => session.continue_turn_with(selection).await,
                Err(error) => Err(error),
            };
            let result = match outcome {
                // Only a continued root turn adopts a model change, and the
                // gate above admits historical failures too, so report what
                // actually happened rather than appearing to have retried.
                Ok(outcome) if outcome.is_empty() => {
                    notices.send(
                        "Nothing to continue: no retained turn is failed or interrupted".to_owned(),
                    );
                    Ok(())
                }
                Ok(outcome) => {
                    if requested && !outcome.selection_applied {
                        notices.send(format!(
                            "Continued {} child agent(s) unchanged; a model or mode change applies to the root turn only",
                            outcome.children_resumed,
                        ));
                    }
                    Ok(())
                }
                // The root turn it continued failed: the journal shows that.
                Err(error) => match session
                    .store()
                    .failed_after(session.root_agent(), read)
                    .await
                {
                    Ok(true) => Ok(()),
                    Ok(false) | Err(_) => Err(error.into()),
                },
            };
            let _ = tx.send(Work::Continued { operation, result });
        });
    }

    /// Export the selected agent's conversation with every detail: its whole
    /// transcript read page by page, then the output of each job card the app does not
    /// hold, written off the event loop from copies of what the app holds now. It is
    /// work in hand until it reports.
    fn export(&mut self) {
        let Some(session) = self.session().cloned() else {
            self.toast("No session to export yet");
            return;
        };
        let agent = self.selected.clone();
        let mut chosen = self.outputs.chosen();
        let (live, projection) = (self.snapshot.clone(), self.projection.clone());
        let today = self.clock.day();
        let label = crate::tui::format::agent_label(&agent).replace(':', "-");
        let path = session
            .store()
            .directory()
            .join(format!("conversation-{label}.md"));
        let (tx, exporting) = (self.tx.clone(), self.exports.clone());
        tokio::spawn(async move {
            let window = match super::super::pages::whole(&session, &agent).await {
                Ok(window) => window,
                Err(error) => {
                    let _ = tx.send(Work::Toast(error.to_string()));
                    return;
                }
            };
            let (mut held, mut queries) = (Vec::new(), Vec::new());
            for job in window.jobs() {
                match chosen.remove(&job) {
                    Some(Ok(output)) => held.push((job, output)),
                    Some(Err(query)) => queries.push(query),
                    None => queries.push(JobOutputQuery::new(job)),
                }
            }
            let loads = futures_util::stream::iter(queries).map(|query| {
                let session = &session;
                async move { (query.job, load_output(session, query).await) }
            });
            let loaded = loads.buffer_unordered(MAX_LOADS).collect::<Vec<_>>().await;
            let outputs: OutputStore = held.into_iter().chain(loaded).collect();
            let write = move || {
                let view = model::EntryView {
                    agent: &window.agent,
                    tab: Tab::Conversation,
                    view: &View::default(),
                    all_details: true,
                    today,
                };
                let inputs = model::Inputs {
                    live: &live,
                    window: &window,
                    projection: &projection,
                    presentation: view,
                    outputs: &outputs,
                    unsaved: &[],
                };
                let entries = model::entries(inputs, true);
                let text = entries.iter().map(|entry| entry.text());
                std::fs::write(&path, text.collect::<Vec<_>>().join("\n\n"))
                    .map(|_| format!("Exported {}", path.display()))
            };
            let message = match tokio::task::spawn_blocking(write).await {
                Ok(Ok(message)) => message,
                Ok(Err(error)) => error.to_string(),
                Err(error) => error.to_string(),
            };
            drop(exporting);
            let _ = tx.send(Work::Toast(message));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::tests::*;
    use super::*;
    use crate::tui::app::played::{observe_until, played_fixture};
    use crossterm::event::{KeyCode, KeyModifiers as M};
    use skyhook::identity::JobId;
    use skyhook::session::SessionEvent;

    #[tokio::test]
    async fn retry_children_without_selection_preserves_the_waiting_root_operation() {
        let (_root, mut app, mut model, session) = played_fixture().await;
        // A terminal failure, unlike a refused connection, which retries.
        let rejected = || {
            let kind = skyhook::provider::ProviderErrorKind::Authentication;
            kind.error("fixture credentials rejected")
        };
        // A foreground child has settled once the script returns, so retry finds it failed.
        let script = "return await tool.agent({prompt:'Fail against the fixture provider'});";
        let launched = tokio::spawn({
            let session = session.clone();
            async move { session.run_script(script).await }
        });
        model.turn().await.fail(rejected());
        let launched = bounded(launched).await.unwrap().unwrap();
        let child: JobId = serde_json::from_value(launched.value["value"]["id"].clone()).unwrap();
        let failed = |record: &skyhook::session::EventRecord| {
            matches!(record.event, SessionEvent::JobFinished {
                job, state: skyhook::job::JobEnd::Failed, ..
            } if job == child)
        };
        observe_until(&mut app, |app| {
            app.journal.iter().any(|record| failed(record))
        })
        .await;
        let mut updates = session.store().observe().updates;
        let root = session.root_agent().clone();
        let waiting = skyhook::agent::ObservedActivity {
            state: skyhook::agent::AgentActivity::WaitingChildren,
            since: 0,
        };
        app.snapshot.activity.insert(root.clone(), waiting.clone());
        assert_eq!(app.selected, root);
        app.operation = true;
        let mut rx = capture_work(&mut app);
        assert!(app.busy());

        app.command(Command::Retry);
        model.turn().await.fail(rejected());
        crate::tests::bounded(async {
            loop {
                let update = updates.recv().await.unwrap();
                if matches!(&update.event, skyhook::agent::RuntimeEvent::Record(record) if failed(record)) {
                    break;
                }
            }
        })
        .await;
        // The child's failure doesn't mean the detached continue task has finished;
        // its last work item does.
        let continued = bounded(async {
            loop {
                match rx.recv().await.unwrap() {
                    work @ Work::Continued { .. } => break work,
                    work => {
                        assert!(!matches!(work, Work::Done { .. }));
                        app.work(work);
                    }
                }
            }
        })
        .await;
        assert!(matches!(
            continued,
            Work::Continued {
                operation: Operation::Joined,
                result: Ok(()),
            }
        ));
        app.work(continued);
        let requested = |record: &std::sync::Arc<skyhook::session::EventRecord>| {
            record.agent == root && matches!(record.event, SessionEvent::ModelRequested { .. })
        };
        assert!(!journal(&session).await.iter().any(requested));
        // Child retry neither replaces nor finishes the pending root operation.
        assert!(app.operation);
        assert_eq!(app.snapshot.activity.get(&root), Some(&waiting));
        session.shutdown().await.unwrap();
    }

    /// On a viewed session /retry asks the runtime, which activates it and alone
    /// knows whether anything resumes: here nothing does, and it says so.
    #[tokio::test]
    async fn retry_on_a_viewed_session_reaches_the_runtime() {
        use skyhook::session::SessionPhase;
        let (_root, app, _model, session) = played_fixture().await;
        session.close().await.unwrap();
        let viewed = app.launch.create(Some(session.id())).await.unwrap();
        let prepared = prepared(viewed.clone()).await;
        let (tx, mut work) = tokio::sync::mpsc::unbounded_channel();
        let mut app = app.sibling(Some(prepared), app.launch.clone(), tx);
        assert_eq!(app.snapshot.phase, SessionPhase::Viewing);
        app.command(Command::Retry);
        bounded(async {
            loop {
                match work.recv().await.unwrap() {
                    Work::Continued { result, .. } => break assert!(result.is_ok()),
                    work => app.work(work),
                }
            }
        })
        .await;
        app.status.flush().await;
        let nothing = "Nothing to continue: no retained turn is failed or interrupted";
        let records = journal(&viewed).await;
        assert!(records.iter().any(|record| matches!(
            &record.event,
            SessionEvent::Status { message } if message == nothing
        )));
        viewed.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn command_menu_shows_default_shortcuts_separately_and_searchably() {
        let (_root, mut app) = draft_fixture().await;
        app.command(Command::Commands);
        let menu = app.menu.as_mut().unwrap();
        let MenuKind::Commands(items) = &menu.kind else {
            panic!("command menu")
        };
        for (command, label, shortcut) in [
            (Command::New, "New session", "Ctrl+X N"),
            (Command::Resume, "Resume queued input", ""),
        ] {
            let item = items.iter().find(|item| item.value == command).unwrap();
            assert_eq!(
                (item.label.as_str(), item.columns.as_slice()),
                (label, &[shortcut.to_owned()][..])
            );
        }
        for (query, expected) in [
            ("CTRL+X N", Command::New),
            ("new SESSION", Command::New),
            ("ctrl+x m", Command::Model),
            ("attention", Command::Attention),
            ("/ATTENTION", Command::Attention),
            ("retry", Command::Retry),
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
        // Typing a command id runs that command.
        app.menu = None;
        app.paused = Some(crate::tui::app::Paused::Stopped);
        key(&mut app, KeyCode::Char('/'), M::NONE);
        let typed: Vec<_> = "resume".chars().map(KeyCode::Char).collect();
        press(&mut app, &typed);
        key(&mut app, KeyCode::Enter, M::NONE);
        assert!(!app.paused.is_some());
        assert!(app.menu.is_none());
    }

    /// An export holds the output of every job, including those never shown.
    #[tokio::test]
    async fn export_loads_the_output_of_jobs_never_shown() {
        let (_root, mut app, session) = approving_fixture().await;
        // The command never spells out what it prints, which only its output holds.
        run_script(
            &mut app,
            "return await tool.exec({command: \"printf 'out%s' put-exported\"});",
        )
        .await;
        let mut work = capture_work(&mut app);
        app.command(Command::Export);
        assert!(!app.settled(), "an export in flight keeps the session");
        let Work::Toast(message) = recv(&mut work).await else {
            panic!("the export reports where it wrote");
        };
        let path = message.strip_prefix("Exported ").expect(&message);
        let text = std::fs::read_to_string(path).unwrap();
        assert!(text.contains("output-exported"), "{text}");
        assert!(app.settled());
        session.shutdown().await.unwrap();
    }
}
