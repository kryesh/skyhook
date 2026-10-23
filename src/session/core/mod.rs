//! The session core: one thread per open session owns the only writer and the
//! state folded from the journal. Commands run on it in order, so an append is
//! validated against the state it applies to, and applied and published before
//! the next command runs. The commands queued together form a group whose commits
//! one sync makes durable: once one of them commits, what they defer and their
//! replies wait for it.
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use chrono::Utc;
use tokio::sync::{broadcast, oneshot};

use super::{
    EventRecord, JournalFailure, RecordSeq, SessionError, SessionEvent, SessionLock, db, request,
};
use crate::{
    agent::{ObservedEvent, RuntimeEvent},
    identity::{AgentId, EventId, JobId, SessionId},
};

mod phase;
pub(crate) use phase::Command;
use phase::Dated;
pub use phase::SessionPhase;
mod state;
pub(crate) use state::CoreState;
pub(super) use state::{Counters, Observed};
mod waits;
pub(crate) use waits::{Dirty, Key, Wait};

/// What a command journals in one transaction, the output it saves in that
/// transaction, and what it changes live in the critical section that applies it.
pub(crate) struct Plan<T> {
    entries: Vec<(AgentId, SessionEvent)>,
    outputs: Vec<Prepared>,
    live: Live<T>,
}

/// A finish's output, prepared for its transaction, or why it could not be: a
/// storage failure or a bug, which stops the session as a failed transaction does.
pub(crate) type Prepared = Result<db::OutputRows, SessionError>;

/// What a plan changes beside its records, given the records it committed. It
/// runs in the critical section that applies them, so it changes only core
/// state, never reads the store, and defers everything else.
type Live<T> = Box<dyn FnOnce(&mut CoreState, &[Arc<EventRecord>]) -> T + Send>;

impl<T> Plan<T> {
    pub(crate) fn new(
        entries: Vec<(AgentId, SessionEvent)>,
        live: impl FnOnce(&mut CoreState, &[Arc<EventRecord>]) -> T + Send + 'static,
    ) -> Self {
        let live = Box::new(live);
        Self {
            entries,
            outputs: Vec::new(),
            live,
        }
    }

    /// The same plan, journaling `entries` and changing the state with `first`
    /// ahead of its own, whose result `map` takes.
    pub(crate) fn after<U: 'static>(
        self,
        mut entries: Vec<(AgentId, SessionEvent)>,
        first: impl FnOnce(&mut CoreState) + Send + 'static,
        map: fn(T) -> U,
    ) -> Plan<U>
    where
        T: 'static,
    {
        let live = self.live;
        let live = move |state: &mut CoreState, records: &[Arc<EventRecord>]| {
            first(state);
            map(live(state, records))
        };
        entries.extend(self.entries);
        Plan::new(entries, live).saving(self.outputs)
    }

    /// Also save `outputs`, the products of the finishes the plan journals.
    pub(crate) fn saving(mut self, outputs: impl IntoIterator<Item = Prepared>) -> Self {
        self.outputs.extend(outputs);
        self
    }
}

impl Plan<()> {
    /// Journal `entries` and nothing more.
    pub(crate) fn journal(entries: Vec<(AgentId, SessionEvent)>) -> Self {
        Self::new(entries, |_, _| ())
    }
}

pub(super) struct Shared {
    pub(super) id: SessionId,
    pub(super) directory: std::path::PathBuf,
    state: RwLock<CoreState>,
    observed: Mutex<Observed>,
    pub(super) events: broadcast::Sender<ObservedEvent>,
    /// Every task the session runs stops once this is cancelled, as when its
    /// journal fails.
    pub(super) root: crate::job::CancellationToken,
}

/// The locks taken after the writer's connection, in this order. A commit holds the
/// connection, then the state, then `observed`, so a thread taking one while it
/// holds a later one can deadlock with it; debug builds check each thread's order.
#[derive(Clone, Copy, Debug)]
enum Order {
    State,
    Observed,
}

