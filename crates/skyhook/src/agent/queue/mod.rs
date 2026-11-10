//! Request-boundary user input mailbox and cancellation ownership.

use super::selection::Adopting;
use std::sync::atomic::AtomicU8;

use super::{
    AgentCommand, AgentContext, AgentSettings, HarnessError, Selection, SessionHandle,
    SessionRuntime, TurnContext,
};
use crate::identity::AgentId;
use crate::session::{Message, SessionError, UserPart};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::sync::oneshot;

/// Who has a queued input: nobody yet, its submitter (withdrawn), the runtime,
/// or nobody again, held with the failed turn that refused it. The host withdraws
/// input from its UI at once, before or after it reaches the core, so the claim is
/// an atomic its handle shares with the core rather than core state.
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
    pub model: crate::provider::model::ModelRef,
    pub mode: Option<crate::config::mode::ModeName>,
}

/// One submission for the request-boundary mailbox.
#[derive(Debug, Default)]
pub struct QueuedPrompt {
    pub text: String,
    pub attachments: Vec<skyhook_media::Attachment>,
    pub options: Selection,
    /// Keep a clone to withdraw the input before the runtime claims it.
    pub cancellation: QueuedPromptCancellation,
}

pub(super) struct QueuedInput {
    pub content: Vec<UserPart>,
    /// Checked and stored with the batch a request boundary takes it in.
    pub attachments: Vec<skyhook_media::Attachment>,
    pub options: Selection,
    pub cancellation: QueuedPromptCancellation,
    pub committed: oneshot::Sender<Result<QueuedPromptCommitted, HarnessError>>,
}

impl QueuedInput {
    fn images(&self) -> bool {
        self.content.iter().any(UserPart::is_image)
            || (self.attachments.iter())
                .any(|attachment| matches!(attachment, skyhook_media::Attachment::Image { .. }))
    }

    pub(super) fn reject(self, error: HarnessError) {
        let _ = self.cancellation.cancel();
        let _ = self.committed.send(Err(error));
    }
}

/// Queued inputs taken as one submission: claimed while a boundary or
/// `continue` commits them, held while a failed turn keeps them for `continue`.
#[derive(Default)]
pub(super) struct Batch {
    inputs: Vec<QueuedInput>,
}

impl Batch {
    /// Hold `inputs` too, in order; one withdrawn meanwhile is handed back.
    pub(super) fn join(&mut self, inputs: impl IntoIterator<Item = QueuedInput>) {
        for input in inputs {
            match input.cancellation.hold() {
                true => self.inputs.push(input),
                false => input.reject(HarnessError::Interrupted),
            }
        }
    }

    /// Whether anything it holds was not withdrawn.
    pub(super) fn retains(&self) -> bool {
        (self.inputs.iter()).any(|input| !input.cancellation.is_withdrawn())
    }

    /// The batch held for `continue`, unless nothing in it is left.
    pub(super) fn held(self) -> Option<Self> {
        let mut held = Self::default();
        held.join(self.inputs);
        (!held.inputs.is_empty()).then_some(held)
    }

    /// The batch claimed, unless everything in it was withdrawn.
    pub(super) fn claimed(self) -> Option<Claimed> {
        Claimed::claim(self.inputs)
    }

    /// Whether anything held is left, once what was withdrawn since is dropped.
    pub(super) fn holds(&mut self) -> bool {
        for input in self
            .inputs
            .extract_if(.., |input| input.cancellation.is_withdrawn())
        {
            input.reject(HarnessError::Interrupted);
        }
        !self.inputs.is_empty()
    }

    /// Give every input back to its submitter.
    pub(super) fn hand_back(self) {
        (self.inputs.into_iter()).for_each(|input| input.reject(HarnessError::Interrupted));
    }
}

/// Queued inputs the runtime claimed as one submission: none can be withdrawn.
pub(super) struct Claimed(Vec<QueuedInput>);

impl Claimed {
    /// `inputs` claimed in order, handing back each withdrawn meanwhile, unless
    /// none is left.
    fn claim(inputs: impl IntoIterator<Item = QueuedInput>) -> Option<Self> {
        let mut claimed = Vec::new();
        for input in inputs {
            match input.cancellation.try_claim() {
                true => claimed.push(input),
                false => input.reject(HarnessError::Interrupted),
            }
        }
        (!claimed.is_empty()).then_some(Self(claimed))
    }

