//! Exercise context ownership through the real agent loop, without a remote model.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;

use tokio::sync::{Notify, Semaphore};

use super::*;
use crate::provider::{ProviderContext, ProviderError, ProviderFuture, ResponseStream};

struct Tracking {
    next: AtomicUsize,
    opened: Mutex<Vec<(usize, String)>>,
    dropped: Mutex<Vec<usize>>,
    requests: Mutex<Vec<(usize, ModelRequest)>>,
    fail_open: AtomicBool,
    fail_call: AtomicBool,
    fail_all_calls: AtomicBool,
    entered: Notify,
    released: Notify,
    gate: Semaphore,
}

impl Default for Tracking {
    fn default() -> Self {
        Self {
            next: AtomicUsize::new(0),
            opened: Mutex::default(),
            dropped: Mutex::default(),
            requests: Mutex::default(),
            fail_open: AtomicBool::new(false),
            fail_call: AtomicBool::new(false),
            fail_all_calls: AtomicBool::new(false),
            entered: Notify::new(),
            released: Notify::new(),
            gate: Semaphore::new(0),
        }
    }
}

struct Factory(Arc<Tracking>);
struct Context {
    tracking: Arc<Tracking>,
    id: usize,
    correlation: String,
}

impl Provider for Factory {
    fn open_context(&self, correlation: String) -> Result<Box<dyn ProviderContext>, ProviderError> {
        if self.0.fail_open.load(Ordering::SeqCst) {
            return Err(ProviderError::protocol("context creation failed"));
        }
        let id = self.0.next.fetch_add(1, Ordering::SeqCst);
        self.0
            .opened
            .lock()
            .unwrap()
            .push((id, correlation.clone()));
        Ok(Box::new(Context {
            tracking: self.0.clone(),
            id,
            correlation,
        }))
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        self.tracking.dropped.lock().unwrap().push(self.id);
        self.tracking.released.notify_one();
    }
}

impl ProviderContext for Context {
    fn invoke(&mut self, request: ModelRequest) -> ProviderFuture {
        assert_eq!(
            request.correlation.as_deref(),
            Some(self.correlation.as_str())
        );
        let latest = request
            .messages
            .iter()
            .rev()
            .find_map(|message| match message {
                Message::User(blocks) => blocks.iter().find_map(|block| match block {
                    UserContent::Text { text } => Some(text.as_str()),
                    _ => None,
                }),
                _ => None,
            });
        let block = latest == Some("block");
        let tool = latest == Some("use tool")
            && matches!(request.messages.iter().rev().nth(1), Some(Message::User(_)));
        self.tracking
            .requests
            .lock()
            .unwrap()
            .push((self.id, request));
        let tracking = self.tracking.clone();
        Box::pin(async move {
            if tracking.fail_call.swap(false, Ordering::SeqCst)
                || tracking.fail_all_calls.load(Ordering::SeqCst)
            {
                return Err(ProviderError::protocol("retry this request"));
            }
            if block {
                tracking.entered.notify_one();
                tracking.gate.acquire().await.unwrap().forget();
            }
            let response = if tool {
                ResponseChunk::Block {
                    block: AssistantContent::ToolCall(ToolCall {
                        id: "todo-call".into(),
                        name: "todo".into(),
                        arguments: json!({"items": []}),
                    }),
                }
            } else {
                ResponseChunk::TextDelta {
                    text: "done".into(),
                }
            };
            Ok(Box::pin(futures_util::stream::iter([Ok(response)])) as ResponseStream)
        })
    }
}

#[tokio::test]
async fn cancelled_and_failed_children_release_only_their_own_context() {
    let root = tempfile::tempdir().unwrap();
    let tracking = Arc::new(Tracking::default());
    let harness = harness(root.path(), tracking.clone(), tracking.clone()).await;
    let session = harness.new_session().await.unwrap();
    for (index, fail) in [(1, false), (2, true)] {
        let child = session.root.child(index);
        let sender = session
            .runtime
            .spawn_agent(AgentLaunch {
                id: child.clone(),
                parent: Some(session.root.clone()),
                owner_job: None,
                model_profile: "first".into(),
                agent_profile: None,
                todos: None,
                one_shot: true,
                available_depth: 0,
                location: crate::execution::ExecutionLocation::root(root.path().to_owned()),
            })
            .await
            .unwrap();
        tracking.fail_all_calls.store(fail, Ordering::SeqCst);
        let (done, received) = oneshot::channel();
        sender
            .send(AgentCommand::Input {
                model: None,
                content: vec![UserContent::Text {
                    text: "block".into(),
                }],
                done: Some(done),
            })
            .await
            .unwrap();
        if !fail {
            tracking.entered.notified().await;
            session.runtime.interrupt_tree(&child).await;
        }
        assert!(received.await.unwrap().is_err());
        wait_dropped(&tracking, index as usize).await;
        tracking.fail_all_calls.store(false, Ordering::SeqCst);
        session.prompt("root still usable").await.unwrap();
        assert!(!tracking.dropped.lock().unwrap().contains(&0));
    }
    assert_eq!(tracking.opened.lock().unwrap().len(), 3);
    session.shutdown().await.unwrap();
    wait_dropped(&tracking, 0).await;
}