#[cfg(debug_assertions)]
thread_local! {
    /// How many locks of each order this thread holds.
    static HELD: std::cell::Cell<[u32; 2]> = const { std::cell::Cell::new([0; 2]) };
}

impl Order {
    /// Count a lock of this order this thread takes, by 1, or lets go of, by -1:
    /// taking one while a later one is held panics.
    #[cfg_attr(not(debug_assertions), allow(unused_variables))]
    fn count(self, by: i32) {
        #[cfg(debug_assertions)]
        HELD.with(|held| {
            let mut counts = held.get();
            let later = counts[self as usize + 1..].iter().any(|n| *n > 0);
            assert!(by < 0 || !later, "{self:?} taken after a later lock");
            counts[self as usize] = counts[self as usize].wrapping_add_signed(by);
            held.set(counts);
        });
    }
}

/// Panic if this thread holds a lock it may take only after the writer's connection.
#[cfg(debug_assertions)]
pub(super) fn assert_connection_first() {
    let held = HELD.with(std::cell::Cell::get);
    assert_eq!(
        held, [0; 2],
        "the connection is taken after the state or `observed`"
    );
}

/// A guard of a lock of one order.
pub(super) struct Ordered<G> {
    guard: G,
    order: Order,
}

impl<G> Ordered<G> {
    fn take(order: Order, lock: impl FnOnce() -> G) -> Self {
        order.count(1);
        let guard = lock();
        Self { guard, order }
    }
}

impl<G> Drop for Ordered<G> {
    fn drop(&mut self) {
        self.order.count(-1);
    }
}

impl<G: std::ops::Deref> std::ops::Deref for Ordered<G> {
    type Target = G::Target;

    fn deref(&self) -> &G::Target {
        &self.guard
    }
}

impl<G: std::ops::DerefMut> std::ops::DerefMut for Ordered<G> {
    fn deref_mut(&mut self) -> &mut G::Target {
        &mut self.guard
    }
}

type StateGuard<'a> = Ordered<std::sync::RwLockWriteGuard<'a, CoreState>>;

/// What the journal's rows reopen the session to.
type Opened = Result<(CoreState, Observed), SessionError>;

/// The state a failed commit still holds, with what its rows reopen to.
type Held<'a> = (StateGuard<'a>, Opened);

/// Why a commit failed, with the state it folded into, still held.
struct Unfolded<'a>(SessionError, Option<StateGuard<'a>>);

impl<E: Into<SessionError>> From<E> for Unfolded<'_> {
    fn from(cause: E) -> Self {
        Self(cause.into(), None)
    }
}

impl Shared {
    pub(super) fn read(&self) -> Ordered<std::sync::RwLockReadGuard<'_, CoreState>> {
        Ordered::take(Order::State, || {
            self.state.read().unwrap_or_else(PoisonError::into_inner)
        })
    }

    pub(super) fn write(&self) -> StateGuard<'_> {
        Ordered::take(Order::State, || {
            self.state.write().unwrap_or_else(PoisonError::into_inner)
        })
    }

    pub(super) fn observed(&self) -> Ordered<std::sync::MutexGuard<'_, Observed>> {
        Ordered::take(Order::Observed, || {
            self.observed.lock().unwrap_or_else(PoisonError::into_inner)
        })
    }

    /// Fold and publish an update the journal does not carry.
    pub(super) fn post(&self, event: RuntimeEvent) {
        let mut observed = self.observed();
        observed.post(event.clone());
        let _ = self.events.send(observed.publish(event));
    }

    /// Publish `Observed::output_changed`'s notice, if any.
    pub(super) fn output_changed(&self, run: crate::job::Run) {
        let mut observed = self.observed();
        if let Some(event) = observed.output_changed(run) {
            let _ = self.events.send(observed.publish(event));
        }
    }
}

type Work = Box<dyn FnOnce(&mut Writer) + Send>;

