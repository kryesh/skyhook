use super::*;
use skyhook::agent::{HarnessError, QueuedPrompt, QueuedPromptCancellation};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueuedInputId(u64);

/// Where a queued row is on its way to the session.
pub(super) enum RowState {
    Waiting,
    /// Dispatched; the cancellation withdraws it while its commit is pending.
    InFlight(QueuedPromptCancellation),
}

/// One composer submission waiting behind the current operation.
pub struct QueuedInput {
    pub(super) id: QueuedInputId,
    /// Advanced by every dispatch and withdrawal, so a late receipt is ignored.
    pub(super) generation: u64,
    pub(super) state: RowState,
    pub(super) submission: Submission,
    pub(super) model: ModelRef,
    pub(super) mode: ModeName,
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
                            let result = receipt.await;
                            let revision = session.observe().snapshot.revision;
                            Work::QueueCommitted { id, generation, revision, result }
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
        self.follow_latest();
        self.reject_pending_questions();
        self.advance_draft();
        let queued = self.queued_input(submission);
        if self.busy() || self.paused || !self.queue.is_empty() {
            self.queue.push_back(queued);
            self.refresh_menu();
            self.deliver_queue();
            self.dirty = true;
            return;
        }
        self.send_input(queued);
    }
    pub(super) fn send_input(&mut self, queued: QueuedInput) {
        let Some(session) = self.session().cloned() else {
            return self.begin_session(PendingStart::Input(Box::new(queued)));
        };
        self.commit_row(&queued);
        let QueuedInput {
            submission: Submission { text, attachments },
            model,
            mode,
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
                .unwrap_or_default(),
        ));
        let tx = self.tx.clone();
        let status = self.status.clone();
        tokio::spawn(async move {
            status.flush().await;
            let result = match session.selection(Some(&model), Some(&mode)) {
                Ok(selection) => {
                    session
                        .prompt_with_options(text, &attachments, selection)
                        .await
                }
                Err(error) => Err(error),
            };
            let _ = tx.send(Work::Done {
                result: result.map(|_| ()).map_err(Into::into),
            });
        });
        self.dirty = true;
    }
    fn remember_selection(&mut self, model: &ModelRef, mode: &ModeName) {
        let selection = (Some(model.clone()), Some(mode.clone()));
        if (&self.remembered_model, &self.remembered_mode) == (&selection.0, &selection.1) {
            return;
        }
        (self.remembered_model, self.remembered_mode) = selection.clone();
        let notices = self.root_notifier();
        let workspace = self.launch.workspace.clone();
        tokio::task::spawn_blocking(move || {
            let saved = state::update(&workspace, |state| {
                (state.model, state.mode) = selection;
            });
            if let Err(error) = saved {
                notices.send(format!("Could not save model and mode selection: {error}"));
            }
        });
    }
    /// A committed or directly sent row goes through ordinary history bookkeeping.
    pub(super) fn commit_row(&mut self, input: &QueuedInput) {
        self.history.push(input.submission.text.clone());
        self.history_browse = None;
        self.set_title(&input.submission.text);
        self.remember_selection(&input.model, &input.mode);
    }
    /// Withdraw every dispatched row that is still cancellable.
    pub(super) fn cancel_queue_delivery(&mut self) {
        for input in &mut self.queue {
            if matches!(&input.state, RowState::InFlight(row) if row.cancel()) {
                input.state = RowState::Waiting;
                input.generation = input.generation.wrapping_add(1);
            }
        }
    }
    /// Stop dispatch and withdraw what can still be withdrawn.
    pub(super) fn pause_queue(&mut self) {
        self.paused = true;
        self.cancel_queue_delivery();
    }

    pub(super) fn deliver_queue(&mut self) {
        // A session its failed journal stopped takes nothing more.
        if self.snapshot.phase.failure().is_some() {
            self.queue.clear();
            return;
        }
        // Lag recovery replaces the snapshot without replaying each event. Only
        // records after the gate can release it.
        if let Some(&(_, after)) = self.initial_input.as_ref() {
            let after = (std::ops::Bound::Excluded(after), std::ops::Bound::Unbounded);
            for record in self.snapshot.records.range(after).map(|(_, record)| record) {
                if observe_initial_input(&mut self.initial_input, record) {
                    break;
                }
            }
        }
        if self.start.is_creating() || self.stopping || self.paused || self.queue.is_empty() {
            return;
        }
        let Some(session) = self.session().cloned() else {
            // Retain the head across creation, then register the entire queue.
            let input = self.queue.pop_front().unwrap();
            self.refresh_menu();
            return self.begin_session(PendingStart::QueuedInput(Box::new(input)));
        };
        if self.initial_input.is_some() {
            return;
        }
        let mut deliveries = Vec::new();
        let mut rejected = None;
        let queue = self.queue.iter_mut();
        for input in queue {
            if matches!(input.state, RowState::InFlight(_)) {
                continue;
            }
            // A row the session cannot take holds itself and the rows behind it back.
            let options = match session.selection(Some(&input.model), Some(&input.mode)) {
                Ok(options) => options,
                Err(error) => {
                    rejected = Some(error);
                    break;
                }
            };
            let cancellation = QueuedPromptCancellation::default();
            input.generation = input.generation.wrapping_add(1);
            input.state = RowState::InFlight(cancellation.clone());
            deliveries.push(QueueDelivery {
                id: input.id,
                generation: input.generation,
                prompt: QueuedPrompt {
                    text: input.submission.text.clone(),
                    attachments: input.submission.attachments.clone(),
                    options,
                    cancellation,
                },
            });
        }
        if let Some(error) = rejected {
            self.paused = true;
            self.notice(format!(
                "Queued message was not submitted: {error}. Resume to retry."
            ));
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
    pub(super) fn queue_committed(
        &mut self,
        id: QueuedInputId,
        generation: u64,
        revision: u64,
        result: Result<(), HarnessError>,
    ) {
        let Some(index) = self.queue.iter().position(|input| {
            input.id == id
                && input.generation == generation
                && matches!(input.state, RowState::InFlight(_))
        }) else {
            return;
        };
        match result {
            Ok(()) => {
                let input = self.queue.remove(index).unwrap();
                self.queue_activity_revision = self.queue_activity_revision.max(revision);
                self.commit_row(&input);
            }
            Err(error) => {
                self.queue[index].state = RowState::Waiting;
                self.pause_queue();
                // A held-back row is reported by whatever held it back, and a
                // failed journal by the session it stopped.
                if matches!(error, HarnessError::Interrupted) || error.journal_failed() {
                    return self.refresh_menu();
                }
                self.notice(format!(
                    "Queued message was not submitted: {error}. Resume to retry."
                ));
            }
        }
        self.refresh_menu();
    }
    pub(super) fn queued_input(&mut self, submission: Submission) -> QueuedInput {
        self.next_queued_id.0 += 1;
        QueuedInput {
            id: self.next_queued_id,
            generation: 0,
            state: RowState::Waiting,
            submission,
            model: self.model.clone(),
            mode: self.mode.clone(),
        }
    }
    pub(super) fn queue_items(&self) -> Vec<Item<QueuedInputId>> {
        self.queue
            .iter()
            .map(|queued| {
                let profile = self.launch.model.config().model(&queued.model);
                Item::new(
                    queued.id,
                    brief(&queued.submission.text, TITLE_CHARS),
                    match profile {
                        Some(profile) => profile.model.to_string(),
                        None => queued.model.to_string(),
                    },
                )
            })
            .collect()
    }
    /// Take a row out of the queue, withdrawing its dispatch if one is pending.
    pub(super) fn remove_queued(&mut self, id: QueuedInputId) -> Option<QueuedInput> {
        let index = self.queue.iter().position(|queued| queued.id == id)?;
        if matches!(&self.queue[index].state, RowState::InFlight(row) if !row.cancel()) {
            self.notice("This message is already being submitted");
            return None;
        }
        let mut input = self.queue.remove(index).unwrap();
        input.state = RowState::Waiting;
        Some(input)
    }
}

#[cfg(test)]
mod tests {
    use super::super::played::*;
    use super::super::tests::*;
    use super::*;
    use skyhook::session::RecordSeq;

    type Deliveries = mpsc::UnboundedReceiver<Vec<QueueDelivery>>;

    /// A busy fixture whose dispatches the test receives instead of a runtime.
    async fn queue_fixture() -> (tempfile::TempDir, App, Deliveries) {
        let (root, mut app) = fixture().await;
        let deliveries = busy(&mut app);
        (root, app, deliveries)
    }

    /// Make `app` busy, its dispatches received by the test instead of a runtime.
    fn busy(app: &mut App) -> Deliveries {
        let (tx, rx) = mpsc::unbounded_channel();
        app.queue_sender = Some(tx);
        app.operation = true;
        rx
    }

    /// Every notice so far: journaled with a session, otherwise sent back as work.
    async fn notices(app: &App, rx: &mut mpsc::UnboundedReceiver<Work>) -> Vec<String> {
        app.status.flush().await;
        let mut notices = Vec::new();
        if let Some(session) = app.session() {
            let records = session.observe().snapshot.records;
            notices.extend(records.values().filter_map(|record| match &record.event {
                SessionEvent::Status { message } => Some(message.clone()),
                _ => None,
            }));
        }
        while let Ok(work) = rx.try_recv() {
            if let Work::Unsaved { message, .. } = work {
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
            id: row.id,
            generation: row.generation,
            revision: app.snapshot.revision + 1,
            result,
        });
    }

    /// Submitting follows the latest activity, even from a view pinned by a text click.
    #[tokio::test]
    async fn submission_follows_latest_from_a_pinned_view() {
        use skyhook::provider::protocol::AssistantItem;
        let (_root, mut app, mut model, _session) = played_fixture().await;
        let text = "ordinary prose\n".repeat(100);
        let answer = vec![AssistantItem::text("pin", 0, &text)];
        exchange(&mut app, &mut model, "question", answer).await;
        let _deliveries = busy(&mut app);
        app.refresh();
        let screen = draw(&mut app);
        let (x, y) = (screen.lines().enumerate())
            .find_map(|(y, line)| Some((line[..line.find("ordinary")?].chars().count(), y)))
            .unwrap();
        click(&mut app, Rect::new(x as u16, y as u16, 1, 1));
        assert!(app.view().scroll.is_some() && app.selection.is_some());
        app.submit("next".into());
        assert!(app.view().scroll.is_none() && app.selection.is_none());
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
        let model = first[0]
            .prompt
            .options
            .model
            .as_ref()
            .map(|model| model.name().to_string());
        assert_eq!(model.as_deref(), Some("test/first"));
        assert!(
            app.queue
                .iter()
                .all(|input| matches!(input.state, RowState::InFlight(_)))
        );
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
        assert!(
            app.queue
                .iter()
                .all(|input| matches!(input.state, RowState::Waiting))
        );
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
        assert!(
            app.queue
                .iter()
                .all(|input| matches!(input.state, RowState::Waiting))
        );
        let notices = notices(&app, &mut rx).await;
        // Only the claimed row reports; the rows it held back stay quiet.
        let failed = notices
            .iter()
            .filter(|notice| notice.contains("was not submitted"));
        assert_eq!(failed.count(), 1);
        app.session().unwrap().shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn dead_dispatcher_returns_rows_to_waiting_and_resume_spawns_a_new_one() {
        let (_root, mut app, deliveries) = queue_fixture().await;
        let mut rx = capture_work(&mut app);
        drop(deliveries);
        app.submit("first".into());
        assert!(app.paused && app.queue_sender.is_none());
        assert!(matches!(app.queue[0].state, RowState::Waiting));
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
            receipt.await.unwrap();
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
        let (_root, mut app, _model, _session) = played_fixture().await;
        let _asked = ask(app.session().unwrap(), "initial");
        // The root's input, committed while the played model holds its turn open.
        let input = committed(user);
        let first = |app: &App| {
            let mut records = app.snapshot.records.values();
            records.find(|record| input(&record.event)).cloned()
        };
        let committed = observe_find(&mut app, first).await;
        let mut deliveries = busy(&mut app);
        let root = app.root_agent().clone();
        // Bound to the input just committed, which comes before what releases it.
        app.initial_input = Some((root.clone(), committed.sequence));
        app.submit("followup".into());
        app.tick();
        assert!(deliveries.try_recv().is_err());
        // Only a later user record from the bound root releases the gate.
        assert!(!observe_initial_input(&mut app.initial_input, &committed));
        app.initial_input = Some((root.clone(), RecordSeq::default()));
        let child = EventRecord {
            agent: root.child(1),
            ..committed.clone()
        };
        let status = EventRecord {
            event: SessionEvent::Status {
                message: "status".into(),
            },
            ..committed.clone()
        };
        for ignored in [&child, &status] {
            assert!(!observe_initial_input(&mut app.initial_input, ignored));
        }
        app.observe(ObservedEvent {
            revision: app.snapshot.revision + 1,
            event: RuntimeEvent::Record(Box::new(committed)),
        });
        assert!(app.initial_input.is_none());
        app.tick();
        assert_eq!(texts(&deliveries.try_recv().unwrap()), ["followup"]);
    }
}
