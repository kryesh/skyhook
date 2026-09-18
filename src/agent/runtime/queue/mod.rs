//! Request-boundary user input mailbox and cancellation ownership.

use std::sync::{MutexGuard, PoisonError, Weak};

use super::*;
use crate::{identity::QueueAttemptId, session::AppendIdentity};

mod durable;
mod input;

/// Durable random submission identity, preserved across process restarts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct QueuedPromptIdentity(pub(crate) QueueAttemptId);

impl QueuedPromptIdentity {
    #[must_use]
    pub fn attempt(&self) -> QueueAttemptId {
        self.0
    }
}

/// Why a queue operation refused an attempt; the journal is left unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum QueueConflict {
    #[error("unknown queued submission")]
    Unknown,
    #[error("only committed submissions may be acknowledged")]
    Uncommitted,
    #[error("a committed submission cannot be abandoned")]
    Committed,
    #[error("queued submission is held by a live runtime operation")]
    Held,
    #[error("queued permit is not reserved by this runtime; recover first")]
    NotReserved,
}

/// Shared by every handle for a runtime. The gate serializes durable preparation,
/// recovery reservations, reclaims, dispatch and retirement. Every attempt this
/// runtime issued (prepared or recovered) stays listed until it is retired, and
/// is live exactly while its current token exists.
#[derive(Default)]
pub(in crate::agent::runtime) struct QueueRuntimeState {
    pub(super) gate: Mutex<()>,
    entries: std::sync::Mutex<std::collections::HashMap<QueuedPromptIdentity, Weak<Authority>>>,
}

impl QueueRuntimeState {
    fn entries(
        &self,
    ) -> MutexGuard<'_, std::collections::HashMap<QueuedPromptIdentity, Weak<Authority>>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn register(&self, token: &QueuedPromptToken) {
        self.entries()
            .insert(token.identity(), Arc::downgrade(&token.0));
    }

    /// Whether this runtime issued the attempt and has not yet retired it.
    fn issued(&self, identity: QueuedPromptIdentity) -> bool {
        self.entries().contains_key(&identity)
    }

    /// Forget a retired attempt; a later reopen owns any journal row it left.
    fn retire(&self, identity: QueuedPromptIdentity) {
        self.entries().remove(&identity);
    }

    /// The live token holding this attempt's authority in this runtime.
    fn holder(&self, identity: QueuedPromptIdentity) -> Option<Arc<Authority>> {
        self.entries().get(&identity).and_then(Weak::upgrade)
    }

    /// Whether `state` is the attempt this runtime registered and still holds.
    fn holds(&self, state: &Arc<SubmissionState>) -> bool {
        self.holder(state.identity)
            .is_some_and(|holder| Arc::ptr_eq(&holder.0, state))
    }
}

/// Immutable draft and affine authority to dispatch it. Dropping a durably
/// prepared permit leaves its journal intent recoverable; it does not discard the draft.
#[derive(Debug)]
pub struct PreparedQueuedPrompt {
    pub(super) content: Vec<UserContent>,
    pub(super) model: Option<String>,
    pub(super) token: QueuedPromptToken,
}

impl PreparedQueuedPrompt {
    #[must_use]
    pub fn identity(&self) -> QueuedPromptIdentity {
        self.token.identity()
    }
    #[must_use]
    pub fn content(&self) -> &[UserContent] {
        &self.content
    }
    #[must_use]
    pub fn cancellation_handle(&self) -> QueuedPromptCancellation {
        self.token.cancellation_handle()
    }
}

#[derive(Debug)]
pub struct RecoveredQueuedPrompt {
    pub submission: QueuedPromptIdentity,
    pub content: Vec<UserContent>,
    /// The draft's attachments loaded from the blob store; empty once committed.
    pub attachments: Vec<crate::media::Attachment>,
    pub model: Option<String>,
    pub state: RecoveredQueuedPromptState,
}

#[derive(Debug)]
pub enum RecoveredQueuedPromptState {
    Committed(QueuedPromptCommit),
    /// A draft saved by a previous process: this exclusive permit reserves it.
    Retry(PreparedQueuedPrompt),
    /// Definitely uncommitted and issued by this runtime, whose permit was
    /// dropped. No permit is minted by a scan: the holder of the attempt's
    /// cancellation handle regains one with `reclaim_queued_prompt`.
    Released,
    /// A live permit, queued input or claim in this runtime still holds it.
    Unresolved,
}

