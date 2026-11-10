//! What the core keeps of each running agent: its turn, inbox and wake window. A
//! parked child holds nothing.
use super::inbox::Window;
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    ops::Bound,
};

use super::{
    AgentCommand, HarnessError, TurnFailure,
    queue::{self, QueuedInput},
};
use crate::identity::AgentId;
use crate::session::{
    AgentActivity, CoreState, Dirty, ModelPurpose, ObservedActivity, RequestPhase,
    agent_state::Last,
};

/// The agents whose loops run, and whether they take deliveries. Every change
/// to a running agent's facts marks it for the waits that read it.
#[derive(Default)]
pub(crate) struct Agents {
    running: BTreeMap<AgentId, AgentFacts>,
    flow: Flow,
    activation: Activation,
    resuming: super::resume::Waiting,
    /// The asks each issuer is batching.
    questions: super::questions::Batches,
    /// How many loops of each child have ended.
    ended: HashMap<AgentId, u64>,
    dirty: Dirty,
}

/// Whether agents take their deliveries. An interrupt that stops anything pauses
/// the session until `continue` or new input opens it: what jobs deliver
/// meanwhile stays pending, and the next request carries it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Flow {
    #[default]
    Open,
    Paused,
}

/// Whether a viewed session's root is being prepared to run: a first call that
/// finds it so joins that activation rather than preparing the root again.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Activation {
    #[default]
    Idle,
    Preparing,
}

impl std::ops::Deref for Agents {
    type Target = BTreeMap<AgentId, AgentFacts>;
    fn deref(&self) -> &Self::Target {
        &self.running
    }
}

pub(crate) struct AgentFacts {
    /// Changed only by `set_turn`, which keeps `since`.
    pub(super) turn: Turn,
    /// When the turn last began or stopped being busy (epoch milliseconds).
    pub(super) since: i64,
    /// Commands in arrival order: senders never wait for the agent.
    pub(super) inbox: VecDeque<AgentCommand>,
    pub(super) window: Window,
    pub(super) invocation: Invocation,
    /// The token of the work it last took, which an interrupt cancels.
    pub(super) turn_token: tokio_util::sync::CancellationToken,
    /// The refused batch its parked failed turn holds for `continue`. Only
    /// [`park_holding`](Self::park_holding) sets it, and any change of turn
    /// hands it back unless `continue` took it first.
    pub(super) held: Option<queue::Batch>,
}

impl AgentFacts {
    /// Move to `turn`, returning the turn it left: entering or leaving a busy
    /// turn starts its time.
    pub(super) fn set_turn(&mut self, turn: Turn) -> Turn {
        if let Some(held) = self.held.take() {
            held.hand_back();
        }
        if turn.busy() != self.turn.busy() {
            self.since = chrono::Utc::now().timestamp_millis();
        }
        std::mem::replace(&mut self.turn, turn)
    }

    /// Stop the work it took, as an interrupt does: `Stopping` until its loop
    /// unwinds that work, or interrupted at once while that work is a
    /// `continue` claim its loop has yet to take, as only a claim queues one.
    pub(super) fn stop(&mut self) {
        let claimed =
            (self.inbox.iter()).any(|command| matches!(command, AgentCommand::Continue { .. }));
        self.set_turn(if claimed { INTERRUPTED } else { Turn::Stopping });
        self.turn_token.cancel();
    }

    /// Park failed, holding the batch whose refusal failed the turn.
    pub(super) fn park_holding(&mut self, failure: crate::session::Failure, held: queue::Batch) {
        self.set_turn(Turn::Parked(TurnFailure::Failed(failure)));
        self.held = Some(held);
    }

    /// Whether input queued for a request boundary waits in the inbox.
    pub(super) fn queued(&self) -> bool {
        (self.inbox.iter()).any(|command| matches!(command, AgentCommand::QueuedInputs(_)))
    }
}

