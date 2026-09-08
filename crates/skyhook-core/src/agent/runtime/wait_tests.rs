//! Wait is a notification barrier, not an output reader or a background job.
//! Provider gates and job state establish ordering; no synchronization sleeps.

use std::{
    future::Future,
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

use serde_json::Value;
use tokio::sync::{Notify, Semaphore};

use super::*;
use crate::{
    job::{JobOutcome, JobSpec, JobState},
    provider::{
        ProviderContext, ProviderError, ProviderFuture, ResponseStream,
        protocol::{ItemKind, StopReason, events_for_content},
    },
    tool::ToolOutput,
};

struct Step {
    model: &'static str,
    content: AssistantContent,
    gate: Semaphore,
}

struct Tracking {
    steps: Vec<Step>,
    requests: StdMutex<Vec<(usize, ModelRequest)>>,
    changed: Notify,
}

impl Tracking {
    fn new(steps: Vec<(&'static str, AssistantContent)>) -> Arc<Self> {
        Arc::new(Self {
            steps: steps
                .into_iter()
                .map(|(model, content)| Step {
                    model,
                    content,
                    gate: Semaphore::new(0),
                })
                .collect(),
            requests: StdMutex::new(Vec::new()),
            changed: Notify::new(),
        })
    }

    async fn request(&self, step: usize) -> ModelRequest {
        bounded(async {
            loop {
                let notified = self.changed.notified();
                if let Some((_, request)) = self
                    .requests
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|(id, _)| *id == step)
                {
                    return request.clone();
                }
                notified.await;
            }
        })
        .await
    }

    fn release(&self, step: usize) {
        self.steps[step].gate.add_permits(1);
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
        let step = {
            let mut requests = tracking.requests.lock().unwrap();
            let step = tracking
                .steps
                .iter()
                .enumerate()
                .position(|(index, step)| {
                    step.model == request.model && !requests.iter().any(|(seen, _)| *seen == index)
                })
                .expect("unexpected extra model request");
            requests.push((step, request));
            step
        };
        tracking.changed.notify_one();
        Box::pin(async move {
            tracking.steps[step].gate.acquire().await.unwrap().forget();
            let content = tracking.steps[step].content.clone();
            let stop_reason = if content.kind == ItemKind::ToolCall {
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
            };
            let mut events = events_for_content(&[content]);
            events.push(ResponseChunk::ResponseEnded { stop_reason });
            Ok(Box::pin(futures_util::stream::iter(events.into_iter().map(Ok))) as ResponseStream)
        })
    }
}

fn call(id: &str, name: &str, arguments: Value) -> AssistantContent {
    AssistantContent::tool_call(
        id,
        0,
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments,
        },
    )
}

fn answer() -> AssistantContent {
    AssistantContent::text("answer", 0, "done".to_owned())
}

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .expect("wait test synchronization timed out")
}

async fn harness(root: &Path, tracking: Arc<Tracking>) -> Harness {
    let profile = |model: &str| ModelProfile {
        provider: "wait-test".into(),
        model: model.into(),
        reasoning: None,
        max_context: 128_000,
        max_output: 4096,
        supports_images: true,
    };
    HarnessBuilder::new(root)
        .session_root(root.join("sessions"))
        .provider("wait-test", Arc::new(Factory(tracking)))
        .model_profile("root", profile("root"))
        .model_profile("child", profile("child"))
        .default_model_profile("root")
        .build()
        .await
        .unwrap()
}

fn prompt(session: &Arc<SessionHandle>) -> tokio::task::JoinHandle<Result<String, HarnessError>> {
    let session = session.clone();
    tokio::spawn(async move { session.prompt("start").await })
}

