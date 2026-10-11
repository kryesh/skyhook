//! The session core: one thread per open session owns the only writer and the
//! state folded from the journal. Commands run on it in order, so an append is
//! validated against the state it applies to, and applied and published before
//! the next command runs.
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use chrono::Utc;
use tokio::sync::{broadcast, oneshot};

use super::{
    EventRecord, JournalFailure, RecordSeq, SessionError, SessionEvent, SessionLock, db,
    mode_editions, request,
};
#[cfg(test)]
use super::{Held, Hold};
use crate::{
    agent::{ObservedEvent, RuntimeEvent},
    identity::{AgentId, EventId, SessionId},
};

mod phase;
pub(crate) use phase::Command;
use phase::Dated;
pub use phase::SessionPhase;
mod state;
pub(crate) use state::CoreState;
pub(super) use state::Observed;

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
/// runs in the critical section that applies them, so it changes only core state
/// and the supervisor's handles, which follow the core's state in lock order,
/// never reads the store, and defers everything else.
type Live<T> = Box<dyn FnOnce(&mut CoreState, &[EventRecord]) -> T + Send>;

impl<T> Plan<T> {
    pub(crate) fn new(
        entries: Vec<(AgentId, SessionEvent)>,
        live: impl FnOnce(&mut CoreState, &[EventRecord]) -> T + Send + 'static,
    ) -> Self {
        let live = Box::new(live);
        Self {
            entries,
            outputs: Vec::new(),
            live,
        }
    }

    /// Also save `outputs`, the products of the finishes the plan journals.
    pub(crate) fn saving(mut self, outputs: impl IntoIterator<Item = Prepared>) -> Self {
        self.outputs.extend(outputs);
        self
    }
}

impl<T: 'static> Plan<T> {
    /// Also apply `change`, after what the plan itself changes.
    pub(crate) fn then(self, change: impl FnOnce(&mut CoreState) + Send + 'static) -> Self {
        let live = self.live;
        let plan = Self::new(self.entries, move |state, records| {
            let value = live(state, records);
            change(state);
            value
        });
        plan.saving(self.outputs)
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
    /// Taken after `state`, never before it.
    observed: Mutex<Observed>,
    pub(super) events: broadcast::Sender<ObservedEvent>,
    /// Every task the session runs stops once this is cancelled, as when its
    /// journal fails.
    pub(super) root: crate::job::CancellationToken,
}

impl Shared {
    pub(super) fn read(&self) -> std::sync::RwLockReadGuard<'_, CoreState> {
        self.state.read().unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn write(&self) -> std::sync::RwLockWriteGuard<'_, CoreState> {
        self.state.write().unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn observed(&self) -> std::sync::MutexGuard<'_, Observed> {
        self.observed.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

type Work = Box<dyn FnOnce(&mut Writer) + Send>;

/// The core thread and its queue. Dropping the last handle ends the thread
/// once the commands already queued have run, closing the session.
pub(super) struct Core {
    commands: Option<std::sync::mpsc::Sender<Work>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// Updates an observer may fall behind by before it lags.
const PUBLICATION_CAPACITY: usize = 1024;

impl Core {
    /// Start the thread that owns `state` and writes `db`, holding `lock`.
    pub(super) fn start(
        id: SessionId,
        directory: std::path::PathBuf,
        (state, observed): (CoreState, Observed),
        db: db::SharedDb,
        lock: Option<SessionLock>,
    ) -> Result<(Arc<Shared>, Self), SessionError> {
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
            lock,
            shared: shared.clone(),
            encoder: db::Encoder::default(),
            committed: false,
            #[cfg(test)]
            hold: None,
        };
        // What a command drops, as an abandoned job lease, may spawn its cleanup.
        let runtime = tokio::runtime::Handle::current();
        let (commands, queue) = std::sync::mpsc::channel::<Work>();
        let thread = std::thread::Builder::new()
            .name(format!("session-{id}"))
            .spawn(move || {
                let _runtime = runtime.enter();
                for command in queue {
                    command(&mut writer);
                }
                // Only the session's owner closes it; a replica shares its connection.
                if writer.lock.is_some() {
                    let _ = writer.close();
                }
            })?;
        let core = Self {
            commands: Some(commands),
            thread: Some(thread),
        };
        Ok((shared, core))
    }