/// What a group of commands leaves until its sync, in the order they left it.
enum Outgoing {
    Effect(state::Effect),
    /// A command's reply. If the sync that would make what it committed durable
    /// fails, a command that committed reports that failure instead.
    Reply {
        committed: bool,
        send: Box<dyn FnOnce(Option<JournalFailure>) + Send>,
    },
}

/// The core thread and its queue. Dropping the last handle ends the thread
/// once the commands already queued have run, closing the session.
pub(super) struct Core {
    commands: Option<tokio::sync::mpsc::UnboundedSender<Work>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// Queues work on a core it neither keeps running nor ever ends: once the
/// session's handles are gone, what it posts is dropped.
#[derive(Clone)]
pub(crate) struct WeakCore(tokio::sync::mpsc::WeakUnboundedSender<Work>);

impl WeakCore {
    /// Change facts the journal does not record, behind every command queued
    /// before it, if the session is still open.
    pub(crate) fn enqueue(&self, change: impl FnOnce(&mut CoreState) + Send + 'static) {
        if let Some(commands) = self.0.upgrade() {
            let work =
                move |writer: &mut Writer| drop(guarded(writer, |writer| writer.change(change)));
            let _ = commands.send(Box::new(work));
        }
    }
}

/// Updates an observer may fall behind by before it lags.
const PUBLICATION_CAPACITY: usize = 1024;

impl Core {
    /// Start the thread that owns `state` and writes `db`, as the session's `owner`
    /// if it is one.
    pub(super) fn start(
        id: SessionId,
        directory: std::path::PathBuf,
        (mut state, observed): (CoreState, Observed),
        (db, journal): (db::SharedDb, super::Journal),
        owner: Option<Owner>,
    ) -> Result<(Arc<Shared>, Self), SessionError> {
        state.seed_activity();
        let shared = Arc::new(Shared {
            id,
            directory,
            state: RwLock::new(state),
            observed: Mutex::new(observed),
            events: broadcast::channel(PUBLICATION_CAPACITY).0,
            root: crate::job::CancellationToken::new(),
        });
        let mut writer = Writer {
            db,
            journal,
            owner,
            shared: shared.clone(),
            encoder: db::Encoder::default(),
            committed: false,
            commits: 0,
            synced: 0,
            outbox: Vec::new(),
            #[cfg(test)]
            fail_committed: false,
            #[cfg(test)]
            syncs: 0,
        };
        if writer.owner.is_some() {
            // Nothing here needs a sync: what a crash undoes, the next open removes again.
            let removed = writer.db.write(db::remove_staged);
            removed.map_err(db::DbError::from)?;
        }
        // What a command drops, as an abandoned job lease, may spawn its cleanup.
        let runtime = tokio::runtime::Handle::current();
        let (commands, mut queue) = tokio::sync::mpsc::unbounded_channel::<Work>();
        let thread = std::thread::Builder::new()
            .name(format!("session-{id}"))
            .spawn(move || {
                // Waiting on the queue is outside the runtime; its commands run inside.
                while let Some(command) = queue.blocking_recv() {
                    let _runtime = runtime.enter();
                    // What is already queued behind it joins its group; nothing waits
                    // for more.
                    let queued = queue.len();
                    let behind = std::iter::from_fn(|| queue.try_recv().ok()).take(queued);
                    for command in std::iter::once(command).chain(behind) {
                        command(&mut writer);
                        // Until the group commits, what it leaves waits for no sync.
                        if writer.synced == writer.commits {
                            writer.flush();
                        }
                    }
                    writer.flush();
                }
                let _runtime = runtime.enter();
                // Only the session's owner closes it; a replica shares its connection.
                if writer.owner.is_some() {
                    let _ = writer.close();
                }
                // However the session ended, closed or dropped: a failed check fails
                // what dropped the store.
                #[cfg(debug_assertions)]
                writer.assert_folded();
            })?;
        let core = Self {
            commands: Some(commands),
            thread: Some(thread),
        };
        Ok((shared, core))
    }

