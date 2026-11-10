//! Incremental journal indexes, agent lifecycle state, and usage totals.

use super::entries::Dep;
use super::state_name;
use crate::tui::format::Timing;
use serde_json::Value;
use skyhook::identity::AgentId;
use skyhook::job::JobRole;
use skyhook::job::{JobEnvelope, JobState, RunTiming};
use skyhook::session::{AgentActivity, AgentOutline, ObservationSnapshot, SessionEvent};
use skyhook::session::{AgentsPage, EventRecord, RecordSeq, ResidentLedger, Turns};
use skyhook::session::{StoppedReading, TurnFailure};
use skyhook_tools::target::TargetRef;
use skyhook_worker::job::JobId;
use std::collections::{HashMap, HashSet};
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

    /// What the agent's own activity shows, when that alone settles it: idle or tool
    /// work reads from its jobs instead.
    pub(super) fn of_activity(activity: &AgentActivity) -> Option<Self> {
        Some(match activity {
            AgentActivity::Working => Self::Working,
            AgentActivity::Reconnecting { attempt } => Self::Reconnecting { attempt: *attempt },
            AgentActivity::Compacting => Self::Compacting,
            AgentActivity::Stopped(failure) => Self::stopped(failure),
            AgentActivity::WaitingChildren => Self::Waiting(WaitReason::Child),
            AgentActivity::Idle | AgentActivity::Tools => return None,
        })
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
        /// When the owner job's run ended (epoch milliseconds), when known: another
        /// ending is another run, however many records folded it into one update.
        until: Option<i64>,
        grace_started: Option<Instant>,
    },
}

impl AgentLifecycle {
    /// Ended at `until` (epoch milliseconds), when known: discoverable for what is
    /// left of its grace, so an ending read long after it happened shows none.
    fn ended(until: Option<i64>) -> Self {
        let age = |until: i64| {
            let millis = chrono::Utc::now().timestamp_millis().saturating_sub(until);
            Duration::from_millis(u64::try_from(millis).unwrap_or_default())
        };
        let grace_started = (until.map(age))
            .filter(|age| *age < COMPLETED_AGENT_GRACE)
            .and_then(|age| Instant::now().checked_sub(age));
        Self::Terminal {
            until,
            grace_started,
        }
    }

    fn retire_grace(&mut self, now: Instant) -> bool {
        if let Self::Terminal { grace_started, .. } = self
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
        matches!(self, Self::Terminal { grace_started: Some(started), .. } if started.elapsed() < COMPLETED_AGENT_GRACE)
    }
}

#[derive(Clone)]
pub struct AgentInfo {
    pub id: AgentId,
    pub name: String,
    pub outline: AgentOutline,
    pub lifecycle: AgentLifecycle,
}
impl AgentInfo {
    /// `agent`, started with `outline`: named after the product when it is the root.
    fn started(agent: AgentId, outline: AgentOutline, host: &skyhook::identity::HostName) -> Self {
        Self {
            name: match agent.path().is_empty() {
                true => host.to_string(),
                false => agent.default_name(),
            },
            id: agent,
            outline,
            lifecycle: AgentLifecycle::Active,
        }
    }

    pub fn terminal(&self) -> bool {
        matches!(self.lifecycle, AgentLifecycle::Terminal { .. })
    }
}

#[derive(Clone)]
pub struct Projection {
    host: skyhook::identity::HostName,
    pub agents: Vec<AgentInfo>,
    /// The newest record the agents' summaries read: updates through it repeat them.
    through: RecordSeq,
    /// What records and job views folded since the last `take_changes` touched.
    changes: HashSet<Dep>,
    turns: Turns,
    /// Each agent's most recently finished turn.
    last_turn: HashMap<AgentId, Timing>,
    /// The jobs the session let go of, as it left them, while the agents or the view
    /// show them.
    released: HashMap<JobId, JobEnvelope>,
    /// Moves on with each start or mode change folded: what an agent runs under.
    pub contract: RecordSeq,
}
impl Projection {
    /// Nothing folded yet; `host` names the root agent once it starts.
    pub fn empty(host: &skyhook::identity::HostName) -> Self {
        Self {
            host: host.clone(),
            agents: Vec::new(),
            through: RecordSeq::default(),
            changes: HashSet::new(),
            turns: Turns::default(),
            last_turn: HashMap::new(),
            released: HashMap::new(),
            contract: RecordSeq::default(),
        }
    }

