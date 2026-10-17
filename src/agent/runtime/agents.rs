//! What the core keeps of each agent: what it runs under, and of each running
//! one its turn, inbox and wake window.
use std::collections::{BTreeMap, HashMap, VecDeque};

use super::{
    AgentCommand, HarnessError, TurnFailure,
    queue::{self, QueuedInput},
};
use crate::{
    agent::{AgentActivity, ObservedActivity},
    execution::ExecutionLocation,
    identity::AgentId,
    session::{
        CoreState, EventRecord, Message, ModelContext, ModelPurpose, ProfileSnapshot, RecordSeq,
        RequestLedger, RequestPhase, SessionEvent,
    },
    tool::policy::{CapabilitySet, Mode, ModeName},
};

/// The agents whose loops run, and whether they take deliveries.
#[derive(Default)]
pub(crate) struct Agents {
    running: BTreeMap<AgentId, AgentFacts>,
    pub(super) flow: Flow,
    pub(super) activation: Activation,
    /// The highest child index each agent has started or reserved. A launch
    /// the session refuses leaves its index unused.
    children: HashMap<AgentId, u32>,
    /// What each agent's journal says it runs under, whether it runs or not.
    contracts: HashMap<AgentId, RecordedContract>,
    /// Each mode's latest journaled edition.
    editions: HashMap<ModeName, std::sync::Arc<Mode>>,
    /// Where each agent's journal leaves its last turn, and when.
    last: HashMap<AgentId, (Last, i64)>,
}

/// Where an agent's journal leaves its last turn: its newest message awaits the
/// model or answers it, a failure since stopped it, or one was reported with
/// nothing open to continue.
enum Last {
    Awaiting,
    Answered,
    Failed(TurnFailure),
    Refused(TurnFailure),
}