    /// Run `work` on the core thread after every command queued before it, and
    /// reply once what its group committed is durable. Once queued it runs to the
    /// end: dropping this future, with its caller or its runtime, loses only the
    /// reply. A panic in `work` stops the session.
    pub(super) async fn call<T: Send + 'static>(
        &self,
        work: impl FnOnce(&mut Writer) -> T + Send + 'static,
    ) -> Result<T, SessionError> {
        let (reply, result) = oneshot::channel();
        let commands = self.commands.as_ref().expect("open until dropped");
        // The thread outlives every handle, so the queue is open.
        let _ = commands.send(Box::new(move |writer: &mut Writer| {
            let commits = writer.commits;
            let value = guarded(writer, work);
            let committed = writer.commits != commits;
            let send = move |failure: Option<JournalFailure>| {
                let value =
                    failure.map_or(value, |failure| Err(SessionError::JournalFailed(failure)));
                drop(reply.send(value));
            };
            let send = Box::new(send);
            writer.outbox.push(Outgoing::Reply { committed, send });
        }));
        result.await.unwrap_or(Err(SessionError::WriterLost))
    }

    pub(super) fn downgrade(&self) -> WeakCore {
        let commands = self.commands.as_ref().expect("open until dropped");
        WeakCore(commands.downgrade())
    }

    /// Queue `work` behind every command queued before it, without waiting for it.
    pub(super) fn post(&self, work: impl FnOnce(&mut Writer) + Send + 'static) {
        let commands = self.commands.as_ref().expect("open until dropped");
        let _ = commands.send(Box::new(move |writer| drop(guarded(writer, work))));
    }
}

/// Run `work`; a panic in it stops the session.
fn guarded<T>(writer: &mut Writer, work: impl FnOnce(&mut Writer) -> T) -> Result<T, SessionError> {
    let run = std::panic::AssertUnwindSafe(|| work(&mut *writer));
    std::panic::catch_unwind(run).map_err(|_| writer.lost())
}

impl Drop for Core {
    fn drop(&mut self) {
        drop(self.commands.take());
        // The last handle can drop inside a command, on the thread itself.
        if let Some(thread) = self.thread.take()
            && thread.thread().id() != std::thread::current().id()
        {
            // A check that fails as the session closes fails what closed it.
            if let Err(panic) = thread.join()
                && cfg!(debug_assertions)
                && !std::thread::panicking()
            {
                std::panic::resume_unwind(panic);
            }
        }
    }
}

/// What the one store that owns a durable session holds: its lock, and the
/// checkpointer of its log.
pub(super) struct Owner {
    pub(super) lock: SessionLock,
    pub(super) checkpointer: db::Checkpointer,
}

/// The only read-write connection, owned by the core thread.
pub(super) struct Writer {
    db: db::SharedDb,
    /// What the journal committed, read without the connection output writers share.
    journal: super::Journal,
    /// Until it releases the session, if it owns it.
    pub(super) owner: Option<Owner>,
    pub(super) shared: Arc<Shared>,
    encoder: db::Encoder,
    /// Whether the running command committed, until it has applied it.
    committed: bool,
    /// Transactions committed, and how many of them the last sync made durable.
    commits: u64,
    synced: u64,
    /// What the commands run since the last sync left for after it.
    outbox: Vec<Outgoing>,
    /// Test fault: the next commit fails once it committed.
    #[cfg(test)]
    pub(super) fail_committed: bool,
    /// How many syncs the writer ran.
    #[cfg(test)]
    pub(super) syncs: u64,
}

impl Writer {
    /// Run `plan` against the current state, if its phase admits `command`, and
    /// commit what it plans, dated and moving the phase as `command` does. A
    /// command that closes the session releases it once it applies.
    pub(super) fn run<T, E: From<SessionError>>(
        &mut self,
        command: Command,
        plan: impl FnOnce(&CoreState) -> Result<Plan<T>, E>,
    ) -> Result<T, E> {
        let state = self.shared.read();
        let (phase, dated) = state.phase().admits(command)?;
        let Plan {
            entries,
            outputs,
            live,
        } = plan(&state)?;
        drop(state);
        let closes = phase == SessionPhase::Closed;
        let live = move |state: &mut CoreState, records: &[Arc<EventRecord>]| {
            state.set_phase(phase);
            if closes {
                state.close_waits();
            }
            live(state, records)
        };
        let value = self.commit(entries, outputs, dated, live)?;
        if closes {
            self.release()?;
        }
        Ok(value)
    }

