//! What the core folds from the journal: the one in-memory copy of session state.
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

use super::{
    EventRecord, JournalFailure, SessionError, SessionEvent, SessionPhase,
    waits::{Dirty, Wait, Waits},
};
use crate::{
    agent::{
        ObservationSnapshot, ObservedActivity, ObservedEvent, RuntimeEvent,
        todo::{self, AgentTodos},
    },
    identity::{AgentId, JobId},
    job::{Invocation, Jobs, Run},
    session::{
        EntryKind, Message, RecordSeq, ResidentLedger, UserPart, agent_state::AgentStates,
        db::Opening,
    },
};

#[derive(Default)]
pub(crate) struct CoreState {
    phase: SessionPhase,
    counters: Counters,
    /// Each agent's newest request and what its turn leaves open.
    pub(crate) requests: ResidentLedger,
    /// What the journal leaves of each started agent beside its requests.
    pub(crate) agent_states: AgentStates,
    pub(crate) todos: BTreeMap<AgentId, AgentTodos>,
    pub(crate) jobs: Jobs,
    /// Never journaled: the agents whose loops run.
    pub(crate) agents: crate::agent::Agents,
    pub(crate) targets: crate::target::TargetRegistry,
    /// Whether the session has a title: one it was given, or else its root's
    /// first prompt text.
    pub(crate) titled: bool,
    /// Registered predicates, never journaled: each resolves by replying, or goes
    /// once nobody waits for it.
    waits: Waits,
    /// What the records applied since the last settle touched.
    dirty: Dirty,
    /// Each agent's activity as of the last settle.
    activity: HashMap<AgentId, ObservedActivity>,
    /// What the change being applied does once its critical section ends. Effects
    /// carry futures that are not `Sync`, such as an agent loop's provider context:
    /// the lock keeps the state `Sync`, and only `get_mut` reaches it.
    deferred: std::sync::Mutex<Deferred>,
}

/// What a change defers: the replies of the waits it decided, which go ahead of its
/// effects.
#[derive(Default)]
struct Deferred {
    replies: Vec<Effect>,
    effects: Vec<Effect>,
}

/// What the journal's newest records leave, kept as each applies rather than read
/// back from the records.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::session) struct Counters {
    /// The newest record's sequence: journal sequences have no gaps.
    pub(in crate::session) newest: RecordSeq,
    /// When the newest record that is the session's activity was journaled.
    pub(in crate::session) last_activity: Option<i64>,
}

impl Counters {
    fn apply(&mut self, record: &EventRecord) {
        self.newest = record.sequence;
        if record.event.is_activity() {
            self.last_activity = Some(record.timestamp_millis);
        }
    }
}

/// A non-blocking step a change takes outside the critical section that applies it.
pub(crate) type Effect = Box<dyn FnOnce() + Send>;

/// What observers see beside the committed state: the views the runtime posts
/// without journaling them, as streaming deltas, and the revision every
/// publication takes.
#[derive(Default)]
pub(in crate::session) struct Observed {
    /// The newest record applied; journal sequences have no gaps.
    applied: RecordSeq,
    /// Counts publications, so an observer applies each one once.
    revision: u64,
    views: crate::agent::Views,
    /// The run of each job whose output changed after it was last read, as
    /// observers were told.
    unread: HashMap<JobId, Invocation>,
    /// Each open feed of an agent's records, in order as they apply.
    feeds: HashMap<AgentId, Vec<Feed>>,
}

/// A feed of one agent's records of the kinds its subscriber folds.
struct Feed {
    kinds: fn(EntryKind) -> bool,
    records: std::sync::mpsc::Sender<Arc<EventRecord>>,
}

/// The views the newest publication left, which the state does not hold.
pub(in crate::session) struct Posted {
    revision: u64,
    applied: RecordSeq,
    views: crate::agent::Views,
}

impl CoreState {
    /// Why `agent`'s journal leaves its last turn short of an answer, if it does.
    pub(crate) fn stopped_turn(&self, agent: &AgentId) -> Option<crate::agent::TurnFailure> {
        self.agent_states.get(agent)?.stopped(agent, &self.requests)
    }

