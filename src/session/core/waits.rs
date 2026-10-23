//! Registered waits, decided again only when what they depend on changes.
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use tokio::sync::oneshot::Sender;

use super::CoreState;
use crate::identity::{AgentId, JobId};

/// What a wait reads: it is decided again once a change touches one of these.
/// A session-wide change (phase, flow or activation) decides every wait again,
/// so none declares those.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Key {
    /// The agent's running facts, its journaled turn and records, its requests,
    /// and every job it owns or answers through.
    Agent(AgentId),
    Job(JobId),
}

/// The keys the change being applied touched.
#[derive(Default)]
pub(crate) struct Dirty {
    keys: HashSet<Key>,
    /// A session-wide change (phase, flow or activation): every wait is decided again.
    session: bool,
}

impl Dirty {
    pub(crate) fn agent(&mut self, agent: &AgentId) {
        self.keys.insert(Key::Agent(agent.clone()));
    }

    pub(crate) fn job(&mut self, job: JobId) {
        self.keys.insert(Key::Job(job));
    }

    pub(crate) fn session(&mut self) {
        self.session = true;
    }

    pub(super) fn absorb(&mut self, other: Self) {
        self.keys.extend(other.keys);
        self.session |= other.session;
    }

    pub(super) fn is_session(&self) -> bool {
        self.session
    }

    pub(super) fn agents(&self) -> impl Iterator<Item = &AgentId> {
        self.keys.iter().filter_map(|key| match key {
            Key::Agent(agent) => Some(agent),
            Key::Job(_) => None,
        })
    }
}

/// A wait to register: the keys it reads, and the waiter it becomes with its reply.
pub(crate) struct Wait<T> {
    keys: Vec<Key>,
    waiter: Box<dyn FnOnce(Sender<T>) -> Box<dyn Waiter> + Send>,
}

impl<T> Wait<T> {
    /// The jobs it reads.
    pub(crate) fn jobs(&self) -> Vec<JobId> {
        let job = |key: &Key| match key {
            Key::Job(job) => Some(*job),
            Key::Agent(_) => None,
        };
        self.keys.iter().filter_map(job).collect()
    }
}

impl<T: Send + 'static> Wait<T> {
    /// Resolve with what `decide` reads once it reads anything.
    pub(crate) fn on(
        keys: impl IntoIterator<Item = Key>,
        decide: impl Fn(&CoreState) -> Option<T> + Send + Sync + 'static,
    ) -> Self {
        Self::taking(keys, decide, |_, decided| decided)
    }

    /// Resolve with what `take` makes of `decide`'s decision, in the change that
    /// decides it: `take` consumes what was decided on, so the waiter loses
    /// nothing by abandoning the reply.
    pub(crate) fn taking<D: 'static>(
        keys: impl IntoIterator<Item = Key>,
        decide: impl Fn(&CoreState) -> Option<D> + Send + Sync + 'static,
        take: impl FnMut(&mut CoreState, D) -> T + Send + Sync + 'static,
    ) -> Self {
        Self {
            keys: keys.into_iter().collect(),
            waiter: Box::new(move |reply| {
                let reply = Some(reply);
                Box::new(Decision {
                    decide,
                    take,
                    reply,
                })
            }),
        }
    }
}

/// A registered wait and the reply it resolves.
pub(super) trait Waiter: Send + Sync {
    /// Decide, replying once decided: true once it decided or its waiter gave up.
    fn resolve(&mut self, state: &mut CoreState) -> bool;
    /// Whether it would reply now, read without changing anything.
    #[cfg(debug_assertions)]
    fn ready(&self, state: &CoreState) -> bool;
}

/// A decision split from what it consumes, so whether it is ready can be read
/// without changing anything.
struct Decision<F, G, T> {
    decide: F,
    take: G,
    reply: Option<Sender<T>>,
}

