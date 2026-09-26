//! Request-boundary user input mailbox and cancellation ownership.

use std::sync::atomic::AtomicU8;

use super::*;

mod input;

/// Who has a queued input: nobody yet, its submitter (withdrawn), or the runtime.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Claim {
    Pending = 0,
    Cancelled = 1,
    Claimed = 2,
}

impl Claim {
    fn load(atomic: &AtomicU8, ordering: Ordering) -> Self {
        Self::from_stored(atomic.load(ordering))
    }

    fn from_stored(value: u8) -> Self {
        match value {
            0 => Self::Pending,
            1 => Self::Cancelled,
            2 => Self::Claimed,
            _ => unreachable!("only a Claim is stored"),
        }
    }
}

/// Clonable handle deciding whether a queued input may still be withdrawn.
/// Cancellation and the runtime's claim are mutually exclusive.
#[derive(Clone, Debug, Default)]
pub struct QueuedPromptCancellation(Arc<AtomicU8>);

impl QueuedPromptCancellation {
    /// Success guarantees the user message cannot enter history. Failure means
    /// the runtime already claimed the input: await its receipt.
    #[must_use]
    pub fn cancel(&self) -> bool {
        self.transition(Claim::Cancelled) != Claim::Claimed
    }

    #[must_use]
    pub fn is_claimed(&self) -> bool {
        Claim::load(&self.0, Ordering::Acquire) == Claim::Claimed
    }

    fn try_claim(&self) -> bool {
        self.transition(Claim::Claimed) == Claim::Pending
    }

    /// Move a pending input to `claim`; returns the claim found.
    fn transition(&self, claim: Claim) -> Claim {
        let result = self.0.compare_exchange(
            Claim::Pending as u8,
            claim as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        Claim::from_stored(result.unwrap_or_else(|found| found))
    }
}

/// What consuming a queued batch did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct BatchOutcome {
    pub(super) consumed: bool,
    /// A claimed member failed: reject the batches queued behind it.
    pub(super) failed: bool,
}

/// One submission for the request-boundary mailbox.
#[derive(Debug, Default)]
pub struct QueuedPrompt {
    pub text: String,
    pub attachments: Vec<crate::media::Attachment>,
    pub options: Selection,
    /// Keep a clone to withdraw the input before the runtime claims it.
    pub cancellation: QueuedPromptCancellation,
}

pub(super) struct QueuedInput {
    pub content: Vec<UserPart>,
    pub options: Selection,
    pub cancellation: QueuedPromptCancellation,
    pub committed: oneshot::Sender<Result<(), HarnessError>>,
}

impl QueuedInput {
    fn reject(self, error: HarnessError) {
        let _ = self.cancellation.cancel();
        let _ = self.committed.send(Err(error));
    }
}

/// Resolves once the input's user message is committed to history, not when
/// the model turn ends. A closed receipt means the runtime stopped first.
pub type QueuedPromptReceipt = oneshot::Receiver<Result<(), HarnessError>>;

impl SessionHandle {
    /// Publish the prompts as one FIFO request-boundary batch, returning one
    /// receipt per prompt in input order. A prompt that fails validation holds
    /// back itself and every prompt behind it: their receipts already hold errors.
    pub async fn enqueue_prompts(&self, prompts: Vec<QueuedPrompt>) -> Vec<QueuedPromptReceipt> {
        let mut batch = Vec::new();
        let mut receipts = Vec::new();
        for prompt in prompts {
            let (committed, receipt) = oneshot::channel();
            receipts.push(receipt);
            let prepared = if self.runtime.shutting_down.load(Ordering::Acquire) {
                Err(HarnessError::AgentStopped)
            } else if receipts.len() > batch.len() + 1 {
                // Dispatching past a rejected prompt would reorder the submissions.
                Err(HarnessError::Interrupted)
            } else {
                self.prepare_prompt(prompt.text, &prompt.attachments, &prompt.options)
                    .await
            };
            match prepared {
                Ok(content) => batch.push(QueuedInput {
                    content,
                    options: prompt.options,
                    cancellation: prompt.cancellation,
                    committed,
                }),
                Err(error) => drop(committed.send(Err(error))),
            }
        }
        if !batch.is_empty() {
            self.runtime.redirect(&self.root, false).await;
            let _ = self.root_tx.send(AgentCommand::QueuedInputs(batch)).await;
        }
        receipts
    }
}

