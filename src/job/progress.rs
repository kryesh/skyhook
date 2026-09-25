//! Incremental projection of durable, exclusive child-agent progress.
//!
//! Count committed assistant messages, not requests/usage (which include retries and
//! compaction), and owned job creations, not assistant tool-call blocks (which miss
//! calls launched inside scripts). Retained resumes keep the same agent identity.
use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::{
    identity::{AgentId, JobId},
    session::{EventRecord, Message, RecordSeq, SessionEvent, StateJob, StateJobKind},
};

/// A child agent's durable progress: committed turns and jobs it created.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct AgentProgress {
    pub turns: u64,
    pub tool_calls: u64,
}

#[derive(Default)]
pub(super) struct Progress {
    pub sequence: RecordSeq,
    pub agents: HashMap<JobId, AgentId>,
    counts: HashMap<AgentId, AgentProgress>,
}

impl Progress {
    pub fn project(&mut self, records: &[EventRecord]) {
        for record in records {
            if record.sequence <= self.sequence {
                continue;
            }
            match &record.event {
                SessionEvent::AgentStarted {
                    owner_job: Some(job),
                    ..
                } => {
                    self.agents.insert(*job, record.agent.clone());
                }
                SessionEvent::MessageCommitted {
                    message: Message::Assistant(_),
                } => {
                    self.counts.entry(record.agent.clone()).or_default().turns += 1;
                }
                SessionEvent::JobCreated { .. } => {
                    self.counts
                        .entry(record.agent.clone())
                        .or_default()
                        .tool_calls += 1;
                }
                _ => {}
            }
            self.sequence = record.sequence;
        }
    }

    pub fn for_job(&self, job: JobId) -> AgentProgress {
        self.agents
            .get(&job)
            .and_then(|agent| self.counts.get(agent))
            .copied()
            .unwrap_or_default()
    }
}

