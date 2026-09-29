//! Commands, from the palette, a shortcut or a typed `/command`.
use super::{ConfirmAction, Item, MenuKind};
use crate::state;
use crate::tui::app::{App, Focus, HostRequest, Operation, StartState, Work};
use crate::tui::keys::{COMMANDS, Command};
use crate::tui::model::{self, Tab, View};

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
                self.menu_mut().unwrap().selected = selected.unwrap_or(0);
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
                self.menu_mut().unwrap().selected = selected.unwrap_or(0);
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
                    for view in self.views.values_mut() {
                        view.clear_collapsed();
                    }
                }
            }
            Command::Copy => self.copy(),
            Command::Attention => self.activate_prompt(),
            Command::Resume => {
                self.paused = false;
                // Not busy means no creation is in flight, so only a parked retry can be taken.
                if !self.busy()
                    && let StartState::RetryScript(path) = std::mem::take(&mut self.start)
                {
                    self.start_script(path);
                }
                self.notice("Queued input resumed");
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
                let (sidebar, notices) = (self.sidebar, self.root_notifier());
                let workspace = self.launch.workspace.clone();
                tokio::task::spawn_blocking(move || {
                    if let Err(error) = state::update(&workspace, |state| state.sidebar = sidebar) {
                        notices.send(format!("Could not save sidebar setting: {error}"));
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
                        "Content: / search; n/N next/previous; [ ] inspector tabs\n",
                        "Ctrl+A/E: line start/end; Ctrl+W: delete word; Ctrl+U/K: delete to line boundary\n",
                        "Ctrl+- / Ctrl+.: undo/redo\n\n",
                        "Footer: session output · total input(uncached) · estimated context (current/capacity)\n",
                        "Context belongs to the selected agent and includes system, tools and runtime state.\n",
                        "Messages always go to skyhook, including while viewing a child.\n",
                        "Model and mode changes apply from the next submitted message, or from continuing a failed root turn. Instruction changes apply to new sessions.\n",
                        "Mouse: click agent or tool, scroll, drag text then copy.\n",
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
        if self.start.is_creating()
            || self.stopping
            || !self
                .snapshot
                .activity
                .values()
                .any(|activity| activity.state.is_retryable())
        {
            self.notice("No failed or interrupted turns to retry");
            return;
        }
        let Some(session) = self.session().cloned() else {
            return;
        };
        // Only own a new root operation if the old one has already ended.
        // Resuming suspended children must leave a waiting parent alone.
        let operation = if !self.operation && self.root_interrupted() {
            self.operation = true;
            Operation::Owned
        } else {
            Operation::Joined
        };
        // A pending model or mode choice is otherwise captured only by a submitted
        // message. Forward it here too: a refusal repeats deterministically
        // on the same model, so "swap then continue" must actually swap.
        let mut agents = self.projection.agents.iter();
        let active = agents.find(|agent| &agent.id == self.root_agent());
        let model = active.and_then(|agent| agent.model.as_ref());
        let model = (model != Some(&self.model)).then(|| self.model.clone());
        let mode = active.and_then(|agent| agent.mode.as_ref());
        let mode = (mode != Some(&self.mode)).then(|| self.mode.clone());
        let tx = self.tx.clone();
        let notices = self.root_notifier();
        notices.send("Continue requested");
        let requested = model.is_some() || mode.is_some();
        tokio::spawn(async move {
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
                Err(error) => Err(error.into()),
            };
            let _ = tx.send(Work::Continued { operation, result });
        });
    }

    fn export(&mut self) {
        let view = model::EntryView {
            agent: &self.selected,
            tab: Tab::Conversation,
            view: &View::default(),
            all_details: true,
            today: self.clock.day(),
        };
        let entries = model::entries(&self.snapshot, &self.projection, view, &self.outputs, true);
        let text = entries.iter().map(|entry| entry.text()).collect::<Vec<_>>();
        let text = text.join("\n\n");
        let Some(session) = self.session() else {
            self.notice("No session to export yet");
            return;
        };
        let label = crate::tui::format::agent_label(&self.selected).replace(':', "-");
        let path = session.directory().join(format!("conversation-{label}.md"));
        let notices = self.notifier();
        tokio::spawn(async move {
            let notice = match tokio::fs::write(&path, text).await {
                Ok(_) => format!("Exported {}", path.display()),
                Err(error) => error.to_string(),
            };
            notices.send(notice);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::tests::*;
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers as M};
    use skyhook::agent::ObservationSnapshot;
    use skyhook::identity::JobId;
    use skyhook::session::{RecordSeq, SessionEvent};

    #[tokio::test(start_paused = true)]
    async fn retry_children_without_selection_preserves_the_waiting_root_operation() {
        let (_root, mut app) = permanent_failure_fixture().await;
        let session = app.session().cloned().unwrap();
        // A foreground child has settled once the script returns, so retry finds it failed.
        let script = "return await tool.agent({prompt:'Fail against the fixture provider'});";
        let launched = session.run_script(script).await.unwrap();
        let child: JobId = serde_json::from_value(launched.value["value"]["id"].clone()).unwrap();
        let failures = |snapshot: &ObservationSnapshot| {
            let records = snapshot.records.values();
            let failed = records.filter(|record| {
                matches!(record.event, SessionEvent::JobFinished {
                    job, state: skyhook::job::JobEnd::Failed, ..
                } if job == child)
            });
            failed.count()
        };
        let observation = session.observe().await;
        let (mut snapshot, mut updates) = (observation.snapshot, observation.updates);
        assert_eq!(failures(&snapshot), 1);
        app.snapshot = snapshot.clone();
        let root = session.root_agent().clone();
        let waiting = skyhook::agent::ObservedActivity {
            state: skyhook::agent::AgentActivity::WaitingChildren,
            since: 0,
        };
        app.snapshot.activity.insert(root.clone(), waiting.clone());
        assert_eq!(app.selected, root);
        app.operation = true;
        let initial_input = Some((root.clone(), RecordSeq::default()));
        app.initial_input = initial_input.clone();
        let mut rx = capture_work(&mut app);
        assert!(app.busy());

        app.command(Command::Retry);
        crate::tests::bounded(async {
            while failures(&snapshot) < 2 {
                snapshot.apply(updates.recv().await.unwrap());
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
        assert!(!snapshot.records.values().any(|record| {
            record.agent == root && matches!(record.event, SessionEvent::ModelRequested { .. })
        }));
        // Child retry neither replaces nor finishes the pending root operation.
        assert!(app.operation);
        assert_eq!(app.initial_input, initial_input);
        assert_eq!(app.snapshot.activity.get(&root), Some(&waiting));
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn command_menu_shows_default_shortcuts_separately_and_searchably() {
        let (_root, mut app) = draft_fixture().await;
        app.command(Command::Commands);
        let menu = app.menu_mut().unwrap();
        let MenuKind::Commands(items) = &menu.kind else {
            panic!("command menu")
        };
        for (command, label, shortcut) in [
            (Command::New, "New session", "Ctrl+X N"),
            (Command::Mode, "Mode", "Ctrl+X P"),
            (Command::Help, "Help and shortcuts", "Ctrl+X H"),
            (Command::Resume, "Resume queued input", ""),
            (Command::Model, "Model", "Ctrl+X M"),
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
        app.overlay = None;
        app.paused = true;
        key(&mut app, KeyCode::Char('/'), M::NONE);
        let typed: Vec<_> = "resume".chars().map(KeyCode::Char).collect();
        press(&mut app, &typed);
        key(&mut app, KeyCode::Enter, M::NONE);
        assert!(!app.paused);
        assert!(app.menu().is_none());
    }
}
