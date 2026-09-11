//! Agent-facing event waiting, separate from saved-output inspection.

use super::{AgentCommand, SessionRuntime};

mod delivery;
mod receipt;
use crate::tool::{ToolContext, ToolError};
pub(super) use receipt::PendingEventBatch;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::{mpsc, watch};

/// Input and background notifications use the same per-agent delivery gate.
#[derive(Clone)]
pub(super) struct AgentSender {
    sender: mpsc::Sender<AgentCommand>,
    wake: Arc<AgentWake>,
}

struct AgentWake {
    revision: watch::Sender<u64>,
    batch: std::sync::Mutex<EventBatch>,
    ready_input_revision: AtomicU64,
    observed_input: AtomicU64,
    observed: AtomicU64,
}

#[derive(Default)]
struct EventBatch {
    revision: u64,
    input_revision: u64,
    scheduled: bool,
}

impl AgentSender {
    pub(super) fn new(sender: mpsc::Sender<AgentCommand>) -> Self {
        Self {
            sender,
            wake: Arc::new(AgentWake {
                revision: watch::channel(0).0,
                batch: std::sync::Mutex::new(EventBatch::default()),
                ready_input_revision: AtomicU64::new(0),
                observed_input: AtomicU64::new(0),
                observed: AtomicU64::new(0),
            }),
        }
    }

    pub(super) async fn send(
        &self,
        command: AgentCommand,
    ) -> Result<(), mpsc::error::SendError<AgentCommand>> {
        let notify = !matches!(command, AgentCommand::JobsReady);
        let permit = match self.sender.reserve().await {
            Ok(permit) => permit,
            Err(_) => return Err(mpsc::error::SendError(command)),
        };
        // Schedule before publication; no await separates these operations.
        if notify {
            self.schedule(true);
        }
        permit.send(command);
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn capacity(&self) -> usize {
        self.sender.capacity()
    }

    pub(super) async fn closed(&self) {
        self.sender.closed().await
    }

    pub(super) fn jobs_ready(&self) {
        self.schedule(false);
        // A full mailbox already guarantees another request boundary. Never
        // stall notification of other agents behind this one's mailbox.
        let _ = self.sender.try_send(AgentCommand::JobsReady);
    }

    /// One bounded coalescing window per agent, shared by every delivery path.
    /// Continuous activity cannot postpone an already scheduled batch.
    fn schedule(&self, input: bool) {
        let mut batch = self
            .wake
            .batch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        batch.revision = batch.revision.wrapping_add(1);
        if input {
            batch.input_revision = batch.input_revision.wrapping_add(1);
        }
        if batch.scheduled {
            return;
        }
        batch.scheduled = true;
        let wake = Arc::downgrade(&self.wake);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let Some(wake) = wake.upgrade() else { return };
            let mut batch = wake
                .batch
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            wake.ready_input_revision
                .store(batch.input_revision, Ordering::Release);
            batch.scheduled = false;
            // Publish under the lock before another window can begin.
            wake.revision.send_replace(batch.revision);
        });
    }

    pub(super) async fn flush_events(
        &self,
        cancellation: &crate::job::CancellationToken,
    ) -> Result<(), super::HarnessError> {
        let mut revision = self.wake.revision.subscribe();
        let requested = self
            .wake
            .batch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .revision;
        loop {
            if cancellation.is_cancelled() {
                return Err(super::HarnessError::Interrupted);
            }
            if *revision.borrow_and_update() >= requested {
                return Ok(());
            }
            tokio::select! {
                biased;
                () = cancellation.cancelled() => return Err(super::HarnessError::Interrupted),
                result = revision.changed() => {
                    if result.is_err() { return Err(super::HarnessError::Interrupted); }
                }
            }
        }
    }

    pub(super) fn begin_request(&self) {
        self.wake
            .observed
            .store(*self.wake.revision.borrow(), Ordering::Release);
        self.wake.observed_input.store(
            self.wake.ready_input_revision.load(Ordering::Acquire),
            Ordering::Release,
        );
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct WaitArgs {
    /// Maximum seconds to wait, as a positive integer. Omitted/null waits indefinitely.
    #[schemars(range(min = 1))]
    pub(super) timeout: Option<u64>,
}

#[derive(Serialize, JsonSchema, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(super) enum WakeReason {
    Event,
    Timeout,
}

