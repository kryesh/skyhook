//! Exercise context ownership through the real agent loop, without a remote model.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;

use tokio::sync::{Notify, Semaphore};

use super::*;
use crate::provider::{
    ProviderContext, ProviderError, ProviderFuture, ResponseStream,
    protocol::{StopReason, events_for_content},
};

struct Tracking {
    next: AtomicUsize,
    opened: Mutex<Vec<(usize, String)>>,
    dropped: Mutex<Vec<usize>>,
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
}

impl Provider for Factory {
    fn open_context(&self, correlation: String) -> Result<Box<dyn ProviderContext>, ProviderError> {
        let id = self.0.next.fetch_add(1, Ordering::SeqCst);
        self.0
            .opened
            .lock()
            .unwrap()
            .push((id, correlation.clone()));
        Ok(Box::new(Context {
            tracking: self.0.clone(),
            id,
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
        let tracking = self.tracking.clone();
        Box::pin(async move {
            if tracking.fail_all_calls.load(Ordering::SeqCst) {
                return Err(ProviderError::protocol("retry this request"));
            }
            if block {
                tracking.entered.notify_one();
                tracking.gate.acquire().await.unwrap().forget();
            }
            let item = AssistantContent::text("text/0", 0, "done");
            let mut events = events_for_content(&[item]);
            events.push(ResponseChunk::ResponseEnded {
                stop_reason: StopReason::EndTurn,
            });
            Ok(Box::pin(futures_util::stream::iter(events.into_iter().map(Ok))) as ResponseStream)
        })
    }
}

async fn child_job(session: &SessionHandle) -> JobId {
    session
        .runtime
        .jobs
        .create(crate::job::JobSpec::test(session.root.clone(), "agent"))
        .await
        .unwrap()
        .id
}

#[tokio::test]
async fn cancelled_and_failed_children_release_only_their_own_context() {
    let root = tempfile::tempdir().unwrap();
    let tracking = Arc::new(Tracking::default());
    let harness = harness(root.path(), tracking.clone()).await;
    let session = harness.new_session().await.unwrap();
    for (index, fail) in [(1, false), (2, true)] {
        let child = session.root.child(index);
        let owner_job = child_job(&session).await;
        let sender = session
            .runtime
            .spawn_agent(AgentLaunch {
                id: child.clone(),
                owner_job: Some(owner_job),
                model_profile: "first".into(),
                todos: None,
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

async fn harness(root: &Path, tracking: Arc<Tracking>) -> Harness {
    HarnessBuilder::new(root)
        .session_root(root.join("sessions"))
        .provider("test", Arc::new(Factory(tracking)))
        .model_profile("first", profile("first"))
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