/// The turn an interrupt stopped.
pub(super) const INTERRUPTED: Turn = Turn::Parked(TurnFailure::Interrupted);

/// Where an agent's loop is, as interrupt and continue read it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Turn {
    Idle,
    /// From taking work that begins a turn until that turn ends, or commits its answer with
    /// nothing left to carry. Only a busy turn requests, commits a response or
    /// makes calls.
    Busy,
    /// An interrupted turn still waiting on the foreground child invocations it
    /// holds. `continue` makes it busy again, as does cancelling a job it holds;
    /// a wait that ends otherwise parks it interrupted.
    Held,
    /// A turn an interrupt stopped whose loop has yet to unwind the work it
    /// took. It reads interrupted, but nothing claims it, or what it would
    /// hold, until its loop parks it so.
    Stopping,
    /// The last turn ended without an answer: only input or `continue` resumes
    /// it, and deliveries wait for that request. A turn whose tree its owner
    /// cancelled is parked `Cancelled` until it ends as it unwinds.
    Parked(TurnFailure),
    /// An idle or held turn whose tree was cancelled, as closing cancels every
    /// tree: it reads as it was and takes no work, so nothing makes it busy
    /// again. A held one ends interrupted once its wait does.
    Cancelled(Was),
}

/// What a turn was when its tree was cancelled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Was {
    Idle,
    Held,
}

impl Turn {
    fn busy(&self) -> bool {
        matches!(self, Self::Busy | Self::Held | Self::Cancelled(Was::Held))
    }

    /// Whether an interrupt stopped it, unwound or not.
    pub(super) fn interrupted(&self) -> bool {
        matches!(
            self,
            Self::Stopping | Self::Parked(TurnFailure::Interrupted)
        )
    }
}

/// `agent`'s loop is gone, which its owner's parking reads, and with a child's loop
/// its contract.
pub(super) fn end_loop(state: &mut CoreState, agent: &AgentId) -> Option<AgentFacts> {
    state.contracts.release(agent);
    state.agents.remove(agent)
}

/// Each agent's activity as the session's state reads: see [`activity`].
pub(crate) fn activities(state: &CoreState) -> HashMap<AgentId, ObservedActivity> {
    let activity = |agent: &AgentId| Some((agent.clone(), activity(state, agent)?));
    observed_agents(state).filter_map(activity).collect()
}

/// The agents with an activity: those that run, and the root, as its journal
/// leaves its last turn.
pub(crate) fn observed_agents(state: &CoreState) -> impl Iterator<Item = &AgentId> {
    let running = &state.agents.running;
    let held = (state.contracts.agents()).filter(|agent| !running.contains_key(*agent));
    running.keys().chain(held)
}

/// `agent`'s activity as the session's state reads: a running agent's from its
/// turn, its open request and its jobs; the root's, before it runs, as its
/// journal leaves it, since its last turn. A parked child has none. A session
/// that no longer runs reads a busy one as interrupted at its last activity.
pub(crate) fn activity(state: &CoreState, agent: &AgentId) -> Option<ObservedActivity> {
    let agents = &state.agents;
    let Some(facts) = agents.running.get(agent) else {
        state.contracts.get(agent)?;
        let since = state.agent_states.get(agent)?.last.as_ref()?.millis;
        let state = (state.stopped_turn(agent)).map_or(AgentActivity::Idle, AgentActivity::Stopped);
        return Some(ObservedActivity { state, since });
    };
    let stopped = state.phase().stopped_at(state.last_activity());
    Some(match (running(state, agent, facts), stopped) {
        (state, Some(since)) if state.is_busy() => ObservedActivity {
            state: interrupted(),
            since,
        },
        (state, _) => ObservedActivity {
            state,
            since: facts.since,
        },
    })
}

fn interrupted() -> AgentActivity {
    AgentActivity::Stopped(TurnFailure::Interrupted)
}

