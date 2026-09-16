use super::*;
use skyhook::agent::{
    PreparedQueuedPrompt, QueuedPromptCancellation, QueuedPromptCommit, QueuedPromptError,
    QueuedPromptIdentity, QueuedPromptRecovery, RecoveredQueuedPrompt, RecoveredQueuedPromptState,
};
use skyhook::provider::protocol::UserContent;

mod durable;
pub(super) use durable::QueueScan;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueuedInputId(u64);

/// One composer submission waiting behind the current operation. `submission`
/// and `model` are display/edit hints; once saved, the immutable content lives
/// in the session journal under the row's attempt.
pub struct QueuedInput {
    pub(super) id: QueuedInputId,
    pub(super) generation: u64,
    pub(super) state: RowState,
    pub(super) submission: Submission,
    pub(super) model: String,
}

/// Where a row's single journal attempt stands. The UI owns cancellation and
/// recovery evidence, never a second enqueue permit.
#[derive(Debug, Default)]
pub(super) enum RowState {
    /// Not saved to the journal yet.
    #[default]
    Unsaved,
    /// Durable preparation is running; nothing can have been dispatched.
    Preparing(QueuedPromptCancellation),
    /// Saved, holding the exclusive dispatch permit.
    Ready(PreparedQueuedPrompt),
    /// Dispatched; its commit receipt is pending.
    InFlight(QueuedPromptCancellation),
    /// Dispatch was cancelled; the runtime's rejection receipt makes it `Saved`.
    Withdrawn(QueuedPromptCancellation),
    /// Saved without a permit; reclaiming the attempt regains one.
    Saved(QueuedPromptCancellation),
    /// A reclaim for this saved attempt is running.
    Reclaiming(QueuedPromptCancellation),
    /// The journal lists the attempt but cannot resolve it yet.
    Unresolved(QueuedPromptIdentity),
    /// Outcome unknown; evidence is kept until the journal resolves it.
    Recovery {
        evidence: QueuedPromptRecovery,
        cancellation: QueuedPromptCancellation,
    },
}

impl RowState {
    pub(super) fn identity(&self) -> Option<QueuedPromptIdentity> {
        match self {
            Self::Unsaved => None,
            Self::Preparing(attempt)
            | Self::InFlight(attempt)
            | Self::Withdrawn(attempt)
            | Self::Saved(attempt)
            | Self::Reclaiming(attempt) => Some(attempt.identity()),
            Self::Ready(permit) => Some(permit.identity()),
            Self::Unresolved(identity) => Some(*identity),
            Self::Recovery { evidence, .. } => Some(evidence.submission),
        }
    }

    /// Dispatched or unresolved: only a receipt or the journal settles it.
    pub(super) fn unsettled(&self) -> bool {
        matches!(
            self,
            Self::InFlight(_) | Self::Unresolved(_) | Self::Recovery { .. }
        )
    }

    /// Saved without live authority: a reclaim or journal scan releases it.
    fn lacks_authority(&self) -> bool {
        matches!(
            self,
            Self::Saved(_) | Self::Reclaiming(_) | Self::Unresolved(_) | Self::Recovery { .. }
        )
    }
}

impl QueuedInput {
    /// Recovery evidence is read-only: only an authoritative reconciliation may
    /// release this row for retry or record it as committed.
    #[cfg(test)]
    pub(super) fn recovery(&self) -> Option<&QueuedPromptRecovery> {
        match &self.state {
            RowState::Recovery { evidence, .. } => Some(evidence),
            _ => None,
        }
    }

    fn retain_recovery(&mut self, recovery: QueuedPromptRecovery) {
        match &mut self.state {
            RowState::Preparing(attempt)
            | RowState::InFlight(attempt)
            | RowState::Withdrawn(attempt) => {
                let cancellation = attempt.clone();
                self.state = RowState::Recovery {
                    evidence: recovery,
                    cancellation,
                };
            }
            RowState::Recovery { evidence, .. } => {
                // A late/less complete receipt must not erase accepted identities.
                for append in recovery.appends {
                    if !evidence.appends.contains(&append) {
                        evidence.appends.push(append);
                    }
                }
                if evidence.message.is_none() {
                    evidence.message = recovery.message;
                }
            }
            _ => {}
        }
    }

    /// Refresh, but never resolve, uncertainty after the runtime has drained.
    pub(super) fn refresh_recovery(&mut self) {
        let RowState::Recovery { cancellation, .. } = &self.state else {
            return;
        };
        if let Some(recovery) = cancellation.recovery() {
            self.retain_recovery(recovery);
        }
    }
}

pub(super) struct QueueDelivery {
    pub(super) id: QueuedInputId,
    pub(super) generation: u64,
    pub(super) prepared: PreparedQueuedPrompt,
}