    /// Check every input as the session takes a message, then resolve what the
    /// batch commits under: what `over` names, else the last model and mode
    /// any input names. A refused batch is the refusal's, as it was claimed.
    pub(super) fn admit(
        self,
        runtime: &SessionRuntime,
        over: Selection,
    ) -> Result<(Admitted, Selection), Refusal> {
        let Self(inputs) = self;
        let admits =
            |input: &QueuedInput| runtime.admit_message(&input.attachments, &input.options);
        if let Some(error) = inputs.iter().find_map(|input| admits(input).err()) {
            let batch = Some(Box::new(Batch { inputs }));
            return Err(Refusal { error, batch });
        }
        let named = || inputs.iter().rev().map(|input| &input.options);
        let selection = Selection {
            model: (over.model).or_else(|| named().find_map(|options| options.model.clone())),
            mode: (over.mode).or_else(|| named().find_map(|options| options.mode.clone())),
        };
        Ok((Admitted(inputs), selection))
    }
}

/// A claimed batch the session admitted, for the turn that takes it to prepare
/// and commit, or to hold again as it was.
pub(super) struct Admitted(Vec<QueuedInput>);

impl Admitted {
    /// Whether any of its inputs carries an image.
    pub(super) fn images(&self) -> bool {
        self.0.iter().any(QueuedInput::images)
    }

    /// Its messages, once their attachments are stored.
    pub(super) async fn messages(
        &self,
        runtime: &SessionRuntime,
    ) -> Result<Vec<Message>, HarnessError> {
        let mut messages = Vec::with_capacity(self.0.len());
        for input in &self.0 {
            let content = input.content.clone();
            let content = runtime.with_attachments(content, &input.attachments);
            messages.push(Message::User(content.await?));
        }
        Ok(messages)
    }

    /// Tell every submitter its message committed under the agent's selection.
    pub(super) fn committed(self, context: &AgentContext, settings: &AgentSettings) {
        let committed = QueuedPromptCommitted {
            model: context.model.profile.name.clone(),
            mode: (settings.mode.as_ref()).map(|mode| mode.name.clone()),
        };
        for input in self.0 {
            let _ = input.committed.send(Ok(committed.clone()));
        }
    }

    /// The batch held again, with `behind` queued since, unless nothing in it
    /// is left.
    pub(super) fn release(self, behind: Vec<QueuedInput>) -> Option<Batch> {
        let mut batch = Batch { inputs: self.0 };
        batch.join(behind);
        batch.held()
    }

    /// The batch `error` refused, for whoever holds it.
    pub(super) fn refused(self, error: HarnessError) -> Refusal {
        let batch = Some(Box::new(Batch { inputs: self.0 }));
        Refusal { error, batch }
    }