fn profile(model: &str) -> ModelProfile {
    ModelProfile {
        provider: "test".into(),
        model: model.into(),
        reasoning: None,
        max_context: 128_000,
        max_output: 16_384,
        supports_images: false,
    }
}

async fn harness(root: &Path, tracking: Arc<Tracking>, replacement: Arc<Tracking>) -> Harness {
    HarnessBuilder::new(root)
        .session_root(root.join("sessions"))
        .provider("test", Arc::new(Factory(tracking)))
        .provider("replacement", Arc::new(Factory(replacement)))
        .model_profile("first", profile("first"))
        .model_profile("alias", profile("first"))
        .model_profile(
            "second",
            ModelProfile {
                provider: "replacement".into(),
                max_context: 64_000,
                ..profile("second")
            },
        )
        .default_model_profile("first")
        .build()
        .await
        .unwrap()
}

async fn wait_dropped(tracking: &Tracking, id: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let notified = tracking.released.notified();
            if tracking.dropped.lock().unwrap().contains(&id) {
                break;
            }
            notified.await;
        }
    })
    .await
    .expect("context released");
}

async fn select(session: &SessionHandle, model: &str) -> Result<String, HarnessError> {
    session
        .prompt_with_options(
            "new model",
            &[],
            PromptOptions {
                model: Some(model.into()),
            },
        )
        .await
}

#[tokio::test]
async fn turns_tools_retries_and_interruptions_reuse_one_context() {
    let root = tempfile::tempdir().unwrap();
    let tracking = Arc::new(Tracking::default());
    let harness = harness(root.path(), tracking.clone(), tracking.clone()).await;
    let session = harness.new_session().await.unwrap();
    session.prompt("use tool").await.unwrap();
    tracking.fail_call.store(true, Ordering::SeqCst);
    session.prompt("retry").await.unwrap();
    assert_eq!(tracking.requests.lock().unwrap().len(), 4);
    let running = session.clone();
    let prompt = tokio::spawn(async move { running.prompt("block").await });
    tracking.entered.notified().await;
    session.interrupt().await;
    assert!(prompt.await.unwrap().is_err());
    session.prompt("after interrupt").await.unwrap();
    assert_eq!(tracking.opened.lock().unwrap().len(), 1);
    assert!(
        tracking
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|(id, _)| *id == 0)
    );
    assert!(tracking.dropped.lock().unwrap().is_empty());
    session.shutdown().await.unwrap();
    wait_dropped(&tracking, 0).await;
}

#[tokio::test]
async fn blocked_child_has_an_independent_context_and_releases_it_on_completion() {
    let root = tempfile::tempdir().unwrap();
    let tracking = Arc::new(Tracking::default());
    let harness = harness(root.path(), tracking.clone(), tracking.clone()).await;
    let session = harness.new_session().await.unwrap();
    let sender = session
        .runtime
        .spawn_agent(AgentLaunch {
            id: session.root.child(1),
            parent: Some(session.root.clone()),
            owner_job: None,
            model_profile: "first".into(),
            agent_profile: None,
            todos: None,
            one_shot: true,
            available_depth: 0,
            location: crate::execution::ExecutionLocation::root(root.path().to_owned()),
        })
        .await
        .unwrap();
    let (done, received) = oneshot::channel();
    sender
        .send(AgentCommand::Input {
            model: None,
            content: vec![UserContent::Text {
                text: "block".into(),
            }],
            done: Some(done),
        })
        .await
        .unwrap();
    tracking.entered.notified().await;
    tokio::time::timeout(
        Duration::from_secs(5),
        session.prompt("root while child waits"),
    )
    .await
    .unwrap()
    .unwrap();
    let opened = tracking.opened.lock().unwrap().clone();
    assert_eq!(
        opened,
        [
            (0, session.root.to_string()),
            (1, session.root.child(1).to_string())
        ]
    );
    assert!(tracking.dropped.lock().unwrap().is_empty());
    tracking.gate.add_permits(1);
    assert_eq!(received.await.unwrap().unwrap(), "done");
    wait_dropped(&tracking, 1).await;
    session.prompt("root after child").await.unwrap();
    assert_eq!(tracking.opened.lock().unwrap().len(), 2);
    session.shutdown().await.unwrap();
    wait_dropped(&tracking, 0).await;
}