async fn running_job(session: &SessionHandle, tool: &str) -> JobId {
    bounded(async {
        loop {
            if let Some(job) = session
                .runtime
                .jobs
                .list(&session.root)
                .await
                .into_iter()
                .find(|job| job.tool == tool && job.state == JobState::Running)
            {
                return job.id;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
}

async fn complete_background(session: &SessionHandle, value: &str) -> JobId {
    let lease = session
        .runtime
        .jobs
        .create(JobSpec {
            background: true,
            ..JobSpec::test(session.root.clone(), "wait-fixture")
        })
        .await
        .unwrap();
    session
        .runtime
        .jobs
        .transition(lease.id, JobState::Running)
        .await
        .unwrap();
    session
        .runtime
        .jobs
        .finish(
            lease.id,
            JobOutcome::Completed(ToolOutput::new(json!({"value":value}))),
        )
        .await
        .unwrap();
    lease.id
}

fn events(request: &ModelRequest) -> Vec<Value> {
    request
        .messages
        .iter()
        .flat_map(|message| match message {
            Message::User(content) => content
                .iter()
                .filter_map(|block| match block {
                    UserContent::Runtime { text } => text
                        .strip_prefix("<skyhook_job_events>\n")
                        .and_then(|text| text.strip_suffix("\n</skyhook_job_events>"))
                        .map(|text| serde_json::from_str::<Vec<Value>>(text).unwrap()),
                    _ => None,
                })
                .flatten()
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .collect()
}

fn assert_reason(request: &ModelRequest, id: &str, reason: &str) {
    let results = request
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::Tool(results) => Some(results),
            _ => None,
        })
        .flatten()
        .filter(|result| result.call_id == id)
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 1, "wait must have exactly one result");
    assert!(!results[0].is_error, "wait failed: {:?}", results[0]);
    assert_eq!(results[0].result["result"], json!({"reason":reason}));
}

#[tokio::test]
async fn positive_timeouts_do_not_wake_on_their_own_jobs() {
    let workspace = tempfile::tempdir().unwrap();
    let tracking = Tracking::new(vec![
        ("root", call("first", "wait", json!({"timeout":1}))),
        ("root", call("second", "wait", json!({"timeout":1}))),
        ("root", answer()),
    ]);
    let harness = harness(workspace.path(), tracking.clone()).await;
    let session = Arc::new(harness.new_session().await.unwrap());
    let turn = prompt(&session);
    tracking.request(0).await;
    tracking.release(0);
    assert_reason(&tracking.request(1).await, "first", "timeout");
    tracking.release(1);
    let request = tracking.request(2).await;
    assert_reason(&request, "second", "timeout");
    assert!(
        events(&request).is_empty(),
        "foreground wait completion must not notify its owner"
    );
    tracking.release(2);
    assert_eq!(bounded(turn).await.unwrap().unwrap(), "done");
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn clustered_background_completions_wake_and_inject_saved_output_once() {
    let workspace = tempfile::tempdir().unwrap();
    let tracking = Tracking::new(vec![
        ("root", call("waiting", "wait", json!({}))),
        ("root", call("again", "wait", json!({"timeout":1}))),
        ("root", answer()),
    ]);
    let harness = harness(workspace.path(), tracking.clone()).await;
    let session = Arc::new(harness.new_session().await.unwrap());
    let turn = prompt(&session);
    tracking.request(0).await;
    tracking.release(0);
    running_job(&session, "wait").await;
    let first = complete_background(&session, "first-output").await;
    let second = complete_background(&session, "second-output").await;
    let request = tracking.request(1).await;
    assert_reason(&request, "waiting", "event");
    let notifications = events(&request);
    assert_eq!(
        notifications.len(),
        2,
        "clustered completions should share the next request"
    );
    for (id, value) in [(first, "first-output"), (second, "second-output")] {
        let notification = notifications
            .iter()
            .find(|event| event["id"] == id.get())
            .unwrap();
        assert!(serde_json::to_string(notification).unwrap().contains(value));
    }
    tracking.release(1);
    let next = tracking.request(2).await;
    assert_reason(&next, "again", "timeout");
    assert_eq!(
        events(&next),
        notifications,
        "retained history must not get another copy"
    );
    tracking.release(2);
    bounded(turn).await.unwrap().unwrap();
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn completion_before_wait_registration_is_not_lost() {
    let workspace = tempfile::tempdir().unwrap();
    let tracking = Tracking::new(vec![
        ("root", call("waiting", "wait", json!({"timeout":1}))),
        ("root", answer()),
    ]);
    let harness = harness(workspace.path(), tracking.clone()).await;
    let session = Arc::new(harness.new_session().await.unwrap());
    let turn = prompt(&session);
    tracking.request(0).await;
    let job = complete_background(&session, "already-ready").await;
    tracking.release(0);
    let next = tracking.request(1).await;
    assert_reason(&next, "waiting", "event");
    assert_eq!(
        events(&next)
            .iter()
            .filter(|event| event["id"] == job.get())
            .count(),
        1
    );
    tracking.release(1);
    bounded(turn).await.unwrap().unwrap();
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn queued_user_input_wakes_wait_without_consuming_the_input() {
    let workspace = tempfile::tempdir().unwrap();
    let tracking = Tracking::new(vec![
        ("root", call("waiting", "wait", json!({"timeout":null}))),
        ("root", call("again", "wait", json!({"timeout":1}))),
        ("root", answer()),
    ]);
    let harness = harness(workspace.path(), tracking.clone()).await;
    let session = Arc::new(harness.new_session().await.unwrap());
    let turn = prompt(&session);
    tracking.request(0).await;
    tracking.release(0);
    running_job(&session, "wait").await;
    let token = QueuedPromptToken::new();
    let queued = tokio::spawn({
        let session = session.clone();
        async move {
            session
                .enqueue_prompt_with_options(
                    "queued-wake-marker",
                    &[],
                    PromptOptions::default(),
                    token,
                )
                .await
        }
    });
    let request = tracking.request(1).await;
    bounded(queued).await.unwrap().unwrap();
    assert_reason(&request, "waiting", "event");
    assert!(
        serde_json::to_string(&request.messages)
            .unwrap()
            .contains("queued-wake-marker")
    );
    tracking.release(1);
    let next = tracking.request(2).await;
    assert_reason(&next, "again", "timeout");
    tracking.release(2);
    bounded(turn).await.unwrap().unwrap();
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn parent_input_wakes_wait_and_is_in_the_next_model_request() {
    let workspace = tempfile::tempdir().unwrap();
    let tracking = Tracking::new(vec![
        (
            "root",
            call(
                "child",
                "agent",
                json!({"prompt":"child task", "model":"child", "bg":true}),
            ),
        ),
        ("child", call("child-wait", "wait", json!({}))),
        ("root", call("parent-wait", "wait", json!({}))),
        ("child", answer()),
        ("root", answer()),
    ]);
    let harness = harness(workspace.path(), tracking.clone()).await;
    let session = Arc::new(harness.new_session().await.unwrap());
    let turn = prompt(&session);
    tracking.request(0).await;
    tracking.release(0);
    tracking.request(1).await;
    tracking.request(2).await;
    let child_job = running_job(&session, "agent").await;
    let child = session
        .runtime
        .agents
        .read()
        .unwrap()
        .keys()
        .find(|agent| **agent != session.root)
        .unwrap()
        .clone();
    tracking.release(1);
    bounded(async {
        loop {
            if session
                .runtime
                .jobs
                .list(&child)
                .await
                .iter()
                .any(|job| job.tool == "wait" && job.state == JobState::Running)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    session
        .runtime
        .jobs
        .send(child_job, json!("parent-wake-marker"))
        .await
        .unwrap();
    let next = tracking.request(3).await;
    assert_reason(&next, "child-wait", "event");
    assert_eq!(
        serde_json::to_string(&next.messages)
            .unwrap()
            .matches("parent-wake-marker")
            .count(),
        1
    );
    tracking.release(2);
    running_job(&session, "wait").await;
    tracking.release(3);
    tracking.request(4).await;
    tracking.release(4);
    bounded(turn).await.unwrap().unwrap();
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn wait_schema_only_accepts_optional_positive_integer_timeout() {
    let workspace = tempfile::tempdir().unwrap();
    let harness = harness(workspace.path(), Tracking::new(vec![])).await;
    let session = Arc::new(harness.new_session().await.unwrap());
    let surface = session
        .tools()
        .surface(&session.runtime.harness.capabilities);
    let wait = surface.get("wait").unwrap();
    let properties = wait.input_schema["properties"].as_object().unwrap();
    assert_eq!(
        properties.keys().map(String::as_str).collect::<Vec<_>>(),
        ["timeout"]
    );
    for valid in [
        json!({}),
        json!({"timeout":null}),
        json!({"timeout":1}),
        json!({"timeout":2}),
    ] {
        surface.validate_arguments("wait", &valid).unwrap();
    }
    assert_eq!(properties["timeout"]["minimum"], 1);
    // The surface checks field names; deserialization and the handler enforce types/bounds.
    for invalid in [
        json!({"timeout":0}),
        json!({"timeout":0.125}),
        json!({"timeout":-1}),
        json!({"timeout":1e300}),
        json!({"timeout":"1"}),
        json!({"timeout":true}),
        json!({"bg":true}),
        json!({"bg":false}),
        json!({"job":1}),
        json!({"wait":1}),
    ] {
        let result = bounded(session.runtime.executor.execute(
            session.root.clone(),
            "wait",
            invalid.clone(),
            None,
        ))
        .await;
        assert!(result.is_err(), "accepted {invalid}");
    }
    assert!(
        !surface.get("job_output").unwrap().input_schema["properties"]
            .as_object()
            .unwrap()
            .contains_key("wait")
    );
    assert!(!wait.description.contains("100"));
    assert!(!wait.description.contains("debounc"));
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn native_script_wait_supports_direct_and_builder_calls_without_self_wake() {
    let workspace = tempfile::tempdir().unwrap();
    let harness = harness(workspace.path(), Tracking::new(vec![])).await;
    let session = Arc::new(harness.new_session().await.unwrap());
    let output = bounded(session.run_script(
        "const direct = await tool.wait({timeout:1}); const builder = await tool.wait().timeout(1); return [direct, builder];"
    )).await.unwrap();
    assert_eq!(
        output.value,
        json!({"value":[{"reason":"timeout"}, {"reason":"timeout"}], "console":""})
    );
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancelling_an_indefinite_wait_unblocks_the_agent() {
    let workspace = tempfile::tempdir().unwrap();
    let tracking = Tracking::new(vec![
        ("root", call("waiting", "wait", json!({}))),
        ("root", answer()),
    ]);
    let harness = harness(workspace.path(), tracking.clone()).await;
    let session = Arc::new(harness.new_session().await.unwrap());
    let turn = prompt(&session);
    tracking.request(0).await;
    tracking.release(0);
    let job = running_job(&session, "wait").await;
    session.cancel_job(job).await.unwrap();
    let next = tracking.request(1).await;
    let text = serde_json::to_string(&next.messages).unwrap();
    assert!(text.contains("cancel"), "{text}");
    assert_eq!(
        session.runtime.jobs.snapshot(job).await.unwrap().state,
        JobState::Cancelled
    );
    tracking.release(1);
    bounded(turn).await.unwrap().unwrap();
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn child_completion_wakes_parent_and_injects_output_once() {
    let workspace = tempfile::tempdir().unwrap();
    let tracking = Tracking::new(vec![
        (
            "root",
            call(
                "child",
                "agent",
                json!({"prompt":"child task", "model":"child", "bg":true}),
            ),
        ),
        (
            "child",
            AssistantContent::text("child-answer", 0, "child-output-marker".to_owned()),
        ),
        ("root", call("waiting", "wait", json!({}))),
        ("root", call("again", "wait", json!({"timeout":1}))),
        ("root", answer()),
    ]);
    let harness = harness(workspace.path(), tracking.clone()).await;
    let session = Arc::new(harness.new_session().await.unwrap());
    let turn = prompt(&session);
    tracking.request(0).await;
    tracking.release(0);
    tracking.request(1).await;
    tracking.request(2).await;
    let child_job = running_job(&session, "agent").await;
    tracking.release(2);
    running_job(&session, "wait").await;
    tracking.release(1);
    let next = tracking.request(3).await;
    assert_reason(&next, "waiting", "event");
    let notifications = events(&next);
    assert_eq!(notifications.len(), 1);
    assert_eq!(notifications[0]["id"], child_job.get());
    assert!(
        serde_json::to_string(&notifications[0])
            .unwrap()
            .contains("child-output-marker")
    );
    tracking.release(3);
    let again = tracking.request(4).await;
    assert_reason(&again, "again", "timeout");
    assert_eq!(events(&again), notifications);
    tracking.release(4);
    bounded(turn).await.unwrap().unwrap();
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn idle_agent_batches_background_notifications_without_a_wait_call() {
    let workspace = tempfile::tempdir().unwrap();
    let tracking = Tracking::new(vec![
        ("root", answer()),
        ("root", answer()),
        ("root", answer()),
    ]);
    let harness = harness(workspace.path(), tracking.clone()).await;
    let session = Arc::new(harness.new_session().await.unwrap());
    let initial = prompt(&session);
    tracking.request(0).await;
    tracking.release(0);
    bounded(initial).await.unwrap().unwrap();

    let first = complete_background(&session, "idle-first-output").await;
    let second = complete_background(&session, "idle-second-output").await;
    let next = tracking.request(1).await;
    let notifications = events(&next);
    assert_eq!(
        notifications.len(),
        2,
        "idle completion batching must not require calling wait"
    );
    for (id, value) in [(first, "idle-first-output"), (second, "idle-second-output")] {
        let notification = notifications
            .iter()
            .find(|event| event["id"] == id.get())
            .unwrap();
        assert!(serde_json::to_string(notification).unwrap().contains(value));
    }

    let barrier = tokio::spawn({
        let session = session.clone();
        async move { session.prompt("idle-batch-barrier").await }
    });
    tracking.release(1);
    let final_request = tracking.request(2).await;
    assert!(
        serde_json::to_string(&final_request.messages)
            .unwrap()
            .contains("idle-batch-barrier")
    );
    assert_eq!(
        events(&final_request),
        notifications,
        "completed output must not be injected twice"
    );
    tracking.release(2);
    bounded(barrier).await.unwrap().unwrap();
    session.shutdown().await.unwrap();
}
