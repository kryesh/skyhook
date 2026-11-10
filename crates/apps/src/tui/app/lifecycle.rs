use super::events::Work;
use super::observation::PreparedObservation;
use super::queue::{Paused, QueuedInput};
use super::{App, Phase, Stage};
use crate::tui::composer::Submission;
use crate::tui::{model, shell};
use skyhook::config::mode::ModeName;
use skyhook::launch::LaunchError;
use skyhook::provider::model::ModelRef;
use std::path::PathBuf;
use tokio::sync::mpsc;

/// What a new session is created for, and the model and mode it starts with.
pub(super) enum PendingStart {
    Input(Box<QueuedInput>),
    Script {
        path: PathBuf,
        model: ModelRef,
        mode: ModeName,
    },
}
impl PendingStart {
    fn selection(&self) -> (&ModelRef, &ModeName) {
        match self {
            Self::Input(input) => (&input.model, &input.mode),
            Self::Script { model, mode, .. } => (model, mode),
        }
    }
}

impl App {
    /// Create the session `action` waits for, from a draft with nothing starting.
    pub(super) fn begin_session(&mut self, action: PendingStart) {
        let (model, mode) = action.selection();
        let Ok(launch) = self.launch.selecting(model, mode.clone()) else {
            let unavailable = skyhook::agent::HarnessError::UnknownModel(model.clone());
            self.notice(crate::tui::format::harness_error(&unavailable));
            self.pause(Paused::Stopped);
            return self.park_action(action);
        };
        let scope = match self.scope.as_mut().map(super::scope::ScopePanel::freeze) {
            None => None,
            Some(Ok(reply)) => Some(reply),
            Some(Err(invalid)) => {
                // Delivery waits for an explicit retry once the field is corrected,
                // never the first value that happens to parse mid-edit.
                self.scope_invalid(invalid);
                self.pause(Paused::Stopped);
                return self.park_action(action);
            }
        };
        let Phase::Draft {
            start: start @ None,
            ..
        } = &mut self.phase
        else {
            return self.park_action(action);
        };
        *start = Some(action);
        let (tx, ui) = (self.tx.clone(), self.ui.clone());
        let open = async move {
            let opening = skyhook::product::Opening::New(scope.as_ref());
            shell::open(&launch, &ui, opening, None).await
        };
        shell::open_in_background(open, move |result| {
            let result = result.map(Box::new);
            match tx.send(Work::Started { result }) {
                Err(mpsc::error::SendError(Work::Started {
                    result: Ok(prepared),
                })) => Some(*prepared),
                _ => None,
            }
        });
        self.dirty = true;
    }
    pub(super) fn session_started(&mut self, prepared: PreparedObservation) {
        let draft = self.root_agent().clone();
        let (pending, attached_draft) = match &mut self.phase {
            Phase::Draft { root, start } => (start.take(), Some(root.clone())),
            Phase::Open { attached_draft, .. } => (None, attached_draft.clone()),
        };
        // A new session keeps the panel its draft was sent with.
        let PreparedObservation {
            active,
            scope: _,
            budget,
            shown:
                super::observation::Shown {
                    snapshot,
                    projection,
                    window,
                    ..
                },
        } = prepared;
        self.phase = Phase::Open {
            observation: active,
            attached_draft,
            owns_root: false,
        };
        self.snapshot = *snapshot;
        self.forget_unread_responses();
        self.projection = *projection;
        self.window = window.map(|window| *window);
        self.budget = budget;
        self.selected = self.root_agent().clone();
        self.views.transfer(&draft, self.selected.clone());
        for status in &mut self.unsaved_status {
            if status.agent == draft {
                status.agent = self.selected.clone();
            }
        }
        self.content_cache = model::ContentCache::default();
        self.render.reset_session();
        self.refresh();
        self.show_warnings();
        self.load_title();
        if self.stage == Stage::Stopping {
            self.finish_shutdown();
            return;
        }
        match pending {
            Some(action) if self.paused.is_some() => self.park_action(action),
            Some(PendingStart::Input(input)) => {
                self.own_queue().push_front(*input);
                self.deliver_queue();
            }
            Some(PendingStart::Script { path, .. }) => self.start_script(path),
            None => {}
        }
    }
    /// Settle an action whose session was not created: input returns to the
    /// head of the queue, and a script is dropped.
    fn park_action(&mut self, action: PendingStart) {
        match action {
            PendingStart::Input(input) => {
                self.own_queue().push_front(*input);
                self.refresh_menu();
            }
            PendingStart::Script { path, .. } => {
                self.notice(format!("Script {} was not run", path.display()));
            }
        }
    }
    pub(super) fn start_failed(&mut self, error: LaunchError) {
        self.pause(Paused::Stopped);
        self.notice(error.to_string());
        if let Some(panel) = self.scope.as_mut() {
            panel.reopen();
            self.scope_changed();
        }
        if let Phase::Draft { start, .. } = &mut self.phase
            && let Some(action) = start.take()
        {
            self.park_action(action);
        }
        if self.stage == Stage::Stopping {
            self.finish_shutdown();
        }
    }
    /// Submit the command-line prompt. When an image cannot be read, keep the
    /// prompt as an editable draft so the user can fix it and resend.
    pub async fn start_prompt(&mut self, text: String, images: Vec<PathBuf>) {
        match skyhook::attachment::read_images(self.launch.workspace(), &images).await {
            Ok(attachments) => self.submit(Submission { text, attachments }),
            Err(error) => {
                self.notice(error.to_string());
                self.replace_draft(Submission {
                    text,
                    attachments: Vec::new(),
                });
                self.dirty = true;
            }
        }
    }
    pub fn start_script(&mut self, path: PathBuf) {
        if self.busy() {
            self.toast("Wait for the current operation before starting a script");
            return;
        }
        let Some(session) = self.session().cloned() else {
            let (model, mode) = (self.model.clone(), self.mode.clone());
            return self.begin_session(PendingStart::Script { path, model, mode });
        };
        self.own_root(true);
        self.set_title(&path.display().to_string());
        let tx = self.tx.clone();
        let status = self.status.clone();
        tokio::spawn(async move {
            status.flush().await;
            let result = async {
                let source = tokio::fs::read_to_string(path).await?;
                match session.run_script(source).await {
                    // The interrupt that withdrew it reports it.
                    Ok(_) | Err(skyhook::agent::HarnessError::Interrupted) => Ok(()),
                    Err(error) => Err(error.into()),
                }
            }
            .await;
            let _ = tx.send(Work::Done { result });
        });
    }
    pub fn shutdown(&mut self) {
        if self.stage != Stage::Running {
            return;
        }
        self.pause_queue(Paused::Stopped);
        self.stage = Stage::Stopping;
        // A creation task owns a handle which must arrive before teardown.
        if !self.creating() {
            self.finish_shutdown();
        }
        self.dirty = true;
    }
    /// A session is being created for a draft's first action.
    pub(super) fn creating(&self) -> bool {
        matches!(self.phase, Phase::Draft { start: Some(_), .. })
    }
    /// Whether a script or continuation this app ran owns the root.
    pub(super) fn owns_root(&self) -> bool {
        matches!(
            self.phase,
            Phase::Open {
                owns_root: true,
                ..
            }
        )
    }
    /// Take or release the root for a script or continuation of this app's own.
    pub(super) fn own_root(&mut self, owned: bool) {
        if let Phase::Open { owns_root, .. } = &mut self.phase {
            *owns_root = owned;
        }
    }
    /// The slot shut its session down, or is doing so.
    pub fn stopping(&self) -> bool {
        self.stage != Stage::Running
    }
    pub(super) fn finish_shutdown(&mut self) {
        let session = self.session().cloned();
        let tx = self.tx.clone();
        let notices = self.root_notifier();
        let status = self.status.clone();
        tokio::spawn(async move {
            status.flush().await;
            if let Some(session) = &session
                && let Err(e) = session.shutdown().await
                && !e.journal_failed()
            {
                notices.send(e.to_string());
            }
            status.flush().await;
            // Closed before the host lets go of the slot, so it reopens at once;
            // shutdown has reported any journal failure.
            if let Some(session) = session {
                let _ = session.close().await;
            }
            let _ = tx.send(Work::Stopped);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::*;
    use crate::tui::commands::Command;
    use crate::tui::editor::TextField;
    use crate::tui::format;
    use skyhook::identity::AgentId;
    use skyhook_media::Attachment;
    use std::assert_matches;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn unreadable_start_image_keeps_the_prompt_as_a_draft() {
        let (_root, mut app) = draft_fixture().await;
        app.start_prompt("long prompt".into(), vec!["typo.png".into()])
            .await;
        assert_eq!(app.editor.expanded_text(), "long prompt");
        assert!(app.editor.attachments().is_empty());
        assert!(app.history.is_empty() && app.queue.is_empty());
        assert!(!app.creating() && app.session().is_none());
    }

    async fn started(rx: &mut mpsc::UnboundedReceiver<Work>) -> PreparedObservation {
        let Work::Started {
            result: Ok(prepared),
        } = next_lifecycle(rx).await
        else {
            panic!("session creation should succeed");
        };
        *prepared
    }

    async fn fail_creation(app: &mut App, rx: &mut mpsc::UnboundedReceiver<Work>) {
        let work = next_lifecycle(rx).await;
        assert!(matches!(work, Work::Started { result: Err(_), .. }));
        app.work(work);
    }

    /// Model proofs cannot be invalidated after selection, so fail creation at
    /// the real filesystem boundary instead.
    fn block_sessions(app: &App) {
        std::fs::create_dir_all(app.launch.sessions().parent().unwrap()).unwrap();
        std::fs::write(app.launch.sessions(), "not a directory").unwrap();
    }

    fn queued(app: &App) -> Vec<(&str, String, &[Attachment])> {
        let rows = app.queue.iter();
        rows.map(|row| {
            (
                &*row.submission.text,
                row.model.to_string(),
                &row.submission.attachments[..],
            )
        })
        .collect()
    }

    #[tokio::test]
    async fn first_submit_creates_once_and_preserves_queue_models_and_draft() {
        let (_root, mut app) = draft_fixture().await;
        let mut rx = capture_work(&mut app);
        app.submit("first input".into());
        assert!(app.creating() && app.session().is_none());
        app.model = "test/second".parse().unwrap();
        let attachments = vec![png_attachment("queued.png").await];
        app.submit(Submission {
            text: "second input".into(),
            attachments,
        });
        app.editor.set("still composing".into());
        app.editor.insert_paste("unsent attachment".into());
        let prepared = started(&mut rx).await;
        assert_eq!(std::fs::read_dir(app.launch.sessions()).unwrap().count(), 1);
        app.session_started(prepared);
        // The session cannot take the second row's model, so the batch stays queued whole.
        assert!(!app.creating() && app.paused.is_some());
        let image = [png_attachment("queued.png").await];
        assert_eq!(
            queued(&app),
            [
                ("first input", "test/first".to_owned(), &[][..]),
                ("second input", "test/second".to_owned(), &image[..])
            ]
        );
        let second = app.remove_queued(app.queue[1].id).unwrap();
        app.command(Command::Resume);
        app.tick();
        assert!(app.busy());
        bounded(async {
            while app.history.is_empty() {
                let work = recv(&mut rx).await;
                app.work(work);
            }
        })
        .await;
        assert_eq!(app.history, ["first input"]);
        assert_eq!(app.model, "test/second".parse().unwrap());
        assert_eq!(second.model, "test/second".parse().unwrap());
        assert_eq!(
            app.editor.expanded_text(),
            "still composingunsent attachment"
        );
        let pastes: Vec<_> = app.editor.pastes().map(|(_, text)| text).collect();
        assert_eq!(pastes, ["unsent attachment"]);
        app.session().unwrap().shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn failed_creation_retains_inputs_and_can_resume_without_duplicate_creation() {
        let (_root, mut app) = draft_fixture().await;
        let mut rx = capture_work(&mut app);
        block_sessions(&app);
        let attachments = vec![png_attachment("first.png").await];
        app.submit(Submission {
            text: "first".into(),
            attachments,
        });
        app.model = "test/second".parse().unwrap();
        app.submit("second".into());
        app.editor.set("new draft".into());
        fail_creation(&mut app, &mut rx).await;
        assert!(!app.busy() && app.paused.is_some() && app.session().is_none());
        let image = [png_attachment("first.png").await];
        assert_eq!(
            queued(&app),
            [
                ("first", "test/first".to_owned(), &image[..]),
                ("second", "test/second".to_owned(), &[])
            ]
        );
        assert_eq!(app.editor.text(), "new draft");
        std::fs::remove_file(app.launch.sessions()).unwrap();
        app.command(Command::Resume);
        app.tick();
        assert!(app.creating());
        app.tick();
        assert_eq!(app.queue.len(), 1);
        let prepared = started(&mut rx).await;
        app.shutdown();
        app.session_started(prepared);
        app.work(next_lifecycle(&mut rx).await);
        assert_eq!(app.stage, Stage::Stopped);
        assert!(
            app.history.is_empty(),
            "shutdown must suppress the pending prompt"
        );
    }

    #[tokio::test]
    async fn an_unconfigured_model_keeps_the_input_queued_and_pauses() {
        let (_root, mut app) = draft_fixture().await;
        let mut rx = capture_work(&mut app);
        app.status = crate::tui::status::StatusLog::new(app.tx.clone());
        app.model = "test/removed".parse().unwrap();
        app.submit("input".into());
        app.status.flush().await;
        let notices: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|work| match work {
                Work::Unsaved { message, .. } => Some(message),
                _ => None,
            })
            .collect();
        assert_eq!(
            notices,
            ["model `test/removed` is not configured; choose another with /model"]
        );
        assert!(app.paused.is_some() && !app.creating());
        assert_eq!(app.queue.len(), 1);
    }

    #[tokio::test]
    async fn shutdown_waits_for_failed_creation() {
        let (_root, mut app) = draft_fixture().await;
        let mut rx = capture_work(&mut app);
        block_sessions(&app);
        app.submit("unsent".into());
        app.shutdown();
        assert_ne!(app.stage, Stage::Stopped);
        fail_creation(&mut app, &mut rx).await;
        app.work(next_lifecycle(&mut rx).await);
        assert!(app.stage == Stage::Stopped && app.launch.sessions().is_file());
    }

    /// A failed journal stops the session as a crash would: its queued rows go,
    /// its conversation shows why once, and it stays open until it is closed.
    #[tokio::test]
    async fn a_failed_journal_stops_the_session_and_shows_why_in_its_conversation() {
        use super::super::played::{observe_until, played_fixture};
        use skyhook::identity::SessionId;
        let (_root, mut app, mut model, session) = played_fixture().await;
        let mut rx = capture_work(&mut app);
        app.status = crate::tui::status::StatusLog::new(app.tx.clone());
        app.submit("first".into());
        let _turn = model.turn().await;
        bounded(async {
            while !app.queue.is_empty() {
                let work = recv(&mut rx).await;
                app.work(work);
            }
        })
        .await;
        app.submit("queued".into());
        assert_eq!(app.queue.len(), 1);
        let foreign = AgentId::root(SessionId::generate().unwrap());
        let refused = session.record_status(foreign, "elsewhere".into()).await;
        assert!(refused.is_err());
        observe_until(&mut app, |app| app.queue.is_empty()).await;
        app.status.flush().await;
        while let Ok(work) = rx.try_recv() {
            app.work(work);
        }
        assert!(app.stage != Stage::Stopped && app.queue.is_empty());
        app.rebuild_content();
        let shown = (app.entries().iter())
            .filter(|entry| entry.text().contains("could not write the session journal"));
        assert_eq!(shown.count(), 1);
        app.command(Command::Close);
        app.work(next_lifecycle(&mut rx).await);
        assert_eq!(app.stage, Stage::Stopped);
    }

    /// What a failed journal cut short reads as stopped: a job's card on the Jobs tab
    /// where it is already shown, as its own update would show it, a request's row on
    /// the Requests tab, and a child its background work kept alive after it answered
    /// as interrupted, out of the live tree.
    #[tokio::test]
    async fn a_failed_journal_stops_the_calls_it_cut_short() {
        use super::super::played::{
            answered, ask, call, children, count, observe_until, played_fixture,
        };
        use crate::tui::model::AgentDisplayState;
        use serde_json::json;
        use skyhook::identity::SessionId;
        use skyhook::job::JobState;
        use skyhook::provider::protocol::ContextId;
        let (_root, mut app, mut played, session) = played_fixture().await;
        app.tab = model::Tab::Jobs;
        super::super::tests::settled(&mut app).await;
        let root = app.root_agent().clone();
        let _asked = ask(&session, "delegate");
        let task = json!({"prompt": "work", "bg": true});
        played
            .turn()
            .await
            .end(vec![call("agent", "agent", task, 0)]);
        // The root's next request stays open while its child answers.
        let mut turns = [played.turn().await, played.turn().await];
        if turns[0].context == ContextId::from(&root) {
            turns.swap(0, 1);
        }
        let [child_turn, _turn] = turns;
        let work = json!({"source": "await sleep(60000);", "bg": true});
        child_turn.end(vec![call("work", "script", work, 0)]);
        played.turn().await.answer("done");
        let shown = |app: &App| {
            let mut entries = app.entries().iter().rev();
            entries.find_map(|entry| {
                let call = matches!(entry.key(), model::EntryKey::Request(_));
                (call || entry.job_id().is_some()).then_some(entry.timing)
            })
        };
        // Folded as the host folds, so only what the update changed rebuilds.
        observe_until(&mut app, |app| {
            matches!(shown(app), Some(format::Timing::Since(_)))
                && count(&app.journal, answered) == 1
        })
        .await;
        assert!(app.projection.has_active_children());
        let foreign = AgentId::root(SessionId::generate().unwrap());
        let refused = session.record_status(foreign, "elsewhere".into()).await;
        assert!(refused.is_err());
        observe_until(&mut app, |app| app.snapshot.phase.failure().is_some()).await;
        assert_matches!(shown(&app), Some(format::Timing::Took { .. }));
        app.set_tab(model::Tab::Requests);
        super::super::tests::settled(&mut app).await;
        assert_matches!(shown(&app), Some(format::Timing::Took { .. }));
        let child = children(&app.journal).remove(0);
        let mut agents = app.projection.agents.iter();
        let child = agents.find(|info| info.id == child).unwrap();
        let interrupted = AgentDisplayState::Job(JobState::Interrupted);
        assert_eq!(app.projection.status(child, &app.snapshot), interrupted);
        assert!(!app.projection.has_active_children());
    }
}
