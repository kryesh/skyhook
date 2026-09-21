//! Agent-facing event waiting, separate from saved-output inspection.

use super::{AgentCommand, SessionRuntime};

mod delivery;
mod receipt;
use crate::tool::{
    ToolContext, ToolError,
    diagnostic::{Effects, Operation, Subject},
};
pub(super) use receipt::PendingEventBatch;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;
use tokio::sync::{mpsc, watch};

/// A batch releases this long after the *most recent* event of a burst, so
/// events that trickle in (a child's final message notification, then its job
/// completion a few milliseconds later) become one wake instead of two.
const COALESCE_QUIET_WINDOW: Duration = Duration::from_millis(100);
/// Ceiling on a single window, measured from the burst's first event: a steady
/// stream of notifications would otherwise reset the quiet timer forever, and
/// both an idle agent's wake and `flush_events` (which blocks a turn on the
/// open window) must stay bounded regardless of activity.
const COALESCE_MAX_WINDOW: Duration = Duration::from_millis(500);

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
}

impl AgentWake {
    /// The latest scheduled wake revision; differs from the released one while a
    /// coalescing window is open.
    fn scheduled(&self) -> u64 {
        self.batch().revision
    }

    fn batch(&self) -> std::sync::MutexGuard<'_, EventBatch> {
        let batch = self.batch.lock();
        batch.unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[derive(Default)]
struct EventBatch {
    revision: u64,
    input_revision: u64,
    /// Present exactly while a window is open; signals the window task that
    /// more events landed and its quiet timer must restart.
    activity: Option<Arc<tokio::sync::Notify>>,
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
    /// The window is trailing: activity restarts the quiet timer instead of
    /// being cut off into the next window, bounded by `COALESCE_MAX_WINDOW`.
    /// Stays synchronous and non-blocking; `send` calls this between
    /// `reserve` and `permit.send` with no await in between.
    fn schedule(&self, input: bool) {
        let mut batch = self.wake.batch();
        batch.revision = batch.revision.wrapping_add(1);
        if input {
            batch.input_revision = batch.input_revision.wrapping_add(1);
        }
        if let Some(activity) = &batch.activity {
            // `notify_one` stores a permit, so a reset racing with the window
            // task's select is observed rather than lost. A reset racing with
            // release is harmless: this revision bump happened under the same
            // lock the releasing task publishes from.
            activity.notify_one();
            return;
        }
        let activity = Arc::new(tokio::sync::Notify::new());
        batch.activity = Some(activity.clone());
        // `Weak`: a dropped agent must not be kept alive by its own window.
        let wake = Arc::downgrade(&self.wake);
        tokio::spawn(async move {
            let quiet = tokio::time::sleep(COALESCE_QUIET_WINDOW);
            let cap = tokio::time::sleep(COALESCE_MAX_WINDOW);
            tokio::pin!(quiet, cap);
            loop {
                tokio::select! {
                    () = &mut quiet => break,
                    () = &mut cap => break,
                    () = activity.notified() => quiet
                        .as_mut()
                        .reset(tokio::time::Instant::now() + COALESCE_QUIET_WINDOW),
                }
            }
            let Some(wake) = wake.upgrade() else { return };
            let mut batch = wake.batch();
            wake.ready_input_revision
                .store(batch.input_revision, Ordering::Release);
            batch.activity = None;
            // Publish under the lock before another window can begin.
            wake.revision.send_replace(batch.revision);
        });
    }