    /// Run `work` on the core thread after every command queued before it. Once
    /// queued it runs to the end: dropping this future, with its caller or its
    /// runtime, loses only the reply. A panic in `work` stops the session.
    pub(super) async fn call<T: Send + 'static>(
        &self,
        work: impl FnOnce(&mut Writer) -> T + Send + 'static,
    ) -> Result<T, SessionError> {
        let (reply, result) = oneshot::channel();
        let commands = self.commands.as_ref().expect("open until dropped");
        // The thread outlives every handle, so the queue is open.
        let _ = commands.send(Box::new(move |writer| {
            drop(reply.send(guarded(writer, work)))
        }));
        result.await.unwrap_or(Err(SessionError::WriterLost))
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
            let _ = thread.join();
        }
    }
}

/// The only read-write connection, owned by the core thread.
pub(super) struct Writer {
    pub(super) db: db::SharedDb,
    pub(super) lock: Option<SessionLock>,
    pub(super) shared: Arc<Shared>,
    encoder: db::Encoder,
    /// Whether the running command committed, until it has applied it.
    committed: bool,
    #[cfg(test)]
    pub(super) hold: Option<Hold>,
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
        let (phase, dated) = state.phase.admits(command)?;
        let Plan {
            entries,
            outputs,
            live,
        } = plan(&state)?;
        drop(state);
        let closes = phase == SessionPhase::Closed;
        let live = move |state: &mut CoreState, records: &[EventRecord]| {
            state.phase = phase;
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

    /// Journal `entries` with `outputs` in one transaction, then apply them and
    /// `live` in the one critical section that publishes them. A transaction the
    /// journal cannot take stops the session.
    fn commit<T>(
        &mut self,
        entries: Vec<(AgentId, SessionEvent)>,
        outputs: Vec<Prepared>,
        dated: Dated,
        live: impl FnOnce(&mut CoreState, &[EventRecord]) -> T,
    ) -> Result<T, SessionError> {
        if entries.is_empty() {
            return Ok(self.change(|state| live(state, &[])));
        }
        // Output writers share the connection; hold it from BEGIN through COMMIT
        // only. No other commit can begin before this one applies, so their
        // writes may interleave with the apply.
        let connection = self.db.clone();
        let db = connection.lock();
        let records = match self.accept(&db, entries, outputs, dated) {
            Ok(records) => records,
            Err(cause) => {
                drop(db);
                return Err(self.fail(cause));
            }
        };
        if let Err(source) = db.commit() {
            let _ = db.rollback();
            drop(db);
            return Err(self.fail(SessionError::Unconfirmed(Some(source))));
        }
        self.committed = true;
        drop(db);
        #[cfg(test)]
        if let Some(failed) = self.held(&records) {
            return Err(failed);
        }
        let value = self.change(|state| {
            let mut observed = self.shared.observed();
            for record in &records {
                // A record arrives with the job it moved already as it leaves it.
                let job = state.apply(&mut observed, record);
                let view = job.and_then(|job| state.jobs.view(job));
                let events = view.map(|view| RuntimeEvent::Job(Box::new(view)));
                let record = RuntimeEvent::Record(Box::new(record.clone()));
                for event in events.into_iter().chain([record]) {
                    let _ = self.shared.events.send(observed.publish(event));
                }
            }
            drop(observed);
            live(state, &records)
        });
        self.committed = false;
        Ok(value)
    }

    /// Change the state in one critical section that also resolves the waits it
    /// decides and publishes the phase it moves to, then run the effects it
    /// deferred, in order.
    pub(super) fn change<T>(&self, change: impl FnOnce(&mut CoreState) -> T) -> T {
        let mut state = self.shared.write();
        let (phase, before) = (state.phase.clone(), crate::agent::activities(&state));
        let value = change(&mut state);
        let effects = state.settle();
        let moved = (state.phase != phase).then(|| RuntimeEvent::Phase(state.phase.clone()));
        let after = crate::agent::activities(&state);
        let activity = (after.into_iter())
            .filter(|(agent, activity)| before.get(agent) != Some(activity))
            .map(|(agent, activity)| RuntimeEvent::Activity { agent, activity });
        for event in activity.chain(moved) {
            let mut observed = self.shared.observed();
            observed.post(event.clone());
            let _ = self.shared.events.send(observed.publish(event));
        }
        drop(state);
        effects.into_iter().for_each(|effect| effect());
        value
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
    /// failure, which every later write reports too.
    pub(super) fn fail(&mut self, cause: SessionError) -> SessionError {
        self.committed = false;
        self.encoder.reset();
        let _ = self.db.lock().close_writes();
        let root = self.shared.root.clone();
        let failure = self.change(move |state| {
            if let Some(failure) = state.phase.failure() {
                return failure.clone();
            }
            let failure = JournalFailure(Arc::new(cause));
            state.fail(failure.clone());
            root.cancel();
            failure
        });
        SessionError::JournalFailed(failure)
    }

    #[cfg(test)]
    fn held(&mut self, records: &[EventRecord]) -> Option<SessionError> {
        let held = |Hold(matches, _): &mut Hold| records.iter().any(|r| matches(&r.event));
        match self.hold.take_if(held)? {
            Hold(_, Held::Pause(reached, resume)) => {
                let _ = reached.send(());
                let _ = resume.blocking_recv();
                None
            }
            Hold(_, Held::Fail) => Some(self.fail(SessionError::Unconfirmed(None))),
        }
    }

    /// Validate and encode inside an open transaction, with the output it saves;
    /// any error is a definite rejection.
    fn accept(
        &mut self,
        db: &db::Db,
        entries: Vec<(AgentId, SessionEvent)>,
        outputs: Vec<Prepared>,
        dated: Dated,
    ) -> Result<Vec<EventRecord>, SessionError> {
        let outputs = outputs.into_iter().collect::<Result<Vec<_>, _>>()?;
        let state = self.shared.read();
        let first = state.next_sequence();
        let now = || Utc::now().timestamp_millis();
        let timestamp_millis = match dated {
            Dated::Now => now(),
            Dated::LastActivity => EventRecord::last_activity(&state.records).unwrap_or_else(now),
        };
        let mut records = Vec::with_capacity(entries.len());
        // A definition that differs from its name's latest edition records a new one.
        // Appends are serialized here, so agents applying one at once record it once.
        let mut editions: Option<
            std::collections::HashMap<crate::tool::policy::ModeName, crate::tool::policy::Mode>,
        > = None;
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
                let editions = editions.get_or_insert_with(|| {
                    let recorded = mode_editions(&state.records);
                    recorded
                        .map(|(name, mode)| (name.clone(), mode.clone()))
                        .collect()
                });
                if editions.get(&mode.name) == Some(definition) {
                    mode.definition = None;
                } else {
                    editions.insert(mode.name.clone(), definition.clone());
                }
            }
            records.push(EventRecord {
                id: EventId::generate()?,
                sequence: RecordSeq::new(first.get().saturating_add(offset as u64)),
                timestamp_millis,
                agent,
                event,
            });
        }
        // Order-dependent rules validate against the committed prefix plus this batch.
        for (index, record) in records.iter().enumerate() {
            request::validate(&state.records, &records[..index], record)?;
        }
        state.admit(&records)?;
        drop(state);
        let encoder = &mut self.encoder;
        let result = db.transaction(|| {
            encoder.records(db, &records)?;
            outputs.into_iter().try_for_each(|output| output.insert(db))
        });
        if result.is_err() {
            self.encoder.reset();
        }
        result?;
        Ok(records)
    }

    /// Stop admission and release the session for another open; closing a
    /// closed one does nothing. A failed session is released too, and reports
    /// its failure.
    pub(super) fn close(&mut self) -> Result<(), SessionError> {
        let nothing = |_: &CoreState| Ok::<_, SessionError>(Plan::new(Vec::new(), |_, _| ()));
        self.run(Command::Close, nothing)
            .inspect_err(|_| drop(self.release()))
    }

    /// Take no more writes and release the session for another open. Its owner
    /// removes a session that journaled nothing.
    fn release(&mut self) -> Result<(), SessionError> {
        // Taking the connection rolls back a transaction a panicked command left
        // open, which would otherwise keep the next owner from writing. Only the
        // database knows whether an append whose outcome was lost committed.
        let frozen = self.db.lock().close_writes();
        let entry =
            (self.db.lock()).query_row("SELECT 1 FROM entry LIMIT 1", Vec::new(), |_| Ok(()));
        if let Some(_lock) = self.lock.take()
            && let Ok(None) = entry
        {
            let _ = std::fs::remove_dir_all(&self.shared.directory);
        }
        frozen.map_err(|cause| self.fail(cause.into()))
    }
}