    /// Open the state the journal's rows leave, with `counters` from its index.
    pub(in crate::session) fn open(
        counters: Counters,
        opening: Opening,
    ) -> Result<(Self, Observed), SessionError> {
        let Opening {
            ledger,
            agents,
            records,
            children,
            todos,
            gauged,
            jobs,
            seeds,
            titled,
            entries,
        } = opening;
        let mut registry = crate::target::TargetRegistry::default();
        records
            .iter()
            .for_each(|record| registry.apply(&record.event));
        let mut state = Self {
            counters,
            requests: ledger,
            agent_states: AgentStates::opened(agents),
            todos,
            agents: crate::agent::Agents::opened(&records, children),
            targets: registry,
            titled,
            ..Self::default()
        };
        let views = crate::agent::Views {
            entries,
            context: crate::agent::ContextGauge::opened(&gauged),
            ..Default::default()
        };
        let observed = Observed {
            applied: counters.newest,
            views,
            ..Observed::default()
        };
        state.jobs.open(seeds, crate::job::Settled::of(jobs)?);
        // Nothing waits yet.
        state.take_dirty();
        Ok((state, observed))
    }

    /// The sequence the next record takes.
    pub(crate) fn next_sequence(&self) -> RecordSeq {
        self.counters.newest.next()
    }

    /// When the newest record that is the session's activity was journaled.
    pub(crate) fn last_activity(&self) -> Option<i64> {
        self.counters.last_activity
    }

    /// Fold one record its transaction journals into the whole state, ahead of its
    /// COMMIT. Returns the job whose lifecycle it moved; a transition its job's phase
    /// does not admit is refused, before it changes anything.
    pub(super) fn apply(
        &mut self,
        observed: &mut Observed,
        record: &Arc<EventRecord>,
    ) -> Result<Option<JobId>, SessionError> {
        let job = self.jobs.apply(record)?;
        self.agent_states.apply(record, &mut self.requests);
        observed.views.record(&self.requests, record);
        self.agents.observe(record);
        todo::apply(&mut self.todos, record);
        self.targets.apply(&record.event);
        self.dirty.agent(&record.agent);
        self.titled |= match &record.event {
            SessionEvent::TitleSet { .. } => true,
            SessionEvent::MessageCommitted {
                message: Message::User(parts),
            } => {
                record.agent.parent().is_none()
                    && (parts.iter()).any(|part| matches!(part, UserPart::Text { .. }))
            }
            _ => false,
        };
        Ok(job)
    }

    /// Count `record`, applied and committed, as the journal's newest, and hand it
    /// to observers' feeds.
    pub(super) fn applied(&mut self, observed: &mut Observed, record: &Arc<EventRecord>) {
        self.counters.apply(record);
        observed.applied = record.sequence;
        observed.views.count(record);
        observed.feed(record);
        // The finish tells observers of the run's last output.
        if let SessionEvent::JobFinished { job, .. } = &record.event {
            observed.unread.remove(job);
        }
    }

    pub(crate) fn phase(&self) -> &SessionPhase {
        &self.phase
    }

    /// Move to `phase`, which decides every wait again.
    pub(in crate::session) fn set_phase(&mut self, phase: SessionPhase) {
        if self.phase != phase {
            self.phase = phase;
            self.dirty.session();
        }
    }

    /// Run `effect` once the change being applied leaves its critical section,
    /// after the effects deferred before it.
    pub(crate) fn defer(&mut self, effect: impl FnOnce() + Send + 'static) {
        self.deferred().effects.push(Box::new(effect));
    }

    /// Send a decided wait's reply once the change's critical section ends, ahead of
    /// the effects it deferred.
    pub(super) fn reply(&mut self, send: impl FnOnce() + Send + 'static) {
        self.deferred().replies.push(Box::new(send));
    }

    fn take_deferred(&mut self) -> Vec<Effect> {
        let Deferred {
            mut replies,
            mut effects,
        } = std::mem::take(self.deferred());
        replies.append(&mut effects);
        replies
    }

