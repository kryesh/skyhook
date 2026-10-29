//! Durable, exclusive child-agent progress, which the job fold counts.
//!
//! Count committed assistant messages, not requests/usage (which include retries and
//! compaction), and owned job creations, not assistant tool-call blocks (which miss
//! calls launched inside scripts). Retained resumes keep the same agent identity.
use serde::{Deserialize, Serialize};

use crate::{
    identity::JobId,
    session::{StateJob, StateJobKind},
};

/// A child agent's durable progress: committed turns and jobs it created.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct AgentProgress {
    pub turns: u64,
    pub tool_calls: u64,
}

/// Children follow agent ownership, not job parentage: scripts may launch agent
/// jobs on their owner's behalf. Only active agent jobs appear below the root list.
pub(super) fn active_job(
    id: JobId,
    jobs: &super::Jobs,
    capabilities: &crate::tool::policy::CapabilitySet,
    now_millis: i64,
    path: &mut std::collections::HashSet<JobId>,
) -> StateJob {
    let entry = jobs.get(id).expect("an active job is held");
    let child = entry.child();
    let launched = child.and_then(|child| child.agent.as_ref());
    let mut children = Vec::new();
    path.insert(id);
    if let Some(agent) = launched {
        let child_ids = (jobs.unfinished(agent))
            .filter(|(job, child)| child.child().is_some() && !path.contains(job))
            .map(|(job, _)| job)
            .collect::<Vec<_>>();
        for child in child_ids {
            children.push(active_job(child, jobs, capabilities, now_millis, path));
        }
    }
    path.remove(&id);
    StateJob {
        job: id,
        kind: match child {
            Some(child) => StateJobKind::Agent {
                progress: child.progress,
            },
            None => StateJobKind::Tool {
                tool: entry.tool.clone(),
            },
        },
        name: entry.name.clone(),
        state: entry.state().presented(),
        target: capabilities.visible_target(&entry.location.target).cloned(),
        workspace: entry.location.workspace.clone(),
        age_seconds: u64::try_from(now_millis.saturating_sub(entry.created_at_millis)).unwrap_or(0)
            / 1_000,
        children,
    }
}

#[cfg(test)]
mod tests {
    use crate::job::*;
    use crate::provider::protocol::AssistantItem;
    use crate::session::{Message, StateJob, StateJobKind};
    use crate::tool::policy::Capability;
    use tokio::sync::Notify;

    fn progress_of(job: &StateJob) -> Option<AgentProgress> {
        match job.kind {
            StateJobKind::Agent { progress } => Some(progress),
            StateJobKind::Tool { .. } => None,
        }
    }

    fn location(target: &str) -> ExecutionLocation {
        ExecutionLocation::named(target.parse().unwrap(), format!("/{target}/work").into())
    }

    async fn started(jobs: &JobManager, agent: &AgentId, owner: JobId, target: &str) {
        let event = crate::session::tests::child_started(Some(owner), location(target));
        jobs.test_append(agent.clone(), event).await;
    }

    async fn committed(jobs: &JobManager, agent: &AgentId) {
        let message = Message::Assistant(vec![AssistantItem::text("answer", 0, "done")]);
        jobs.test_append(agent.clone(), SessionEvent::MessageCommitted { message })
            .await;
    }

    fn snapshot(jobs: &JobManager, owner: &AgentId, targets: bool) -> Vec<StateJob> {
        let mut capabilities = CapabilitySet::default();
        if targets {
            capabilities.insert(Capability::Targets);
        }
        jobs.active_states(owner, &capabilities, i64::MAX)
    }

    fn agent_spec(agent: &AgentId, parent: Option<JobId>, target: &str) -> JobSpec {
        JobSpec {
            parent,
            role: JobRole::Agent,
            location: location(target),
            ..JobSpec::test(agent.clone(), "delegate")
        }
    }

    fn progress(turns: u64, tool_calls: u64) -> Option<AgentProgress> {
        Some(AgentProgress { turns, tool_calls })
    }