    /// Give every input back to its submitter.
    pub(super) fn hand_back(self) {
        Batch { inputs: self.0 }.hand_back();
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
        // A session that cannot run takes nothing, nor one whose root's model is
        // gone unless a prompt names another: the first prompt says why.
        let admitted = async {
            self.resume_work().await?;
            self.running().await?;
            let root = self.runtime.root_model();
            let named = batch.iter().any(|input| input.options.model.is_some());
            match named || self.runtime.harness.models.contains_key(&root) {
                true => Ok(()),
                false => Err(HarnessError::UnknownModel(root)),
            }
        };
        if let Err(error) = admitted.await {
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
    /// Consume, as one submission before a provider request, `taken`, then
    /// every input queued for the agent now: claim every member its submitter
    /// has not withdrawn, admit them, select what they name, then store their
    /// attachments and commit their messages with that selection in one
    /// transaction. None when it claims nothing, or an interrupt stopped the
    /// turn first, which hands the batch back. A batch the session refuses is
    /// the refusal's, for the failure of the turn to hold.
    pub(super) async fn consume_queued(
        &self,
        turn: &mut TurnContext<'_>,
        context: &mut AgentContext,
        taken: Vec<QueuedInput>,
    ) -> Option<Result<(), Refusal>> {
        let agent = turn.agent.clone();
        let queued = (self.store)
            .live(move |state| state.agents.take_queued(&agent))
            .await;
        let claimed = Claimed::claim(taken.into_iter().chain(queued))?;
        let (admitted, options) = match claimed.admit(self, Selection::default()) {
            Ok(admitted) => admitted,
            Err(refusal) => return Some(Err(refusal)),
        };
        let images = admitted.images();
        let adopted = async {
            let selected = self.select(turn, context, options, images);
            let selected = selected.await?;
            let adopting = Adopting::Joins(admitted.messages(self).await?);
            self.adopt(turn, context, selected, adopting).await
        };
        match adopted.await {
            Ok(()) => {
                admitted.committed(context, turn.settings);
                Some(Ok(()))
            }
            // The interrupt already handed back what was queued: what waits since
            // is new input.
            Err(HarnessError::Interrupted) => {
                admitted.hand_back();
                None
            }
            Err(error) => Some(Err(admitted.refused(error))),
        }
    }

    /// Consume the input queued now at a busy turn's request boundary; whether
    /// any committed, or why the session refused it, which fails the turn.
    pub(super) async fn consume_queued_inputs(
        &self,
        turn: &mut TurnContext<'_>,
        context: &mut AgentContext,
    ) -> Result<bool, Refusal> {
        let consumed = self.consume_queued(turn, context, Vec::new());
        consumed
            .await
            .transpose()
            .map(|consumed| consumed.is_some())
    }
}

#[cfg(test)]
pub(super) mod tests {
    use std::{assert_matches, sync::Arc};

    use super::super::*;
    pub(super) use crate::agent::tests::{
        AssistantItem, Script, Step, bounded, enqueue_prompts, ephemeral_session, events, found,
        models, provider_name, response, spawn_prompt, turn,
    };
    pub(super) use crate::agent::tests::{Sent, SentPart};

    pub(in crate::agent) fn count(script: &Script) -> usize {
        script.requests.lock().unwrap().len()
    }

    /// A session on provider "queue-test" with models "first" (default), "second",
    /// "third" and the text-only "blind".
    pub(in crate::agent) async fn start(
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
        let harness = HarnessBuilder::new(root.path(), "test".parse().unwrap())
            .session_root(root.path().join("sessions"))
            .coalescing(inbox::Coalescing::NONE)
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
    pub(in crate::agent) fn enqueue(
        session: &Arc<SessionHandle>,
        text: &'static str,
        attachments: Vec<skyhook_media::Attachment>,
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
    pub(in crate::agent) fn todo_finished(message: &Sent) -> bool {
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
    pub(in crate::agent) async fn buffered(session: &SessionHandle, count: usize) {
        let (key, root) = (Key::Agent(session.root.clone()), session.root.clone());
        found(session, [key], move |state| {
            (state.agents.get(&root)?.inbox.len() == count).then_some(())
        })
        .await;
    }

    /// The fixture's own prompts are sent as user text prefixed `test:`.
    pub(in crate::agent) fn texts<'a>(messages: impl IntoIterator<Item = &'a Sent>) -> Vec<String> {
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
    pub(in crate::agent) fn committed(session: &SessionHandle) -> Vec<Sent> {
        let records = session.runtime.store.records();
        events!(records, SessionEvent::MessageCommitted { message } => crate::agent::tests::sent(message))
    }

    pub(in crate::agent) fn model_changes(session: &SessionHandle) -> Vec<String> {
        let records = session.runtime.store.records();
        events!(records, SessionEvent::ModelChanged { profile } => profile.name.to_string())
    }

    pub(in crate::agent) async fn stop(session: &SessionHandle) {
        bounded(session.shutdown()).await.unwrap();
    }

    /// Parent input reaches the model as user text; the fixture's own prompts are
    /// the `test:` ones, so every other user text is parent input.
    pub(in crate::agent) fn parent_inputs<'a>(
        messages: impl IntoIterator<Item = &'a Sent>,
    ) -> Vec<String> {
        user_texts(messages)
            .into_iter()
            .filter(|text| !text.starts_with("test:"))
            .collect()
    }

    fn queued(
        session: &SessionHandle,
        text: &str,
        attachments: Vec<skyhook_media::Attachment>,
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
    async fn an_idle_batch_starts_one_turn_with_every_input() {
        let (_root, tracking, session) = start(false).await;
        let inputs = vec![
            queued(&session, "test:first", vec![], None),
            queued(&session, "test:last", vec![], None),
        ];
        let tokens = inputs.iter().map(|input| input.cancellation.clone());
        let tokens = tokens.collect::<Vec<_>>();
        let enqueue = enqueue_batch(&session, inputs);
        let request = tracking.request(0).await;
        assert!(bounded(enqueue).await.unwrap().iter().all(Result::is_ok));
        assert!(tokens.iter().all(QueuedPromptCancellation::is_claimed));
        assert_eq!(texts(request.messages()), ["test:first", "test:last"]);
        tracking.release(0);
        stop(&session).await;
        assert_eq!(count(&tracking), 1);
    }

    /// An input withdrawn once queued is skipped unchecked: what it carries,
    /// text or a selection, cannot refuse its batch or commit.
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
                    crate::tests::over_limit_images().await,
                    Some("queue-test/third"),
                )
            },
            queued(&session, "test:two", vec![], None),
        ];
        let enqueue = enqueue_batch(&session, inputs);
        buffered(&session, 1).await;
        assert!(withdrawn.cancel());
        assert!(withdrawn.cancel());
        tracking.release(0);
        let next = tracking.request(1).await;
        let results = bounded(enqueue).await.unwrap();
        assert_matches!(
            results.as_slice(),
            [Ok(_), Err(HarnessError::Interrupted), Ok(_)]
        );
        assert!(!withdrawn.is_claimed());
        assert_eq!(next.model.as_str(), "second-model");
        let sent = ["test:initial", "test:one", "test:two"];
        assert_eq!(texts(next.messages()), sent);
        assert_eq!(model_changes(&session), ["queue-test/second"]);
        tracking.release(1);
        bounded(turn).await.unwrap().unwrap();
        stop(&session).await;
        assert_eq!(texts(&committed(&session)), sent);
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
        assert_matches!(results[..], [Err(HarnessError::Interrupted)]);
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
            vec![unsupported().await],
            Some("queue-test/blind"),
        )];
        let second = enqueue_batch(&session, blind);
        buffered(&session, 2).await;
        tracking.release(0);
        let ended = bounded(turn).await.unwrap();
        assert_matches!(ended, Err(HarnessError::ImagesUnsupported(_)));
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
        let refused = vec![queued(
            &session,
            "test:refused",
            vec![unsupported().await],
            blind,
        )];
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