#[derive(Serialize, JsonSchema, Debug, PartialEq)]
pub(super) struct WaitOutput {
    pub(super) reason: WakeReason,
}

impl SessionRuntime {
    pub(super) async fn wait_for_event(
        &self,
        context: &ToolContext,
        args: WaitArgs,
    ) -> Result<WaitOutput, ToolError> {
        let deadline = args
            .timeout
            .map(|seconds| {
                if seconds == 0 {
                    return Err(ToolError::InvalidArguments(
                        "timeout must be a positive integer".into(),
                    ));
                }
                tokio::time::Instant::now()
                    .checked_add(std::time::Duration::from_secs(seconds))
                    .ok_or_else(|| ToolError::InvalidArguments("timeout is too large".into()))
            })
            .transpose()?;
        let sender = self
            .agent_sender(&context.agent)
            .ok_or_else(|| ToolError::Failed("calling agent is not active".into()))?;
        let mut revision = sender.wake.revision.subscribe();
        loop {
            if context.is_cancelled() {
                return Err(ToolError::Cancelled);
            }
            // Only released batches are visible. A stale completion signal
            // must not count as an event if its output was already claimed.
            let released = *revision.borrow_and_update();
            if sender.wake.ready_input_revision.load(Ordering::Acquire)
                != sender.wake.observed_input.load(Ordering::Acquire)
                || (released != sender.wake.observed.load(Ordering::Acquire)
                    && self.jobs.has_pending(&context.agent).await)
            {
                return Ok(WaitOutput {
                    reason: WakeReason::Event,
                });
            }
            tokio::select! {
                biased;
                () = context.cancelled() => return Err(ToolError::Cancelled),
                changed = revision.changed() => {
                    if changed.is_err() { return Err(ToolError::Cancelled); }
                }
                () = async {
                    match deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending().await,
                    }
                } => return Ok(WaitOutput { reason: WakeReason::Timeout }),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        sync::{Arc, Mutex as StdMutex},
        time::Duration,
    };

    use serde_json::Value;
    use tokio::sync::{Notify, Semaphore};

    use super::super::*;
    use crate::{
        job::{JobOutcome, JobSpec, JobState},
        provider::{
            ProviderContext, ProviderError, ProviderFuture, ResponseStream,
            protocol::{ItemKind, StopReason, events_for_content},
        },
        tool::ToolOutput,
    };

    pub(super) struct Step {
        model: &'static str,
        pub(super) content: Vec<AssistantContent>,
        gate: Semaphore,
    }

    pub(super) struct Tracking {
        pub(super) steps: Vec<Step>,
        pub(super) requests: StdMutex<Vec<(usize, ModelRequest)>>,
        changed: Notify,
    }

    impl Tracking {
        pub(super) fn new(steps: Vec<(&'static str, AssistantContent)>) -> Arc<Self> {
            Self::responses(
                steps
                    .into_iter()
                    .map(|(model, content)| (model, vec![content]))
                    .collect(),
            )
        }

        pub(super) fn responses(steps: Vec<(&'static str, Vec<AssistantContent>)>) -> Arc<Self> {
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

        pub(super) async fn request(&self, step: usize) -> ModelRequest {
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

        pub(super) fn release(&self, step: usize) {
            self.steps[step].gate.add_permits(1);
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
            let step = {
                let mut requests = tracking.requests.lock().unwrap();
                let step = tracking
                    .steps
                    .iter()
                    .enumerate()
                    .position(|(index, step)| {
                        step.model == request.model
                            && !requests.iter().any(|(seen, _)| *seen == index)
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
                Ok(
                    Box::pin(futures_util::stream::iter(events.into_iter().map(Ok)))
                        as ResponseStream,
                )
            })
        }
    }

    pub(super) fn call(id: &str, name: &str, arguments: Value) -> AssistantContent {
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

    pub(super) fn answer() -> AssistantContent {
        AssistantContent::text("answer", 0, "done".to_owned())
    }

    pub(super) async fn bounded<T>(future: impl Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(10), future)
            .await
            .expect("wait test synchronization timed out")
    }

    pub(super) async fn harness(root: &Path, tracking: Arc<Tracking>) -> Harness {
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

    pub(super) fn prompt(
        session: &Arc<SessionHandle>,
    ) -> tokio::task::JoinHandle<Result<String, HarnessError>> {
        let session = session.clone();
        tokio::spawn(async move { session.prompt("start").await })
    }

    pub(super) async fn running_job(session: &SessionHandle, tool: &str) -> JobId {
        let runtime = &session.runtime;
        bounded(async {
            loop {
                if let Some(job) = runtime
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

    pub(super) async fn complete_background(session: &SessionHandle, value: &str) -> JobId {
        complete_background_for(session, &session.root, value).await
    }

    pub(super) async fn complete_background_for(
        session: &SessionHandle,
        owner: &AgentId,
        value: &str,
    ) -> JobId {
        let runtime = &session.runtime;
        let lease = runtime
            .jobs
            .create(JobSpec {
                background: true,
                ..JobSpec::test(owner.clone(), "wait-fixture")
            })
            .await
            .unwrap();
        runtime
            .jobs
            .transition(lease.id, JobState::Running)
            .await
            .unwrap();
        runtime
            .jobs
            .finish(
                lease.id,
                JobOutcome::Completed(ToolOutput::new(json!({"value":value}))),
            )
            .await
            .unwrap();
        lease.id
    }

    pub(super) fn events(request: &ModelRequest) -> Vec<Value> {
        runtime_entries(request, "skyhook_job_events")
            .into_iter()
            .filter(|entry| entry["kind"] != "message")
            .collect()
    }

    pub(super) fn agent_messages(request: &ModelRequest) -> Vec<Value> {
        runtime_entries(request, "skyhook_job_events")
            .into_iter()
            .filter(|entry| entry["kind"] == "message")
            .collect()
    }

    pub(super) fn runtime_entries(request: &ModelRequest, tag: &str) -> Vec<Value> {
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

    pub(super) async fn child_completed(session: &SessionHandle, job: JobId) {
        let runtime = &session.runtime;
        bounded(async {
            loop {
                let state = runtime.jobs.snapshot(job).await.unwrap().state;
                if state.is_terminal() {
                    assert_eq!(state, JobState::Completed);
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
    }

    pub(super) fn assert_child_completion(event: &Value, message: &Value) {
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

    pub(super) fn assert_reason(request: &ModelRequest, id: &str, reason: &str) {
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
        let runtime = &session.runtime;
        let turn = prompt(&session);
        tracking.request(0).await;
        tracking.release(0);
        tracking.request(1).await;
        tracking.request(2).await;
        let child_job = running_job(&session, "agent").await;
        let child = runtime
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
                if runtime
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
        runtime
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
    async fn wait_rejects_invalid_timeouts_and_unknown_arguments() {
        let workspace = tempfile::tempdir().unwrap();
        let harness = harness(workspace.path(), Tracking::new(vec![])).await;
        let session = Arc::new(harness.new_session().await.unwrap());
        let runtime = &session.runtime;
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
            let result = bounded(runtime.executor.execute(
                session.root.clone(),
                "wait",
                invalid.clone(),
                None,
            ))
            .await;
            assert!(result.is_err(), "accepted {invalid}");
        }
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn native_script_wait_supports_direct_and_builder_calls_without_self_wake() {
        let workspace = tempfile::tempdir().unwrap();
        let harness = harness(workspace.path(), Tracking::new(vec![])).await;
        let session = harness.new_session().await.unwrap();
        let output = bounded(session.run_script(
            "const direct = await tool.wait({timeout:1}); const builder = await tool.wait().timeout(1); return [direct, builder];",
        ))
        .await
        .unwrap();
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
        let runtime = &session.runtime;
        let turn = prompt(&session);
        tracking.request(0).await;
        tracking.release(0);
        let job = running_job(&session, "wait").await;
        session.cancel_job(job).await.unwrap();
        let next = tracking.request(1).await;
        let text = serde_json::to_string(&next.messages).unwrap();
        assert!(text.contains("cancel"), "{text}");
        assert_eq!(
            runtime.jobs.snapshot(job).await.unwrap().state,
            JobState::Cancelled
        );
        tracking.release(1);
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
}
