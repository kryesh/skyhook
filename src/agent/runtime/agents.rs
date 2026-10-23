//! What the core keeps of each running agent: its turn, inbox and wake window,
//! and what its journal leaves of it. A parked child holds nothing; its journal
//! is read again as its loop starts.
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    ops::Bound,
};

use super::{
    AgentCommand, HarnessError, TurnFailure,
    queue::{self, QueuedInput},
};
use crate::{
    agent::{AgentActivity, ObservedActivity},
    execution::ExecutionLocation,
    identity::AgentId,
    session::{
        CoreState, Dirty, EventRecord, ModelContext, ModelPurpose, ProfileSnapshot, RecordSeq,
        RequestPhase, SessionEvent, agent_state::Last,
    },
    tool::policy::{Mode, ModeName},
};

/// The agents whose loops run, and whether they take deliveries. Every change
/// to a running agent's facts marks it for the waits that read it.
#[derive(Default)]
pub(crate) struct Agents {
    running: BTreeMap<AgentId, AgentFacts>,
    flow: Flow,
    activation: Activation,
    /// What the journal leaves of the root, and of each child while its loop
    /// runs: only those facts' changes.
    journaled: HashMap<AgentId, Journaled>,
    /// Each mode's latest journaled edition.
    editions: HashMap<ModeName, std::sync::Arc<Mode>>,
    /// How many loops of each child have ended.
    ended: HashMap<AgentId, u64>,
    dirty: Dirty,
}

/// What an agent's journal leaves of its contract that its loop, its owner and
/// the host read.
#[derive(Clone)]
pub(super) struct Journaled {
    pub(super) contract: RecordedContract,
    /// The highest child index it has started or reserved. A launch the
    /// session refuses leaves its index unused.
    children: u32,
}

/// The contract an agent runs under. A journaled one is its start, latest applied
/// profile, and its latest agent model context; its grants are its state's.
#[derive(Clone)]
pub(super) struct RecordedContract {
    pub(super) profile: ProfileSnapshot,
    pub(super) mode: Option<AppliedMode>,
    pub(super) location: ExecutionLocation,
    /// The latest agent model context and its record, while the mode is unchanged:
    /// a mode pins its prompt and tools with a context of its own.
    pub(super) context: Option<(RecordSeq, ModelContext)>,
}

/// A mode as an agent runs it: the edition it was applied under, which the
/// configuration may since have changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct AppliedMode {
    pub(super) name: ModeName,
    pub(super) definition: std::sync::Arc<Mode>,
}

impl AppliedMode {
    /// The configured `definition` of `name`, as an agent entering it now runs it.
    pub(super) fn configured(name: &ModeName, definition: &Mode) -> Self {
        Self {
            name: name.clone(),
            definition: definition.clone().into(),
        }
    }

    /// The journal form of applying it, which records its definition only when
    /// that is a new edition.
    pub(super) fn selection(&self) -> crate::session::ModeSelection {
        crate::session::ModeSelection {
            name: self.name.clone(),
            definition: Some(Mode::clone(&self.definition)),
        }
    }
}

/// The edition `selection` applies: the one it records, or its name's latest.
fn applied(
    editions: &mut HashMap<ModeName, std::sync::Arc<Mode>>,
    selection: &crate::session::ModeSelection,
) -> Option<AppliedMode> {
    if let Some(definition) = &selection.definition {
        editions.insert(selection.name.clone(), definition.clone().into());
    }
    Some(AppliedMode {
        name: selection.name.clone(),
        definition: editions.get(&selection.name)?.clone(),
    })
}

