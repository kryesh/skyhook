//! Incremental journal indexes, agent lifecycle state, and usage totals.

use super::super::format::agent_label;
use super::entries::Dep;
use super::state_name;
use serde_json::Value;
use skyhook::agent::{AgentActivity, ObservationSnapshot, TodoItem, TurnFailure};
use skyhook::execution::ExecutionLocation;
use skyhook::identity::{AgentId, JobId};
use skyhook::job::{JobRole, JobState, JobTransition};
use skyhook::provider::protocol::Usage;
use skyhook::session::{MessageSeq, RecordSeq, SessionEvent};
use skyhook::target::TargetRef;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ops::Bound;
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
    /// The model the agent runs on; none for a tool-only agent.
    pub model: Option<skyhook::provider::profile::ModelRef>,
    /// The root agent's applied mode.
    pub mode: Option<skyhook::tool::policy::ModeName>,
    pub capabilities: Vec<skyhook::tool::policy::Capability>,
    pub target: TargetRef,
    pub owner: Option<JobId>,
    pub lifecycle: AgentLifecycle,
}
impl AgentInfo {
    pub fn terminal(&self) -> bool {
        matches!(self.lifecycle, AgentLifecycle::Terminal { .. })
    }
}

#[derive(Clone)]
pub struct JobInfo {
    pub id: JobId,
    pub agent: AgentId,
    pub name: Option<String>,
    pub tool: String,
    pub role: JobRole,
    pub args: Value,
    pub parent: Option<JobId>,
    pub state: JobState,
    pub location: ExecutionLocation,
    pub error: Option<String>,
}

impl JobInfo {
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

#[derive(Default)]
pub struct Projection {
    pub agents: Vec<AgentInfo>,
    /// Written only through the record fold, which keeps `open_jobs` in step.
    jobs: BTreeMap<JobId, JobInfo>,
    /// Each agent's unfinished jobs.
    open_jobs: HashMap<AgentId, BTreeSet<JobId>>,
    pub usage: Usage,
    pub agent_usage: HashMap<AgentId, Usage>,
    pub todos: HashMap<AgentId, Vec<TodoItem>>,
    pub(super) through: RecordSeq,
    pub(super) records_by_agent: HashMap<AgentId, Vec<RecordSeq>>,
    /// Calls that admitted a job, so the job card stands in for the call.
    pub(super) tool_origins: HashSet<(AgentId, MessageSeq, String)>,
    /// What records folded since the last `take_changes` touched.
    changes: HashSet<Dep>,
}
impl Projection {
    pub fn jobs(&self) -> &BTreeMap<JobId, JobInfo> {
        &self.jobs
    }

