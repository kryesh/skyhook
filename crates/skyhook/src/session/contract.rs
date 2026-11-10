//! What the journal leaves of each agent's contract: its profile, mode, location and
//! latest agent model context, and each mode's latest edition. The root's is held from
//! the session's opening and a child's only while its loop runs; a parked child's is
//! read again from the records its state names.
use std::{collections::HashMap, sync::Arc};

use super::db::AgentRecords;
use crate::identity::AgentId;
use crate::session::{
    CoreState, EventRecord, ModeSelection, ModelContext, ModelPurpose, ProfileSnapshot, RecordSeq,
    SessionError, SessionEvent, SessionStore, TurnFailure,
};
use crate::{
    config::mode::{Edition, ModeName},
    tool::policy::CapabilitySet,
};
use skyhook_tools::target::ExecutionLocation;

/// The contracts the session holds, and each mode's latest journaled edition.
#[derive(Default)]
pub(crate) struct Contracts {
    held: HashMap<AgentId, Journaled>,
    editions: HashMap<ModeName, Arc<Edition>>,
}

/// What an agent's journal leaves of its contract that its loop, its owner and the
/// host read.
#[derive(Clone)]
pub(crate) struct Journaled {
    pub(crate) contract: Contract,
    /// The highest child index it has started or reserved. A launch the session
    /// refuses leaves its index unused.
    children: u32,
}

/// The contract an agent runs under. A journaled one is its start, latest applied
/// profile, and its latest agent model context; its grants are its state's.
#[derive(Clone)]
pub(crate) struct Contract {
    pub(crate) profile: ProfileSnapshot,
    pub(crate) mode: Option<AppliedMode>,
    pub(crate) location: ExecutionLocation,
    /// The latest agent model context and its record, while the mode is unchanged:
    /// a mode pins its prompt and tools with a context of its own.
    pub(crate) context: Option<(RecordSeq, ModelContext)>,
}

/// A mode as an agent runs it: the edition it was applied under, which the
/// configuration may since have changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AppliedMode {
    pub(crate) name: ModeName,
    pub(crate) definition: Arc<Edition>,
}

impl AppliedMode {
    /// The journal form of applying it, which records its definition only when
    /// that is a new edition.
    pub(crate) fn selection(&self) -> ModeSelection {
        ModeSelection {
            name: self.name.clone(),
            definition: Some(Edition::clone(&self.definition)),
        }
    }
}

/// An agent's journaled contract, the grants its state holds, and why its last turn
/// stopped short of an answer, if it did.
pub(crate) struct Recorded {
    pub(crate) journaled: Journaled,
    pub(crate) available_depth: usize,
    pub(crate) capabilities: CapabilitySet,
    pub(crate) stopped: Option<TurnFailure>,
}

/// The edition `selection` applies: the one it records, or its name's latest.
fn applied(
    editions: &mut HashMap<ModeName, Arc<Edition>>,
    selection: &ModeSelection,
) -> Option<AppliedMode> {
    if let Some(definition) = &selection.definition {
        editions.insert(selection.name.clone(), definition.clone().into());
    }
    Some(AppliedMode {
        name: selection.name.clone(),
        definition: editions.get(&selection.name)?.clone(),
    })
}

/// Fold `record` into what `held` holds of its agent: its start begins that where it
/// `begins`, and a child's start counts in its parent's children. Every record keeps
/// the mode editions it records.
fn fold(
    editions: &mut HashMap<ModeName, Arc<Edition>>,
    held: &mut HashMap<AgentId, Journaled>,
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
            && let Some(parent) = held.get_mut(&parent)
        {
            parent.children = parent.children.max(*index);
        }
        let contract = Contract {
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
            held.insert(agent.clone(), started);
        }
        return;
    }
    let Some(held) = held.get_mut(agent) else {
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
    fn read(agent: &AgentId, read: &AgentRecords) -> Option<Self> {
        let (mut editions, mut held) = (HashMap::new(), HashMap::new());
        for record in &read.records {
            let begins = &record.agent == agent;
            fold(&mut editions, &mut held, begins, record);
        }
        let mut journaled = held.remove(agent)?;
        journaled.children = journaled.children.max(read.children);
        Some(journaled)
    }
}