/// Fold `record` into what `journaled` holds of its agent: its start begins that
/// where it `begins`, and a child's start counts in its parent's children. Every
/// record keeps the mode editions it records.
fn fold(
    editions: &mut HashMap<ModeName, std::sync::Arc<Mode>>,
    journaled: &mut HashMap<AgentId, Journaled>,
    begins: bool,
    record: &EventRecord,
) {
    let agent = &record.agent;
    let mode = match &record.event {
        SessionEvent::AgentStarted {
            mode: Some(mode), ..
        }
        | SessionEvent::ModeChanged { mode, .. } => applied(editions, mode),
        _ => None,
    };
    if let SessionEvent::AgentStarted {
        profile, location, ..
    } = &record.event
    {
        if let (Some(parent), Some(index)) = (agent.parent(), agent.path().last())
            && let Some(parent) = journaled.get_mut(&parent)
        {
            parent.children = parent.children.max(*index);
        }
        let contract = RecordedContract {
            profile: profile.clone(),
            mode,
            location: location.clone(),
            context: None,
        };
        if begins {
            let started = Journaled {
                contract,
                children: 0,
            };
            journaled.insert(agent.clone(), started);
        }
        return;
    }
    let Some(held) = journaled.get_mut(agent) else {
        return;
    };
    match &record.event {
        SessionEvent::ModeChanged { .. } => {
            held.contract.mode = mode;
            // The mode's prompt and tools are pinned by its next model context.
            held.contract.context = None;
        }
        SessionEvent::ModelChanged { profile } => held.contract.profile = profile.clone(),
        SessionEvent::ModelContext { context } if context.purpose == ModelPurpose::Agent => {
            held.contract.context = Some((record.sequence, context.clone()));
        }
        _ => {}
    }
}

