//! Queued submission receipts are commit acknowledgements, not turn completions.
//! Gates and command-channel occupancy establish ordering without timing sleeps.

use std::{
    future::Future,
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

use tokio::sync::{Notify, Semaphore};

use super::*;
use crate::provider::{ProviderContext, ProviderError, ProviderFuture, ResponseStream};

struct Tracking {
    requests: StdMutex<Vec<ModelRequest>>,
    changed: Notify,
    gates: [Semaphore; 2],
    first_calls_tool: bool,
}

impl Tracking {
    fn new(first_calls_tool: bool) -> Arc<Self> {
        Arc::new(Self {
            requests: StdMutex::new(Vec::new()),
            changed: Notify::new(),
            gates: [Semaphore::new(0), Semaphore::new(0)],
            first_calls_tool,
        })
    }

    async fn request(&self, index: usize) -> ModelRequest {
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

    fn release(&self, index: usize) {
        self.gates[index].add_permits(1);
    }
}

struct Factory(Arc<Tracking>);
struct Context(Arc<Tracking>);

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
            let chunk = if index == 0 && tracking.first_calls_tool {
                ResponseChunk::Block {
                    block: AssistantContent::ToolCall(ToolCall {
                        id: "queue-todo".into(),
                        name: "todo".into(),
                        arguments: json!({"items": []}),
                    }),
                }
            } else {
                ResponseChunk::TextDelta {
                    text: format!("answer-{index}"),
                }
            };
            Ok(Box::pin(futures_util::stream::iter([Ok(chunk)])) as ResponseStream)
        })
    }
}

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .expect("deterministic queue synchronization timed out")
}