/// Children follow agent ownership, not job parentage: scripts may launch agent
/// jobs on their owner's behalf. Only active agent jobs appear below the root list.
pub(super) fn active_job(
    id: JobId,
    jobs: &HashMap<JobId, super::JobEntry>,
    progress: &Progress,
    capabilities: &crate::tool::policy::CapabilitySet,
    now_millis: i64,
    path: &mut std::collections::HashSet<JobId>,
) -> StateJob {
    let entry = &jobs[&id];
    let is_agent = entry.child().is_some();
    let mut children = Vec::new();
    path.insert(id);
    if is_agent && let Some(agent) = progress.agents.get(&id) {
        let mut child_ids = jobs
            .iter()
            .filter(|(job, child)| {
                &child.agent == agent
                    && child.end().is_none()
                    && child.child().is_some()
                    && !path.contains(job)
            })
            .map(|(job, _)| *job)
            .collect::<Vec<_>>();
        child_ids.sort_unstable();
        for child in child_ids {
            children.push(active_job(
                child,
                jobs,
                progress,
                capabilities,
                now_millis,
                path,
            ));
        }
    }
    path.remove(&id);
    StateJob {
        job: id,
        kind: if is_agent {
            StateJobKind::Agent {
                progress: progress.for_job(id),
            }
        } else {
            StateJobKind::Tool {
                tool: entry.tool.clone(),
            }
        },
        name: entry.name.clone().map(String::from),
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

    fn progress_of(job: &StateJob) -> Option<AgentProgress> {
        match job.kind {
            StateJobKind::Agent { progress } => Some(progress),
            StateJobKind::Tool { .. } => None,
        }
    }

    async fn started(jobs: &JobManager, agent: &AgentId, owner: JobId, target: &str) {
        let location =
            ExecutionLocation::named(target.parse().unwrap(), format!("/{target}/work").into());
        let event = crate::session::fixture::child_started(Some(owner), location.clone());
        jobs.test_append(agent.clone(), event).await;
        jobs.set_agent_location(owner, location).await.unwrap();
    }

    async fn committed(jobs: &JobManager, agent: &AgentId) {
        let message = Message::Assistant(vec![AssistantItem::text("answer", 0, "done")]);
        jobs.test_append(agent.clone(), SessionEvent::MessageCommitted { message })
            .await;
    }

    async fn snapshot(jobs: &JobManager, owner: &AgentId, targets: bool) -> Vec<StateJob> {
        let mut capabilities = CapabilitySet::default();
        if targets {
            capabilities.insert(Capability::Targets);
        }
        jobs.active_states(owner, &capabilities, i64::MAX).await
    }

    fn agent_spec(agent: &AgentId, parent: Option<JobId>) -> JobSpec {
        JobSpec {
            parent,
            role: JobRole::Agent,
            ..JobSpec::test(agent.clone(), "delegate")
        }
    }

    fn progress(turns: u64, tool_calls: u64) -> Option<AgentProgress> {
        Some(AgentProgress { turns, tool_calls })
    }

    #[tokio::test]
    async fn active_progress_is_recursive_exclusive_and_location_filtered() {
        let session = crate::session::fixture::MemorySession::new().await;
        let (store, root) = (session.store.clone(), session.agent.clone());
        let child = root.child(1);
        let grandchild = child.child(1);
        let great_grandchild = grandchild.child(1);
        let jobs = JobManager::new(store);
        let delegated = jobs.test_lease(agent_spec(&root, None)).await;
        let ordinary = jobs.test_lease(JobSpec::test(root.clone(), "agent")).await;
        // Even queued agents without AgentStarted have zero counters; tools do not.
        let initial = snapshot(&jobs, &root, false).await;
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
            .test_approving(agent_spec(&child, Some(script.id())))
            .await;
        started(&jobs, &grandchild, delegated_again.id(), "grandchild").await;
        committed(&jobs, &grandchild).await;
        let deepest = jobs.test_lease(agent_spec(&grandchild, None)).await;
        started(&jobs, &great_grandchild, deepest.id(), "deepest").await;
        committed(&jobs, &great_grandchild).await;
        let _leaf = jobs
            .test_create(JobSpec::test(great_grandchild.clone(), "read"))
            .await;
        let terminal = jobs.test_lease(agent_spec(&child, None)).await;
        jobs.test_finish(terminal.id(), Value::Null).await;

        let state = snapshot(&jobs, &root, false).await;
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
        let visible = snapshot(&jobs, &root, true).await;
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
        // Polling and a different viewer cannot double count the same journal suffix.
        assert_eq!(snapshot(&jobs, &root, false).await, state);
        assert_eq!(
            progress_of(&snapshot(&jobs, &child, false).await[1]),
            progress_of(grand)
        );
        let future = jobs
            .active_states(&root, &CapabilitySet::default(), 0)
            .await;
        assert_eq!(future[0].age_seconds, 0);
    }

    #[tokio::test]
    async fn progress_survives_retained_resume_and_replay() {
        let session = crate::session::fixture::MemorySession::new().await;
        let (store, root) = (session.store.clone(), session.agent.clone());
        let child = root.child(1);
        let jobs = JobManager::new(store.clone());
        let spec = JobSpec {
            accepts_input: true,
            ..agent_spec(&root, None)
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
        let handler: ResumeHandler = Arc::new({
            let (jobs, child, release) = (jobs.clone(), child.clone(), release.clone());
            move |_, _| {
                let (jobs, child, release) = (jobs.clone(), child.clone(), release.clone());
                Box::pin(async move {
                    turn(jobs, child).await;
                    release.acquire().await.unwrap().forget();
                    Ok(ToolOutput::default())
                })
            }
        });
        jobs.set_resume_handler(delegated.id(), handler)
            .await
            .unwrap();
        let turns = |jobs: &[StateJob]| progress_of(&jobs[0]).map(|progress| progress.turns);
        assert_eq!(turns(&snapshot(&jobs, &root, false).await), Some(1));
        jobs.test_finish(delegated.id(), Value::Null).await;
        assert!(snapshot(&jobs, &root, false).await.is_empty());
        jobs.send(delegated.id(), Value::Null).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while progress_of(&snapshot(&jobs, &root, false).await[0])
                .map(|progress| progress.tool_calls)
                != Some(2)
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(turns(&snapshot(&jobs, &root, false).await), Some(2));
        release.add_permits(1);
        jobs.wait(delegated.id(), Some(Duration::from_secs(5)), true)
            .await
            .unwrap();
        jobs.clear_resume_handler(delegated.id()).await;

        let records = store.records().await;
        let restored = JobManager::restore(store, &records).await.unwrap();
        let retained = async || restored.inner.progress.lock().await.for_job(delegated.id());
        // Completed agents remain omitted after replay, but retain progress for resumption.
        assert!(snapshot(&restored, &root, false).await.is_empty());
        assert_eq!(retained().await, progress(2, 2).unwrap());
        // Exercise the same projection after replay without resetting the retained identity.
        committed(&restored, &child).await;
        snapshot(&restored, &root, false).await;
        assert_eq!(retained().await.turns, 3);
    }
}
