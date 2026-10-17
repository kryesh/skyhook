//! Request-boundary user input mailbox and cancellation ownership.

use std::sync::atomic::AtomicU8;

use super::*;

mod input;
pub(super) use input::Adopting;

/// Who has a queued input: nobody yet, its submitter (withdrawn), the runtime,
/// or nobody again, held with the failed turn that refused it.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Claim {
    Pending = 0,
    Cancelled = 1,
    Claimed = 2,
    Retained = 3,
}

impl Claim {
    fn from_stored(value: u8) -> Self {
        match value {
            0 => Self::Pending,
            1 => Self::Cancelled,
            2 => Self::Claimed,
            3 => Self::Retained,
            _ => unreachable!("only a Claim is stored"),
        }
    }
}

/// Clonable handle deciding whether a queued input may still be withdrawn.
/// Cancellation and the runtime's claim are mutually exclusive.
#[derive(Clone, Default)]
pub struct QueuedPromptCancellation(Arc<Cancellation>);

#[derive(Default)]
struct Cancellation {
    claim: AtomicU8,
    /// Where the input was queued, which settles what withdrawing it from a
    /// held batch leaves.
    queued: std::sync::OnceLock<Queued>,
}

/// The session core and agent an input was queued for, held without keeping
/// the session open.
struct Queued {
    core: crate::session::WeakCore,
    agent: AgentId,
}

impl std::fmt::Debug for QueuedPromptCancellation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("QueuedPromptCancellation")
            .field(&self.claim())
            .finish()
    }
}

impl QueuedPromptCancellation {
    /// Success guarantees the user message cannot enter history. Failure means
    /// the runtime already claimed the input: await its receipt.
    #[must_use]
    pub fn cancel(&self) -> bool {
        let found = self.transition(Claim::Cancelled);
        // A failed turn whose held batch this empties rests.
        if found == Claim::Retained
            && let Some(Queued { core, agent }) = self.0.queued.get()
        {
            let agent = agent.clone();
            core.enqueue(move |state| {
                super::agents::settle_held(state, &agent);
            });
        }
        found != Claim::Claimed
    }

    fn queue_for(&self, core: crate::session::WeakCore, agent: AgentId) {
        let _ = self.0.queued.set(Queued { core, agent });
    }

    /// Whether the session holds the input with the failed turn that refused
    /// its batch, for `continue` to try again; withdrawing it still succeeds.
    pub fn is_retained(&self) -> bool {
        self.claim() == Claim::Retained
    }

    #[cfg(test)]
    pub(super) fn is_claimed(&self) -> bool {
        self.claim() == Claim::Claimed
    }

    fn is_withdrawn(&self) -> bool {
        self.claim() == Claim::Cancelled
    }

    fn claim(&self) -> Claim {
        Claim::from_stored(self.0.claim.load(Ordering::Acquire))
    }

    fn try_claim(&self) -> bool {
        matches!(
            self.transition(Claim::Claimed),
            Claim::Pending | Claim::Retained
        )
    }

    /// Hold the input with a refused batch, unless it was withdrawn: the
    /// runtime's own claim, or an input it was sent, moves to retained.
    fn hold(&self) -> bool {
        let mut found = self.claim();
        loop {
            match found {
                Claim::Retained => return true,
                Claim::Cancelled => return false,
                Claim::Pending | Claim::Claimed => {
                    let held = (self.0.claim).compare_exchange(
                        found as u8,
                        Claim::Retained as u8,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                    match held {
                        Ok(_) => return true,
                        Err(actual) => found = Claim::from_stored(actual),
                    }
                }
            }
        }
    }

    /// Move an input nobody holds to `claim`; returns the claim found.
    fn transition(&self, claim: Claim) -> Claim {
        let mut found = self.claim();
        while matches!(found, Claim::Pending | Claim::Retained) {
            match (self.0.claim).compare_exchange(
                found as u8,
                claim as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(actual) => found = Claim::from_stored(actual),
            }
        }
        found
    }
}

/// The model and mode a queued prompt committed under: those its batch, or the
/// `continue` that retried it, selected, or else the agent's own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuedPromptCommitted {
    pub model: crate::provider::profile::ModelRef,
    pub mode: Option<crate::tool::policy::ModeName>,
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
    /// Checked and stored with the batch a request boundary takes it in.
    pub attachments: Vec<crate::media::Attachment>,
    pub options: Selection,
    pub cancellation: QueuedPromptCancellation,
    pub committed: oneshot::Sender<Result<QueuedPromptCommitted, HarnessError>>,
}

