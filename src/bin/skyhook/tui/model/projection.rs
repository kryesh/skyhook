//! Incremental journal indexes, agent lifecycle state, and usage totals.

use super::super::format::agent_label;
use super::retry::RetryState;
use super::state_name;
use serde_json::Value;
use skyhook::agent::{AgentActivity, LiveResponse, ObservationSnapshot, TodoItem};
use skyhook::execution::ExecutionLocation;
use skyhook::identity::{AgentId, JobId};
use skyhook::job::{JobRole, JobState};
use skyhook::provider::protocol::{Message, Usage};
use skyhook::session::{ModelFailureKind, ModelPurpose, SessionEvent};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant};

/// Keep newly completed agents discoverable briefly in the live tree.
pub const COMPLETED_AGENT_GRACE: Duration = Duration::from_secs(2);

/// Semantic agent presentation state. Labels are output, never state discriminants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitReason {
    ParentInput,
    Permission,
    Input,
    Child,
    Event,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentDisplayState {
    Ready,
    Working,
    Reconnecting { attempt: u64 },
    Compacting,
    RunningTools,
    Waiting(WaitReason),
    Job(JobState),
}

impl AgentDisplayState {
    pub fn running(self) -> bool {
        matches!(
            self,
            Self::Working | Self::Reconnecting { .. } | Self::Compacting | Self::RunningTools
        )
    }

    pub fn label(self) -> String {
        match self {
            Self::Ready => "Ready".into(),
            Self::Working => "Working".into(),
            Self::Reconnecting { attempt } => format!("Retrying · attempt {attempt}"),
            Self::Compacting => "Compacting".into(),
            Self::RunningTools => "Running tools".into(),
            Self::Waiting(reason) => match reason {
                WaitReason::ParentInput => "Waiting for parent input",
                WaitReason::Permission => "Waiting for permission",
                WaitReason::Input => "Waiting for input",
                WaitReason::Child => "Waiting for child",
                WaitReason::Event => "Waiting",
            }
            .into(),
            Self::Job(state) => state_name(state).into(),
        }
    }
}

/// Terminality and its optional live-tree grace cannot disagree.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AgentLifecycle {
    #[default]
    Active,
    Terminal {
        grace_started: Option<Instant>,
    },
}

impl AgentLifecycle {
    fn complete(&mut self, now: Instant) {
        if matches!(self, Self::Active) {
            *self = Self::Terminal {
                grace_started: Some(now),
            };
        }
    }

    fn retire_grace(&mut self, now: Instant) -> bool {
        if let Self::Terminal { grace_started } = self
            && grace_started.is_some_and(|started| {
                now.saturating_duration_since(started) >= COMPLETED_AGENT_GRACE
            })
        {
            *grace_started = None;
            return true;
        }
        false
    }

    fn within_grace(self) -> bool {
        matches!(self, Self::Terminal { grace_started: Some(started) } if started.elapsed() < COMPLETED_AGENT_GRACE)
    }
}

