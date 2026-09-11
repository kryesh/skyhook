//! Request-boundary user input mailbox and cancellation ownership.

use std::sync::atomic::{AtomicU8, Ordering};

use super::*;

mod input;

const PENDING: u8 = 0;
const CLAIMED: u8 = 1;
const CANCELLED: u8 = 2;

/// Cancellation/ownership token for one request-boundary user submission.
///
/// Clone the token before passing it to `enqueue_prompt_with_options`. A
/// successful `cancel` guarantees the input will never enter model history. Once
/// claimed, only the enqueue commit receipt can determine whether it committed;
/// do not edit or resubmit it until that receipt resolves. Use a fresh token for
/// each submission (including edits and retries).
#[derive(Clone, Debug, Default)]
pub struct QueuedPromptToken(Arc<AtomicU8>);

impl QueuedPromptToken {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Cancel unless the runtime has already claimed this input for commit.
    /// Returns true for an input that was already cancelled, too.
    #[must_use]
    pub fn cancel(&self) -> bool {
        match self
            .0
            .compare_exchange(PENDING, CANCELLED, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) | Err(CANCELLED) => true,
            Err(_) => false,
        }
    }

    #[must_use]
    pub fn is_claimed(&self) -> bool {
        self.0.load(Ordering::Acquire) == CLAIMED
    }

    pub(super) fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire) == CANCELLED
    }

    fn claim(&self) -> bool {
        self.0
            .compare_exchange(PENDING, CLAIMED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}

/// One submission in an atomic request-boundary batch.
#[derive(Clone, Debug)]
pub struct QueuedPrompt {
    pub text: String,
    pub paths: Vec<PathBuf>,
    pub options: PromptOptions,
    pub token: QueuedPromptToken,
}

pub(super) struct QueuedInput {
    pub content: Vec<UserContent>,
    pub model: Option<String>,
    pub token: QueuedPromptToken,
    pub committed: oneshot::Sender<Result<(), HarnessError>>,
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
                let _ = input.token.cancel();
                let _ = input.committed.send(Err(HarnessError::Interrupted));
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
                let _ = input.token.cancel();
                let _ = input.committed.send(Err(HarnessError::Interrupted));
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
    use std::{
        future::Future,
        sync::{Arc, Mutex as StdMutex},
        time::Duration,
    };

    use tokio::sync::{Notify, Semaphore};

    use super::super::*;
    use super::QueuedPromptToken;
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
        pub(super) fn new(first_calls_tool: bool) -> Arc<Self> {
            Arc::new(Self {
                requests: StdMutex::new(Vec::new()),
                changed: Notify::new(),
                gates: [Semaphore::new(0), Semaphore::new(0)],
                first_calls_tool,
            })
        }

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
                let index = requests.len();
                requests.push(request);
                index
            };
            tracking.changed.notify_one();
            Box::pin(async move {
                if let Some(gate) = tracking.gates.get(index) {
                    gate.acquire().await.unwrap().forget();
                }
                let tool = index == 0 && tracking.first_calls_tool;
                let item = if tool {
                    AssistantContent::tool_call(
                        "queue-todo",
                        0,
                        ToolCall {
                            id: "queue-todo".into(),
                            name: "todo".into(),
                            arguments: json!({"items": []}),
                        },
                    )
                } else {
                    AssistantContent::text("text/0", 0, format!("answer-{index}"))
                };
                let mut events = events_for_content(&[item]);
                events.push(ResponseChunk::ResponseEnded {
                    stop_reason: if tool {
                        StopReason::ToolUse
                    } else {
                        StopReason::EndTurn
                    },
                });
                Ok(
                    Box::pin(futures_util::stream::iter(events.into_iter().map(Ok)))
                        as ResponseStream,
                )
            })
        }
    }

    pub(super) async fn bounded<T>(future: impl Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(10), future)
            .await
            .expect("deterministic queue synchronization timed out")
    }

    pub(super) async fn harness(root: &Path, tracking: Arc<Tracking>) -> Harness {
        let profile = |model: &str| ModelProfile {
            provider: "queue-test".into(),
            model: model.into(),
            reasoning: None,
            max_context: 128_000,
            max_output: 4096,
            supports_images: true,
        };
        HarnessBuilder::new(root)
            .session_root(root.join("sessions"))
            .provider("queue-test", Arc::new(Factory(tracking)))
            .model_profile("first", profile("first-model"))
            .model_profile("second", profile("second-model"))
            .model_profile("third", profile("third-model"))
            .default_model_profile("first")
            .build()
            .await
            .unwrap()
    }

    pub(super) fn prompt(
        session: &Arc<SessionHandle>,
        text: &'static str,
    ) -> tokio::task::JoinHandle<Result<String, HarnessError>> {
        let session = session.clone();
        tokio::spawn(async move { session.prompt(text).await })
    }

    pub(super) fn enqueue(
        session: &Arc<SessionHandle>,
        text: &'static str,
        paths: Vec<PathBuf>,
        model: Option<&str>,
        token: &QueuedPromptToken,
    ) -> tokio::task::JoinHandle<Result<(), HarnessError>> {
        let session = session.clone();
        let token = token.clone();
        let options = PromptOptions {
            model: model.map(str::to_owned),
        };
        tokio::spawn(async move {
            session
                .enqueue_prompt_with_options(text, &paths, options, token)
                .await
        })
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

    pub(super) fn texts(messages: &[Message]) -> Vec<String> {
        messages
            .iter()
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
        let runtime = &session.runtime;
        runtime
            .store
            .records()
            .await
            .into_iter()
            .filter_map(|record| match record.event {
                SessionEvent::MessageCommitted { message } => Some(message),
                _ => None,
            })
            .collect()
    }

    pub(super) async fn stop(session: &SessionHandle) {
        bounded(session.shutdown()).await.unwrap();
        bounded(session.root_tx.closed()).await;
    }

    pub(super) fn parent_inputs(messages: &[Message]) -> Vec<String> {
        messages
            .iter()
            .filter_map(|message| match message {
                Message::User(blocks) => Some(blocks),
                _ => None,
            })
            .flatten()
            .filter_map(|block| match block {
                UserContent::ParentInput { text } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    fn queued(text: &str, paths: Vec<PathBuf>, model: Option<&str>) -> QueuedPrompt {
        QueuedPrompt {
            text: text.into(),
            paths,
            options: PromptOptions {
                model: model.map(str::to_owned),
            },
            token: QueuedPromptToken::new(),
        }
    }

    #[tokio::test]
    async fn idle_batch_prepares_images_before_any_request() {
        let root = tempfile::tempdir().unwrap();
        let tracking = Tracking::new(false);
        let harness = harness(root.path(), tracking.clone()).await;
        let session = Arc::new(harness.new_session().await.unwrap());

        let image = root.path().join("batch.png");
        fs::write(&image, b"batch image fixture").await.unwrap();
        let inputs = vec![
            queued("test:first", vec![], Some("second")),
            queued("test:image", vec![image], None),
            queued("test:last", vec![], Some("third")),
        ];
        let tokens = inputs
            .iter()
            .map(|input| input.token.clone())
            .collect::<Vec<_>>();
        let enqueue = {
            let session = session.clone();
            tokio::spawn(async move { session.enqueue_prompts_with_options(inputs).await })
        };
        let request = tracking.request(0).await;
        assert!(bounded(enqueue).await.unwrap().iter().all(Result::is_ok));
        assert!(tokens.iter().all(QueuedPromptToken::is_claimed));
        assert_eq!(
            texts(&request.messages),
            ["test:first", "test:image", "test:last"]
        );
        assert_eq!(request.model, "third-model");
        assert!(request.messages.iter().any(|message| matches!(message,
            Message::User(blocks) if blocks.iter().any(|block| matches!(block,
                UserContent::Image { image } if image.name == "batch.png" && image.data_base64.is_some())))));
        tracking.release(0);
        // A normal prompt is a mailbox barrier after the batch's completed turn.
        let barrier = prompt(&session, "test:barrier");
        let next = tracking.request(1).await;
        assert_eq!(
            texts(&next.messages),
            ["test:first", "test:image", "test:last", "test:barrier"]
        );
        tracking.release(1);
        bounded(barrier).await.unwrap().unwrap();
        stop(&session).await;
        assert_eq!(tracking.requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn active_batch_is_one_command_and_skips_only_invalid_or_canceled_items() {
        let root = tempfile::tempdir().unwrap();
        let tracking = Tracking::new(true);
        let harness = harness(root.path(), tracking.clone()).await;
        let session = Arc::new(harness.new_session().await.unwrap());

        let turn = prompt(&session, "test:initial");
        tracking.request(0).await;
        let canceled = QueuedPromptToken::new();
        let inputs = vec![
            queued("test:one", vec![], Some("second")),
            queued("test:invalid", vec![], Some("missing")),
            queued("test:two", vec![], None),
            QueuedPrompt {
                token: canceled.clone(),
                ..queued("test:canceled", vec![], Some("third"))
            },
            queued(
                "test:missing-image",
                vec![root.path().join("missing.png")],
                Some("third"),
            ),
        ];
        let enqueue = {
            let session = session.clone();
            tokio::spawn(async move { session.enqueue_prompts_with_options(inputs).await })
        };
        buffered(&session, 1).await;
        assert!(canceled.cancel());
        tracking.release(0);
        let next = tracking.request(1).await;
        let results = bounded(enqueue).await.unwrap();
        assert_eq!(results.len(), 5);
        assert!(results[0].is_ok());
        assert!(matches!(
            results[1],
            Err(HarnessError::UnknownModelProfile(_))
        ));
        assert!(results[2].is_ok());
        assert!(matches!(results[3], Err(HarnessError::Interrupted)));
        assert!(results[4].is_err());
        assert_eq!(next.model, "second-model");
        assert_eq!(
            texts(&next.messages),
            ["test:initial", "test:one", "test:two"]
        );
        tracking.release(1);
        bounded(turn).await.unwrap().unwrap();
        stop(&session).await;
        assert_eq!(tracking.requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn interrupt_rejects_every_member_of_buffered_batch() {
        let root = tempfile::tempdir().unwrap();
        let tracking = Tracking::new(false);
        let harness = harness(root.path(), tracking.clone()).await;
        let session = Arc::new(harness.new_session().await.unwrap());

        let turn = prompt(&session, "test:initial");
        tracking.request(0).await;
        let enqueue = {
            let session = session.clone();
            tokio::spawn(async move {
                session
                    .enqueue_prompts_with_options(vec![
                        queued("test:one", vec![], None),
                        queued("test:two", vec![], Some("second")),
                    ])
                    .await
            })
        };
        buffered(&session, 1).await;
        session.interrupt().await;
        let results = bounded(enqueue).await.unwrap();
        assert_eq!(results.len(), 2);
        assert!(
            results
                .iter()
                .all(|result| matches!(result, Err(HarnessError::Interrupted)))
        );
        let _ = bounded(turn).await.unwrap();
        stop(&session).await;
        assert_eq!(texts(&committed(&session).await), ["test:initial"]);
        assert_eq!(tracking.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn shutdown_resolves_every_buffered_batch_receipt() {
        let root = tempfile::tempdir().unwrap();
        let tracking = Tracking::new(false);
        let harness = harness(root.path(), tracking.clone()).await;
        let session = Arc::new(harness.new_session().await.unwrap());

        let turn = prompt(&session, "test:initial");
        tracking.request(0).await;
        let enqueue = {
            let session = session.clone();
            tokio::spawn(async move {
                session
                    .enqueue_prompts_with_options(vec![
                        queued("test:one", vec![], None),
                        queued("test:two", vec![], Some("second")),
                    ])
                    .await
            })
        };
        buffered(&session, 1).await;
        stop(&session).await;
        let results = bounded(enqueue).await.unwrap();
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(Result::is_err));
        let _ = bounded(turn).await.unwrap();
        assert_eq!(texts(&committed(&session).await), ["test:initial"]);
        assert_eq!(tracking.requests.lock().unwrap().len(), 1);
    }
    #[test]
    fn cancellation_and_claim_are_exclusive_across_clones() {
        let pending = QueuedPromptToken::new();
        let cancelled = pending.clone();
        assert!(cancelled.cancel());
        assert!(pending.cancel());
        assert!(!pending.claim());
        assert!(!pending.is_claimed());

        let claimed = QueuedPromptToken::new();
        let copy = claimed.clone();
        assert!(claimed.claim());
        assert!(copy.is_claimed());
        assert!(!copy.cancel());
        assert!(!copy.claim());
    }
}
