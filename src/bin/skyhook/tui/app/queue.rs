use super::*;
use crate::tui::format::model_label;
use skyhook::agent::{HarnessError, QueuedPrompt, QueuedPromptCancellation, QueuedPromptCommitted};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueuedInputId(u64);

/// Where a queued row is on its way to the session.
pub(super) enum RowState {
    Waiting,
    /// Dispatched; the cancellation withdraws it while its commit is pending.
    InFlight(QueuedPromptCancellation),
    /// Taken back into the composer to edit: it keeps its place, and is never
    /// sent, until the edit is submitted, which replaces it, or discarded, which
    /// restores it.
    Editing,
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

/// Why queue delivery is paused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Paused {
    /// Something stopped what the queue was sending: a failure, an interrupt or
    /// the session. `/retry` resumes it.
    Stopped,
    /// The user took a row back to edit it: nothing queued or held is sent until
    /// the edit is submitted or discarded, whatever stops it meanwhile. `stopped`
    /// says whether something also stopped the queue, which holds it once the
    /// edit ends.
    Editing { stopped: bool },
}

/// How a dispatched row's receipt ended.
pub enum Delivered {
    /// Committed under this model and mode.
    Committed(QueuedPromptCommitted),
    /// Handed back: whatever held it reports why, such as a failed turn, an
    /// interrupt, or the session, which reports its own stop.
    HeldBack,
    /// Refused before any turn took it, which nothing else reports.
    Refused(HarnessError),
}

impl From<Result<QueuedPromptCommitted, HarnessError>> for Delivered {
    fn from(result: Result<QueuedPromptCommitted, HarnessError>) -> Self {
        match result {
            Ok(committed) => Self::Committed(committed),
            Err(HarnessError::Interrupted | HarnessError::Session(_)) => Self::HeldBack,
            Err(error) => Self::Refused(error),
        }
    }
}

pub(super) struct QueueDelivery {
    pub(super) id: QueuedInputId,
    pub(super) generation: u64,
    pub(super) prompt: QueuedPrompt,
}

