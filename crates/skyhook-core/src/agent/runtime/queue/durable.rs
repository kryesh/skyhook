//! Durable draft admission, restart recovery and retirement. Preparation,
//! recovery reservations, dispatch and retirement share one gate; an attempt's
//! authority lives exactly as long as its token.

use super::*;
use crate::session::{QueueIntent, QueueSettlement};

impl SessionHandle {
    /// Store attachments and durably record an immutable draft without dispatching it.
    /// `None` model means no requested model change, not a model snapshot.
    pub async fn prepare_queued_prompt(
        &self,
        input: QueuedPrompt,
    ) -> Result<PreparedQueuedPrompt, QueuedPromptError> {
        let _gate = self.runtime.queue_state.gate.lock().await;
        if self.runtime.shutting_down.load(Ordering::Acquire) {
            return Err(QueuedPromptError::Rejected(HarnessError::AgentStopped));
        }
        if input.token.is_cancelled() {
            return Err(QueuedPromptError::Rejected(HarnessError::Interrupted));
        }
        let content = self
            .prepare_prompt(input.text, &input.attachments, &input.options)
            .await
            .map_err(QueuedPromptError::Rejected)?;
        // Register before the first durable write. Recovery cannot race this
        // producer because it requires the same gate.
        self.runtime.queue_state.register(&input.token);
        if input.token.is_cancelled() {
            return Err(QueuedPromptError::Rejected(HarnessError::Interrupted));
        }
        let intent = QueueIntent {
            attempt: input.token.identity().0,
            content,
            model: input.options.model,
        };
        let event = SessionEvent::QueueIntent {
            intent: intent.clone(),
        };
        self.runtime
            .store
            .append(self.root.clone(), event)
            .await
            .map_err(|error| input.token.cancellation_handle().failed(error.into()))?;
        Ok(PreparedQueuedPrompt {
            content: intent.content,
            model: intent.model,
            token: input.token,
        })
    }

    /// Publish all prepared drafts as one FIFO request-boundary batch and await
    /// their history-commit receipts, not their model turn. Once sent to the
    /// mailbox, dropping this waiter does not cancel dispatch.
    pub async fn enqueue_prepared_queued_prompts(
        &self,
        prepared: Vec<PreparedQueuedPrompt>,
    ) -> Vec<Result<QueuedPromptCommit, QueuedPromptError>> {
        let gate = self.runtime.queue_state.gate.lock().await;
        let mut batch = Vec::new();
        let mut receipts = Vec::new();
        for prepared in prepared {
            let observer = prepared.cancellation_handle();
            if self.runtime.shutting_down.load(Ordering::Acquire) {
                receipts.push(Err(observer.failed(HarnessError::AgentStopped)));
            } else if !self.runtime.queue_state.holds(&observer.0) {
                // A permit is only ever registered by the runtime that prepared or
                // recovered it: a stale or foreign permit cannot dispatch here. The
                // claim CAS later rejects a cancelled token with its own receipt.
                receipts.push(Err(observer.failed(QueueConflict::NotReserved.into())));
            } else {
                let (committed, receipt) = oneshot::channel();
                batch.push(QueuedInput {
                    prepared,
                    committed,
                });
                receipts.push(Ok((receipt, observer)));
            }
        }
        if !batch.is_empty() {
            let _ = self.root_tx.send(AgentCommand::QueuedInputs(batch)).await;
        }
        drop(gate);
        let mut results = Vec::new();
        for receipt in receipts {
            results.push(match receipt {
                Ok((receipt, observer)) => receipt
                    .await
                    .unwrap_or_else(|_| Err(observer.lost_receipt())),
                Err(error) => Err(error),
            });
        }
        results
    }

    /// Discover durable drafts. Only a draft saved by a previous process yields
    /// a retry permit, which reserves authority until consumed or dropped; a
    /// draft this runtime issued is `Released` to the holder of its handle.
    /// Repeated scans never mint duplicate authority; live preparations, queued
    /// inputs and claims are Unresolved.
    pub async fn recover_queued_prompts(&self) -> Result<Vec<RecoveredQueuedPrompt>, HarnessError> {
        let _gate = self.runtime.queue_state.gate.lock().await;
        let mut minted = Vec::new();
        let result = self.scan_queued_prompts(&mut minted).await;
        if result.is_err() {
            // The permits died with the failed scan; a rescan must mint them again.
            for identity in minted {
                self.runtime.queue_state.retire(identity);
            }
        }
        result
    }