    /// Journal `entries` with `outputs` in one transaction, validating each record
    /// against the journal ahead of it and folding it into the state, whose changed
    /// job, request and agent rows it writes, before COMMIT; then publish them and apply
    /// `live` in the critical section the fold began. A transaction the journal cannot
    /// take stops the session.
    fn commit<T>(
        &mut self,
        entries: Vec<(AgentId, SessionEvent)>,
        outputs: Vec<Prepared>,
        dated: Dated,
        live: impl FnOnce(&mut CoreState, &[Arc<EventRecord>]) -> T,
    ) -> Result<T, SessionError> {
        if entries.is_empty() {
            return Ok(self.change(|state| live(state, &[])));
        }
        let addressed: Vec<_> = self.shared.read().jobs.addressed(&entries).collect();
        if let Err(cause) = self.hold(addressed) {
            return Err(self.fail(cause));
        }
        let (records, outputs) = match self.accept(entries, outputs, dated) {
            Ok(accepted) => accepted,
            Err(cause) => return Err(self.fail(cause)),
        };
        let shared = Arc::clone(&self.shared);
        let (db, encoder) = (self.db.clone(), &mut self.encoder);
        let work = |db: &db::Db| {
            (outputs.into_iter()).try_for_each(|output| output.insert(db))?;
            encoder.start(db, &records)?;
            for record in &records {
                request::validate(db, record)?;
                encoder.record(db, record)?;
            }
            // Held through COMMIT, or through the rollback and what its rows reopen to,
            // so no reader sees the state ahead of the journal.
            let mut state = shared.write();
            let mut observed = shared.observed();
            let mut fold = || {
                let mut views = Vec::with_capacity(records.len());
                for record in &records {
                    // A record arrives with the job it moved already as it leaves it.
                    let job = state.apply(&mut observed, record)?;
                    views.push(job.and_then(|job| state.jobs.view(job)));
                }
                db::write_status(db, &state.jobs.take_changed())?;
                let folded = &mut *state;
                let rows = folded.agent_states.take_unwritten();
                encoder.write_rows(db, &rows, &folded.requests)?;
                Ok::<_, SessionError>(views)
            };
            match fold() {
                Ok(views) => Ok((state, observed, views)),
                Err(cause) => Err(Unfolded(cause, Some(state))),
            }
        };
        let written = db.write_undoing(work, |db, failed| {
            let (cause, state) = match failed {
                Ok(((state, ..), cause)) => (SessionError::Unconfirmed(Some(cause)), Some(state)),
                Err(Unfolded(cause, state)) => (cause, state),
            };
            let _ = db.close_writes();
            let held = state.map(|state| (state, super::open_state(db, shared.id)));
            (cause, held)
        });
        let (state, mut observed, views) = match written {
            Ok(written) => written,
            Err((cause, held)) => return Err(self.fail_holding(cause, held)),
        };
        self.commits += 1;
        self.committed = true;
        #[cfg(test)]
        if std::mem::take(&mut self.fail_committed) {
            drop((state, observed));
            return Err(self.fail(SessionError::Unconfirmed(None)));
        }
        let value = self.changed(state, |state| {
            for (record, view) in records.iter().zip(views) {
                state.applied(&mut observed, record);
                let events = view.map(|view| RuntimeEvent::Job(Box::new(view)));
                let record = RuntimeEvent::Record(Arc::clone(record));
                for event in events.into_iter().chain([record]) {
                    let _ = shared.events.send(observed.publish(event));
                }
            }
            drop(observed);
            live(state, &records)
        });
        self.committed = false;
        Ok(value)
    }