/// Whether `agent`'s parked turn leaves `continue` something to try again: the
/// journal reads it short of an answer, a delivery waits, or a batch it held is
/// still held. One its owner cancelled always does.
pub(super) fn continues(state: &CoreState, agent: &AgentId) -> bool {
    let Some(facts) = state.agents.get(agent) else {
        return false;
    };
    match facts.turn {
        Turn::Parked(TurnFailure::Cancelled) => true,
        Turn::Parked(_) => {
            facts.held.as_ref().is_some_and(queue::Batch::retains)
                || (state.stopped_turn(agent))
                    .or_else(|| held_deliveries(state, agent))
                    .is_some()
        }
        _ => false,
    }
}

/// Rest `agent`'s parked turn once nothing is left for `continue` to try again,
/// dropping what of a batch it held was withdrawn. Whether it stays parked.
pub(super) fn settle_held(state: &mut CoreState, agent: &AgentId) -> bool {
    let continues = continues(state, agent);
    let Some(facts) = state.agents.get_mut(agent) else {
        return false;
    };
    if !facts.held.as_mut().is_some_and(queue::Batch::holds) {
        facts.held = None;
    }
    if !continues && matches!(facts.turn, Turn::Parked(_)) {
        facts.set_turn(Turn::Idle);
    }
    continues
}

/// Why `agent`, with no turn to continue, rests parked on the deliveries
/// pending for it: the failure last reported since its turn, or an interruption.
pub(crate) fn held_deliveries(state: &CoreState, agent: &AgentId) -> Option<TurnFailure> {
    let last = state
        .agent_states
        .get(agent)
        .and_then(|held| held.last.as_ref());
    let held = match last.map(|last| &last.value) {
        Some(Last::Refused(failure)) => TurnFailure::Failed(failure.clone()),
        _ => TurnFailure::Interrupted,
    };
    state.jobs.has_pending(agent).then_some(held)
}

/// A running agent's activity: an answered child still waits on what it runs.
fn running(state: &CoreState, agent: &AgentId, facts: &AgentFacts) -> AgentActivity {
    match &facts.turn {
        Turn::Parked(failure) => AgentActivity::Stopped(failure.clone()),
        Turn::Stopping => interrupted(),
        Turn::Held | Turn::Cancelled(Was::Held) => AgentActivity::WaitingChildren,
        Turn::Idle | Turn::Cancelled(Was::Idle)
            if agent.parent().is_some() && state.jobs.has_live(agent) =>
        {
            AgentActivity::WaitingChildren
        }
        Turn::Idle | Turn::Cancelled(Was::Idle) => AgentActivity::Idle,
        Turn::Busy => match (state.requests.open(agent)).and_then(|r| state.requests.get(r)) {
            Some(request) => match (&request.phase, request.purpose) {
                (RequestPhase::Retrying { attempt, .. }, _) => AgentActivity::Reconnecting {
                    attempt: attempt.saturating_add(1),
                },
                (_, ModelPurpose::Compaction) => AgentActivity::Compacting,
                _ => AgentActivity::Working,
            },
            None => {
                let work = state.jobs.live_work(agent);
                if work.children && work.blocking.is_empty() {
                    AgentActivity::WaitingChildren
                } else if state.jobs.has_live(agent) {
                    AgentActivity::Tools
                } else {
                    AgentActivity::Working
                }
            }
        },
    }
}

/// Where a child's loop is, as work that needs it reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ChildLoop {
    Running,
    /// Let go while it had nothing to do: the job it answers through, which can
    /// still run it, runs it again, and how many of its loops have ended, which
    /// what a launch prepared for it reads.
    Parked(skyhook_worker::job::JobId, u64),
    /// The job it answers through can no longer run it.
    Stopped,
}

/// Where child `agent`'s loop is.
pub(super) fn child_loop(state: &CoreState, agent: &AgentId) -> ChildLoop {
    if state.agents.running.contains_key(agent) {
        return ChildLoop::Running;
    }
    let ended = state.agents.ended(agent);
    (state.jobs.resumes(agent)).map_or(ChildLoop::Stopped, |job| ChildLoop::Parked(job, ended))
}