/// Register each UI queue snapshot atomically as one request-boundary batch.
pub(super) async fn queue_dispatcher(
    session: SessionHandle,
    mut rx: mpsc::UnboundedReceiver<Vec<QueueDelivery>>,
    tx: mpsc::UnboundedSender<Work>,
) {
    use futures_util::{StreamExt, stream::FuturesOrdered};
    let mut pending = FuturesOrdered::new();
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
                    let session = session.clone();
                    pending.push_back(async move {
                        let ids: Vec<_> = deliveries.iter()
                            .map(|input| (input.id, input.generation, input.prepared.identity()))
                            .collect();
                        let inputs = deliveries.into_iter().map(|input| input.prepared).collect();
                        // Core promises one ordered receipt per permit.
                        let results = session.enqueue_prepared_queued_prompts(inputs).await;
                        let revision = session.observe().await.snapshot.revision;
                        ids.into_iter().zip(results).map(|((id, generation, submission), result)| {
                            Work::QueueCommitted {
                                session: session.id(), id, generation, submission, revision,
                                result,
                            }
                        }).collect::<Vec<_>>()
                    });
                }
                None => open = false,
            },
            Some(work) = pending.next(), if !pending.is_empty() => {
                for item in work {
                    let _ = tx.send(item);
                }
            }
        }
    }
}

// A draft identity only scopes UI state/notices; it never names a session directory.