    /// Hold those of `jobs` that were created, with their ancestors, as the journal
    /// leaves them, for a command, a wait or the records it commits to read them by
    /// id; the sweep after the next change lets go of any nothing keeps. Only this
    /// thread changes the state, so what the journal is read for stays unheld until
    /// it is held, and no lock is held across the read: under one, an in-memory
    /// session's read through its writer's connection could wait on a holder of
    /// that connection waiting on the lock.
    pub(super) fn hold(&self, jobs: Vec<JobId>) -> Result<(), SessionError> {
        let mut unheld = self.shared.read().jobs.unheld(jobs);
        if unheld.is_empty() {
            return Ok(());
        }
        let mut settled = crate::job::Settled::default();
        while !unheld.is_empty() {
            let parents = settled.merge(crate::job::Settled::read(&self.journal, &unheld)?);
            unheld = self.shared.read().jobs.unheld(parents);
        }
        self.shared.write().restore(settled);
        Ok(())
    }

    /// Change the state in one critical section that also resolves the waits it
    /// decides and publishes the phase it moves to, leaving the effects it deferred
    /// to run, in order, once the group's commits are durable.
    pub(super) fn change<T>(&mut self, change: impl FnOnce(&mut CoreState) -> T) -> T {
        let shared = Arc::clone(&self.shared);
        self.changed(shared.write(), change)
    }

    /// [`Writer::change`], in the critical section `state` holds.
    fn changed<T>(
        &mut self,
        mut state: StateGuard<'_>,
        change: impl FnOnce(&mut CoreState) -> T,
    ) -> T {
        let phase = state.phase().clone();
        let value = change(&mut state);
        let (effects, activity) = state.settle();
        debug_assert!(
            (state.jobs.written() && state.agent_states.written())
                || state.phase().failure().is_some(),
            "a job's, request's or agent's state changed outside the fold of its records"
        );
        let moved = (state.phase() != &phase).then(|| RuntimeEvent::Phase(state.phase().clone()));
        let activity = (activity.into_iter())
            .map(|(agent, activity)| RuntimeEvent::Activity { agent, activity });
        // Observers let go of the jobs the sweep evicted, whose rows are final.
        let unheld = Some(state.jobs.take_evicted()).filter(|jobs| !jobs.is_empty());
        (activity.chain(moved))
            .chain(unheld.map(RuntimeEvent::JobsUnheld))
            .for_each(|event| self.shared.post(event));
        drop(state);
        self.outbox
            .extend(effects.into_iter().map(Outgoing::Effect));
        value
    }

    /// Commit `work` in a transaction of its own, which the group's sync makes
    /// durable. A transaction the journal cannot take stops the session.
    pub(super) fn write<T>(
        &mut self,
        work: impl FnOnce(&db::Db, &mut db::Encoder) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        let encoder = &mut self.encoder;
        match self.db.write(|db| work(db, encoder)) {
            Ok(value) => {
                self.commits += 1;
                Ok(value)
            }
            Err(db::WriteError::Refused(cause)) => Err(self.fail(cause)),
            Err(db::WriteError::Unconfirmed(cause)) => {
                Err(self.fail(SessionError::Unconfirmed(Some(cause))))
            }
        }
    }

    /// End the group: make what it committed durable, then run what it deferred and
    /// send its replies, in order. A sync that fails stops the session, and so does
    /// an effect that panics; the replies of what committed then report the failure,
    /// and nothing more the group deferred runs.
    pub(super) fn flush(&mut self) {
        let mut failure = self.sync().err().map(|cause| self.stop(cause, None));
        for outgoing in std::mem::take(&mut self.outbox) {
            match outgoing {
                Outgoing::Effect(effect) if failure.is_none() => {
                    let run = std::panic::AssertUnwindSafe(effect);
                    if std::panic::catch_unwind(run).is_err() {
                        failure = Some(self.stop(SessionError::WriterLost, None));
                    }
                }
                Outgoing::Effect(_) => {}
                Outgoing::Reply { committed, send } => send(failure.clone().filter(|_| committed)),
            }
        }
    }