async fn harness(root: &Path, tracking: Arc<Tracking>) -> Harness {
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

fn prompt(
    session: &Arc<SessionHandle>,
    text: &'static str,
) -> tokio::task::JoinHandle<Result<String, HarnessError>> {
    let session = session.clone();
    tokio::spawn(async move { session.prompt(text).await })
}

fn enqueue(
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
async fn buffered(session: &SessionHandle, count: usize) {
    bounded(async {
        while session.root_tx.capacity() != AGENT_CHANNEL_CAPACITY - count {
            tokio::task::yield_now().await;
        }
    })
    .await;
}

fn texts(messages: &[Message]) -> Vec<String> {
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

async fn committed(session: &SessionHandle) -> Vec<Message> {
    session
        .runtime
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

async fn stop(session: &SessionHandle) {
    bounded(session.shutdown()).await.unwrap();
    bounded(session.root_tx.closed()).await;
}

#[test]
fn queued_token_cancellation_is_shared_and_idempotent() {
    for token in [QueuedPromptToken::new(), QueuedPromptToken::default()] {
        let clone = token.clone();
        assert!(!token.is_claimed());
        assert!(clone.cancel());
        assert!(token.cancel());
        assert!(!clone.is_claimed());
    }
}

#[tokio::test]
async fn queued_fifo_images_and_all_model_changes_commit_before_next_request() {
    let root = tempfile::tempdir().unwrap();
    let tracking = Tracking::new(true);
    let harness = harness(root.path(), tracking.clone()).await;
    let session = Arc::new(harness.new_session().await.unwrap());
    let first_image = root.path().join("first.png");
    let second_image = root.path().join("second.jpg");
    fs::write(&first_image, b"first image fixture")
        .await
        .unwrap();
    fs::write(&second_image, b"second image fixture")
        .await
        .unwrap();
    let turn = prompt(&session, "test:initial");
    let in_flight = tracking.request(0).await;
    assert_eq!(in_flight.model, "first-model");

    let first_token = QueuedPromptToken::new();
    let first = enqueue(
        &session,
        "test:queued-one",
        vec![first_image],
        Some("second"),
        &first_token,
    );
    buffered(&session, 1).await;
    let second_token = QueuedPromptToken::new();
    let second = enqueue(
        &session,
        "test:queued-two",
        vec![second_image],
        Some("third"),
        &second_token,
    );
    buffered(&session, 2).await;
    assert!(!first.is_finished());
    assert!(!second.is_finished());
    assert!(!first_token.is_claimed());
    assert!(!second_token.is_claimed());
    assert_eq!(texts(&committed(&session).await), ["test:initial"]);
    assert_eq!(tracking.requests.lock().unwrap().len(), 1);

    tracking.release(0);
    let next = tracking.request(1).await;
    bounded(first).await.unwrap().unwrap();
    bounded(second).await.unwrap().unwrap();
    assert!(
        !turn.is_finished(),
        "receipts must not wait for the gated next response"
    );
    assert!(first_token.is_claimed());
    assert!(second_token.is_claimed());
    assert!(
        !first_token.cancel(),
        "a committed submission cannot be recalled"
    );
    assert!(!second_token.cancel());
    assert_eq!(next.model, "third-model");
    assert_eq!(
        texts(&next.messages),
        ["test:initial", "test:queued-one", "test:queued-two"]
    );
    let images = next
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::User(blocks) => Some(blocks),
            _ => None,
        })
        .flatten()
        .filter_map(|block| match block {
            UserContent::Image { image } => Some(image),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        images
            .iter()
            .map(|image| image.name.as_str())
            .collect::<Vec<_>>(),
        ["first.png", "second.jpg"]
    );
    assert_eq!(
        images
            .iter()
            .map(|image| image.media_type.as_str())
            .collect::<Vec<_>>(),
        ["image/png", "image/jpeg"]
    );
    assert!(images.iter().all(|image| image.data_base64.is_some()));
    for (image, expected) in images.iter().zip([
        b"first image fixture".as_slice(),
        b"second image fixture".as_slice(),
    ]) {
        use base64::Engine as _;
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(image.data_base64.as_ref().unwrap())
                .unwrap(),
            expected
        );
    }
    assert!(
        next.messages.iter().any(|message| matches!(
            message,
            Message::Tool(results) if results.iter().any(|result|
                result.call_id == "queue-todo" && !result.is_error)
        )),
        "the ongoing tool call must finish normally"
    );

    let records = session.runtime.store.records().await;
    let changes = records
        .iter()
        .filter_map(|record| match &record.event {
            SessionEvent::ModelChanged { model_profile, .. } => Some(model_profile.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        changes,
        ["second", "third"],
        "do not collapse intermediate captured model changes"
    );
    let persisted = records
        .iter()
        .filter_map(|record| {
            if matches!(record.event, SessionEvent::ModelRequested { .. }) {
                Some(
                    crate::session::reconstruct_model_request(&records, record.sequence)
                        .unwrap()
                        .1,
                )
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(persisted.len(), 2);
    assert_eq!(persisted[0].model, "first-model");
    assert_eq!(persisted[1].model, "third-model");
    assert_eq!(texts(&persisted[1].messages), texts(&next.messages));

    tracking.release(1);
    bounded(turn).await.unwrap().unwrap();
    stop(&session).await;
    assert_eq!(
        tracking.requests.lock().unwrap().len(),
        2,
        "queued inputs must not become additional turns"
    );
    assert_eq!(
        texts(&committed(&session).await),
        ["test:initial", "test:queued-one", "test:queued-two"]
    );
}

#[tokio::test]
async fn queued_input_is_not_lost_at_a_final_response_boundary() {
    let root = tempfile::tempdir().unwrap();
    let tracking = Tracking::new(false);
    let harness = harness(root.path(), tracking.clone()).await;
    let session = Arc::new(harness.new_session().await.unwrap());
    let turn = prompt(&session, "test:initial");
    tracking.request(0).await;
    let token = QueuedPromptToken::new();
    let receipt = enqueue(&session, "test:follow-up", vec![], None, &token);
    buffered(&session, 1).await;
    assert!(!receipt.is_finished());
    tracking.release(0);
    let next = tracking.request(1).await;
    bounded(receipt).await.unwrap().unwrap();
    assert!(token.is_claimed());
    assert_eq!(texts(&next.messages), ["test:initial", "test:follow-up"]);
    // A final response may complete the original prompt before the queued
    // continuation starts. The queued receipt still acknowledges its commit
    // without waiting for that continuation's gated provider response.
    tracking.release(1);
    bounded(turn).await.unwrap().unwrap();
    stop(&session).await;
    assert_eq!(tracking.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn canceled_buffered_submission_never_commits_text_or_model() {
    let root = tempfile::tempdir().unwrap();
    let tracking = Tracking::new(true);
    let harness = harness(root.path(), tracking.clone()).await;
    let session = Arc::new(harness.new_session().await.unwrap());
    let turn = prompt(&session, "test:initial");
    tracking.request(0).await;
    let token = QueuedPromptToken::new();
    let receipt = enqueue(&session, "test:canceled", vec![], Some("second"), &token);
    buffered(&session, 1).await;
    assert!(!receipt.is_finished());
    assert!(token.cancel());
    assert!(token.cancel());
    tracking.release(0);
    let next = tracking.request(1).await;
    assert!(bounded(receipt).await.unwrap().is_err());
    assert!(!token.is_claimed());
    assert_eq!(next.model, "first-model");
    assert_eq!(texts(&next.messages), ["test:initial"]);
    assert!(
        !session
            .runtime
            .store
            .records()
            .await
            .iter()
            .any(|record| matches!(record.event, SessionEvent::ModelChanged { .. }))
    );
    tracking.release(1);
    bounded(turn).await.unwrap().unwrap();
    stop(&session).await;
    assert_eq!(texts(&committed(&session).await), ["test:initial"]);
    assert_eq!(tracking.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn ordinary_prompt_still_waits_for_its_own_turn() {
    let root = tempfile::tempdir().unwrap();
    let tracking = Tracking::new(true);
    let harness = harness(root.path(), tracking.clone()).await;
    let session = Arc::new(harness.new_session().await.unwrap());
    let turn = prompt(&session, "test:initial");
    tracking.request(0).await;
    let ordinary = prompt(&session, "test:ordinary");
    buffered(&session, 1).await;
    tracking.release(0);
    let next = tracking.request(1).await;
    assert_eq!(texts(&next.messages), ["test:initial"]);
    assert!(!ordinary.is_finished());
    assert!(!turn.is_finished());
    tracking.release(1);
    assert_eq!(bounded(turn).await.unwrap().unwrap(), "answer-1");
    assert_eq!(bounded(ordinary).await.unwrap().unwrap(), "answer-2");
    assert_eq!(
        texts(&tracking.request(2).await.messages),
        ["test:initial", "test:ordinary"]
    );
    stop(&session).await;
    assert_eq!(tracking.requests.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn idle_enqueue_acknowledges_commit_without_waiting_for_provider() {
    let root = tempfile::tempdir().unwrap();
    let tracking = Tracking::new(false);
    let harness = harness(root.path(), tracking.clone()).await;
    let session = Arc::new(harness.new_session().await.unwrap());
    let token = QueuedPromptToken::new();
    let receipt = enqueue(&session, "test:idle", vec![], Some("second"), &token);
    let request = tracking.request(0).await;
    bounded(receipt).await.unwrap().unwrap();
    assert!(token.is_claimed());
    assert!(!token.cancel());
    assert_eq!(request.model, "second-model");
    assert_eq!(texts(&request.messages), ["test:idle"]);
    assert_eq!(texts(&committed(&session).await), ["test:idle"]);
    // The first provider future is still blocked when shutdown interrupts it.
    stop(&session).await;
    assert_eq!(tracking.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn already_canceled_token_never_starts_a_turn() {
    let root = tempfile::tempdir().unwrap();
    let tracking = Tracking::new(false);
    let harness = harness(root.path(), tracking.clone()).await;
    let session = harness.new_session().await.unwrap();
    let token = QueuedPromptToken::default();
    assert!(token.cancel());
    assert!(
        bounded(session.enqueue_prompt_with_options(
            "test:already-canceled",
            &[],
            PromptOptions {
                model: Some("second".into())
            },
            token.clone(),
        ))
        .await
        .is_err()
    );
    assert!(!token.is_claimed());
    stop(&session).await;
    assert!(tracking.requests.lock().unwrap().is_empty());
    assert!(texts(&committed(&session).await).is_empty());
    assert!(
        !session
            .runtime
            .store
            .records()
            .await
            .iter()
            .any(|record| matches!(record.event, SessionEvent::ModelChanged { .. }))
    );
}

#[tokio::test]
async fn queued_validation_errors_do_not_claim_or_commit() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let tracking = Tracking::new(false);
    let harness = harness(root.path(), tracking.clone()).await;
    let session = harness.new_session().await.unwrap();
    let unsupported = root.path().join("unsupported.txt");
    fs::write(&unsupported, b"not an image").await.unwrap();
    let outside_image = outside.path().join("outside.png");
    fs::write(&outside_image, b"outside").await.unwrap();
    let oversized = root.path().join("oversized.png");
    fs::File::create(&oversized)
        .await
        .unwrap()
        .set_len(MAX_IMAGE_BYTES + 1)
        .await
        .unwrap();
    let before = session.runtime.store.records().await.len();
    let cases = [
        (Some("missing-model"), vec![], "model"),
        (None, vec![root.path().join("missing.png")], "missing"),
        (None, vec![unsupported], "format"),
        (None, vec![outside_image], "outside"),
        (None, vec![oversized], "size"),
        (
            None,
            vec![root.path().join("missing.png"); MAX_IMAGES_PER_SUBMISSION + 1],
            "count",
        ),
    ];
    for (model, paths, case) in cases {
        let token = QueuedPromptToken::new();
        let error = bounded(session.enqueue_prompt_with_options(
            "test:invalid",
            &paths,
            PromptOptions {
                model: model.map(str::to_owned),
            },
            token.clone(),
        ))
        .await
        .unwrap_err();
        match case {
            "model" => assert!(matches!(error, HarnessError::UnknownModelProfile(_))),
            "missing" => assert!(matches!(error, HarnessError::Io(_))),
            "format" => assert!(matches!(error, HarnessError::UnsupportedImage)),
            "outside" => assert!(matches!(error, HarnessError::OutsideWorkspace)),
            "size" | "count" => assert!(matches!(error, HarnessError::ImageLimit)),
            _ => unreachable!(),
        }
        assert!(!token.is_claimed(), "{case}");
        assert!(token.cancel(), "{case}");
        assert_eq!(
            session.runtime.store.records().await.len(),
            before,
            "{case}"
        );
    }
    assert!(tracking.requests.lock().unwrap().is_empty());
    stop(&session).await;
}

#[tokio::test]
async fn interrupt_and_shutdown_resolve_queued_receipts_without_silent_loss() {
    for shutdown in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let tracking = Tracking::new(false);
        let harness = harness(root.path(), tracking.clone()).await;
        let session = Arc::new(harness.new_session().await.unwrap());
        let turn = prompt(&session, "test:initial");
        tracking.request(0).await;
        let token = QueuedPromptToken::new();
        let receipt = enqueue(&session, "test:lifecycle", vec![], None, &token);
        buffered(&session, 1).await;
        assert!(!receipt.is_finished());
        // Permit any continuation after interruption, but never release the
        // interrupted first invocation. No provider response can mask the race.
        tracking.release(1);
        if shutdown {
            bounded(session.shutdown()).await.unwrap();
        } else {
            bounded(session.interrupt()).await;
        }
        let outcome = bounded(receipt).await.unwrap();
        assert!(bounded(turn).await.unwrap().is_err());
        stop(&session).await;
        let count = texts(&committed(&session).await)
            .iter()
            .filter(|text| text.as_str() == "test:lifecycle")
            .count();
        if outcome.is_ok() {
            assert_eq!(
                count, 1,
                "successful receipt must correspond to one durable message"
            );
            assert!(token.is_claimed());
        } else {
            assert_eq!(
                count, 0,
                "failed receipt must not silently accept the message"
            );
        }
    }
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
    assert!(
        session
            .enqueue_prompts_with_options(vec![])
            .await
            .is_empty()
    );
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