impl App {
    pub fn submit(&mut self, submission: Submission) {
        if submission.is_empty() {
            return;
        }
        self.reject_pending_questions();
        self.advance_draft();
        let queued = self.queued_input(submission);
        if self.busy() || self.paused || !self.queue.is_empty() {
            self.prepare_queue_input(queued);
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
        if self.queue_requires_recovery() || queued.state.unsettled() {
            self.queue.push_back(queued);
            self.paused = true;
            self.refresh_queue_menu();
            self.notice(
                "Queued submission requires reconciliation before new input can be dispatched",
            );
            return;
        }
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
            if let RowState::InFlight(attempt) = &input.state
                && attempt.cancel()
            {
                input.state = RowState::Withdrawn(attempt.clone());
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
    pub(super) fn queue_requires_recovery(&self) -> bool {
        self.queue.iter().any(|input| {
            matches!(
                input.state,
                RowState::Unresolved(_) | RowState::Recovery { .. }
            )
        })
    }

    /// A dead dispatcher cannot provide a receipt. Rows it never claimed are
    /// definitely safe to retry; claimed rows keep their evidence.
    fn recover_closed_queue_dispatcher(&mut self) {
        if !self
            .queue_sender
            .as_ref()
            .is_some_and(|sender| sender.is_closed())
        {
            return;
        }
        self.queue_sender = None;
        for input in &mut self.queue {
            let (RowState::InFlight(attempt) | RowState::Withdrawn(attempt)) = &input.state else {
                continue;
            };
            let attempt = attempt.clone();
            match attempt.lost_receipt() {
                QueuedPromptError::Rejected(_) => input.state = RowState::Saved(attempt),
                QueuedPromptError::Indeterminate(recovery) => input.retain_recovery(*recovery),
            }
        }
        self.paused = true;
        if self.queue_requires_recovery() {
            self.notice(
                "Queued message outcome is unknown; reconciliation is required before retry",
            );
        } else {
            self.notice("Could not deliver queued messages; resume to retry");
        }
    }

    pub(super) fn deliver_queue(&mut self) {
        self.recover_closed_queue_dispatcher();
        // Lag recovery replaces the snapshot without replaying each event.
        for record in self.snapshot.records.values() {
            if observe_initial_input(&mut self.initial_input, record) {
                break;
            }
        }
        if self.start.is_creating()
            || self.stopping
            || self.switching.is_some()
            || self.queue.is_empty()
        {
            return;
        }
        let Some(session) = self.session().cloned() else {
            if self.paused {
                return;
            }
            // Retain the head across creation, then register the entire queue.
            let input = self.queue.pop_front().unwrap();
            self.refresh_queue_menu();
            match self.select_queued_model(input) {
                Some(input) => self.begin_session(PendingStart::QueuedInput(Box::new(input))),
                None => self.refresh_queue_menu(),
            }
            return;
        };
        // Rows are saved even while paused, so a paused queue survives reopen.
        self.prepare_unbound_rows(&session);
        if self.queue_requires_recovery() || self.queue_scan == QueueScan::Failed {
            // A pending journal scan may still release these rows.
            if !matches!(self.queue_scan, QueueScan::Running { .. }) {
                self.paused = true;
            }
            return;
        }
        if self.paused || self.initial_input.is_some() {
            return;
        }
        let sender = self.queue_sender.get_or_insert_with(|| {
            let (tx, rx) = mpsc::unbounded_channel();
            tokio::spawn(queue_dispatcher(session, rx, self.tx.clone()));
            tx
        });
        // Dispatch the ready prefix only: a row still preparing or awaiting
        // recovery keeps everything behind it in FIFO order.
        let mut deliveries = Vec::new();
        for input in &mut self.queue {
            match std::mem::take(&mut input.state) {
                RowState::Ready(prepared) => {
                    input.generation = input.generation.wrapping_add(1);
                    input.state = RowState::InFlight(prepared.cancellation_handle());
                    deliveries.push(QueueDelivery {
                        id: input.id,
                        generation: input.generation,
                        prepared,
                    });
                }
                state @ RowState::InFlight(_) => input.state = state,
                state => {
                    input.state = state;
                    break;
                }
            }
        }
        if deliveries.is_empty() {
            return;
        }
        if let Err(mpsc::error::SendError(deliveries)) = sender.send(deliveries) {
            // Send failure returns the permits before dispatch. Existing attempts
            // on that same dead dispatcher still need their own claim check.
            for delivery in deliveries {
                if let Some(input) = self.queue.iter_mut().find(|input| input.id == delivery.id) {
                    input.state = RowState::Ready(delivery.prepared);
                }
            }
            self.recover_closed_queue_dispatcher();
        }
    }
    pub(super) fn queue_committed(
        &mut self,
        id: QueuedInputId,
        generation: u64,
        submission: QueuedPromptIdentity,
        revision: u64,
        result: Result<QueuedPromptCommit, QueuedPromptError>,
    ) {
        // Editing, cancellation and retry invalidate old receipts; a removed
        // row's attempt was already abandoned.
        let Some(index) = self.queue.iter().position(|input| {
            input.id == id
                && input.generation == generation
                && input.state.identity() == Some(submission)
        }) else {
            return;
        };
        match result {
            Ok(commit) => {
                let input = self.queue.remove(index).unwrap();
                self.queue_activity_revision = self.queue_activity_revision.max(revision);
                self.commit_row(&input);
                if let Some(session) = self.session().cloned() {
                    self.acknowledge_attempt(&session, commit.submission);
                }
            }
            Err(QueuedPromptError::Rejected(error)) => {
                let input = &mut self.queue[index];
                match std::mem::take(&mut input.state) {
                    // A withdrawn row expected exactly this rejection.
                    RowState::Withdrawn(attempt) => input.state = RowState::Saved(attempt),
                    RowState::InFlight(attempt) => {
                        input.state = RowState::Saved(attempt);
                        self.pause_queue();
                        if self.queue_requires_recovery() {
                            self.notice(format!(
                                "Queued message was rejected: {error}. Other submissions require reconciliation before retry."
                            ));
                        } else {
                            self.notice(format!(
                                "Queued message was not submitted: {error}. Resume to retry."
                            ));
                        }
                    }
                    // A late rejection cannot discharge already retained uncertainty.
                    state => input.state = state,
                }
            }
            Err(QueuedPromptError::Indeterminate(recovery)) => self.retain_uncertain(
                index,
                *recovery,
                "Queued message outcome is unknown; reconciliation is required before retry",
            ),
        }
        self.refresh_queue_menu();
    }
    /// Keep a row's uncertain outcome as evidence, pause, and say why.
    fn retain_uncertain(&mut self, index: usize, recovery: QueuedPromptRecovery, notice: &str) {
        self.queue[index].retain_recovery(recovery);
        self.pause_queue();
        self.notice(notice);
    }
    pub(super) fn queued_input(&mut self, submission: Submission) -> QueuedInput {
        self.next_queued_id.0 += 1;
        QueuedInput {
            id: self.next_queued_id,
            generation: 0,
            state: RowState::Unsaved,
            submission,
            model: self.model.clone(),
        }
    }
    pub(super) fn queue_items(&self) -> Vec<Item<QueuedInputId>> {
        self.queue
            .iter()
            .map(|queued| {
                Item::new(
                    queued.id,
                    crate::tui::format::brief(&queued.submission.text, 100),
                    self.launch
                        .model
                        .config()
                        .config()
                        .models
                        .get(&queued.model)
                        .map_or(queued.model.as_str(), |profile| profile.model.as_str()),
                )
            })
            .collect()
    }
    /// Take a row back into the composer. Its saved attempt, if any, is
    /// abandoned durably now; the edit becomes a fresh attempt.
    pub(super) fn remove_queued(&mut self, id: QueuedInputId) -> Option<QueuedInput> {
        let index = self.queue.iter().position(|queued| queued.id == id)?;
        let attempt = match &self.queue[index].state {
            RowState::Unsaved => None,
            RowState::Ready(permit) => Some(permit.cancellation_handle()),
            RowState::Withdrawn(attempt)
            | RowState::Saved(attempt)
            | RowState::Reclaiming(attempt) => Some(attempt.clone()),
            RowState::Preparing(attempt) | RowState::InFlight(attempt) if attempt.cancel() => {
                Some(attempt.clone())
            }
            _ => {
                self.notice(
                    "This message cannot be edited or cancelled: its submission outcome is unresolved",
                );
                return None;
            }
        };
        let mut input = self.queue.remove(index).unwrap();
        input.state = RowState::Unsaved;
        if let Some(attempt) = attempt {
            self.abandon(attempt);
        }
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
    use skyhook::agent::{HarnessError, QueuedPromptToken};
    use skyhook::identity::EventId;

    type Records = [skyhook::session::EventRecord];
    type Deliveries = mpsc::UnboundedReceiver<Vec<QueueDelivery>>;

    fn commit(app: &App, submission: QueuedPromptIdentity) -> QueuedPromptCommit {
        QueuedPromptCommit {
            submission,
            append: skyhook::session::AppendIdentity {
                event: EventId::generate().unwrap(),
                queue_attempt: Some(submission.attempt()),
                session: app.session_id().unwrap(),
                sequence: 42,
            },
        }
    }

    fn rejected() -> Result<QueuedPromptCommit, QueuedPromptError> {
        Err(QueuedPromptError::Rejected(HarnessError::AgentStopped))
    }

    /// A fixture whose asynchronous queue work and dispatch the test applies
    /// itself; a failing one rejects every provider turn deterministically.
    async fn queue_fixture(
        failing: bool,
    ) -> (
        tempfile::TempDir,
        App,
        mpsc::UnboundedReceiver<Work>,
        Deliveries,
    ) {
        let (root, mut app) = if failing {
            permanent_failure_fixture().await
        } else {
            fixture().await
        };
        let rx = capture_work(&mut app);
        let deliveries = hold_dispatcher(&mut app);
        (root, app, rx, deliveries)
    }

    /// Holding the receiver makes dispatch deterministic without invoking a provider.
    fn hold_dispatcher(app: &mut App) -> Deliveries {
        let (tx, rx) = mpsc::unbounded_channel();
        app.queue_sender = Some(tx);
        app.operation = true;
        rx
    }

    fn next_delivery(deliveries: &mut Deliveries) -> QueueDelivery {
        deliveries.try_recv().unwrap().pop().unwrap()
    }

    /// Apply work until the condition holds. Nothing in these tests depends on
    /// a provider turn; preparation and journal scans are the only producers.
    async fn settle(
        app: &mut App,
        rx: &mut mpsc::UnboundedReceiver<Work>,
        done: impl Fn(&App) -> bool,
    ) {
        while !done(app) {
            let work = recv(rx).await;
            app.work(work);
        }
    }

    /// Poll the journal file until it satisfies the condition, applying any
    /// work that arrives meanwhile. Reading the file directly never reserves
    /// a permit, so it cannot race the app's own recovery or abandonment.
    async fn journal_settles(
        app: &mut App,
        rx: &mut mpsc::UnboundedReceiver<Work>,
        done: impl Fn(&Records) -> bool,
    ) {
        let (root, id) = (app.launch.sessions.clone(), app.session_id().unwrap());
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                while let Ok(work) = rx.try_recv() {
                    app.work(work);
                }
                if done(&SessionStore::read_records(&root, id).await.unwrap()) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("journal settled");
    }

    fn acknowledged(records: &Records, submission: QueuedPromptIdentity) -> bool {
        records.iter().any(|record| {
            matches!(record.event, SessionEvent::QueueAcknowledged { attempt } if attempt == submission.attempt())
        })
    }

    fn in_flight(app: &App, index: usize) -> bool {
        let state = app.queue.get(index).map(|input| &input.state);
        matches!(state, Some(RowState::InFlight(_)))
    }

    fn scanning(app: &App) -> bool {
        matches!(app.queue_scan, QueueScan::Running { .. })
    }

    fn text_of(prepared: &PreparedQueuedPrompt) -> &str {
        match &prepared.content()[0] {
            UserContent::Text { text } => text,
            other => panic!("unexpected content {other:?}"),
        }
    }

    fn ack(app: &mut App, result: Result<QueuedPromptCommit, QueuedPromptError>) {
        let input = &app.queue[0];
        app.work(Work::QueueCommitted {
            session: app.session_id().unwrap(),
            id: input.id,
            generation: input.generation,
            submission: input.state.identity().unwrap(),
            revision: 42,
            result,
        });
    }

    #[tokio::test]
    async fn queue_delivery_is_immediate_while_busy_and_cancellation_rejects_stale_acks() {
        let (_root, mut app, mut rx, mut deliveries) = queue_fixture(false).await;
        let attachments = vec![png_attachment("pending.png")];
        app.submit(Submission {
            text: "pending".into(),
            attachments,
        });
        assert_eq!(app.queue.len(), 1);
        assert!(app.history.is_empty());
        assert!(matches!(app.queue[0].state, RowState::Preparing(_)));
        assert!(app.busy());
        settle(&mut app, &mut rx, |app| in_flight(app, 0)).await;
        let first = next_delivery(&mut deliveries);
        let (id, generation) = (app.queue[0].id, app.queue[0].generation);
        let RowState::InFlight(token) = &app.queue[0].state else {
            panic!("sent while busy");
        };
        let token = token.clone();
        let submission = token.identity();
        assert_eq!(first.prepared.identity(), submission);
        assert_eq!(app.queue[0].state.identity(), Some(submission));
        app.paused = true;
        app.cancel_queue_delivery();
        assert!(token.cancel());
        assert!(matches!(app.queue[0].state, RowState::Withdrawn(_)));
        app.queue_committed(id, generation, submission, 0, rejected());
        assert_eq!(app.queue.len(), 1);
        assert!(matches!(app.queue[0].state, RowState::Saved(_)));
        // Resuming reclaims the same durable attempt instead of saving a copy.
        drop(first);
        app.paused = false;
        app.deliver_queue();
        assert!(matches!(app.queue[0].state, RowState::Reclaiming(_)));
        assert!(!scanning(&app));
        let resent = |app: &App| app.queue[0].generation > generation && in_flight(app, 0);
        settle(&mut app, &mut rx, resent).await;
        let second = next_delivery(&mut deliveries);
        assert_eq!((second.id, second.prepared.identity()), (id, submission));
        assert!(second.generation > 1);
        assert_eq!(text_of(&second.prepared), "pending");
        assert!(matches!(
            second.prepared.content()[1],
            UserContent::Attachment {
                attachment: skyhook::media::AttachmentRef::Image(ref image),
            } if image.file.as_deref() == Some("pending.png")
        ));
        // Exactly one saved attempt exists for the row, and it is reserved.
        let rows = app
            .session()
            .unwrap()
            .recover_queued_prompts()
            .await
            .unwrap();
        assert!(
            matches!(&rows[..], [row] if matches!(row.state, RecoveredQueuedPromptState::Unresolved))
        );
        app.queue_committed(id, generation, submission, 0, Ok(commit(&app, submission)));
        assert_eq!(app.queue.len(), 1);
        assert!(app.history.is_empty());
        let edited = app.remove_queued(id).unwrap();
        assert_eq!(
            edited.submission.attachments,
            [png_attachment("pending.png")]
        );
        assert!(app.queue.is_empty());
        app.session().unwrap().shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn queue_waits_for_initial_input_and_commit_does_not_finish_original_operation() {
        let (_root, mut app, mut rx, _deliveries) = queue_fixture(false).await;
        let root = app.root_agent().clone();
        app.initial_input = Some((root.clone(), 0));
        app.submit("followup".into());
        app.tick();
        assert!(!in_flight(&app, 0));
        // The initial input commit releases follow-ups before its first request,
        // including when that request first needs compaction.
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
            event: RuntimeEvent::Record(Box::new(committed.clone())),
        });
        assert!(app.initial_input.is_none());
        assert!(!observe_initial_input(&mut app.initial_input, &committed));
        settle(&mut app, &mut rx, |app| in_flight(app, 0)).await;
        let input = &app.queue[0];
        let RowState::InFlight(attempt) = &input.state else {
            panic!("dispatched")
        };
        // This unit test synthesizes the ack rather than invoking a provider.
        assert!(attempt.cancel());
        let submission = attempt.identity();
        app.work(Work::QueueCommitted {
            session: app.session_id().unwrap(),
            id: input.id,
            generation: input.generation,
            submission,
            revision: app.snapshot.revision + 1,
            result: Ok(commit(&app, submission)),
        });
        assert!(app.queue.is_empty());
        assert_eq!(app.history, ["followup"]);
        assert!(
            app.operation,
            "commit is not a turn-completion acknowledgement"
        );
        app.operation = false;
        assert!(
            app.busy(),
            "wait for observation activity after an idle enqueue"
        );
        app.snapshot.revision = app.queue_activity_revision;
        assert!(!app.busy());
        app.session().unwrap().shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn queue_failure_retains_row_and_pauses_remaining_deliveries() {
        let (_root, mut app, mut rx, _deliveries) = queue_fixture(false).await;
        app.submit("first".into());
        app.submit("second".into());
        settle(&mut app, &mut rx, |app| {
            in_flight(app, 0) && in_flight(app, 1)
        })
        .await;
        let input = &app.queue[0];
        let RowState::InFlight(attempt) = &input.state else {
            panic!("dispatched")
        };
        assert!(attempt.cancel());
        let submission = attempt.identity();
        app.queue_committed(input.id, input.generation, submission, 0, rejected());
        assert!(app.paused);
        // The rejected row and the withdrawn one behind it both stay saved.
        assert_eq!(app.queue.len(), 2);
        assert!(matches!(app.queue[0].state, RowState::Saved(_)));
        assert!(matches!(app.queue[1].state, RowState::Withdrawn(_)));
        assert!(app.history.is_empty());

        app.session().unwrap().shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn closed_dispatcher_before_claim_is_definite_rejection() {
        let (_root, mut app, mut rx, deliveries) = queue_fixture(false).await;
        app.submit("unsent".into());
        settle(&mut app, &mut rx, |app| in_flight(app, 0)).await;
        let submission = app.queue[0].state.identity().unwrap();
        // Dropping the channel drops the undispatched permit with it.
        drop(deliveries);
        app.deliver_queue();
        assert!(app.paused);
        assert!(!app.queue_requires_recovery());
        assert!(matches!(app.queue[0].state, RowState::Saved(_)));
        assert!(app.history.is_empty());
        let mut deliveries = hold_dispatcher(&mut app);
        app.paused = false;
        app.deliver_queue();
        settle(&mut app, &mut rx, |app| in_flight(app, 0)).await;
        let delivery = next_delivery(&mut deliveries);
        assert_eq!(text_of(&delivery.prepared), "unsent");
        assert_eq!(delivery.prepared.identity(), submission);
        app.session().unwrap().shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn indeterminate_attempt_retains_evidence_and_blocks_edit_cancel_and_new_dispatch_until_reconciled()
     {
        let (_root, mut app, mut rx, mut deliveries) = queue_fixture(true).await;
        app.submit("uncertain".into());
        settle(&mut app, &mut rx, |app| in_flight(app, 0)).await;
        let delivery = next_delivery(&mut deliveries);
        let submission = delivery.prepared.identity();
        let append = commit(&app, submission).append;
        // Obtain the core-owned recovery state through the public observer; the
        // test varies only the public receipt evidence, never its authority.
        let observer = delivery.prepared.cancellation_handle();
        let session = app.session().unwrap();
        let enqueued = session
            .enqueue_prepared_queued_prompts(vec![delivery.prepared])
            .await;
        assert!(enqueued[0].is_ok(), "{enqueued:?}");
        let mut recovery = observer
            .recovery()
            .expect("the runtime claimed the submission");
        recovery.appends = vec![append];
        recovery.message = Some(append);
        recovery.reason = "sync acknowledgement lost".into();
        let indeterminate = |recovery| Err(QueuedPromptError::Indeterminate(Box::new(recovery)));
        ack(&mut app, indeterminate(recovery.clone()));
        assert!(app.queue_requires_recovery());
        let (id, generation) = (app.queue[0].id, app.queue[0].generation);
        app.paused = false; // Even a caller bypassing Resume's guard cannot enqueue.
        app.deliver_queue();
        assert!(app.paused);
        assert!(app.remove_queued(id).is_none());
        app.cancel_queue_delivery();
        app.submit("later".into());
        assert!(deliveries.try_recv().is_err());
        assert_eq!(app.queue.len(), 2);
        assert_eq!(app.queue[0].generation, generation);
        assert_eq!(app.queue[0].submission.text, "uncertain");
        let evidence = app.queue[0].recovery().unwrap();
        assert_eq!(
            (evidence.submission, &evidence.appends[..]),
            (submission, &[append][..])
        );
        assert_eq!(evidence.message, Some(append));
        assert_eq!(evidence.reason, "sync acknowledgement lost");
        recovery.appends.clear();
        recovery.message = None;
        recovery.reason = "less complete late receipt".into();
        ack(&mut app, indeterminate(recovery));
        let evidence = app.queue[0].recovery().unwrap();
        assert_eq!(
            (&evidence.appends[..], evidence.message),
            (&[append][..], Some(append))
        );
        assert!(app.history.is_empty());
        // A contradictory late rejection is not reconciliation authority.
        ack(&mut app, rejected());
        assert!(app.queue_requires_recovery());
        // Resume reconciles through the journal, which holds the real commit.
        app.command(Command::Resume);
        assert!(scanning(&app));
        settle(&mut app, &mut rx, |app| !scanning(app)).await;
        assert!(!app.queue_requires_recovery());
        assert_eq!(app.history, ["uncertain"]);
        assert_eq!(app.queue.len(), 1);
        assert_eq!(app.queue[0].submission.text, "later");
        app.session().unwrap().shutdown().await.unwrap();
    }

    /// An unresolved row no longer traps the user: it stays saved in the old
    /// session's journal and the reopened session offers it again.
    #[tokio::test]
    async fn switching_away_keeps_unresolved_rows_saved_for_reopen() {
        let (_root, mut app, mut rx, mut deliveries) = queue_fixture(false).await;
        app.submit("unresolved".into());
        settle(&mut app, &mut rx, |app| in_flight(app, 0)).await;
        let delivery = next_delivery(&mut deliveries);
        let submission = delivery.prepared.identity();
        let recovery = QueuedPromptRecovery {
            submission,
            appends: vec![],
            message: None,
            reason: "receipt lost".into(),
        };
        ack(
            &mut app,
            Err(QueuedPromptError::Indeterminate(Box::new(recovery))),
        );
        assert!(app.queue_requires_recovery());
        drop(delivery);
        let id = app.session_id().unwrap();
        app.switch(None);
        assert!(app.switching.is_some(), "the switch proceeds");
        let work = next_lifecycle(&mut rx).await;
        assert!(matches!(work, Work::SessionReady { result: Ok(None) }));
        app.set_session(None);
        assert!(app.queue.is_empty());
        let resumed = reopen(&app, id).await;
        app.set_session(Some(PreparedObservation::subscribe(resumed).await));
        assert_eq!(app.queue.len(), 1);
        assert_eq!(app.queue[0].submission.text, "unresolved");
        let state = &app.queue[0].state;
        assert!(matches!(state, RowState::Ready(permit) if permit.identity() == submission));
        app.session().unwrap().shutdown().await.unwrap();
    }

    /// Lag recovery must not treat a row removed while its abandon is still in
    /// flight as a journal row the queue forgot.
    #[tokio::test]
    async fn lag_resubscribe_does_not_restore_a_row_whose_abandon_is_in_flight() {
        let (_root, mut app, mut rx, mut deliveries) = queue_fixture(false).await;
        app.submit("removed".into());
        settle(&mut app, &mut rx, |app| in_flight(app, 0)).await;
        // The dispatcher still holds the permit, so the journal cannot resolve it.
        let held = next_delivery(&mut deliveries);
        // Removal cancels the dispatch; the abandon it starts has not landed yet.
        let removed = app.queue.pop_front().unwrap();
        let RowState::InFlight(attempt) = removed.state else {
            panic!("dispatched")
        };
        assert!(attempt.cancel());
        app.resubscribe().await;
        assert!(app.queue.is_empty(), "the removed row was restored");
        let session = app.session().cloned().unwrap();
        session.abandon_queued_prompt(&attempt).await.unwrap();
        drop(held);
        let retired = |records: &Records| acknowledged(records, attempt.identity());
        journal_settles(&mut app, &mut rx, retired).await;
        assert!(app.queue.is_empty());
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn lost_claimed_dispatcher_receipt_retains_accepted_user_identity_until_reconciled() {
        let (_root, mut app, mut rx, mut deliveries) = queue_fixture(true).await;
        app.submit("claimed with lost receipt".into());
        settle(&mut app, &mut rx, |app| in_flight(app, 0)).await;
        let delivery = next_delivery(&mut deliveries);
        let cancellation = delivery.prepared.cancellation_handle();
        let session = app.session().unwrap();
        let committed = session
            .enqueue_prepared_queued_prompts(vec![delivery.prepared])
            .await;
        let committed = committed.into_iter().next().unwrap().unwrap();
        assert!(cancellation.is_claimed());
        // Simulate the dispatcher exiting after core claim/acceptance but before
        // publishing Work. The UI cannot use the receipt held by this test.
        drop(deliveries);
        app.deliver_queue();
        assert!(app.queue_requires_recovery());
        app.queue[0].refresh_recovery();
        let recovery = app.queue[0].recovery().unwrap();
        assert_eq!(recovery.submission, committed.submission);
        assert!(recovery.appends.contains(&committed.append));
        assert_eq!(recovery.message, Some(committed.append));
        assert!(app.history.is_empty());
        // The journal, not the lost receipt, proves the commit on Resume.
        app.command(Command::Resume);
        settle(&mut app, &mut rx, |app| !scanning(app)).await;
        assert_eq!(app.history, ["claimed with lost receipt"]);
        assert!(app.queue.is_empty());
        let retired = |records: &Records| acknowledged(records, committed.submission);
        journal_settles(&mut app, &mut rx, retired).await;
        app.session().unwrap().shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn reopened_session_restores_saved_drafts_and_acknowledges_committed_rows() {
        let (_root, mut app, mut rx, mut deliveries) = queue_fixture(false).await;
        let attachments = vec![png_attachment("draft.png")];
        let text = "saved draft".into();
        app.submit(Submission { text, attachments });
        settle(&mut app, &mut rx, |app| in_flight(app, 0)).await;
        let held = next_delivery(&mut deliveries);
        let draft = held.prepared.identity();
        let session = app.session().cloned().unwrap();
        // A row committed by core whose client acknowledgement never happened.
        let options = skyhook::agent::PromptOptions {
            model: Some("first".into()),
        };
        let prompt = skyhook::agent::QueuedPrompt {
            text: "committed row".into(),
            attachments: vec![],
            options,
            token: QueuedPromptToken::new().unwrap(),
        };
        let prepared = session.prepare_queued_prompt(prompt).await.unwrap();
        let committed = session
            .enqueue_prepared_queued_prompts(vec![prepared])
            .await;
        let committed = committed.into_iter().next().unwrap().unwrap();
        drop(held);
        let id = session.id();
        app.set_session(None);
        session.shutdown().await.unwrap();
        drop(session);
        let resumed = reopen(&app, id).await;
        app.set_session(Some(PreparedObservation::subscribe(resumed).await));
        assert!(app.paused);
        assert_eq!(app.queue.len(), 1);
        let row = &app.queue[0];
        assert_eq!(row.submission.text, "saved draft");
        assert_eq!(row.submission.attachments, [png_attachment("draft.png")]);
        assert_eq!(row.model, "first");
        assert!(matches!(&row.state, RowState::Ready(permit) if permit.identity() == draft));
        assert!(app.history.is_empty());
        // The committed row is retired durably; the saved draft stays reserved.
        assert_ne!(committed.submission, draft);
        journal_settles(&mut app, &mut rx, |records| {
            acknowledged(records, committed.submission) && !acknowledged(records, draft)
        })
        .await;
        let mut deliveries = hold_dispatcher(&mut app);
        app.command(Command::Resume);
        app.tick();
        let delivery = next_delivery(&mut deliveries);
        assert_eq!(delivery.prepared.identity(), draft);
        assert_eq!(text_of(&delivery.prepared), "saved draft");
        app.session().unwrap().shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn editing_or_cancelling_a_saved_row_abandons_its_attempt() {
        let (_root, mut app, mut rx, mut deliveries) = queue_fixture(false).await;
        app.submit("edit me".into());
        settle(&mut app, &mut rx, |app| in_flight(app, 0)).await;
        let delivery = next_delivery(&mut deliveries);
        let (id, first) = (delivery.id, delivery.prepared.identity());
        ack(&mut app, rejected());
        drop(delivery);
        let edited = app.remove_queued(id).unwrap();
        assert_eq!(edited.submission.text, "edit me");
        assert!(matches!(edited.state, RowState::Unsaved));
        journal_settles(&mut app, &mut rx, |records| acknowledged(records, first)).await;
        // Cancelling during preparation discards the permit once it arrives.
        app.paused = false;
        app.submit("cancel me".into());
        let id = app.queue[0].id;
        let RowState::Preparing(cancellation) = &app.queue[0].state else {
            panic!("preparing")
        };
        let second = cancellation.identity();
        assert!(app.remove_queued(id).is_some());
        assert!(app.queue.is_empty());
        journal_settles(&mut app, &mut rx, |records| {
            // Either the intent was never written or it was abandoned durably.
            let attempt = Some(second.attempt());
            !records.iter().any(|record| record.queue_attempt == attempt)
                || acknowledged(records, second)
        })
        .await;
        let (root, session) = (&app.launch.sessions, app.session_id().unwrap());
        let records = SessionStore::read_records(root, session).await.unwrap();
        let mut events = records.iter().map(|record| &record.event);
        assert!(!events.any(|event| matches!(event, SessionEvent::MessageCommitted { .. })));
        assert!(deliveries.try_recv().is_err());
        app.session().unwrap().shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn paused_rows_are_saved_and_a_removed_dispatch_is_abandoned_at_once() {
        let (_root, mut app, mut rx, mut deliveries) = queue_fixture(false).await;
        app.paused = true;
        app.submit("while paused".into());
        let ready = |app: &App| matches!(app.queue[0].state, RowState::Ready(_));
        settle(&mut app, &mut rx, ready).await;
        let saved = app.queue[0].state.identity().unwrap();
        app.paused = false;
        app.deliver_queue();
        assert!(in_flight(&app, 0));
        let _held = deliveries.try_recv().unwrap();
        // Abandoned on removal rather than on a receipt, so an immediate
        // shutdown cannot bring the message back on reopen.
        assert!(app.remove_queued(app.queue[0].id).is_some());
        journal_settles(&mut app, &mut rx, |records| acknowledged(records, saved)).await;
        app.session().unwrap().shutdown().await.unwrap();
    }
}
