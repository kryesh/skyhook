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

#[cfg(test)]
mod tests {
    // Independent public-boundary tests for actual durable queue reopen.

    use super::*;
    use crate::agent::runtime::tests::*;

    async fn durable_harness(root: &Path) -> Harness {
        let sessions = root.join("sessions");
        let builder = test_builder(root, &sessions, Arc::new(HangingProvider), true);
        builder.build().await.unwrap()
    }

    async fn reopen(harness: &Harness, session: SessionHandle) -> SessionHandle {
        let id = session.id();
        session.shutdown().await.unwrap();
        drop(session);
        // A completed runtime worker can release its last Arc after publishing its
        // completion. A real new SessionStore still must acquire the filesystem lock.
        bounded(async {
            loop {
                match harness.resume_session(id).await {
                    Ok(session) => return session,
                    Err(HarnessError::Session(SessionError::AlreadyOpen(_))) => {
                        tokio::task::yield_now().await
                    }
                    Err(error) => panic!("reopen failed: {error}"),
                }
            }
        })
        .await
    }

    async fn prepared_prompt(session: &SessionHandle, text: &str) -> PreparedQueuedPrompt {
        let prompt = QueuedPrompt {
            text: text.into(),
            attachments: vec![],
            options: PromptOptions::default(),
            token: QueuedPromptToken::new().unwrap(),
        };
        session.prepare_queued_prompt(prompt).await.unwrap()
    }

    async fn committed_user_messages(session: &SessionHandle) -> usize {
        let records = session.runtime.store.records().await;
        count!(&records, SessionEvent::MessageCommitted { message } if matches!(message, Message::User(_)))
    }