    /// Make what committed since the last sync durable.
    fn sync(&mut self) -> Result<(), SessionError> {
        if self.synced == self.commits {
            return Ok(());
        }
        self.synced = self.commits;
        #[cfg(test)]
        {
            self.syncs += 1;
        }
        let synced = self.db.sync();
        synced.map_err(|source| SessionError::Unconfirmed(Some(source.into())))
    }

    /// What a command that panicked leaves: past its COMMIT, its own append is
    /// indeterminate; before it, nothing durable, as the connection's next holder
    /// rolls back the transaction it left open. Either stops the session.
    fn lost(&mut self) -> SessionError {
        let cause = match std::mem::take(&mut self.committed) {
            true => SessionError::Unconfirmed(None),
            false => SessionError::WriterLost,
        };
        self.fail(cause)
    }

    /// Stop the session for `cause`, unless an earlier failure already did, as a
    /// crash stops it: the connection takes no more writes, the session's work is
    /// cancelled and no agent runs, and every wait and reply resolves with the
    /// failure, which every later write reports too. Its state is what reopening it
    /// shows. What the group committed before it stands, durable, with what that
    /// deferred.
    pub(super) fn fail(&mut self, cause: SessionError) -> SessionError {
        self.fail_holding(cause, None)
    }

    /// [`fail`](Self::fail), where `held` is the state a failed commit still holds,
    /// with what the rows it rolled back reopen to.
    fn fail_holding(&mut self, cause: SessionError, held: Option<Held<'_>>) -> SessionError {
        self.committed = false;
        let failure = self.stop(cause, held);
        self.flush();
        SessionError::JournalFailed(failure)
    }

    /// Stop the session for `cause`, unless an earlier failure already did.
    fn stop(&mut self, cause: SessionError, held: Option<Held<'_>>) -> JournalFailure {
        self.encoder.reset();
        let shared = Arc::clone(&self.shared);
        let (state, opened) = match held {
            Some(held) => held,
            None => {
                let db = self.db.lock();
                let _ = db.close_writes();
                if let Some(failure) = shared.read().phase().failure().cloned() {
                    return failure;
                }
                let opened = super::open_state(&db, shared.id);
                drop(db);
                (shared.write(), opened)
            }
        };
        let (failure, replaced) = self.changed(state, |state| {
            if let Some(failure) = state.phase().failure() {
                return (failure.clone(), None);
            }
            let failure = JournalFailure(Arc::new(cause));
            let replaced = state.fail(&mut shared.observed(), failure.clone(), opened);
            shared.root.cancel();
            (failure, Some(replaced))
        });
        drop(replaced);
        failure
    }

    /// Date `entries`, with the output they save; any error is a definite rejection.
    fn accept(
        &self,
        entries: Vec<(AgentId, SessionEvent)>,
        outputs: Vec<Prepared>,
        dated: Dated,
    ) -> Result<(Vec<Arc<EventRecord>>, Vec<db::OutputRows>), SessionError> {
        let outputs = outputs.into_iter().collect::<Result<Vec<_>, _>>()?;
        let state = self.shared.read();
        let first = state.next_sequence();
        let now = || Utc::now().timestamp_millis();
        let timestamp_millis = match dated {
            Dated::Now => now(),
            Dated::LastActivity => state.last_activity().unwrap_or_else(now),
        };
        let mut records = Vec::with_capacity(entries.len());
        // A definition that differs from its name's latest edition records a new one.
        // Appends are serialized here, so agents applying one at once record it once.
        let mut editions = std::collections::HashMap::new();
        for (offset, (agent, mut event)) in entries.into_iter().enumerate() {
            if agent.session() != self.shared.id {
                return Err(SessionError::WrongSession);
            }
            if let SessionEvent::AgentStarted {
                mode: Some(mode), ..
            }
            | SessionEvent::ModeChanged { mode, .. } = &mut event
                && let Some(definition) = &mode.definition
            {
                let latest =
                    (editions.get(&mode.name)).or_else(|| state.agents.edition(&mode.name));
                if latest == Some(definition) {
                    mode.definition = None;
                } else {
                    editions.insert(mode.name.clone(), definition.clone());
                }
            }
            records.push(Arc::new(EventRecord {
                id: EventId::generate()?,
                sequence: RecordSeq::new(first.get().saturating_add(offset as u64)),
                timestamp_millis,
                agent,
                event,
            }));
        }
        Ok((records, outputs))
    }