/// Whether child `agent`'s loop has nothing to do until its job runs it again,
/// so it parks: its invocation resolved, its turn idle or failed with nothing in
/// its inbox, window or held batch, and it neither owns work nor runs a child,
/// either of which would deliver to it. Its journal tells a failed one apart
/// when it starts again.
pub(super) fn quiescent(state: &CoreState, agent: &AgentId) -> bool {
    let agents = &state.agents;
    let Some(facts) = agents.running.get(agent) else {
        return false;
    };
    let idle = facts.invocation == Invocation::Resolved
        && matches!(
            facts.turn,
            Turn::Idle | Turn::Parked(TurnFailure::Failed(_))
        )
        && facts.inbox.is_empty()
        && facts.held.is_none()
        && facts.window.settled();
    idle && !state.jobs.owns_work(agent) && !agents.runs_within(agent)
}

/// Where owner input to a child goes: into its open invocation, or, once that
/// resolved, into a new one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Invocation {
    Open,
    Resolved,
}

impl Agents {
    /// `agent`'s running facts, to change: the agent is marked for the waits
    /// that read it.
    pub(crate) fn get_mut(&mut self, agent: &AgentId) -> Option<&mut AgentFacts> {
        let facts = self.running.get_mut(agent)?;
        self.dirty.agent(agent);
        Some(facts)
    }

    /// Run `agent` with `facts`: only the change that finds it without a loop
    /// starts one.
    pub(super) fn insert(&mut self, agent: AgentId, facts: AgentFacts) {
        self.dirty.agent(&agent);
        self.running.insert(agent, facts);
    }

    /// `agent`'s loop is gone, which its owner's parking reads.
    fn remove(&mut self, agent: &AgentId) -> Option<AgentFacts> {
        self.dirty.agent(agent);
        if let Some(owner) = agent.parent() {
            self.dirty.agent(&owner);
            *self.ended.entry(agent.clone()).or_default() += 1;
        }
        self.running.remove(agent)
    }

    /// Whether a descendant of `agent` runs its loop.
    fn runs_within(&self, agent: &AgentId) -> bool {
        let after = (Bound::Excluded(agent), Bound::Unbounded);
        let next = self.running.range::<AgentId, _>(after).next();
        next.is_some_and(|(next, _)| next.is_within(agent))
    }

    pub(super) fn flow(&self) -> Flow {
        self.flow
    }

    /// Let agents take their deliveries, or not: every wait decides again.
    pub(super) fn set_flow(&mut self, flow: Flow) {
        if self.flow != flow {
            self.flow = flow;
            self.dirty.session();
        }
    }

    pub(super) fn activation(&self) -> Activation {
        self.activation
    }

    pub(super) fn set_activation(&mut self, activation: Activation) {
        if self.activation != activation {
            self.activation = activation;
            self.dirty.session();
        }
    }

    /// Drop every question batch: its asks see it stopped.
    pub(crate) fn drop_batches(&mut self) {
        self.questions = Default::default();
    }

    pub(super) fn batches_mut(&mut self) -> &mut super::questions::Batches {
        &mut self.questions
    }

    pub(super) fn resuming(&self) -> &super::resume::Waiting {
        &self.resuming
    }

    pub(super) fn resuming_mut(&mut self) -> &mut super::resume::Waiting {
        &mut self.resuming
    }

    /// Withdraw every call waiting on the resume hook, as an interrupt does, and
    /// count them: each decides again.
    pub(super) fn withdraw_resumes(&mut self) -> usize {
        let withdrawn = self.resuming.withdraw();
        if withdrawn > 0 {
            self.dirty.session();
        }
        withdrawn
    }

    /// What every change since the last take touched.
    pub(crate) fn take_dirty(&mut self) -> Dirty {
        std::mem::take(&mut self.dirty)
    }

