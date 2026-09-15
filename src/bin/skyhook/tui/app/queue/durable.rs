//! Journal-backed queue rows. Core owns every attempt identity, permit and
//! settlement; this module only moves UI rows between states in response to
//! durable preparation, journal reconciliation and acknowledgement outcomes.

use super::*;
use skyhook::agent::{HarnessError, PromptOptions, QueueConflict, QueuedPrompt, QueuedPromptToken};

/// Reconciliation of the attached session's queued rows with its journal.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::tui::app) enum QueueScan {
    /// Every journal row is represented in the queue.
    #[default]
    Synced,
    /// A requested scan is running. Rows the queue does not know are restored
    /// only when the previous scan failed; otherwise they were removed.
    Running { restore: bool },
    /// The last scan failed; journal rows may be missing from the queue.
    Failed,
}

/// Rebuild an editable row from immutable journal content and its loaded attachments.
fn content_hints(content: &[UserContent], attachments: Vec<Attachment>) -> Submission {
    let text = content
        .iter()
        .filter_map(|block| match block {
            UserContent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    Submission { text, attachments }
}

impl App {
    /// Park a submission behind the current operation and save it durably.
    pub(in crate::tui::app) fn prepare_queue_input(&mut self, queued: QueuedInput) {
        self.queue.push_back(queued);
        self.refresh_queue_menu();
        self.deliver_queue();
    }

    /// Start durable preparation for rows that never reached the journal, and
    /// reclaim the permit of saved rows whose attempt lost it.
    pub(in crate::tui::app) fn prepare_unbound_rows(&mut self, session: &SessionHandle) {
        // Only dispatch needs a permit: a paused queue leaves saved rows alone,
        // so a refused reclaim waits for Resume instead of retrying every tick.
        if !self.paused && !matches!(self.queue_scan, QueueScan::Running { .. }) {
            for index in 0..self.queue.len() {
                if let RowState::Saved(attempt) = &self.queue[index].state {
                    let attempt = attempt.clone();
                    self.reclaim_row(session, index, attempt);
                }
            }
        }
        for index in 0..self.queue.len() {
            if !matches!(self.queue[index].state, RowState::Unsaved) {
                continue;
            }
            let token = match QueuedPromptToken::new() {
                Ok(token) => token,
                Err(error) => {
                    self.paused = true;
                    self.notice(format!("Queued message was not saved: {error}"));
                    break;
                }
            };
            let input = &mut self.queue[index];
            input.generation = input.generation.wrapping_add(1);
            input.state = RowState::Preparing(token.cancellation_handle());
            let (id, generation) = (input.id, input.generation);
            let prompt = QueuedPrompt {
                text: input.submission.text.clone(),
                attachments: input.submission.attachments.clone(),
                options: PromptOptions {
                    model: Some(input.model.clone()),
                },
                token,
            };
            let session = session.clone();
            let tx = self.tx.clone();
            tokio::spawn(async move {
                let result = session.prepare_queued_prompt(prompt).await;
                let _ = tx.send(Work::QueuePrepared {
                    session: session.id(),
                    id,
                    generation,
                    result,
                });
            });
        }
    }

    fn reclaim_row(
        &mut self,
        session: &SessionHandle,
        index: usize,
        attempt: QueuedPromptCancellation,
    ) {
        let input = &mut self.queue[index];
        input.generation = input.generation.wrapping_add(1);
        input.state = RowState::Reclaiming(attempt.clone());
        let (id, generation) = (input.id, input.generation);
        let session = session.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = session.reclaim_queued_prompt(&attempt).await;
            let _ = tx.send(Work::QueueReclaimed {
                session: session.id(),
                id,
                generation,
                result,
            });
        });
    }

    /// Apply a reclaim like a preparation result: only the requesting row and
    /// generation may take the permit.
    pub(in crate::tui::app) fn queue_reclaimed(
        &mut self,
        id: QueuedInputId,
        generation: u64,
        result: Result<PreparedQueuedPrompt, HarnessError>,
    ) {
        let Some(index) = self.queue.iter().position(|input| {
            input.id == id
                && input.generation == generation
                && matches!(input.state, RowState::Reclaiming(_))
        }) else {
            return;
        };
        let RowState::Reclaiming(attempt) = std::mem::take(&mut self.queue[index].state) else {
            unreachable!("matched a reclaiming row");
        };
        match result {
            Ok(permit) => self.queue[index].state = RowState::Ready(permit),
            // Already in history: leave through normal bookkeeping.
            Err(HarnessError::Queue(QueueConflict::Committed)) => {
                let input = self.queue.remove(index).unwrap();
                self.commit_row(&input);
                if let Some(session) = self.session().cloned() {
                    self.acknowledge_attempt(&session, attempt.identity());
                }
            }
            // Settled as never committed: the row can be prepared afresh.
            Err(HarnessError::Queue(QueueConflict::Unknown)) => {
                self.queue[index].state = RowState::Unsaved;
            }
            Err(error) => {
                self.queue[index].state = RowState::Saved(attempt);
                self.pause_queue();
                self.notice(format!(
                    "Queued message could not be resumed: {error}. Resume to retry."
                ));
            }
        }
        self.deliver_queue();
        self.refresh_queue_menu();
        self.dirty = true;
    }

    pub(in crate::tui::app) fn queue_prepared(
        &mut self,
        id: QueuedInputId,
        generation: u64,
        result: Result<PreparedQueuedPrompt, QueuedPromptError>,
    ) {
        // Removing a row abandoned its attempt, so a late permit is just dropped.
        let Some(index) = self.queue.iter().position(|input| {
            input.id == id
                && input.generation == generation
                && matches!(input.state, RowState::Preparing(_))
        }) else {
            return;
        };
        match result {
            Ok(permit) => {
                self.queue[index].state = RowState::Ready(permit);
                self.deliver_queue();
            }
            Err(QueuedPromptError::Rejected(error)) => {
                self.queue[index].state = RowState::Unsaved;
                self.pause_queue();
                self.notice(format!(
                    "Queued message was not saved: {error}. Edit it or resume to retry."
                ));
            }
            Err(QueuedPromptError::Indeterminate(recovery)) => self.retain_uncertain(
                index,
                *recovery,
                "Queued message was not confirmed saved; reconciliation is required before retry",
            ),
        }
        self.refresh_queue_menu();
        self.dirty = true;
    }

    /// One journal scan at a time; its result reaches `queue_recovered`.
    pub(in crate::tui::app) fn request_queue_recovery(&mut self, session: &SessionHandle) {
        let restore = match self.queue_scan {
            QueueScan::Running { .. } => return,
            QueueScan::Synced => false,
            QueueScan::Failed => true,
        };
        self.queue_scan = QueueScan::Running { restore };
        let session = session.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = session
                .recover_queued_prompts()
                .await
                .map_err(|error| error.to_string());
            let _ = tx.send(Work::QueueRecovered {
                session: session.id(),
                result,
            });
        });
    }

    /// Apply a requested scan and dispatch whatever it released. The store lock
    /// guarantees a result from an earlier attachment arrives before reattaching.
    pub(in crate::tui::app) fn queue_recovered(
        &mut self,
        result: Result<Vec<RecoveredQueuedPrompt>, String>,
    ) {
        if let QueueScan::Running { restore } = self.queue_scan {
            self.restore_durable_queue(result, restore);
            self.deliver_queue();
        }
    }

    /// Apply the journal's view of every outstanding attempt: committed rows
    /// leave through normal history bookkeeping, drafts from a previous process
    /// regain their exclusive retry permit, released rows reclaim theirs with
    /// the row's handle, and unresolved rows stay blocked.
    pub(in crate::tui::app) fn restore_durable_queue(
        &mut self,
        result: Result<Vec<RecoveredQueuedPrompt>, String>,
        restore: bool,
    ) {
        let recovered = match result {
            Ok(recovered) => recovered,
            Err(error) => {
                self.queue_scan = QueueScan::Failed;
                self.paused = true;
                self.notice(format!(
                    "Queued messages could not be reconciled with the session journal: {error}"
                ));
                return;
            }
        };
        self.queue_scan = QueueScan::Synced;
        let Some(session) = self.session().cloned() else {
            return;
        };
        use RecoveredQueuedPromptState::{Committed, Released, Retry, Unresolved};
        let mut listed = Vec::with_capacity(recovered.len());
        for RecoveredQueuedPrompt {
            submission,
            content,
            attachments,
            model,
            state,
        } in recovered
        {
            listed.push(submission);
            let existing = self
                .queue
                .iter()
                .position(|input| input.state.identity() == Some(submission));
            match (existing, state) {
                (Some(index), Committed(commit)) => {
                    let input = self.queue.remove(index).unwrap();
                    self.commit_row(&input);
                    self.acknowledge_attempt(&session, commit.submission);
                }
                // Already part of the replayed history; only the receipt was lost.
                (None, Committed(commit)) => {
                    self.acknowledge_attempt(&session, commit.submission);
                }
                // Only a row without live authority takes the reclaimed permit.
                (Some(index), Retry(permit)) => {
                    let input = &mut self.queue[index];
                    if input.state.lacks_authority() {
                        input.state = RowState::Ready(permit);
                    }
                }
                (None, state @ (Retry(_) | Unresolved)) if restore => {
                    let mut input = self.queued_input(content_hints(&content, attachments));
                    input.model = model.unwrap_or(input.model);
                    input.state = match state {
                        Retry(permit) => RowState::Ready(permit),
                        _ => RowState::Unresolved(submission),
                    };
                    self.queue.push_back(input);
                }
                // The queue already knew every saved row, so this one was removed.
                (None, Retry(permit)) => {
                    self.abandon(permit.cancellation_handle());
                }
                // Definitely uncommitted and issued here: a recovery row's own
                // handle reclaims the permit once the queue resumes.
                (Some(index), Released) => {
                    let input = &mut self.queue[index];
                    if let RowState::Recovery { cancellation, .. } = &input.state {
                        input.state = RowState::Saved(cancellation.clone());
                    }
                }
                // Without a handle nothing here may re-arm it; the draft stays
                // saved in the journal and a reopen offers it for retry.
                (None, Released) => {}
                // Our own live permit, dispatch or reclaim is why the journal
                // cannot resolve it; a saved row learns that from its reclaim.
                (_, Unresolved) => {}
            }
        }
        // An attempt the journal no longer lists was never durably saved or was
        // already abandoned: the row is definitely uncommitted and can be
        // prepared afresh.
        for input in &mut self.queue {
            if input.state.lacks_authority()
                && input
                    .state
                    .identity()
                    .is_some_and(|identity| !listed.contains(&identity))
            {
                input.state = RowState::Unsaved;
            }
        }
        if self.queue_requires_recovery() {
            self.paused = true;
        }
        self.refresh_queue_menu();
        self.dirty = true;
    }

    /// Retire a committed row; a lost acknowledgement is retried on reopen.
    pub(in crate::tui::app) fn acknowledge_attempt(
        &self,
        session: &SessionHandle,
        submission: QueuedPromptIdentity,
    ) {
        let session = session.clone();
        let notices = self.root_notifier();
        tokio::spawn(async move {
            if let Err(error) = session.acknowledge_queued_prompt(submission).await {
                notices.send(format!("Could not acknowledge a queued message: {error}"));
            }
        });
    }

    /// Retire a removed row's saved attempt so it never resurfaces on reopen.
    pub(in crate::tui::app) fn abandon(&self, attempt: QueuedPromptCancellation) {
        let Some(session) = self.session().cloned() else {
            return;
        };
        let notices = self.root_notifier();
        tokio::spawn(async move {
            if let Err(error) = session.abandon_queued_prompt(&attempt).await {
                notices.send(format!("Could not discard a queued draft: {error}"));
            }
        });
    }
}
