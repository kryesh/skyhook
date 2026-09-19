use super::*;
use skyhook::agent::{HarnessError, PromptOptions, QueuedPrompt, QueuedPromptCancellation};
use skyhook::session::SessionError;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueuedInputId(u64);

/// One composer submission waiting behind the current operation.
pub struct QueuedInput {
    pub(super) id: QueuedInputId,
    /// Advanced by every dispatch and withdrawal, so a late receipt is ignored.
    pub(super) generation: u64,
    /// Held while the row is dispatched and its commit receipt is pending.
    pub(super) in_flight: Option<QueuedPromptCancellation>,
    /// The commit outcome is unknown: only the user may send this row again,
    /// and nothing behind it is dispatched until they edit or remove it.
    pub(super) unknown: bool,
    pub(super) submission: Submission,
    pub(super) model: String,
}

pub(super) struct QueueDelivery {
    pub(super) id: QueuedInputId,
    pub(super) generation: u64,
    pub(super) prompt: QueuedPrompt,
}

/// Register each UI queue snapshot as one request-boundary batch, in order.
pub(super) async fn queue_dispatcher(
    session: SessionHandle,
    mut rx: mpsc::UnboundedReceiver<Vec<QueueDelivery>>,
    tx: mpsc::UnboundedSender<Work>,
) {
    use futures_util::{StreamExt, stream::FuturesUnordered};
    let mut pending = FuturesUnordered::new();
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
                    let (ids, prompts): (Vec<_>, Vec<_>) = deliveries
                        .into_iter()
                        .map(|input| ((input.id, input.generation), input.prompt))
                        .unzip();
                    let receipts = session.enqueue_prompts(prompts).await;
                    for ((id, generation), receipt) in ids.into_iter().zip(receipts) {
                        let session = session.clone();
                        pending.push(async move {
                            let result = receipt.await.unwrap_or(Err(HarnessError::AgentStopped));
                            let revision = session.observe().await.snapshot.revision;
                            Work::QueueCommitted {
                                session: session.id(), id, generation, revision, result,
                            }
                        });
                    }
                }
                None => open = false,
            },
            Some(work) = pending.next(), if !pending.is_empty() => {
                let _ = tx.send(work);
            }
        }
    }
}