    /// Until the root's turn is parked, or not.
    async fn rests(session: &SessionHandle, parked: bool) {
        bounded(async {
            while matches!(turn(session, &session.root), Turn::Parked(_)) != parked {
                tokio::task::yield_now().await;
            }
        })
        .await
    }

    /// The [`start`] session failed by the image a batch it holds brings on the
    /// text-only "blind", which a text input joined.
    struct HeldOnBlind {
        _root: tempfile::TempDir,
        tracking: Arc<Script>,
        session: Arc<SessionHandle>,
        held: [QueuedPromptCancellation; 2],
        receipts: [tokio::task::JoinHandle<Vec<Result<QueuedPromptCommitted, HarnessError>>>; 2],
    }

    async fn held_on_blind() -> HeldOnBlind {
        let (_root, tracking, session) = start(false).await;
        let ([first], refused) = held_image(&session, "test:refused").await;
        let joined = queued(&session, "test:joined", vec![], None);
        let held = [first, joined.cancellation.clone()];
        let joined = enqueue_batch(&session, vec![joined]);
        until_held(&session, &held).await;
        HeldOnBlind {
            _root,
            tracking,
            session,
            held,
            receipts: [refused, joined],
        }
    }

    /// A batch an idle root refuses fails its turn, which holds it, with input
    /// queued since joining it. A `continue` the batch refuses, bare or
    /// selecting a model, is refused, journaling nothing and leaving it held as
    /// it was queued: an input withdrawn from it leaves the batch no selection.
    #[tokio::test]
    async fn a_refused_idle_batch_fails_its_turn_and_is_held_through_refused_continues() {
        let HeldOnBlind {
            tracking,
            session,
            held,
            receipts,
            ..
        } = held_on_blind().await;
        let changes = model_changes(&session);
        let bare = bounded(session.continue_turn()).await;
        assert_matches!(bare, Err(HarnessError::ImagesUnsupported(_)));
        let again = bounded(retry(&session, "queue-test/blind")).await.unwrap();
        assert_matches!(again, Err(HarnessError::ImagesUnsupported(_)));
        until_held(&session, &held).await;
        assert!(changes.is_empty() && model_changes(&session).is_empty());
        assert_eq!(count(&tracking), 0);
        assert!(held[0].cancel());
        let continued = spawn_continue(&session);
        let request = tracking.request(0).await;
        assert_eq!(request.model.as_str(), "first-model");
        assert_eq!(texts(request.messages()), ["test:joined"]);
        tracking.release(0);
        assert_eq!(bounded(continued).await.unwrap().unwrap(), "answer-0");
        assert!(model_changes(&session).is_empty());
        let [refused, joined] = receipts;
        let refused = bounded(refused).await.unwrap();
        assert_matches!(refused[..], [Err(HarnessError::Interrupted)]);
        assert!(bounded(joined).await.unwrap().iter().all(Result::is_ok));
        stop(&session).await;
        let failures = failures(&session);
        assert!(
            matches!(&failures[..], [failure] if failure.contains("does not support image inputs")),
            "{failures:?}"
        );
    }