/// A submission's single dispatch attempt: claim and cancellation are exclusive.
#[derive(Debug, Default)]
enum Phase {
    #[default]
    Pending,
    Cancelled,
    /// Appends accepted for the attempt, and which of them is the user message.
    Claimed {
        appends: Vec<AppendIdentity>,
        message: Option<AppendIdentity>,
    },
}

#[derive(Debug)]
struct SubmissionState {
    identity: QueuedPromptIdentity,
    phase: std::sync::Mutex<Phase>,
}

impl SubmissionState {
    fn phase(&self) -> MutexGuard<'_, Phase> {
        self.phase.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Dispatch authority for one attempt, alive exactly as long as its token.
#[derive(Debug)]
struct Authority(Arc<SubmissionState>);

/// Single-use enqueue permit. Cancellation observers cannot enqueue its identity.
///
/// ```compile_fail
/// use skyhook::agent::QueuedPromptToken;
/// let permit = QueuedPromptToken::new().unwrap();
/// let duplicate = permit.clone();
/// ```
#[derive(Debug)]
pub struct QueuedPromptToken(Arc<Authority>);

/// Clonable cancellation observer, never an enqueue permit.
///
/// ```compile_fail
/// use skyhook::agent::QueuedPromptToken;
/// let permit = QueuedPromptToken::new().unwrap();
/// let not_a_permit: QueuedPromptToken = permit.cancellation_handle();
/// ```
#[derive(Clone, Debug)]
pub struct QueuedPromptCancellation(Arc<SubmissionState>);

impl QueuedPromptToken {
    pub fn new() -> Result<Self, HarnessError> {
        let identity = QueueAttemptId::generate().map_err(|error| {
            HarnessError::Initialization(format!("queue identity randomness unavailable: {error}"))
        })?;
        Ok(Self::with_identity(QueuedPromptIdentity(identity)))
    }

    fn with_identity(identity: QueuedPromptIdentity) -> Self {
        Self(Arc::new(Authority(Arc::new(SubmissionState {
            identity,
            phase: std::sync::Mutex::default(),
        }))))
    }

    /// Re-arm an attempt whose previous token is gone. An unclaimed attempt keeps
    /// its submission state, so the caller's existing cancellation handle also
    /// cancels the new permit; a once-claimed attempt starts from fresh state.
    fn rearm(attempt: &QueuedPromptCancellation) -> Self {
        let mut phase = attempt.0.phase();
        if matches!(*phase, Phase::Claimed { .. }) {
            drop(phase);
            return Self::with_identity(attempt.identity());
        }
        *phase = Phase::Pending;
        drop(phase);
        Self(Arc::new(Authority(attempt.0.clone())))
    }

    #[must_use]
    pub fn cancellation_handle(&self) -> QueuedPromptCancellation {
        QueuedPromptCancellation(self.0.0.clone())
    }

    #[must_use]
    pub fn identity(&self) -> QueuedPromptIdentity {
        self.0.0.identity
    }

    pub(in crate::agent::runtime) fn is_cancelled(&self) -> bool {
        matches!(*self.0.0.phase(), Phase::Cancelled)
    }
}

impl QueuedPromptCancellation {
    #[must_use]
    pub fn identity(&self) -> QueuedPromptIdentity {
        self.0.identity
    }

    /// Success guarantees the user message cannot enter history. Failure is not
    /// proof of commitment: await the typed receipt or reconcile the identity.
    #[must_use]
    pub fn cancel(&self) -> bool {
        let mut phase = self.0.phase();
        match *phase {
            Phase::Pending => *phase = Phase::Cancelled,
            Phase::Cancelled => {}
            Phase::Claimed { .. } => return false,
        }
        true
    }

    #[must_use]
    pub fn is_claimed(&self) -> bool {
        matches!(*self.0.phase(), Phase::Claimed { .. })
    }