    pub(super) fn agent_name(&self, agent: &AgentId) -> &str {
        let info = self.agents.iter().find(|info| &info.id == agent);
        info.map_or("Agent", |info| info.name.as_str())
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

    /// Everything records folded since the last call touched, for history to rebuild.
    pub(super) fn take_changes(&mut self) -> HashSet<Dep> {
        std::mem::take(&mut self.changes)
    }

    /// Every job state change goes through here, so the open index follows each
    /// transition, including a finished child resumed under its own job.
    fn set_job_state(&mut self, job: JobId, state: JobState) -> Option<&mut JobInfo> {
        let info = self.jobs.get_mut(&job)?;
        info.state = state;
        let open = self.open_jobs.entry(info.agent.clone()).or_default();
        if state.is_terminal() {
            open.remove(&job);
        } else {
            open.insert(job);
        }
        Some(info)
    }

    pub fn rebuild(&mut self, snapshot: &ObservationSnapshot) {
        let requests = snapshot.ledger.changed_after(self.through);
        self.changes.extend(requests.map(Dep::Request));
        let after = (Bound::Excluded(self.through), Bound::Unbounded);
        for (_, record) in snapshot.records.range(after) {
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
                    // The owner job's card shows where its agent runs.
                    self.changes.extend(owner_job.map(Dep::Job));
                    self.agents.retain(|agent| agent.id != record.agent);
                    self.agents.push(AgentInfo {
                        id: record.agent.clone(),
                        name: if record.agent.path().is_empty() {
                            "skyhook".into()
                        } else {
                            format!("agent {}", agent_label(&record.agent))
                        },
                        model: profile.as_ref().map(|profile| profile.name.clone()),
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
                    self.changes.insert(Dep::Job(*job));
                    if let Some(origin) = origin {
                        let call = origin.call_id.clone();
                        self.changes.insert(Dep::Call(origin.message, call));
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
                            name: name.clone().map(String::from),
                            tool: tool.clone(),
                            role: *role,
                            args: arguments.clone(),
                            parent: *parent,
                            state: JobState::Queued,
                            location: location.clone(),
                            error: None,
                        },
                    );
                    self.set_job_state(*job, JobState::Queued);
                }
                SessionEvent::JobStateChanged { job, state } => {
                    self.changes.insert(Dep::Job(*job));
                    self.set_job_state(*job, (*state).into());
                    // Resuming a retained child reuses its owner job and does not
                    // emit AgentStarted again. Reopen its tree row on that job's
                    // Running event, not on ordinary model/tool activity updates.
                    if *state == JobTransition::Running {
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
                    self.changes.insert(Dep::Job(*job));
                    let capabilities = self
                        .agents
                        .iter()
                        .find(|agent| agent.id == record.agent)
                        .map(|agent| agent.capabilities.iter().copied().collect())
                        .unwrap_or_else(skyhook::tool::policy::CapabilitySet::empty);
                    if let Some(info) = self.set_job_state(*job, (*state).into()) {
                        info.error = diagnostic
                            .as_ref()
                            .map(|diagnostic| diagnostic.render(&capabilities));
                    }
                }
                SessionEvent::AgentCompleted | SessionEvent::AgentInterrupted => {
                    self.complete_agent(&record.agent);
                }
                SessionEvent::ModelChanged { profile } => {
                    if let Some(agent) = self.agents.iter_mut().find(|a| a.id == record.agent) {
                        agent.model = Some(profile.name.clone());
                    }
                }
                SessionEvent::ModeChanged { mode, capabilities } => {
                    if let Some(agent) = self.agents.iter_mut().find(|a| a.id == record.agent) {
                        agent.mode = Some(mode.name.clone());
                        agent.capabilities.clone_from(capabilities);
                    }
                }
                SessionEvent::Usage { usage, .. } => {
                    self.usage.accumulate(*usage);
                    self.agent_usage
                        .entry(record.agent.clone())
                        .or_default()
                        .accumulate(*usage);
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
    /// Where a child of `caller` runs, given the agent call's `target` argument;
    /// a requested target that does not parse is returned as given.
    pub(super) fn child_target<'a>(
        &self,
        caller: &AgentId,
        target: Option<&'a Value>,
    ) -> Result<TargetRef, &'a str> {
        match target.and_then(Value::as_str) {
            Some(requested) => requested.parse().map_err(|_| requested),
            None => Ok(self
                .agents
                .iter()
                .find(|agent| &agent.id == caller)
                .map_or(TargetRef::Root, |agent| agent.target.clone())),
        }
    }
    pub(super) fn job_target<'a>(&self, job: &'a JobInfo) -> Result<TargetRef, &'a str> {
        if job.role == JobRole::Agent {
            self.agents
                .iter()
                .find(|agent| agent.owner == Some(job.id))
                .map_or_else(
                    || self.child_target(&job.agent, job.args.get(skyhook::tool::TARGET)),
                    |agent| Ok(agent.target.clone()),
                )
        } else {
            Ok(job.location.target.clone())
        }
    }
    /// An agent's footer statistics.
    pub fn agent_stats(&self, snapshot: &ObservationSnapshot, agent: &AgentId) -> [String; 3] {
        let usage = self.agent_usage.get(agent).copied().unwrap_or_default();
        let context = snapshot.context.get(agent);
        super::super::format::footer_stats(usage, context.map(|c| (c.tokens, c.capacity)))
    }
    pub fn has_open_jobs(&self) -> bool {
        self.open_jobs.values().any(|jobs| !jobs.is_empty())
    }
    /// Indices of the agents the live tree shows.
    pub fn visible<'a>(&'a self, selected: &'a AgentId) -> impl Iterator<Item = usize> + 'a {
        let shown = |a: &AgentInfo| {
            a.id.path().is_empty()
                || !a.terminal()
                || selected.is_within(&a.id)
                || a.lifecycle.within_grace()
        };
        let agents = self.agents.iter().enumerate();
        agents.filter_map(move |(index, agent)| shown(agent).then_some(index))
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
            Some(AgentActivity::Stopped(TurnFailure::Interrupted)) => {
                State::Job(JobState::Interrupted)
            }
            Some(AgentActivity::Stopped(_)) => State::Job(JobState::Failed),
            Some(AgentActivity::WaitingChildren) => State::Waiting(WaitReason::Child),
            activity => {
                let open = self.open_jobs.get(&agent.id).into_iter().flatten();
                let jobs: Vec<_> = open.filter_map(|id| self.jobs.get(id)).collect();
                if !jobs.is_empty() && jobs.iter().all(|j| j.state == JobState::AwaitingApproval) {
                    State::Waiting(WaitReason::Permission)
                } else if jobs
                    .iter()
                    .any(|j| j.role == JobRole::Question || j.state == JobState::WaitingInput)
                {
                    State::Waiting(WaitReason::Input)
                } else if jobs
                    .iter()
                    .any(|j| j.role == JobRole::Wait && j.state == JobState::Running)
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

#[cfg(test)]
mod tests {
    use super::super::tests::{Journal, created, finished};
    use super::*;

    fn agent(id: AgentId) -> AgentInfo {
        AgentInfo {
            id,
            name: "Failed Waiting for permission".into(),
            model: Some("test/test".parse().unwrap()),
            mode: None,
            capabilities: Vec::new(),
            target: TargetRef::Root,
            owner: None,
            lifecycle: AgentLifecycle::Active,
        }
    }

    fn changed(id: u64, state: JobTransition) -> SessionEvent {
        let job = JobId::new(id).unwrap();
        SessionEvent::JobStateChanged { job, state }
    }

    fn status(
        journal: &Journal,
        projection: &mut Projection,
        agent: &AgentInfo,
    ) -> AgentDisplayState {
        projection.rebuild(&journal.snapshot);
        projection.status(agent, &journal.snapshot)
    }

    #[tokio::test]
    async fn display_precedence_preserves_terminal_owner_activity_and_job_order() {
        use AgentDisplayState as State;
        use JobTransition::{AwaitingApproval, Running, WaitingInput};
        let mut journal = Journal::new().await;
        let root = journal.agent();
        let id = root.child(1);
        let mut agent = agent(id.clone());
        let mut projection = Projection::default();
        let activity = |journal: &mut Journal, activity| {
            journal.snapshot.activity.insert(id.clone(), activity);
        };
        activity(&mut journal, AgentActivity::Reconnecting { attempt: 2 });
        // Recovery is active without any job.
        let state = status(&journal, &mut projection, &agent);
        assert_eq!(state, State::Reconnecting { attempt: 2 });
        // Behavior comes from roles, not the deliberately misleading tool names.
        journal
            .record(&root, created(1, "agent", JobRole::Agent))
            .await;
        journal.record(&root, changed(1, WaitingInput)).await;
        agent.owner = JobId::new(1).ok();
        journal.record(&id, start(agent.owner)).await;
        activity(&mut journal, AgentActivity::Working);
        let state = status(&journal, &mut projection, &agent);
        assert_eq!(state, State::Waiting(WaitReason::ParentInput));
        agent.lifecycle.complete(Instant::now());
        let state = status(&journal, &mut projection, &agent);
        assert_eq!(state, State::Job(JobState::WaitingInput));
        agent.lifecycle = AgentLifecycle::Active;
        agent.owner = None;
        assert_eq!(status(&journal, &mut projection, &agent), State::Working);
        journal.snapshot.activity.clear();
        journal
            .record(&id, created(2, "agent", JobRole::Tool))
            .await;
        journal.record(&id, changed(2, Running)).await;
        assert_eq!(
            status(&journal, &mut projection, &agent),
            State::RunningTools
        );
        journal.record(&id, finished(2)).await;
        journal
            .record(&id, created(3, "agent", JobRole::Agent))
            .await;
        let state = status(&journal, &mut projection, &agent);
        assert_eq!(state, State::Waiting(WaitReason::Child));
        journal.record(&id, created(4, "wait", JobRole::Wait)).await;
        journal.record(&id, changed(4, Running)).await;
        let state = status(&journal, &mut projection, &agent);
        assert_eq!(state, State::Waiting(WaitReason::Event));
        journal
            .record(&id, created(5, "agent", JobRole::Question))
            .await;
        let state = status(&journal, &mut projection, &agent);
        assert_eq!(state, State::Waiting(WaitReason::Input));
        for job in [3, 4, 5] {
            journal.record(&id, changed(job, AwaitingApproval)).await;
        }
        let state = status(&journal, &mut projection, &agent);
        assert_eq!(state, State::Waiting(WaitReason::Permission));
        activity(&mut journal, AgentActivity::WaitingChildren);
        let state = status(&journal, &mut projection, &agent);
        assert_eq!(state, State::Waiting(WaitReason::Child));
    }

    #[tokio::test]
    async fn wait_tool_status_tracks_classification_and_settlement() {
        use AgentDisplayState as State;
        let waiting = State::Waiting(WaitReason::Event);
        for (tool, role, running) in [
            ("wait", JobRole::Wait, waiting),
            ("exec", JobRole::Tool, State::RunningTools),
        ] {
            let mut journal = Journal::new().await;
            let agent = agent(journal.agent());
            let mut projection = Projection::default();
            // A wait pauses the agent even with other owned work still running.
            let work = [
                created(1, "exec", JobRole::Tool),
                changed(1, JobTransition::Running),
            ];
            for event in work.into_iter().chain([created(2, tool, role)]) {
                journal.record(&agent.id, event).await;
            }
            let state = status(&journal, &mut projection, &agent);
            assert_eq!(state, State::RunningTools, "{tool}");
            journal
                .record(&agent.id, changed(2, JobTransition::Running))
                .await;
            assert_eq!(status(&journal, &mut projection, &agent), running, "{tool}");
            journal.record(&agent.id, finished(2)).await;
            let state = status(&journal, &mut projection, &agent);
            assert_eq!(state, State::RunningTools, "{tool}");
        }
    }

    fn start(owner_job: Option<JobId>) -> SessionEvent {
        SessionEvent::AgentStarted {
            owner_job,
            profile: None,
            available_depth: 0,
            mode: None,
            capabilities: Vec::new(),
            location: ExecutionLocation::root("/workspace".into()),
        }
    }

    #[tokio::test]
    async fn completion_grace_retires_once_and_running_owner_reopens() {
        let mut journal = Journal::new().await;
        let root = journal.agent();
        let child = root.child(1);
        let mut projection = Projection::default();
        journal
            .record(&root, created(1, "agent", JobRole::Agent))
            .await;
        journal
            .record(&root, changed(1, JobTransition::WaitingInput))
            .await;
        let owner_id = JobId::new(1).unwrap();
        journal.record(&child, start(Some(owner_id))).await;
        journal.record(&child, SessionEvent::AgentCompleted).await;
        projection.rebuild(&journal.snapshot);
        assert!(projection.agents[0].terminal());
        assert!(!projection.has_active_children());
        assert_eq!(projection.visible(&root).count(), 1);
        let initial = projection.agents[0].lifecycle;
        projection.complete_agent(&child);
        assert_eq!(projection.agents[0].lifecycle, initial);
        projection.agents[0].lifecycle = expired_grace();
        assert!(projection.retire_completed_grace());
        assert!(!projection.retire_completed_grace());
        assert_eq!(projection.visible(&root).count(), 0);
        assert_eq!(projection.visible(&child.child(2)).count(), 1);
        // Ordinary activity cannot reopen a completed agent with a waiting owner.
        journal
            .snapshot
            .activity
            .insert(child.clone(), AgentActivity::Working);
        projection.rebuild(&journal.snapshot);
        assert!(projection.agents[0].terminal());
        journal
            .record(&child, changed(1, JobTransition::Running))
            .await;
        projection.rebuild(&journal.snapshot);
        assert!(!projection.agents[0].terminal());
        assert!(projection.has_active_children());
        journal.record(&root, finished(1)).await;
        projection.rebuild(&journal.snapshot);
        assert!(projection.agents[0].terminal());
        // Explicit repeated completion, unlike owner reconciliation, renews grace.
        projection.complete_agent(&child);
        assert_eq!(projection.visible(&root).count(), 1);
        // A retained child resumes under its finished owner job, which is open work.
        journal
            .record(&child, changed(1, JobTransition::Running))
            .await;
        projection.rebuild(&journal.snapshot);
        assert!(projection.has_open_jobs());
        let owner = projection.status(&agent(root), &journal.snapshot);
        assert_eq!(owner, AgentDisplayState::Waiting(WaitReason::Child));
    }

    fn expired_grace() -> AgentLifecycle {
        AgentLifecycle::Terminal {
            grace_started: Instant::now().checked_sub(COMPLETED_AGENT_GRACE),
        }
    }
}