    fn deferred(&mut self) -> &mut Deferred {
        (self.deferred.get_mut()).unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Register `wait`, replying on `reply` once it decides.
    pub(in crate::session) fn wait<T: Send + 'static>(
        &mut self,
        wait: Wait<T>,
        reply: tokio::sync::oneshot::Sender<T>,
    ) {
        if matches!(self.phase, SessionPhase::Closed | SessionPhase::Failed(_)) {
            return;
        }
        self.waits.register(wait, reply);
    }

    /// What every change since the last settle touched.
    fn take_dirty(&mut self) -> Dirty {
        let mut dirty = std::mem::take(&mut self.dirty);
        dirty.absorb(self.jobs.take_dirty());
        dirty.absorb(self.agents.take_dirty());
        dirty
    }

    /// Resolve the waits the change just applied wakes, again while what they
    /// consume wakes more; then read the activity of each agent it touched.
    /// Returns the replies and effects it deferred, in order, and the activities
    /// that changed.
    pub(super) fn settle(&mut self) -> (Vec<Effect>, Vec<(AgentId, Option<ObservedActivity>)>) {
        let (mut touched, mut everyone) = (std::collections::HashSet::new(), false);
        // Held out of the state while they decide, so a wait that panics drops
        // every wait with it.
        let mut waits = std::mem::take(&mut self.waits);
        loop {
            waits.absorb(std::mem::take(&mut self.waits));
            let dirty = self.take_dirty();
            everyone |= dirty.is_session();
            touched.extend(dirty.agents().cloned());
            let woken = waits.woken(&dirty);
            if woken.is_empty() {
                break;
            }
            for id in woken {
                let Some(mut registered) = waits.take(id) else {
                    continue;
                };
                if registered.waiter.resolve(self) {
                    waits.forget(id, registered);
                } else {
                    waits.restore(id, registered);
                }
            }
        }
        self.waits = waits;
        self.jobs.sweep();
        let activity = self.observe_activity(everyone, touched);
        #[cfg(debug_assertions)]
        self.assert_settled();
        (self.take_deferred(), activity)
    }

    /// Read again the activity of every agent in `touched`, and of every agent
    /// once the session moved, keeping what changed; none for an agent whose
    /// activity ended.
    fn observe_activity(
        &mut self,
        everyone: bool,
        mut touched: std::collections::HashSet<AgentId>,
    ) -> Vec<(AgentId, Option<ObservedActivity>)> {
        if everyone {
            touched.extend(crate::agent::observed_agents(self).cloned());
        }
        let mut changed = Vec::new();
        for agent in touched {
            let activity = crate::agent::activity(self, &agent);
            if self.activity.get(&agent) != activity.as_ref() {
                match &activity {
                    Some(activity) => self.activity.insert(agent.clone(), activity.clone()),
                    None => self.activity.remove(&agent),
                };
                changed.push((agent, activity));
            }
        }
        changed
    }

    /// Seed every agent's activity, as nothing settled reads it yet.
    pub(super) fn seed_activity(&mut self) {
        self.take_dirty();
        self.activity = crate::agent::activities(self);
    }

    /// No wait is left ready and every activity is current: what a change
    /// touched it declared.
    #[cfg(debug_assertions)]
    fn assert_settled(&self) {
        self.jobs.assert_indexed();
        if let Some(keys) = self.waits.missed(self) {
            panic!("missed wakeup: a wait on {keys:?} is ready though nothing it declares changed");
        }
        assert_eq!(
            self.activity,
            crate::agent::activities(self),
            "an agent's activity changed though nothing marked it"
        );
    }

    /// Drop every wait: a closed session decides nothing more.
    pub(super) fn close_waits(&mut self) {
        self.waits.clear();
    }