    /// A `continue` on a model that takes the held batch claims it before it
    /// journals anything, so once its selection is journaled nothing in the batch
    /// can be withdrawn, and commits it all on that model.
    #[tokio::test]
    async fn a_continue_on_a_model_that_takes_the_held_batch_claims_and_commits_it() {
        let HeldOnBlind {
            tracking,
            session,
            held,
            receipts,
            ..
        } = held_on_blind().await;
        let continued = retry(&session, "queue-test/second");
        // Withdrawing what `continue` took finds it already being submitted.
        let selected = |records: &[Arc<EventRecord>]| {
            let changed = |record: &Arc<EventRecord>| {
                matches!(record.event, SessionEvent::ModelChanged { .. })
            };
            records.iter().any(changed).then_some(())
        };
        let selected =
            crate::session::tests::on_records(&session.runtime.store, &session.root, selected);
        bounded(session.runtime.store.until(selected))
            .await
            .unwrap();
        assert!(held.iter().all(|held| !held.cancel()));
        let next = tracking.request(0).await;
        assert_eq!(next.model.as_str(), "second-model");
        assert_eq!(texts(next.messages()), ["test:refused", "test:joined"]);
        for results in receipts {
            assert!(bounded(results).await.unwrap().iter().all(Result::is_ok));
        }
        tracking.release(0);
        assert_eq!(bounded(continued).await.unwrap().unwrap(), "answer-0");
        rests(&session, false).await;
        stop(&session).await;
    }

    /// A `continue` with nothing to resume changes nothing, though it selects a
    /// model.
    #[tokio::test]
    async fn a_continue_with_nothing_to_resume_changes_nothing() {
        let (_root, tracking, session) = start(false).await;
        let prompted = spawn_prompt(&session, "test:answered");
        tracking.request(0).await;
        tracking.release(0);
        assert_eq!(bounded(prompted).await.unwrap().unwrap(), "answer-0");
        let records = session.runtime.store.records().len();
        let third = session.selection(Some(&"queue-test/third".parse().unwrap()), None);
        let idle = bounded(session.continue_turn_with(third.unwrap())).await;
        assert!(idle.unwrap().is_empty());
        assert!(model_changes(&session).is_empty());
        assert_eq!(session.runtime.store.records().len(), records);
        stop(&session).await;
    }

    /// Once a held batch is withdrawn, the root rests at once, and new input
    /// runs on its own.
    #[tokio::test]
    async fn a_withdrawn_held_batch_rests_its_root_for_new_input_alone() {
        let (_root, tracking, session) = start(false).await;
        let empty = skyhook_media::Attachment::Image {
            file: None,
            image: crate::tests::png(b"").await,
        };
        let many = vec![empty; crate::agent::ImageBudget::MAX_IMAGES + 1];
        let many = queued(&session, "test:many-images", many, Some("queue-test/third"));
        let cancellation = many.cancellation.clone();
        let many = enqueue_batch(&session, vec![many]);
        until_held(&session, std::slice::from_ref(&cancellation)).await;
        let records = session.runtime.store.records().len();
        let bare = bounded(session.continue_turn()).await;
        assert_matches!(bare, Err(HarnessError::ImageLimit));
        assert!(model_changes(&session).is_empty());
        assert_eq!(session.runtime.store.records().len(), records);
        assert!(cancellation.cancel());
        rests(&session, false).await;
        assert!(bounded(many).await.unwrap().iter().all(Result::is_err));
        let next = enqueue_batch(&session, vec![queued(&session, "test:next", vec![], None)]);
        let request = tracking.request(0).await;
        assert_eq!(texts(request.messages()), ["test:next"]);
        tracking.release(0);
        assert!(bounded(next).await.unwrap().iter().all(Result::is_ok));
        stop(&session).await;
        assert_eq!(failures(&session), [HarnessError::ImageLimit.to_string()]);
        assert_eq!(count(&tracking), 1);
    }