    /// Stop admission and release the session for another open; closing a
    /// closed one does nothing. A failed session is released too, and reports
    /// its failure.
    pub(super) fn close(&mut self) -> Result<(), SessionError> {
        let nothing = |_: &CoreState| Ok::<_, SessionError>(Plan::new(Vec::new(), |_, _| ()));
        self.run(Command::Close, nothing)
            .inspect_err(|_| drop(self.release()))
    }

    /// Take no more writes and release the session for another open, once what it
    /// committed is durable and, owning the session, in the database file, then
    /// report the failure that stopped it, if one did. Its owner removes a session
    /// that journaled nothing.
    fn release(&mut self) -> Result<(), SessionError> {
        self.flush();
        let lock = (self.owner.take()).map(|Owner { lock, checkpointer }| {
            checkpointer.close(&self.db);
            lock
        });
        // Taking the connection rolls back a transaction a panicked command left
        // open, which would otherwise keep the next owner from writing. Only the
        // database knows whether an append whose outcome was lost committed.
        let frozen = self.db.lock().close_writes();
        let entry =
            (self.db.lock()).query_row("SELECT 1 FROM entry LIMIT 1", Vec::new(), |_| Ok(()));
        if let Some(_lock) = lock
            && let Ok(None) = entry
        {
            let _ = std::fs::remove_dir_all(&self.shared.directory);
        }
        frozen.map_err(|cause| self.fail(cause.into()))?;
        self.shared.read().phase().failed()
    }

    /// Panic unless the job, request and agent rows hold what a fold of every record
    /// leaves, and open from them as that fold would.
    #[cfg(debug_assertions)]
    fn assert_folded(&self) {
        let db = self.db.lock();
        let id = self.shared.id;
        let jobs = db.query("SELECT id FROM job", Vec::new(), |row| {
            Ok(JobId::new(db::u64_of(row.get(0)?)).expect("a job id"))
        });
        let records = db::decode_records(&db, id, db::Entries::All);
        // A database a test corrupted reads as nothing to compare.
        if let (Ok(jobs), Ok(records)) = (jobs, records) {
            let rows = db::job_rows(&db, id, &jobs).expect("job rows read");
            crate::job::assert_folded(rows, &records);
            let mut ledger = crate::session::ResidentLedger::default();
            let mut agents = crate::session::agent_state::AgentStates::default();
            for record in &records {
                agents.apply(record, &mut ledger);
            }
            let crate::session::agent_state::Rows {
                requests,
                mut decided,
                ..
            } = agents.take_unwritten();
            let mut written = db::decided(&db).expect("decided rows read");
            decided.sort_unstable();
            written.sort_unstable();
            assert_eq!(written, decided, "decided rows differ from the fold");
            let mut read = std::collections::BTreeMap::new();
            let paths = db::agent_paths(&db, id, db::Scope::All).expect("agents read");
            for agent in paths.values() {
                let each = |seq, request| drop(read.insert(seq, request));
                db::stats::each_request(&db, agent, each).expect("request rows read");
            }
            assert_eq!(read, requests, "request rows differ from the fold");
            let opened = db::opening(&db, id).expect("rows open");
            let folded = agents
                .iter()
                .map(|(agent, state)| (agent.clone(), state.clone()));
            let folded: std::collections::HashMap<_, _> = folded.collect();
            assert_eq!(opened.agents, folded, "agent rows differ from the fold");
            assert_eq!(opened.ledger, ledger, "rows open another ledger");
        }
    }
}