    /// How many of `agent`'s loops have ended.
    pub(super) fn ended(&self, agent: &AgentId) -> u64 {
        self.ended.get(agent).copied().unwrap_or_default()
    }

    /// Schedule `agent`'s window for work to collect.
    pub(crate) fn wake(&mut self, agent: &AgentId) {
        if let Some(facts) = self.get_mut(agent) {
            facts.window.schedule(false);
        }
    }

    /// Queue `command` for `agent`; false once its loop is gone, when the command's
    /// waiters see it stopped.
    pub(super) fn send(&mut self, agent: &AgentId, command: AgentCommand) -> bool {
        let Some(facts) = self.get_mut(agent) else {
            return false;
        };
        // Input reaching a failed turn that holds a batch waits with it for
        // `continue`, rather than resuming the turn.
        let command = match command {
            AgentCommand::QueuedInputs(inputs) if let Some(held) = &mut facts.held => {
                held.join(inputs);
                return true;
            }
            command => command,
        };
        facts.window.schedule(true);
        facts.inbox.push_back(command);
        true
    }

    /// Begin `agent`'s invocation, opened in this change with its prompt
    /// committed, answering `done`.
    pub(super) fn begin(&mut self, agent: &AgentId, done: super::ChildCompletion) {
        let facts = self.get_mut(agent).expect("the change runs its loop");
        facts.inbox.push_back(AgentCommand::Begin(done));
    }

    /// Take `agent`'s queued inputs, leaving every other command in order.
    pub(super) fn take_queued(&mut self, agent: &AgentId) -> Vec<QueuedInput> {
        let Some(facts) = self.get_mut(agent) else {
            return Vec::new();
        };
        let mut queued = Vec::new();
        for command in std::mem::take(&mut facts.inbox) {
            match command {
                AgentCommand::QueuedInputs(inputs) => queued.extend(inputs),
                kept => facts.inbox.push_back(kept),
            }
        }
        queued
    }

    /// Whether `agent` may do its turn's model work: request, commit a response
    /// or make calls. A turn an interrupt stopped or held may not, nor one whose
    /// tree was cancelled, nor one that already answered.
    pub(crate) fn busy(&self, agent: &AgentId) -> bool {
        (self.running.get(agent)).is_none_or(|facts| facts.turn == Turn::Busy)
    }

    /// `agent` committed a response without calls. Its turn does no more model
    /// work, so an interrupt finds it between turns.
    pub(crate) fn answered(&mut self, agent: &AgentId) {
        if let Some(facts) = self.get_mut(agent)
            && facts.turn == Turn::Busy
        {
            facts.set_turn(Turn::Idle);
        }
    }

    /// Whether `agent`'s answered turn goes on at the boundary after its answer,
    /// busy again, for the work an agent between turns would take: input queued
    /// for it, or `deliveries` it takes. One whose tree was cancelled ends
    /// instead.
    pub(super) fn rejoin(&mut self, agent: &AgentId, deliveries: bool) -> bool {
        let Some(facts) = self.get_mut(agent) else {
            return false;
        };
        let answered = matches!(facts.turn, Turn::Idle | Turn::Busy);
        let rejoins = answered && (facts.queued() || deliveries);
        if rejoins {
            facts.set_turn(Turn::Busy);
        }
        rejoins
    }

    /// Whether an interrupt stopped `agent`'s turn.
    pub(crate) fn stopped(&self, agent: &AgentId) -> bool {
        (self.running.get(agent)).is_some_and(|facts| facts.turn.interrupted())
    }

    /// Hand back the input `agent` has yet to take, as interrupted, apart from
    /// the work that begins its invocation yet to begin: that runs as the
    /// invocation's first turn. Whether it handed any back.
    pub(crate) fn hand_back(&mut self, agent: &AgentId, jobs: &crate::job::Jobs) -> bool {
        let Some(facts) = self.get_mut(agent) else {
            return false;
        };
        let begins = jobs.unbegun(agent).is_some()
            && matches!(facts.inbox.front(), Some(AgentCommand::Begin(_)));
        let handed: Vec<_> = facts.inbox.drain(usize::from(begins)..).collect();
        let untaken = !handed.is_empty();
        for command in handed {
            if let Some(batch) = command.refuse(|| HarnessError::Interrupted) {
                self.hold_again(agent, batch);
            }
        }
        untaken
    }

