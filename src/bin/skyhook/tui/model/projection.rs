//! Incremental journal indexes, agent lifecycle state, and usage totals.

use super::super::format::agent_label;
use super::entries::Dep;
use super::{Timing, state_name};
use serde_json::Value;
use skyhook::agent::{AgentActivity, ObservationSnapshot, StoppedReading, TodoItem, TurnFailure};
use skyhook::identity::{AgentId, JobId};
use skyhook::job::{JobEnvelope, JobRole, JobState, JobTransition};
use skyhook::provider::protocol::UsageTotals;
use skyhook::session::{Message, MessageSeq, RecordSeq, SessionEvent, Turns};
use skyhook::target::TargetRef;
use std::collections::{HashMap, HashSet};
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
    /// A turn that ended without its answer, as its job would have ended.
    pub fn stopped(failure: &TurnFailure) -> Self {
        match failure {
            TurnFailure::Interrupted => Self::Job(JobState::Interrupted),
            TurnFailure::Cancelled => Self::Job(JobState::Cancelled),
            TurnFailure::Failed(_) => Self::Job(JobState::Failed),
        }
    }

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
    /// The model the agent runs on.
    pub model: skyhook::provider::profile::ModelRef,
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

#[derive(Default)]
pub struct Projection {
    pub agents: Vec<AgentInfo>,
    pub usage: UsageTotals,
    pub agent_usage: HashMap<AgentId, UsageTotals>,
    pub todos: HashMap<AgentId, Vec<TodoItem>>,
    pub(super) through: RecordSeq,
    pub(super) records_by_agent: HashMap<AgentId, Vec<RecordSeq>>,
    /// Calls that admitted a job, so the job card stands in for the call.
    pub(super) tool_origins: HashSet<(AgentId, MessageSeq, String)>,
    /// What records and job views folded since the last `take_changes` touched.
    changes: HashSet<Dep>,
    turns: Turns,
    /// Each agent's most recently finished turn.
    last_turn: HashMap<AgentId, Timing>,
    /// When each assistant message's turn began.
    pub(super) turn_starts: HashMap<MessageSeq, i64>,
}
/// Whether any job the session holds has yet to end.
pub fn open_jobs(snapshot: &ObservationSnapshot) -> bool {
    snapshot.jobs.values().any(|job| !job.state.is_terminal())
}

impl Projection {
    /// A projection of everything `snapshot` holds.
    pub fn new(snapshot: &ObservationSnapshot) -> Self {
        let mut projection = Self::default();
        (snapshot.jobs.values()).for_each(|job| projection.observe_job(job));
        projection.rebuild(snapshot);
        projection
    }

    /// Fold a job's view as it arrives, before the record that moved it.
    pub fn observe_job(&mut self, job: &JobEnvelope) {
        self.changes.insert(Dep::Job(job.id));
    }