    /// A `continue` is a turn from the moment the root takes it: a second
    /// `continue` while it prepares leaves the root alone, and once that
    /// preparation fails the batch is held again with nothing journaled.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_continue_while_another_prepares_leaves_its_batch_held() {
        let (_root, tracking, session, gate) = gated(false).await;
        let (held, batch) = held_image(&session, "test:image").await;
        let records = session.runtime.store.records().len();
        let preparing = retry(&session, "gate/gated");
        gate.opening().await;
        assert_eq!(bounded(session.continue_turn()).await.unwrap(), "");
        assert_eq!(turn(&session, &session.root), Turn::Busy);
        gate.open(false);
        let refused = bounded(preparing).await.unwrap();
        assert!(
            matches!(refused, Err(HarnessError::Provider(_))),
            "{refused:?}"
        );
        until_held(&session, &held).await;
        assert_eq!(session.runtime.store.records().len(), records);
        let continued = retry(&session, "queue-test/second");
        assert_eq!(tracking.request(0).await.model.as_str(), "second-model");
        tracking.release(0);
        assert_eq!(bounded(continued).await.unwrap().unwrap(), "answer-0");
        assert!(bounded(batch).await.unwrap().iter().all(Result::is_ok));
        stop(&session).await;
    }

    /// An interrupt that reaches a `continue` before the root takes it stops it:
    /// nothing is journaled or requested, and the batch waits for the next
    /// `continue`. One that lands while the `continue` prepares is
    /// `a_continue_waits_for_a_stopped_continue_to_unwind`'s.
    #[tokio::test]
    async fn an_interrupt_before_the_root_takes_a_continue_stops_it() {
        let (_root, tracking, session) = start(false).await;
        let (held, batch) = held_image(&session, "test:image").await;
        let records = session.runtime.store.records().len();
        let (runtime, root) = (session.runtime.clone(), session.root.clone());
        let (done, refused) = oneshot::channel();
        let interrupt = move |state: &mut CoreState| {
            let facts = state.agents.get_mut(&root).unwrap();
            let claimed = facts.held.take().and_then(super::Batch::claimed).unwrap();
            let Ok((batch, selection)) = claimed.admit(&runtime, Selection::default()) else {
                panic!("the batch is admitted");
            };
            let continuing = AgentCommand::Continue {
                selection,
                batch: Some(batch),
                resting: Box::new(facts.set_turn(Turn::Busy)),
                done: RequestCompletion::Root(done),
            };
            state.agents.send(&root, continuing);
            runtime.stop_turns(state, &root)
        };
        assert_eq!(bounded(session.runtime.store.live(interrupt)).await, 1);
        let refused = bounded(refused).await.unwrap();
        assert_matches!(refused, Err(HarnessError::Interrupted));
        until_held(&session, &held).await;
        assert_eq!(session.runtime.store.records().len(), records);
        let continued = retry(&session, "queue-test/second");
        let request = tracking.request(0).await;
        assert_eq!(request.model.as_str(), "second-model");
        assert_eq!(texts(request.messages()), ["test:image"]);
        tracking.release(0);
        assert_eq!(bounded(continued).await.unwrap().unwrap(), "answer-0");
        assert!(bounded(batch).await.unwrap().iter().all(Result::is_ok));
        stop(&session).await;
    }

    /// A `continue` behind one an interrupt stopped while it prepared waits for
    /// that one to unwind, however its preparation ends: the stopped one
    /// commits nothing and holds its batch again, which the next commits under
    /// its own selection.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_continue_waits_for_a_stopped_continue_to_unwind() {
        for opens in [true, false] {
            continue_behind_a_stopped_continue(opens).await;
        }
    }

    async fn continue_behind_a_stopped_continue(opens: bool) {
        let (_root, tracking, session, gate) = gated(true).await;
        let initial = spawn_prompt(&session, "test:initial");
        tracking.request(0).await;
        let blind = Some("queue-test/blind");
        let image = queued(&session, "test:image", vec![unsupported().await], blind);
        let held = [image.cancellation.clone()];
        let batch = enqueue_batch(&session, vec![image]);
        buffered(&session, 1).await;
        tracking.release(0);
        let failed = bounded(initial).await.unwrap();
        assert!(
            matches!(failed, Err(HarnessError::ImagesUnsupported(_))),
            "{failed:?}"
        );
        until_held(&session, &held).await;
        let records = session.runtime.store.records().len();

        let stopped = retry(&session, "gate/gated");
        gate.opening().await;
        assert_eq!(bounded(session.interrupt()).await, 1);
        let root = Key::Agent(session.root.clone());
        let reading = |root| {
            session
                .runtime
                .store
                .read(|state| state.waits_reading(root))
        };
        let waits = reading(&root);
        let next = retry(&session, "queue-test/second");
        // Until the stopped one unwinds, the next neither claims the root nor
        // journals anything.
        bounded(async {
            while reading(&root) == waits && turn(&session, &session.root) == Turn::Stopping {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert_eq!(turn(&session, &session.root), Turn::Stopping);
        assert_eq!(session.runtime.store.records().len(), records);
        gate.open(opens);

        let stopped = bounded(stopped).await.unwrap();
        assert!(stopped.is_err(), "{stopped:?}");
        let request = tracking.request(1).await;
        assert_eq!(request.model.as_str(), "second-model");
        assert_eq!(texts(request.messages()), ["test:initial", "test:image"]);
        tracking.release(1);
        assert_eq!(bounded(next).await.unwrap().unwrap(), "answer-1");
        let receipts = bounded(batch).await.unwrap();
        let second = |receipt: &Result<QueuedPromptCommitted, _>| matches!(receipt, Ok(committed) if committed.model.to_string() == "queue-test/second");
        assert!(receipts.iter().all(second), "{receipts:?}");
        assert_eq!(model_changes(&session), ["queue-test/second"]);
        stop(&session).await;
    }

    /// Queue `text` with an image the text-only "blind" refuses, and wait until
    /// the root's failed turn holds it: its cancellation handle and receipts.
    async fn held_image(session: &Arc<SessionHandle>, text: &'static str) -> HeldImage {
        let blind = Some("queue-test/blind");
        let image = queued(session, text, vec![unsupported().await], blind);
        let held = [image.cancellation.clone()];
        let batch = enqueue_batch(session, vec![image]);
        until_held(session, &held).await;
        (held, batch)
    }

    type HeldImage = (
        [QueuedPromptCancellation; 1],
        tokio::task::JoinHandle<Vec<Result<QueuedPromptCommitted, HarnessError>>>,
    );

    /// Once the root's failed turn holds `held`, every input still retained.
    async fn until_held(session: &SessionHandle, held: &[QueuedPromptCancellation]) {
        bounded(async {
            while !held.iter().all(QueuedPromptCancellation::is_retained)
                || !matches!(turn(session, &session.root), Turn::Parked(_))
            {
                tokio::task::yield_now().await;
            }
        })
        .await;
    }

    /// A bare `continue` from a task.
    fn spawn_continue(
        session: &Arc<SessionHandle>,
    ) -> tokio::task::JoinHandle<Result<String, HarnessError>> {
        let session = session.clone();
        tokio::spawn(async move { session.continue_turn().await })
    }

    /// Opens a provider context only once the test lets it, holding a
    /// `continue` that selects its model while it prepares.
    pub(super) struct Gate {
        script: Arc<Script>,
        opening: tokio::sync::Semaphore,
        opens: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<bool>>,
        open: tokio::sync::mpsc::UnboundedSender<bool>,
    }

    impl Gate {
        /// Once a context has begun to open.
        async fn opening(&self) {
            bounded(self.opening.acquire()).await.unwrap().forget();
        }

        /// Let the opening context through, or fail it.
        fn open(&self, opens: bool) {
            self.open.send(opens).unwrap();
        }
    }

    impl crate::provider::Provider for Gate {
        fn open_context(
            &self,
            context: crate::provider::protocol::ContextId,
        ) -> Result<Box<dyn crate::provider::ProviderContext>, crate::provider::ProviderError>
        {
            self.opening.add_permits(1);
            // Opening a context is synchronous: the worker hands its other tasks
            // on while it waits for the test.
            let opens = async { self.opens.lock().await.recv().await };
            let opens = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(bounded(opens))
            });
            match opens {
                Some(true) => self.script.open_context(context),
                _ => Err(crate::provider::ProviderErrorKind::Protocol.error("not opened")),
            }
        }
    }

    /// The [`start`] session, whose provider "gate" serves model "gated" through
    /// a [`Gate`], with only the first two responses, both gated.
    async fn gated(
        first_calls_tool: bool,
    ) -> (
        tempfile::TempDir,
        Arc<Script>,
        Arc<SessionHandle>,
        Arc<Gate>,
    ) {
        let root = tempfile::tempdir().unwrap();
        let answers = (0..2).map(|index| {
            let item = if index == 0 && first_calls_tool {
                let todo = ToolCall::new("queue-todo", "todo", json!({"items": []})).unwrap();
                AssistantItem::tool_call("queue-todo", 0, todo)
            } else {
                AssistantItem::text("text/0", 0, format!("answer-{index}"))
            };
            Step::new(response(vec![item])).gated()
        });
        let tracking = Script::new(answers, &Default::default());
        let (open, opens) = tokio::sync::mpsc::unbounded_channel();
        let gate = Arc::new(Gate {
            script: tracking.clone(),
            opening: tokio::sync::Semaphore::new(0),
            opens: tokio::sync::Mutex::new(opens),
            open,
        });
        let profile = |model: &str| crate::tests::profile(model, true);
        let blind = ModelProfile {
            supports_images: false,
            ..profile("blind-model")
        };
        let harness = HarnessBuilder::new(root.path(), "test".parse().unwrap())
            .session_root(root.path().join("sessions"))
            .coalescing(inbox::Coalescing::NONE)
            .provider(
                provider_name("queue-test"),
                tracking.clone(),
                models([
                    ("first", profile("first-model")),
                    ("second", profile("second-model")),
                    ("blind", blind),
                ]),
            )
            .provider(
                provider_name("gate"),
                gate.clone(),
                models([("gated", profile("gated-model"))]),
            )
            .default_model("queue-test/first".parse().unwrap())
            .build()
            .await
            .unwrap();
        let session = Arc::new(ephemeral_session(&harness).await);
        (root, tracking, session, gate)
    }

    /// A direct prompt breaks the hold of a refused batch: the batch is handed
    /// back, and the prompt runs on its own selection.
    #[tokio::test]
    async fn a_prompt_breaks_the_hold_of_a_refused_batch() {
        let (_root, tracking, session) = start(false).await;
        let (_held, refused) = held_image(&session, "test:refused").await;
        let prompt = spawn_prompt(&session, "test:direct");
        let request = tracking.request(0).await;
        assert_eq!(texts(request.messages()), ["test:direct"]);
        assert_eq!(request.model.as_str(), "first-model");
        let refused = bounded(refused).await.unwrap();
        assert_matches!(refused[..], [Err(HarnessError::Interrupted)]);
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
        let refused = queued(&session, "test:refused", vec![unsupported().await], blind);
        let held = refused.cancellation.clone();
        let refused = enqueue_batch(&session, vec![refused]);
        buffered(&session, 1).await;
        tracking.release(0);
        assert!(bounded(turn_task).await.unwrap().is_err());
        assert!(held.is_retained() && held.cancel());
        let refused = bounded(refused).await.unwrap();
        assert_matches!(refused[..], [Err(HarnessError::Interrupted)]);
        let continued = retry(&session, "queue-test/first");
        let request = tracking.request(1).await;
        assert_eq!(texts(request.messages()), ["test:initial"]);
        assert!(request.messages().any(todo_finished));
        tracking.release(1);
        assert_eq!(bounded(continued).await.unwrap().unwrap(), "answer-1");
        stop(&session).await;
        assert_eq!(count(&tracking), 2);
    }

    async fn unsupported() -> skyhook_media::Attachment {
        skyhook_media::Attachment::Image {
            file: None,
            image: crate::tests::png(b"unsupported").await,
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
        assert_matches!(
            results[..],
            [Err(HarnessError::Session(SessionError::Refused(_)))]
        );
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
        let _ = dropped.await;
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
        let (_root, _requests, session) = crate::agent::tests::scripted_session([]).await;
        let receipt = |store| super::QueuedPromptReceipt {
            committed: tokio::sync::oneshot::channel().1,
            store,
        };
        let running = receipt(session.runtime.store.downgrade());
        assert_matches!(running.await, Err(HarnessError::AgentStopped));
        let dropped = receipt(session.runtime.store.downgrade());
        session.shutdown().await.unwrap();
        drop(session);
        let closed = crate::session::SessionPhase::Closed;
        assert_matches!(dropped.await,
            Err(HarnessError::Session(SessionError::Refused(phase))) if phase == closed);
    }
}