#[derive(Clone)]
pub struct AgentInfo {
    pub id: AgentId,
    pub name: String,
    pub model: String,
    /// The root agent's applied mode.
    pub mode: Option<String>,
    pub capabilities: Vec<skyhook::tool::policy::Capability>,
    pub target: String,
    pub owner: Option<JobId>,
    pub lifecycle: AgentLifecycle,
}
impl AgentInfo {
    pub fn terminal(&self) -> bool {
        matches!(self.lifecycle, AgentLifecycle::Terminal { .. })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum JobActivity {
    Work,
    Wait,
}

#[derive(Clone)]
pub struct JobInfo {
    pub id: JobId,
    pub agent: AgentId,
    pub name: Option<String>,
    pub tool: String,
    pub role: JobRole,
    pub(super) activity: JobActivity,
    pub args: Value,
    pub parent: Option<JobId>,
    pub state: JobState,
    pub location: ExecutionLocation,
    pub error: Option<String>,
}

impl JobInfo {
    pub fn target(&self) -> &str {
        &self.location.target
    }

    pub fn location_label(&self) -> String {
        format!(
            "{} · {}",
            self.location.target,
            self.location.workspace.display()
        )
    }

    pub fn remote(&self) -> bool {
        !self.location.is_root()
    }
}

/// Only ModelRequested establishes a start; an unresolved model is still a start.
pub(super) struct RequestStart {
    pub(super) timestamp_millis: i64,
    pub(super) purpose: ModelPurpose,
    pub(super) model: Option<String>,
}

#[derive(Default)]
pub(super) struct RequestInfo {
    pub(super) start: Option<RequestStart>,
    pub(super) finished_millis: Option<i64>,
    pub(super) usage: Option<Usage>,
    pub(super) failed: bool,
    pub(super) retry: Option<RetryState>,
    pub(super) response: Option<u64>,
}

#[derive(Default)]
pub struct Projection {
    pub agents: Vec<AgentInfo>,
    pub jobs: BTreeMap<JobId, JobInfo>,
    pub usage: Usage,
    pub agent_usage: HashMap<AgentId, Usage>,
    pub todos: HashMap<AgentId, Vec<TodoItem>>,
    pub(super) through: u64,
    pub(super) records_by_agent: HashMap<AgentId, Vec<u64>>,
    pub(super) requests: HashMap<u64, RequestInfo>,
    pub(super) active_request: HashMap<AgentId, u64>,
    pub(super) response_requests: HashMap<u64, u64>,
    pub(super) tool_origins: HashSet<(AgentId, u64, String)>,
}
impl Projection {
    pub(super) fn agent_name(&self, agent: &AgentId) -> &str {
        let info = self.agents.iter().find(|info| &info.id == agent);
        info.map_or("Agent", |info| info.name.as_str())
    }

    /// The request's latest attempt failed, so its retry card owns the content.
    pub(super) fn retry_failed(&self, request: u64) -> bool {
        let retry = self
            .requests
            .get(&request)
            .and_then(|info| info.retry.as_ref());
        retry.is_some_and(RetryState::has_error)
    }

    pub fn complete_agent(&mut self, agent: &AgentId) {
        // A fresh explicit completion after retirement starts another grace;
        // owner-state reconciliation in rebuild must not do so on every rebuild.
        if let Some(info) = self.agents.iter_mut().find(|info| &info.id == agent)
            && !matches!(
                info.lifecycle,
                AgentLifecycle::Terminal {
                    grace_started: Some(_)
                }
            )
        {
            info.lifecycle = AgentLifecycle::Terminal {
                grace_started: Some(Instant::now()),
            };
        }
    }

    /// Returns true once when grace is retired, so idle UI ticks stop redrawing.
    pub fn retire_completed_grace(&mut self) -> bool {
        let now = Instant::now();
        let mut changed = false;
        for agent in &mut self.agents {
            changed |= agent.lifecycle.retire_grace(now);
        }
        changed
    }

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
                    profile,
                    mode,
                    capabilities,
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
                        model: profile
                            .as_ref()
                            .map(|profile| profile.name.clone())
                            .unwrap_or_default(),
                        mode: mode.as_ref().map(|mode| mode.name.clone()),
                        capabilities: capabilities.clone(),
                        target: location.target.clone(),
                        owner: *owner_job,
                        lifecycle: AgentLifecycle::Active,
                    });
                }
                SessionEvent::JobCreated {
                    job,
                    tool,
                    role,
                    name,
                    arguments,
                    parent,
                    location,
                    origin,
                    ..
                } => {
                    if let Some(origin) = origin {
                        self.tool_origins.insert((
                            record.agent.clone(),
                            origin.message,
                            origin.call_id.clone(),
                        ));
                    }
                    self.jobs.insert(
                        *job,
                        JobInfo {
                            id: *job,
                            agent: record.agent.clone(),
                            name: name.clone(),
                            tool: tool.clone(),
                            role: *role,
                            activity: match (role, tool.as_str()) {
                                (JobRole::Tool, "wait") => JobActivity::Wait,
                                _ => JobActivity::Work,
                            },
                            args: arguments.clone(),
                            parent: *parent,
                            state: JobState::Queued,
                            location: location.clone(),
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
                        for agent in self
                            .agents
                            .iter_mut()
                            .filter(|agent| agent.owner == Some(*job) && agent.terminal())
                        {
                            agent.lifecycle = AgentLifecycle::Active;
                        }
                    }
                }
                SessionEvent::JobFinished {
                    job,
                    state,
                    diagnostic,
                    ..
                } => {
                    let capabilities = self
                        .agents
                        .iter()
                        .find(|agent| agent.id == record.agent)
                        .map(|agent| agent.capabilities.iter().copied().collect())
                        .unwrap_or_else(skyhook::tool::policy::CapabilitySet::empty);
                    if let Some(info) = self.jobs.get_mut(job) {
                        info.state = *state;
                        info.error = diagnostic
                            .as_ref()
                            .map(|diagnostic| diagnostic.render(&capabilities));
                    }
                }
                SessionEvent::AgentCompleted => self.complete_agent(&record.agent),
                SessionEvent::AgentInterrupted => {
                    self.complete_agent(&record.agent);
                    if let Some(request) = self.active_request.get(&record.agent)
                        && let Some(info) = self.requests.get_mut(request)
                    {
                        info.finished_millis.get_or_insert(record.timestamp_millis);
                    }
                }
                SessionEvent::ModelChanged { profile } => {
                    if let Some(agent) = self.agents.iter_mut().find(|a| a.id == record.agent) {
                        agent.model.clone_from(&profile.name);
                    }
                }
                SessionEvent::ModeChanged { mode, capabilities } => {
                    if let Some(agent) = self.agents.iter_mut().find(|a| a.id == record.agent) {
                        agent.mode = Some(mode.name.clone());
                        agent.capabilities.clone_from(capabilities);
                    }
                }
                SessionEvent::Usage { request, usage } => {
                    add_usage(&mut self.usage, *usage);
                    add_usage(
                        self.agent_usage.entry(record.agent.clone()).or_default(),
                        *usage,
                    );
                    if let Some(request) = request {
                        let info = self.requests.entry(*request).or_default();
                        add_usage(info.usage.get_or_insert_with(Usage::default), *usage);
                        info.finished_millis.get_or_insert(record.timestamp_millis);
                    }
                }
                SessionEvent::ModelRequested {
                    context, purpose, ..
                } => {
                    let model =
                        snapshot
                            .records
                            .get(context)
                            .and_then(|record| match &record.event {
                                SessionEvent::ModelContext { context } => {
                                    Some(context.profile.profile.model.clone())
                                }
                                _ => None,
                            });
                    let info = self.requests.entry(record.sequence).or_default();
                    info.start = Some(RequestStart {
                        timestamp_millis: record.timestamp_millis,
                        purpose: *purpose,
                        model,
                    });
                    self.active_request
                        .insert(record.agent.clone(), record.sequence);
                }
                SessionEvent::ModelFailed {
                    request,
                    attempt,
                    error,
                    kind,
                } => {
                    let info = self.requests.entry(*request).or_default();
                    info.failed = true;
                    let (attempt, error) = (*attempt, error.clone());
                    info.retry = Some(match kind {
                        ModelFailureKind::Refusal => RetryState::Refused { attempt, error },
                        ModelFailureKind::Error => RetryState::Failed { attempt, error },
                    });
                    info.finished_millis.get_or_insert(record.timestamp_millis);
                }
                SessionEvent::ModelAttemptStarted { request, attempt } => {
                    let info = self.requests.entry(*request).or_default();
                    info.failed = false;
                    info.finished_millis = None;
                    info.retry = Some(RetryState::Started { attempt: *attempt });
                }
                SessionEvent::ModelRecoveryScheduled {
                    request,
                    attempt,
                    delay_millis,
                    error,
                } => {
                    self.requests.entry(*request).or_default().retry =
                        Some(RetryState::Scheduled {
                            attempt: *attempt,
                            delay_millis: *delay_millis,
                            error: error.clone(),
                        });
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
                SessionEvent::TodosReplaced { items } => {
                    self.todos.insert(record.agent.clone(), items.clone());
                }
                SessionEvent::Compaction { checkpoint } => {
                    self.todos
                        .insert(record.agent.clone(), checkpoint.todos.clone());
                }
                _ => {}
            }
        }
        for agent in &mut self.agents {
            if let Some(job) = agent.owner.and_then(|id| self.jobs.get(&id)) {
                if let Some(name) = &job.name {
                    agent.name = name.clone();
                }
                if job.state.is_terminal() {
                    agent.lifecycle.complete(Instant::now());
                }
            }
        }
        self.agents.sort_by(|a, b| a.id.path().cmp(b.id.path()));
    }
    pub fn has_active_children(&self) -> bool {
        self.agents
            .iter()
            .any(|agent| !agent.id.path().is_empty() && !agent.terminal())
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
        if job.role == JobRole::Agent {
            self.agents
                .iter()
                .find(|agent| agent.owner == Some(job.id))
                .map_or_else(
                    || self.child_target(&job.agent, &job.args),
                    |agent| agent.target.as_str(),
                )
        } else {
            job.target()
        }
    }
    pub fn visible(&self, selected: &AgentId) -> Vec<AgentInfo> {
        self.agents
            .iter()
            .filter(|a| {
                a.id.path().is_empty()
                    || !a.terminal()
                    || selected.path().starts_with(a.id.path())
                    || a.lifecycle.within_grace()
            })
            .cloned()
            .collect()
    }
    pub fn status(&self, agent: &AgentInfo, snapshot: &ObservationSnapshot) -> AgentDisplayState {
        use AgentDisplayState as State;
        if agent.terminal() {
            return State::Job(
                agent
                    .owner
                    .and_then(|j| self.jobs.get(&j))
                    .map_or(JobState::Completed, |j| j.state),
            );
        }
        if let Some(owner) = agent.owner.and_then(|id| self.jobs.get(&id))
            && owner.state == JobState::WaitingInput
        {
            return State::Waiting(WaitReason::ParentInput);
        }
        match snapshot.activity.get(&agent.id) {
            Some(AgentActivity::Working) => State::Working,
            Some(AgentActivity::Reconnecting { attempt }) => {
                State::Reconnecting { attempt: *attempt }
            }
            Some(AgentActivity::Compacting) => State::Compacting,
            Some(AgentActivity::Interrupted) => State::Job(JobState::Interrupted),
            Some(AgentActivity::Failed(_)) => State::Job(JobState::Failed),
            Some(AgentActivity::WaitingChildren) => State::Waiting(WaitReason::Child),
            activity => {
                let jobs: Vec<_> = self
                    .jobs
                    .values()
                    .filter(|j| j.agent == agent.id && !j.state.is_terminal())
                    .collect();
                if !jobs.is_empty() && jobs.iter().all(|j| j.state == JobState::AwaitingApproval) {
                    State::Waiting(WaitReason::Permission)
                } else if jobs
                    .iter()
                    .any(|j| j.role == JobRole::Question || j.state == JobState::WaitingInput)
                {
                    State::Waiting(WaitReason::Input)
                } else if jobs
                    .iter()
                    .any(|j| j.activity == JobActivity::Wait && j.state == JobState::Running)
                {
                    // A wait pauses the agent even while its background jobs keep running.
                    State::Waiting(WaitReason::Event)
                } else if !jobs.is_empty() && jobs.iter().all(|j| j.role == JobRole::Agent) {
                    State::Waiting(WaitReason::Child)
                } else if matches!(activity, Some(AgentActivity::Tools)) || !jobs.is_empty() {
                    State::RunningTools
                } else {
                    State::Ready
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
    use super::super::tests::record;
    use super::*;
    use skyhook::identity::SessionId;

    fn agent() -> AgentInfo {
        AgentInfo {
            id: AgentId::root(SessionId::from_bytes([1; 16])),
            name: "Failed Waiting for permission".into(),
            model: "test".into(),
            mode: None,
            capabilities: Vec::new(),
            target: "root".into(),
            owner: None,
            lifecycle: AgentLifecycle::Active,
        }
    }

    fn job(agent: &AgentInfo, id: u64, role: JobRole, state: JobState) -> JobInfo {
        // Deliberately misleading: behavior comes from role, not this label.
        JobInfo {
            tool: "agent".into(),
            ..super::super::tests::job_info(&agent.id, id, role, state)
        }
    }

    #[test]
    fn display_precedence_preserves_terminal_owner_activity_and_job_order() {
        use AgentDisplayState as State;
        let mut agent = agent();
        let mut projection = Projection::default();
        let mut snapshot = ObservationSnapshot::default();
        let reconnecting = AgentActivity::Reconnecting { attempt: 2 };
        snapshot.activity.insert(agent.id.clone(), reconnecting);
        // Recovery is active without any job.
        assert_eq!(
            projection.status(&agent, &snapshot),
            State::Reconnecting { attempt: 2 }
        );
        let owner = job(&agent, 1, JobRole::Agent, JobState::WaitingInput);
        agent.owner = Some(owner.id);
        projection.jobs.insert(owner.id, owner);
        snapshot
            .activity
            .insert(agent.id.clone(), AgentActivity::Working);
        assert_eq!(
            projection.status(&agent, &snapshot),
            State::Waiting(WaitReason::ParentInput)
        );
        agent.lifecycle.complete(Instant::now());
        assert_eq!(
            projection.status(&agent, &snapshot),
            State::Job(JobState::WaitingInput)
        );
        agent.lifecycle = AgentLifecycle::Active;
        agent.owner = None;
        assert_eq!(projection.status(&agent, &snapshot), State::Working);
        snapshot.activity.clear();
        projection.jobs.clear();
        let tool = job(&agent, 2, JobRole::Tool, JobState::Running);
        projection.jobs.insert(tool.id, tool);
        assert_eq!(projection.status(&agent, &snapshot), State::RunningTools);
        projection
            .jobs
            .get_mut(&JobId::new(2).unwrap())
            .unwrap()
            .role = JobRole::Agent;
        assert_eq!(
            projection.status(&agent, &snapshot),
            State::Waiting(WaitReason::Child)
        );
        let wait = JobInfo {
            activity: JobActivity::Wait,
            ..job(&agent, 4, JobRole::Tool, JobState::Running)
        };
        projection.jobs.insert(wait.id, wait);
        assert_eq!(
            projection.status(&agent, &snapshot),
            State::Waiting(WaitReason::Event)
        );
        let question = job(&agent, 3, JobRole::Question, JobState::Running);
        projection.jobs.insert(question.id, question);
        assert_eq!(
            projection.status(&agent, &snapshot),
            State::Waiting(WaitReason::Input)
        );
        for job in projection.jobs.values_mut() {
            job.state = JobState::AwaitingApproval;
        }
        assert_eq!(
            projection.status(&agent, &snapshot),
            State::Waiting(WaitReason::Permission)
        );
        snapshot
            .activity
            .insert(agent.id.clone(), AgentActivity::WaitingChildren);
        assert_eq!(
            projection.status(&agent, &snapshot),
            State::Waiting(WaitReason::Child)
        );
    }

    #[test]
    fn wait_tool_status_tracks_classification_and_settlement() {
        use AgentDisplayState as State;
        let agent = agent();
        let waiting = State::Waiting(WaitReason::Event);
        assert_eq!(waiting.label(), "Waiting");
        for (tool, role, running) in [
            ("wait", JobRole::Tool, waiting),
            ("exec", JobRole::Tool, State::RunningTools),
            ("wait_fixture", JobRole::Tool, State::RunningTools),
            ("wait", JobRole::Script, State::RunningTools),
        ] {
            let mut snapshot = ObservationSnapshot::default();
            let mut projection = Projection::default();
            // A wait pauses the agent even with other owned work still running.
            let work = job(&agent, 1, JobRole::Tool, JobState::Running);
            projection.jobs.insert(work.id, work);
            let candidate = job(&agent, 2, role, JobState::Queued);
            record(
                &mut snapshot,
                &agent.id,
                SessionEvent::JobCreated {
                    job: candidate.id,
                    parent: None,
                    origin: None,
                    tool: tool.into(),
                    role,
                    name: None,
                    arguments: candidate.args,
                    output_schema: None,
                    accepts_input: false,
                    background: false,
                    authorization_scope: None,
                    location: candidate.location,
                },
            );
            for (state, expected) in [
                (JobState::Queued, State::RunningTools),
                (JobState::Running, running),
                (JobState::Completed, State::RunningTools),
            ] {
                record(
                    &mut snapshot,
                    &agent.id,
                    SessionEvent::JobStateChanged {
                        job: candidate.id,
                        state,
                    },
                );
                projection.rebuild(&snapshot);
                assert_eq!(projection.status(&agent, &snapshot), expected, "{tool}");
            }
        }
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
    fn start(owner_job: Option<JobId>) -> SessionEvent {
        SessionEvent::AgentStarted {
            parent: None,
            owner_job,
            profile: None,
            available_depth: 0,
            mode: None,
            capabilities: Vec::new(),
            location: ExecutionLocation::root("/workspace".into()),
        }
    }

    #[test]
    fn completion_grace_retires_once_and_running_owner_reopens() {
        let root = agent().id;
        let child = root.child(1);
        let mut projection = Projection::default();
        let mut snapshot = ObservationSnapshot::default();
        let owner = job(&agent(), 1, JobRole::Agent, JobState::WaitingInput);
        let owner_id = owner.id;
        projection.jobs.insert(owner_id, owner);
        record(&mut snapshot, &child, start(Some(owner_id)));
        record(&mut snapshot, &child, SessionEvent::AgentCompleted);
        projection.rebuild(&snapshot);
        assert!(projection.agents[0].terminal());
        assert!(!projection.has_active_children());
        assert_eq!(projection.visible(&root).len(), 1);
        let initial = projection.agents[0].lifecycle;
        projection.complete_agent(&child);
        assert_eq!(projection.agents[0].lifecycle, initial);
        projection.agents[0].lifecycle = expired_grace();
        assert!(projection.retire_completed_grace());
        assert!(!projection.retire_completed_grace());
        assert!(projection.visible(&root).is_empty());
        assert_eq!(projection.visible(&child.child(2)).len(), 1);
        // Ordinary activity cannot reopen a completed agent with a waiting owner.
        snapshot
            .activity
            .insert(child.clone(), AgentActivity::Working);
        projection.rebuild(&snapshot);
        assert!(projection.agents[0].terminal());
        record(
            &mut snapshot,
            &child,
            SessionEvent::JobStateChanged {
                job: owner_id,
                state: JobState::Running,
            },
        );
        projection.rebuild(&snapshot);
        assert!(!projection.agents[0].terminal());
        assert!(projection.has_active_children());
        projection.jobs.get_mut(&owner_id).unwrap().state = JobState::Completed;
        projection.rebuild(&snapshot);
        assert!(projection.agents[0].terminal());
        // Explicit repeated completion, unlike owner reconciliation, renews grace.
        projection.complete_agent(&child);
        assert_eq!(projection.visible(&root).len(), 1);
    }

    fn expired_grace() -> AgentLifecycle {
        AgentLifecycle::Terminal {
            grace_started: Instant::now().checked_sub(COMPLETED_AGENT_GRACE),
        }
    }
}