    /// Settle `agent`'s busy turn at `turn`, unless an interrupt stopped it
    /// first; whether it did.
    pub(super) fn rest(&mut self, agent: &AgentId, turn: Turn) -> bool {
        let busy = self.get_mut(agent).filter(|facts| facts.turn == Turn::Busy);
        busy.map(|facts| facts.set_turn(turn)).is_some()
    }

    /// End `agent`'s work that ran no turn: busy, it rests at `turn`; stopped,
    /// its loop has unwound, and it parks interrupted.
    pub(super) fn unwind(&mut self, agent: &AgentId, turn: Turn) {
        let Some(facts) = self.get_mut(agent) else {
            return;
        };
        match facts.turn {
            Turn::Busy => drop(facts.set_turn(turn)),
            Turn::Stopping => drop(facts.set_turn(INTERRUPTED)),
            _ => {}
        }
    }

    /// Hold `batch`, as it was claimed, again for `agent`'s failed turn, with
    /// the input queued since; a turn that no longer waits hands it back.
    pub(super) fn hold_again(&mut self, agent: &AgentId, batch: queue::Admitted) {
        let held = batch.release(self.take_queued(agent));
        match self.get_mut(agent) {
            Some(facts) if matches!(facts.turn, Turn::Parked(_)) => facts.held = held,
            _ => held.into_iter().for_each(queue::Batch::hand_back),
        }
    }

    /// A job `agent` held was cancelled on request: its held turn runs on.
    pub(crate) fn run_on(&mut self, agent: &AgentId) {
        if let Some(facts) = self.get_mut(agent)
            && facts.turn == Turn::Held
        {
            facts.set_turn(Turn::Busy);
        }
    }

    /// The agents whose turn is `turn`.
    pub(super) fn turned(&self, turn: &Turn) -> Vec<AgentId> {
        let agents = self.running.iter().filter(|(_, facts)| &facts.turn == turn);
        agents.map(|(agent, _)| agent.clone()).collect()
    }

    /// Stop every agent, refusing what each had yet to take with `error`.
    pub(crate) fn stop(&mut self, error: impl Fn() -> HarnessError) {
        self.drop_batches();
        for (agent, facts) in std::mem::take(&mut self.running) {
            self.dirty.agent(&agent);
            let claimed = facts
                .inbox
                .into_iter()
                .filter_map(|command| command.refuse(&error));
            claimed.for_each(queue::Admitted::hand_back);
        }
    }

    /// Cancel the work each of `agents` last took for good, so nothing more its
    /// turn asks is admitted: a busy turn is parked `Cancelled`, and an idle or
    /// held one is never made busy again.
    pub(super) fn cancel_turns(&mut self, agents: &[AgentId]) {
        for agent in agents {
            let Some(facts) = self.get_mut(agent) else {
                continue;
            };
            let cancelled = match facts.turn {
                Turn::Busy => Some(Turn::Parked(TurnFailure::Cancelled)),
                Turn::Idle => Some(Turn::Cancelled(Was::Idle)),
                Turn::Held => Some(Turn::Cancelled(Was::Held)),
                Turn::Stopping | Turn::Parked(_) | Turn::Cancelled(_) => None,
            };
            if let Some(turn) = cancelled {
                facts.set_turn(turn);
            }
            facts.turn_token.cancel();
        }
    }

    /// The agents within `root`.
    pub(super) fn within(&self, root: &AgentId) -> Vec<AgentId> {
        let agents = self.running.keys().filter(|agent| agent.is_within(root));
        agents.cloned().collect()
    }
}