/// Reject unclaimed inputs on interrupt without discarding ordinary commands or
/// job notifications. The caller receives an error and retains the editable row.
pub(super) fn reject_pending(
    rx: &mut mpsc::Receiver<AgentCommand>,
    deferred: &mut VecDeque<AgentCommand>,
) {
    let count = rx.len();
    for _ in 0..count {
        let Ok(command) = rx.try_recv() else { break };
        deferred.push_back(command);
    }
    deferred.retain_mut(|command| {
        if let AgentCommand::QueuedInputs(inputs) = command {
            for input in inputs.drain(..) {
                input.reject(HarnessError::Interrupted);
            }
            false
        } else {
            true
        }
    });
}

impl SessionRuntime {
    /// Consume every member before allowing a provider request, even if commits
    /// yield. An interrupt, or a claimed member that fails, rejects the unclaimed
    /// remainder; the caller must then `reject_pending` the batches queued behind it.
    pub(super) async fn consume_queued_batch(
        &self,
        turn: &mut TurnContext<'_>,
        context: &mut AgentContext,
        settings: &mut AgentSettings,
        inputs: Vec<QueuedInput>,
    ) -> BatchOutcome {
        let cancellation = turn.cancellation;
        let mut outcome = BatchOutcome::default();
        for input in inputs {
            if outcome.failed || cancellation.is_cancelled() {
                input.reject(HarnessError::Interrupted);
            } else {
                let claim = input.cancellation.clone();
                let committed = self
                    .consume_queued_input(turn, context, settings, input)
                    .await;
                outcome.failed = !committed && claim.is_claimed();
                outcome.consumed |= committed;
            }
        }
        outcome
    }