    #[tokio::test]
    async fn active_progress_is_recursive_exclusive_and_location_filtered() {
        let session = crate::tests::TestRuntime::new().await;
        let (jobs, root) = (session.jobs.clone(), session.agent.clone());
        let child = root.child(1);
        let grandchild = child.child(1);
        let great_grandchild = grandchild.child(1);
        let delegated = jobs.test_lease(agent_spec(&root, None, "child")).await;
        let ordinary = jobs.test_lease(JobSpec::test(root.clone(), "agent")).await;
        // Even queued agents without AgentStarted have zero counters; tools do not.
        let initial = snapshot(&jobs, &root, false);
        assert_eq!(progress_of(&initial[0]), progress(0, 0));
        assert!(initial[0].children.is_empty());
        assert_eq!(progress_of(&initial[1]), None);
        started(&jobs, &child, delegated.id(), "child").await;
        committed(&jobs, &child).await;
        committed(&jobs, &child).await;
        committed(&jobs, &root).await; // Not child progress.
        let script = jobs
            .test_lease(JobSpec::test(child.clone(), "script"))
            .await;
        let spec = JobSpec {
            parent: Some(script.id()),
            ..JobSpec::test(child.clone(), "exec")
        };
        let nested = jobs.test_lease(spec).await;
        jobs.test_finish(nested.id(), Value::Null).await;
        let delegated_again = jobs
            .test_approving(agent_spec(&child, Some(script.id()), "grandchild"))
            .await;
        started(&jobs, &grandchild, delegated_again.id(), "grandchild").await;
        committed(&jobs, &grandchild).await;
        let deepest = jobs
            .test_lease(agent_spec(&grandchild, None, "deepest"))
            .await;
        started(&jobs, &great_grandchild, deepest.id(), "deepest").await;
        committed(&jobs, &great_grandchild).await;
        let _leaf = jobs
            .test_create(JobSpec::test(great_grandchild.clone(), "read"))
            .await;
        let terminal = jobs.test_lease(agent_spec(&child, None, "child")).await;
        jobs.test_finish(terminal.id(), Value::Null).await;

        let state = snapshot(&jobs, &root, false);
        assert_eq!(state.len(), 2);
        assert_eq!(
            (state[0].job, state[1].job),
            (delegated.id(), ordinary.id())
        );
        // Script, nested exec, and two agent jobs.
        assert_eq!(progress_of(&state[0]), progress(2, 4));
        assert_eq!(state[0].children.len(), 1);
        let grand = &state[0].children[0];
        assert_eq!(grand.job, delegated_again.id());
        assert_eq!(grand.state, JobState::Queued); // Approval stays host-only.
        assert_eq!(progress_of(grand), progress(1, 1));
        assert_eq!(grand.workspace, std::path::Path::new("/grandchild/work"));
        let deepest_state = &grand.children[0];
        assert_eq!(deepest_state.job, deepest.id());
        assert_eq!(progress_of(deepest_state), progress(1, 1));
        assert!(deepest_state.children.is_empty());
        let visible = snapshot(&jobs, &root, true);
        let mut visible = visible.first();
        for (entry, target) in [
            (&state[0], "child"),
            (grand, "grandchild"),
            (deepest_state, "deepest"),
        ] {
            assert!(entry.target.is_none());
            let shown = visible.expect("a visible job at every depth");
            assert_eq!(
                shown.target.as_ref().map(crate::target::TargetRef::as_str),
                Some(target)
            );
            visible = shown.children.first();
        }
        let future = jobs.active_states(&root, &CapabilitySet::default(), 0);
        assert_eq!(future[0].age_seconds, 0);
    }

    #[tokio::test]
    async fn progress_survives_retained_resume_and_replay() {
        let session = crate::tests::TestRuntime::new().await;
        let (jobs, root) = (session.jobs.clone(), session.agent.clone());
        let child = root.child(1);
        let spec = JobSpec {
            accepts_input: true,
            ..agent_spec(&root, None, "child")
        };
        let delegated = jobs.test_lease(spec).await;
        started(&jobs, &child, delegated.id(), "child").await;
        let turn = async |jobs: JobManager, child: AgentId| {
            committed(&jobs, &child).await;
            let tool = jobs.test_lease(JobSpec::test(child, "read")).await;
            jobs.test_finish(tool.id(), Value::Null).await;
        };
        turn(jobs.clone(), child.clone()).await;
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let turned = Arc::new(Notify::new());
        let handler: ResumeHandler = Arc::new({
            let (jobs, child) = (jobs.clone(), child.clone());
            let (release, turned) = (release.clone(), turned.clone());
            move |_, _| {
                let (jobs, child) = (jobs.clone(), child.clone());
                let (release, turned) = (release.clone(), turned.clone());
                Box::pin(async move {
                    turn(jobs, child).await;
                    turned.notify_one();
                    release.acquire().await.unwrap().forget();
                    Ok(ToolOutput::default())
                })
            }
        });
        jobs.test_resume_with(delegated.id(), handler);
        let turns = |jobs: &[StateJob]| progress_of(&jobs[0]).map(|progress| progress.turns);
        assert_eq!(turns(&snapshot(&jobs, &root, false)), Some(1));
        jobs.test_finish(delegated.id(), Value::Null).await;
        assert!(snapshot(&jobs, &root, false).is_empty());
        jobs.send(delegated.id(), Value::Null).await.unwrap();
        crate::tests::bounded(turned.notified()).await;
        let resumed = progress_of(&snapshot(&jobs, &root, false)[0]);
        assert_eq!(resumed, progress(2, 2));
        release.add_permits(1);
        crate::tests::bounded(jobs.wait(delegated.id(), true))
            .await
            .unwrap();

        let restored = jobs.test_replay();
        // Completed agents remain omitted after replay, but retain progress for resumption.
        assert!(snapshot(&restored, &root, false).is_empty());
        let retained = restored.facts(|jobs| jobs.get(delegated.id())?.child().map(|c| c.progress));
        assert_eq!(retained, progress(2, 2));
    }
}
