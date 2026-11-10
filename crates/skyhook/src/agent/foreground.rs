//! Moving a turn's blocked foreground calls to the background, so a call that runs
//! long gives way to what arrives for its agent, and none holds it indefinitely.

use std::time::Duration;

use tokio::time::Instant;

use super::SessionRuntime;
use crate::identity::AgentId;
use crate::{
    job::MoveTrigger,
    session::{Key, Wait},
};
use skyhook_worker::job::JobId;

/// How long a turn's foreground calls hold their agent. The notices a moved call
/// returns name these durations.
#[derive(Clone, Copy, Debug)]
pub(super) struct Limits {
    /// Until then, what arrives waits for the calls; after it, it moves them.
    pub(super) event_after: Duration,
    /// Then every call moves but a child agent's, whose child reports on its own.
    pub(super) limit: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            event_after: Duration::from_secs(30),
            limit: Duration::from_secs(3 * 60),
        }
    }
}

impl SessionRuntime {
    /// Move `calls`, the movable foreground calls of `agent`'s response that began at
    /// `began`, to the background together once they have run `event_after` and an
    /// outcome, a question or input is pending for the agent, or at the limit. Ends
    /// once none blocks the agent; the turn drops it once its calls return.
    pub(super) async fn move_blocked(&self, agent: &AgentId, calls: Vec<JobId>, began: Instant) {
        if calls.is_empty() {
            return;
        }
        let Limits { event_after, limit } = self.harness.foreground;
        tokio::time::sleep_until(began + event_after).await;
        let mut limit = Some(began + limit);
        loop {
            let pending = {
                let (agent, calls) = (agent.clone(), calls.clone());
                Wait::on([Key::Agent(agent.clone())], move |state| {
                    if !calls.iter().any(|job| state.jobs.blocks(*job)) {
                        return Some(false);
                    }
                    let input = (state.agents.get(&agent)).is_some_and(|facts| {
                        facts.window.ready_inputs != facts.window.observed_inputs
                    });
                    (input || state.jobs.notified(&agent)).then_some(true)
                })
            };
            let trigger = tokio::select! {
                pending = self.store.until(pending) => match pending {
                    Ok(true) => MoveTrigger::Event,
                    _ => return,
                },
                () = tokio::time::sleep_until(limit.unwrap_or(began)), if limit.is_some() => {
                    MoveTrigger::Limit
                }
            };
            if self.jobs.move_calls(calls.clone(), trigger).await.is_err()
                || trigger == MoveTrigger::Event
            {
                return;
            }
            limit = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::{Value, json};

    use super::*;
    use crate::{
        agent::{
            HarnessBuilder, SessionHandle,
            queue::QueuedPrompt,
            tests::{
                Script, bounded, enqueue_prompts, ephemeral_session, jobs_found, models,
                provider_name, spawn_prompt,
            },
            wait::tests::{answer, call_at, call_view, tracking_all},
        },
        job::{JobState, Notice},
        provider::{model::ModelProfile, protocol::ModelRequest},
    };

    const SLEEP: fn() -> Value = || json!({"command": ["sleep", "30"]});

    /// A session whose "root" and "child" profiles `tracking` answers, under `limits`.
    async fn start(tracking: &Arc<Script>, limits: Limits) -> (tempfile::TempDir, SessionHandle) {
        let root = tempfile::tempdir().unwrap();
        let profile = |model: &str| ModelProfile {
            hint: Some(model.parse().unwrap()),
            ..crate::tests::profile(model, true)
        };
        let harness = HarnessBuilder::new(root.path(), "test".parse().unwrap())
            .session_root(root.path().join("sessions"))
            .coalescing(super::super::inbox::Coalescing::NONE)
            .foreground(limits)
            .provider(
                provider_name("wait-test"),
                tracking.clone(),
                models([("root", profile("root")), ("child", profile("child"))]),
            )
            .default_model("wait-test/root".parse().unwrap())
            .build()
            .await
            .unwrap();
        let session = ephemeral_session(&harness).await;
        (root, session)
    }

    /// Call `id`'s view in `request`: its state and notice.
    fn moved(request: &ModelRequest, id: &str) -> (Value, Value) {
        let (failed, view) = call_view(request, id);
        assert!(!failed, "{view}");
        (
            view["state"].clone(),
            view["presentation"]["notice"].clone(),
        )
    }

    /// Input arriving once the response's calls have run long enough moves them all to
    /// the background together, and the next request carries the input.
    #[tokio::test]
    async fn an_event_moves_every_blocked_call_together() {
        let limits = Limits {
            event_after: Duration::ZERO,
            limit: Duration::from_secs(3600),
        };
        let tracking = tracking_all(vec![
            (
                "root",
                vec![
                    call_at(0, "a", "exec", SLEEP()),
                    call_at(1, "b", "exec", SLEEP()),
                ],
            ),
            ("root", vec![answer()]),
        ]);
        let (_root, session) = start(&tracking, limits).await;
        let turn = spawn_prompt(&session, "start");
        tracking.pass(0).await;
        let root = session.root.clone();
        jobs_found(&session, &root, [], |_, jobs| {
            let running = jobs.iter().filter(|job| job.state == JobState::Running);
            (running.count() == 2).then_some(())
        })
        .await;
        let steer = QueuedPrompt {
            text: "steer".into(),
            ..Default::default()
        };
        let queued = tokio::spawn({
            let session = session.clone();
            async move { enqueue_prompts(&session, vec![steer]).await.pop().unwrap() }
        });
        let request = tracking.request(1).await;
        bounded(queued).await.unwrap().unwrap();
        let notice = json!(Notice::MovedOnEvent.to_string());
        for id in ["a", "b"] {
            assert_eq!(
                moved(&request, id),
                (json!("running"), notice.clone()),
                "{id}"
            );
        }
        assert!(format!("{request:?}").contains("steer"));
        tracking.release(1);
        bounded(turn).await.unwrap().unwrap();
        session.shutdown().await.unwrap();
    }

    /// A background job's outcome moves a blocked call too.
    #[tokio::test]
    async fn a_background_outcome_moves_a_blocked_call() {
        let limits = Limits {
            event_after: Duration::ZERO,
            limit: Duration::from_secs(3600),
        };
        let quick = json!({"command": ["true"], "bg": true});
        let tracking = tracking_all(vec![
            (
                "root",
                vec![
                    call_at(0, "a", "exec", SLEEP()),
                    call_at(1, "quick", "exec", quick),
                ],
            ),
            ("root", vec![answer()]),
        ]);
        let (_root, session) = start(&tracking, limits).await;
        let turn = spawn_prompt(&session, "start");
        tracking.pass(0).await;
        let request = tracking.request(1).await;
        let notice = json!(Notice::MovedOnEvent.to_string());
        assert_eq!(moved(&request, "a"), (json!("running"), notice));
        tracking.release(1);
        bounded(turn).await.unwrap().unwrap();
        session.shutdown().await.unwrap();
    }

    /// At the limit a call moves with nothing pending, but a child agent's call beside
    /// it keeps the agent until its child answers.
    #[tokio::test]
    async fn the_limit_moves_all_but_a_child_agents_call() {
        let limits = Limits {
            event_after: Duration::ZERO,
            limit: Duration::from_millis(50),
        };
        let child = json!({"prompt": "child task", "model": "wait-test/child"});
        let tracking = tracking_all(vec![
            (
                "root",
                vec![
                    call_at(0, "a", "exec", SLEEP()),
                    call_at(1, "kid", "agent", child),
                ],
            ),
            ("child", vec![answer()]),
            ("root", vec![answer()]),
        ]);
        let (_root, session) = start(&tracking, limits).await;
        let turn = spawn_prompt(&session, "start");
        tracking.pass(0).await;
        let root = session.root.clone();
        let kid = jobs_found(&session, &root, [], |_, jobs| {
            jobs.iter()
                .find(|job| job.tool == "exec" && job.moved == Some(MoveTrigger::Limit))?;
            let kid = jobs.iter().find(|job| job.tool == "agent")?;
            Some((kid.moved, kid.state))
        })
        .await;
        assert_eq!(kid, (None, JobState::Running));
        tracking.pass(1).await;
        let request = tracking.request(2).await;
        let at_limit = json!(Notice::MovedAtLimit.to_string());
        assert_eq!(moved(&request, "a"), (json!("running"), at_limit));
        assert_eq!(
            call_view(&request, "kid").1["presentation"]["notice"],
            Value::Null
        );
        tracking.release(2);
        bounded(turn).await.unwrap().unwrap();
        session.shutdown().await.unwrap();
    }
}