    /// Drain a bounded snapshot of the mailbox, preserving all non-queue commands
    /// for the outer loop (notably JobsReady and ordinary prompt completion).
    pub(super) async fn consume_queued_inputs(
        &self,
        turn: &mut TurnContext<'_>,
        context: &mut AgentContext,
        settings: &mut AgentSettings,
        rx: &mut mpsc::Receiver<AgentCommand>,
        deferred: &mut VecDeque<AgentCommand>,
    ) -> bool {
        let mut consumed = false;
        let count = rx.len();
        for _ in 0..count {
            if turn.cancellation.is_cancelled() {
                break;
            }
            let Ok(command) = rx.try_recv() else { break };
            match command {
                AgentCommand::QueuedInputs(inputs) => {
                    let batch = self
                        .consume_queued_batch(turn, context, settings, inputs)
                        .await;
                    consumed |= batch.consumed;
                    if batch.failed {
                        reject_pending(rx, deferred);
                    }
                }
                command => deferred.push_back(command),
            }
        }
        consumed
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::super::*;
    pub(super) use crate::agent::runtime::tests::{
        AssistantItem, Script, Step, bounded, enqueue_prompts, ephemeral_session, events, models,
        provider_name, quiet_root, response,
    };
    pub(super) use crate::agent::runtime::tests::{Sent, SentPart};

    pub(super) fn count(script: &Script) -> usize {
        script.requests.lock().unwrap().len()
    }

    /// A session on provider "queue-test" with models "first" (default), "second",
    /// "third" and the text-only "blind".
    pub(super) async fn start(
        first_calls_tool: bool,
    ) -> (tempfile::TempDir, Arc<Script>, Arc<SessionHandle>) {
        let root = tempfile::tempdir().unwrap();
        // Only the first two responses are gated; request `n` is answered `answer-n`.
        let answer = |index| {
            response(vec![AssistantItem::text(
                "text/0",
                0,
                format!("answer-{index}"),
            )])
        };
        let todo = ToolCall::new("queue-todo", "todo", json!({"items": []})).unwrap();
        let todo = response(vec![AssistantItem::tool_call("queue-todo", 0, todo)]);
        let first = if first_calls_tool { todo } else { answer(0) };
        let steps = [Step::new(first).gated(), Step::new(answer(1)).gated()];
        let steps = steps
            .into_iter()
            .chain((2..6).map(|index| Step::new(answer(index))));
        let tracking = Script::new(steps, &Default::default());
        let profile = |model: &str| ModelProfile::new(model, None, 128_000, 4096, true);
        let harness = HarnessBuilder::new(root.path())
            .session_root(root.path().join("sessions"))
            .provider(
                provider_name("queue-test"),
                tracking.clone(),
                models([
                    ("first", profile("first-model")),
                    ("second", profile("second-model")),
                    ("third", profile("third-model")),
                    (
                        "blind",
                        ModelProfile {
                            supports_images: false,
                            ..profile("blind-model")
                        },
                    ),
                ]),
            )
            .default_model("queue-test/first".parse().unwrap())
            .build()
            .await
            .unwrap();
        let session = Arc::new(ephemeral_session(&harness).await);
        (root, tracking, session)
    }

    pub(super) fn prompt(
        session: &Arc<SessionHandle>,
        text: &'static str,
    ) -> tokio::task::JoinHandle<Result<String, HarnessError>> {
        let session = session.clone();
        tokio::spawn(async move { session.prompt(text).await })
    }
    pub(super) type Receipt = tokio::task::JoinHandle<Result<(), HarnessError>>;

    /// Enqueues from a task; returns its receipt and the submission's cancellation handle.
    pub(super) fn enqueue(
        session: &Arc<SessionHandle>,
        text: &'static str,
        attachments: Vec<crate::media::Attachment>,
        model: Option<&str>,
    ) -> (Receipt, QueuedPromptCancellation) {
        let input = queued(session, text, attachments, model);
        let cancellation = input.cancellation.clone();
        let session = session.clone();
        let receipt =
            tokio::spawn(
                async move { enqueue_prompts(&session, vec![input]).await.pop().unwrap() },
            );
        (receipt, cancellation)
    }

    /// Whether a message carries the successful result of the first response's todo call.
    pub(super) fn todo_finished(message: &Sent) -> bool {
        matches!(message, Sent::Tool(results)
            if results.iter().any(|result| result.call_id == "queue-todo" && !result.is_error))
    }

    fn enqueue_batch(
        session: &Arc<SessionHandle>,
        inputs: Vec<QueuedPrompt>,
    ) -> tokio::task::JoinHandle<Vec<Result<(), HarnessError>>> {
        let session = session.clone();
        tokio::spawn(async move { enqueue_prompts(&session, inputs).await })
    }

    // The provider gate prevents consumption. Waiting for the actual send also covers
    // asynchronous image loading/import, which merely polling once would not do.
    pub(super) async fn buffered(session: &SessionHandle, count: usize) {
        bounded(async {
            while session.root_tx.capacity() != AGENT_CHANNEL_CAPACITY - count {
                tokio::task::yield_now().await;
            }
        })
        .await;
    }

    /// The fixture's own prompts are sent as user text prefixed `test:`.
    pub(super) fn texts<'a>(messages: impl IntoIterator<Item = &'a Sent>) -> Vec<String> {
        user_texts(messages)
            .into_iter()
            .filter(|text| text.starts_with("test:"))
            .collect()
    }

