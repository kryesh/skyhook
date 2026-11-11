//! Agent-facing event waiting, separate from saved-output inspection.

use super::{HarnessError, SessionRuntime};

mod delivery;
use crate::identity::AgentId;
use crate::{
    job::{CancellationToken, WaitFloor, Waiting},
    session::{CoreState, Key, Wait},
    tool::{ToolContext, ToolError, ToolOutput},
};
pub(super) use delivery::Boundary;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use skyhook_worker::diagnostic::{Operation, Subject};
use std::time::Duration;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct WaitArgs {
    /// Maximum seconds to wait
    #[schemars(extend("default" = MAX_WAIT))]
    pub(super) timeout: Option<WaitSeconds>,
}

type WaitSeconds = skyhook_base::bounded::Bounded<1, 1800>;

/// The longest a wait lasts, and how long one without a timeout lasts: a bound wait
/// returns control to an agent stuck on work that will never report.
const MAX_WAIT: WaitSeconds = WaitSeconds::new::<1800>();

impl WaitArgs {
    fn duration(&self) -> Duration {
        Duration::from_secs(self.timeout.unwrap_or(MAX_WAIT).into())
    }
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
    /// Whole seconds waited
    pub(super) waited_s: u64,
}

/// The result a cancelled wait's error carries, read as its output would be.
#[derive(Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
enum Unwoken {
    Cancelled { waited_s: u64 },
}

impl SessionRuntime {
    /// Wait for the window open now, if any, to release; the request it precedes
    /// then counts the input it released as seen. A turn cancelled meanwhile may
    /// abandon that count, but makes no request, and every later one flushes again.
    pub(super) async fn flush_events(
        &self,
        agent: &AgentId,
        cancellation: &CancellationToken,
    ) -> Result<(), HarnessError> {
        let agent = agent.clone();
        let scheduled = {
            let agent = agent.clone();
            move |state: &mut CoreState| {
                state.agents.get(&agent).map(|facts| facts.window.scheduled)
            }
        };
        let released = move |requested: Option<u64>| {
            let (key, decided) = (Key::Agent(agent.clone()), agent.clone());
            // Without the agent there is nothing to wait out: it was gone when
            // this registered, in the change that first decides it.
            let decide = move |state: &CoreState| match (state.agents.get(&decided), requested) {
                (Some(facts), Some(requested)) => {
                    (facts.window.released >= requested).then_some(())
                }
                _ => Some(()),
            };
            let take = move |state: &mut CoreState, ()| {
                if let Some(facts) = state.agents.get_mut(&agent) {
                    facts.window.observed_inputs = facts.window.ready_inputs;
                }
            };
            Wait::taking([key], decide, take)
        };
        tokio::select! {
            biased;
            () = cancellation.cancelled() => Err(HarnessError::Interrupted),
            released = self.store.until_after(scheduled, released) => Ok(released?),
        }
    }

    pub(super) async fn wait_for_event(
        &self,
        context: &ToolContext,
        args: WaitArgs,
    ) -> Result<WaitOutput, ToolError> {
        let entered = tokio::time::Instant::now();
        let deadline = entered + args.duration();
        let (agent, caller) = (context.agent().clone(), context.job());
        if self.agents(|agents| agents.get(&agent).is_none()) {
            return Err(ToolError::failed("calling agent is not active").operation(
                Operation::Lookup,
                Subject::Label(format!("calling agent {agent}")),
            ));
        }
        // The agent cannot act while its foreground work is outstanding, so that
        // takes precedence; the next request carries every event together. Waiting
        // for any open batch to release keeps a burst in one report.
        let keys = [Key::Agent(agent.clone()), Key::Job(caller)];
        let decided = move |state: &CoreState| {
            let waiting = state.jobs.waiting(&agent, caller);
            let Some(facts) = state.agents.get(&agent) else {
                return Some(None);
            };
            let window = &facts.window;
            let (unseen, seen_input, stamp) = match waiting {
                Waiting::Cancelled => return Some(None),
                Waiting::Deferred => return None,
                Waiting::Open {
                    unseen,
                    seen_input,
                    stamp,
                } => (unseen, seen_input, stamp),
            };
            let input = window.ready_inputs;
            let ready = input != window.observed_inputs && seen_input != Some(input);
            ((unseen || ready) && window.settled()).then_some(Some(WaitFloor { stamp, input }))
        };
        let timeout = tokio::time::sleep_until(deadline);
        // Parked in the change that first decides, so a concurrent wait that
        // defers to this one is released, and an event ready as it parks is
        // decided before its timeout can pass. An event ready with the timeout is
        // reported; one lost to it stays unseen.
        let park = move |state: &mut CoreState| state.jobs.park(caller);
        let decided = move |()| Wait::on(keys, decided);
        let reason = tokio::select! {
            biased;
            decided = self.store.until_after(park, decided) => match decided {
                Ok(Some(floor)) => {
                    self.store.live(move |state| state.jobs.shown(caller, floor)).await;
                    Some(WakeReason::Event)
                }
                Ok(None) | Err(_) => None,
            },
            () = timeout => Some(WakeReason::Timeout),
        };
        let waited_s = entered.elapsed().as_secs();
        match reason {
            Some(reason) => Ok(WaitOutput { reason, waited_s }),
            None => Err(ToolError::cancelled()
                .operation(Operation::Wait, Subject::Job(caller))
                .with_result(ToolOutput::new(serde_json::json!(Unwoken::Cancelled {
                    waited_s
                })))),
        }
    }
}