    /// Stop for `failure` as reopening the session shows it: as `opened` from the
    /// journal's rows, or holding nothing if they could not be read. No agent runs, and
    /// nothing waits or follows from the change that failed. Returns the state it
    /// replaced, whose jobs observers let go of.
    pub(super) fn fail(
        &mut self,
        observed: &mut Observed,
        failure: JournalFailure,
        opened: Result<(Self, Observed), SessionError>,
    ) -> Self {
        let failed = failure.clone();
        (self.agents).stop(|| SessionError::JournalFailed(failed.clone()).into());
        let counters = self.counters;
        let (mut state, reopened) = match opened {
            Ok(opened) => opened,
            // Unread, it holds nothing past the newest record it applied.
            Err(_) => (
                Self {
                    counters,
                    ..Self::default()
                },
                Observed::default(),
            ),
        };
        // Observers read each activity again, ending those it no longer has.
        state.activity = std::mem::take(&mut self.activity);
        (state.activity.keys()).for_each(|agent| state.dirty.agent(agent));
        state.jobs.let_go(&self.jobs);
        state.set_phase(SessionPhase::Failed(failure));
        (observed.applied, observed.views) = (state.counters.newest, reopened.views);
        std::mem::replace(self, state)
    }

    /// The live part of the committed state, beside the views `posted` with it.
    pub(in crate::session) fn snapshot(&self, posted: Posted) -> ObservationSnapshot {
        ObservationSnapshot {
            phase: self.phase.clone(),
            revision: posted.revision,
            applied: posted.applied,
            last_activity: self.last_activity(),
            requests: self.requests.clone(),
            jobs: self.jobs.views(),
            activity: self.activity.clone(),
            views: posted.views,
            usage: (self.agent_states.iter())
                .map(|(agent, state)| (agent.clone(), state.usage))
                .collect(),
        }
    }
}

impl Observed {
    /// Fold an update the journal does not carry.
    pub(in crate::session) fn post(&mut self, event: RuntimeEvent) {
        let applied = self.applied;
        let committed = |sequence| sequence <= applied;
        crate::agent::live(&mut self.views.responses, None, committed, event);
    }

    /// The views as of the newest publication.
    pub(in crate::session) fn posted(&self) -> Posted {
        Posted {
            revision: self.revision,
            applied: self.applied,
            views: self.views.clone(),
        }
    }

    /// The update telling observers `run` captured output, unless they were
    /// told since its output was last read.
    pub(super) fn output_changed(&mut self, Run(job, invocation): Run) -> Option<RuntimeEvent> {
        let told = self.unread.insert(job, invocation) == Some(invocation);
        (!told).then_some(RuntimeEvent::OutputChanged { job, invocation })
    }

    /// `run`'s output is being read: tell observers of what it captures next.
    pub(in crate::session) fn output_read(&mut self, Run(job, invocation): Run) {
        if self.unread.get(&job) == Some(&invocation) {
            self.unread.remove(&job);
        }
    }

    /// The newest record applied: each feed has taken every record through it.
    pub(in crate::session) fn applied(&self) -> RecordSeq {
        self.applied
    }

    /// The newest record applied, and a feed of every record of `agent` applied after
    /// it whose kind `kinds` takes.
    pub(in crate::session) fn subscribe(
        &mut self,
        agent: &AgentId,
        kinds: fn(EntryKind) -> bool,
    ) -> (RecordSeq, std::sync::mpsc::Receiver<Arc<EventRecord>>) {
        let (records, feed) = std::sync::mpsc::channel();
        let feeds = self.feeds.entry(agent.clone()).or_default();
        feeds.push(Feed { kinds, records });
        (self.applied, feed)
    }

    /// Hand `record` to its agent's feeds that take its kind, forgetting those whose
    /// subscriber is gone.
    fn feed(&mut self, record: &Arc<EventRecord>) {
        let Some(feeds) = self.feeds.get_mut(&record.agent) else {
            return;
        };
        let kind = record.event.kind();
        feeds.retain(|feed| !(feed.kinds)(kind) || feed.records.send(Arc::clone(record)).is_ok());
        if feeds.is_empty() {
            self.feeds.remove(&record.agent);
        }
    }

    /// The newest publication's revision.
    pub(in crate::session) fn revision(&self) -> u64 {
        self.revision
    }

    /// The next publication.
    pub(in crate::session) fn publish(&mut self, event: RuntimeEvent) -> ObservedEvent {
        self.revision += 1;
        ObservedEvent {
            revision: self.revision,
            event,
        }
    }
}