/// Register each UI queue snapshot as one request-boundary batch, in order,
/// after the notices sent before it.
pub(super) async fn queue_dispatcher(
    session: SessionHandle,
    status: crate::tui::status::StatusLog,
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
                    status.flush().await;
                    let receipts = session.enqueue_prompts(prompts).await;
                    for ((id, generation), receipt) in ids.into_iter().zip(receipts) {
                        let session = session.clone();
                        pending.push(async move {
                            let delivered = Delivered::from(receipt.await);
                            let revision = session.store().revision();
                            Work::QueueCommitted { id, generation, revision, delivered }
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
        // An edited message takes its row's place.
        match self.edited() {
            Some(at) => {
                self.own_queue()[at] = queued;
                self.end_edit();
            }
            None => self.own_queue().push_back(queued),
        }
        self.refresh_menu();
        self.deliver_queue();
        self.dirty = true;
    }

    /// The queue to change. Rows the session holds for `/retry` are taken back
    /// first, waiting under the queue's pause: once the queue changes, it alone
    /// orders them. A row already being submitted stays where it is.
    pub(super) fn own_queue(&mut self) -> &mut VecDeque<QueuedInput> {
        for input in &mut self.queue {
            if matches!(&input.state, RowState::InFlight(row) if row.is_retained() && row.cancel())
            {
                input.state = RowState::Waiting;
                input.generation = input.generation.wrapping_add(1);
            }
        }
        &mut self.queue
    }

    /// Take row `id` back to edit, holding the queue until the edit is done, and
    /// return what to edit. An edit already open goes back to its place, edited
    /// or not, and any other draft is queued first.
    pub(super) fn begin_edit(&mut self, id: QueuedInputId) -> Option<Submission> {
        let queue = self.own_queue();
        let index = queue.iter().position(|input| input.id == id)?;
        if matches!(&queue[index].state, RowState::InFlight(row) if !row.cancel()) {
            self.toast("This message is already being submitted");
            return None;
        }
        let draft = self.editor.take();
        match self.edited() {
            Some(at) if draft.is_empty() => self.queue[at].state = RowState::Waiting,
            Some(at) => self.queue[at] = self.queued_input(draft),
            None if draft.is_empty() => {}
            None => {
                let draft = self.queued_input(draft);
                self.queue.push_front(draft);
            }
        }
        let stopped = matches!(
            self.paused,
            Some(Paused::Stopped | Paused::Editing { stopped: true })
        );
        let input = self.queue.iter_mut().find(|input| input.id == id)?;
        input.state = RowState::Editing;
        input.generation = input.generation.wrapping_add(1);
        let submission = input.submission.clone();
        self.paused = None;
        self.pause_queue(Paused::Editing { stopped });
        Some(submission)
    }

    /// Discard an open edit: its row goes back as it was.
    pub(super) fn discard_edit(&mut self) {
        if let Some(at) = self.edited() {
            self.own_queue()[at].state = RowState::Waiting;
        }
        self.end_edit();
    }

    /// End an open edit, leaving the queue as a stop meanwhile left it.
    fn end_edit(&mut self) {
        if let Some(Paused::Editing { stopped }) = self.paused {
            self.paused = stopped.then_some(Paused::Stopped);
        }
    }

    /// The place of the row being edited.
    fn edited(&self) -> Option<usize> {
        (self.queue.iter()).position(|input| matches!(input.state, RowState::Editing))
    }
    fn remember_selection(&mut self, model: &ModelRef, mode: &ModeName) {
        let selection = (Some(model.clone()), Some(mode.clone()));
        if (&self.remembered_model, &self.remembered_mode) == (&selection.0, &selection.1) {
            return;
        }
        (self.remembered_model, self.remembered_mode) = selection.clone();
        let tx = self.tx.clone();
        let workspace = self.launch.workspace.clone();
        tokio::task::spawn_blocking(move || {
            let saved = state::update(&workspace, |state| {
                (state.model, state.mode) = selection;
            });
            if let Err(error) = saved {
                let message = format!("Could not save model and mode selection: {error}");
                let _ = tx.send(Work::Toast(message));
            }
        });
    }
    /// A committed row goes through ordinary history bookkeeping, remembering
    /// the model and mode it committed under.
    pub(super) fn commit_row(&mut self, input: &QueuedInput, committed: QueuedPromptCommitted) {
        self.history.push(input.submission.text.clone());
        self.history_browse = None;
        let mode = committed.mode.unwrap_or_else(|| input.mode.clone());
        self.remember_selection(&committed.model, &mode);
    }
    /// Whether the session holds rows with a failed turn for `/retry`.
    pub(super) fn holds_rows(&self) -> bool {
        (self.queue.iter())
            .any(|input| matches!(&input.state, RowState::InFlight(row) if row.is_retained()))
    }

    /// Withdraw every dispatched row that is still cancellable, except those the
    /// session holds for `/retry`.
    pub(super) fn cancel_queue_delivery(&mut self) {
        for input in &mut self.queue {
            if matches!(&input.state, RowState::InFlight(row) if !row.is_retained() && row.cancel())
            {
                input.state = RowState::Waiting;
                input.generation = input.generation.wrapping_add(1);
            }
        }
    }
    /// Pause delivery for `why`; an open edit holds it whatever else does,
    /// remembering a stop for when the edit ends.
    pub(super) fn pause(&mut self, why: Paused) {
        match &mut self.paused {
            Some(Paused::Editing { stopped }) => *stopped |= why == Paused::Stopped,
            paused => *paused = Some(why),
        }
    }

    /// Stop dispatch for `why` and withdraw what can still be withdrawn.
    pub(super) fn pause_queue(&mut self, why: Paused) {
        self.pause(why);
        self.cancel_queue_delivery();
    }

    /// Resume delivery the queue stopped for, re-sending its waiting rows as one
    /// batch on the bottom bar's model and mode, as `/retry` does; whether any.
    pub(super) fn resume_stopped_queue(&mut self) -> bool {
        let waiting = |input: &&mut QueuedInput| matches!(input.state, RowState::Waiting);
        let (model, mode) = (self.model.clone(), self.mode.clone());
        let mut resumed = false;
        if self.paused == Some(Paused::Stopped) {
            for input in self.queue.iter_mut().filter(waiting) {
                (input.model, input.mode) = (model.clone(), mode.clone());
                resumed = true;
            }
        }
        if resumed {
            self.paused = None;
            self.deliver_queue();
        }
        resumed
    }

    /// A failed root turn, however it started, holds the input queued behind
    /// it. Every record since the last read is checked, so a failure a lagged
    /// update stream skipped still pauses before anything more is delivered.
    pub(super) fn hold_after_root_failure(&mut self) {
        if std::mem::take(&mut self.root_failed) {
            self.pause_queue(Paused::Stopped);
        }
    }

    pub(super) fn deliver_queue(&mut self) {
        // A session its failed journal stopped takes nothing more.
        if self.snapshot.phase.failure().is_some() {
            self.queue.clear();
            return;
        }
        if self.creating.is_some()
            || self.stopping
            || self.paused.is_some()
            || self.queue.is_empty()
        {
            return;
        }
        let Some(session) = self.session().cloned() else {
            // Retain the head across creation, then register the entire queue.
            let input = self.queue.pop_front().unwrap();
            self.refresh_menu();
            return self.begin_session(PendingStart::Input(Box::new(input)));
        };
        let waiting = |input: &&mut QueuedInput| matches!(input.state, RowState::Waiting);
        // The waiting rows are one batch: a row the session cannot take holds
        // every one of them back.
        let selections = (self.queue.iter_mut().filter(waiting))
            .map(|input| session.selection(Some(&input.model), Some(&input.mode)))
            .collect::<Result<Vec<_>, _>>();
        let selections = match selections {
            Ok(selections) => selections,
            Err(error) => {
                self.pause(Paused::Stopped);
                self.not_submitted(&error);
                // The session's root model is gone: choosing one sends the queue.
                if matches!(error, HarnessError::UnknownModel(_)) {
                    self.command(crate::tui::keys::Command::Model);
                }
                return;
            }
        };
        let mut deliveries = Vec::new();
        for (input, options) in self.queue.iter_mut().filter(waiting).zip(selections) {
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
        if deliveries.is_empty() {
            return;
        }
        let sender = self.queue_sender.get_or_insert_with(|| {
            let (tx, rx) = mpsc::unbounded_channel();
            let status = self.status.clone();
            tokio::spawn(queue_dispatcher(session, status, rx, self.tx.clone()));
            tx
        });
        if sender.send(deliveries).is_err() {
            // Nothing reached the runtime: every row is waiting again.
            self.queue_sender = None;
            self.pause_queue(Paused::Stopped);
            self.notice("Could not deliver queued messages; resume to retry");
        }
    }
    pub(super) fn queue_committed(
        &mut self,
        id: QueuedInputId,
        generation: u64,
        revision: u64,
        delivered: Delivered,
    ) {
        let Some(index) = self.queue.iter().position(|input| {
            input.id == id
                && input.generation == generation
                && matches!(input.state, RowState::InFlight(_))
        }) else {
            return;
        };
        match delivered {
            Delivered::Committed(committed) => {
                let input = self.queue.remove(index).unwrap();
                self.queue_activity_revision = self.queue_activity_revision.max(revision);
                self.commit_row(&input, committed);
            }
            Delivered::HeldBack => {
                self.queue[index].state = RowState::Waiting;
                self.pause_queue(Paused::Stopped);
            }
            Delivered::Refused(error) => {
                self.queue[index].state = RowState::Waiting;
                self.pause_queue(Paused::Stopped);
                self.not_submitted(&error);
            }
        }
        self.refresh_menu();
    }
    fn not_submitted(&self, error: &HarnessError) {
        let error = crate::tui::format::harness_error(error);
        self.notice(format!(
            "Queued message was not submitted: {error}. Send it again with /retry."
        ));
    }
    /// Send what follows on `model`. A session whose root's model the
    /// configuration no longer has activates on it first, then sends the queue
    /// that model stopped.
    pub(super) fn choose_model(&mut self, model: ModelRef) {
        let config = self.launch.model.config();
        let root = (self.projection.agents.iter()).find(|agent| &agent.id == self.root_agent());
        let gone = root.is_some_and(|root| config.model(&root.model).is_none());
        self.model.clone_from(&model);
        let Some(session) = self.session().filter(|_| gone).cloned() else {
            return;
        };
        let (tx, activating) = (self.tx.clone(), self.tasks.clone());
        tokio::spawn(async move {
            let activated = session.activate_on(&model).await;
            drop(activating);
            let _ = tx.send(Work::Activated(activated));
        });
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
            .filter(|queued| !matches!(queued.state, RowState::Editing))
            .map(|queued| {
                let profile = self.launch.model.config().model(&queued.model);
                Item::new(
                    queued.id,
                    brief(&queued.submission.text, TITLE_CHARS),
                    model_label(&queued.model, profile.map(|profile| &profile.model)),
                )
            })
            .collect()
    }
    /// Take a row out of the queue, withdrawing its dispatch if one is pending.
    pub(super) fn remove_queued(&mut self, id: QueuedInputId) -> Option<QueuedInput> {
        let queue = self.own_queue();
        let index = queue.iter().position(|queued| queued.id == id)?;
        if matches!(&queue[index].state, RowState::InFlight(row) if !row.cancel()) {
            self.toast("This message is already being submitted");
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
    use std::sync::Arc;

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
            let records = journal(session).await;
            notices.extend(records.iter().filter_map(|record| match &record.event {
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

    async fn image_row() -> Submission {
        // The fixture's model does not support images: a claimed row that fails.
        Submission {
            text: "image".into(),
            attachments: vec![png_attachment("image.png").await],
        }
    }

    fn texts(batch: &[QueueDelivery]) -> Vec<&str> {
        batch.iter().map(|row| row.prompt.text.as_str()).collect()
    }

    /// What a batch committed under, as the fixture's model reports it.
    fn committed() -> QueuedPromptCommitted {
        let model = "test/first".parse().unwrap();
        QueuedPromptCommitted { model, mode: None }
    }

    fn receipt(app: &mut App, row: &QueueDelivery, delivered: Delivered) {
        app.work(Work::QueueCommitted {
            id: row.id,
            generation: row.generation,
            revision: app.snapshot.revision + 1,
            delivered,
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
        assert!(app.view().top.is_some() && app.selection.is_some());
        app.submit("next".into());
        assert!(app.view().top.is_none() && app.selection.is_none());
    }

    #[tokio::test]
    async fn busy_submissions_dispatch_in_order_and_leave_on_their_receipt() {
        let (_root, mut app, mut deliveries) = queue_fixture().await;
        app.submit(Submission {
            text: "first".into(),
            attachments: vec![png_attachment("first.png").await],
        });
        app.submit("second".into());
        let first = deliveries.try_recv().unwrap();
        let second = deliveries.try_recv().unwrap();
        assert_eq!(
            (texts(&first), texts(&second)),
            (vec!["first"], vec!["second"])
        );
        assert_eq!(
            first[0].prompt.attachments,
            [png_attachment("first.png").await]
        );
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

        receipt(&mut app, &first[0], Delivered::Committed(committed()));
        assert_eq!(app.history, ["first"]);
        assert_eq!(app.queue.len(), 1);
        // A commit is not a turn completion, and an idle enqueue stays busy
        // until the observed activity catches up.
        assert!(app.operation);
        app.operation = false;
        receipt(&mut app, &second[0], Delivered::Committed(committed()));
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
        app.pause_queue(Paused::Stopped);
        assert!(
            app.queue
                .iter()
                .all(|input| matches!(input.state, RowState::Waiting))
        );
        assert!(stale[0].prompt.cancellation.cancel());
        // Neither outcome of a withdrawn dispatch touches the row.
        receipt(&mut app, &stale[0], Delivered::Committed(committed()));
        receipt(&mut app, &stale[0], Delivered::HeldBack);
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
    async fn a_refused_batch_stays_pending_for_retry_and_its_failed_turn_reports_it_once() {
        let (_root, mut app) = fixture().await;
        let _rx = capture_work(&mut app);
        app.paused = Some(Paused::Stopped);
        app.submit(image_row().await);
        app.submit("second".into());
        app.submit("third".into());
        app.command(Command::Resume);
        app.tick();
        let refusal = "does not support image inputs";
        bounded(async {
            while shown(&app, refusal).await == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert_eq!(shown(&app, refusal).await, 1);
        // The session holds the whole batch for /retry: the queue pauses without
        // taking any row back.
        resubscribed(&mut app).await;
        assert!(app.paused.is_some() && app.history.is_empty());
        assert_eq!(app.queue.len(), 3);
        assert!(
            app.queue
                .iter()
                .all(|input| matches!(&input.state, RowState::InFlight(row) if row.is_retained()))
        );
        // Editing a held row withdraws it and the rows behind it, which then follow
        // it; while the edit is open, neither a later failure nor /retry sends them.
        let editing = app.queue[0].submission.text.clone();
        app.command(Command::Queue);
        app.choose();
        assert_eq!(app.editor.text(), editing);
        app.pause_queue(Paused::Stopped);
        app.command(Command::Retry);
        let toast = app.toast.take().map(|(toast, _)| toast);
        assert!(toast.is_some_and(|toast| toast.starts_with("Submit the message you are editing")));
        assert!(matches!(app.paused, Some(Paused::Editing { .. })));
        assert!(matches!(app.queue[0].state, RowState::Editing));
        assert!(
            app.queue
                .iter()
                .skip(1)
                .all(|input| matches!(input.state, RowState::Waiting))
        );
        // Submitted, the edit goes back ahead of the rows behind it; the stop still
        // holds them all.
        let edited = app.editor.take();
        app.submit(edited);
        let texts: Vec<_> = (app.queue.iter())
            .map(|row| row.submission.text.as_str())
            .collect();
        assert_eq!(texts, ["image", "second", "third"]);
        app.tick();
        assert_eq!(app.paused, Some(Paused::Stopped));
        assert!(
            app.queue
                .iter()
                .all(|row| matches!(row.state, RowState::Waiting))
        );
        // /retry sends them again as one batch, on the bottom bar's model.
        app.command(Command::Retry);
        assert_eq!(app.paused, None);
        assert!(
            app.queue
                .iter()
                .all(|row| matches!(row.state, RowState::InFlight(_)) && row.model == app.model)
        );
        app.session().unwrap().shutdown().await.unwrap();
    }

    /// A receipt the session's own stop resolves is reported by the session; only
    /// a refusal no turn took is the row's to report.
    #[test]
    fn receipts_parse_into_who_reports_them() {
        use skyhook::session::{SessionError, SessionPhase};
        assert!(matches!(
            Delivered::from(Ok(committed())),
            Delivered::Committed(_)
        ));
        for held in [
            HarnessError::Interrupted,
            HarnessError::Session(SessionError::Refused(SessionPhase::ShuttingDown)),
            HarnessError::Session(SessionError::Refused(SessionPhase::Closed)),
        ] {
            assert!(matches!(Delivered::from(Err(held)), Delivered::HeldBack));
        }
        let refused = Delivered::from(Err(HarnessError::ImageLimit));
        assert!(matches!(
            refused,
            Delivered::Refused(HarnessError::ImageLimit)
        ));
    }

    #[tokio::test]
    async fn dead_dispatcher_returns_rows_to_waiting_and_resume_spawns_a_new_one() {
        let (_root, mut app, deliveries) = queue_fixture().await;
        let mut rx = capture_work(&mut app);
        drop(deliveries);
        app.submit("first".into());
        assert!(app.paused.is_some() && app.queue_sender.is_none());
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

    /// Editing a row holds the queue: `/retry` leaves it paused and nothing is
    /// sent. The edit keeps its place whatever happens to the rows around it.
    #[tokio::test]
    async fn an_edit_holds_the_queue_and_keeps_its_place() {
        let (_root, mut app, mut deliveries) = queue_fixture().await;
        for text in ["before", "edited", "behind", "last"] {
            app.submit(text.into());
        }
        while deliveries.try_recv().is_ok() {}
        let id = |app: &App, text: &str| {
            let mut rows = app.queue.iter();
            rows.find(|row| row.submission.text == text).unwrap().id
        };
        let texts = |app: &App| {
            let rows = app.queue.iter();
            rows.map(|row| row.submission.text.clone())
                .collect::<Vec<_>>()
        };
        assert!(app.begin_edit(id(&app, "edited")).is_some());
        app.command(Command::Retry);
        app.tick();
        assert!(matches!(app.paused, Some(Paused::Editing { .. })));
        assert!(matches!(app.queue[1].state, RowState::Editing));
        assert!(deliveries.try_recv().is_err());
        for removed in ["behind", "before"] {
            assert!(app.remove_queued(id(&app, removed)).is_some());
        }
        app.submit("edited again".into());
        assert_eq!(texts(&app), ["edited again", "last"]);
        assert!(app.begin_edit(id(&app, "edited again")).is_some());
        assert!(app.remove_queued(id(&app, "last")).is_some());
        app.submit("final".into());
        assert_eq!(texts(&app), ["final"]);
        assert_eq!(app.paused, None);
    }

    #[tokio::test]
    async fn editing_a_dispatched_row_withdraws_it_unless_it_is_already_being_submitted() {
        let (_root, mut app, mut deliveries) = queue_fixture().await;
        app.submit("editable".into());
        let editable = deliveries.try_recv().unwrap();
        app.command(Command::Queue);
        app.choose();
        assert!(matches!(app.queue[0].state, RowState::Editing) && app.paused.is_some());
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
        app.menu
            .as_mut()
            .unwrap()
            .filter(|input| input.set("claimed".into()));
        app.choose();
        assert_eq!(app.queue.len(), 2);
        assert_eq!(app.editor.text(), "draft");
        let toast = app.toast.take().map(|(toast, _)| toast);
        assert_eq!(
            toast.as_deref(),
            Some("This message is already being submitted")
        );
        session.shutdown().await.unwrap();
    }

    const REJECTED: &str = "fixture credentials rejected";

    /// A terminal failure, unlike a refused connection, which retries.
    fn rejected() -> skyhook::provider::ProviderError {
        skyhook::provider::ProviderErrorKind::Authentication.error(REJECTED)
    }

    /// How many entries of the root's conversation show `text`, once every
    /// notice sent so far is journaled.
    async fn shown(app: &App, text: &str) -> usize {
        app.status.flush().await;
        let session = app.session().unwrap();
        let read = read(session, app.root_agent()).await;
        let entries = read.entries(&app.outputs, false);
        entries
            .iter()
            .filter(|entry| entry.text().contains(text))
            .count()
    }

    async fn continued(app: &mut App, rx: &mut mpsc::UnboundedReceiver<Work>) -> Work {
        bounded(async {
            loop {
                match recv(rx).await {
                    work @ Work::Continued { .. } => break work,
                    work => app.work(work),
                }
            }
        })
        .await
    }

    fn agent_failed(event: &SessionEvent) -> bool {
        matches!(event, SessionEvent::AgentFailed { .. })
    }

    /// A failed root turn pauses the queue, whether a submission or /retry
    /// started it, and the conversation shows each failure once: its request's
    /// card. A /retry refused before its turn starts reports its own error.
    #[tokio::test]
    async fn a_failed_root_turn_pauses_the_queue_and_is_shown_once() {
        let (_root, mut app, mut model, session) = played_fixture().await;
        let mut rx = capture_work(&mut app);
        let root = app.root_agent().clone();
        app.model = "test/seeing".parse().unwrap();
        app.submit(Submission {
            text: "first".into(),
            attachments: vec![png_attachment("first.png").await],
        });
        model.turn().await.fail(rejected());
        observe_until(&mut app, |app| stopped(&app.snapshot, &root)).await;
        assert!(app.paused.is_some());
        assert_eq!(shown(&app, REJECTED).await, 1);

        app.command(Command::Retry);
        model.turn().await.fail(rejected());
        let work = continued(&mut app, &mut rx).await;
        assert!(matches!(work, Work::Continued { result: Ok(()), .. }));
        app.work(work);
        observe_until(&mut app, |app| {
            count(&app.journal, agent_failed) == 2 && stopped(&app.snapshot, &root)
        })
        .await;
        assert_eq!(shown(&app, REJECTED).await, 2);

        // Swapping to a model without image support refuses the continue.
        app.model = "test/first".parse().unwrap();
        app.command(Command::Retry);
        let work = continued(&mut app, &mut rx).await;
        let Work::Continued {
            result: Err(error), ..
        } = &work
        else {
            panic!("the swap is refused");
        };
        assert!(error.to_string().contains("does not support image inputs"));
        app.work(work);
        assert_eq!(shown(&app, "does not support image inputs").await, 1);
        assert_eq!(shown(&app, REJECTED).await, 2);
        session.shutdown().await.unwrap();
    }

    /// A root parked by a failed turn passes through work it does not take
    /// without failing again.
    #[tokio::test]
    async fn a_withdrawn_row_does_not_fail_a_parked_root_again() {
        let (_root, mut app, mut model, session) = played_fixture().await;
        let mut rx = capture_work(&mut app);
        let root = app.root_agent().clone();
        app.submit("first".into());
        model.turn().await.fail(rejected());
        observe_until(&mut app, |app| {
            stopped(&app.snapshot, &root) && app.paused.is_some()
        })
        .await;
        // Sent, then withdrawn before the runtime claims it.
        app.paused = None;
        app.submit("second".into());
        let row = app.queue.iter().find(|row| row.submission.text == "second");
        let id = row.unwrap().id;
        assert!(app.remove_queued(id).is_some());
        // Its receipt resolves once the session has dropped it, whether or not
        // the root took it as work.
        bounded(async {
            while !matches!(recv(&mut rx).await,
                Work::QueueCommitted { id: committed, .. } if committed == id)
            {}
        })
        .await;
        let busy = || {
            session.store().observe().snapshot.activity[&root]
                .state
                .is_busy()
        };
        bounded(async {
            while busy() {
                tokio::task::yield_now().await;
            }
        })
        .await;
        resubscribed(&mut app).await;
        assert_eq!(count(&app.journal, agent_failed), 1);
        assert!(!app.paused.is_some());
        session.shutdown().await.unwrap();
    }

    /// A failure inside updates the app skipped still holds the queue once it
    /// resubscribes.
    #[tokio::test]
    async fn a_root_failure_missed_by_a_lagged_observer_pauses_on_resubscribe() {
        let (_root, mut app, mut model, session) = played_fixture().await;
        let _rx = capture_work(&mut app);
        let mut updates = session.store().observe().updates;
        app.submit("first".into());
        app.close_observation();
        model.turn().await.fail(rejected());
        bounded(async {
            while !matches!(updates.recv().await.unwrap().event, RuntimeEvent::Record(record)
                if agent_failed(&record.event))
            {}
        })
        .await;
        resubscribed(&mut app).await;
        assert!(app.paused.is_some());
        app.submit("second".into());
        let row = app.queue.iter().find(|row| row.submission.text == "second");
        assert!(matches!(row.unwrap().state, RowState::Waiting));
        session.shutdown().await.unwrap();
    }

    /// Reopening a session whose last root turn failed shows that failure
    /// once, and takes new input.
    #[tokio::test]
    async fn a_reopened_failed_session_takes_new_input() {
        let (_root, mut app, mut model, session) = played_fixture().await;
        let _rx = capture_work(&mut app);
        app.submit("first".into());
        model.turn().await.fail(rejected());
        observe_until(&mut app, |app| app.paused.is_some()).await;
        session.close().await.unwrap();
        let reopened = app.launch.create(Some(session.id())).await.unwrap();
        let prepared = prepared(reopened.clone()).await;
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = app.sibling(Some(prepared), app.launch.clone(), tx);
        app.submit("second".into());
        let user = |event: &SessionEvent| {
            matches!(
                event,
                SessionEvent::MessageCommitted {
                    message: Message::User(_)
                }
            )
        };
        // Only the second is committed after the reopening.
        observe_until(&mut app, |app| count(&app.journal, user) == 1).await;
        assert!(!app.paused.is_some());
        assert!(
            !app.queue
                .iter()
                .any(|row| matches!(row.state, RowState::Waiting))
        );
        assert_eq!(shown(&app, REJECTED).await, 1);
        reopened.shutdown().await.unwrap();
    }

    /// A delivery its idle root cannot take fails the root's turn as a failed
    /// request would: journaled, shown, and holding the queue. Here a retained
    /// child's answer reaches a reopened root that must first adopt its
    /// redefined mode, in a conversation its provider no longer opens.
    #[tokio::test]
    async fn a_delivery_the_idle_root_refuses_fails_its_turn_and_pauses_the_queue() {
        use serde_json::json;
        use skyhook::agent::HarnessBuilder;
        use skyhook::provider::protocol::ContextId;
        use skyhook::tool::policy::{AllowAll, Capability, Mode};
        let (_root, mut app) = draft_fixture().await;
        let name = app.mode.clone();
        let mode = |instructions: &str| {
            let general = Mode {
                capabilities: vec![Capability::Read, Capability::Agents],
                instructions: Some(instructions.parse().unwrap()),
                hint: None,
            };
            let modes = [(name.clone(), general)].into_iter().collect();
            move |builder: HarnessBuilder| builder.modes(modes)
        };
        let policy = std::sync::Arc::new(AllowAll);
        let (harness, mut model) = played_harness(&app.launch, policy.clone(), mode("Look.")).await;
        let session = harness.new_session().await.unwrap();
        let root = session.root_agent().clone();
        attach(&mut app, session.clone()).await;
        let _rx = capture_work(&mut app);
        app.submit("delegate".into());
        let task = json!({"prompt": "work", "bg": true});
        model
            .turn()
            .await
            .end(vec![call("agent-0", "agent", task, 0)]);
        // The root answers while its child's request stays open, until closing
        // interrupts it.
        let (first, second) = (model.turn().await, model.turn().await);
        let (answering, _child) = match first.context == ContextId::from(&root) {
            true => (first, second),
            false => (second, first),
        };
        answering.answer("delegated");
        observe_until(&mut app, |app| count(&app.journal, answered) == 1).await;
        session.shutdown().await.unwrap();
        session.close().await.unwrap();

        let (harness, mut model) = played_harness(&app.launch, policy, mode("Changed.")).await;
        let reopened = harness.resume_session(session.id()).await.unwrap();
        // Activation opens the root's conversation: the last one opened for it.
        let activated = reopened.record_status(root.clone(), "reopened".into());
        activated.await.unwrap();
        model.refuse(&root);
        attach(&mut app, reopened.clone()).await;
        let mut rx = capture_work(&mut app);
        app.command(Command::Retry);
        model.turn().await.answer("worked");
        let work = continued(&mut app, &mut rx).await;
        assert!(matches!(work, Work::Continued { result: Ok(()), .. }));
        app.work(work);
        observe_until(&mut app, |app| {
            let mut records = app.journal.iter();
            let failed = records.any(|record| record.agent == root && agent_failed(&record.event));
            failed && stopped(&app.snapshot, &root)
        })
        .await;
        assert!(app.paused.is_some());
        assert_eq!(shown(&app, "no longer opens this conversation").await, 1);
        reopened.shutdown().await.unwrap();
    }

    /// `/retry` tries held rows on the bottom bar's model, even one the root
    /// already runs, so swapping then retrying does swap.
    #[tokio::test]
    async fn retry_tries_held_rows_on_the_bottom_bar_model() {
        let (_root, mut app, mut model, session) = played_fixture().await;
        let mut rx = capture_work(&mut app);
        let root = app.root_agent().clone();
        app.model = "test/seeing".parse().unwrap();
        app.submit("warmup".into());
        model.turn().await.answer("ok");
        observe_until(&mut app, |app| count(&app.journal, answered) == 1).await;
        bounded(async {
            while !app.queue.is_empty() {
                let work = recv(&mut rx).await;
                app.work(work);
            }
        })
        .await;
        app.projection.reconcile(&app.snapshot);
        let active = app
            .projection
            .agents
            .iter()
            .find(|agent| agent.id == root)
            .unwrap();
        assert_eq!(active.model.to_string(), "test/seeing");
        app.model = "test/first".parse().unwrap();
        app.submit(image_row().await);
        observe_until(&mut app, |app| {
            stopped(&app.snapshot, &root) && app.paused.is_some()
        })
        .await;
        bounded(async {
            while !app.holds_rows() {
                tokio::task::yield_now().await;
            }
        })
        .await;
        app.model = "test/seeing".parse().unwrap();
        app.command(Command::Retry);
        model.turn().await.answer("seen");
        let work = continued(&mut app, &mut rx).await;
        assert!(matches!(work, Work::Continued { result: Ok(()), .. }));
        app.work(work);
        bounded(async {
            while !app.queue.is_empty() {
                let work = recv(&mut rx).await;
                app.work(work);
            }
        })
        .await;
        assert_eq!(app.history, ["warmup", "image"]);
        // What the retry committed under is remembered, not what the row captured.
        let seeing: ModelRef = "test/seeing".parse().unwrap();
        assert_eq!(app.remembered_model, Some(seeing.clone()));
        let workspace = app.launch.workspace.clone();
        bounded(async {
            while crate::state::load(&workspace).0.model != Some(seeing.clone()) {
                tokio::task::yield_now().await;
            }
        })
        .await;
        let mode = app.remembered_mode.clone();
        assert_eq!(crate::state::load(&workspace).0.mode, mode);
        session.shutdown().await.unwrap();
    }

    /// Changing the queue takes back every row the session holds, so the queue
    /// alone orders them: an edit discarded, or left for another edit and then
    /// submitted, a draft queued by an edit, and a delete.
    #[tokio::test]
    async fn an_edit_keeps_the_held_rows_behind_it_in_order() {
        use skyhook::session::{Message, UserPart};
        let (_root, mut app, mut model, session) = played_fixture().await;
        let mut rx = capture_work(&mut app);
        let committed = async || {
            let records = journal(&session).await;
            let users =
                records
                    .into_iter()
                    .filter_map(|record| match Arc::unwrap_or_clone(record).event {
                        SessionEvent::MessageCommitted {
                            message: Message::User(parts),
                        } => parts.into_iter().find_map(|part| match part {
                            UserPart::Text { text } => Some(text),
                            _ => None,
                        }),
                        _ => None,
                    });
            users.collect::<Vec<_>>()
        };
        let id = |app: &App, text: &str| {
            let mut rows = app.queue.iter();
            rows.find(|row| row.submission.text == text).unwrap().id
        };
        let seeing: ModelRef = "test/seeing".parse().unwrap();
        for case in 0..4 {
            let behind: &[&str] = if case == 0 {
                &["behind"]
            } else {
                &["second", "third"]
            };
            app.model = "test/first".parse().unwrap();
            app.submit(image_row().await);
            for text in behind {
                app.submit((*text).into());
            }
            observe_until(&mut app, |app| {
                app.paused.is_some()
                    && (app.queue.iter()).all(
                        |row| matches!(&row.state, RowState::InFlight(row) if row.is_retained()),
                    )
            })
            .await;
            let edit = |app: &mut App, text: &str| {
                let edited = app.begin_edit(id(app, text)).unwrap();
                app.editor.set_submission(edited);
            };
            let expected = match case {
                0 => {
                    edit(&mut app, "image");
                    app.discard_edit();
                    vec!["image", "behind"]
                }
                1 => {
                    edit(&mut app, "image");
                    edit(&mut app, "third");
                    app.submit("third edited".into());
                    vec!["image", "second", "third edited"]
                }
                2 => {
                    app.editor.set("draft".into());
                    edit(&mut app, "second");
                    app.submit("second edited".into());
                    vec!["draft", "image", "second edited", "third"]
                }
                _ => {
                    assert!(app.remove_queued(id(&app, "second")).is_some());
                    vec!["image", "third"]
                }
            };
            app.model = seeing.clone();
            app.command(Command::Retry);
            model.turn().await.answer("seen");
            bounded(async {
                while !app.queue.is_empty() {
                    let work = recv(&mut rx).await;
                    app.work(work);
                }
            })
            .await;
            let committed = committed().await;
            assert_eq!(committed[committed.len() - expected.len()..], expected[..]);
        }
        session.shutdown().await.unwrap();
    }

    /// A row the session refuses before any turn takes it, as a reopened
    /// session whose root cannot activate does, is reported with why.
    #[tokio::test]
    async fn a_row_refused_before_a_turn_is_reported() {
        use skyhook::tool::policy::AllowAll;
        let (_root, mut app, mut model, session) = played_fixture().await;
        let root = session.root_agent().clone();
        // The reopened session's status log reports on this channel.
        let mut status = capture_work(&mut app);
        app.submit("first".into());
        model.turn().await.answer("ok");
        observe_until(&mut app, |app| count(&app.journal, answered) == 1).await;
        session.shutdown().await.unwrap();
        session.close().await.unwrap();
        let policy = std::sync::Arc::new(AllowAll);
        let (harness, model) = played_harness(&app.launch, policy, |builder| builder).await;
        model.refuse(&root);
        let reopened = harness.resume_session(session.id()).await.unwrap();
        attach(&mut app, reopened.clone()).await;
        let mut rx = capture_work(&mut app);
        app.submit("second".into());
        bounded(async {
            loop {
                let work = recv(&mut rx).await;
                let done = matches!(work, Work::QueueCommitted { .. });
                app.work(work);
                if done {
                    break;
                }
            }
        })
        .await;
        let notices = notices(&app, &mut status).await;
        assert!(app.paused.is_some() && matches!(app.queue[0].state, RowState::Waiting));
        assert!(
            notices
                .iter()
                .any(|notice| notice.contains("no longer opens")),
            "{notices:?}"
        );
        reopened.shutdown().await.unwrap();
    }

    /// An app showing a session reopened on a configuration that renamed its
    /// model, `test/first`, to `test/second`, and the work it sends from then.
    async fn gone_fixture() -> (
        tempfile::TempDir,
        App,
        SessionHandle,
        mpsc::UnboundedReceiver<Work>,
    ) {
        use skyhook::provider::dialect::ProviderSettings;
        let (root, mut app) = draft_fixture().await;
        let session = app.launch.create(None).await.unwrap();
        session.shutdown().await.unwrap();
        session.close().await.unwrap();
        let mut config = app.launch.model.config().config().clone();
        let ProviderSettings::Compatible(provider) = &mut config.providers["test"].settings else {
            panic!("compatible fixture");
        };
        let profile = provider.models.shift_remove("first").unwrap();
        provider.models.insert("second".parse().unwrap(), profile);
        app.launch.model = config.into_runtime().unwrap().default_model();
        let reopened = app.launch.create(Some(session.id())).await.unwrap();
        let work = capture_work(&mut app);
        attach(&mut app, reopened.clone()).await;
        (root, app, reopened, work)
    }

    fn model_changes(records: &[Arc<skyhook::session::EventRecord>]) -> Vec<String> {
        let changed = records.iter().filter_map(|record| match &record.event {
            SessionEvent::ModelChanged { profile } => Some(profile.name.to_string()),
            _ => None,
        });
        changed.collect()
    }

    /// A session reopened after its model left the configuration keeps a message
    /// queued and opens the model picker: the model chosen there runs the session,
    /// which then takes the message.
    #[tokio::test]
    async fn a_session_whose_model_is_gone_holds_the_queue_until_a_model_is_picked() {
        let (_root, mut app, reopened, mut status) = gone_fixture().await;
        app.submit("held".into());
        let notices = notices(&app, &mut status).await;
        assert!(app.paused.is_some() && matches!(app.queue[0].state, RowState::Waiting));
        let pointed = "model `test/first` is not configured; choose another with /model";
        assert!(
            notices.iter().any(|notice| notice.contains(pointed)),
            "{notices:?}"
        );
        let picker = app.menu.as_ref().map(|menu| &menu.kind);
        let Some(MenuKind::Models(models)) = picker else {
            panic!("the model picker opens");
        };
        let second = models
            .iter()
            .position(|item| item.value.to_string() == "test/second");
        app.menu.as_mut().unwrap().select(second.unwrap());
        let mut rx = capture_work(&mut app);
        app.choose();
        bounded(async {
            loop {
                let work = recv(&mut rx).await;
                let done = matches!(work, Work::QueueCommitted { .. });
                app.work(work);
                if done {
                    break;
                }
            }
        })
        .await;
        assert!(app.queue.is_empty() && app.paused.is_none());
        assert_eq!(model_changes(&journal(&reopened).await), ["test/second"]);
        reopened.shutdown().await.unwrap();
    }

    /// Switching away from an idle session as its picked model activates does
    /// not close it first.
    #[tokio::test]
    async fn a_picked_model_activating_keeps_the_session_open() {
        let (_root, mut app, reopened, mut rx) = gone_fixture().await;
        assert!(app.settled());
        app.choose_model("test/second".parse().unwrap());
        assert!(!app.settled(), "an activation in flight keeps the session");
        let activated = bounded(async {
            loop {
                if let Work::Activated(activated) = recv(&mut rx).await {
                    break activated;
                }
            }
        });
        assert!(activated.await.is_ok());
        assert!(app.settled());
        assert_eq!(model_changes(&journal(&reopened).await), ["test/second"]);
        reopened.shutdown().await.unwrap();
    }
}