impl Contracts {
    /// What `records` leave of the root's contract, its highest child index being
    /// `children`, and of the mode editions they record, as a session opens.
    pub(crate) fn opened(records: &[Arc<EventRecord>], children: u32) -> Self {
        let mut contracts = Self::default();
        for record in records {
            let begins = record.agent.parent().is_none();
            fold(&mut contracts.editions, &mut contracts.held, begins, record);
        }
        for held in contracts.held.values_mut() {
            held.children = held.children.max(children);
        }
        contracts
    }

    /// Fold what a committed `record` says of its agent, if its contract is held: a
    /// start begins it, as the agent's loop starts in that commit.
    pub(crate) fn observe(&mut self, record: &EventRecord) {
        fold(&mut self.editions, &mut self.held, true, record);
    }

    /// The latest journaled edition of mode `name`.
    pub(crate) fn edition(&self, name: &ModeName) -> Option<&Edition> {
        self.editions.get(name).map(|mode| &**mode)
    }

    /// `agent`'s contract, while it is held.
    pub(crate) fn get(&self, agent: &AgentId) -> Option<&Contract> {
        self.held.get(agent).map(|held| &held.contract)
    }

    /// The agents whose contracts are held.
    pub(crate) fn agents(&self) -> impl Iterator<Item = &AgentId> {
        self.held.keys()
    }

    /// Reserve `parent`'s next child id, while its contract is held.
    pub(crate) fn next_child(&mut self, parent: &AgentId) -> Option<AgentId> {
        let held = self.held.get_mut(parent)?;
        held.children = held.children.saturating_add(1);
        Some(parent.child(held.children))
    }

    /// Hold `agent`'s contract as its loop starts: `read` for one that resumes, unless
    /// that is held, as the root's always is.
    pub(crate) fn hold(&mut self, agent: &AgentId, read: Option<Journaled>) {
        if let Some(read) = read {
            self.held.entry(agent.clone()).or_insert(read);
        }
    }

    /// Let go of a child's contract once its loop is gone.
    pub(crate) fn release(&mut self, agent: &AgentId) {
        if agent.parent().is_some() {
            self.held.remove(agent);
        }
    }
}

impl SessionStore {
    /// What `agent`'s journal leaves of its contract and grants, and why its last turn
    /// stopped short of an answer, if it did; none for an agent it never started. A
    /// contract that is not held is read from the records the agent's state names.
    pub(crate) async fn contract(&self, agent: &AgentId) -> Result<Option<Recorded>, SessionError> {
        let id = agent.clone();
        let read = self.read(move |state: &CoreState| {
            let started = state.agent_states.get(&id)?;
            let held = state.contracts.held.get(&id).cloned().ok_or_else(|| {
                let context = (state.requests).context(&id, ModelPurpose::Agent);
                ([started.profiled, started.granted], context)
            });
            let stopped = started.stopped(&id, &state.requests);
            let grants = (started.available_depth, started.capabilities.clone());
            Some((held, grants, stopped))
        });
        let Some((held, (available_depth, capabilities), stopped)) = read else {
            return Ok(None);
        };
        let journaled = match held {
            Ok(held) => held,
            Err((entries, context)) => {
                let (journal, id) = (self.journal(), agent.clone());
                let read = move || {
                    let read = journal.agent(&id, entries, context)?;
                    Ok::<_, SessionError>(read.and_then(|read| Journaled::read(&id, &read)))
                };
                match self.reads().hold(read).await? {
                    Some(journaled) => journaled,
                    None => return Ok(None),
                }
            }
        };
        Ok(Some(Recorded {
            journaled,
            available_depth,
            capabilities,
            stopped,
        }))
    }
}