    /// What `summaries` say of every agent, with the owner jobs `owners` read after
    /// them, and the jobs `live` holds.
    pub fn new(
        summaries: AgentsPage,
        owners: Vec<JobEnvelope>,
        live: &ObservationSnapshot,
        host: &skyhook::identity::HostName,
    ) -> Self {
        let mut projection = Self {
            released: owners.into_iter().map(|job| (job.id, job)).collect(),
            ..Self::empty(host)
        };
        let mut turns = Vec::new();
        for summary in summaries.agents {
            let agent = AgentInfo::started(summary.agent.clone(), summary.outline, host);
            projection.agents.push(agent);
            if let Some((since, until)) = summary.finished {
                let finished = Timing::Took { since, until };
                (projection.last_turn).insert(summary.agent.clone(), finished);
            }
            turns.extend(summary.turn.map(|turn| (summary.agent, turn)));
        }
        projection.turns = turns.into_iter().collect();
        (projection.through, projection.contract) = (summaries.through, summaries.through);
        projection.reconcile(live);
        projection
    }

    /// A job as the session last showed it: as the live slice holds it, as the
    /// session let go of it, or as `window`'s pages read it.
    pub fn job<'a>(
        &'a self,
        live: &'a ObservationSnapshot,
        window: Option<&'a super::Window>,
        job: &JobId,
    ) -> Option<&'a JobEnvelope> {
        (live.jobs.get(job))
            .or_else(|| self.released.get(job))
            .or_else(|| window?.job(job))
    }

    /// Keep `jobs`, which the session let go of, as it left them. Returns whether
    /// there were any.
    pub fn release(&mut self, jobs: &[JobEnvelope]) -> bool {
        (self.released).extend(jobs.iter().map(|job| (job.id, job.clone())));
        !jobs.is_empty()
    }

    /// Let go of the jobs kept for being let go of that neither an agent nor `shown`
    /// shows.
    pub fn retain_released(&mut self, shown: &HashSet<JobId>) {
        let owners: HashSet<_> = (self.agents.iter())
            .filter_map(|agent| agent.outline.owner_job)
            .collect();
        (self.released).retain(|job, _| shown.contains(job) || owners.contains(job));
    }

    /// When `agent`'s open turn began.
    pub fn turn_started(&self, agent: &AgentId) -> Option<i64> {
        self.turns.started(agent)
    }

    /// Note a job's view as it arrives, before the record that moved it.
    pub fn observe_job(&mut self, job: JobId) {
        self.changes.insert(Dep::Job(job));
    }

    /// Fold the requests the session's stopped reading changed, which no record
    /// reports. Returns whether it changed anything: the projection then
    /// rebuilds as for a record, so agents reconcile with their owner jobs.
    pub fn observe_stopped(&mut self, reading: StoppedReading) -> bool {
        let changed = !(reading.jobs.is_empty() && reading.requests.is_empty());
        self.changes
            .extend(reading.requests.into_iter().map(Dep::Request));
        changed
    }

    /// Note the requests an update changed, which history shows.
    pub fn observe_requests(&mut self, requests: &[skyhook::session::RequestSeq]) {
        self.changes
            .extend(requests.iter().copied().map(Dep::Request));
    }

    pub(super) fn agent_name(&self, agent: &AgentId) -> &str {
        let info = self.agents.iter().find(|info| &info.id == agent);
        info.map_or("Agent", |info| info.name.as_str())
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
    pub fn take_changes(&mut self) -> HashSet<Dep> {
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
            _ => match agent
                .outline
                .owner_job
                .and_then(|job| self.job(snapshot, None, &job))
            {
                Some(job) => job.timing.into(),
                None => (self.last_turn.get(&agent.id).copied()).unwrap_or(Timing::Untimed),
            },
        }
    }

    /// Fold an update's record, after `snapshot` has; one a summary read is skipped.
    pub fn observe(&mut self, record: &EventRecord, snapshot: &ObservationSnapshot) {
        if record.sequence > self.through {
            self.fold(record, &snapshot.requests);
        }
    }

    fn fold(&mut self, record: &EventRecord, requests: &ResidentLedger) {
        if let Some(since) = self.turns.observe(record, requests) {
            let until = record.timestamp_millis;
            (self.last_turn).insert(record.agent.clone(), Timing::Took { since, until });
        }
        if let Some(outline) = AgentOutline::started(&record.event) {
            self.contract = record.sequence;
            // The owner job's card shows where its agent runs.
            self.changes.extend(outline.owner_job.map(Dep::Job));
            self.agents.retain(|agent| agent.id != record.agent);
            let agent = AgentInfo::started(record.agent.clone(), outline, &self.host);
            self.agents.push(agent);
        } else if let Some(agent) = self.agents.iter_mut().find(|a| a.id == record.agent) {
            agent.outline.apply(&record.event);
            if matches!(record.event, SessionEvent::ModeChanged { .. }) {
                self.contract = record.sequence;
            }
        }
    }

    /// Bring agents in line with their owner jobs: named after them, ended while they
    /// are, and running again once they run. A child ends with its owner job, never
    /// with what records happened to be seen; the root, which none owns, never ends.
    pub fn reconcile(&mut self, live: &ObservationSnapshot) {
        // The owner jobs read with the summaries are newer than a live slice that has
        // not yet applied what the summaries read; once it has, it is the newer.
        let caught_up = live.applied >= self.through;
        let mut agents = std::mem::take(&mut self.agents);
        for agent in &mut agents {
            let Some(id) = agent.outline.owner_job else {
                continue;
            };
            let (live, read) = (live.jobs.get(&id), self.released.get(&id));
            let job = if caught_up {
                live.or(read)
            } else {
                read.or(live)
            };
            let Some(job) = job else { continue };
            if let Some(name) = &job.name {
                agent.name = name.to_string();
            }
            let until = match job.timing {
                RunTiming::Took { until, .. } => Some(until),
                _ => None,
            };
            match (job.state.is_terminal(), agent.lifecycle) {
                (true, AgentLifecycle::Terminal { until: ended, .. }) if ended == until => {}
                (true, _) => agent.lifecycle = AgentLifecycle::ended(until),
                (false, AgentLifecycle::Terminal { .. }) => {
                    agent.lifecycle = AgentLifecycle::Active;
                }
                (false, AgentLifecycle::Active) => {}
            }
        }
        agents.sort_by(|a, b| a.id.path().cmp(b.id.path()));
        self.agents = agents;
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
                .map_or(TargetRef::Root, |agent| agent.outline.target.clone())),
        }
    }
    pub(super) fn job_target<'a>(&self, job: &'a JobEnvelope) -> Result<TargetRef, &'a str> {
        if job.role == JobRole::Agent {
            self.agents
                .iter()
                .find(|agent| agent.outline.owner_job == Some(job.id))
                .map_or_else(
                    || {
                        self.child_target(
                            &job.agent,
                            job.arguments.get(skyhook::tool::catalog::TARGET),
                        )
                    },
                    |agent| Ok(agent.outline.target.clone()),
                )
        } else {
            Ok(job.location.target.clone())
        }
    }
    /// Why `job` failed, as its agent's capabilities render it.
    pub(super) fn job_error(&self, job: &JobEnvelope) -> Option<String> {
        let agent = self.agents.iter().find(|agent| agent.id == job.agent);
        job.error_text(agent.map(|agent| &agent.outline.capabilities))
    }
    /// An agent's footer statistics.
    pub fn agent_stats(&self, snapshot: &ObservationSnapshot, agent: &AgentId) -> [String; 3] {
        let usage = snapshot.usage.get(agent).copied().unwrap_or_default();
        let context = snapshot.views.context.get(agent);
        super::super::format::footer_stats(usage, context.map(|c| (c.tokens, c.capacity)))
    }
    /// Indices of the agents a tree lists: the root, every unfinished agent and those in their
    /// completion grace, and the `viewed` agent with its ancestors when there is one.
    pub fn visible<'a>(&'a self, viewed: Option<&'a AgentId>) -> impl Iterator<Item = usize> + 'a {
        let shown = move |a: &AgentInfo| {
            a.id.path().is_empty()
                || !a.terminal()
                || viewed.is_some_and(|viewed| viewed.is_within(&a.id))
                || a.lifecycle.within_grace()
        };
        let agents = self.agents.iter().enumerate();
        agents.filter_map(move |(index, agent)| shown(agent).then_some(index))
    }
    pub fn status(&self, agent: &AgentInfo, snapshot: &ObservationSnapshot) -> AgentDisplayState {
        use AgentDisplayState as State;
        let owner = agent
            .outline
            .owner_job
            .and_then(|id| self.job(snapshot, None, &id));
        if agent.terminal() {
            return State::Job(owner.map_or(JobState::Completed, |job| job.state));
        }
        if let Some(owner) = owner
            && owner.state == JobState::WaitingInput
        {
            return State::Waiting(WaitReason::ParentInput);
        }
        let activity = (snapshot.activity.get(&agent.id)).map(|activity| &activity.state);
        match activity.and_then(State::of_activity) {
            Some(state) => state,
            None => {
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
    use crate::tests::bounded;
    use crate::tui::app::{
        App,
        played::{
            activity, answer_child, answered, ask, asking, at, attended_fixture, call, children,
            completed, count, delegate, exchange, job_finished, job_of_call, job_state, next_turn,
            observe_find, observe_record, observe_until, played_fixture, reached, recorded,
            requested, spawn_children,
        },
        tests::viewing,
    };
    use crate::tui::model::Tab;
    use serde_json::json;
    use skyhook::interaction::{ApprovalReply, Prompt, PromptKind, Reply};
    use skyhook::job::JobTransition;
    use skyhook::provider::protocol::{AssistantItem, ContextId};
    use skyhook::session::ObservedActivity;
    use tokio::sync::mpsc::UnboundedReceiver;

    fn agent(projection: &Projection, id: &AgentId) -> AgentInfo {
        let mut agents = projection.agents.iter();
        agents.find(|agent| &agent.id == id).unwrap().clone()
    }

    /// What `app` shows agent `id` doing, as a fresh projection of the agents'
    /// summaries, as a resubscription builds, agrees.
    async fn status(app: &App, id: &AgentId) -> AgentDisplayState {
        let state = (app.projection).status(&agent(&app.projection, id), &app.snapshot);
        let session = app.session().unwrap();
        let agents = bounded(session.store().agents()).await.unwrap();
        let owners = agents
            .agents
            .iter()
            .filter_map(|agent| agent.outline.owner_job);
        let owners = bounded(session.jobs().views(owners.collect()))
            .await
            .unwrap();
        let fresh = Projection::new(agents, owners, &app.snapshot, session.host());
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
                && recorded(&app.journal, completed)
        })
        .await;
        let since = at(&app.journal, requested, 0);
        let until = at(&app.journal, completed, 0);
        let took = Timing::Took { since, until };
        let shown = agent(&app.projection, &root);
        assert_eq!(app.projection.timing(&shown, &app.snapshot), took);
        // An app opening the session later reads it from the root's summary.
        let (_viewer, opened) = viewing(app.session().unwrap(), Tab::Conversation).await;
        let shown = agent(&opened.projection, &root);
        assert_eq!(opened.projection.timing(&shown, &opened.snapshot), took);
    }

    /// A child shows its turn while busy and its owner job's run otherwise, also
    /// once a crash before its completion record leaves it busy though finished.
    #[tokio::test]
    async fn a_child_shows_its_turn_while_busy_and_its_owner_jobs_run_otherwise() {
        let (_root, mut app, mut model, _session) = played_fixture().await;
        let (children, mut turns) = spawn_children(&mut app, &mut model, 1).await;
        turns.remove(0).answer("done");
        let owner = observe_find(&mut app, |app| job_of_call(&app.journal, "agent-0")).await;
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
            until: None,
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
        observe_until(&mut app, |app| app.snapshot.open_jobs()).await;
        session.close().await.unwrap();
        observe_until(&mut app, |app| app.snapshot.phase == SessionPhase::Closed).await;
        assert!(!app.peer(SlotKey::default(), true).working);
        let session = app
            .create(skyhook::product::Opening::Resume(session.id()))
            .await
            .unwrap()
            .session;
        attach(&mut app, session.clone()).await;
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
        observe_until(&mut app, |app| !children(&app.journal).is_empty()).await;
        let child = children(&app.journal).remove(0);
        let doing = |app: &mut App, state| {
            let activity = ObservedActivity { state, since: 0 };
            app.snapshot.activity.insert(child.clone(), activity);
        };
        doing(&mut app, AgentActivity::Reconnecting { attempt: 2 });
        assert_eq!(
            status(&app, &child).await,
            State::Reconnecting { attempt: 2 }
        );
        assert_eq!(status(&app, &root).await, State::Waiting(WaitReason::Child));

        let sleep = json!({"command": ["sleep", "60"]});
        turns.remove(0).end(vec![
            call("exec-0", "exec", sleep.clone(), 0),
            call("exec-1", "exec", sleep, 1),
        ]);
        let allow = approval(&mut prompts).await;
        let mut execs = Vec::new();
        for id in ["exec-0", "exec-1"] {
            let exec = observe_find(&mut app, |app| job_of_call(&app.journal, id)).await;
            reached(&mut app, exec, JobTransition::AwaitingApproval).await;
            execs.push(exec);
        }
        assert_eq!(
            status(&app, &child).await,
            State::Waiting(WaitReason::Permission)
        );
        // Allowed once, one call runs while the other still awaits approval.
        allow.send(Ok(ApprovalReply::Allow)).unwrap();
        let running = |app: &App| {
            let ran =
                |exec: &JobId| recorded(&app.journal, job_state(*exec, JobTransition::Running));
            execs.iter().any(ran)
        };
        observe_until(&mut app, running).await;
        assert_eq!(status(&app, &child).await, State::RunningTools);
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
        assert_eq!(
            status(&app, &child).await,
            State::Waiting(WaitReason::Event)
        );
        session.cancel_job(wait).await.unwrap();

        let turn = next_turn(&mut model, &child).await;
        turn.end(vec![asking()]);
        let owner = agent(&app.projection, &child).outline.owner_job.unwrap();
        reached(&mut app, owner, JobTransition::WaitingInput).await;
        // The root waits on with the question open: input over an event.
        model
            .turn()
            .await
            .end(vec![call("hold", "wait", json!({}), 0)]);
        let hold = observe_find(&mut app, |app| job_of_call(&app.journal, "hold")).await;
        reached(&mut app, hold, JobTransition::Running).await;
        doing(&mut app, AgentActivity::Working);
        assert_eq!(
            status(&app, &child).await,
            State::Waiting(WaitReason::ParentInput)
        );
        assert_eq!(status(&app, &root).await, State::Waiting(WaitReason::Input));

        answer_child(&session, owner, "{pick: 'this'}").await;
        let turn = next_turn(&mut model, &child).await;
        turn.answer("progress");
        let waiting =
            |app: &App| activity(&app.snapshot, &child) == Some(&AgentActivity::WaitingChildren);
        observe_until(&mut app, waiting).await;
        assert_eq!(
            status(&app, &child).await,
            State::Waiting(WaitReason::Child)
        );

        session.cancel_job(work).await.unwrap();
        let turn = next_turn(&mut model, &child).await;
        turn.answer("done");
        observe_record(&mut app, job_finished(owner)).await;
        doing(&mut app, AgentActivity::Working);
        assert_eq!(status(&app, &child).await, State::Job(JobState::Completed));
    }

    /// A wait pauses the agent even with other owned work still running; any
    /// other running tool leaves it running tools.
    #[tokio::test]
    async fn wait_tool_status_tracks_classification_and_settlement() {
        use AgentDisplayState as State;
        let (_root, mut app, mut model, session) = played_fixture().await;
        let root = app.root_agent().clone();
        let _asked = ask(&session, "work");
        let (sleep, background) = (
            json!({"command": ["sleep", "60"]}),
            json!({"command": ["sleep", "60"], "bg": true}),
        );
        model.turn().await.end(vec![
            call("background", "exec", background, 0),
            call("wait", "wait", json!({}), 1),
            call("foreground", "exec", sleep, 2),
        ]);
        let mut jobs = vec![];
        for name in ["wait", "foreground", "background"] {
            let job = observe_find(&mut app, |app| job_of_call(&app.journal, name)).await;
            reached(&mut app, job, JobTransition::Running).await;
            jobs.push(job);
        }
        let [wait, foreground, background] = jobs[..] else {
            unreachable!()
        };
        assert_eq!(status(&app, &root).await, State::Waiting(WaitReason::Event));
        session.cancel_job(wait).await.unwrap();
        observe_record(&mut app, job_finished(wait)).await;
        assert_eq!(status(&app, &root).await, State::RunningTools);
        session.cancel_job(foreground).await.unwrap();
        observe_record(&mut app, job_finished(foreground)).await;
        // The root may already be requesting its next turn: its jobs alone decide.
        let idle = ObservedActivity {
            state: AgentActivity::Idle,
            since: 0,
        };
        app.snapshot.activity.insert(root.clone(), idle);
        assert_eq!(status(&app, &root).await, State::RunningTools);
        session.cancel_job(background).await.unwrap();
    }

    /// A completed child stays briefly in the tree and retires once. Input to its
    /// finished owner job runs it again, as open work its owner waits on, also as a
    /// fresh projection shows it, and its next completion starts another grace.
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
                count(&app.journal, answered) == answers
                    && activity(&app.snapshot, root) == Some(&AgentActivity::Idle)
            }
        };
        observe_until(&mut app, settled(1)).await;
        child_turn.answer("done");
        model.turn().await.answer("noted");
        observe_until(&mut app, settled(3)).await;
        assert!(recorded(&app.journal, agent_completed));
        let child = children(&app.journal).remove(0);
        let index = |app: &App| {
            let mut agents = app.projection.agents.iter();
            agents.position(|agent| agent.id == child).unwrap()
        };
        assert!(app.projection.agents[index(&app)].terminal());
        assert!(!app.projection.has_active_children());
        let visible = |projection: &Projection, at: usize, selected: &AgentId| {
            let mut visible = projection.visible(Some(selected));
            visible.any(|visible| visible == at)
        };
        let at = index(&app);
        assert!(visible(&app.projection, at, &root));
        app.projection.agents[at].lifecycle = expired_grace();
        assert!(app.projection.retire_completed_grace());
        assert!(!app.projection.retire_completed_grace());
        assert!(!visible(&app.projection, at, &root));
        // A retired agent stays in the tree while a descendant is selected.
        assert!(visible(&app.projection, at, &child.child(2)));
        let owner = app.projection.agents[index(&app)]
            .outline
            .owner_job
            .unwrap();
        // A whole later run folded into one update, never seen running, still shows its
        // own ending.
        let mut burst = app.snapshot.clone();
        if let Some(RunTiming::Took { until, .. }) =
            burst.jobs.get_mut(&owner).map(|job| &mut job.timing)
        {
            *until = chrono::Utc::now().timestamp_millis();
        }
        let at = index(&app);
        app.projection.agents[at].lifecycle = expired_grace();
        app.projection.reconcile(&burst);
        assert!(visible(&app.projection, index(&app), &root));
        let subscribed = app.snapshot.clone();
        answer_child(&session, owner, "'more'").await;
        let running = job_state(owner, JobTransition::Running);
        observe_until(&mut app, |app| count(&app.journal, &running) == 2).await;
        // Subscribed before it ran again and summarised after: the updates since the
        // subscription leave it running.
        let agents = bounded(session.store().agents()).await.unwrap();
        let owners = bounded(session.jobs().views(vec![owner])).await.unwrap();
        let mut fresh = Projection::new(agents, owners, &subscribed, session.host());
        fresh.reconcile(&subscribed);
        let since = app.journal.iter();
        for record in since.filter(|record| record.sequence > subscribed.applied) {
            fresh.observe(record, &app.snapshot);
        }
        fresh.reconcile(&app.snapshot);
        assert!(!agent(&fresh, &child).terminal());
        let sent = observe_find(&mut app, |app| job_for(app, "script")).await;
        observe_record(&mut app, job_finished(sent)).await;
        assert!(!app.projection.agents[index(&app)].terminal());
        // As an app opening the session now shows it.
        status(&app, &child).await;
        assert!(app.projection.has_active_children() && app.snapshot.open_jobs());
        let waiting = app
            .projection
            .status(&agent(&app.projection, &root), &app.snapshot);
        assert_eq!(waiting, AgentDisplayState::Waiting(WaitReason::Child));

        next_turn(&mut model, &child).await.answer("done again");
        // The row ends with its owner job, which ends after the agent's completion.
        observe_until(&mut app, |app| {
            count(&app.journal, job_finished(owner)) == 2
        })
        .await;
        assert!(app.projection.agents[index(&app)].terminal());
        assert!(visible(&app.projection, index(&app), &root));
    }

    fn expired_grace() -> AgentLifecycle {
        AgentLifecycle::Terminal {
            until: None,
            grace_started: Instant::now().checked_sub(COMPLETED_AGENT_GRACE),
        }
    }
}