    pub(super) async fn flush_events(
        &self,
        cancellation: &crate::job::CancellationToken,
    ) -> Result<(), super::HarnessError> {
        let mut revision = self.wake.revision.subscribe();
        let requested = self.wake.scheduled();
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
                let invalid = |message: &str| {
                    ToolError::InvalidArguments(message.into())
                        .operation(Operation::Validate, Subject::argument(["timeout"]))
                        .effects(Effects::NotStarted)
                };
                if seconds == 0 {
                    return Err(invalid("timeout must be a positive integer"));
                }
                tokio::time::Instant::now()
                    .checked_add(std::time::Duration::from_secs(seconds))
                    .ok_or_else(|| invalid("timeout is too large"))
            })
            .transpose()?;
        let sender = self.agent_sender(context.agent()).ok_or_else(|| {
            ToolError::Failed("calling agent is not active".into())
                .operation(
                    Operation::Lookup,
                    Subject::Label(format!("calling agent {}", context.agent())),
                )
                .effects(Effects::Unchanged)
        })?;
        let mut revision = sender.wake.revision.subscribe();
        let timeout = async || match deadline {
            Some(deadline) => tokio::time::sleep_until(deadline).await,
            None => std::future::pending().await,
        };
        let cancelled = || {
            ToolError::Cancelled
                .operation(Operation::Wait, Subject::Job(context.job()))
                .effects(Effects::Unchanged)
        };
        loop {
            if context.is_cancelled() {
                return Err(cancelled());
            }
            // The agent cannot act while its foreground work is outstanding, so that
            // takes precedence; the next request carries every event together. Such
            // work can park in a wait later, so each new event re-evaluates.
            let parked = self.jobs.parked(context.agent());
            let state = self.jobs.wait_state(context.agent(), context.job()).await;
            if let Some(holding) = state.holding {
                tokio::select! {
                    biased;
                    () = context.cancelled() => return Err(cancelled()),
                    // Completed foreground work is something the agent acts on: its
                    // result returns with this wait's. A script's wait cannot.
                    _ = self.jobs.wait_settled(holding) => {
                        if !state.hosted && self.jobs.settled(holding).await {
                            return Ok(WaitOutput { reason: WakeReason::Event });
                        }
                        continue;
                    }
                    () = parked => continue,
                    changed = revision.changed() => {
                        if changed.is_err() { return Err(cancelled()); }
                        continue;
                    }
                    () = timeout() => return Ok(WaitOutput { reason: WakeReason::Timeout }),
                }
            }
            // Level-triggered, so nothing that landed earlier is lost; a script's
            // floor (what it was last shown) stops it being told twice. Waiting for
            // any open batch to release first keeps a burst in one report.
            let released = *revision.borrow_and_update();
            let ready_input = sender.wake.ready_input_revision.load(Ordering::Acquire);
            let input = ready_input != sender.wake.observed_input.load(Ordering::Acquire)
                && state.seen_input != Some(ready_input);
            let settled = released == sender.wake.scheduled();
            if (state.unseen || input) && settled {
                self.jobs
                    .set_wait_floor(context.job(), (state.stamp, ready_input))
                    .await;
                return Ok(WaitOutput {
                    reason: WakeReason::Event,
                });
            }
            tokio::select! {
                biased;
                () = context.cancelled() => return Err(cancelled()),
                changed = revision.changed() => {
                    if changed.is_err() { return Err(cancelled()); }
                }
                () = timeout() => return Ok(WaitOutput { reason: WakeReason::Timeout }),
            }
        }
    }
}

