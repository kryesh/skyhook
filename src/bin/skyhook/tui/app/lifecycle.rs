use super::*;

pub(super) enum PendingStart {
    Input(QueuedInput),
    QueuedInput(QueuedInput),
    Script(PathBuf),
}

impl App {
    pub(super) fn begin_session(&mut self, action: PendingStart) {
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
    pub(super) fn park_pending_input(&mut self) {
        match self.pending_start.take() {
            Some(PendingStart::Input(input) | PendingStart::QueuedInput(input)) => {
                self.queue.push_front(input);
                self.refresh_queue_menu();
            }
            other => self.pending_start = other,
        }
    }
    pub(super) fn start_failed(&mut self, error: String) {
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
    pub(super) fn finish_shutdown(&self) {
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
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::*;
    use std::sync::Arc;
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
}