impl QueuedInput {
    pub(super) fn reject(self, error: HarnessError) {
        let _ = self.cancellation.cancel();
        let _ = self.committed.send(Err(error));
    }
}

/// Queued inputs taken as one submission, with the selection that overrides
/// theirs: claimed while a boundary or `continue` commits them, held while a
/// failed turn keeps them for `continue`.
#[derive(Default)]
pub(super) struct Batch {
    inputs: Vec<QueuedInput>,
    selection: Selection,
}

impl Batch {
    /// Claim `inputs` too, in order; one withdrawn meanwhile is handed back.
    fn claim(&mut self, inputs: impl IntoIterator<Item = QueuedInput>) {
        for input in inputs {
            match input.cancellation.try_claim() {
                true => self.inputs.push(input),
                false => input.reject(HarnessError::Interrupted),
            }
        }
    }

    /// Hold `inputs` too, in order; one withdrawn meanwhile is handed back.
    pub(super) fn join(&mut self, inputs: impl IntoIterator<Item = QueuedInput>) {
        for input in inputs {
            match input.cancellation.hold() {
                true => self.inputs.push(input),
                false => input.reject(HarnessError::Interrupted),
            }
        }
    }

    /// The batch held for `continue`, unless nothing in it is left.
    pub(super) fn held(self) -> Option<Self> {
        let mut held = Self {
            inputs: Vec::new(),
            selection: self.selection,
        };
        held.join(self.inputs);
        (!held.inputs.is_empty()).then_some(held)
    }

    /// The held batch claimed for `continue` to commit under `selection`, where it
    /// names a model or mode, unless everything in it was withdrawn.
    pub(super) fn retried(self, selection: Selection) -> Option<Self> {
        let mut retried = Self {
            inputs: Vec::new(),
            selection: Selection {
                model: selection.model.or(self.selection.model),
                mode: selection.mode.or(self.selection.mode),
            },
        };
        retried.claim(self.inputs);
        (!retried.inputs.is_empty()).then_some(retried)
    }

    /// Whether anything held is left, once what was withdrawn since is dropped.
    pub(super) fn holds(&mut self) -> bool {
        let (kept, withdrawn): (Vec<_>, Vec<_>) = std::mem::take(&mut self.inputs)
            .into_iter()
            .partition(|input| !input.cancellation.is_withdrawn());
        self.inputs = kept;
        withdrawn
            .into_iter()
            .for_each(|input| input.reject(HarnessError::Interrupted));
        !self.inputs.is_empty()
    }

    /// Give every input back to its submitter.
    pub(super) fn hand_back(self) {
        (self.inputs.into_iter()).for_each(|input| input.reject(HarnessError::Interrupted));
    }
}

/// Why the session refused what a turn took, with the queued batch it refused,
/// which the turn's failure holds.
pub(super) struct Refusal {
    pub(super) error: HarnessError,
    pub(super) batch: Option<Box<Batch>>,
}

impl From<HarnessError> for Refusal {
    fn from(error: HarnessError) -> Self {
        Self { error, batch: None }
    }
}

/// Resolves once the input's user message is committed to history, not when
/// the model turn ends, or with why it never will be: an input the session
/// dropped unclaimed reports the session's refusal, or the root agent stopped.
/// One withdrawn is `Interrupted`. One in a batch the session refused waits
/// with the turn the refusal failed, which reports why, for `continue`.
pub struct QueuedPromptReceipt {
    committed: oneshot::Receiver<Result<QueuedPromptCommitted, HarnessError>>,
    store: crate::session::WeakStore,
}

impl std::future::IntoFuture for QueuedPromptReceipt {
    type Output = Result<QueuedPromptCommitted, HarnessError>;
    type IntoFuture = futures_util::future::BoxFuture<'static, Self::Output>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move {
            let Ok(committed) = self.committed.await else {
                return Err(match self.store.upgrade() {
                    Some(store) => super::dropped_input(&store).await,
                    None => SessionError::Refused(crate::session::SessionPhase::Closed).into(),
                });
            };
            committed
        })
    }
}