impl Journaled {
    /// What `read` leaves of `agent`'s contract.
    pub(super) fn read(agent: &AgentId, read: &crate::session::AgentRecords) -> Option<Self> {
        let (mut editions, mut journaled) = (HashMap::new(), HashMap::new());
        for record in &read.records {
            let begins = &record.agent == agent;
            fold(&mut editions, &mut journaled, begins, record);
        }
        let mut held = journaled.remove(agent)?;
        held.children = held.children.max(read.children);
        Some(held)
    }
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
    pub(super) fn park_holding(&mut self, failure: crate::agent::Failure, held: queue::Batch) {
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

/// Each agent's activity as the session's state reads: see [`activity`].
pub(crate) fn activities(state: &CoreState) -> HashMap<AgentId, ObservedActivity> {
    let activity = |agent: &AgentId| Some((agent.clone(), activity(state, agent)?));
    observed_agents(state).filter_map(activity).collect()
}

/// The agents with an activity: those that run, and the root, as its journal
/// leaves its last turn.
pub(crate) fn observed_agents(state: &CoreState) -> impl Iterator<Item = &AgentId> {
    let agents = &state.agents;
    let journaled = (agents.journaled.keys()).filter(|agent| !agents.running.contains_key(*agent));
    agents.running.keys().chain(journaled)
}

/// `agent`'s activity as the session's state reads: a running agent's from its
/// turn, its open request and its jobs; the root's, before it runs, as its
/// journal leaves it, since its last turn. A parked child has none. A session
/// that no longer runs reads a busy one as interrupted at its last activity.
pub(crate) fn activity(state: &CoreState, agent: &AgentId) -> Option<ObservedActivity> {
    let agents = &state.agents;
    let Some(facts) = agents.running.get(agent) else {
        agents.journaled.get(agent)?;
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
    Parked(crate::identity::JobId, u64),
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

/// One bounded coalescing window per agent, shared by its inputs and wakes, so
/// a burst releases as one batch. Its timer runs outside the core.
#[derive(Default)]
pub(super) struct Window {
    /// Counts every input and wake; ahead of `released` while a window is open.
    pub(super) scheduled: u64,
    pub(super) released: u64,
    /// Counts the inputs among them.
    inputs: u64,
    /// `inputs` as of the latest release.
    pub(super) ready_inputs: u64,
    /// `ready_inputs` as of the agent's latest request.
    pub(super) observed_inputs: u64,
}

impl Window {
    fn schedule(&mut self, input: bool) {
        self.scheduled += 1;
        self.inputs += u64::from(input);
    }

    pub(super) fn release(&mut self) {
        (self.released, self.ready_inputs) = (self.scheduled, self.inputs);
    }

    pub(super) fn settled(&self) -> bool {
        self.released == self.scheduled
    }

    /// Whether an input is scheduled since the latest release.
    #[cfg(test)]
    pub(super) fn holds_input(&self) -> bool {
        self.inputs > self.ready_inputs
    }
}

impl Agents {
    /// `agent`'s running facts, to change: the agent is marked for the waits
    /// that read it.
    pub(crate) fn get_mut(&mut self, agent: &AgentId) -> Option<&mut AgentFacts> {
        let facts = self.running.get_mut(agent)?;
        self.dirty.agent(agent);
        Some(facts)
    }

    /// Run `agent` with `facts`, under what its journal leaves of it, `read` for
    /// one that resumes unless that is held, as the root's always is: only the
    /// change that finds it without a loop starts one.
    pub(super) fn insert(&mut self, agent: AgentId, facts: AgentFacts, read: Option<Journaled>) {
        self.dirty.agent(&agent);
        if let Some(read) = read {
            self.journaled.entry(agent.clone()).or_insert(read);
        }
        debug_assert!(
            self.journaled.contains_key(&agent),
            "a loop runs under its journal"
        );
        let replaced = self.running.insert(agent, facts);
        debug_assert!(replaced.is_none(), "one loop per agent");
    }

    /// `agent`'s loop is gone, which its owner's parking reads, and with a
    /// child's what its journal left of it.
    pub(super) fn remove(&mut self, agent: &AgentId) -> Option<AgentFacts> {
        self.dirty.agent(agent);
        if let Some(owner) = agent.parent() {
            self.dirty.agent(&owner);
            self.journaled.remove(agent);
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

    /// What every change since the last take touched.
    pub(crate) fn take_dirty(&mut self) -> Dirty {
        std::mem::take(&mut self.dirty)
    }

    /// What `records` leave of the root's contract, its highest child index being
    /// `children`, and of the mode editions they record, as a session opens.
    pub(crate) fn opened(records: &[std::sync::Arc<EventRecord>], children: u32) -> Self {
        let mut agents = Self::default();
        for record in records {
            let begins = record.agent.parent().is_none();
            fold(&mut agents.editions, &mut agents.journaled, begins, record);
        }
        for held in agents.journaled.values_mut() {
            held.children = held.children.max(children);
        }
        agents
    }

    /// Fold what a committed `record` says of its agent, if its journal is held: a
    /// start begins it, as the agent's loop starts in that commit.
    pub(crate) fn observe(&mut self, record: &EventRecord) {
        fold(&mut self.editions, &mut self.journaled, true, record);
    }

    /// The latest journaled edition of mode `name`.
    pub(crate) fn edition(&self, name: &ModeName) -> Option<&Mode> {
        self.editions.get(name).map(|mode| &**mode)
    }

    /// How many of `agent`'s loops have ended.
    pub(super) fn ended(&self, agent: &AgentId) -> u64 {
        self.ended.get(agent).copied().unwrap_or_default()
    }

    /// What `agent`'s journal leaves of it, while that is held.
    pub(super) fn journaled(&self, agent: &AgentId) -> Option<&Journaled> {
        self.journaled.get(agent)
    }

    /// Reserve `parent`'s next child id, while `parent` runs.
    pub(super) fn next_child(&mut self, parent: &AgentId) -> Option<AgentId> {
        let held = self.journaled.get_mut(parent)?;
        held.children = held.children.saturating_add(1);
        Some(parent.child(held.children))
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

    /// Take `agent`'s queued inputs, leaving every other command in order.
    pub(super) fn take_queued(&mut self, agent: &AgentId) -> Vec<QueuedInput> {
        let Some(facts) = self.get_mut(agent) else {
            return Vec::new();
        };
        let (queued, kept) = std::mem::take(&mut facts.inbox)
            .into_iter()
            .partition(|command| matches!(command, AgentCommand::QueuedInputs(_)));
        facts.inbox = kept;
        let inputs = queued.into_iter().flat_map(|command| match command {
            AgentCommand::QueuedInputs(inputs) => inputs,
            AgentCommand::Input { .. }
            | AgentCommand::Select { .. }
            | AgentCommand::Continue { .. } => {
                unreachable!("partitioned out")
            }
        });
        inputs.collect()
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
    /// the input that begins its invocation yet to begin: that runs as the
    /// invocation's first turn. Whether it handed any back.
    pub(crate) fn hand_back(&mut self, agent: &AgentId, jobs: &crate::job::Jobs) -> bool {
        let Some(facts) = self.get_mut(agent) else {
            return false;
        };
        let begins = jobs.unbegun(agent).is_some()
            && matches!(facts.inbox.front(), Some(AgentCommand::Input { .. }));
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
        self.journaled.retain(|agent, _| agent.parent().is_none());
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