    /// Snapshot reconciliation identities after claim, including when the enqueue
    /// waiter was dropped. Re-query after the runtime drains before recovery.
    #[must_use]
    pub fn recovery(&self) -> Option<QueuedPromptRecovery> {
        self.is_claimed().then(|| {
            self.recovery_with_reason(
                "submission claimed; consult receipt or reconcile after runtime drain".into(),
            )
        })
    }

    fn recovery_with_reason(&self, reason: String) -> QueuedPromptRecovery {
        let (appends, message) = match &*self.0.phase() {
            Phase::Claimed { appends, message } => (appends.clone(), *message),
            Phase::Pending | Phase::Cancelled => (Vec::new(), None),
        };
        QueuedPromptRecovery {
            submission: self.identity(),
            appends,
            message,
            reason,
        }
    }

    fn failed(&self, error: HarnessError) -> QueuedPromptError {
        let recovery = self.recovery_with_reason(error.to_string());
        // Only an already accepted append, or a write the journal itself
        // reports as indeterminate, leaves the outcome unknown. Every other
        // failure happened before anything was written.
        let uncertain = !recovery.appends.is_empty()
            || matches!(
                error,
                HarnessError::Session(SessionError::AppendIndeterminate(_))
            );
        if uncertain {
            QueuedPromptError::Indeterminate(Box::new(recovery))
        } else {
            QueuedPromptError::Rejected(error)
        }
    }

    /// Classify a receipt that will never arrive. Winning cancellation proves
    /// the message cannot enter history; losing it proves only a claim.
    #[must_use]
    pub fn lost_receipt(&self) -> QueuedPromptError {
        if self.cancel() {
            QueuedPromptError::Rejected(HarnessError::AgentStopped)
        } else {
            QueuedPromptError::Indeterminate(Box::new(self.recovery_with_reason(
                "claimed submission lost its receipt; reconcile before retry".into(),
            )))
        }
    }
}

/// A known committed user message, after its live projection was installed.
#[derive(Clone, Debug)]
pub struct QueuedPromptCommit {
    pub submission: QueuedPromptIdentity,
    pub append: AppendIdentity,
}

/// Retain this record until reopen/replay resolves every accepted append and the
/// caller's live projection. An empty append list means claim outcome is unknown,
/// not permission to retry. UI row/generation identity remains caller-owned.
#[derive(Clone, Debug)]
pub struct QueuedPromptRecovery {
    pub submission: QueuedPromptIdentity,
    pub appends: Vec<AppendIdentity>,
    /// Accepted user-message append, distinct from model-selection appends.
    pub message: Option<AppendIdentity>,
    pub reason: String,
}

impl std::fmt::Display for QueuedPromptRecovery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum QueuedPromptError {
    #[error("queued prompt rejected before acceptance: {0}")]
    Rejected(HarnessError),
    #[error("queued prompt requires reconciliation: {0}")]
    Indeterminate(Box<QueuedPromptRecovery>),
}

/// One submission to prepare durably. Deliberately not Clone.
#[derive(Debug)]
pub struct QueuedPrompt {
    pub text: String,
    pub attachments: Vec<crate::media::Attachment>,
    pub options: PromptOptions,
    pub token: QueuedPromptToken,
}

pub(super) struct QueuedInput {
    pub prepared: PreparedQueuedPrompt,
    pub committed: oneshot::Sender<Result<QueuedPromptCommit, QueuedPromptError>>,
}

/// A claimed input owns its token, so authority lasts until it settles.
struct ClaimedInput {
    input: QueuedInput,
    /// The journal attempt its appends bind to; `None` for an undurable input.
    attempt: Option<QueueAttemptId>,
}

impl QueuedInput {
    fn try_claim(self, queue: &QueueRuntimeState) -> Result<ClaimedInput, Self> {
        let mut phase = self.prepared.token.0.0.phase();
        let pending = matches!(*phase, Phase::Pending);
        if pending {
            *phase = Phase::Claimed {
                appends: Vec::new(),
                message: None,
            };
        }
        drop(phase);
        if !pending {
            return Err(self);
        }
        let attempt = queue
            .holds(&self.prepared.token.0.0)
            .then(|| self.prepared.identity().0);
        Ok(ClaimedInput {
            input: self,
            attempt,
        })
    }