    async fn scan_queued_prompts(
        &self,
        minted: &mut Vec<QueuedPromptIdentity>,
    ) -> Result<Vec<RecoveredQueuedPrompt>, HarnessError> {
        let queue = &self.runtime.queue_state;
        let mut recovered = Vec::new();
        for record in self.runtime.store.queue_intents().await? {
            if record.agent != self.root || record.acknowledged {
                continue;
            }
            let identity = QueuedPromptIdentity(record.intent.attempt);
            let state = if queue.holder(identity).is_some() {
                RecoveredQueuedPromptState::Unresolved
            } else if let Some(append) = record.message {
                RecoveredQueuedPromptState::Committed(QueuedPromptCommit {
                    submission: identity,
                    append,
                })
            } else if record.settlement.is_some() {
                // Abandoned but not yet acknowledged: never retry authority.
                continue;
            } else if queue.issued(identity) {
                RecoveredQueuedPromptState::Released
            } else {
                let token = QueuedPromptToken::with_identity(identity);
                queue.register(&token);
                minted.push(identity);
                RecoveredQueuedPromptState::Retry(PreparedQueuedPrompt {
                    content: record.intent.content.clone(),
                    model: record.intent.model.clone(),
                    token,
                })
            };
            let mut attachments = Vec::new();
            if !matches!(state, RecoveredQueuedPromptState::Committed(_)) {
                for block in &record.intent.content {
                    if let UserContent::Attachment { attachment } = block {
                        attachments.push(self.runtime.store.load_attachment(attachment).await?);
                    }
                }
            }
            recovered.push(RecoveredQueuedPrompt {
                submission: identity,
                content: record.intent.content,
                attachments,
                model: record.intent.model,
                state,
            });
        }
        Ok(recovered)
    }

    /// Regain the permit for a saved, uncommitted attempt this runtime issued
    /// after its previous permit was dropped (for example a rejected dispatch).
    /// The permit carries the journaled intent, never a new draft.
    pub async fn reclaim_queued_prompt(
        &self,
        attempt: &QueuedPromptCancellation,
    ) -> Result<PreparedQueuedPrompt, HarnessError> {
        let _gate = self.runtime.queue_state.gate.lock().await;
        if self.runtime.shutting_down.load(Ordering::Acquire) {
            return Err(HarnessError::AgentStopped);
        }
        let (queue, identity) = (&self.runtime.queue_state, attempt.identity());
        if !queue.issued(identity) {
            return Err(QueueConflict::NotReserved.into());
        }
        if queue.holder(identity).is_some() {
            return Err(QueueConflict::Held.into());
        }
        let intents = self.runtime.store.queue_intents().await?;
        let Some(record) = intents
            .into_iter()
            .find(|record| record.intent.attempt == identity.0 && record.agent == self.root)
        else {
            return Err(QueueConflict::Unknown.into());
        };
        if record.message.is_some() {
            return Err(QueueConflict::Committed.into());
        }
        if record.acknowledged || record.settlement.is_some() {
            return Err(QueueConflict::Unknown.into());
        }
        let token = QueuedPromptToken::rearm(attempt);
        queue.register(&token);
        Ok(PreparedQueuedPrompt {
            content: record.intent.content,
            model: record.intent.model,
            token,
        })
    }

    /// Retire a known committed row after the client has installed its receipt.
    /// An identity alone is never authority to abandon an uncommitted draft.
    pub async fn acknowledge_queued_prompt(
        &self,
        submission: QueuedPromptIdentity,
    ) -> Result<(), HarnessError> {
        self.retire(submission, None).await
    }

    /// Durably retire an attempt so it never resurfaces on reopen. Winning
    /// cancellation proves this handle's dispatch cannot commit; a live claim,
    /// or another live permit for the same attempt, is refused. An attempt that
    /// was never saved needs nothing, and a committed one must be acknowledged.
    pub async fn abandon_queued_prompt(
        &self,
        attempt: &QueuedPromptCancellation,
    ) -> Result<(), HarnessError> {
        self.retire(attempt.identity(), Some(attempt)).await
    }

    /// Settle (if needed) and acknowledge this root's attempt: acknowledged when
    /// committed, or abandoned through `abandon` when never committed.
    async fn retire(
        &self,
        identity: QueuedPromptIdentity,
        abandon: Option<&QueuedPromptCancellation>,
    ) -> Result<(), HarnessError> {
        let _gate = self.runtime.queue_state.gate.lock().await;
        // A live claim writes its own settlement; retiring first would make that
        // write fail. Only abandon's own, now cancelled, permit may be live.
        let cancelled = abandon.filter(|attempt| attempt.cancel());
        if self
            .runtime
            .queue_state
            .holder(identity)
            .is_some_and(|holder| cancelled.is_none_or(|own| !Arc::ptr_eq(&holder.0, &own.0)))
        {
            return Err(QueueConflict::Held.into());
        }
        let intents = self.runtime.store.queue_intents().await?;
        let Some(record) = intents
            .into_iter()
            .find(|record| record.intent.attempt == identity.0 && record.agent == self.root)
        else {
            return match abandon {
                Some(_) => {
                    self.runtime.queue_state.retire(identity);
                    Ok(())
                }
                None => Err(QueueConflict::Unknown.into()),
            };
        };
        if record.acknowledged {
            self.runtime.queue_state.retire(identity);
            return Ok(());
        }
        let settlement = match (abandon, record.message) {
            (None, Some(append)) => QueueSettlement::Committed {
                event: append.event,
            },
            (None, None) => return Err(QueueConflict::Uncommitted.into()),
            (Some(_), Some(_)) => return Err(QueueConflict::Committed.into()),
            (Some(_), None) => QueueSettlement::NotCommitted,
        };
        let (store, attempt) = (&self.runtime.store, identity.0);
        if record.settlement.is_none() {
            let event = SessionEvent::QueueSettlement {
                attempt,
                settlement,
            };
            store.append(self.root.clone(), event).await?;
        }
        store
            .append(
                self.root.clone(),
                SessionEvent::QueueAcknowledged { attempt },
            )
            .await?;
        self.runtime.queue_state.retire(identity);
        Ok(())
    }
}