impl SessionHandle {
    /// Queue the prompts in order for the next request boundary, returning one
    /// receipt per prompt. A prompt already withdrawn resolves `Interrupted`.
    /// Everything queued when a boundary takes it is one submission, checked and
    /// committed whole or not at all; while a failed turn holds a refused batch,
    /// they join it for `continue`.
    pub async fn enqueue_prompts(&self, prompts: Vec<QueuedPrompt>) -> Vec<QueuedPromptReceipt> {
        let mut receipts = Vec::with_capacity(prompts.len());
        let mut batch = Vec::with_capacity(prompts.len());
        for prompt in prompts {
            let (committed, receipt) = oneshot::channel();
            receipts.push(QueuedPromptReceipt {
                committed: receipt,
                store: self.runtime.store.downgrade(),
            });
            if prompt.cancellation.is_withdrawn() {
                let _ = committed.send(Err(HarnessError::Interrupted));
                continue;
            }
            let core = self.runtime.store.weak_core();
            prompt.cancellation.queue_for(core, self.root.clone());
            batch.push(QueuedInput {
                content: vec![UserPart::Text { text: prompt.text }],
                attachments: prompt.attachments,
                options: prompt.options,
                cancellation: prompt.cancellation,
                committed,
            });
        }
        if batch.is_empty() {
            return receipts;
        }
        // A session that cannot run takes nothing: the first prompt says why.
        if let Err(error) = self.running().await {
            let mut batch = batch.into_iter();
            if let Some(first) = batch.next() {
                first.reject(error);
            }
            batch.for_each(|input| input.reject(HarnessError::Interrupted));
            return receipts;
        }
        // Queued with the releases, so the turn the root's held wait returns to
        // finds the batch at its next request boundary.
        let queued = AgentCommand::QueuedInputs(batch);
        let _ = self.runtime.redirect(&self.root, queued).await;
        receipts
    }
}

impl SessionRuntime {
    /// Consume, as one submission before a provider request, `batch`, then
    /// `taken`, then every input queued for the agent now: claim every member
    /// its submitter has not withdrawn, check each, adopt the last model and
    /// mode any of them names, or the batch's own, then store their attachments
    /// and commit their messages with that selection in one transaction. None
    /// when it claims nothing, or an interrupt stopped the turn first, which
    /// hands the batch back. A batch the session refuses is the refusal's, for
    /// the failure of the turn to hold.
    pub(super) async fn consume_queued(
        &self,
        turn: &mut TurnContext<'_>,
        context: &mut AgentContext,
        settings: &mut AgentSettings,
        mut batch: Batch,
        taken: Vec<QueuedInput>,
    ) -> Option<Result<(), Refusal>> {
        let agent = turn.agent.clone();
        let queued = (self.store)
            .live(move |state| state.agents.take_queued(&agent))
            .await;
        batch.claim(taken.into_iter().chain(queued));
        if batch.inputs.is_empty() {
            return None;
        }
        let (mut options, mut images, mut admitted) = (Selection::default(), false, Ok(()));
        for input in &batch.inputs {
            admitted =
                admitted.and_then(|()| self.admit_message(&input.attachments, &input.options));
            options.model = input.options.model.clone().or(options.model);
            options.mode = input.options.mode.clone().or(options.mode);
            images |= input.content.iter().any(UserPart::is_image)
                || (input.attachments.iter())
                    .any(|attachment| matches!(attachment, crate::media::Attachment::Image { .. }));
        }
        options.model = batch.selection.model.clone().or(options.model);
        options.mode = batch.selection.mode.clone().or(options.mode);
        let adopted = async {
            admitted?;
            let selected = self.select(turn, context, settings, options, images);
            let selected = selected.await?;
            let mut joins = Vec::with_capacity(batch.inputs.len());
            for input in &batch.inputs {
                let content = input.content.clone();
                let content = self.with_attachments(content, &input.attachments).await?;
                joins.push(Message::User(content));
            }
            let adopting = Adopting::Joins(joins);
            self.adopt(turn, context, settings, selected, adopting)
                .await
        };
        match adopted.await {
            Ok(()) => {
                let committed = QueuedPromptCommitted {
                    model: context.model.profile.name.clone(),
                    mode: (settings.mode.as_ref()).map(|mode| mode.name.clone()),
                };
                for input in batch.inputs {
                    let _ = input.committed.send(Ok(committed.clone()));
                }
                Some(Ok(()))
            }
            // The interrupt already handed back what was queued: what waits since
            // is new input.
            Err(HarnessError::Interrupted) => {
                batch.hand_back();
                None
            }
            Err(error) => {
                let batch = Some(Box::new(batch));
                Some(Err(Refusal { error, batch }))
            }
        }
    }