impl App {
    pub fn submit(&mut self, submission: Submission) {
        if submission.is_empty() {
            return;
        }
        self.reject_pending_questions();
        self.advance_draft();
        let queued = self.queued_input(submission);
        if self.busy() || self.paused || !self.queue.is_empty() {
            self.queue.push_back(queued);
            self.refresh_queue_menu();
            self.deliver_queue();
            self.dirty = true;
            return;
        }
        self.send_input(queued);
    }
    /// Select the queued input's model, or retain the input and pause when
    /// that model is no longer configured.
    fn select_queued_model(&mut self, input: QueuedInput) -> Option<QueuedInput> {
        match self.launch.model.config().select_model(&input.model) {
            Ok(model) => {
                self.launch.model = model;
                Some(input)
            }
            Err(_) => {
                self.queue.push_front(input);
                self.paused = true;
                self.notice("Queued input model is no longer configured; input retained");
                None
            }
        }
    }
    pub(super) fn send_input(&mut self, queued: QueuedInput) {
        let Some(session) = self.session().cloned() else {
            if let Some(queued) = self.select_queued_model(queued) {
                self.begin_session(PendingStart::Input(Box::new(queued)));
            }
            return;
        };
        self.commit_row(&queued);
        let QueuedInput {
            submission: Submission { text, attachments },
            model,
            ..
        } = queued;
        self.operation = true;
        self.initial_input = Some((
            self.root_agent().clone(),
            self.snapshot
                .records
                .keys()
                .next_back()
                .copied()
                .unwrap_or(0),
        ));
        let tx = self.tx.clone();
        let status = self.status.clone();
        tokio::spawn(async move {
            status.flush().await;
            let result = session
                .prompt_with_options(
                    text,
                    &attachments,
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
    fn remember_model(&mut self, model: &str) {
        if self.remembered_model.as_deref() == Some(model) {
            return;
        }
        self.remembered_model = Some(model.to_owned());
        let remembered = model.to_owned();
        let notices = self.root_notifier();
        let workspace = self.launch.workspace.clone();
        tokio::task::spawn_blocking(move || {
            if let Err(error) = state::remember(&workspace, &remembered) {
                notices.send(format!("Could not save model selection: {error}"));
            }
        });
    }
    /// A committed or directly sent row goes through ordinary history bookkeeping.
    pub(super) fn commit_row(&mut self, input: &QueuedInput) {
        if let Ok(model) = self.launch.model.config().select_model(&input.model) {
            self.launch.model = model;
        }
        self.history.push(input.submission.text.clone());
        self.history_browse = None;
        self.set_title(&input.submission.text);
        self.remember_model(&input.model);
    }
    /// Withdraw every dispatched row that is still cancellable.
    pub(super) fn cancel_queue_delivery(&mut self) {
        for input in &mut self.queue {
            if input.in_flight.as_ref().is_some_and(|row| row.cancel()) {
                input.in_flight = None;
                input.generation = input.generation.wrapping_add(1);
            }
        }
    }
    /// Stop dispatch, withdraw what can still be withdrawn, and keep a pending
    /// switch from restoring an unpaused queue.
    pub(super) fn pause_queue(&mut self) {
        self.paused = true;
        self.cancel_queue_delivery();
        if let Some(restore_paused) = &mut self.switching {
            *restore_paused = true;
        }
    }

    pub(super) fn deliver_queue(&mut self) {
        // Lag recovery replaces the snapshot without replaying each event.
        for record in self.snapshot.records.values() {
            if observe_initial_input(&mut self.initial_input, record) {
                break;
            }
        }
        if self.start.is_creating()
            || self.stopping
            || self.switching.is_some()
            || self.paused
            || self.queue.is_empty()
        {
            return;
        }
        let Some(session) = self.session().cloned() else {
            // Retain the head across creation, then register the entire queue.
            let input = self.queue.pop_front().unwrap();
            self.refresh_queue_menu();
            match self.select_queued_model(input) {
                Some(input) => self.begin_session(PendingStart::QueuedInput(Box::new(input))),
                None => self.refresh_queue_menu(),
            }
            return;
        };
        if self.initial_input.is_some() {
            return;
        }
        let mut deliveries = Vec::new();
        for input in self.queue.iter_mut().take_while(|input| !input.unknown) {
            if input.in_flight.is_some() {
                continue;
            }
            let cancellation = QueuedPromptCancellation::default();
            input.generation = input.generation.wrapping_add(1);
            input.in_flight = Some(cancellation.clone());
            deliveries.push(QueueDelivery {
                id: input.id,
                generation: input.generation,
                prompt: QueuedPrompt {
                    text: input.submission.text.clone(),
                    attachments: input.submission.attachments.clone(),
                    options: PromptOptions {
                        model: Some(input.model.clone()),
                    },
                    cancellation,
                },
            });
        }
        if deliveries.is_empty() {
            return;
        }
        let sender = self.queue_sender.get_or_insert_with(|| {
            let (tx, rx) = mpsc::unbounded_channel();
            tokio::spawn(queue_dispatcher(session, rx, self.tx.clone()));
            tx
        });
        if sender.send(deliveries).is_err() {
            // Nothing reached the runtime: every row is waiting again.
            self.queue_sender = None;
            self.pause_queue();
            self.notice("Could not deliver queued messages; resume to retry");
        }
    }
    /// `current` is false for a receipt from the session this app switched
    /// away from: its row could not be withdrawn, so it waited for this.
    pub(super) fn queue_committed(
        &mut self,
        current: bool,
        id: QueuedInputId,
        generation: u64,
        revision: u64,
        result: Result<(), HarnessError>,
    ) {
        let Some(index) = self.queue.iter().position(|input| {
            input.id == id && input.generation == generation && input.in_flight.is_some()
        }) else {
            return;
        };
        match result {
            Ok(()) => {
                let input = self.queue.remove(index).unwrap();
                // A row the previous session committed is that session's history.
                if current {
                    self.queue_activity_revision = self.queue_activity_revision.max(revision);
                    self.commit_row(&input);
                }
            }
            Err(error) => {
                let unknown = matches!(
                    error,
                    HarnessError::Session(SessionError::AppendIndeterminate(_))
                );
                let row = &mut self.queue[index];
                (row.in_flight, row.unknown) = (None, unknown);
                self.pause_queue();
                // A held-back row is reported by whatever held it back.
                if matches!(error, HarnessError::Interrupted) {
                    return self.refresh_queue_menu();
                }
                self.notice(if unknown {
                    format!("Queued message may or may not have been submitted: {error}. Check the transcript, then edit or remove it from the queue.")
                } else {
                    format!("Queued message was not submitted: {error}. Resume to retry.")
                });
            }
        }
        self.refresh_queue_menu();
    }
    pub(super) fn queued_input(&mut self, submission: Submission) -> QueuedInput {
        self.next_queued_id.0 += 1;
        QueuedInput {
            id: self.next_queued_id,
            generation: 0,
            in_flight: None,
            unknown: false,
            submission,
            model: self.model.clone(),
        }
    }
    pub(super) fn queue_items(&self) -> Vec<Item<QueuedInputId>> {
        self.queue
            .iter()
            .map(|queued| {
                let models = &self.launch.model.config().config().models;
                Item::new(
                    queued.id,
                    crate::tui::format::brief(&queued.submission.text, 100),
                    match models.get(&queued.model) {
                        _ if queued.unknown => "outcome unknown",
                        Some(profile) => profile.model.as_str(),
                        None => queued.model.as_str(),
                    },
                )
            })
            .collect()
    }
    /// Take a row out of the queue, withdrawing its dispatch if one is pending.
    pub(super) fn remove_queued(&mut self, id: QueuedInputId) -> Option<QueuedInput> {
        let index = self.queue.iter().position(|queued| queued.id == id)?;
        if self.queue[index]
            .in_flight
            .as_ref()
            .is_some_and(|row| !row.cancel())
        {
            self.notice("This message is already being submitted");
            return None;
        }
        let mut input = self.queue.remove(index).unwrap();
        input.in_flight = None;
        Some(input)
    }
    pub(super) fn refresh_queue_menu(&mut self) {
        if !self
            .menu
            .as_ref()
            .is_some_and(|menu| matches!(menu.kind, MenuKind::Queue(_)))
        {
            return;
        }
        let items = self.queue_items();
        self.menu
            .as_mut()
            .unwrap()
            .replace_items(MenuKind::Queue(items), |kind| match kind {
                MenuKind::Queue(items) => Some(items),
                _ => None,
            });
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::*;
    use skyhook::identity::EventId;
    use skyhook::provider::protocol::UserContent;

    type Deliveries = mpsc::UnboundedReceiver<Vec<QueueDelivery>>;

    /// A busy fixture whose dispatches the test receives instead of a runtime.
    async fn queue_fixture() -> (tempfile::TempDir, App, Deliveries) {
        let (root, mut app) = fixture().await;
        let (tx, rx) = mpsc::unbounded_channel();
        app.queue_sender = Some(tx);
        app.operation = true;
        (root, app, rx)
    }

    /// Every notice so far: journaled with a session, otherwise sent back as work.
    async fn notices(app: &App, rx: &mut mpsc::UnboundedReceiver<Work>) -> Vec<String> {
        app.status.flush().await;
        let mut notices = Vec::new();
        if let Some(session) = app.session() {
            let records = session.observe().await.snapshot.records;
            notices.extend(records.values().filter_map(|record| match &record.event {
                SessionEvent::Status { message } => Some(message.clone()),
                _ => None,
            }));
        }
        while let Ok(work) = rx.try_recv() {
            if let Work::StatusFailed { message, .. } = work {
                notices.push(message);
            }
        }
        notices
    }

    fn image_row() -> Submission {
        // The fixture's model does not support images: a claimed row that fails.
        Submission {
            text: "image".into(),
            attachments: vec![png_attachment("image.png")],
        }
    }

    fn texts(batch: &[QueueDelivery]) -> Vec<&str> {
        batch.iter().map(|row| row.prompt.text.as_str()).collect()
    }

    fn receipt(app: &mut App, row: &QueueDelivery, result: Result<(), HarnessError>) {
        app.work(Work::QueueCommitted {
            session: app.session_id().unwrap(),
            id: row.id,
            generation: row.generation,
            revision: app.snapshot.revision + 1,
            result,
        });
    }

    #[tokio::test]
    async fn busy_submissions_dispatch_in_order_and_leave_on_their_receipt() {
        let (_root, mut app, mut deliveries) = queue_fixture().await;
        app.submit(Submission {
            text: "first".into(),
            attachments: vec![png_attachment("first.png")],
        });
        app.submit("second".into());
        let first = deliveries.try_recv().unwrap();
        let second = deliveries.try_recv().unwrap();
        assert_eq!(
            (texts(&first), texts(&second)),
            (vec!["first"], vec!["second"])
        );
        assert_eq!(first[0].prompt.attachments, [png_attachment("first.png")]);
        assert_eq!(first[0].prompt.options.model.as_deref(), Some("first"));
        assert!(app.queue.iter().all(|input| input.in_flight.is_some()));
        assert!(app.history.is_empty());

        receipt(&mut app, &first[0], Ok(()));
        assert_eq!(app.history, ["first"]);
        assert_eq!(app.queue.len(), 1);
        // A commit is not a turn completion, and an idle enqueue stays busy
        // until the observed activity catches up.
        assert!(app.operation);
        app.operation = false;
        receipt(&mut app, &second[0], Ok(()));
        assert!(app.queue.is_empty() && app.busy());
        app.snapshot.revision = app.queue_activity_revision;
        assert!(!app.busy());
    }

    #[tokio::test]
    async fn pause_withdraws_rows_and_resume_dispatches_them_as_one_batch() {
        let (_root, mut app, mut deliveries) = queue_fixture().await;
        app.submit("first".into());
        app.submit("second".into());
        let stale = deliveries.try_recv().unwrap();
        deliveries.try_recv().unwrap();
        app.pause_queue();
        assert!(app.queue.iter().all(|input| input.in_flight.is_none()));
        assert!(stale[0].prompt.cancellation.cancel());
        // Neither outcome of a withdrawn dispatch touches the row.
        receipt(&mut app, &stale[0], Ok(()));
        receipt(&mut app, &stale[0], Err(HarnessError::Interrupted));
        assert_eq!(app.queue.len(), 2);
        app.tick();
        assert!(deliveries.try_recv().is_err());

        app.command(Command::Resume);
        app.tick();
        let batch = deliveries.try_recv().unwrap();
        assert_eq!(texts(&batch), ["first", "second"]);
        assert!(batch[0].generation > stale[0].generation);
    }

    #[tokio::test]
    async fn failed_receipt_retains_the_row_and_pauses_the_rows_behind_it() {
        let (_root, mut app) = fixture().await;
        let mut rx = capture_work(&mut app);
        app.paused = true;
        app.submit(image_row());
        app.submit("second".into());
        app.submit("third".into());
        app.command(Command::Resume);
        app.tick();
        // The runtime claims and fails the first row, then holds back the rest.
        for _ in 0..3 {
            let work = recv(&mut rx).await;
            assert!(matches!(work, Work::QueueCommitted { .. }));
            app.work(work);
        }
        assert!(app.paused && app.history.is_empty());
        assert_eq!(app.queue.len(), 3);
        assert!(app.queue.iter().all(|input| input.in_flight.is_none()));
        let notices = notices(&app, &mut rx).await;
        let failed = notices.iter().filter(|notice| notice.contains("Queued"));
        assert_eq!(
            failed.collect::<Vec<_>>(),
            [
                "Queued input resumed",
                "Queued message was not submitted: model `fixture` does not support image inputs. Resume to retry.",
            ]
        );
        app.session().unwrap().shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn indeterminate_receipt_is_never_redispatched_and_blocks_the_rows_behind_it() {
        let (_root, mut app, mut deliveries) = queue_fixture().await;
        let mut rx = capture_work(&mut app);
        app.submit("unknown".into());
        app.submit("behind".into());
        let unknown = deliveries.try_recv().unwrap();
        let error = SessionError::AppendIndeterminate(skyhook::session::AppendRecovery {
            identity: skyhook::session::AppendIdentity {
                event: EventId::generate().unwrap(),
                session: app.session_id().unwrap(),
                sequence: 1,
            },
            reason: "lost".into(),
        });
        receipt(&mut app, &unknown[0], Err(HarnessError::Session(error)));
        deliveries.try_recv().unwrap();
        app.command(Command::Resume);
        app.tick();
        assert!(deliveries.try_recv().is_err());
        assert!(
            notices(&app, &mut rx)
                .await
                .iter()
                .any(|notice| notice.contains("Check the transcript"))
        );
        // Only the user resolves it: removing the row releases the rows behind it.
        let id = app.queue[0].id;
        assert_eq!(app.remove_queued(id).unwrap().submission.text, "unknown");
        app.tick();
        assert_eq!(texts(&deliveries.try_recv().unwrap()), ["behind"]);
    }

    #[tokio::test]
    async fn dead_dispatcher_returns_rows_to_waiting_and_resume_spawns_a_new_one() {
        let (_root, mut app, deliveries) = queue_fixture().await;
        let mut rx = capture_work(&mut app);
        drop(deliveries);
        app.submit("first".into());
        assert!(app.paused && app.queue_sender.is_none());
        assert!(app.queue[0].in_flight.is_none());
        let notices = notices(&app, &mut rx).await;
        assert!(
            notices
                .iter()
                .any(|notice| notice.starts_with("Could not deliver"))
        );
        app.command(Command::Resume);
        app.tick();
        // The new dispatcher reaches the runtime: the row commits.
        let work = recv(&mut rx).await;
        app.work(work);
        assert!(app.queue.is_empty());
        assert_eq!(app.history, ["first"]);
        app.session().unwrap().shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn switching_sessions_keeps_every_row_until_its_receipt_arrives() {
        let (_root, mut app) = fixture().await;
        let mut rx = capture_work(&mut app);
        app.status = crate::tui::status::StatusLog::new(app.tx.clone());
        app.operation = true;
        app.submit("committed".into());
        app.submit(image_row());
        // Both rows are claimed, but neither receipt is seen before the switch.
        let receipts = [recv(&mut rx).await, recv(&mut rx).await];
        app.submit("unsent".into());
        app.switch(None);
        let ready = next_lifecycle(&mut rx).await;
        assert!(matches!(ready, Work::SessionReady { result: Ok(None) }));
        app.set_session(None);
        assert!(app.paused);
        let in_flight = app.queue.iter().map(|input| input.in_flight.is_some());
        assert_eq!(in_flight.collect::<Vec<_>>(), [true, true, false]);

        receipts.into_iter().for_each(|receipt| app.work(receipt));
        // The old session's commit is not this session's history; its failure is retained.
        assert!(app.history.is_empty() && app.paused);
        let queued = app.queue.iter();
        let queued =
            queued.map(|input| (input.submission.text.as_str(), input.in_flight.is_some()));
        assert_eq!(
            queued.collect::<Vec<_>>(),
            [("image", false), ("unsent", false)]
        );
        let notices = notices(&app, &mut rx).await;
        assert!(
            notices
                .iter()
                .any(|notice| notice.contains("was not submitted"))
        );
    }

    #[tokio::test]
    async fn editing_a_dispatched_row_withdraws_it_unless_it_is_already_being_submitted() {
        let (_root, mut app, mut deliveries) = queue_fixture().await;
        let mut rx = capture_work(&mut app);
        app.submit("editable".into());
        let editable = deliveries.try_recv().unwrap();
        app.command(Command::Queue);
        app.choose();
        assert!(app.queue.is_empty() && app.paused);
        assert_eq!(app.editor.text(), "editable");
        assert!(editable[0].prompt.cancellation.cancel());

        app.editor.set("draft".into());
        app.command(Command::Resume);
        app.submit("claimed".into());
        let mut claimed = deliveries.try_recv().unwrap();
        // The runtime claims an input exactly when cancellation can no longer win.
        let prompt = claimed.pop().unwrap().prompt;
        let session = app.session().unwrap().clone();
        for receipt in session.enqueue_prompts(vec![prompt]).await {
            receipt.await.unwrap().unwrap();
        }
        app.command(Command::Queue);
        app.choose();
        assert_eq!(app.queue.len(), 1);
        assert_eq!(app.editor.text(), "draft");
        let notices = notices(&app, &mut rx).await;
        assert!(
            notices
                .iter()
                .any(|notice| notice.contains("already being submitted"))
        );
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn queue_waits_for_the_initial_input_to_commit() {
        let (_root, mut app, mut deliveries) = queue_fixture().await;
        let root = app.root_agent().clone();
        app.initial_input = Some((root.clone(), 0));
        app.submit("followup".into());
        app.tick();
        assert!(deliveries.try_recv().is_err());
        let mut committed = app.snapshot.records.values().next_back().unwrap().clone();
        committed.sequence += 1;
        committed.id = EventId::generate().unwrap();
        let text = "initial".into();
        let message = skyhook::provider::protocol::Message::User(vec![UserContent::Text { text }]);
        committed.event = SessionEvent::MessageCommitted { message };
        // Only a later user record from the bound root releases the gate.
        let mut other = committed.clone();
        other.agent = AgentId::root(SessionId::generate().unwrap());
        let mut status = committed.clone();
        status.event = SessionEvent::Status {
            message: "status".into(),
        };
        for ignored in [&other, &status] {
            assert!(!observe_initial_input(&mut app.initial_input, ignored));
        }
        app.initial_input = Some((root.clone(), committed.sequence));
        assert!(!observe_initial_input(&mut app.initial_input, &committed));
        app.initial_input = Some((root, 0));
        app.observe(ObservedEvent {
            revision: app.snapshot.revision + 1,
            event: RuntimeEvent::Record(Box::new(committed)),
        });
        assert!(app.initial_input.is_none());
        app.tick();
        assert_eq!(texts(&deliveries.try_recv().unwrap()), ["followup"]);
    }
}
