//! Incremental projection of durable, exclusive child-agent progress.
//!
//! Count committed assistant messages, not requests/usage (which include retries and
//! compaction), and owned job creations, not assistant tool-call blocks (which miss
//! calls launched inside scripts). Retained resumes keep the same agent identity.

use std::collections::HashMap;

use serde::Serialize;

use crate::{
    identity::{AgentId, JobId},
    provider::protocol::Message,
    session::{EventRecord, SessionEvent},
};

#[derive(Clone, Copy, Default, Serialize)]
pub(crate) struct AgentProgress {
    pub(crate) turns: u64,
    pub(crate) tool_calls: u64,
}

#[derive(Default)]
pub(super) struct Progress {
    pub sequence: u64,
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
) -> super::ActiveJob {
    use crate::tool::policy::Capability;

    let entry = &jobs[&id];
    let is_agent = entry.tool == "agent" || progress.agents.contains_key(&id);
    let mut children = Vec::new();
    path.insert(id);
    if let Some(agent) = progress.agents.get(&id) {
        let mut child_ids = jobs
            .iter()
            .filter(|(job, child)| {
                &child.agent == agent
                    && !child.state.is_terminal()
                    && (child.tool == "agent" || progress.agents.contains_key(job))
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
    super::ActiveJob {
        job: id,
        tool: entry.tool.clone(),
        name: entry.name.clone(),
        state: entry.state.presented(),
        location: super::ActiveJobLocation {
            target: capabilities
                .contains(Capability::Targets)
                .then(|| entry.location.target.clone()),
            workspace: entry.location.workspace.clone(),
        },
        age_seconds: u64::try_from(now_millis.saturating_sub(entry.created_at_millis)).unwrap_or(0)
            / 1_000,
        progress: is_agent.then(|| progress.for_job(id)),
        children,
    }
}

#[cfg(test)]
mod tests {
    use crate::job::*;
    use crate::provider::protocol::{AssistantContent, Message};

    async fn started(jobs: &JobManager, agent: &AgentId, owner: JobId, target: &str) {
        let location = ExecutionLocation::named(target, format!("/{target}/work").into());
        jobs.test_append(
            agent.clone(),
            SessionEvent::AgentStarted {
                parent: None,
                owner_job: Some(owner),
                model_profile: "test".into(),
                max_context: None,
                location: location.clone(),
            },
        )
        .await;
        jobs.set_agent_location(owner, location).await.unwrap();
    }

    async fn committed(jobs: &JobManager, agent: &AgentId) {
        jobs.test_append(
            agent.clone(),
            SessionEvent::MessageCommitted {
                message: Message::Assistant(vec![AssistantContent::text("answer", 0, "done")]),
            },
        )
        .await;
    }

    async fn snapshot(jobs: &JobManager, owner: &AgentId, targets: bool) -> Value {
        let mut capabilities = CapabilitySet::default();
        if targets {
            capabilities.insert(Capability::Targets);
        }
        serde_json::to_value(jobs.active_states(owner, &capabilities, i64::MAX).await).unwrap()
    }

    #[tokio::test]
    async fn active_progress_is_recursive_exclusive_and_location_filtered() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::create(directory.path()).await.unwrap();
        let root = AgentId::root(store.id());
        let child = root.child(1);
        let grandchild = child.child(1);
        let great_grandchild = grandchild.child(1);
        let jobs = JobManager::new(store);
        let delegated = jobs.test_lease(JobSpec::test(root.clone(), "agent")).await;
        let ordinary = jobs.test_lease(JobSpec::test(root.clone(), "exec")).await;
        // Even queued agents without AgentStarted have zero counters; tools do not.
        let initial = snapshot(&jobs, &root, false).await;
        assert_eq!(initial[0]["turns"], 0);
        assert_eq!(initial[0]["tool_calls"], 0);
        assert!(initial[0].get("children").is_none());
        assert!(initial[1].get("turns").is_none());
        assert!(initial[1].get("tool_calls").is_none());
        started(&jobs, &child, delegated.id, "child").await;
        committed(&jobs, &child).await;
        committed(&jobs, &child).await;
        committed(&jobs, &root).await; // Not child progress.
        let script = jobs
            .test_lease(JobSpec::test(child.clone(), "script"))
            .await;
        let nested = jobs
            .test_lease(JobSpec {
                parent: Some(script.id),
                ..JobSpec::test(child.clone(), "exec")
            })
            .await;
        jobs.test_finish(nested.id, serde_json::Value::Null).await;
        let delegated_again = jobs
            .test_lease(JobSpec {
                parent: Some(script.id),
                ..JobSpec::test(child.clone(), "agent")
            })
            .await;
        started(&jobs, &grandchild, delegated_again.id, "grandchild").await;
        jobs.transition(delegated_again.id, JobState::AwaitingApproval)
            .await
            .unwrap();
        committed(&jobs, &grandchild).await;
        let deepest = jobs
            .test_lease(JobSpec::test(grandchild.clone(), "agent"))
            .await;
        started(&jobs, &great_grandchild, deepest.id, "deepest").await;
        committed(&jobs, &great_grandchild).await;
        jobs.test_lease(JobSpec::test(great_grandchild.clone(), "read"))
            .await;
        let terminal = jobs.test_lease(JobSpec::test(child.clone(), "agent")).await;
        jobs.test_finish(terminal.id, serde_json::Value::Null).await;

        let state = snapshot(&jobs, &root, false).await;
        assert_eq!(state.as_array().unwrap().len(), 2);
        assert_eq!(state[0]["job"], delegated.id.get());
        assert_eq!(state[1]["job"], ordinary.id.get());
        assert_eq!(state[0]["turns"], 2);
        assert_eq!(state[0]["tool_calls"], 4); // Script, nested exec, two agent jobs.
        assert_eq!(state[0]["children"].as_array().unwrap().len(), 1);
        let grand = &state[0]["children"][0];
        assert_eq!(grand["job"], delegated_again.id.get());
        assert_eq!(grand["state"], "queued"); // Approval stays host-only.
        assert_eq!(grand["turns"], 1);
        assert_eq!(grand["tool_calls"], 1);
        assert_eq!(grand["location"]["workspace"], "/grandchild/work");
        let deepest_state = &grand["children"][0];
        assert_eq!(deepest_state["job"], deepest.id.get());
        assert_eq!(deepest_state["turns"], 1);
        assert_eq!(deepest_state["tool_calls"], 1);
        assert!(deepest_state.get("children").is_none());
        for entry in [&state[0], grand, deepest_state] {
            assert!(entry["location"].get("target").is_none());
        }
        let visible = snapshot(&jobs, &root, true).await;
        assert_eq!(visible[0]["location"]["target"], "child");
        assert_eq!(
            visible[0]["children"][0]["location"]["target"],
            "grandchild"
        );
        assert_eq!(
            visible[0]["children"][0]["children"][0]["location"]["target"],
            "deepest"
        );
        // Polling and a different viewer cannot double count the same journal suffix.
        assert_eq!(snapshot(&jobs, &root, false).await, state);
        let own = snapshot(&jobs, &child, false).await;
        assert_eq!(own[1]["turns"], grand["turns"]);
        assert_eq!(own[1]["tool_calls"], grand["tool_calls"]);
        let future = serde_json::to_value(
            jobs.active_states(&root, &CapabilitySet::default(), 0)
                .await,
        )
        .unwrap();
        assert_eq!(future[0]["age_seconds"], 0);
    }

    #[tokio::test]
    async fn progress_survives_retained_resume_and_replay() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::create(directory.path()).await.unwrap();
        let root = AgentId::root(store.id());
        let child = root.child(1);
        let jobs = JobManager::new(store.clone());
        let delegated = jobs
            .test_lease(JobSpec {
                accepts_input: true,
                ..JobSpec::test(root.clone(), "agent")
            })
            .await;
        started(&jobs, &child, delegated.id, "child").await;
        committed(&jobs, &child).await;
        let tool = jobs.test_lease(JobSpec::test(child.clone(), "read")).await;
        jobs.test_finish(tool.id, serde_json::Value::Null).await;
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        jobs.set_resume_handler(
            delegated.id,
            Arc::new({
                let jobs = jobs.clone();
                let child = child.clone();
                let release = release.clone();
                move |_, _| {
                    let jobs = jobs.clone();
                    let child = child.clone();
                    let release = release.clone();
                    Box::pin(async move {
                        committed(&jobs, &child).await;
                        let tool = jobs.test_lease(JobSpec::test(child, "read")).await;
                        jobs.test_finish(tool.id, serde_json::Value::Null).await;
                        release.acquire().await.unwrap().forget();
                        Ok(ToolOutput::default())
                    })
                }
            }),
        )
        .await
        .unwrap();
        assert_eq!(snapshot(&jobs, &root, false).await[0]["turns"], 1);
        jobs.test_finish(delegated.id, serde_json::Value::Null)
            .await;
        assert!(
            snapshot(&jobs, &root, false)
                .await
                .as_array()
                .unwrap()
                .is_empty()
        );
        jobs.send(delegated.id, Value::Null).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let state = snapshot(&jobs, &root, false).await;
                if state[0]["tool_calls"] == 2 {
                    assert_eq!(state[0]["turns"], 2);
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        release.add_permits(1);
        jobs.wait(delegated.id, Some(Duration::from_secs(5)), true)
            .await
            .unwrap();
        jobs.clear_resume_handler(delegated.id).await;

        let records = store.records().await;
        let restored = JobManager::restore(store, &records).await.unwrap();
        // Completed agents remain omitted after replay, but retain progress for resumption.
        assert!(
            snapshot(&restored, &root, false)
                .await
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            serde_json::to_value(restored.inner.progress.lock().await.for_job(delegated.id))
                .unwrap(),
            serde_json::json!({"turns": 2, "tool_calls": 2})
        );
        // Exercise the same projection after replay without resetting the retained identity.
        committed(&restored, &child).await;
        snapshot(&restored, &root, false).await;
        assert_eq!(
            serde_json::to_value(restored.inner.progress.lock().await.for_job(delegated.id))
                .unwrap()["turns"],
            3
        );
    }
}
