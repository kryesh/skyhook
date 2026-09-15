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
            .agent_sender(context.agent())
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
                    && self.jobs.has_pending(context.agent()).await)
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
    use std::sync::{Arc, Mutex as StdMutex};

    use serde_json::Value;
    use tokio::sync::{Notify, Semaphore};

    use super::super::*;
    pub(super) use crate::agent::runtime::tests::{bounded, enqueue_prompts};
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
            let steps = steps.into_iter().map(|(m, c)| (m, vec![c]));
            Self::responses(steps.collect())
        }

        pub(super) fn responses(steps: Vec<(&'static str, Vec<AssistantContent>)>) -> Arc<Self> {
            let steps = steps.into_iter().map(|(model, content)| Step {
                model,
                content,
                gate: Semaphore::new(0),
            });
            Arc::new(Self {
                steps: steps.collect(),
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

        /// Waits for a step's request, then lets its response through.
        pub(super) async fn pass(&self, step: usize) -> ModelRequest {
            let request = self.request(step).await;
            self.release(step);
            request
        }

        /// Whether any step at or after `step` has been requested.
        pub(super) fn requested_from(&self, step: usize) -> bool {
            let requests = self.requests.lock().unwrap();
            requests.iter().any(|(seen, _)| *seen >= step)
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
                let events = futures_util::stream::iter(events.into_iter().map(Ok));
                Ok(Box::pin(events) as ResponseStream)
            })
        }
    }

    pub(super) fn call(id: &str, name: &str, arguments: Value) -> AssistantContent {
        AssistantContent::tool_call(id, 0, ToolCall::new(id, name, arguments).unwrap())
    }

    pub(super) fn answer() -> AssistantContent {
        AssistantContent::text("answer", 0, "done".to_owned())
    }

    /// A session whose "root" and "child" profiles are answered by `tracking`.
    pub(super) async fn start(tracking: &Arc<Tracking>) -> (tempfile::TempDir, Arc<SessionHandle>) {
        let root = tempfile::tempdir().unwrap();
        let profile =
            |model: &str| ModelProfile::new("wait-test", model, None, 128_000, 4096, true);
        let harness = HarnessBuilder::new(root.path())
            .session_root(root.path().join("sessions"))
            .provider("wait-test", Arc::new(Factory(tracking.clone())))
            .model_profile("root", profile("root"))
            .model_profile("child", profile("child"))
            .default_model_profile("root")
            .build()
            .await
            .unwrap();
        (root, Arc::new(harness.new_session().await.unwrap()))
    }

    pub(super) fn prompt(
        session: &Arc<SessionHandle>,
    ) -> tokio::task::JoinHandle<Result<String, HarnessError>> {
        let session = session.clone();
        tokio::spawn(async move { session.prompt("start").await })
    }

    pub(super) async fn running_job(session: &SessionHandle, owner: &AgentId, tool: &str) -> JobId {
        bounded(async {
            loop {
                let mut jobs = session.runtime.jobs.list(owner).await.into_iter();
                if let Some(job) =
                    jobs.find(|job| job.tool == tool && job.state == JobState::Running)
                {
                    return job.id;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
    }

    /// The only non-root agent and its command sender.
    pub(super) fn only_child(session: &SessionHandle) -> (AgentId, AgentSender) {
        let agents = session.runtime.agents.read().unwrap();
        let (child, slot) = agents.iter().find(|(id, _)| **id != session.root).unwrap();
        (child.clone(), slot.sender.clone())
    }
    pub(super) async fn complete_background(
        session: &SessionHandle,
        owner: &AgentId,
        value: &str,
    ) -> JobId {
        let jobs = &session.runtime.jobs;
        let spec = JobSpec {
            background: true,
            ..JobSpec::test(owner.clone(), "wait-fixture")
        };
        let lease = jobs.create(spec).await.unwrap();
        let id = lease.id();
        jobs.transition(id, JobState::Running).await.unwrap();
        let outcome = JobOutcome::Completed(ToolOutput::new(json!({"value":value})));
        jobs.finish(id, outcome).await.unwrap();
        lease.into_test_id()
    }

    pub(super) fn events(request: &ModelRequest) -> Vec<Value> {
        job_entries(request, false)
    }

    pub(super) fn agent_messages(request: &ModelRequest) -> Vec<Value> {
        job_entries(request, true)
    }

    fn job_entries(request: &ModelRequest, messages: bool) -> Vec<Value> {
        let (prefix, suffix) = ("<skyhook_job_events>\n", "\n</skyhook_job_events>");
        let blocks = request.messages().flat_map(|message| match message {
            Message::User(content) => content.as_slice(),
            _ => &[],
        });
        let entries = blocks.filter_map(|block| match block {
            UserContent::Runtime { text } => text.strip_prefix(prefix)?.strip_suffix(suffix),
            _ => None,
        });
        let entries = entries.flat_map(|text| serde_json::from_str::<Vec<Value>>(text).unwrap());
        entries
            .filter(|entry| (entry["kind"] == "message") == messages)
            .collect()
    }

    pub(super) async fn child_completed(session: &SessionHandle, job: JobId) {
        let snapshot =
            crate::agent::runtime::tests::until(session, job, |job| job.state.is_terminal());
        assert_eq!(snapshot.await.state, JobState::Completed);
    }
    /// A completion references the child's last message rather than copying its text.
    pub(super) fn assert_child_completion(event: &Value, message: &Value) {
        assert_eq!(event["id"], message["id"]);
        assert_eq!(event["state"], "completed");
        assert_eq!(event["last_message"], message["message"]);
        assert!(event.get("result").is_none(), "{event}");
        assert!(event.get("text").is_none(), "{event}");
    }

    pub(super) fn assert_reason(request: &ModelRequest, id: &str, reason: &str) {
        let results = request.messages().flat_map(|message| match message {
            Message::Tool(results) => results.as_slice(),
            _ => &[],
        });
        let results = results
            .filter(|result| result.call_id == id)
            .collect::<Vec<_>>();
        assert_eq!(results.len(), 1, "wait must have exactly one result");
        assert!(!results[0].is_error, "wait failed: {:?}", results[0]);
        assert_eq!(results[0].result["result"], json!({"reason":reason}));
    }

    /// Completions share one request and each carries its saved output.
    fn assert_notifications(notifications: &[Value], expected: &[(JobId, &str)]) {
        assert_eq!(notifications.len(), expected.len());
        for (id, value) in expected {
            let notification = notifications.iter().find(|event| event["id"] == id.get());
            let notification = serde_json::to_string(notification.unwrap()).unwrap();
            assert!(notification.contains(value));
        }
    }

    #[tokio::test]
    async fn positive_timeouts_do_not_wake_on_their_own_jobs() {
        let tracking = Tracking::new(vec![
            ("root", call("first", "wait", json!({"timeout":1}))),
            ("root", call("second", "wait", json!({"timeout":1}))),
            ("root", answer()),
        ]);
        let (_root, session) = start(&tracking).await;
        let turn = prompt(&session);
        tracking.pass(0).await;
        assert_reason(&tracking.pass(1).await, "first", "timeout");
        let request = tracking.pass(2).await;
        assert_reason(&request, "second", "timeout");
        let notifications = events(&request);
        // foreground wait completion must not notify its owner
        assert!(notifications.is_empty());
        assert_eq!(bounded(turn).await.unwrap().unwrap(), "done");
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn clustered_background_completions_wake_and_inject_saved_output_once() {
        let tracking = Tracking::new(vec![
            ("root", call("waiting", "wait", json!({}))),
            ("root", call("again", "wait", json!({"timeout":1}))),
            ("root", answer()),
        ]);
        let (_root, session) = start(&tracking).await;
        let root = &session.root;
        let turn = prompt(&session);
        tracking.pass(0).await;
        running_job(&session, root, "wait").await;
        let first = complete_background(&session, root, "first-output").await;
        let second = complete_background(&session, root, "second-output").await;
        let request = tracking.pass(1).await;
        assert_reason(&request, "waiting", "event");
        let notifications = events(&request);
        assert_notifications(
            &notifications,
            &[(first, "first-output"), (second, "second-output")],
        );
        let next = tracking.pass(2).await;
        assert_reason(&next, "again", "timeout");
        // retained history must not get another copy
        assert_eq!(events(&next), notifications);
        bounded(turn).await.unwrap().unwrap();
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn completion_before_wait_registration_is_not_lost() {
        let tracking = Tracking::new(vec![
            ("root", call("waiting", "wait", json!({"timeout":1}))),
            ("root", answer()),
        ]);
        let (_root, session) = start(&tracking).await;
        let turn = prompt(&session);
        tracking.request(0).await;
        let job = complete_background(&session, &session.root, "already-ready").await;
        tracking.release(0);
        let next = tracking.pass(1).await;
        assert_reason(&next, "waiting", "event");
        assert_notifications(&events(&next), &[(job, "already-ready")]);
        bounded(turn).await.unwrap().unwrap();
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn queued_user_input_wakes_wait_without_consuming_the_input() {
        let tracking = Tracking::new(vec![
            ("root", call("waiting", "wait", json!({"timeout":null}))),
            ("root", call("again", "wait", json!({"timeout":1}))),
            ("root", answer()),
        ]);
        let (_root, session) = start(&tracking).await;
        let turn = prompt(&session);
        tracking.pass(0).await;
        running_job(&session, &session.root, "wait").await;
        let prompt = QueuedPrompt {
            text: "queued-wake-marker".into(),
            attachments: vec![],
            options: PromptOptions::default(),
            token: QueuedPromptToken::new().unwrap(),
        };
        let queued = tokio::spawn({
            let session = session.clone();
            async move { enqueue_prompts(&session, vec![prompt]).await.pop().unwrap() }
        });
        let request = tracking.request(1).await;
        bounded(queued).await.unwrap().unwrap();
        assert_reason(&request, "waiting", "event");
        let messages = serde_json::to_string(&request.messages().collect::<Vec<_>>()).unwrap();
        assert!(messages.contains("queued-wake-marker"));
        tracking.release(1);
        assert_reason(&tracking.pass(2).await, "again", "timeout");
        bounded(turn).await.unwrap().unwrap();
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn parent_input_wakes_wait_and_is_in_the_next_model_request() {
        let launch = json!({"prompt":"child task", "model":"child", "bg":true});
        let tracking = Tracking::new(vec![
            ("root", call("child", "agent", launch)),
            ("child", call("child-wait", "wait", json!({}))),
            ("root", call("parent-wait", "wait", json!({}))),
            ("child", answer()),
            ("root", answer()),
        ]);
        let (_root, session) = start(&tracking).await;
        let turn = prompt(&session);
        tracking.pass(0).await;
        tracking.request(1).await;
        tracking.request(2).await;
        let child_job = running_job(&session, &session.root, "agent").await;
        let (child, _) = only_child(&session);
        tracking.release(1);
        running_job(&session, &child, "wait").await;
        let jobs = &session.runtime.jobs;
        jobs.send(child_job, json!("parent-wake-marker"))
            .await
            .unwrap();
        let next = tracking.pass(3).await;
        assert_reason(&next, "child-wait", "event");
        let messages = serde_json::to_string(&next.messages().collect::<Vec<_>>()).unwrap();
        assert_eq!(messages.matches("parent-wake-marker").count(), 1);
        child_completed(&session, child_job).await;
        tracking.release(2);
        tracking.pass(4).await;
        bounded(turn).await.unwrap().unwrap();
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn wait_rejects_invalid_arguments_and_script_waits_do_not_self_wake() {
        let (_root, session) = start(&Tracking::new(vec![])).await;
        let invalid = json!([{"timeout":0}, {"timeout":0.125}, {"timeout":-1}, {"timeout":1e300}, {"timeout":"1"}, {"timeout":true}, {"bg":true}, {"bg":false}, {"job":1}, {"wait":1}]);
        for invalid in invalid.as_array().unwrap() {
            let executor = &session.runtime.executor;
            let result = executor.execute(session.root.clone(), "wait", invalid.clone(), None);
            assert!(bounded(result).await.is_err(), "accepted {invalid}");
        }
        let source = "const direct = await tool.wait({timeout:1}); const builder = await tool.wait().timeout(1); return [direct, builder];";
        let output = bounded(session.run_script(source)).await.unwrap();
        let timeouts = json!([{"reason":"timeout"}, {"reason":"timeout"}]);
        assert_eq!(output.value, json!({"value":timeouts, "console":""}));
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cancelling_an_indefinite_wait_unblocks_the_agent() {
        let tracking = Tracking::new(vec![
            ("root", call("waiting", "wait", json!({}))),
            ("root", answer()),
        ]);
        let (_root, session) = start(&tracking).await;
        let turn = prompt(&session);
        tracking.pass(0).await;
        let job = running_job(&session, &session.root, "wait").await;
        session.cancel_job(job).await.unwrap();
        let next = tracking.request(1).await;
        let text = serde_json::to_string(&next.messages().collect::<Vec<_>>()).unwrap();
        assert!(text.contains("cancel"), "{text}");
        let state = session.runtime.jobs.snapshot(job).await.unwrap().state;
        assert_eq!(state, JobState::Cancelled);
        tracking.release(1);
        bounded(turn).await.unwrap().unwrap();
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn idle_agent_batches_background_notifications_without_a_wait_call() {
        let tracking = Tracking::new(vec![("root", answer()); 3]);
        let (_root, session) = start(&tracking).await;
        let root = &session.root;
        let initial = prompt(&session);
        tracking.pass(0).await;
        bounded(initial).await.unwrap().unwrap();

        let first = complete_background(&session, root, "idle-first-output").await;
        let second = complete_background(&session, root, "idle-second-output").await;
        let next = tracking.request(1).await;
        let notifications = events(&next);
        let expected = [(first, "idle-first-output"), (second, "idle-second-output")];
        assert_notifications(&notifications, &expected);

        let barrier = tokio::spawn({
            let session = session.clone();
            async move { session.prompt("idle-batch-barrier").await }
        });
        tracking.release(1);
        let final_request = tracking.pass(2).await;
        let messages =
            serde_json::to_string(&final_request.messages().collect::<Vec<_>>()).unwrap();
        assert!(messages.contains("idle-batch-barrier"));
        // completed output must not be injected twice
        assert_eq!(events(&final_request), notifications);
        bounded(barrier).await.unwrap().unwrap();
        session.shutdown().await.unwrap();
    }
}
