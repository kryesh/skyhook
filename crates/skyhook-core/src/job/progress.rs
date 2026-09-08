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
pub(super) struct AgentProgress {
    turns: u64,
    tool_calls: u64,
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