    #[tokio::test]
    async fn durable_queue_reopens_immutable_image_draft_without_external_file() {
        let root = tempfile::tempdir().unwrap();
        let harness = durable_harness(root.path()).await;
        let session = harness.new_session().await.unwrap();
        // The attachment names a file that never exists: only the stored blob may back the draft.
        let png = crate::tests::png(b"durable imported image fixture");
        let attachment = crate::media::Attachment::Image {
            file: Some(root.path().join("queued.png")),
            image: png.clone(),
        };
        let token = QueuedPromptToken::new().unwrap();
        let text = "immutable intended text".into();
        let prepared = QueuedPrompt {
            text,
            attachments: vec![attachment.clone()],
            options: PromptOptions::default(),
            token,
        };
        let prepared = session.prepare_queued_prompt(prepared).await.unwrap();
        let identity = prepared.identity();
        let content = prepared.content().to_vec();
        let UserContent::Attachment {
            attachment: crate::media::AttachmentRef::Image(image),
        } = &content[1]
        else {
            panic!("imported image")
        };
        let blob = async |session: &SessionHandle| {
            let limit = crate::media::MAX_IMAGE_BYTES as usize;
            let store = &session.runtime.store;
            store.read_blob(&image.blob, limit).await.unwrap()
        };
        assert_eq!(blob(&session).await, png.bytes());
        let dispatched = committed_user_messages(&session).await;
        // saving a paused draft must not dispatch a model turn
        assert_eq!(dispatched, 0);
        drop(prepared);
        let session = reopen(&harness, session).await;
        let mut recovered = session.recover_queued_prompts().await.unwrap();
        assert_eq!(recovered.len(), 1);
        let recovered = recovered.pop().unwrap();
        assert_eq!(recovered.submission, identity);
        assert_eq!(recovered.content, content);
        assert_eq!(recovered.attachments, vec![attachment]);
        let RecoveredQueuedPromptState::Retry(permit) = recovered.state else {
            panic!("undispatched durable intent grants an exclusive retry permit")
        };
        let duplicate = session.recover_queued_prompts().await.unwrap();
        let state = &duplicate[0].state;
        assert!(matches!(state, RecoveredQueuedPromptState::Unresolved));
        assert_eq!(blob(&session).await, png.bytes());
        let committed = session.enqueue_prepared_queued_prompts(vec![permit]).await;
        let committed = committed.into_iter().next().unwrap().unwrap();
        assert_eq!(committed.submission, identity);
        assert_eq!(committed_user_messages(&session).await, 1);
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn durable_queue_reopens_committed_before_client_ack_and_retires_durably() {
        let root = tempfile::tempdir().unwrap();
        let harness = durable_harness(root.path()).await;
        let session = harness.new_session().await.unwrap();
        let prepared = prepared_prompt(&session, "committed once").await;
        let committed = session
            .enqueue_prepared_queued_prompts(vec![prepared])
            .await;
        let committed = committed.into_iter().next().unwrap().unwrap();
        let session = reopen(&harness, session).await;
        let recovered = session.recover_queued_prompts().await.unwrap();
        assert_eq!(recovered.len(), 1);
        let RecoveredQueuedPromptState::Committed(replayed) = &recovered[0].state else {
            panic!("message commitment survives lost caller acknowledgment")
        };
        assert_eq!(replayed.submission, committed.submission);
        assert_eq!(replayed.append, committed.append);
        let acknowledged = session.acknowledge_queued_prompt(committed.submission);
        acknowledged.await.unwrap();
        let session = reopen(&harness, session).await;
        assert!(session.recover_queued_prompts().await.unwrap().is_empty());
        assert_eq!(committed_user_messages(&session).await, 1);
        session.shutdown().await.unwrap();
    }

    /// Only a saved draft's holder decides its fate: a retained draft resurfaces on
    /// reopen, while an abandoned draft does not.
    #[tokio::test]
    async fn abandoned_drafts_do_not_resurface_on_reopen() {
        let root = tempfile::tempdir().unwrap();
        let harness = durable_harness(root.path()).await;
        let session = harness.new_session().await.unwrap();
        let conflict = |result: Result<(), HarnessError>| match result {
            Err(HarnessError::Queue(conflict)) => conflict,
            other => panic!("expected a queue conflict, got {other:?}"),
        };
        let retained = prepared_prompt(&session, "retained").await.identity();
        let abandoned = prepared_prompt(&session, "abandoned").await;
        let attempt = abandoned.cancellation_handle();
        let held = session.acknowledge_queued_prompt(attempt.identity()).await;
        assert_eq!(conflict(held), QueueConflict::Held);
        drop(abandoned);
        let uncommitted = session.acknowledge_queued_prompt(attempt.identity()).await;
        assert_eq!(conflict(uncommitted), QueueConflict::Uncommitted);
        session.abandon_queued_prompt(&attempt).await.unwrap();
        session.abandon_queued_prompt(&attempt).await.unwrap();

        let session = reopen(&harness, session).await;
        let recovered = session.recover_queued_prompts().await.unwrap();
        let listed = recovered
            .iter()
            .map(|row| row.submission)
            .collect::<Vec<_>>();
        assert_eq!(listed, [retained]);
        session.shutdown().await.unwrap();
    }

    /// Within one runtime a dropped draft is never handed out by a scan: only the
    /// holder of its cancellation handle may reclaim it, while it stays uncommitted.
    #[tokio::test]
    async fn same_runtime_drafts_are_released_to_their_handle_not_the_scan() {
        let root = tempfile::tempdir().unwrap();
        let harness = durable_harness(root.path()).await;
        let session = harness.new_session().await.unwrap();
        let conflict = |result: Result<PreparedQueuedPrompt, HarnessError>| match result {
            Err(HarnessError::Queue(conflict)) => conflict,
            other => panic!("expected a queue conflict, got {other:?}"),
        };
        let only_state = async |session: &SessionHandle| {
            let mut rows = session.recover_queued_prompts().await.unwrap();
            assert_eq!(rows.len(), 1);
            rows.pop().unwrap().state
        };
        let prepared = prepared_prompt(&session, "released").await;
        let (attempt, content) = (prepared.cancellation_handle(), prepared.content().to_vec());
        let held = session.reclaim_queued_prompt(&attempt).await;
        assert_eq!(conflict(held), QueueConflict::Held);
        drop(prepared);
        let state = only_state(&session).await;
        assert!(matches!(state, RecoveredQueuedPromptState::Released));
        let permit = session.reclaim_queued_prompt(&attempt).await.unwrap();
        assert_eq!(
            (permit.identity(), permit.content()),
            (attempt.identity(), &content[..])
        );
        let state = only_state(&session).await;
        assert!(matches!(state, RecoveredQueuedPromptState::Unresolved));
        let duplicate = session.reclaim_queued_prompt(&attempt).await;
        assert_eq!(conflict(duplicate), QueueConflict::Held);
        let committed = session.enqueue_prepared_queued_prompts(vec![permit]).await;
        let committed = committed.into_iter().next().unwrap().unwrap();
        let late = session.reclaim_queued_prompt(&attempt).await;
        assert_eq!(conflict(late), QueueConflict::Committed);
        session
            .acknowledge_queued_prompt(committed.submission)
            .await
            .unwrap();
        let retired = session.reclaim_queued_prompt(&attempt).await;
        assert_eq!(conflict(retired), QueueConflict::NotReserved);
        let foreign = QueuedPromptToken::new().unwrap().cancellation_handle();
        let foreign = session.reclaim_queued_prompt(&foreign).await;
        assert_eq!(conflict(foreign), QueueConflict::NotReserved);

        // The row's existing handle also cancels the reclaimed permit, so abandon wins.
        let prepared = prepared_prompt(&session, "abandoned after reclaim").await;
        let attempt = prepared.cancellation_handle();
        drop(prepared);
        let permit = session.reclaim_queued_prompt(&attempt).await.unwrap();
        session.abandon_queued_prompt(&attempt).await.unwrap();
        let rejected = session.enqueue_prepared_queued_prompts(vec![permit]).await;
        assert!(rejected[0].is_err());

        // Shutdown refuses a new reclaim at once rather than waiting for its drain.
        let prepared = prepared_prompt(&session, "reclaimed during shutdown").await;
        let attempt = prepared.cancellation_handle();
        drop(prepared);
        session.shutdown().await.unwrap();
        let stopped = session.reclaim_queued_prompt(&attempt).await;
        assert!(matches!(stopped, Err(HarnessError::AgentStopped)));
        // A dropped draft from the previous process is again a retry permit.
        let session = reopen(&harness, session).await;
        let state = only_state(&session).await;
        assert!(matches!(state, RecoveredQueuedPromptState::Retry(_)));
        session.shutdown().await.unwrap();
    }
}