    /// Consume the input queued now at a busy turn's request boundary; whether
    /// any committed, or why the session refused it, which fails the turn.
    pub(super) async fn consume_queued_inputs(
        &self,
        turn: &mut TurnContext<'_>,
        context: &mut AgentContext,
        settings: &mut AgentSettings,
    ) -> Result<bool, Refusal> {
        let consumed = self.consume_queued(turn, context, settings, Batch::default(), Vec::new());
        consumed
            .await
            .transpose()
            .map(|consumed| consumed.is_some())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::super::*;
    pub(super) use crate::agent::runtime::tests::{
        AssistantItem, Script, Step, bounded, enqueue_prompts, ephemeral_session, events, models,
        provider_name, quiet_root, response, spawn_prompt, turn,
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
        let profile = |model: &str| crate::tests::profile(model, true);
        let harness = HarnessBuilder::new(root.path())
            .session_root(root.path().join("sessions"))
            .coalescing(wait::Coalescing::NONE)
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

    pub(super) type Receipt = tokio::task::JoinHandle<Result<QueuedPromptCommitted, HarnessError>>;

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
    ) -> tokio::task::JoinHandle<Vec<Result<QueuedPromptCommitted, HarnessError>>> {
        let session = session.clone();
        tokio::spawn(async move { enqueue_prompts(&session, inputs).await })
    }

    // The provider gate prevents consumption. Waiting for the actual send also covers
    // asynchronous image loading/import, which merely polling once would not do.
    pub(super) async fn buffered(session: &SessionHandle, count: usize) {
        bounded(async {
            while super::super::tests::inbox(session, &session.root) != count {
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
    pub(super) fn committed(session: &SessionHandle) -> Vec<Sent> {
        let records = session.runtime.store.records();
        events!(records, SessionEvent::MessageCommitted { message } => crate::agent::runtime::tests::sent(message))
    }

    pub(super) fn model_changes(session: &SessionHandle) -> Vec<String> {
        let records = session.runtime.store.records();
        events!(records, SessionEvent::ModelChanged { profile } => profile.name.to_string())
    }

    pub(super) async fn stop(session: &SessionHandle) {
        bounded(session.shutdown()).await.unwrap();
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
        assert_eq!(request.model.as_str(), "third-model");
        // Only the selection the batch ends with is journaled.
        assert_eq!(model_changes(&session), ["queue-test/third"]);
        let file = file.display().to_string();
        assert!(request.messages().any(|message| matches!(message,
            Sent::User(blocks) if blocks.iter().any(|block| matches!(block,
                SentPart::Attachment { attachment: crate::media::AttachmentRef::Image(image) }
                    if image.file.as_deref() == Some(file.as_str())
                        && request.blobs.get(&image.blob).ok() == Some(png.bytes()))))));
        tracking.release(0);
        // A normal prompt is a mailbox barrier after the batch's completed turn.
        let barrier = spawn_prompt(&session, "test:barrier");
        let next = tracking.request(1).await;
        let found = texts(next.messages());
        assert_eq!(found, [&expected[..], &["test:barrier"]].concat());
        tracking.release(1);
        bounded(barrier).await.unwrap().unwrap();
        stop(&session).await;
        assert_eq!(count(&tracking), 2);
    }

    /// An input withdrawn once queued is skipped unchecked: what it carries
    /// cannot refuse its batch.
    #[tokio::test]
    async fn a_withdrawn_input_is_skipped_and_cannot_refuse_its_batch() {
        let (_root, tracking, session) = start(true).await;
        let turn = spawn_prompt(&session, "test:initial");
        tracking.request(0).await;
        let withdrawn = QueuedPromptCancellation::default();
        let inputs = vec![
            queued(&session, "test:one", vec![], Some("queue-test/second")),
            QueuedPrompt {
                cancellation: withdrawn.clone(),
                ..queued(
                    &session,
                    "test:withdrawn",
                    vec![oversized()],
                    Some("queue-test/third"),
                )
            },
            queued(&session, "test:two", vec![], None),
        ];
        let enqueue = enqueue_batch(&session, inputs);
        buffered(&session, 1).await;
        assert!(withdrawn.cancel());
        tracking.release(0);
        let next = tracking.request(1).await;
        let results = bounded(enqueue).await.unwrap();
        assert!(matches!(
            results.as_slice(),
            [Ok(_), Err(HarnessError::Interrupted), Ok(_)]
        ));
        assert_eq!(next.model.as_str(), "second-model");
        let found = texts(next.messages());
        assert_eq!(found, ["test:initial", "test:one", "test:two"]);
        tracking.release(1);
        bounded(turn).await.unwrap().unwrap();
        stop(&session).await;
        assert_eq!(count(&tracking), 2);
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
        assert!(committed(&session).is_empty());
        assert!(model_changes(&session).is_empty());
    }

    /// Every batch queued at a request boundary is one submission, checked
    /// there: a prompt the session cannot take refuses all of them, whichever
    /// call queued it. Mid tool loop, that fails and parks the turn, and
    /// `continue` makes its next request with the batch, on the model it picks.
    #[tokio::test]
    async fn batches_queued_at_one_boundary_are_checked_and_refused_together() {
        let (_root, tracking, session) = start(true).await;
        let turn = spawn_prompt(&session, "test:initial");
        tracking.request(0).await;
        let first = vec![queued(
            &session,
            "test:a",
            vec![],
            Some("queue-test/second"),
        )];
        let first = enqueue_batch(&session, first);
        buffered(&session, 1).await;
        let blind = vec![queued(
            &session,
            "test:b",
            vec![unsupported()],
            Some("queue-test/blind"),
        )];
        let second = enqueue_batch(&session, blind);
        buffered(&session, 2).await;
        tracking.release(0);
        let ended = bounded(turn).await.unwrap();
        assert!(matches!(ended, Err(HarnessError::ImagesUnsupported(_))));
        assert_eq!(failures(&session).len(), 1);
        assert!(!first.is_finished() && !second.is_finished());
        let continued = retry(&session, "queue-test/third");
        let next = tracking.request(1).await;
        assert_eq!(next.model.as_str(), "third-model");
        assert_eq!(texts(next.messages()), ["test:initial", "test:a", "test:b"]);
        assert!(next.messages().any(todo_finished));
        for results in [first, second] {
            assert!(bounded(results).await.unwrap().iter().all(Result::is_ok));
        }
        tracking.release(1);
        assert_eq!(bounded(continued).await.unwrap().unwrap(), "answer-1");
        stop(&session).await;
    }

    /// A batch refused once the turn has answered leaves the answer standing,
    /// and `continue` tries the batch again as a turn of its own.
    #[tokio::test]
    async fn a_batch_refused_after_the_answer_leaves_it_standing() {
        let (_root, tracking, session) = start(false).await;
        let turn = spawn_prompt(&session, "test:initial");
        tracking.request(0).await;
        let blind = Some("queue-test/blind");
        let refused = vec![queued(&session, "test:refused", vec![unsupported()], blind)];
        let refused = enqueue_batch(&session, refused);
        buffered(&session, 1).await;
        tracking.release(0);
        assert_eq!(bounded(turn).await.unwrap().unwrap(), "answer-0");
        assert_eq!(failures(&session).len(), 1);
        let continued = retry(&session, "queue-test/second");
        let next = tracking.request(1).await;
        assert_eq!(next.model.as_str(), "second-model");
        assert_eq!(texts(next.messages()), ["test:initial", "test:refused"]);
        assert!(bounded(refused).await.unwrap().iter().all(Result::is_ok));
        tracking.release(1);
        assert_eq!(bounded(continued).await.unwrap().unwrap(), "answer-1");
        stop(&session).await;
    }

    /// A batch an idle root's session refuses fails the turn it began and is
    /// held with it: input reaching the root meanwhile waits with the batch. A
    /// `continue` that is refused again fails and holds it again, journaling no
    /// selection; one on a model that takes it claims it at once, so it can no
    /// longer be withdrawn, and commits it all on that model. Once a held batch
    /// is withdrawn, the root rests at once and new input runs on its own.
    #[tokio::test]
    async fn a_refused_idle_batch_fails_its_turn_and_waits_for_continue() {
        let (_root, tracking, session) = start(false).await;
        let root = session.root.clone();
        let rests = async |parked: bool| {
            bounded(async {
                while matches!(turn(&session, &root), Turn::Parked(_)) != parked {
                    tokio::task::yield_now().await;
                }
            })
            .await
        };
        let until = async |done: &dyn Fn() -> bool| {
            bounded(async {
                while !done() {
                    tokio::task::yield_now().await;
                }
            })
            .await
        };
        let blind = Some("queue-test/blind");
        let refused = queued(&session, "test:refused", vec![unsupported()], blind);
        let first = refused.cancellation.clone();
        let refused = enqueue_batch(&session, vec![refused]);
        rests(true).await;
        let joined = queued(&session, "test:joined", vec![], None);
        let held = joined.cancellation.clone();
        let joined = enqueue_batch(&session, vec![joined]);
        until(&|| held.is_retained()).await;
        let again = bounded(retry(&session, "queue-test/blind")).await.unwrap();
        assert!(matches!(again, Err(HarnessError::ImagesUnsupported(_))));
        until(&|| first.is_retained() && held.is_retained()).await;
        assert!(model_changes(&session).is_empty());
        assert_eq!(count(&tracking), 0);
        let continued = retry(&session, "queue-test/second");
        // Withdrawing what `continue` took finds it already being submitted.
        until(&|| turn(&session, &root) == Turn::Busy).await;
        assert!(!first.cancel() && !held.cancel());
        let next = tracking.request(0).await;
        assert_eq!(next.model.as_str(), "second-model");
        assert_eq!(texts(next.messages()), ["test:refused", "test:joined"]);
        for results in [refused, joined] {
            assert!(bounded(results).await.unwrap().iter().all(Result::is_ok));
        }
        tracking.release(0);
        assert_eq!(bounded(continued).await.unwrap().unwrap(), "answer-0");
        rests(false).await;
        let empty = crate::media::Attachment::Image {
            file: None,
            image: crate::tests::png(b""),
        };
        let many = vec![empty; MAX_IMAGES_PER_SUBMISSION + 1];
        let many = queued(&session, "test:many-images", many, None);
        let cancellation = many.cancellation.clone();
        let many = enqueue_batch(&session, vec![many]);
        rests(true).await;
        assert!(cancellation.cancel());
        rests(false).await;
        assert!(bounded(many).await.unwrap().iter().all(Result::is_err));
        let next = enqueue_batch(&session, vec![queued(&session, "test:next", vec![], None)]);
        let request = tracking.request(1).await;
        let texts = texts(request.messages());
        assert_eq!(texts, ["test:refused", "test:joined", "test:next"]);
        assert!(bounded(next).await.unwrap().iter().all(Result::is_ok));
        tracking.release(1);
        stop(&session).await;
        let failures = failures(&session);
        assert!(
            failures[0].contains("does not support image inputs"),
            "{failures:?}"
        );
        assert_eq!(failures[1], failures[0]);
        assert_eq!(failures[2..], [HarnessError::ImageLimit.to_string()]);
        assert_eq!(count(&tracking), 2);
    }

    /// A direct prompt breaks the hold of a refused batch: the batch is handed
    /// back, and the prompt runs on its own selection.
    #[tokio::test]
    async fn a_prompt_breaks_the_hold_of_a_refused_batch() {
        let (_root, tracking, session) = start(false).await;
        let blind = Some("queue-test/blind");
        let refused = queued(&session, "test:refused", vec![unsupported()], blind);
        let held = refused.cancellation.clone();
        let refused = enqueue_batch(&session, vec![refused]);
        bounded(async {
            while !held.is_retained() || !matches!(turn(&session, &session.root), Turn::Parked(_)) {
                tokio::task::yield_now().await;
            }
        })
        .await;
        let prompt = spawn_prompt(&session, "test:direct");
        let request = tracking.request(0).await;
        assert_eq!(texts(request.messages()), ["test:direct"]);
        assert_eq!(request.model.as_str(), "first-model");
        let refused = bounded(refused).await.unwrap();
        assert!(matches!(refused[..], [Err(HarnessError::Interrupted)]));
        tracking.release(0);
        assert_eq!(bounded(prompt).await.unwrap().unwrap(), "answer-0");
        stop(&session).await;
    }

    /// Withdrawing the whole batch a refusal mid tool loop held leaves the turn
    /// open: `continue` goes on with it, once, without the batch.
    #[tokio::test]
    async fn continue_goes_on_with_a_turn_whose_held_batch_was_withdrawn() {
        let (_root, tracking, session) = start(true).await;
        let turn_task = spawn_prompt(&session, "test:initial");
        tracking.request(0).await;
        let blind = Some("queue-test/blind");
        let refused = queued(&session, "test:refused", vec![unsupported()], blind);
        let held = refused.cancellation.clone();
        let refused = enqueue_batch(&session, vec![refused]);
        buffered(&session, 1).await;
        tracking.release(0);
        assert!(bounded(turn_task).await.unwrap().is_err());
        assert!(held.is_retained() && held.cancel());
        let refused = bounded(refused).await.unwrap();
        assert!(matches!(refused[..], [Err(HarnessError::Interrupted)]));
        let continued = retry(&session, "queue-test/first");
        let request = tracking.request(1).await;
        assert_eq!(texts(request.messages()), ["test:initial"]);
        assert!(request.messages().any(todo_finished));
        tracking.release(1);
        assert_eq!(bounded(continued).await.unwrap().unwrap(), "answer-1");
        stop(&session).await;
        assert_eq!(count(&tracking), 2);
    }

    fn unsupported() -> crate::media::Attachment {
        crate::media::Attachment::Image {
            file: None,
            image: crate::tests::png(b"unsupported"),
        }
    }

    fn oversized() -> crate::media::Attachment {
        crate::media::Attachment::Image {
            file: None,
            image: crate::tests::png(&vec![0; MAX_IMAGE_BYTES as usize]),
        }
    }

    /// `continue` from a task, on `model`.
    fn retry(
        session: &Arc<SessionHandle>,
        model: &str,
    ) -> tokio::task::JoinHandle<Result<String, HarnessError>> {
        let options = session
            .selection(Some(&model.parse().unwrap()), None)
            .unwrap();
        let session = session.clone();
        tokio::spawn(async move {
            let outcome = session.continue_turn_with(options).await?;
            Ok(outcome.answer.unwrap_or_default())
        })
    }

    /// The failures journaled for the session's turns.
    fn failures(session: &SessionHandle) -> Vec<String> {
        let records = session.runtime.store.records();
        events!(records, SessionEvent::AgentFailed { failure } => failure.to_string())
    }

    /// A batch commits in one transaction: a journal that refuses its last
    /// message keeps every message and the selection out.
    #[tokio::test]
    async fn a_batch_commits_its_selection_and_messages_together() {
        let (_root, tracking, session) = start(false).await;
        let rule = "BEFORE INSERT ON user_part WHEN NEW.text = 'test:last'";
        let _refused = crate::session::tests::refuse(&session.runtime.store, rule);
        let inputs = vec![
            queued(&session, "test:first", vec![], Some("queue-test/second")),
            queued(&session, "test:last", vec![], None),
        ];
        let results = bounded(enqueue_prompts(&session, inputs)).await;
        assert!(results.iter().all(Result::is_err));
        assert!(committed(&session).is_empty());
        assert!(model_changes(&session).is_empty());
        assert_eq!(count(&tracking), 0);
    }

    /// Input that reaches its commit once shutdown has begun is refused, not journaled.
    #[tokio::test]
    async fn input_reaching_its_commit_while_shutting_down_is_refused() {
        let (_root, tracking, session) = start(false).await;
        let turn = spawn_prompt(&session, "test:initial");
        tracking.request(0).await;
        let late = vec![queued(&session, "test:late", vec![], None)];
        let enqueue = enqueue_batch(&session, late);
        buffered(&session, 1).await;
        let store = &session.runtime.store;
        bounded(store.enter(Command::BeginShutdown)).await.unwrap();
        tracking.release(0);
        // The refusal leaves the answer standing; closing drops the batch it holds.
        assert_eq!(bounded(turn).await.unwrap().unwrap(), "answer-0");
        stop(&session).await;
        let results = bounded(enqueue).await.unwrap();
        assert!(matches!(
            results[..],
            [Err(HarnessError::Session(SessionError::Refused(_)))]
        ));
        assert_eq!(texts(&committed(&session)), ["test:initial"]);
    }

    /// Queued input is not lost at a final response boundary: its receipt resolves
    /// claimed without waiting for the continuation it joins, and input whose
    /// receipt was dropped still commits, exactly once.
    #[tokio::test]
    async fn queued_input_joins_a_final_boundary_once_even_with_its_receipt_dropped() {
        let (_root, tracking, session) = start(false).await;
        let turn = spawn_prompt(&session, "test:initial");
        tracking.request(0).await;
        let (dropped, _) = enqueue(&session, "test:dropped", vec![], Some("queue-test/second"));
        buffered(&session, 1).await;
        let (receipt, claim) = enqueue(&session, "test:follow-up", vec![], None);
        buffered(&session, 2).await;
        dropped.abort();
        assert!(dropped.await.unwrap_err().is_cancelled());
        assert!(!receipt.is_finished());
        tracking.release(0);
        let next = tracking.request(1).await;
        bounded(receipt).await.unwrap().unwrap();
        assert!(claim.is_claimed());
        let expected = ["test:initial", "test:dropped", "test:follow-up"];
        assert_eq!(next.model.as_str(), "second-model");
        assert_eq!(texts(next.messages()), expected);
        tracking.release(1);
        bounded(turn).await.unwrap().unwrap();
        stop(&session).await;
        assert_eq!(texts(&committed(&session)), expected);
        assert_eq!(count(&tracking), 2);
    }

    /// Every buffered receipt resolves when an interrupt or shutdown lands, before
    /// its batch is claimed or while the claim commits: exactly the inputs whose
    /// receipts are Ok are committed.
    #[tokio::test]
    async fn interrupt_and_shutdown_resolve_every_receipt_and_commit_exactly_the_ok_ones() {
        for (shutdown, claimed) in [(false, false), (true, false), (false, true), (true, true)] {
            let (_root, tracking, session) = start(false).await;
            let turn = spawn_prompt(&session, "test:initial");
            tracking.request(0).await;
            let inputs = vec![
                queued(&session, "test:one", vec![], Some("queue-test/second")),
                queued(&session, "test:two", vec![], Some("queue-test/third")),
            ];
            let claim = inputs[0].cancellation.clone();
            let enqueue = enqueue_batch(&session, inputs);
            buffered(&session, 1).await;
            if claimed {
                tracking.release(0);
                // The claim precedes the commit's awaits: stop while it is in progress.
                bounded(async {
                    while !claim.is_claimed() {
                        tokio::task::yield_now().await;
                    }
                })
                .await;
            }
            if shutdown {
                stop(&session).await;
            } else {
                session.interrupt().await;
            }
            let results = bounded(enqueue).await.unwrap();
            assert_eq!(results.len(), 2);
            let _ = bounded(turn).await.unwrap();
            stop(&session).await;
            let accepted = results.iter().filter(|result| result.is_ok()).count();
            let found = texts(&committed(&session));
            assert_eq!(found[1..], ["test:one", "test:two"][..accepted]);
            if !claimed {
                assert!(results.iter().all(|result| shutdown && result.is_err()
                    || matches!(result, Err(HarnessError::Interrupted))));
                assert_eq!(count(&tracking), 1);
            }
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

    /// A receipt whose input was dropped unclaimed reports why: the agent stopped
    /// in a session that still takes input, and closed once nothing holds it.
    #[tokio::test]
    async fn a_dropped_receipt_reports_why_its_input_was_not_taken() {
        let (_root, _requests, session) = crate::agent::runtime::tests::scripted_session([]).await;
        let receipt = |store| super::QueuedPromptReceipt {
            committed: tokio::sync::oneshot::channel().1,
            store,
        };
        let running = receipt(session.runtime.store.downgrade());
        assert!(matches!(running.await, Err(HarnessError::AgentStopped)));
        let dropped = receipt(session.runtime.store.downgrade());
        session.shutdown().await.unwrap();
        drop(session);
        let closed = crate::session::SessionPhase::Closed;
        assert!(matches!(dropped.await,
            Err(HarnessError::Session(SessionError::Refused(phase))) if phase == closed));
    }
}
