//! Incremental journal indexes, agent lifecycle state, and usage totals.

use super::super::format::agent_label;
use super::state_name;
use serde_json::Value;
use skyhook::agent::{AgentActivity, LiveResponse, ObservationSnapshot};
use skyhook::identity::{AgentId, JobId};
use skyhook::job::JobState;
use skyhook::provider::protocol::{Message, Usage};
use skyhook::session::SessionEvent;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Instant;

#[derive(Clone)]
pub struct AgentInfo {
    pub id: AgentId,
    pub name: String,
    pub model: String,
    pub target: String,
    pub owner: Option<JobId>,
    pub terminal: bool,
}
#[derive(Clone)]
pub struct JobInfo {
    pub id: JobId,
    pub agent: AgentId,
    pub name: Option<String>,
    pub tool: String,
    pub args: Value,
    pub parent: Option<JobId>,
    pub state: JobState,
    pub target: String,
    pub location: String,
    pub remote: bool,
    pub error: Option<String>,
}

#[derive(Default)]
pub(super) struct RequestInfo {
    pub(super) started_millis: Option<i64>,
    pub(super) finished_millis: Option<i64>,
    pub(super) usage: Option<Usage>,
    pub(super) model: Option<String>,
    pub(super) failed: bool,
    pub(super) response: Option<u64>,
}

#[derive(Default)]
pub struct Projection {
    pub agents: Vec<AgentInfo>,
    pub jobs: BTreeMap<JobId, JobInfo>,
    pub usage: Usage,
    pub agent_usage: HashMap<AgentId, Usage>,
    pub completed: HashMap<AgentId, Instant>,
    pub(super) through: u64,
    pub(super) records_by_agent: HashMap<AgentId, Vec<u64>>,
    pub(super) requests: HashMap<u64, RequestInfo>,
    pub(super) active_request: HashMap<AgentId, u64>,
    pub(super) response_requests: HashMap<u64, u64>,
    pub(super) tool_origins: HashSet<(AgentId, u64, String)>,
}
impl Projection {
    pub(super) fn response_committed(&self, request: u64) -> bool {
        self.requests
            .get(&request)
            .is_some_and(|r| r.response.is_some())
    }

    pub(super) fn live_response(&self, request: u64, response: &LiveResponse) -> bool {
        // Settlement can announce a journal sequence before its record arrives.
        // Keep that snapshot visible until the projection has consumed the commit.
        (!response.settled || response.message.is_some()) && !self.response_committed(request)
    }