    /// Fold the jobs and requests the session's stopped reading changed, which
    /// no record reports, as their own updates would. Returns whether it changed
    /// any: the projection then rebuilds as for a record, so agents reconcile
    /// with their owner jobs.
    pub fn observe_stopped(&mut self, reading: StoppedReading) -> bool {
        let changed = !(reading.jobs.is_empty() && reading.requests.is_empty());
        self.changes.extend(reading.jobs.into_iter().map(Dep::Job));
        self.changes
            .extend(reading.requests.into_iter().map(Dep::Request));
        changed
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

    /// Everything folded since the last call touched, for history to rebuild.
    pub(super) fn take_changes(&mut self) -> HashSet<Dep> {
        std::mem::take(&mut self.changes)
    }

    /// An agent's clock: its turn while busy and not terminal, otherwise its owner
    /// job's run, or for an agent no job owns (the root) its last finished turn.
    /// Replay can leave a terminal agent busy after a crash.
    pub fn timing(&self, agent: &AgentInfo, snapshot: &ObservationSnapshot) -> Timing {
        match snapshot.activity.get(&agent.id) {
            Some(activity) if !agent.terminal() && activity.state.is_busy() => {
                Timing::Since(activity.since)
            }
            _ => match agent.owner.and_then(|job| snapshot.jobs.get(&job)) {
                Some(job) => job.timing.into(),
                None => (self.last_turn.get(&agent.id).copied()).unwrap_or(Timing::Untimed),
            },
        }
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
            if let Some(since) = self.turns.observe(record, &snapshot.ledger) {
                let until = record.timestamp_millis;
                self.last_turn
                    .insert(record.agent.clone(), Timing::Took { since, until });
            }
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
                        model: profile.name.clone(),
                        mode: mode.as_ref().map(|mode| mode.name.clone()),
                        capabilities: capabilities.clone(),
                        target: location.target.clone(),
                        owner: *owner_job,
                        lifecycle: AgentLifecycle::Active,
                    });
                }
                SessionEvent::MessageCommitted {
                    message: Message::Assistant(_),
                } => {
                    if let Some(started) = self.turns.started(&record.agent) {
                        (self.turn_starts).insert(record.sequence.message(), started);
                    }
                }
                SessionEvent::MessageCommitted {
                    message: Message::Tool(result),
                } => {
                    // The call shows the result that answered it.
                    let answered = snapshot.ledger.answered(record.sequence);
                    let call = answered.map(|message| Dep::Call(message, result.call_id.clone()));
                    self.changes.extend(call);
                }
                SessionEvent::JobCreated {
                    origin: Some(origin),
                    ..
                } => {
                    let call = origin.call_id.clone();
                    self.changes.insert(Dep::Call(origin.message, call.clone()));
                    (self.tool_origins).insert((record.agent.clone(), origin.message, call));
                }
                SessionEvent::JobStateChanged { job, state } => {
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
                // Stopping the root ends its process, not the agent: a reopened
                // session resumes it, and its activity says how.
                SessionEvent::AgentCompleted | SessionEvent::AgentInterrupted
                    if record.agent.parent().is_some() =>
                {
                    self.complete_agent(&record.agent);
                }
                SessionEvent::ModelChanged { profile } => {
                    if let Some(agent) = self.agents.iter_mut().find(|a| a.id == record.agent) {
                        agent.model = profile.name.clone();
                    }
                }
                SessionEvent::ModeChanged { mode, capabilities } => {
                    if let Some(agent) = self.agents.iter_mut().find(|a| a.id == record.agent) {
                        agent.mode = Some(mode.name.clone());
                        agent.capabilities.clone_from(capabilities);
                    }
                }
                SessionEvent::Usage { usage, .. } => {
                    self.usage.add(*usage);
                    self.agent_usage
                        .entry(record.agent.clone())
                        .or_default()
                        .add(*usage);
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
            if let Some(job) = agent.owner.and_then(|id| snapshot.jobs.get(&id)) {
                if let Some(name) = &job.name {
                    agent.name = name.to_string();
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
    pub(super) fn job_target<'a>(&self, job: &'a JobEnvelope) -> Result<TargetRef, &'a str> {
        if job.role == JobRole::Agent {
            self.agents
                .iter()
                .find(|agent| agent.owner == Some(job.id))
                .map_or_else(
                    || self.child_target(&job.agent, job.arguments.get(skyhook::tool::TARGET)),
                    |agent| Ok(agent.target.clone()),
                )
        } else {
            Ok(job.location.target.clone())
        }
    }
    /// Why `job` failed, as its agent's capabilities render it.
    pub(super) fn job_error(&self, job: &JobEnvelope) -> Option<String> {
        let agent = self.agents.iter().find(|agent| agent.id == job.agent);
        let capabilities = agent.map(|agent| agent.capabilities.iter().copied().collect());
        let capabilities = capabilities.unwrap_or_else(skyhook::tool::policy::CapabilitySet::empty);
        job.diagnostic
            .as_ref()
            .map(|diagnostic| diagnostic.render(&capabilities))
    }
    /// An agent's footer statistics.
    pub fn agent_stats(&self, snapshot: &ObservationSnapshot, agent: &AgentId) -> [String; 3] {
        let usage = self.agent_usage.get(agent).copied().unwrap_or_default();
        let context = snapshot.context.get(agent);
        super::super::format::footer_stats(usage, context.map(|c| (c.tokens, c.capacity)))
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
        let owner = agent.owner.and_then(|id| snapshot.jobs.get(&id));
        if agent.terminal() {
            return State::Job(owner.map_or(JobState::Completed, |job| job.state));
        }
        if let Some(owner) = owner
            && owner.state == JobState::WaitingInput
        {
            return State::Waiting(WaitReason::ParentInput);
        }
        match snapshot
            .activity
            .get(&agent.id)
            .map(|activity| &activity.state)
        {
            Some(AgentActivity::Working) => State::Working,
            Some(AgentActivity::Reconnecting { attempt }) => {
                State::Reconnecting { attempt: *attempt }
            }
            Some(AgentActivity::Compacting) => State::Compacting,
            Some(AgentActivity::Stopped(failure)) => State::stopped(failure),
            Some(AgentActivity::WaitingChildren) => State::Waiting(WaitReason::Child),
            activity => {
                let jobs: Vec<_> = (snapshot.jobs.values())
                    .filter(|job| job.agent == agent.id && !job.state.is_terminal())
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
    use super::*;
    use crate::interaction::{ApprovalReply, Prompt, PromptKind, Reply};
    use crate::tests::bounded;
    use crate::tui::app::{
        App,
        played::{
            activity, answer_child, answered, ask, asking, at, attended_fixture, call, children,
            completed, count, delegate, exchange, job_finished, job_of_call, job_state, next_turn,
            observe_find, observe_record, observe_until, played_fixture, reached, recorded,
            requested, spawn_children,
        },
    };
    use serde_json::json;
    use skyhook::agent::ObservedActivity;
    use skyhook::provider::protocol::{AssistantItem, ContextId};
    use tokio::sync::mpsc::UnboundedReceiver;

    fn agent(projection: &Projection, id: &AgentId) -> AgentInfo {
        let mut agents = projection.agents.iter();
        agents.find(|agent| &agent.id == id).unwrap().clone()
    }

    /// What `app` shows agent `id` doing, as a fresh projection of its snapshot,
    /// as a resubscription builds, agrees.
    fn status(app: &App, id: &AgentId) -> AgentDisplayState {
        let state = (app.projection).status(&agent(&app.projection, id), &app.snapshot);
        let fresh = Projection::new(&app.snapshot);
        assert_eq!(fresh.status(&agent(&fresh, id), &app.snapshot), state);
        state
    }

    /// The job `app` last saw created for `tool`, once there is one.
    fn job_for(app: &App, tool: &str) -> Option<JobId> {
        let mut jobs = app.snapshot.jobs.values().rev();
        jobs.find(|job| job.tool == tool).map(|job| job.id)
    }

    /// The reply to the next approval the session asks for.
    async fn approval(prompts: &mut UnboundedReceiver<Prompt>) -> Reply<ApprovalReply> {
        let prompt = bounded(prompts.recv()).await.unwrap();
        let PromptKind::Approval { reply, .. } = prompt.kind else {
            panic!("an approval")
        };
        reply
    }

    /// An agent no job owns (the root) shows its last finished turn once idle.
    #[tokio::test]
    async fn an_idle_root_shows_its_last_turn() {
        let (_root, mut app, mut model, _session) = played_fixture().await;
        let root = app.root_agent().clone();
        let answer = vec![AssistantItem::text("text", 0, "done")];
        exchange(&mut app, &mut model, "question", answer).await;
        observe_until(&mut app, |app| {
            activity(&app.snapshot, &root) == Some(&AgentActivity::Idle)
                && recorded(&app.snapshot, completed)
        })
        .await;
        let since = at(&app.snapshot, requested, 0);
        let until = at(&app.snapshot, completed, 0);
        let took = Timing::Took { since, until };
        let root = agent(&app.projection, &root);
        assert_eq!(app.projection.timing(&root, &app.snapshot), took);
    }

    /// A child shows its turn while busy and its owner job's run otherwise, also
    /// once a crash before its completion record leaves it busy though finished.
    #[tokio::test]
    async fn a_child_shows_its_turn_while_busy_and_its_owner_jobs_run_otherwise() {
        let (_root, mut app, mut model, _session) = played_fixture().await;
        let (children, mut turns) = spawn_children(&mut app, &mut model, 1).await;
        turns.remove(0).answer("done");
        let owner = observe_find(&mut app, |app| job_of_call(&app.snapshot, "agent-0")).await;
        observe_record(&mut app, job_finished(owner)).await;
        // As it shows before its completion record lands.
        let mut child = AgentInfo {
            lifecycle: AgentLifecycle::Active,
            ..agent(&app.projection, &children[0])
        };
        let mut snapshot = app.snapshot.clone();
        let ran = Timing::from(snapshot.jobs[&owner].timing);
        assert_eq!(app.projection.timing(&child, &snapshot), ran);
        let working = ObservedActivity {
            state: AgentActivity::Working,
            since: 7,
        };
        snapshot.activity.insert(child.id.clone(), working);
        assert_eq!(app.projection.timing(&child, &snapshot), Timing::Since(7));
        child.lifecycle = AgentLifecycle::Terminal {
            grace_started: None,
        };
        assert_eq!(app.projection.timing(&child, &snapshot), ran);
    }

    /// Closing a session stops its root mid-turn, and what it runs, as a crash
    /// would: the session no longer works, and a reopen resumes the root, which
    /// shows how it resumes, viewed and once running, never completed.
    #[tokio::test]
    async fn a_stopped_root_resumes_rather_than_completing() {
        use crate::tui::app::{
            SlotKey,
            played::{played_fixture, sleeping_script},
            tests::attach,
        };
        use skyhook::session::SessionPhase;
        let (_root, mut app, mut model, session) = played_fixture().await;
        let _asked = ask(&session, "question");
        let _turn = model.turn().await;
        let _script = sleeping_script(&session);
        observe_until(&mut app, |app| open_jobs(&app.snapshot)).await;
        session.close().await.unwrap();
        observe_until(&mut app, |app| app.snapshot.phase == SessionPhase::Closed).await;
        assert!(!app.peer(SlotKey::default(), true).working);
        let session = app.launch.create(Some(session.id())).await.unwrap();
        attach(&mut app, session.clone());
        let stopped = AgentDisplayState::stopped(&TurnFailure::Interrupted);
        assert_eq!(app.peer(SlotKey::default(), true).state, stopped);
        let root = app.root_agent().clone();
        session
            .record_status(root, "reopened".into())
            .await
            .unwrap();
        let reopened = |event: &SessionEvent| matches!(event, SessionEvent::Status { message } if message == "reopened");
        observe_record(&mut app, reopened).await;
        assert_eq!(app.peer(SlotKey::default(), true).state, stopped);
        session.shutdown().await.unwrap();
    }

    /// A finished child shows its owner job's state; a child whose owner waits
    /// for its parent's input shows that over its own activity; recovery shows
    /// without any job. Otherwise an agent's jobs say what it waits on, over its
    /// running tools: approval while every job awaits it, then input, an event or
    /// its children, as does an answered child whose own work still runs.
    #[tokio::test]
    async fn display_precedence_preserves_terminal_owner_activity_and_job_order() {
        use AgentDisplayState as State;
        let (_root, mut app, mut model, session, mut prompts) = attended_fixture().await;
        let root = app.root_agent().clone();
        let mut turns = delegate(&session, &mut model, &[json!({"prompt": "ask"})]).await;
        observe_until(&mut app, |app| !children(&app.snapshot).is_empty()).await;
        let child = children(&app.snapshot).remove(0);
        let doing = |app: &mut App, state| {
            let activity = ObservedActivity { state, since: 0 };
            app.snapshot.activity.insert(child.clone(), activity);
        };
        doing(&mut app, AgentActivity::Reconnecting { attempt: 2 });
        assert_eq!(status(&app, &child), State::Reconnecting { attempt: 2 });
        assert_eq!(status(&app, &root), State::Waiting(WaitReason::Child));

        let sleep = json!({"command": ["sleep", "60"]});
        turns.remove(0).end(vec![
            call("exec-0", "exec", sleep.clone(), 0),
            call("exec-1", "exec", sleep, 1),
        ]);
        let allow = approval(&mut prompts).await;
        let mut execs = Vec::new();
        for id in ["exec-0", "exec-1"] {
            let exec = observe_find(&mut app, |app| job_of_call(&app.snapshot, id)).await;
            reached(&mut app, exec, JobTransition::AwaitingApproval).await;
            execs.push(exec);
        }
        assert_eq!(status(&app, &child), State::Waiting(WaitReason::Permission));
        // Allowed once, one call runs while the other still awaits approval.
        allow.send(Ok(ApprovalReply::Allow)).unwrap();
        let running = |app: &App| {
            let ran =
                |exec: &JobId| recorded(&app.snapshot, job_state(*exec, JobTransition::Running));
            execs.iter().any(ran)
        };
        observe_until(&mut app, running).await;
        assert_eq!(status(&app, &child), State::RunningTools);
        for exec in execs {
            session.cancel_job(exec).await.unwrap();
        }

        let turn = next_turn(&mut model, &child).await;
        let work = json!({"source": "return await receive();", "bg": true});
        turn.end(vec![
            call("work", "script", work, 0),
            call("wait", "wait", json!({}), 1),
        ]);
        let work = observe_find(&mut app, |app| job_for(app, "script")).await;
        let wait = observe_find(&mut app, |app| job_for(app, "wait")).await;
        reached(&mut app, wait, JobTransition::Running).await;
        assert_eq!(status(&app, &child), State::Waiting(WaitReason::Event));
        session.cancel_job(wait).await.unwrap();

        let turn = next_turn(&mut model, &child).await;
        turn.end(vec![asking()]);
        let owner = agent(&app.projection, &child).owner.unwrap();
        reached(&mut app, owner, JobTransition::WaitingInput).await;
        // The root waits on with the question open: input over an event.
        model
            .turn()
            .await
            .end(vec![call("hold", "wait", json!({}), 0)]);
        let hold = observe_find(&mut app, |app| job_of_call(&app.snapshot, "hold")).await;
        reached(&mut app, hold, JobTransition::Running).await;
        doing(&mut app, AgentActivity::Working);
        assert_eq!(
            status(&app, &child),
            State::Waiting(WaitReason::ParentInput)
        );
        assert_eq!(status(&app, &root), State::Waiting(WaitReason::Input));

        answer_child(&session, owner, "{pick: 'this'}").await;
        let turn = next_turn(&mut model, &child).await;
        turn.answer("progress");
        let waiting =
            |app: &App| activity(&app.snapshot, &child) == Some(&AgentActivity::WaitingChildren);
        observe_until(&mut app, waiting).await;
        assert_eq!(status(&app, &child), State::Waiting(WaitReason::Child));

        session.cancel_job(work).await.unwrap();
        let turn = next_turn(&mut model, &child).await;
        turn.answer("done");
        observe_record(&mut app, job_finished(owner)).await;
        doing(&mut app, AgentActivity::Working);
        assert_eq!(status(&app, &child), State::Job(JobState::Completed));
    }

    /// A wait pauses the agent even with other owned work still running; any
    /// other running tool leaves it running tools.
    #[tokio::test]
    async fn wait_tool_status_tracks_classification_and_settlement() {
        use AgentDisplayState as State;
        for (tool, arguments, running) in [
            ("wait", json!({}), State::Waiting(WaitReason::Event)),
            (
                "exec",
                json!({"command": ["sleep", "60"]}),
                State::RunningTools,
            ),
        ] {
            let (_root, mut app, mut model, session) = played_fixture().await;
            let root = app.root_agent().clone();
            let _asked = ask(&session, "work");
            let background = json!({"command": ["sleep", "60"], "bg": true});
            model.turn().await.end(vec![
                call("background", "exec", background, 0),
                call("foreground", tool, arguments, 1),
            ]);
            let job = observe_find(&mut app, |app| job_of_call(&app.snapshot, "foreground")).await;
            reached(&mut app, job, JobTransition::Running).await;
            assert_eq!(status(&app, &root), running, "{tool}");
            // Created first, but it starts beside the foreground call.
            let background = job_of_call(&app.snapshot, "background").unwrap();
            reached(&mut app, background, JobTransition::Running).await;
            session.cancel_job(job).await.unwrap();
            observe_record(&mut app, job_finished(job)).await;
            // The root may already be requesting its next turn: its jobs alone decide.
            let idle = ObservedActivity {
                state: AgentActivity::Idle,
                since: 0,
            };
            app.snapshot.activity.insert(root.clone(), idle);
            assert_eq!(status(&app, &root), State::RunningTools, "{tool}");
            session.cancel_job(background).await.unwrap();
        }
    }

    /// A completed child stays briefly in the tree and retires once. Input to its
    /// finished owner job runs it again, as open work its owner waits on, and its
    /// next completion starts another grace.
    #[tokio::test]
    async fn completion_grace_retires_once_and_running_owner_reopens() {
        let (_root, mut app, mut model, session) = played_fixture().await;
        let root = app.root_agent().clone();
        let _asked = ask(&session, "delegate");
        let task = json!({"prompt": "work", "bg": true});
        model
            .turn()
            .await
            .end(vec![call("agent-0", "agent", task, 0)]);
        // The root goes on beside its background child, and notes its completion.
        let mut turns = [model.turn().await, model.turn().await];
        if turns[0].context != ContextId::from(&root) {
            turns.swap(0, 1);
        }
        let [root_turn, child_turn] = turns;
        root_turn.answer("waiting");
        let agent_completed = |event: &SessionEvent| matches!(event, SessionEvent::AgentCompleted);
        let settled = |answers| {
            let root = &root;
            move |app: &App| {
                count(&app.snapshot, answered) == answers
                    && activity(&app.snapshot, root) == Some(&AgentActivity::Idle)
            }
        };
        observe_until(&mut app, settled(1)).await;
        child_turn.answer("done");
        model.turn().await.answer("noted");
        observe_until(&mut app, settled(3)).await;
        assert!(recorded(&app.snapshot, agent_completed));
        let child = children(&app.snapshot).remove(0);
        let mut projection = Projection::new(&app.snapshot);
        let index = projection
            .agents
            .iter()
            .position(|agent| agent.id == child)
            .unwrap();
        assert!(projection.agents[index].terminal());
        assert!(!projection.has_active_children());
        let visible = |projection: &Projection, selected: &AgentId| {
            let mut visible = projection.visible(selected);
            visible.any(|visible| visible == index)
        };
        assert!(visible(&projection, &root));
        let initial = projection.agents[index].lifecycle;
        projection.complete_agent(&child);
        assert_eq!(projection.agents[index].lifecycle, initial);
        projection.agents[index].lifecycle = expired_grace();
        assert!(projection.retire_completed_grace());
        assert!(!projection.retire_completed_grace());
        assert!(!visible(&projection, &root));
        // A retired agent stays in the tree while a descendant is selected.
        assert!(visible(&projection, &child.child(2)));
        // Ordinary activity cannot reopen a completed agent.
        app.snapshot.activity.insert(
            child.clone(),
            ObservedActivity {
                state: AgentActivity::Working,
                since: 0,
            },
        );
        projection.rebuild(&app.snapshot);
        assert!(projection.agents[index].terminal());
        let owner = projection.agents[index].owner.unwrap();
        answer_child(&session, owner, "'more'").await;
        let running = job_state(owner, JobTransition::Running);
        observe_until(&mut app, |app| count(&app.snapshot, &running) == 2).await;
        let sent = observe_find(&mut app, |app| job_for(app, "script")).await;
        observe_record(&mut app, job_finished(sent)).await;
        (app.snapshot.jobs.values()).for_each(|job| projection.observe_job(job));
        projection.rebuild(&app.snapshot);
        assert!(!projection.agents[index].terminal());
        assert!(projection.has_active_children() && open_jobs(&app.snapshot));
        let waiting = projection.status(&agent(&projection, &root), &app.snapshot);
        assert_eq!(waiting, AgentDisplayState::Waiting(WaitReason::Child));

        next_turn(&mut model, &child).await.answer("done again");
        observe_until(&mut app, |app| count(&app.snapshot, agent_completed) == 2).await;
        (app.snapshot.jobs.values()).for_each(|job| projection.observe_job(job));
        projection.rebuild(&app.snapshot);
        assert!(projection.agents[index].terminal());
        assert!(visible(&projection, &root));
    }

    fn expired_grace() -> AgentLifecycle {
        AgentLifecycle::Terminal {
            grace_started: Instant::now().checked_sub(COMPLETED_AGENT_GRACE),
        }
    }
}