// Most tests pause time: journal and job I/O runs in `spawn_blocking`, which holds
// the paused clock still. A test that spawns a real process must use real time.
#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::Value;

    use super::super::*;
    use super::{COALESCE_MAX_WINDOW, COALESCE_QUIET_WINDOW};
    pub(super) use crate::agent::runtime::tests::{
        Script, Step, bounded, enqueue_prompts, ephemeral_session, poll, rendered, response,
    };
    use crate::{
        job::{JobOutcome, JobSpec, JobState},
        tool::ToolOutput,
    };

    /// Gated steps, each served to the named model profile's next request.
    pub(super) fn tracking_all(steps: Vec<(&'static str, Vec<AssistantContent>)>) -> Arc<Script> {
        let steps = steps
            .into_iter()
            .map(|(model, content)| Step::new(response(content)).model(model).gated());
        Script::new(steps, &Default::default())
    }

    pub(super) fn tracking(steps: Vec<(&'static str, AssistantContent)>) -> Arc<Script> {
        tracking_all(steps.into_iter().map(|(m, c)| (m, vec![c])).collect())
    }

    pub(super) fn call(id: &str, name: &str, arguments: Value) -> AssistantContent {
        AssistantContent::tool_call(id, 0, ToolCall::new(id, name, arguments).unwrap())
    }

    pub(super) fn answer() -> AssistantContent {
        AssistantContent::text("answer", 0, "done".to_owned())
    }

    /// A session whose "root" and "child" profiles are answered by `tracking`.
    pub(super) async fn start(tracking: &Arc<Script>) -> (tempfile::TempDir, Arc<SessionHandle>) {
        let root = tempfile::tempdir().unwrap();
        let profile = |model: &str| ModelProfile {
            hint: Some(model.to_owned()),
            ..ModelProfile::new("wait-test", model, None, 128_000, 4096, true)
        };
        let harness = HarnessBuilder::new(root.path())
            .session_root(root.path().join("sessions"))
            .provider("wait-test", tracking.clone())
            .model_profile("root", profile("root"))
            .model_profile("child", profile("child"))
            .default_model_profile("root")
            .build()
            .await
            .unwrap();
        (root, Arc::new(ephemeral_session(&harness).await))
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
                poll().await;
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
        assert_eq!(event["meta"]["last_message"], message["message"]);
        assert!(event["result"].is_null(), "{event}");
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

    /// Drives one window's timers without the harness: paused time plus the
    /// full session would race the provider gates, and only the release time
    /// is under test here. One yield lets the window task register or rearm
    /// its timers before the next `advance`.
    async fn settle() {
        tokio::task::yield_now().await;
    }

    #[tokio::test(start_paused = true)]
    async fn trailing_window_coalesces_a_split_burst_into_one_batch() {
        let sender = AgentSender::new(mpsc::channel(1).0);
        let mut revision = sender.wake.revision.subscribe();
        // Each gap ends inside the open quiet period, so every event resets it.
        let gap = COALESCE_QUIET_WINDOW - std::time::Duration::from_millis(20);
        sender.schedule(true);
        settle().await;
        for _ in 0..3 {
            tokio::time::advance(gap).await;
            settle().await;
            assert_eq!(
                *revision.borrow_and_update(),
                0,
                "batch released before the quiet period elapsed"
            );
            sender.schedule(true);
            settle().await;
        }
        tokio::time::advance(COALESCE_QUIET_WINDOW).await;
        settle().await;
        // Four notifications spread over 340ms, one wake carrying all of them.
        assert_eq!(*revision.borrow_and_update(), 4);
        assert_eq!(
            sender.wake.ready_input_revision.load(Ordering::Acquire),
            4,
            "released batch must expose every accumulated input"
        );
        assert!(
            !revision.has_changed().unwrap(),
            "a trailing burst must not release a second batch"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn continuous_activity_cannot_postpone_a_batch_past_the_cap() {
        let sender = AgentSender::new(mpsc::channel(1).0);
        let mut revision = sender.wake.revision.subscribe();
        let step = std::time::Duration::from_millis(10);
        sender.schedule(false);
        settle().await;
        let mut elapsed = std::time::Duration::ZERO;
        while *revision.borrow_and_update() == 0 {
            assert!(
                elapsed <= COALESCE_MAX_WINDOW,
                "cap failed to release after {elapsed:?} of continuous activity"
            );
            tokio::time::advance(step).await;
            settle().await;
            elapsed += step;
            // Activity never quiets down: only the cap can end this window.
            sender.schedule(false);
            settle().await;
        }
        assert_eq!(elapsed, COALESCE_MAX_WINDOW);
    }

    #[tokio::test(start_paused = true)]
    async fn positive_timeouts_do_not_wake_on_their_own_jobs() {
        let tracking = tracking(vec![
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

    #[tokio::test(start_paused = true)]
    async fn clustered_background_completions_wake_and_inject_saved_output_once() {
        let tracking = tracking(vec![
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

    #[tokio::test(start_paused = true)]
    async fn completion_before_wait_registration_is_not_lost() {
        let tracking = tracking(vec![
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

    #[tokio::test(start_paused = true)]
    async fn queued_user_input_wakes_wait_without_consuming_the_input() {
        let tracking = tracking(vec![
            ("root", call("waiting", "wait", json!({"timeout":null}))),
            ("root", call("again", "wait", json!({"timeout":1}))),
            ("root", answer()),
        ]);
        let (_root, session) = start(&tracking).await;
        let turn = prompt(&session);
        tracking.pass(0).await;
        running_job(&session, &session.root, "wait").await;
        let prompt = queued("queued-wake-marker");
        let queued = tokio::spawn({
            let session = session.clone();
            async move { enqueue_prompts(&session, vec![prompt]).await.pop().unwrap() }
        });
        let request = tracking.request(1).await;
        bounded(queued).await.unwrap().unwrap();
        assert_reason(&request, "waiting", "event");
        let messages = rendered(&request);
        assert!(messages.contains("queued-wake-marker"));
        tracking.release(1);
        assert_reason(&tracking.pass(2).await, "again", "timeout");
        bounded(turn).await.unwrap().unwrap();
        session.shutdown().await.unwrap();
    }

    fn queued(text: &str) -> QueuedPrompt {
        QueuedPrompt {
            text: text.into(),
            ..Default::default()
        }
    }

    /// A foreground call inside a background script does not hold the agent, so a
    /// later `wait` resolves on an event while that call is still running.
    #[tokio::test]
    async fn waits_do_not_defer_to_work_inside_a_background_script() {
        let sleeper = json!({"source":"await tool.exec({argv:['sleep','30']});", "bg":true});
        let tracking = tracking(vec![
            ("root", call("bg", "script", sleeper)),
            ("root", call("hold", "wait", json!({"timeout":null}))),
            ("root", answer()),
        ]);
        let (_root, session) = start(&tracking).await;
        let turn = prompt(&session);
        tracking.pass(0).await;
        let exec = running_job(&session, &session.root, "exec").await;
        tracking.pass(1).await;
        running_job(&session, &session.root, "wait").await;
        let queued = tokio::spawn({
            let session = session.clone();
            async move {
                enqueue_prompts(&session, vec![queued("bg-marker")])
                    .await
                    .pop()
                    .unwrap()
            }
        });
        let request = tracking.request(2).await;
        bounded(queued).await.unwrap().unwrap();
        assert_reason(&request, "hold", "event");
        let state = session.runtime.jobs.snapshot(exec).await.unwrap().state;
        assert_eq!(state, JobState::Running);
        tracking.release(2);
        bounded(turn).await.unwrap().unwrap();
        session.shutdown().await.unwrap();
    }

    /// A wait deferring to a script must wake when that script parks in its own
    /// wait, not sleep until the script ends.
    #[tokio::test(start_paused = true)]
    async fn a_deferring_wait_wakes_when_its_holder_parks() {
        let first = json!({"source":"return (await tool.wait({timeout:30})).result;"});
        // Holds the agent for a (virtual) second, then parks in a wait of its own.
        let second = json!({"source":"await sleep(1000); \
            await tool.wait({timeout:30}); return (await tool.wait({timeout:1})).result;"});
        let tracking = tracking_all(vec![
            (
                "root",
                vec![
                    call("first", "script", first),
                    AssistantContent::tool_call(
                        "second",
                        1,
                        ToolCall::new("second", "script", second).unwrap(),
                    ),
                ],
            ),
            ("root", vec![answer()]),
        ]);
        let (_root, session) = start(&tracking).await;
        let turn = prompt(&session);
        tracking.pass(0).await;
        let scripts = async || {
            let jobs = session.runtime.jobs.list(&session.root).await;
            jobs.into_iter()
                .filter(|job| job.tool == "script")
                .collect::<Vec<_>>()
        };
        let wait = running_job(&session, &session.root, "wait").await;
        let deferring = session.runtime.jobs.snapshot(wait).await.unwrap().parent;
        let queued = tokio::spawn({
            let session = session.clone();
            async move {
                enqueue_prompts(&session, vec![queued("park-marker")])
                    .await
                    .pop()
                    .unwrap()
            }
        });
        // The input's batch has released, yet the wait defers to the busy script.
        tokio::time::sleep(COALESCE_MAX_WINDOW).await;
        assert!(
            scripts()
                .await
                .iter()
                .all(|job| job.state == JobState::Running)
        );
        let finished = bounded(async {
            loop {
                let mut scripts = scripts().await.into_iter();
                if let Some(done) = scripts.find(|job| job.state.is_terminal()) {
                    return done.id;
                }
                poll().await;
            }
        })
        .await;
        assert_eq!(Some(finished), deferring);
        // The first to finish is the deferring script, told about the input while
        // the other is still parked in its second wait.
        let others = scripts().await.into_iter().filter(|job| job.id != finished);
        assert!(others.into_iter().all(|job| job.state == JobState::Running));
        let output = session
            .runtime
            .jobs
            .wait(finished, None, false)
            .await
            .unwrap()
            .output;
        let value = output.as_ref().map(|output| &output["value"]);
        assert_eq!(value, Some(&json!({"reason":"event"})), "{output:?}");
        tracking.request(1).await;
        bounded(queued).await.unwrap().unwrap();
        tracking.release(1);
        bounded(turn).await.unwrap().unwrap();
        session.shutdown().await.unwrap();
    }

    /// What a script was shown depends only on what was pending when it looked, not
    /// on when the wake revision catches up: a notification becomes pending before
    /// its wake is forwarded, and must not be reported to the same script twice.
    #[tokio::test(start_paused = true)]
    async fn a_script_is_shown_each_pending_notification_once() {
        let (_root, session) = start(&tracking(vec![])).await;
        let (jobs, root) = (&session.runtime.jobs, &session.root);
        let script = JobSpec {
            role: crate::job::JobRole::Script,
            ..JobSpec::test(root.clone(), "script")
        };
        let script = jobs.test_create(script).await;
        let hosted = || JobSpec {
            parent: Some(script),
            ..JobSpec::test(root.clone(), "wait")
        };
        complete_background(&session, root, "first").await;
        let first = jobs.test_create(hosted()).await;
        let state = jobs.wait_state(root, first).await;
        assert!(state.unseen);
        jobs.set_wait_floor(first, (state.stamp, 0)).await;
        let second = jobs.test_create(hosted()).await;
        assert!(!jobs.wait_state(root, second).await.unseen);
        complete_background(&session, root, "second").await;
        assert!(jobs.wait_state(root, second).await.unseen);
        // A model caller has no script floor: its request boundary consumes instead.
        let model = jobs.test_create(JobSpec::test(root.clone(), "wait")).await;
        assert!(jobs.wait_state(root, model).await.unseen);
        session.shutdown().await.unwrap();
    }

    /// Waits must not defer to each other: both wake on the same event.
    #[tokio::test(start_paused = true)]
    async fn concurrent_waits_do_not_defer_to_each_other() {
        let second = ToolCall::new("second", "wait", json!({"timeout":null})).unwrap();
        let tracking = tracking_all(vec![
            (
                "root",
                vec![
                    call("first", "wait", json!({"timeout":null})),
                    AssistantContent::tool_call("second", 1, second),
                ],
            ),
            ("root", vec![answer()]),
        ]);
        let (_root, session) = start(&tracking).await;
        let turn = prompt(&session);
        tracking.pass(0).await;
        bounded(async {
            while session
                .runtime
                .jobs
                .list(&session.root)
                .await
                .iter()
                .filter(|job| job.tool == "wait" && job.state == JobState::Running)
                .count()
                < 2
            {
                poll().await;
            }
        })
        .await;
        let prompt = queued("concurrent-wake-marker");
        let queued = tokio::spawn({
            let session = session.clone();
            async move { enqueue_prompts(&session, vec![prompt]).await.pop().unwrap() }
        });
        let request = tracking.request(1).await;
        bounded(queued).await.unwrap().unwrap();
        assert_reason(&request, "first", "event");
        assert_reason(&request, "second", "event");
        tracking.release(1);
        bounded(turn).await.unwrap().unwrap();
        session.shutdown().await.unwrap();
    }

    /// Queued input stays unconsumed while a script runs; it must still wake the
    /// script's waits only once.
    #[tokio::test(start_paused = true)]
    async fn queued_input_does_not_respin_a_script_wait() {
        let source = "const first = (await tool.wait({timeout:60})).result; \
            const second = (await tool.wait({timeout:1})).result; return [first, second];";
        let tracking = tracking(vec![
            ("root", call("run", "script", json!({"source": source}))),
            ("root", answer()),
        ]);
        let (_root, session) = start(&tracking).await;
        let turn = prompt(&session);
        tracking.pass(0).await;
        let script = running_job(&session, &session.root, "script").await;
        running_job(&session, &session.root, "wait").await;
        let prompt = queued("queued-respin-marker");
        let queued = tokio::spawn({
            let session = session.clone();
            async move { enqueue_prompts(&session, vec![prompt]).await.pop().unwrap() }
        });
        let request = tracking.request(1).await;
        bounded(queued).await.unwrap().unwrap();
        let output = session
            .runtime
            .jobs
            .wait(script, None, false)
            .await
            .unwrap()
            .output;
        let expected = json!([{"reason":"event"}, {"reason":"timeout"}]);
        assert_eq!(
            output.as_ref().map(|output| &output["value"]),
            Some(&expected),
            "{output:?}"
        );
        let history = rendered(&request);
        assert_eq!(
            history.matches("queued-respin-marker").count(),
            1,
            "{history}"
        );
        tracking.release(1);
        bounded(turn).await.unwrap().unwrap();
        session.shutdown().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn parent_input_wakes_wait_and_is_in_the_next_model_request() {
        let launch = json!({"prompt":"child task", "model":"child", "bg":true});
        let tracking = tracking(vec![
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
        let messages = rendered(&next);
        assert_eq!(messages.matches("parent-wake-marker").count(), 1);
        child_completed(&session, child_job).await;
        tracking.release(2);
        tracking.pass(4).await;
        bounded(turn).await.unwrap().unwrap();
        session.shutdown().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn wait_rejects_invalid_arguments_and_script_waits_do_not_self_wake() {
        let (_root, session) = start(&tracking(vec![])).await;
        let invalid = json!([{"timeout":0}, {"timeout":0.125}, {"timeout":-1}, {"timeout":1e300}, {"timeout":"1"}, {"timeout":true}, {"bg":true}, {"bg":false}, {"job":1}, {"wait":1}]);
        for invalid in invalid.as_array().unwrap() {
            let executor = &session.runtime.executor;
            let result = executor.execute(session.root.clone(), "wait", invalid.clone(), None);
            assert!(bounded(result).await.is_err(), "accepted {invalid}");
        }
        let source = "const direct = (await tool.wait({timeout:1})).result; const builder = (await tool.wait().timeout(1)).result; return [direct, builder];";
        let output = bounded(session.run_script(source)).await.unwrap();
        let timeouts = json!([{"reason":"timeout"}, {"reason":"timeout"}]);
        assert_eq!(
            output.value,
            json!({"value":timeouts, "console":"", "failure":null})
        );
        session.shutdown().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn cancelling_an_indefinite_wait_unblocks_the_agent() {
        let tracking = tracking(vec![
            ("root", call("waiting", "wait", json!({}))),
            ("root", answer()),
        ]);
        let (_root, session) = start(&tracking).await;
        let turn = prompt(&session);
        tracking.pass(0).await;
        let job = running_job(&session, &session.root, "wait").await;
        session.cancel_job(job).await.unwrap();
        let next = tracking.request(1).await;
        let text = rendered(&next);
        assert!(text.contains("cancel"), "{text}");
        let state = session.runtime.jobs.snapshot(job).await.unwrap().state;
        assert_eq!(state, JobState::Cancelled);
        tracking.release(1);
        bounded(turn).await.unwrap().unwrap();
        session.shutdown().await.unwrap();
    }

    /// Observed activity trails the journal; interrupting earlier races the guard.
    async fn running_tools(session: &SessionHandle, agent: &AgentId) {
        bounded(async {
            while !matches!(
                session.observe().await.snapshot.activity.get(agent),
                Some(AgentActivity::Tools)
            ) {
                poll().await;
            }
        })
        .await;
    }

    #[tokio::test(start_paused = true)]
    async fn interrupt_unblocks_an_agent_held_by_a_foreground_script_wait() {
        let source = json!({"source":"await tool.wait({timeout:30}); return 'late';"});
        let tracking = tracking(vec![
            ("root", call("blocked", "script", source)),
            ("root", answer()),
        ]);
        let (_root, session) = start(&tracking).await;
        let turn = prompt(&session);
        tracking.pass(0).await;
        let script = running_job(&session, &session.root, "script").await;
        let waiting = running_job(&session, &session.root, "wait").await;
        running_tools(&session, &session.root).await;
        assert_eq!(bounded(session.interrupt()).await, 1);
        assert!(bounded(turn).await.unwrap().is_err());
        for job in [script, waiting] {
            let state = session.runtime.jobs.snapshot(job).await.unwrap().state;
            assert_eq!(state, JobState::Cancelled, "job {job:?}");
        }
        // The unwinding turn must not reach another request boundary.
        assert!(!tracking.requested_from(1));
        let resumed = tokio::spawn({
            let session = session.clone();
            async move { session.continue_turn().await }
        });
        let history = tracking.request(1).await;
        let history = rendered(&history);
        assert!(history.contains("cancel"), "{history}");
        tracking.release(1);
        assert_eq!(bounded(resumed).await.unwrap().unwrap(), "done");
        session.shutdown().await.unwrap();
    }

    /// A model-delegated child is retained while the script blocking the same turn
    /// is cancelled. (A child the script launched would die with it.)
    #[tokio::test(start_paused = true)]
    async fn interrupt_cancels_blocking_tools_and_retains_delegated_children() {
        let delegate = json!({"prompt":"work", "model":"child", "name":"kid"});
        let blocked = json!({"source":"await tool.wait({timeout:30});"});
        let tracking = tracking_all(vec![
            (
                "root",
                vec![
                    call("kid", "agent", delegate),
                    AssistantContent::tool_call(
                        "blocked",
                        1,
                        ToolCall::new("blocked", "script", blocked).unwrap(),
                    ),
                ],
            ),
            ("child", vec![call("child-hold", "wait", json!({}))]),
            ("root", vec![answer()]),
        ]);
        let (_root, session) = start(&tracking).await;
        let turn = prompt(&session);
        tracking.pass(0).await;
        let script = running_job(&session, &session.root, "script").await;
        let delegated = running_job(&session, &session.root, "agent").await;
        tracking.request(1).await;
        let (child, _) = only_child(&session);
        tracking.release(1);
        running_job(&session, &child, "wait").await;
        running_tools(&session, &session.root).await;
        assert_eq!(bounded(session.interrupt()).await, 2);
        let state = async |job| session.runtime.jobs.snapshot(job).await.unwrap().state;
        // Both outcomes are journaled asynchronously; settle before comparing.
        let settled = async |job, want| {
            bounded(async {
                while state(job).await != want {
                    poll().await;
                }
            })
            .await;
        };
        settled(script, JobState::Cancelled).await;
        settled(delegated, JobState::Interrupted).await;
        assert!(!tracking.requested_from(2));
        // A suspended child keeps the parent's `agent` call waiting for `continue`;
        // cancelling it releases the turn, which ends without another request.
        session.cancel_job(delegated).await.unwrap();
        assert!(bounded(turn).await.unwrap().is_err());
        assert!(!tracking.requested_from(2));
        session.shutdown().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn idle_agent_batches_background_notifications_without_a_wait_call() {
        let tracking = tracking(vec![("root", answer()); 3]);
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
        let messages = rendered(&final_request);
        assert!(messages.contains("idle-batch-barrier"));
        // completed output must not be injected twice
        assert_eq!(events(&final_request), notifications);
        bounded(barrier).await.unwrap().unwrap();
        session.shutdown().await.unwrap();
    }
}