    pub fn rebuild(&mut self, snapshot: &ObservationSnapshot) {
        for (_, record) in snapshot.records.range((self.through + 1)..) {
            self.through = record.sequence;
            self.records_by_agent
                .entry(record.agent.clone())
                .or_default()
                .push(record.sequence);
            match &record.event {
                SessionEvent::AgentStarted {
                    model_profile,
                    location,
                    owner_job,
                    ..
                } => {
                    self.agents.retain(|agent| agent.id != record.agent);
                    self.agents.push(AgentInfo {
                        id: record.agent.clone(),
                        name: if record.agent.path().is_empty() {
                            "skyhook".into()
                        } else {
                            format!("agent {}", agent_label(&record.agent))
                        },
                        model: model_profile.clone(),
                        target: location.target.clone(),
                        owner: *owner_job,
                        terminal: false,
                    });
                    self.completed.remove(&record.agent);
                }
                SessionEvent::JobCreated {
                    job,
                    tool,
                    name,
                    arguments,
                    parent,
                    location,
                    ..
                } => {
                    self.jobs.insert(
                        *job,
                        JobInfo {
                            id: *job,
                            agent: record.agent.clone(),
                            name: name.clone(),
                            tool: tool.clone(),
                            args: arguments.clone(),
                            parent: *parent,
                            state: JobState::Queued,
                            target: location.target.clone(),
                            location: format!(
                                "{} · {}",
                                location.target,
                                location.workspace.display()
                            ),
                            remote: location.target != "root",
                            error: None,
                        },
                    );
                }
                SessionEvent::JobStateChanged { job, state } => {
                    if let Some(info) = self.jobs.get_mut(job) {
                        info.state = *state;
                    }
                    // Resuming a retained child reuses its owner job and does not
                    // emit AgentStarted again. Reopen its tree row on that job's
                    // Running event, not on ordinary model/tool activity updates.
                    if *state == JobState::Running {
                        for agent in &mut self.agents {
                            if agent.owner == Some(*job) && agent.terminal {
                                agent.terminal = false;
                                self.completed.remove(&agent.id);
                            }
                        }
                    }
                }
                SessionEvent::JobFinished {
                    job, state, error, ..
                } => {
                    if let Some(info) = self.jobs.get_mut(job) {
                        info.state = *state;
                        info.error = error.clone();
                    }
                }
                SessionEvent::AgentCompleted | SessionEvent::AgentInterrupted => {
                    if let Some(info) = self.agents.iter_mut().find(|a| a.id == record.agent) {
                        info.terminal = true;
                    }
                    self.completed
                        .entry(record.agent.clone())
                        .or_insert_with(Instant::now);
                }
                SessionEvent::ModelChanged { model_profile, .. } => {
                    if let Some(agent) = self.agents.iter_mut().find(|a| a.id == record.agent) {
                        agent.model.clone_from(model_profile);
                    }
                }
                SessionEvent::Usage { usage, .. } => {
                    add_usage(&mut self.usage, *usage);
                    add_usage(
                        self.agent_usage.entry(record.agent.clone()).or_default(),
                        *usage,
                    );
                }
                _ => {}
            }
            match &record.event {
                SessionEvent::ModelRequested { context, .. } => {
                    let model =
                        snapshot
                            .records
                            .get(context)
                            .and_then(|record| match &record.event {
                                SessionEvent::ModelContext { template, .. } => {
                                    Some(template.model.clone())
                                }
                                _ => None,
                            });
                    let info = self.requests.entry(record.sequence).or_default();
                    info.model = model;
                    info.started_millis = Some(record.timestamp_millis);
                    self.active_request
                        .insert(record.agent.clone(), record.sequence);
                }
                SessionEvent::AgentInterrupted => {
                    if let Some(request) = self.active_request.get(&record.agent)
                        && let Some(info) = self.requests.get_mut(request)
                    {
                        info.finished_millis.get_or_insert(record.timestamp_millis);
                    }
                }
                SessionEvent::ModelFailed { request, .. } => {
                    let info = self.requests.entry(*request).or_default();
                    info.failed = true;
                    info.finished_millis.get_or_insert(record.timestamp_millis);
                }
                SessionEvent::Usage {
                    request: Some(request),
                    usage,
                } => {
                    let info = self.requests.entry(*request).or_default();
                    add_usage(info.usage.get_or_insert_with(Usage::default), *usage);
                    info.finished_millis.get_or_insert(record.timestamp_millis);
                }
                SessionEvent::MessageCommitted {
                    message: Message::Assistant(_),
                } => {
                    if let Some(&request_id) = self.active_request.get(&record.agent) {
                        let request = self.requests.entry(request_id).or_default();
                        if request.response.is_none() {
                            self.response_requests.insert(record.sequence, request_id);
                            request.response = Some(record.sequence);
                            request
                                .finished_millis
                                .get_or_insert(record.timestamp_millis);
                        }
                    }
                }
                SessionEvent::JobCreated {
                    origin: Some(origin),
                    ..
                } => {
                    self.tool_origins.insert((
                        record.agent.clone(),
                        origin.message,
                        origin.call_id.clone(),
                    ));
                }
                _ => {}
            }
        }
        for agent in &mut self.agents {
            if let Some(job) = agent.owner.and_then(|id| self.jobs.get(&id)) {
                if let Some(name) = &job.name {
                    agent.name = name.clone();
                }
                if job.state.is_terminal() && !agent.terminal {
                    agent.terminal = true;
                    self.completed
                        .entry(agent.id.clone())
                        .or_insert_with(Instant::now);
                }
            }
        }
        self.agents.sort_by(|a, b| a.id.path().cmp(b.id.path()));
    }
    pub fn has_active_children(&self) -> bool {
        self.agents
            .iter()
            .any(|agent| !agent.id.path().is_empty() && !agent.terminal)
    }
    pub(super) fn child_target<'a>(&'a self, caller: &AgentId, arguments: &'a Value) -> &'a str {
        arguments
            .get("target")
            .and_then(Value::as_str)
            .or_else(|| {
                self.agents
                    .iter()
                    .find(|agent| &agent.id == caller)
                    .map(|agent| agent.target.as_str())
            })
            .unwrap_or("root")
    }
    pub(super) fn job_target<'a>(&'a self, job: &'a JobInfo) -> &'a str {
        if job.tool == "agent" {
            self.agents
                .iter()
                .find(|agent| agent.owner == Some(job.id))
                .map_or_else(
                    || self.child_target(&job.agent, &job.args),
                    |agent| agent.target.as_str(),
                )
        } else {
            &job.target
        }
    }
    pub fn visible(&self, selected: &AgentId) -> Vec<AgentInfo> {
        self.agents
            .iter()
            .filter(|a| {
                a.id.path().is_empty()
                    || !a.terminal
                    || selected.path().starts_with(a.id.path())
                    || self
                        .completed
                        .get(&a.id)
                        .is_some_and(|t| t.elapsed().as_secs() < 2)
            })
            .cloned()
            .collect()
    }
    pub fn status(&self, agent: &AgentInfo, snapshot: &ObservationSnapshot) -> (bool, String) {
        if agent.terminal {
            return (
                false,
                agent
                    .owner
                    .and_then(|j| self.jobs.get(&j))
                    .map_or("Completed".into(), |j| state_name(j.state).into()),
            );
        }
        if let Some(owner) = agent.owner.and_then(|id| self.jobs.get(&id))
            && owner.state == JobState::WaitingInput
        {
            return (false, "Waiting for parent input".into());
        }
        match snapshot.activity.get(&agent.id) {
            Some(AgentActivity::Working) => (true, "Working".into()),
            Some(AgentActivity::Reconnecting {
                attempt,
                max_attempts,
            }) => (
                true,
                format!("Reconnecting · attempt {attempt} of {max_attempts}"),
            ),
            Some(AgentActivity::Compacting) => (true, "Compacting".into()),
            Some(AgentActivity::Interrupted) => (false, "Interrupted".into()),
            Some(AgentActivity::Failed(_)) => (false, "Failed".into()),
            Some(AgentActivity::WaitingChildren) => (false, "Waiting for child".into()),
            activity => {
                let jobs: Vec<_> = self
                    .jobs
                    .values()
                    .filter(|j| j.agent == agent.id && !j.state.is_terminal())
                    .collect();
                if !jobs.is_empty() && jobs.iter().all(|j| j.state == JobState::AwaitingApproval) {
                    (false, "Waiting for permission".into())
                } else if jobs
                    .iter()
                    .any(|j| j.tool == "ask" || j.state == JobState::WaitingInput)
                {
                    (false, "Waiting for input".into())
                } else if !jobs.is_empty() && jobs.iter().all(|j| j.tool == "agent") {
                    (false, "Waiting for child".into())
                } else if matches!(activity, Some(AgentActivity::Tools)) || !jobs.is_empty() {
                    (true, "Running tools".into())
                } else {
                    (false, "Ready".into())
                }
            }
        }
    }
}
fn add_usage(sum: &mut Usage, value: Usage) {
    sum.input_tokens = sum.input_tokens.saturating_add(value.input_tokens);
    sum.cached_input_tokens = sum
        .cached_input_tokens
        .saturating_add(value.cached_input_tokens);
    sum.output_tokens = sum.output_tokens.saturating_add(value.output_tokens);
}
pub fn agent_footer(
    snapshot: &ObservationSnapshot,
    projection: &Projection,
    agent: &AgentId,
) -> String {
    agent_footer_stats(snapshot, projection, agent).join(" · ")
}
pub fn agent_footer_stats(
    snapshot: &ObservationSnapshot,
    projection: &Projection,
    agent: &AgentId,
) -> [String; 3] {
    super::super::format::footer_stats(
        projection
            .agent_usage
            .get(agent)
            .copied()
            .unwrap_or_default(),
        snapshot
            .context
            .get(agent)
            .map(|context| (context.tokens, context.capacity)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use skyhook::identity::SessionId;

    #[test]
    fn recovery_status_is_active_without_a_job() {
        let agent = AgentInfo {
            id: AgentId::root(SessionId::from_bytes([1; 16])),
            name: "skyhook".into(),
            model: "test".into(),
            target: "root".into(),
            owner: None,
            terminal: false,
        };
        let mut snapshot = ObservationSnapshot::default();
        snapshot.activity.insert(
            agent.id.clone(),
            AgentActivity::Reconnecting {
                attempt: 2,
                max_attempts: 3,
            },
        );
        let projection = Projection::default();
        assert_eq!(
            projection.status(&agent, &snapshot),
            (true, "Reconnecting · attempt 2 of 3".into())
        );
    }

    #[test]
    fn settlement_keeps_the_live_response_until_its_journal_commit_arrives() {
        let mut projection = Projection::default();
        let mut live = LiveResponse::default();
        assert!(projection.live_response(4, &live));
        live.settled = true;
        assert!(!projection.live_response(4, &live));
        live.message = Some(7);
        assert!(projection.live_response(4, &live));
        projection.requests.entry(4).or_default().response = Some(7);
        assert!(!projection.live_response(4, &live));
        live.settled = false;
        assert!(!projection.live_response(4, &live));
    }
}
