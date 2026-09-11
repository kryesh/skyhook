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
    content: Vec<AssistantContent>,
    gate: Semaphore,
}

struct Tracking {
    steps: Vec<Step>,
    requests: StdMutex<Vec<(usize, ModelRequest)>>,
    changed: Notify,
}

impl Tracking {
    fn new(steps: Vec<(&'static str, AssistantContent)>) -> Arc<Self> {
        Self::responses(
            steps
                .into_iter()
                .map(|(model, content)| (model, vec![content]))
                .collect(),
        )
    }

    fn responses(steps: Vec<(&'static str, Vec<AssistantContent>)>) -> Arc<Self> {
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
            let stop_reason = if content.iter().any(|item| item.kind == ItemKind::ToolCall) {
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
            };
            let mut events = events_for_content(&content);
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
    complete_background_for(session, &session.root, value).await
}

async fn complete_background_for(session: &SessionHandle, owner: &AgentId, value: &str) -> JobId {
    let lease = session
        .runtime
        .jobs
        .create(JobSpec {
            background: true,
            ..JobSpec::test(owner.clone(), "wait-fixture")
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
    runtime_entries(request, "skyhook_job_events")
        .into_iter()
        .filter(|entry| entry["kind"] != "message")
        .collect()
}

fn agent_messages(request: &ModelRequest) -> Vec<Value> {
    assert!(
        runtime_entries(request, "skyhook_agent_messages").is_empty(),
        "new child messages must share the job event envelope"
    );
    runtime_entries(request, "skyhook_job_events")
        .into_iter()
        .filter(|entry| entry["kind"] == "message")
        .collect()
}

fn runtime_entries(request: &ModelRequest, tag: &str) -> Vec<Value> {
    let prefix = format!("<{tag}>\n");
    let suffix = format!("\n</{tag}>");
    request
        .messages
        .iter()
        .flat_map(|message| match message {
            Message::User(content) => content
                .iter()
                .filter_map(|block| match block {
                    UserContent::Runtime { text } => text
                        .strip_prefix(&prefix)
                        .and_then(|text| text.strip_suffix(&suffix))
                        .map(|text| serde_json::from_str::<Vec<Value>>(text).unwrap()),
                    _ => None,
                })
                .flatten()
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .collect()
}

async fn child_completed(session: &SessionHandle, job: JobId) {
    bounded(async {
        loop {
            let state = session.runtime.jobs.snapshot(job).await.unwrap().state;
            if state.is_terminal() {
                assert_eq!(state, JobState::Completed);
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
}

fn assert_child_completion(event: &Value, message: &Value) {
    assert_eq!(event["id"], message["id"]);
    assert_eq!(event["state"], "completed");
    assert_eq!(event["last_message"], message["message"]);
    assert!(
        event.get("result").is_none(),
        "completion must not repeat child text: {event}"
    );
    assert!(
        event.get("text").is_none(),
        "completion must reference the message, not copy it"
    );
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
    tracking.release(3);
    child_completed(&session, child_job).await;
    tracking.release(2);
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
    // Keep the parent invoke gated until both independently published events
    // are pending; neither event is required to coalesce with the other.
    tracking.release(1);
    child_completed(&session, child_job).await;
    tracking.release(2);
    let next = tracking.request(3).await;
    assert_reason(&next, "waiting", "event");
    let notifications = events(&next);
    assert_eq!(notifications.len(), 1);
    assert_eq!(notifications[0]["id"], child_job.get());
    let messages = agent_messages(&next);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["text"], "child-output-marker");
    assert!(messages[0]["message"].is_u64());
    assert_child_completion(&notifications[0], &messages[0]);
    assert_eq!(
        session
            .runtime
            .jobs
            .snapshot(child_job)
            .await
            .unwrap()
            .output,
        Some(json!("child-output-marker")),
        "saved job output remains the final response"
    );
    tracking.release(3);
    let again = tracking.request(4).await;
    assert_reason(&again, "again", "timeout");
    assert_eq!(events(&again), notifications);
    assert_eq!(agent_messages(&again), messages);
    tracking.release(4);
    bounded(turn).await.unwrap().unwrap();
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn no_tool_child_reports_survive_queued_input_and_wake_parent_independently() {
    no_tool_child_reports(true, false).await;
}

#[tokio::test]
async fn pending_no_tool_child_reports_survive_queued_input_at_request_boundary() {
    no_tool_child_reports(false, false).await;
}

#[tokio::test]
async fn no_tool_child_reports_survive_pending_runtime_event() {
    no_tool_child_reports(false, true).await;
}

async fn no_tool_child_reports(parent_already_waiting: bool, pending_runtime_event: bool) {
    const A: &str = "child-report-A";
    const B: &str = "child-addendum-B";
    let workspace = tempfile::tempdir().unwrap();
    let tracking = Tracking::new(vec![
        (
            "root",
            call(
                "launch",
                "agent",
                json!({
                    "prompt":"report", "model":"child", "name":"reporter", "bg":true
                }),
            ),
        ),
        ("child", AssistantContent::text("report", 0, A)),
        ("root", call("first-report", "wait", json!({}))),
        ("child", AssistantContent::text("addendum", 0, B)),
        ("root", call("no-repeat", "wait", json!({"timeout":1}))),
        ("root", call("last-report", "wait", json!({}))),
        ("root", answer()),
    ]);
    let harness = harness(workspace.path(), tracking.clone()).await;
    let session = Arc::new(harness.new_session().await.unwrap());
    let turn = prompt(&session);
    tracking.request(0).await;
    tracking.release(0);
    tracking.request(1).await;
    tracking.request(2).await;
    let job = running_job(&session, "agent").await;
    let (child, sender) = {
        let agents = session.runtime.agents.read().unwrap();
        let (child, slot) = agents.iter().find(|(id, _)| **id != session.root).unwrap();
        (child.clone(), slot.sender.clone())
    };
    if pending_runtime_event {
        complete_background_for(&session, &child, "please add an addendum").await;
        assert!(session.runtime.jobs.has_pending(&child).await);
    } else {
        let sent = bounded(session.run_script(format!(
            "return await tool.job({}).send({{value:'please add an addendum'}});",
            job.get()
        )))
        .await
        .unwrap();
        assert_eq!(sent.value["value"], json!({"accepted":true}));
        // send() acknowledges the job mailbox; occupancy proves forwarding to the
        // child's command queue while A's no-tool provider invoke is still gated.
        bounded(async {
            while sender.capacity() != AGENT_CHANNEL_CAPACITY - 1 {
                tokio::task::yield_now().await;
            }
        })
        .await;
    }
    if parent_already_waiting {
        tracking.release(2);
        running_job(&session, "wait").await;
    }
    tracking.release(1);
    let addendum = tracking.request(3).await;
    assert_eq!(
        serde_json::to_string(&addendum.messages)
            .unwrap()
            .matches("please add an addendum")
            .count(),
        1
    );
    assert!(addendum.messages.iter().any(|message| matches!(message,
        Message::Assistant(content) if content == &tracking.steps[1].content)));
    if !parent_already_waiting {
        assert!(
            tracking
                .requests
                .lock()
                .unwrap()
                .iter()
                .all(|(step, _)| *step < 4)
        );
        tracking.release(2);
    }
    let first = tracking.request(4).await;
    assert_reason(&first, "first-report", "event");
    let messages = agent_messages(&first);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["text"], A);
    assert_eq!(messages[0]["id"], job.get());
    assert_eq!(messages[0]["name"], "reporter");
    assert!(
        events(&first).is_empty(),
        "A must not complete the still-working child"
    );
    assert_eq!(
        session.runtime.jobs.snapshot(job).await.unwrap().state,
        JobState::Running
    );
    assert!(
        !turn.is_finished(),
        "parent must not finish before addendum work"
    );
    tracking.release(4);
    let waiting = tracking.request(5).await;
    assert_reason(&waiting, "no-repeat", "timeout");
    assert_eq!(agent_messages(&waiting), messages);
    assert!(events(&waiting).is_empty());

    // Hold this parent boundary until B and completion are both durable. They
    // remain separate events, but need not wake two separate model requests.
    tracking.release(3);
    child_completed(&session, job).await;
    tracking.release(5);
    let completed = tracking.request(6).await;
    assert_reason(&completed, "last-report", "event");
    let delivered = agent_messages(&completed);
    assert_eq!(delivered.len(), 2);
    assert_eq!(delivered[0], messages[0]);
    assert_eq!(delivered[1]["text"], B);
    let records = session.runtime.store.records().await;
    for (index, step) in [1, 3].into_iter().enumerate() {
        let source = records
            .iter()
            .find(|record| {
                record.agent == child
                    && matches!(
            &record.event, SessionEvent::MessageCommitted { message: Message::Assistant(content) }
                if content == &tracking.steps[step].content)
            })
            .unwrap();
        assert_eq!(delivered[index]["message"], source.sequence);
    }
    assert_ne!(delivered[0]["message"], delivered[1]["message"]);
    let lifecycle = events(&completed);
    assert_eq!(lifecycle.len(), 1);
    assert_child_completion(&lifecycle[0], &delivered[1]);
    let history = serde_json::to_string(&completed.messages).unwrap();
    assert_eq!(
        history.matches(A).count(),
        1,
        "A must not be overwritten or repeated"
    );
    assert_eq!(
        history.matches(B).count(),
        1,
        "completion must not repeat B"
    );
    assert_eq!(
        session.runtime.jobs.snapshot(job).await.unwrap().output,
        Some(json!(B))
    );
    let saved =
        bounded(session.run_script(format!("return await tool.job({}).output();", job.get())))
            .await
            .unwrap();
    assert_eq!(
        saved.value["value"]["result"], B,
        "job_output retains final string"
    );
    tracking.release(6);
    bounded(turn).await.unwrap().unwrap();
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn intermediate_child_reply_wakes_parent_and_is_injected_once() {
    intermediate_child_reply(true).await;
}

#[tokio::test]
async fn intermediate_child_reply_before_wait_is_delivered_at_request_boundary() {
    intermediate_child_reply(false).await;
}

async fn intermediate_child_reply(parent_already_waiting: bool) {
    const REPLY: &str = "intermediate-child-reply-marker";
    const FINAL: &str = "final-child-output-marker";
    let workspace = tempfile::tempdir().unwrap();
    let mut child_tool = call("continue-child", "script", json!({"source":"return 42;"}));
    child_tool.position = 1;
    let reply = vec![AssistantContent::text("child-reply", 0, REPLY), child_tool];
    let tracking = Tracking::responses(vec![
        (
            "root",
            vec![call(
                "child",
                "agent",
                json!({"prompt":"child task", "model":"child", "name":"child-replier", "bg":true}),
            )],
        ),
        ("child", vec![call("child-wait", "wait", json!({}))]),
        ("root", vec![call("waiting", "wait", json!({}))]),
        ("child", reply.clone()),
        (
            "child",
            vec![AssistantContent::text("child-final", 0, FINAL)],
        ),
        ("root", vec![call("again", "wait", json!({"timeout":1}))]),
        ("root", vec![call("finish-wait", "wait", json!({}))]),
        (
            "root",
            vec![call("after-final", "wait", json!({"timeout":1}))],
        ),
        ("root", vec![answer()]),
    ]);
    let harness = harness(workspace.path(), tracking.clone()).await;
    let session = Arc::new(harness.new_session().await.unwrap());
    let turn = prompt(&session);
    tracking.request(0).await;
    tracking.release(0);
    tracking.request(1).await;
    let in_flight = tracking.request(2).await;
    assert!(agent_messages(&in_flight).is_empty());
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
    if parent_already_waiting {
        tracking.release(2);
        running_job(&session, "wait").await;
    }

    // Send through the public job API, not directly into the agent's queue.
    // The child's next request proves that the input actually reached it.
    let sent = bounded(session.run_script(format!(
        "return await tool.job({}).send({{value:\"parent-reply-request-marker\"}});",
        child_job.get()
    )))
    .await
    .unwrap();
    assert_eq!(sent.value["value"], json!({"accepted":true}));
    let child_request = tracking.request(3).await;
    assert_reason(&child_request, "child-wait", "event");
    assert_eq!(
        serde_json::to_string(&child_request.messages)
            .unwrap()
            .matches("parent-reply-request-marker")
            .count(),
        1
    );
    tracking.release(3);
    // The child has committed text WITH a tool call and reached another invoke.
    // Keep that final response gated until after the parent has read the reply.
    tracking.request(4).await;
    assert_eq!(
        session
            .runtime
            .jobs
            .snapshot(child_job)
            .await
            .unwrap()
            .state,
        JobState::Running
    );
    if !parent_already_waiting {
        // No new parent request may start while its current invoke is gated.
        assert!(
            tracking
                .requests
                .lock()
                .unwrap()
                .iter()
                .all(|(step, _)| *step < 5)
        );
        tracking.release(2);
    }
    let next = tracking.request(5).await;
    assert_reason(&next, "waiting", "event");
    assert_eq!(
        session
            .runtime
            .jobs
            .snapshot(child_job)
            .await
            .unwrap()
            .state,
        JobState::Running,
        "the reply must not depend on child completion"
    );
    assert!(events(&next).is_empty(), "a reply is not a job completion");
    let records = session.runtime.store.records().await;
    let committed = records
        .iter()
        .filter(|record| {
            record.agent == child
                && matches!(&record.event,
                    SessionEvent::MessageCommitted { message: Message::Assistant(content) }
                        if content == &reply)
        })
        .collect::<Vec<_>>();
    assert_eq!(committed.len(), 1);
    let replies = agent_messages(&next);
    assert_eq!(
        replies,
        vec![json!({
            "kind":"message",
            "id":child_job.get(),
            "name":"child-replier",
            "message":committed[0].sequence,
            "text":REPLY,
        })]
    );
    assert_eq!(
        serde_json::to_string(&next.messages)
            .unwrap()
            .matches(REPLY)
            .count(),
        1,
        "only the runtime envelope should contain the reply"
    );

    tracking.release(5);
    let again = tracking.request(6).await;
    assert_reason(&again, "again", "timeout");
    assert_eq!(
        agent_messages(&again),
        replies,
        "do not inject another copy"
    );
    assert!(events(&again).is_empty());
    tracking.release(4);
    child_completed(&session, child_job).await;
    tracking.release(6);
    let completed = tracking.request(7).await;
    assert_reason(&completed, "finish-wait", "event");
    let all_replies = agent_messages(&completed);
    assert_eq!(&all_replies[..replies.len()], replies.as_slice());
    assert_eq!(all_replies.len(), 2);
    assert_eq!(all_replies[1]["text"], FINAL);
    let notifications = events(&completed);
    assert_eq!(notifications.len(), 1);
    assert_child_completion(&notifications[0], &all_replies[1]);
    let snapshot = session.runtime.jobs.snapshot(child_job).await.unwrap();
    assert_eq!(snapshot.state, JobState::Completed);
    let output = serde_json::to_string(&snapshot.output).unwrap();
    assert!(output.contains(FINAL));
    assert!(
        !output.contains(REPLY),
        "saved final output repeated the reply"
    );

    tracking.release(7);
    let last = tracking.request(8).await;
    assert_reason(&last, "after-final", "timeout");
    assert_eq!(agent_messages(&last), all_replies);
    assert_eq!(events(&last), notifications);
    tracking.release(8);
    bounded(turn).await.unwrap().unwrap();
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn intermediate_child_reply_crosses_parent_no_tool_boundary() {
    child_replies_at_no_tool_boundary(1, false).await;
}

#[tokio::test]
async fn intermediate_child_replies_outlive_a_full_parent_mailbox() {
    child_replies_at_no_tool_boundary(AGENT_CHANNEL_CAPACITY + 1, true).await;
}

async fn child_replies_at_no_tool_boundary(reply_count: usize, fill_mailbox: bool) {
    let workspace = tempfile::tempdir().unwrap();
    let mut steps = vec![
        (
            "root",
            vec![call(
                "child",
                "agent",
                json!({"prompt":"child task", "model":"child", "bg":true}),
            )],
        ),
        // Unlike wait, this response has no tool call and can end the parent turn.
        ("root", vec![answer()]),
    ];
    for index in 0..reply_count {
        let mut tool = call(
            &format!("child-tool-{index}"),
            "script",
            json!({"source":"return 42;"}),
        );
        tool.position = 1;
        steps.push((
            "child",
            vec![
                AssistantContent::text(
                    format!("child-reply-{index}"),
                    0,
                    format!("mailbox-child-reply-{index}"),
                ),
                tool,
            ],
        ));
    }
    let child_final = steps.len();
    steps.push((
        "child",
        vec![AssistantContent::text(
            "child-final",
            0,
            "mailbox-child-final",
        )],
    ));
    let parent_reply = steps.len();
    steps.push(("root", vec![call("finish-wait", "wait", json!({}))]));
    let parent_completed = steps.len();
    steps.push(("root", vec![answer()]));
    let tracking = Tracking::responses(steps);
    let harness = harness(workspace.path(), tracking.clone()).await;
    let session = Arc::new(harness.new_session().await.unwrap());
    let turn = prompt(&session);
    tracking.request(0).await;
    tracking.release(0);
    tracking.request(1).await;
    let child_job = running_job(&session, "agent").await;
    if fill_mailbox {
        // Fill the command channel while the parent is inside invoke. Durable
        // message events must remain pending even when JobsReady cannot fit.
        while session.root_tx.capacity() > 0 {
            bounded(session.root_tx.send(AgentCommand::JobsReady))
                .await
                .unwrap();
        }
        assert_eq!(session.root_tx.capacity(), 0);
    }
    for step in 2..child_final {
        tracking.request(step).await;
        tracking.release(step);
    }
    // More replies than the entire channel capacity have been published without
    // allowing the parent to drain anything. The child must not deadlock here.
    tracking.request(child_final).await;
    assert_eq!(
        session
            .runtime
            .jobs
            .snapshot(child_job)
            .await
            .unwrap()
            .state,
        JobState::Running
    );
    assert!(
        tracking
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|(step, _)| *step < parent_reply)
    );
    if fill_mailbox {
        assert_eq!(session.root_tx.capacity(), 0);
    }
    tracking.release(1);
    let next = tracking.request(parent_reply).await;
    assert_eq!(
        session
            .runtime
            .jobs
            .snapshot(child_job)
            .await
            .unwrap()
            .state,
        JobState::Running,
        "a no-tool parent response must not strand replies until child completion"
    );
    assert!(events(&next).is_empty());
    let replies = agent_messages(&next);
    assert_eq!(
        replies.len(),
        reply_count,
        "a full mailbox must not drop payloads"
    );
    let records = session.runtime.store.records().await;
    for (index, reply) in replies.iter().enumerate() {
        let content = &tracking.steps[index + 2].content;
        let committed = records
            .iter()
            .find(|record| {
                record.agent != session.root
                    && matches!(&record.event,
                    SessionEvent::MessageCommitted { message: Message::Assistant(actual) }
                        if actual == content)
            })
            .unwrap();
        assert_eq!(reply["id"], child_job.get());
        assert_eq!(reply["message"], committed.sequence);
        assert_eq!(reply["text"], format!("mailbox-child-reply-{index}"));
        assert!(
            reply.get("name").is_none(),
            "unnamed child should omit name"
        );
    }
    tracking.release(child_final);
    child_completed(&session, child_job).await;
    tracking.release(parent_reply);
    let completed = tracking.request(parent_completed).await;
    assert_reason(&completed, "finish-wait", "event");
    let all_replies = agent_messages(&completed);
    assert_eq!(
        &all_replies[..reply_count],
        replies.as_slice(),
        "retained replies are exactly once"
    );
    assert_eq!(all_replies.len(), reply_count + 1);
    assert_eq!(all_replies[reply_count]["text"], "mailbox-child-final");
    let notifications = events(&completed);
    assert_eq!(notifications.len(), 1);
    assert_child_completion(&notifications[0], &all_replies[reply_count]);
    tracking.release(parent_completed);
    bounded(turn).await.unwrap().unwrap();
    bounded(session.shutdown()).await.unwrap();
    bounded(session.root_tx.closed()).await;
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

/// Publish through the same durable source-history path as a real child, without
/// invoking another provider. The owner stays gated while a test prepares receipts.
async fn fixture_child(session: &SessionHandle, root: &Path) -> (JobId, AgentId) {
    let lease = session
        .runtime
        .jobs
        .create(JobSpec {
            background: true,
            ..JobSpec::test(session.root.clone(), "agent")
        })
        .await
        .unwrap();
    let child = session.root.child(123);
    session
        .runtime
        .store
        .append(
            child.clone(),
            SessionEvent::AgentStarted {
                parent: Some(session.root.clone()),
                owner_job: Some(lease.id),
                model_profile: "child".into(),
                max_context: None,
                location: crate::execution::ExecutionLocation::root(root.to_path_buf()),
            },
        )
        .await
        .unwrap();
    session
        .runtime
        .jobs
        .transition(lease.id, JobState::Running)
        .await
        .unwrap();
    (lease.id, child)
}

async fn fixture_message(session: &SessionHandle, job: JobId, child: &AgentId, text: &str) -> u64 {
    session
        .runtime
        .jobs
        .commit_child_message(
            child,
            job,
            Message::Assistant(vec![AssistantContent::text("fixture-reply", 0, text)]),
            text.to_owned(),
        )
        .await
        .unwrap()
}

async fn pending_content(
    session: &SessionHandle,
    root: &Path,
) -> (Vec<UserContent>, wait::PendingEventBatch) {
    session
        .runtime
        .pending_event_content(
            session.root_agent(),
            &session.runtime.harness.capabilities,
            &crate::execution::ExecutionLocation::root(root.to_path_buf()),
        )
        .await
        .unwrap()
}

/// Snapshotting must not consume the only durable copy of a child's progress.
#[tokio::test]
async fn child_progress_snapshot_survives_abandoned_parent_commits() {
    let workspace = tempfile::tempdir().unwrap();
    let tracking = Tracking::new(vec![("root", answer()), ("root", answer())]);
    let harness = harness(workspace.path(), tracking.clone()).await;
    let session = Arc::new(harness.new_session().await.unwrap());
    let turn = prompt(&session);
    tracking.request(0).await;
    let (job, child) = fixture_child(&session, workspace.path()).await;
    let first = fixture_message(&session, job, &child, "retained-progress").await;
    let (content, batch) = pending_content(&session, workspace.path()).await;
    assert_eq!(content.len(), 1);
    drop(batch);
    assert!(session.runtime.jobs.has_pending(session.root_agent()).await);

    let (retry, batch) = pending_content(&session, workspace.path()).await;
    assert_eq!(
        retry, content,
        "abandoning a receipt must retain its source message"
    );
    batch
        .commit(&session.runtime, session.root_agent(), Message::User(retry))
        .await
        .unwrap();
    assert!(!session.runtime.jobs.has_pending(session.root_agent()).await);
    // Equal text is a new independent event when its source sequence differs.
    let second = fixture_message(&session, job, &child, "retained-progress").await;
    assert_ne!(first, second);
    let (remaining, batch) = pending_content(&session, workspace.path()).await;
    assert!(
        serde_json::to_string(&remaining)
            .unwrap()
            .contains("retained-progress")
    );
    drop(batch);
    tracking.release(0);
    let next = tracking.request(1).await;
    let delivered = agent_messages(&next);
    assert_eq!(delivered.len(), 2);
    assert_eq!(delivered[0]["message"], first);
    assert_eq!(delivered[1]["message"], second);
    assert!(!session.runtime.jobs.has_pending(session.root_agent()).await);
    tracking.release(1);
    bounded(turn).await.unwrap().unwrap();
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancelling_a_started_notification_commit_finishes_both_acknowledgments() {
    cancelled_notification_commit(true).await;
}

#[tokio::test]
async fn cancelling_a_started_child_only_commit_serializes_the_next_snapshot() {
    cancelled_notification_commit(false).await;
}

async fn cancelled_notification_commit(with_completion: bool) {
    let workspace = tempfile::tempdir().unwrap();
    let tracking = Tracking::new(vec![("root", answer()), ("root", answer())]);
    let harness = harness(workspace.path(), tracking.clone()).await;
    let session = Arc::new(harness.new_session().await.unwrap());
    let turn = prompt(&session);
    tracking.request(0).await;
    let completion = if with_completion {
        Some(complete_background(&session, "committed-completion").await)
    } else {
        None
    };
    let (job, child) = fixture_child(&session, workspace.path()).await;
    fixture_message(&session, job, &child, "committed-progress").await;
    let location = crate::execution::ExecutionLocation::root(workspace.path().to_path_buf());
    let (content, batch) = session
        .runtime
        .pending_event_content(
            session.root_agent(),
            &session.runtime.harness.capabilities,
            &location,
        )
        .await
        .unwrap();
    assert_eq!(
        content.len(),
        1,
        "progress and completion share the same envelope"
    );
    assert!(session.runtime.jobs.has_pending(session.root_agent()).await);
    let mut records = session.runtime.store.subscribe();
    {
        let commit = batch.commit(
            &session.runtime,
            session.root_agent(),
            Message::User(content),
        );
        tokio::pin!(commit);
        // This current-thread test has not yielded: the owned commit task has
        // been scheduled but cannot have run yet. Dropping its caller models an
        // interrupt exactly after this transaction's ownership boundary.
        assert!(futures_util::poll!(commit.as_mut()).is_pending());
    }
    {
        // An immediate resumed request cannot snapshot progress still owned by
        // the interrupted caller's transaction, even without any terminal job.
        let next_batch = session.runtime.pending_event_content(
            session.root_agent(),
            &session.runtime.harness.capabilities,
            &location,
        );
        tokio::pin!(next_batch);
        assert!(futures_util::poll!(next_batch.as_mut()).is_pending());
    }
    bounded(async {
        loop {
            let record = records.recv().await.unwrap();
            if matches!(
                record.event,
                SessionEvent::MessageCommitted {
                    message: Message::User(_)
                }
            ) {
                break;
            }
        }
        while session.runtime.jobs.has_pending(session.root_agent()).await {
            tokio::task::yield_now().await;
        }
    })
    .await;
    // Duplicate wake commands do not reconstruct already acknowledged output.
    session.root_tx.jobs_ready();
    session.root_tx.jobs_ready();
    tracking.release(0);
    bounded(turn).await.unwrap().unwrap();
    let turn = prompt(&session);
    let next = tracking.request(1).await;
    assert_eq!(agent_messages(&next).len(), 1);
    assert_eq!(events(&next).len(), usize::from(with_completion));
    if with_completion {
        assert_eq!(events(&next)[0]["id"], completion.unwrap().get());
    }
    tracking.release(1);
    bounded(turn).await.unwrap().unwrap();
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn progress_and_completion_cross_the_same_no_tool_boundary_once() {
    let workspace = tempfile::tempdir().unwrap();
    let tracking = Tracking::new(vec![("root", answer()), ("root", answer())]);
    let harness = harness(workspace.path(), tracking.clone()).await;
    let session = Arc::new(harness.new_session().await.unwrap());
    let turn = prompt(&session);
    tracking.request(0).await;
    complete_background(&session, "no-tool-completion").await;
    let (job, child) = fixture_child(&session, workspace.path()).await;
    fixture_message(&session, job, &child, "no-tool-progress").await;
    tracking.release(0);
    let next = tracking.request(1).await;
    assert_eq!(agent_messages(&next).len(), 1);
    assert_eq!(events(&next).len(), 1);
    // Both entries must be in a single persisted parent runtime envelope, not
    // separate history/ack transactions or a follow-on idle turn.
    let records = session.runtime.store.records().await;
    assert!(records.iter().any(|record| {
        record.agent == session.root && matches!(&record.event,
            SessionEvent::MessageCommitted { message: Message::User(content) }
                if content.len() == 1 && content.iter().all(|block| matches!(block, UserContent::Runtime { .. })))
    }));
    assert!(
        !turn.is_finished(),
        "the earlier answer must not finish the caller's turn"
    );
    assert!(!session.runtime.jobs.has_pending(session.root_agent()).await);
    tracking.release(1);
    bounded(turn).await.unwrap().unwrap();
    session.shutdown().await.unwrap();
}
