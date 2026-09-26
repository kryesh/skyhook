use super::*;
use crate::launch::LaunchError;

pub(super) enum PendingStart {
    Input(Box<QueuedInput>),
    QueuedInput(Box<QueuedInput>),
    Script(PathBuf),
}

/// Creation owns its pending action until the asynchronous task completes.
/// A parked or failed script remains an explicit retry, not an active creation.
#[derive(Default)]
pub(super) enum StartState {
    #[default]
    Idle,
    Creating(PendingStart),
    RetryScript(PathBuf),
}
impl StartState {
    pub(super) fn is_creating(&self) -> bool {
        matches!(self, Self::Creating(_))
    }
    /// Leaves the state `Idle`.
    fn take_action(&mut self) -> Option<PendingStart> {
        match std::mem::take(self) {
            Self::Creating(action) => Some(action),
            Self::RetryScript(path) => Some(PendingStart::Script(path)),
            Self::Idle => None,
        }
    }
}

impl App {
    /// Invariant: at most one creation is in flight. Every caller is gated by
    /// `busy()`/`start.is_creating()`, so a `Started` completion is the current one.
    pub(super) fn begin_session(&mut self, action: PendingStart) {
        if matches!(self.start, StartState::RetryScript(_)) {
            self.notice("Cancelled the pending script in favor of the new action");
        }
        if self.start.is_creating() {
            return;
        }
        self.start = StartState::Creating(action);
        let launch = self.launch.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = launch.create(None).await;
            if let Err(error) = tx.send(Work::Started { result })
                && let Work::Started {
                    result: Ok(session),
                    ..
                } = error.0
            {
                let _ = session.shutdown().await;
            }
        });
        self.dirty = true;
    }
    pub(super) fn session_started(&mut self, prepared: PreparedObservation) {
        let pending = self.start.take_action();
        let draft = self.root_agent().clone();
        let attached_draft = match &self.phase {
            Phase::Draft { root } => Some(root.clone()),
            Phase::Open { attached_draft, .. } => attached_draft.clone(),
        };
        self.phase = Phase::Open {
            observation: prepared.active,
            attached_draft,
        };
        self.snapshot = prepared.snapshot;
        self.selected = self.root_agent().clone();
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
            self.finish_shutdown();
            return;
        }
        if self.paused {
            self.park_action(pending);
            return;
        }
        match pending {
            Some(PendingStart::Input(input)) => self.send_input(*input),
            Some(PendingStart::QueuedInput(input)) => {
                self.queue.push_front(*input);
                self.deliver_queue();
            }
            Some(PendingStart::Script(path)) => self.start_script(path),
            None => {}
        }
    }
    fn park_action(&mut self, pending: Option<PendingStart>) {
        match pending {
            Some(PendingStart::Input(input) | PendingStart::QueuedInput(input)) => {
                self.queue.push_front(*input);
                self.refresh_queue_menu();
            }
            Some(PendingStart::Script(path)) => self.start = StartState::RetryScript(path),
            None => {}
        }
    }
    pub(super) fn start_failed(&mut self, error: LaunchError) {
        // Keep an explicit script retry separate from composer input.
        let pending = self.start.take_action();
        self.park_action(pending);
        self.paused = true;
        self.notice(error.to_string());
        if self.stopping {
            self.finish_shutdown();
        }
    }
    /// Submit the command-line prompt. When an image cannot be read, keep the
    /// prompt as an editable draft so the user can fix it and resend.
    pub async fn start_prompt(&mut self, text: String, images: Vec<PathBuf>) {
        match crate::launch::read_images(&self.launch.workspace, &images).await {
            Ok(attachments) => self.submit(Submission { text, attachments }),
            Err(error) => {
                self.notice(error);
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
            self.notice("Wait for the current operation before starting a script");
            return;
        }
        let Some(session) = self.session().cloned() else {
            let Ok(model) = self.launch.model.config().select_model(&self.model) else {
                self.notice("The selected model is no longer configured");
                return;
            };
            self.launch.model = model;
            self.launch.permissions = crate::launch::Permissions::Mode(self.mode.clone());
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
            let _ = tx.send(Work::Done { result });
        });
    }
    pub fn shutdown(&mut self) {
        if self.stopping {
            return;
        }
        self.pause_queue();
        self.stopping = true;
        // A creation task owns a handle which must arrive before teardown.
        if !self.start.is_creating() {
            self.finish_shutdown();
        }
        self.dirty = true;
    }
    pub(super) fn finish_shutdown(&self) {
        let session = self.session().cloned();
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
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::*;

    #[tokio::test]
    async fn unreadable_start_image_keeps_the_prompt_as_a_draft() {
        let (_root, mut app) = draft_fixture().await;
        app.start_prompt("long prompt".into(), vec!["typo.png".into()])
            .await;
        assert_eq!(app.editor.expanded_text(), "long prompt");
        assert!(app.editor.attachments().is_empty());
        assert!(app.history.is_empty() && app.queue.is_empty());
        assert!(!app.start.is_creating() && app.session().is_none());
    }

    async fn started(rx: &mut mpsc::UnboundedReceiver<Work>) -> SessionHandle {
        let Work::Started {
            result: Ok(session),
            ..
        } = next_lifecycle(rx).await
        else {
            panic!("session creation should succeed");
        };
        session
    }

    async fn fail_creation(app: &mut App, rx: &mut mpsc::UnboundedReceiver<Work>) {
        let work = next_lifecycle(rx).await;
        assert!(matches!(work, Work::Started { result: Err(_), .. }));
        app.work(work);
    }

    /// Model proofs cannot be invalidated after selection, so fail creation at
    /// the real filesystem boundary instead.
    fn block_sessions(app: &App) {
        std::fs::create_dir_all(app.launch.sessions.parent().unwrap()).unwrap();
        std::fs::write(&app.launch.sessions, "not a directory").unwrap();
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
        let broken_skill = app.launch.workspace.join(".agents/skills/broken");
        std::fs::create_dir_all(&broken_skill).unwrap();
        let mut rx = capture_work(&mut app);
        for command in [Command::Export, Command::Retry] {
            app.command(command);
        }
        assert!(app.session().is_none() && !app.launch.sessions.exists());
        app.submit("first input".into());
        assert!(app.start.is_creating() && app.session().is_none());
        app.model = "test/second".parse().unwrap();
        let attachments = vec![png_attachment("queued.png")];
        app.submit(Submission {
            text: "second input".into(),
            attachments,
        });
        app.editor.set("still composing".into());
        app.editor.insert_paste("unsent attachment".into());
        let session = started(&mut rx).await;
        assert_eq!(std::fs::read_dir(&app.launch.sessions).unwrap().count(), 1);
        app.session_started(PreparedObservation::subscribe(session).await);
        assert!(!app.start.is_creating() && app.operation);
        assert_eq!(app.history, ["first input"]);
        assert_eq!(app.model.to_string(), "test/second");
        let image = [png_attachment("queued.png")];
        assert_eq!(
            queued(&app),
            [("second input", "test/second".to_owned(), &image[..])]
        );
        assert_eq!(
            app.editor.expanded_text(),
            "still composingunsent attachment"
        );
        let pastes: Vec<_> = app.editor.pastes().map(|(_, text)| text).collect();
        assert_eq!(pastes, ["unsent attachment"]);
        app.status.flush().await;
        let session = app.session().unwrap();
        let snapshot = session.observe().await.snapshot;
        assert!(!session.warnings().is_empty());
        for warning in session.warnings() {
            let status = format!("Startup warning: {warning}");
            assert!(snapshot.records.values().any(|record| matches!(
                &record.event, SessionEvent::Status { message } if message == &status
            )));
        }
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn failed_creation_retains_inputs_and_can_resume_without_duplicate_creation() {
        let (_root, mut app) = draft_fixture().await;
        let mut rx = capture_work(&mut app);
        block_sessions(&app);
        let attachments = vec![png_attachment("first.png")];
        app.submit(Submission {
            text: "first".into(),
            attachments,
        });
        app.model = "test/second".parse().unwrap();
        app.submit("second".into());
        app.editor.set("new draft".into());
        fail_creation(&mut app, &mut rx).await;
        assert!(!app.busy() && app.paused && app.session().is_none());
        let image = [png_attachment("first.png")];
        assert_eq!(
            queued(&app),
            [
                ("first", "test/first".to_owned(), &image[..]),
                ("second", "test/second".to_owned(), &[])
            ]
        );
        assert_eq!(app.editor.text(), "new draft");
        std::fs::remove_file(&app.launch.sessions).unwrap();
        app.command(Command::Resume);
        app.tick();
        assert!(app.start.is_creating());
        app.tick();
        assert_eq!(app.queue.len(), 1);
        let session = started(&mut rx).await;
        app.shutdown();
        app.session_started(PreparedObservation::subscribe(session).await);
        app.work(next_lifecycle(&mut rx).await);
        assert!(app.exit);
        assert!(
            app.history.is_empty(),
            "shutdown must suppress the pending prompt"
        );
    }

    #[tokio::test]
    async fn shutdown_waits_for_failed_creation() {
        let (_root, mut app) = draft_fixture().await;
        let mut rx = capture_work(&mut app);
        block_sessions(&app);
        app.submit("unsent".into());
        app.shutdown();
        assert!(!app.exit);
        fail_creation(&mut app, &mut rx).await;
        app.work(next_lifecycle(&mut rx).await);
        assert!(app.exit && app.launch.sessions.is_file());
    }
}
