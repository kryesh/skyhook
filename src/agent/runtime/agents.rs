//! What the core keeps of each agent: what it runs under, and of each running
//! one its turn, inbox and wake window.
use std::collections::{BTreeMap, HashMap, VecDeque};

use super::{AgentCommand, HarnessError, TurnFailure, queue::QueuedInput};
use crate::{
    agent::{AgentActivity, ObservedActivity},
    execution::ExecutionLocation,
    identity::AgentId,
    provider::protocol::{SystemSegment, ToolDefinition},
    session::{
        CoreState, EventRecord, Message, ModelPurpose, ProfileSnapshot, RequestPhase, SessionEvent,
    },
    tool::policy::{CapabilitySet, ModeName},
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
    /// Where each agent's journal leaves its last turn, and when.
    last: HashMap<AgentId, (Last, i64)>,
}

/// Where an agent's journal leaves its last turn: its newest message awaits the
/// model or answers it, or a failure since stopped it.
enum Last {
    Awaiting,
    Answered,
    Failed(TurnFailure),
}

/// The contract an agent runs under. A journaled one is its start, latest applied
/// profile, and the prompt and tools of its latest agent model context.
#[derive(Clone)]
pub(super) struct RecordedContract {
    pub(super) profile: ProfileSnapshot,
    pub(super) available_depth: usize,
    pub(super) mode: Option<ModeName>,
    pub(super) capabilities: CapabilitySet,
    pub(super) location: ExecutionLocation,
    pub(super) system: Option<Vec<SystemSegment>>,
    pub(super) tools: Option<Vec<ToolDefinition>>,
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
}

impl AgentFacts {
    /// Move to `turn`, returning the turn it left: entering or leaving a busy
    /// turn starts its time.
    pub(super) fn set_turn(&mut self, turn: Turn) -> Turn {
        if turn.busy() != self.turn.busy() {
            self.since = chrono::Utc::now().timestamp_millis();
        }
        std::mem::replace(&mut self.turn, turn)
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
    /// From taking work until the turn it begins ends, or commits its answer with
    /// nothing left to carry. Only a busy turn requests, commits a response or
    /// makes calls.
    Busy,
    /// An interrupted turn still waiting on the foreground child invocations it
    /// holds. `continue` makes it busy again, as does cancelling a job it holds;
    /// a wait that ends otherwise parks it interrupted.
    Held,
    /// The last turn ended without an answer: only input or `continue` resumes
    /// it, and deliveries wait for that request.
    Parked(TurnFailure),
}

impl Turn {
    fn busy(&self) -> bool {
        matches!(self, Self::Busy | Self::Held)
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
    let ledger = &state.ledger;
    let incomplete = (ledger.latest(agent).and_then(|request| ledger.get(request)))
        .is_some_and(|request| !matches!(request.phase, RequestPhase::Completed { .. }));
    match state.agents.last.get(agent).map(|(last, _)| last) {
        Some(Last::Failed(failure)) => Some(failure.clone()),
        Some(Last::Awaiting) => Some(TurnFailure::Interrupted),
        Some(Last::Answered) | None => incomplete.then_some(TurnFailure::Interrupted),
    }
}

/// A running agent's activity: an answered child still waits on what it runs.
fn running(state: &CoreState, agent: &AgentId, facts: &AgentFacts) -> AgentActivity {
    match &facts.turn {
        Turn::Parked(failure) => AgentActivity::Stopped(failure.clone()),
        Turn::Held => AgentActivity::WaitingChildren,
        Turn::Idle if agent.parent().is_some() && state.jobs.has_running(agent) => {
            AgentActivity::WaitingChildren
        }
        Turn::Idle => AgentActivity::Idle,
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
    /// Fold what `record` says of its agent: a start's index into its parent's
    /// count, and what the agent runs under.
    pub(crate) fn observe(&mut self, record: &EventRecord) {
        let agent = &record.agent;
        let contract = self.contracts.get_mut(agent);
        match (&record.event, contract) {
            (
                SessionEvent::AgentStarted {
                    profile: Some(profile),
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
                    mode: mode.as_ref().map(|mode| mode.name.clone()),
                    capabilities: capabilities.iter().copied().collect(),
                    location: location.clone(),
                    system: None,
                    tools: None,
                };
                self.contracts.insert(agent.clone(), contract);
            }
            (SessionEvent::ModelChanged { profile }, Some(contract)) => {
                contract.profile = profile.clone();
            }
            (SessionEvent::ModeChanged { mode, capabilities }, Some(contract)) => {
                contract.mode = Some(mode.name.clone());
                contract.capabilities = capabilities.iter().copied().collect();
                // The mode's prompt and tools are pinned by its next model context.
                (contract.system, contract.tools) = (None, None);
            }
            (SessionEvent::ModelContext { context }, Some(contract))
                if context.purpose == ModelPurpose::Agent =>
            {
                contract.system = Some(context.system.clone());
                contract.tools = Some(context.tools.clone());
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
            (SessionEvent::AgentFailed { failure }, _) => {
                let failed = Last::Failed(failure.clone().into());
                (self.last).insert(agent.clone(), (failed, record.timestamp_millis));
            }
            _ => {}
        }
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
        facts.window.schedule(true);
        facts.inbox.push_back(command);
        true
    }

    /// Take `agent`'s queued input batches, leaving every other command in order.
    pub(super) fn take_queued(&mut self, agent: &AgentId) -> Vec<Vec<QueuedInput>> {
        let Some(facts) = self.running.get_mut(agent) else {
            return Vec::new();
        };
        let (queued, kept) = std::mem::take(&mut facts.inbox)
            .into_iter()
            .partition(|command| matches!(command, AgentCommand::QueuedInputs(_)));
        facts.inbox = kept;
        let batches = queued.into_iter().map(|command| match command {
            AgentCommand::QueuedInputs(inputs) => inputs,
            AgentCommand::Input { .. } => unreachable!("partitioned out"),
        });
        batches.collect()
    }

    /// Whether `agent` may do its turn's model work: request, commit a response
    /// or make calls. A turn an interrupt stopped or held may not, nor one that
    /// already answered.
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
    /// for it, or `deliveries` it takes. The turn keeps its token, which an
    /// interrupt cancels as the supervisor's current one.
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

    /// The agents within `root`.
    pub(super) fn within(&self, root: &AgentId) -> Vec<AgentId> {
        let agents = self.running.keys().filter(|agent| agent.is_within(root));
        agents.cloned().collect()
    }
}