#[tokio::test]
async fn model_swaps_are_transactional_and_resume_reopens_the_same_identity() {
    let root = tempfile::tempdir().unwrap();
    let first = Arc::new(Tracking::default());
    let second = Arc::new(Tracking::default());
    let harness = harness(root.path(), first.clone(), second.clone()).await;
    let session = harness.new_session().await.unwrap();
    session.prompt("original history").await.unwrap();
    select(&session, "alias").await.unwrap();
    assert_eq!(first.opened.lock().unwrap().len(), 1);
    second.fail_open.store(true, Ordering::SeqCst);
    let before = session.runtime.store.records().await.len();
    assert!(select(&session, "second").await.is_err());
    assert_eq!(session.runtime.store.records().await.len(), before);
    session.prompt("still original").await.unwrap();
    assert!(first.dropped.lock().unwrap().is_empty());
    second.fail_open.store(false, Ordering::SeqCst);
    select(&session, "second").await.unwrap();
    wait_dropped(&first, 0).await;
    let request = second.requests.lock().unwrap()[0].1.clone();
    assert_eq!(request.model, "second");
    assert!(
        serde_json::to_string(&request.messages)
            .unwrap()
            .contains("original history")
    );
    select(&session, "first").await.unwrap();
    wait_dropped(&second, 0).await;
    assert_eq!(
        first.opened.lock().unwrap().as_slice(),
        [(0, session.root.to_string()), (1, session.root.to_string())]
    );
    session.shutdown().await.unwrap();
    wait_dropped(&first, 1).await;
    session.runtime.store.close().await.unwrap();
    let resumed = harness.resume_session(session.id()).await.unwrap();
    resumed.prompt("resumed").await.unwrap();
    assert_eq!(
        first.opened.lock().unwrap()[2],
        (2, session.root.to_string())
    );
    let another = harness.new_session().await.unwrap();
    assert_ne!(first.opened.lock().unwrap()[3].1, session.root.to_string());
    resumed.shutdown().await.unwrap();
    another.shutdown().await.unwrap();
    wait_dropped(&first, 2).await;
    wait_dropped(&first, 3).await;
}

#[tokio::test]
async fn recorded_context_preserves_runtime_tail_and_uses_current_history() {
    let root = tempfile::tempdir().unwrap();
    let tracking = Arc::new(Tracking::default());
    let harness = harness(root.path(), tracking.clone(), tracking.clone()).await;
    let session = harness.new_session().await.unwrap();
    session.prompt("first turn").await.unwrap();
    let records = session.runtime.store.records().await;
    let requested = records
        .iter()
        .rev()
        .find(|record| {
            matches!(
                record.event,
                SessionEvent::ModelRequested {
                    purpose: crate::session::ModelPurpose::Agent,
                    ..
                }
            )
        })
        .unwrap();
    let (_, request) =
        crate::session::reconstruct_model_request(&records, requested.sequence).unwrap();
    let runtime = request.messages.last().unwrap();
    assert!(
        matches!(runtime, Message::User(blocks) if blocks.iter().any(|block|
        matches!(block, UserContent::Runtime { .. })))
    );
    let mut template = request.clone();
    template.messages.clear();
    let meter = compact::TokenMeter::restore(&records, &session.root, &template);
    let mut current = template;
    current.messages = crate::session::project_history(&records, &session.root)
        .unwrap()
        .into_iter()
        .map(|(_, message)| message)
        .collect();
    let without_runtime = meter.estimate(&current);
    current.messages.push(runtime.clone());
    let expected = meter.estimate(&current);
    assert!(expected > without_runtime);
    let actual = &recorded_context(&records)[&session.root];
    assert_eq!(actual.tokens, expected);
    assert_eq!(actual.capacity, 128_000);
    session.shutdown().await.unwrap();
}