/// The contract an agent runs under. A journaled one is its start, latest applied
/// profile, and its latest agent model context.
#[derive(Clone)]
pub(super) struct RecordedContract {
    pub(super) profile: ProfileSnapshot,
    pub(super) available_depth: usize,
    pub(super) mode: Option<AppliedMode>,
    pub(super) capabilities: CapabilitySet,
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

impl std::ops::DerefMut for Agents {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.running
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
}

/// Each agent's activity as the session's state reads: a running agent's from
/// its turn, its open request and its jobs; any other's as its journal leaves
/// it, since its last turn. A session that no longer runs reads every busy one
/// as interrupted at its last activity.
pub(crate) fn activities(state: &CoreState) -> HashMap<AgentId, ObservedActivity> {
    let agents = &state.agents;
    let stopped = state.phase.stopped_at(&state.records);
    let journaled = (agents.last.iter()).filter(|(agent, _)| !agents.running.contains_key(*agent));
    let journaled = journaled.map(|(agent, &(_, since))| {
        let state = stopped_turn(state, agent).map_or(AgentActivity::Idle, AgentActivity::Stopped);
        (agent.clone(), ObservedActivity { state, since })
    });
    let running = (agents.running.iter()).map(|(agent, facts)| {
        let activity = match (running(state, agent, facts), stopped) {
            (state, Some(since)) if state.is_busy() => ObservedActivity {
                state: interrupted(),
                since,
            },
            (state, _) => ObservedActivity {
                state,
                since: facts.since,
            },
        };
        (agent.clone(), activity)
    });
    journaled.chain(running).collect()
}

fn interrupted() -> AgentActivity {
    AgentActivity::Stopped(TurnFailure::Interrupted)
}

/// Why `agent`'s journal leaves its last turn short of an answer, so resuming
/// has something to continue: a failure since its newest message, or that
/// message, or its newest request, still awaiting the model.
pub(crate) fn stopped_turn(state: &CoreState, agent: &AgentId) -> Option<TurnFailure> {
    turn_stopped(state.agents.last.get(agent), &state.ledger, agent)
}

fn turn_stopped(
    last: Option<&(Last, i64)>,
    ledger: &RequestLedger,
    agent: &AgentId,
) -> Option<TurnFailure> {
    let incomplete = (ledger.latest(agent).and_then(|request| ledger.get(request)))
        .is_some_and(|request| !matches!(request.phase, RequestPhase::Completed { .. }));
    match last.map(|(last, _)| last) {
        Some(Last::Failed(failure)) => Some(failure.clone()),
        Some(Last::Awaiting) => Some(TurnFailure::Interrupted),
        Some(Last::Answered | Last::Refused(_)) | None => {
            incomplete.then_some(TurnFailure::Interrupted)
        }
    }
}

/// Rest `agent`'s parked turn once nothing is left for `continue` to try again:
/// the journal reads it answered, no delivery waits, and any batch it held was
/// withdrawn. One its owner cancelled stays parked. Whether it stays parked.
pub(super) fn settle_held(state: &mut CoreState, agent: &AgentId) -> bool {
    let open = stopped_turn(state, agent)
        .or_else(|| held_deliveries(state, agent))
        .is_some();
    let Some(facts) = state.agents.running.get_mut(agent) else {
        return false;
    };
    let holds = facts.held.as_mut().is_some_and(queue::Batch::holds);
    if !holds {
        facts.held = None;
    }
    match facts.turn {
        Turn::Parked(TurnFailure::Cancelled) => true,
        Turn::Parked(_) if holds || open => true,
        Turn::Parked(_) => {
            facts.set_turn(Turn::Idle);
            false
        }
        _ => false,
    }
}

/// Why `agent`, with no turn to continue, rests parked on the deliveries
/// pending for it: the failure last reported since its turn, or an interruption.
pub(crate) fn held_deliveries(state: &CoreState, agent: &AgentId) -> Option<TurnFailure> {
    let held = match state.agents.last.get(agent) {
        Some((Last::Refused(failure), _)) => failure.clone(),
        _ => TurnFailure::Interrupted,
    };
    state.jobs.has_pending(agent).then_some(held)
}

/// A running agent's activity: an answered child still waits on what it runs.
fn running(state: &CoreState, agent: &AgentId, facts: &AgentFacts) -> AgentActivity {
    match &facts.turn {
        Turn::Parked(failure) => AgentActivity::Stopped(failure.clone()),
        Turn::Held | Turn::Cancelled(Was::Held) => AgentActivity::WaitingChildren,
        Turn::Idle | Turn::Cancelled(Was::Idle)
            if agent.parent().is_some() && state.jobs.has_running(agent) =>
        {
            AgentActivity::WaitingChildren
        }
        Turn::Idle | Turn::Cancelled(Was::Idle) => AgentActivity::Idle,
        Turn::Busy => match state.ledger.open(agent).and_then(|r| state.ledger.get(r)) {
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
                } else if state.jobs.has_running(agent) {
                    AgentActivity::Tools
                } else {
                    AgentActivity::Working
                }
            }
        },
    }
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
    /// Fold what `record` says of its agent, after `ledger` has: a start's index
    /// into its parent's count, what the agent runs under, and where its last
    /// turn stands.
    pub(crate) fn observe(&mut self, record: &EventRecord, ledger: &RequestLedger) {
        let agent = &record.agent;
        let contract = self.contracts.get_mut(agent);
        match (&record.event, contract) {
            (
                SessionEvent::AgentStarted {
                    profile,
                    available_depth,
                    mode,
                    capabilities,
                    location,
                    ..
                },
                _,
            ) => {
                if let (Some(parent), Some(index)) = (agent.parent(), agent.path().last()) {
                    let taken = self.children.entry(parent).or_default();
                    *taken = (*taken).max(*index);
                }
                let contract = RecordedContract {
                    profile: profile.clone(),
                    available_depth: *available_depth as usize,
                    mode: (mode.as_ref()).and_then(|mode| applied(&mut self.editions, mode)),
                    capabilities: capabilities.iter().copied().collect(),
                    location: location.clone(),
                    context: None,
                };
                self.contracts.insert(agent.clone(), contract);
            }
            (SessionEvent::ModelChanged { profile }, Some(contract)) => {
                contract.profile = profile.clone();
            }
            (SessionEvent::ModeChanged { mode, capabilities }, Some(contract)) => {
                contract.mode = applied(&mut self.editions, mode);
                contract.capabilities = capabilities.iter().copied().collect();
                // The mode's prompt and tools are pinned by its next model context.
                contract.context = None;
            }
            (SessionEvent::ModelContext { context }, Some(contract))
                if context.purpose == ModelPurpose::Agent =>
            {
                contract.context = Some((record.sequence, context.clone()));
            }
            (SessionEvent::MessageCommitted { message }, _) => {
                let answers = match message {
                    Message::Assistant(items) => !items.iter().any(|item| item.call().is_some()),
                    _ => false,
                };
                let last = if answers {
                    Last::Answered
                } else {
                    Last::Awaiting
                };
                (self.last).insert(agent.clone(), (last, record.timestamp_millis));
            }
            // Only a failure that ends an open turn stops it: one with nothing
            // left to continue is reported, and the agent rests.
            (SessionEvent::AgentFailed { failure }, _) => {
                let failure = TurnFailure::from(failure.clone());
                let last = match turn_stopped(self.last.get(agent), ledger, agent) {
                    Some(_) => Last::Failed(failure),
                    None => Last::Refused(failure),
                };
                (self.last).insert(agent.clone(), (last, record.timestamp_millis));
            }
            _ => {}
        }
    }

    /// The latest journaled edition of mode `name`.
    pub(crate) fn edition(&self, name: &ModeName) -> Option<&Mode> {
        self.editions.get(name).map(|mode| &**mode)
    }

    /// What `agent`'s journal says it runs under.
    pub(super) fn contract(&self, agent: &AgentId) -> Option<&RecordedContract> {
        self.contracts.get(agent)
    }

    /// Reserve `parent`'s next child id.
    pub(super) fn next_child(&mut self, parent: &AgentId) -> AgentId {
        let taken = self.children.entry(parent.clone()).or_default();
        *taken = taken.saturating_add(1);
        parent.child(*taken)
    }

    /// Schedule `agent`'s window for work to collect.
    pub(crate) fn wake(&mut self, agent: &AgentId) {
        if let Some(facts) = self.running.get_mut(agent) {
            facts.window.schedule(false);
        }
    }

    /// Queue `command` for `agent`; false once its loop is gone, when the command's
    /// waiters see it stopped.
    pub(super) fn send(&mut self, agent: &AgentId, command: AgentCommand) -> bool {
        let Some(facts) = self.running.get_mut(agent) else {
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
        let Some(facts) = self.running.get_mut(agent) else {
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
            | AgentCommand::Retry { .. } => {
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
        if let Some(facts) = self.running.get_mut(agent)
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
        let Some(facts) = self.running.get_mut(agent) else {
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
        (self.running.get(agent)).is_some_and(|facts| facts.turn == INTERRUPTED)
    }

    /// Hand back the input `agent` has yet to take, as interrupted, apart from
    /// the input that begins its invocation yet to begin: that runs as the
    /// invocation's first turn. Whether it handed any back.
    pub(crate) fn hand_back(&mut self, agent: &AgentId, jobs: &crate::job::Jobs) -> bool {
        let Some(facts) = self.running.get_mut(agent) else {
            return false;
        };
        let begins = jobs.unbegun(agent).is_some()
            && matches!(facts.inbox.front(), Some(AgentCommand::Input { .. }));
        let handed: Vec<_> = facts.inbox.drain(usize::from(begins)..).collect();
        let untaken = !handed.is_empty();
        handed
            .into_iter()
            .for_each(|command| command.refuse(|| HarnessError::Interrupted));
        untaken
    }

    /// A job `agent` held was cancelled on request: its held turn runs on.
    pub(crate) fn run_on(&mut self, agent: &AgentId) {
        if let Some(facts) = self.running.get_mut(agent)
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
        for facts in std::mem::take(&mut self.running).into_values() {
            facts
                .inbox
                .into_iter()
                .for_each(|command| command.refuse(&error));
        }
    }

    /// Cancel the work each of `agents` last took for good, so nothing more its
    /// turn asks is admitted: a busy turn is parked `Cancelled`, and an idle or
    /// held one is never made busy again.
    pub(super) fn cancel_turns(&mut self, agents: &[AgentId]) {
        for agent in agents {
            let Some(facts) = self.running.get_mut(agent) else {
                continue;
            };
            let cancelled = match facts.turn {
                Turn::Busy => Some(Turn::Parked(TurnFailure::Cancelled)),
                Turn::Idle => Some(Turn::Cancelled(Was::Idle)),
                Turn::Held => Some(Turn::Cancelled(Was::Held)),
                Turn::Parked(_) | Turn::Cancelled(_) => None,
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