    fn user_texts<'a>(messages: impl IntoIterator<Item = &'a Sent>) -> Vec<String> {
        let blocks = messages.into_iter().flat_map(|message| match message {
            Sent::User(blocks) => blocks.as_slice(),
            _ => &[],
        });
        blocks
            .filter_map(|block| match block {
                SentPart::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    /// Committed history as a provider would receive it.
    pub(super) async fn committed(session: &SessionHandle) -> Vec<Sent> {
        let records = session.runtime.store.records().await;
        events!(records, SessionEvent::MessageCommitted { message } => message.render())
    }

    pub(super) async fn model_changes(session: &SessionHandle) -> Vec<String> {
        let records = session.runtime.store.records().await;
        events!(records, SessionEvent::ModelChanged { profile } => profile.name.to_string())
    }

    pub(super) async fn stop(session: &SessionHandle) {
        bounded(session.shutdown()).await.unwrap();
        bounded(session.root_tx.closed()).await;
    }

    /// Parent input reaches the model as user text; the fixture's own prompts are
    /// the `test:` ones, so every other user text is parent input.
    pub(super) fn parent_inputs<'a>(messages: impl IntoIterator<Item = &'a Sent>) -> Vec<String> {
        user_texts(messages)
            .into_iter()
            .filter(|text| !text.starts_with("test:"))
            .collect()
    }

    fn queued(
        session: &SessionHandle,
        text: &str,
        attachments: Vec<crate::media::Attachment>,
        model: Option<&str>,
    ) -> QueuedPrompt {
        QueuedPrompt {
            text: text.into(),
            attachments,
            options: session
                .selection(model.map(|model| model.parse().unwrap()).as_ref(), None)
                .unwrap(),
            cancellation: QueuedPromptCancellation::default(),
        }
    }

    #[tokio::test]
    async fn idle_batch_prepares_images_before_any_request() {
        let (root, tracking, session) = start(false).await;
        let file = root.path().join("batch.png");
        let png = crate::tests::png(b"batch image fixture");
        let image = crate::media::Attachment::Image {
            file: Some(file.clone()),
            image: png.clone(),
        };
        let inputs = vec![
            queued(&session, "test:first", vec![], Some("queue-test/second")),
            queued(&session, "test:image", vec![image], None),
            queued(&session, "test:last", vec![], Some("queue-test/third")),
        ];
        let tokens = inputs.iter().map(|input| input.cancellation.clone());
        let tokens = tokens.collect::<Vec<_>>();
        let enqueue = enqueue_batch(&session, inputs);
        let request = tracking.request(0).await;
        assert!(bounded(enqueue).await.unwrap().iter().all(Result::is_ok));
        assert!(tokens.iter().all(QueuedPromptCancellation::is_claimed));
        let expected = ["test:first", "test:image", "test:last"];
        assert_eq!(texts(request.messages()), expected);
        assert_eq!(request.model, "third-model");
        let file = file.display().to_string();
        assert!(request.messages().any(|message| matches!(message,
            Sent::User(blocks) if blocks.iter().any(|block| matches!(block,
                SentPart::Attachment { attachment: crate::media::AttachmentRef::Image(image) }
                    if image.file.as_deref() == Some(file.as_str())
                        && request.blobs.get(&image.blob).ok() == Some(png.bytes()))))));
        tracking.release(0);
        // A normal prompt is a mailbox barrier after the batch's completed turn.
        let barrier = prompt(&session, "test:barrier");
        let next = tracking.request(1).await;
        let found = texts(next.messages());
        assert_eq!(found, [&expected[..], &["test:barrier"]].concat());
        tracking.release(1);
        bounded(barrier).await.unwrap().unwrap();
        stop(&session).await;
        assert_eq!(count(&tracking), 2);
    }

    #[tokio::test]
    async fn active_batch_skips_canceled_items_and_holds_back_everything_behind_a_rejection() {
        use HarnessError::{ImageLimit, Interrupted};
        let (_root, tracking, session) = start(true).await;
        let turn = prompt(&session, "test:initial");
        tracking.request(0).await;
        let canceled = QueuedPromptCancellation::default();
        let oversized = crate::media::Attachment::Image {
            file: None,
            image: crate::tests::png(&vec![0; MAX_IMAGE_BYTES as usize]),
        };
        let inputs = vec![
            queued(&session, "test:one", vec![], Some("queue-test/second")),
            QueuedPrompt {
                cancellation: canceled.clone(),
                ..queued(&session, "test:canceled", vec![], Some("queue-test/third"))
            },
            queued(&session, "test:two", vec![], None),
            queued(
                &session,
                "test:oversized-image",
                vec![oversized],
                Some("queue-test/third"),
            ),
            queued(&session, "test:behind", vec![], None),
        ];
        let enqueue = enqueue_batch(&session, inputs);
        buffered(&session, 1).await;
        assert!(canceled.cancel());
        tracking.release(0);
        let next = tracking.request(1).await;
        let results = bounded(enqueue).await.unwrap();
        assert!(matches!(
            results.as_slice(),
            [
                Ok(()),
                Err(Interrupted),
                Ok(()),
                Err(ImageLimit),
                Err(Interrupted)
            ]
        ));
        assert_eq!(next.model, "second-model");
        let found = texts(next.messages());
        assert_eq!(found, ["test:initial", "test:one", "test:two"]);
        tracking.release(1);
        bounded(turn).await.unwrap().unwrap();
        stop(&session).await;
        assert_eq!(count(&tracking), 2);
    }

    #[tokio::test]
    async fn interrupt_and_shutdown_resolve_every_buffered_batch_receipt() {
        for shutdown in [false, true] {
            let (_root, tracking, session) = start(false).await;
            let turn = prompt(&session, "test:initial");
            tracking.request(0).await;
            let inputs = vec![
                queued(&session, "test:one", vec![], None),
                queued(&session, "test:two", vec![], Some("queue-test/second")),
            ];
            let enqueue = enqueue_batch(&session, inputs);
            buffered(&session, 1).await;
            if shutdown {
                stop(&session).await;
            } else {
                session.interrupt().await;
            }
            let results = bounded(enqueue).await.unwrap();
            assert_eq!(results.len(), 2);
            assert!(results.iter().all(|result| shutdown && result.is_err()
                || matches!(result, Err(HarnessError::Interrupted))));
            let _ = bounded(turn).await.unwrap();
            stop(&session).await;
            assert_eq!(texts(&committed(&session).await), ["test:initial"]);
            assert_eq!(count(&tracking), 1);
        }
    }

    #[tokio::test]
    async fn idle_batch_of_a_canceled_input_starts_no_turn_and_journals_nothing() {
        let (_root, tracking, session) = start(false).await;
        let input = queued(
            &session,
            "test:already-canceled",
            vec![],
            Some("queue-test/second"),
        );
        assert!(input.cancellation.cancel());
        let results = bounded(enqueue_prompts(&session, vec![input])).await;
        assert!(matches!(results[..], [Err(HarnessError::Interrupted)]));
        stop(&session).await;
        assert_eq!(count(&tracking), 0);
        assert!(committed(&session).await.is_empty());
        assert!(model_changes(&session).await.is_empty());
    }

    #[tokio::test]
    async fn claimed_failure_holds_back_the_rest_of_the_batch() {
        let (_root, tracking, session) = start(false).await;
        let image = crate::media::Attachment::Image {
            file: None,
            image: crate::tests::png(b"unsupported"),
        };
        let inputs = vec![
            queued(
                &session,
                "test:image",
                vec![image],
                Some("queue-test/blind"),
            ),
            queued(&session, "test:behind", vec![], None),
        ];
        let results = bounded(enqueue_prompts(&session, inputs)).await;
        use HarnessError::{ImagesUnsupported, Interrupted};
        assert!(matches!(
            results[..],
            [Err(ImagesUnsupported(_)), Err(Interrupted)]
        ));
        stop(&session).await;
        assert_eq!(count(&tracking), 0);
        assert!(committed(&session).await.is_empty());
    }

    #[tokio::test]
    async fn claimed_failure_rejects_the_batches_queued_behind_it() {
        let (_root, tracking, session) = start(false).await;
        let turn = prompt(&session, "test:initial");
        tracking.request(0).await;
        let image = crate::media::Attachment::Image {
            file: None,
            image: crate::tests::png(b"unsupported"),
        };
        let first = enqueue_batch(
            &session,
            vec![
                queued(&session, "test:before", vec![], None),
                queued(
                    &session,
                    "test:image",
                    vec![image],
                    Some("queue-test/blind"),
                ),
            ],
        );
        buffered(&session, 1).await;
        let later = enqueue_batch(&session, vec![queued(&session, "test:later", vec![], None)]);
        buffered(&session, 2).await;
        tracking.release(0);
        use HarnessError::{ImagesUnsupported, Interrupted};
        let first = bounded(first).await.unwrap();
        assert!(matches!(first[..], [Ok(()), Err(ImagesUnsupported(_))]));
        let later = bounded(later).await.unwrap();
        assert!(matches!(later[..], [Err(Interrupted)]));
        let next = tracking.request(1).await;
        assert_eq!(texts(next.messages()), ["test:initial", "test:before"]);
        tracking.release(1);
        bounded(turn).await.unwrap().unwrap();
        stop(&session).await;
    }

    #[tokio::test]
    async fn dropped_receipt_still_commits_the_input_exactly_once() {
        let (_root, tracking, session) = start(false).await;
        let turn = prompt(&session, "test:initial");
        tracking.request(0).await;
        let (waiter, _) = enqueue(&session, "test:dropped", vec![], Some("queue-test/second"));
        buffered(&session, 1).await;
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        tracking.release(0);
        let next = tracking.request(1).await;
        let expected = ["test:initial", "test:dropped"];
        assert_eq!(next.model, "second-model");
        assert_eq!(texts(next.messages()), expected);
        tracking.release(1);
        bounded(turn).await.unwrap().unwrap();
        stop(&session).await;
        assert_eq!(texts(&committed(&session).await), expected);
    }

    #[tokio::test]
    async fn interrupt_and_shutdown_racing_a_claim_commit_exactly_the_ok_receipts() {
        for shutdown in [false, true] {
            let (_root, tracking, session) = start(false).await;
            let turn = prompt(&session, "test:initial");
            tracking.request(0).await;
            let inputs = vec![
                queued(&session, "test:one", vec![], Some("queue-test/second")),
                queued(&session, "test:two", vec![], Some("queue-test/third")),
            ];
            let claim = inputs[0].cancellation.clone();
            let enqueue = enqueue_batch(&session, inputs);
            buffered(&session, 1).await;
            tracking.release(0);
            // The claim precedes the commit's awaits: stop while it is in progress.
            bounded(async {
                while !claim.is_claimed() {
                    tokio::task::yield_now().await;
                }
            })
            .await;
            if shutdown {
                stop(&session).await;
            } else {
                session.interrupt().await;
            }
            let results = bounded(enqueue).await.unwrap();
            let _ = bounded(turn).await.unwrap();
            stop(&session).await;
            let accepted = results.iter().filter(|result| result.is_ok()).count();
            let found = texts(&committed(&session).await);
            assert_eq!(found[1..], ["test:one", "test:two"][..accepted]);
        }
    }

    #[test]
    fn cancellation_and_claim_are_exclusive_across_handles() {
        let cancelled = QueuedPromptCancellation::default();
        let observer = cancelled.clone();
        assert!(cancelled.cancel() && observer.cancel());
        assert!(!observer.try_claim() && !observer.is_claimed());

        let claimed = QueuedPromptCancellation::default();
        let copy = claimed.clone();
        assert!(claimed.try_claim());
        assert!(copy.is_claimed() && !copy.cancel() && !copy.try_claim());
    }
}