impl<D, T, F, G> Waiter for Decision<F, G, T>
where
    T: Send + 'static,
    F: Fn(&CoreState) -> Option<D> + Send + Sync,
    G: FnMut(&mut CoreState, D) -> T + Send + Sync,
{
    fn resolve(&mut self, state: &mut CoreState) -> bool {
        if self.reply.as_ref().is_none_or(|reply| reply.is_closed()) {
            return true;
        }
        let Some(decided) = (self.decide)(state) else {
            return false;
        };
        let outcome = (self.take)(state, decided);
        // Replied once what decided it is durable.
        if let Some(reply) = self.reply.take() {
            state.reply(move || drop(reply.send(outcome)));
        }
        true
    }

    #[cfg(debug_assertions)]
    fn ready(&self, state: &CoreState) -> bool {
        let waiting = self.reply.as_ref().is_some_and(|reply| !reply.is_closed());
        waiting && (self.decide)(state).is_some()
    }
}

pub(super) struct Registered {
    keys: Vec<Key>,
    pub(super) waiter: Box<dyn Waiter>,
}

/// Every registered wait, by registration order and by the keys it reads.
#[derive(Default)]
pub(super) struct Waits {
    next: u64,
    waits: BTreeMap<u64, Registered>,
    keyed: HashMap<Key, BTreeSet<u64>>,
    /// Registered since the last settle, which decides them first.
    fresh: BTreeSet<u64>,
}

impl Waits {
    pub(super) fn register<T: Send + 'static>(&mut self, wait: Wait<T>, reply: Sender<T>) {
        self.insert(Registered {
            keys: wait.keys,
            waiter: (wait.waiter)(reply),
        });
    }

    fn insert(&mut self, registered: Registered) {
        let id = self.next;
        self.next += 1;
        for key in &registered.keys {
            self.keyed.entry(key.clone()).or_default().insert(id);
        }
        self.waits.insert(id, registered);
        self.fresh.insert(id);
    }

    /// The waits `dirty` wakes, in registration order: every one when the
    /// session moved, with those registered since the last settle.
    pub(super) fn woken(&mut self, dirty: &Dirty) -> BTreeSet<u64> {
        let mut woken = std::mem::take(&mut self.fresh);
        if dirty.is_session() {
            woken.extend(self.waits.keys());
        } else {
            let keyed = dirty.keys.iter().filter_map(|key| self.keyed.get(key));
            woken.extend(keyed.flatten());
        }
        woken
    }

    /// Take wait `id` out to decide it.
    pub(super) fn take(&mut self, id: u64) -> Option<Registered> {
        self.waits.remove(&id)
    }

    /// Put back wait `id`, which has yet to resolve.
    pub(super) fn restore(&mut self, id: u64, registered: Registered) {
        self.waits.insert(id, registered);
    }

    /// Forget wait `id`, which resolved.
    pub(super) fn forget(&mut self, id: u64, registered: Registered) {
        for key in &registered.keys {
            if let Some(ids) = self.keyed.get_mut(key) {
                ids.remove(&id);
                if ids.is_empty() {
                    self.keyed.remove(key);
                }
            }
        }
    }

    /// Take over `other`'s waits, as registered after this one's.
    pub(super) fn absorb(&mut self, other: Self) {
        other.waits.into_values().for_each(|wait| self.insert(wait));
    }

    /// How many registered waits read `key`.
    #[cfg(test)]
    pub(super) fn reading(&self, key: &Key) -> usize {
        self.keyed.get(key).map_or(0, BTreeSet::len)
    }

    pub(super) fn clear(&mut self) {
        *self = Self {
            next: self.next,
            ..Self::default()
        };
    }

    /// A wait that would resolve now though nothing woke it: it reads a key it
    /// did not declare.
    #[cfg(debug_assertions)]
    pub(super) fn missed(&self, state: &CoreState) -> Option<&[Key]> {
        let mut waits = self.waits.values();
        let missed = waits.find(|registered| registered.waiter.ready(state));
        missed.map(|registered| registered.keys.as_slice())
    }
}