    fn reject(self, error: HarnessError) {
        let _ = self.prepared.cancellation_handle().cancel();
        // Release authority before the receipt is observable.
        drop(self.prepared);
        let _ = self.committed.send(Err(QueuedPromptError::Rejected(error)));
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
    /// yield. Interrupts reject the unclaimed remainder with individual receipts.
    pub(super) async fn consume_queued_batch(
        &self,
        agent: &AgentId,
        context: &mut AgentContext,
        model_profile: &mut String,
        capabilities: &CapabilitySet,
        cancellation: &CancellationToken,
        inputs: Vec<QueuedInput>,
    ) -> bool {
        let mut consumed = false;
        for input in inputs {
            if cancellation.is_cancelled() {
                input.reject(HarnessError::Interrupted);
            } else {
                consumed |= self
                    .consume_queued_input(agent, context, model_profile, capabilities, input)
                    .await;
            }
        }
        consumed
    }

    /// Drain a bounded snapshot of the mailbox, preserving all non-queue commands
    /// for the outer loop (notably JobsReady and ordinary prompt completion).
    pub(super) async fn consume_queued_inputs(
        &self,
        turn: &TurnContext<'_>,
        context: &mut AgentContext,
        model_profile: &mut String,
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
                    consumed |= self
                        .consume_queued_batch(
                            turn.agent,
                            context,
                            model_profile,
                            turn.capabilities,
                            turn.cancellation,
                            inputs,
                        )
                        .await;
                }
                command => deferred.push_back(command),
            }
        }
        consumed
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex as StdMutex};

    use tokio::sync::{Notify, Semaphore};

    use super::super::*;
    use super::{QueueRuntimeState, QueuedInput, QueuedPromptToken};
    pub(super) use crate::agent::runtime::tests::{bounded, enqueue_prompts, events, quiet_root};
    use crate::provider::{
        ProviderContext, ProviderError, ProviderFuture, ResponseStream,
        protocol::{StopReason, events_for_content},
    };

    pub(super) struct Tracking {
        pub(super) requests: StdMutex<Vec<ModelRequest>>,
        changed: Notify,
        gates: [Semaphore; 2],
        first_calls_tool: bool,
    }

    impl Tracking {
        pub(super) async fn request(&self, index: usize) -> ModelRequest {
            bounded(async {
                loop {
                    let notified = self.changed.notified();
                    if let Some(request) = self.requests.lock().unwrap().get(index).cloned() {
                        return request;
                    }
                    notified.await;
                }
            })
            .await
        }

        pub(super) fn release(&self, index: usize) {
            self.gates[index].add_permits(1);
        }

        pub(super) fn count(&self) -> usize {
            self.requests.lock().unwrap().len()
        }
    }

    pub(super) struct Factory(Arc<Tracking>);
    pub(super) struct Context(Arc<Tracking>);

    impl Provider for Factory {
        fn open_context(&self, _: String) -> Result<Box<dyn ProviderContext>, ProviderError> {
            Ok(Box::new(Context(self.0.clone())))
        }
    }

    impl ProviderContext for Context {
        fn invoke(&mut self, request: ModelRequest) -> ProviderFuture {
            let tracking = self.0.clone();
            let index = {
                let mut requests = tracking.requests.lock().unwrap();
                requests.push(request);
                requests.len() - 1
            };
            tracking.changed.notify_one();
            Box::pin(async move {
                if let Some(gate) = tracking.gates.get(index) {
                    gate.acquire().await.unwrap().forget();
                }
                let (item, stop_reason) = if index == 0 && tracking.first_calls_tool {
                    let todo = ToolCall::new("queue-todo", "todo", json!({"items": []})).unwrap();
                    let item = AssistantContent::tool_call("queue-todo", 0, todo);
                    (item, StopReason::ToolUse)
                } else {
                    let item = AssistantContent::text("text/0", 0, format!("answer-{index}"));
                    (item, StopReason::EndTurn)
                };
                let mut events = events_for_content(&[item]);
                events.push(ResponseChunk::ResponseEnded { stop_reason });
                let events = futures_util::stream::iter(events.into_iter().map(Ok));
                Ok(Box::pin(events) as ResponseStream)
            })
        }
    }

    /// A session with "first" (default), "second" and "third" model profiles.
    pub(super) async fn start(
        first_calls_tool: bool,
    ) -> (tempfile::TempDir, Arc<Tracking>, Arc<SessionHandle>) {
        let root = tempfile::tempdir().unwrap();
        let tracking = Arc::new(Tracking {
            requests: StdMutex::new(Vec::new()),
            changed: Notify::new(),
            gates: [Semaphore::new(0), Semaphore::new(0)],
            first_calls_tool,
        });
        let profile =
            |model: &str| ModelProfile::new("queue-test", model, None, 128_000, 4096, true);
        let harness = HarnessBuilder::new(root.path())
            .session_root(root.path().join("sessions"))
            .provider("queue-test", Arc::new(Factory(tracking.clone())))
            .model_profile("first", profile("first-model"))
            .model_profile("second", profile("second-model"))
            .model_profile("third", profile("third-model"))
            .default_model_profile("first")
            .build()
            .await
            .unwrap();
        let session = Arc::new(harness.new_session().await.unwrap());
        (root, tracking, session)
    }

    pub(super) fn prompt(
        session: &Arc<SessionHandle>,
        text: &'static str,
    ) -> tokio::task::JoinHandle<Result<String, HarnessError>> {
        let session = session.clone();
        tokio::spawn(async move { session.prompt(text).await })
    }
    pub(super) type Receipt =
        tokio::task::JoinHandle<Result<QueuedPromptCommit, QueuedPromptError>>;

    /// Enqueues from a task; returns its receipt and the submission's cancellation handle.
    pub(super) fn enqueue(
        session: &Arc<SessionHandle>,
        text: &'static str,
        attachments: Vec<crate::media::Attachment>,
        model: Option<&str>,
    ) -> (Receipt, QueuedPromptCancellation) {
        let input = queued(text, attachments, model);
        let cancellation = input.token.cancellation_handle();
        let session = session.clone();
        let receipt =
            tokio::spawn(
                async move { enqueue_prompts(&session, vec![input]).await.pop().unwrap() },
            );
        (receipt, cancellation)
    }

    /// Whether a message carries the successful result of the first response's todo call.
    pub(super) fn todo_finished(message: &Message) -> bool {
        matches!(message, Message::Tool(results)
            if results.iter().any(|result| result.call_id == "queue-todo" && !result.is_error))
    }

    fn enqueue_batch(
        session: &Arc<SessionHandle>,
        inputs: Vec<QueuedPrompt>,
    ) -> tokio::task::JoinHandle<Vec<Result<QueuedPromptCommit, QueuedPromptError>>> {
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

    pub(super) fn texts<'a>(messages: impl IntoIterator<Item = &'a Message>) -> Vec<String> {
        messages
            .into_iter()
            .filter_map(|message| match message {
                Message::User(blocks) => blocks.iter().find_map(|block| match block {
                    UserContent::Text { text } if text.starts_with("test:") => Some(text.clone()),
                    _ => None,
                }),
                _ => None,
            })
            .collect()
    }

    pub(super) async fn committed(session: &SessionHandle) -> Vec<Message> {
        let records = session.runtime.store.records().await;
        events!(records, SessionEvent::MessageCommitted { message } => message.clone())
    }

    pub(super) async fn model_changes(session: &SessionHandle) -> Vec<String> {
        let records = session.runtime.store.records().await;
        events!(records, SessionEvent::ModelChanged { profile } => profile.name.clone())
    }

    pub(super) async fn stop(session: &SessionHandle) {
        bounded(session.shutdown()).await.unwrap();
        bounded(session.root_tx.closed()).await;
    }

    pub(super) fn parent_inputs<'a>(
        messages: impl IntoIterator<Item = &'a Message>,
    ) -> Vec<String> {
        let blocks = messages.into_iter().flat_map(|message| match message {
            Message::User(blocks) => blocks.as_slice(),
            _ => &[],
        });
        let inputs = blocks.filter_map(|block| match block {
            UserContent::ParentInput { text } => Some(text.clone()),
            _ => None,
        });
        inputs.collect()
    }

    fn queued(
        text: &str,
        attachments: Vec<crate::media::Attachment>,
        model: Option<&str>,
    ) -> QueuedPrompt {
        QueuedPrompt {
            text: text.into(),
            attachments,
            options: PromptOptions {
                model: model.map(str::to_owned),
            },
            token: QueuedPromptToken::new().unwrap(),
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
            queued("test:first", vec![], Some("second")),
            queued("test:image", vec![image], None),
            queued("test:last", vec![], Some("third")),
        ];
        let tokens = inputs.iter().map(|input| input.token.cancellation_handle());
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
            Message::User(blocks) if blocks.iter().any(|block| matches!(block,
                UserContent::Attachment { attachment: crate::media::AttachmentRef::Image(image) }
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
        assert_eq!(tracking.count(), 2);
    }

    #[tokio::test]
    async fn active_batch_is_one_command_and_skips_only_invalid_or_canceled_items() {
        use HarnessError::{ImageLimit, Interrupted, UnknownModelProfile};
        use QueuedPromptError::Rejected;
        let (_root, tracking, session) = start(true).await;
        let turn = prompt(&session, "test:initial");
        tracking.request(0).await;
        let canceled = QueuedPromptToken::new().unwrap();
        let canceled_cancel = canceled.cancellation_handle();
        let oversized = crate::media::Attachment::Image {
            file: None,
            image: crate::tests::png(&vec![0; MAX_IMAGE_BYTES as usize]),
        };
        let inputs = vec![
            queued("test:one", vec![], Some("second")),
            queued("test:invalid", vec![], Some("missing")),
            queued("test:two", vec![], None),
            QueuedPrompt {
                token: canceled,
                ..queued("test:canceled", vec![], Some("third"))
            },
            queued("test:oversized-image", vec![oversized], Some("third")),
        ];
        let enqueue = enqueue_batch(&session, inputs);
        buffered(&session, 1).await;
        assert!(canceled_cancel.cancel());
        tracking.release(0);
        let next = tracking.request(1).await;
        let results = bounded(enqueue).await.unwrap();
        assert!(matches!(
            results.as_slice(),
            [
                Ok(_),
                Err(Rejected(UnknownModelProfile(_))),
                Ok(_),
                Err(Rejected(Interrupted)),
                Err(Rejected(ImageLimit))
            ]
        ));
        assert_eq!(next.model, "second-model");
        let found = texts(next.messages());
        assert_eq!(found, ["test:initial", "test:one", "test:two"]);
        tracking.release(1);
        bounded(turn).await.unwrap().unwrap();
        stop(&session).await;
        assert_eq!(tracking.count(), 2);
    }

    #[tokio::test]
    async fn interrupt_and_shutdown_resolve_every_buffered_batch_receipt() {
        for shutdown in [false, true] {
            let (_root, tracking, session) = start(false).await;
            let turn = prompt(&session, "test:initial");
            tracking.request(0).await;
            let inputs = vec![
                queued("test:one", vec![], None),
                queued("test:two", vec![], Some("second")),
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
                || matches!(
                    result,
                    Err(QueuedPromptError::Rejected(HarnessError::Interrupted))
                )));
            let _ = bounded(turn).await.unwrap();
            stop(&session).await;
            assert_eq!(texts(&committed(&session).await), ["test:initial"]);
            assert_eq!(tracking.count(), 1);
        }
    }

    #[test]
    fn cancellation_and_claim_are_exclusive_across_handles() {
        let input_for = |token| QueuedInput {
            prepared: PreparedQueuedPrompt {
                content: vec![],
                model: None,
                token,
            },
            committed: oneshot::channel().0,
        };
        let pending = QueuedPromptToken::new().unwrap();
        let cancelled = pending.cancellation_handle();
        let observer = pending.cancellation_handle();
        assert!(cancelled.cancel());
        assert!(observer.cancel());
        let queue = QueueRuntimeState::default();
        assert!(input_for(pending).try_claim(&queue).is_err());
        assert!(!observer.is_claimed());

        let claimed = QueuedPromptToken::new().unwrap();
        let copy = claimed.cancellation_handle();
        let input = input_for(claimed).try_claim(&queue).ok();
        let input = input.expect("first claim wins");
        assert!(copy.is_claimed());
        assert!(!copy.cancel());
        drop(input);
    }
}