// Tests run in real time: a paused clock would advance while the session core's
// thread works. A timeout under test fires through `time_out` once nothing else
// can end its wait.
#[cfg(test)]
pub(super) mod tests {
    use std::sync::Arc;

    use serde_json::Value;

    use super::super::*;
    use crate::agent::inbox::Coalescing;
    pub(super) use crate::agent::tests::{
        AssistantItem, Script, Step, activity_is, bounded, enqueue_prompts, ephemeral_session,
        found, job_found, jobs_found, models, provider_name, rendered, response, spawn_prompt,
        until,
    };
    pub(super) use crate::agent::tests::{Sent, SentPart};
    use crate::{
        job::{JobEnvelope, JobOutcome, JobSpec, JobState},
        tool::ToolOutput,
    };

    /// Gated steps, each served to the named model profile's next request.
    pub(in crate::agent) fn tracking_all(
        steps: Vec<(&'static str, Vec<AssistantItem>)>,
    ) -> Arc<Script> {
        let steps = steps
            .into_iter()
            .map(|(model, content)| Step::new(response(content)).model(model).gated());
        Script::new(steps, &Default::default())
    }

    pub(in crate::agent) fn tracking(steps: Vec<(&'static str, AssistantItem)>) -> Arc<Script> {
        tracking_all(steps.into_iter().map(|(m, c)| (m, vec![c])).collect())
    }

    pub(in crate::agent) fn call(id: &str, name: &str, arguments: Value) -> AssistantItem {
        call_at(0, id, name, arguments)
    }

    pub(in crate::agent) fn call_at(
        position: u32,
        id: &str,
        name: &str,
        arguments: Value,
    ) -> AssistantItem {
        AssistantItem::tool_call(id, position, ToolCall::new(id, name, arguments).unwrap())
    }

    pub(in crate::agent) fn answer() -> AssistantItem {
        AssistantItem::text("answer", 0, "done".to_owned())
    }

    /// A session whose "root" and "child" profiles are answered by `tracking`.
    pub(in crate::agent) async fn start(
        tracking: &Arc<Script>,
    ) -> (tempfile::TempDir, Arc<SessionHandle>) {
        start_coalescing(tracking, Coalescing::NONE).await
    }

    pub(super) async fn start_coalescing(
        tracking: &Arc<Script>,
        window: Coalescing,
    ) -> (tempfile::TempDir, Arc<SessionHandle>) {
        let root = tempfile::tempdir().unwrap();
        let profile = |model: &str| ModelProfile {
            hint: Some(model.parse().unwrap()),
            ..crate::tests::profile(model, true)
        };
        let harness = HarnessBuilder::new(root.path(), "test".parse().unwrap())
            .session_root(root.path().join("sessions"))
            .coalescing(window)
            .provider(
                provider_name("wait-test"),
                tracking.clone(),
                models([("root", profile("root")), ("child", profile("child"))]),
            )
            .default_model("wait-test/root".parse().unwrap())
            .build()
            .await
            .unwrap();
        (root, Arc::new(ephemeral_session(&harness).await))
    }

    pub(super) async fn running_job(session: &SessionHandle, owner: &AgentId, tool: &str) -> JobId {
        let tool = tool.to_owned();
        let running = move |job: &JobEnvelope| job.tool == tool && job.state == JobState::Running;
        job_found(session, owner, running).await
    }

    /// Once `owner`'s `nth` `wait` has parked, let `seconds` pass: what it waits
    /// for has happened, so only its timeout is left to end it. A wait that runs
    /// but has yet to park has set no deadline, and would count from after the
    /// time let pass here.
    pub(super) async fn time_out(
        session: &SessionHandle,
        owner: &AgentId,
        nth: usize,
        seconds: u64,
    ) {
        jobs_found(session, owner, [], move |state, jobs| {
            let mut waits = jobs.into_iter().filter(|job| job.tool == "wait");
            waits
                .nth(nth - 1)
                .filter(|job| state.jobs.parked(job.id))
                .map(drop)
        })
        .await;
        crate::tests::elapse(std::time::Duration::from_secs(seconds)).await;
    }

    /// The only non-root agent.
    pub(super) fn only_child(session: &SessionHandle) -> AgentId {
        let agents = session
            .runtime
            .agents(|agents| agents.within(&session.root));
        agents.into_iter().find(|id| *id != session.root).unwrap()
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
        let id = jobs.test_running(spec).await.into_test_id();
        let outcome = JobOutcome::Completed(ToolOutput::new(json!({"value":value})).into());
        jobs.finish(id, outcome).await.unwrap();
        id
    }

    pub(super) fn events(request: &ModelRequest) -> Vec<Value> {
        job_entries(request, false)
    }

    pub(super) fn agent_messages(request: &ModelRequest) -> Vec<Value> {
        job_entries(request, true)
    }

    /// Job events across the request's envelopes, as their presented JSON.
    fn job_entries(request: &ModelRequest, messages: bool) -> Vec<Value> {
        let blocks = request.messages().flat_map(|message| match message {
            Sent::User(content) => content.as_slice(),
            _ => &[],
        });
        let entries = blocks.flat_map(|block| match block {
            SentPart::Runtime { text } => (crate::session::JOB_EVENTS.body(text))
                .map(|json| serde_json::from_str::<Vec<Value>>(json).unwrap())
                .unwrap_or_default(),
            _ => Vec::new(),
        });
        entries
            .filter(|entry| (entry["kind"] == "message") == messages)
            .collect()
    }

    pub(super) async fn child_completed(session: &SessionHandle, job: JobId) {
        let snapshot = crate::agent::tests::until(session, job, |job| job.state.is_terminal());
        assert_eq!(snapshot.await.state, JobState::Completed);
    }
    /// A child's completion carries its final reply as the result.
    pub(super) fn assert_child_completion(event: &Value, job: JobId, answer: &str) {
        assert_eq!(event["id"], job.get());
        assert_eq!(event["state"], "completed");
        assert_eq!(event["result"]["reply"], answer, "{event}");
    }

    /// The view call `id` returned in `request`, and whether the call failed.
    pub(in crate::agent) fn call_view(request: &ModelRequest, id: &str) -> (bool, Value) {
        let results = request.messages().flat_map(|message| match message {
            Sent::Tool(results) => results.as_slice(),
            _ => &[],
        });
        let results = results
            .filter(|result| result.call_id == id)
            .collect::<Vec<_>>();
        assert_eq!(results.len(), 1, "call {id} must have exactly one result");
        (results[0].is_error, results[0].result.clone())
    }

    /// The native result of wait call `id` in `request`, and whether the call failed.
    fn wait_result(request: &ModelRequest, id: &str) -> (bool, Value) {
        let (failed, view) = call_view(request, id);
        (failed, view["result"].clone())
    }

    pub(super) fn assert_reason(request: &ModelRequest, id: &str, reason: &str) {
        let (failed, result) = wait_result(request, id);
        assert!(!failed, "wait failed: {result}");
        assert_eq!(result["reason"], reason, "{result}");
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
        let tracking = tracking(vec![
            ("root", call("first", "wait", json!({"timeout":1}))),
            ("root", call("second", "wait", json!({"timeout":1}))),
            ("root", answer()),
        ]);
        let (_root, session) = start(&tracking).await;
        let turn = spawn_prompt(&session, "start");
        tracking.pass(0).await;
        time_out(&session, &session.root, 1, 1).await;
        assert_reason(&tracking.pass(1).await, "first", "timeout");
        time_out(&session, &session.root, 2, 1).await;
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
        let tracking = tracking(vec![
            ("root", call("waiting", "wait", json!({}))),
            ("root", call("again", "wait", json!({"timeout":1}))),
            ("root", answer()),
        ]);
        let (window, gate) = Coalescing::held();
        let (_root, session) = start_coalescing(&tracking, window).await;
        let root = &session.root;
        let turn = spawn_prompt(&session, "start");
        tracking.pass(0).await;
        running_job(&session, root, "wait").await;
        let first = complete_background(&session, root, "first-output").await;
        let second = complete_background(&session, root, "second-output").await;
        gate.add_permits(1);
        let request = tracking.pass(1).await;
        assert_reason(&request, "waiting", "event");
        let notifications = events(&request);
        assert_notifications(
            &notifications,
            &[(first, "first-output"), (second, "second-output")],
        );
        time_out(&session, root, 2, 1).await;
        let next = tracking.pass(2).await;
        assert_reason(&next, "again", "timeout");
        // retained history must not get another copy
        assert_eq!(events(&next), notifications);
        bounded(turn).await.unwrap().unwrap();
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn completion_before_wait_registration_is_not_lost() {
        let tracking = tracking(vec![
            ("root", call("waiting", "wait", json!({}))),
            ("root", answer()),
        ]);
        let (_root, session) = start(&tracking).await;
        let turn = spawn_prompt(&session, "start");
        tracking.request(0).await;
        let job = complete_background(&session, &session.root, "already-ready").await;
        tracking.release(0);
        let next = tracking.pass(1).await;
        assert_reason(&next, "waiting", "event");
        assert_notifications(&events(&next), &[(job, "already-ready")]);
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
        let sleeper = json!({"source":"await tool.exec({command:['sleep','30']});", "bg":true});
        let tracking = tracking(vec![
            ("root", call("bg", "script", sleeper)),
            ("root", call("hold", "wait", json!({"timeout":null}))),
            ("root", answer()),
        ]);
        let (_root, session) = start(&tracking).await;
        let turn = spawn_prompt(&session, "start");
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
    #[tokio::test]
    async fn a_deferring_wait_wakes_when_its_holder_parks() {
        let first = json!({"source":"return (await tool.wait({timeout:30})).result.reason;"});
        // Holds the agent until a second elapses, then parks in a wait of its own.
        let second = json!({"source":"await sleep(1000); \
            await tool.wait({timeout:30}); return (await tool.wait({timeout:1})).result.reason;"});
        let tracking = tracking_all(vec![
            (
                "root",
                vec![
                    call("first", "script", first),
                    AssistantItem::tool_call(
                        "second",
                        1,
                        ToolCall::new("second", "script", second).unwrap(),
                    ),
                ],
            ),
            ("root", vec![answer()]),
        ]);
        let (_root, session) = start(&tracking).await;
        let turn = spawn_prompt(&session, "start");
        tracking.pass(0).await;
        let scripts = async || {
            let jobs = session.runtime.jobs.list(&session.root).await.unwrap();
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
        // The input's batch releases, yet the wait defers to the sleeping script.
        let root = session.root.clone();
        let key = Key::Agent(root.clone());
        let released = Wait::on([key], move |state| {
            let window = &state.agents.get(&root)?.window;
            (window.ready_inputs != window.observed_inputs).then_some(())
        });
        bounded(session.runtime.store.until(released))
            .await
            .unwrap();
        assert!(
            scripts()
                .await
                .iter()
                .all(|job| job.state == JobState::Running)
        );
        crate::tests::elapse(std::time::Duration::from_secs(1)).await;
        let done = |job: &crate::job::JobEnvelope| job.tool == "script" && job.state.is_terminal();
        let finished = job_found(&session, &session.root, done).await;
        assert_eq!(Some(finished), deferring);
        // The first to finish is the deferring script, told about the input while
        // the other is still parked in its second wait.
        let others = scripts().await.into_iter().filter(|job| job.id != finished);
        assert!(others.into_iter().all(|job| job.state == JobState::Running));
        let output = session
            .runtime
            .jobs
            .wait(finished, false)
            .await
            .unwrap()
            .output;
        let value = output.as_ref().map(|output| &output["value"]);
        assert_eq!(value, Some(&json!("event")), "{output:?}");
        time_out(&session, &session.root, 3, 1).await;
        tracking.request(1).await;
        bounded(queued).await.unwrap().unwrap();
        tracking.release(1);
        bounded(turn).await.unwrap().unwrap();
        session.shutdown().await.unwrap();
    }

    /// Waits must not defer to each other: both wake on the same queued input,
    /// which reaches the next request rather than being consumed by the wake, and
    /// leaves nothing for a later wait.
    #[tokio::test]
    async fn concurrent_waits_wake_on_queued_input_without_consuming_it() {
        let second = ToolCall::new("second", "wait", json!({"timeout":null})).unwrap();
        let tracking = tracking_all(vec![
            (
                "root",
                vec![
                    call("first", "wait", json!({"timeout":null})),
                    AssistantItem::tool_call("second", 1, second),
                ],
            ),
            ("root", vec![call("again", "wait", json!({"timeout":1}))]),
            ("root", vec![answer()]),
        ]);
        let (_root, session) = start(&tracking).await;
        let turn = spawn_prompt(&session, "start");
        tracking.pass(0).await;
        let root = session.root.clone();
        found(&session, [Key::Agent(root.clone())], move |state| {
            let waits = state
                .jobs
                .views()
                .into_values()
                .filter(|job| job.agent == root);
            let running = waits.filter(|job| job.tool == "wait" && job.state == JobState::Running);
            (running.count() >= 2).then_some(())
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
        assert!(rendered(&request).contains("concurrent-wake-marker"));
        tracking.release(1);
        time_out(&session, &session.root, 3, 1).await;
        assert_reason(&tracking.pass(2).await, "again", "timeout");
        bounded(turn).await.unwrap().unwrap();
        session.shutdown().await.unwrap();
    }

    /// Queued input stays unconsumed while a script runs; it must still wake the
    /// script's waits only once.
    #[tokio::test]
    async fn queued_input_does_not_respin_a_script_wait() {
        let source = "const first = (await tool.wait({timeout:60})).result.reason; \
            const second = (await tool.wait({timeout:1})).result.reason; return [first, second];";
        let tracking = tracking(vec![
            ("root", call("run", "script", json!({"source": source}))),
            ("root", answer()),
        ]);
        let (_root, session) = start(&tracking).await;
        let turn = spawn_prompt(&session, "start");
        tracking.pass(0).await;
        let script = running_job(&session, &session.root, "script").await;
        running_job(&session, &session.root, "wait").await;
        let prompt = queued("queued-respin-marker");
        let queued = tokio::spawn({
            let session = session.clone();
            async move { enqueue_prompts(&session, vec![prompt]).await.pop().unwrap() }
        });
        time_out(&session, &session.root, 2, 1).await;
        let request = tracking.request(1).await;
        bounded(queued).await.unwrap().unwrap();
        let output = session
            .runtime
            .jobs
            .wait(script, false)
            .await
            .unwrap()
            .output;
        let expected = json!(["event", "timeout"]);
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

    /// An event ready together with a script `wait`'s timeout is reported, by that
    /// wait or by the next: one that times out leaves it unseen.
    #[tokio::test]
    async fn an_event_ready_with_the_timeout_reaches_the_script() {
        let source = "const first = (await tool.wait({timeout:1})).result.reason; \
            const second = (await tool.wait({timeout:1})).result.reason; return [first, second];";
        let tracking = tracking(vec![
            ("root", call("run", "script", json!({"source": source}))),
            ("root", answer()),
        ]);
        let (_root, session) = start(&tracking).await;
        let (jobs, store, root) = (&session.runtime.jobs, &session.runtime.store, &session.root);
        let turn = spawn_prompt(&session, "start");
        tracking.pass(0).await;
        let script = running_job(&session, root, "script").await;
        let wait = running_job(&session, root, "wait").await;
        // Parked, the wait has set its deadline.
        let owner = root.clone();
        let parked = Wait::on([Key::Agent(owner.clone())], move |state| {
            let held = state.jobs.live_work(&owner).holding.is_some();
            (state.jobs.parked(wait) && !held).then_some(())
        });
        bounded(store.until(parked)).await.unwrap();
        // Input arrives in the timer pass that fires the wait's timeout, ahead of
        // the wait, which then finds both ready.
        tokio::time::pause();
        let arrives = tokio::time::sleep(std::time::Duration::from_millis(500));
        let (store, owner) = (store.clone(), root.clone());
        let input = tokio::spawn(async move {
            arrives.await;
            let (applied, decided) = std::sync::mpsc::channel();
            store.enqueue(move |state| {
                let input = super::super::queue::QueuedInput {
                    content: vec![UserPart::Text {
                        text: "racing input".into(),
                    }],
                    attachments: Vec::new(),
                    options: Default::default(),
                    cancellation: Default::default(),
                    committed: tokio::sync::oneshot::channel().0,
                };
                state
                    .agents
                    .send(&owner, AgentCommand::QueuedInputs(vec![input]));
                state.agents.get_mut(&owner).unwrap().window.release();
                state.defer(move || applied.send(()).unwrap());
            });
            // Holding this runtime's only thread until the wait has decided.
            decided.recv().unwrap();
        });
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        tokio::time::resume();
        bounded(input).await.unwrap();
        // Whichever wait reported it, the other only times out.
        let mut elapsed = false;
        loop {
            let settled = move |state: &CoreState, jobs: Vec<crate::job::JobEnvelope>| {
                let script = crate::job::tests::job_snapshot(state, script)?;
                if script.state.is_terminal() {
                    return Some(true);
                }
                let mut waits = jobs.iter().filter(|job| job.tool == "wait");
                // Parked, its deadline is set: time let pass sooner would not count.
                let second = waits.nth(1)?.id;
                (!elapsed && state.jobs.parked(second)).then_some(false)
            };
            if jobs_found(&session, root, [Key::Job(script)], settled).await {
                break;
            }
            crate::tests::elapse(std::time::Duration::from_secs(1)).await;
            elapsed = true;
        }
        let output = jobs.wait(script, false).await.unwrap().output.unwrap();
        let event = json!("event");
        let reasons = output["value"].as_array().unwrap();
        assert_eq!(
            reasons.iter().filter(|reason| **reason == event).count(),
            1,
            "{output}"
        );
        tracking.pass(1).await;
        bounded(turn).await.unwrap().unwrap();
        session.shutdown().await.unwrap();
    }

    /// A script's wait times out rather than being woken by the script's own job.
    #[tokio::test]
    async fn script_waits_do_not_self_wake() {
        let (_root, session) = start(&tracking(vec![])).await;
        let source = "return (await tool.wait({timeout:1})).result.reason;";
        let timeout = time_out(&session, &session.root, 1, 1);
        let (output, ()) = tokio::join!(bounded(session.run_script(source)), timeout);
        assert_eq!(output.unwrap().value, json!({"value":"timeout"}));
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cancelling_an_indefinite_wait_unblocks_the_agent() {
        let tracking = tracking(vec![
            ("root", call("waiting", "wait", json!({}))),
            ("root", answer()),
        ]);
        let (_root, session) = start(&tracking).await;
        let turn = spawn_prompt(&session, "start");
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

    /// Interrupting before `agent` runs tools races the guard.
    async fn running_tools(session: &SessionHandle, agent: &AgentId) {
        let tools = |state: &_| matches!(state, crate::session::AgentActivity::Tools);
        activity_is(session, agent, tools).await;
    }

    #[tokio::test]
    async fn interrupted_wait_reports_how_long_it_waited() {
        let tracking = tracking(vec![
            ("root", call("waiting", "wait", json!({}))),
            ("root", answer()),
        ]);
        let (_root, session) = start(&tracking).await;
        let turn = spawn_prompt(&session, "start");
        tracking.pass(0).await;
        // Nothing can end this wait: the seconds let pass are only waited.
        time_out(&session, &session.root, 1, 2).await;
        running_tools(&session, &session.root).await;
        assert_eq!(bounded(session.interrupt()).await, 1);
        assert!(bounded(turn).await.unwrap().is_err());
        let resumed = tokio::spawn({
            let session = session.clone();
            async move { session.continue_turn().await }
        });
        let (failed, result) = wait_result(&tracking.request(1).await, "waiting");
        assert!(failed);
        assert_eq!(result["reason"], "cancelled", "{result}");
        assert!(result["waited_s"].as_u64() >= Some(2), "{result}");
        tracking.release(1);
        assert_eq!(bounded(resumed).await.unwrap().unwrap(), "done");
        session.shutdown().await.unwrap();
    }

    /// A model-delegated child is retained while the script blocking the same turn
    /// is cancelled with the wait it blocks on. (A child the script launched would
    /// die with it.)
    #[tokio::test]
    async fn interrupt_cancels_blocking_tools_and_retains_delegated_children() {
        let delegate = json!({"prompt":"work", "model":"wait-test/child", "name":"kid"});
        let blocked = json!({"source":"await tool.wait({timeout:30});"});
        let tracking = tracking_all(vec![
            (
                "root",
                vec![
                    call("kid", "agent", delegate),
                    AssistantItem::tool_call(
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
        let turn = spawn_prompt(&session, "start");
        tracking.pass(0).await;
        let script = running_job(&session, &session.root, "script").await;
        let waiting = running_job(&session, &session.root, "wait").await;
        let delegated = running_job(&session, &session.root, "agent").await;
        tracking.request(1).await;
        let child = only_child(&session);
        tracking.release(1);
        running_job(&session, &child, "wait").await;
        running_tools(&session, &session.root).await;
        assert_eq!(bounded(session.interrupt()).await, 2);
        // Both outcomes are journaled asynchronously; settle before comparing.
        for job in [script, waiting] {
            until(&session, job, |job| job.state == JobState::Cancelled).await;
        }
        until(&session, delegated, |job| {
            job.state == JobState::Interrupted
        })
        .await;
        assert!(!tracking.requested_from(2));
        // A suspended child keeps the parent's `agent` call waiting for `continue`;
        // cancelling it releases the held turn, which runs on.
        session.cancel_job(delegated).await.unwrap();
        tracking.pass(2).await;
        assert_eq!(bounded(turn).await.unwrap().unwrap(), "done");
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn idle_agent_batches_background_notifications_without_a_wait_call() {
        let tracking = tracking(vec![("root", answer()); 3]);
        let (window, gate) = Coalescing::held();
        let (_root, session) = start_coalescing(&tracking, window).await;
        let root = &session.root;
        let initial = spawn_prompt(&session, "start");
        tracking.pass(0).await;
        bounded(initial).await.unwrap().unwrap();

        let first = complete_background(&session, root, "idle-first-output").await;
        let second = complete_background(&session, root, "idle-second-output").await;
        gate.add_permits(1);
        let next = tracking.request(1).await;
        let notifications = events(&next);
        let expected = [(first, "idle-first-output"), (second, "idle-second-output")];
        assert_notifications(&notifications, &expected);

        let barrier = spawn_prompt(&session, "idle-batch-barrier");
        tracking.release(1);
        let final_request = tracking.pass(2).await;
        let messages = rendered(&final_request);
        assert!(messages.contains("idle-batch-barrier"));
        // completed output must not be injected twice
        assert_eq!(events(&final_request), notifications);
        bounded(barrier).await.unwrap().unwrap();
        session.shutdown().await.unwrap();
    }
    /// A sibling call that fails before it has a job is answered with the same batch.
    #[tokio::test]
    async fn a_jobless_sibling_answer_resolves_its_responses_wait() {
        let tracking = tracking_all(vec![
            (
                "root",
                vec![
                    call_at(0, "unknown", "no_such_tool", json!({})),
                    call_at(1, "waiting", "wait", json!({})),
                ],
            ),
            ("root", vec![answer()]),
        ]);
        let (_root, session) = start(&tracking).await;
        let turn = spawn_prompt(&session, "start");
        tracking.pass(0).await;
        assert_reason(&tracking.pass(1).await, "waiting", "event");
        assert_eq!(bounded(turn).await.unwrap().unwrap(), "done");
        session.shutdown().await.unwrap();
    }
}
