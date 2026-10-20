//! Session persistence in one normalized SQLite database per session.
use std::{
    fs::OpenOptions as StdOpenOptions,
    path::{Path, PathBuf},
    sync::Arc,
};
use thiserror::Error;

use crate::{
    blocking,
    identity::{AgentId, JobId, SessionId},
    provider::protocol::ModelRequest,
};

pub(crate) mod agent_state;
mod content;
mod core;
mod db;
mod event;
pub(crate) mod ledger;
mod pages;
mod projection;
mod request;
pub mod stats;

pub use content::{JobEvent, Message, RuntimeState, StateJob, StateJobKind, UserPart};
pub use core::SessionPhase;

pub use agent_state::Turns;
pub(crate) use core::{Command, CoreState, Dirty, Key, Plan, Prepared, Wait, WeakCore};
pub(crate) use db::{
    AgentRecords, CaptureExtent, CaptureRow, JobRow, OutputRows, Reads, SavedRows, SharedDb,
    StagedCapture,
};
pub use db::{DbError, SessionSummary, SessionTitle};
pub(crate) use event::EntryKind;
pub use event::{
    AttemptFailure, AttemptRef, CompactionCheckpoint, CompactionFailure, CompactionTrigger,
    CompletedOutcome, EventRecord, MessageSeq, ModeSelection, ModelCallOrigin, ModelContext,
    ModelPurpose, ProfileSnapshot, RecordSeq, Refused, RequestSeq, RetryDelay, SessionEvent,
    TitleSource, Truncation,
};
pub use ledger::{Fact, RequestFailure, RequestPhase, RequestRecord, ResidentLedger};
pub use pages::{AgentSummary, AgentsPage, Answer, Page, PageFacts, TranscriptPage};
pub(crate) use projection::{CompletedRequest, OpenedHistory, Requested};
pub use projection::{Projected, Projection, render_history};
use request::Referenced;
pub use request::{CheckpointError, ReplayError};

const DATABASE_FILE: &str = "session.db";
const LOCK_FILE: &str = "lock";

struct SessionLock(std::fs::File);

impl Drop for SessionLock {
    fn drop(&mut self) {
        // Ownership ends with the store, not with a transient fork-inherited fd.
        let _ = self.0.unlock();
    }
}

/// How long an open waits before its one retry of a held lock: `is_open` holds
/// it shared for an instant, which must not make an open fail.
const LOCK_RETRY: std::time::Duration = std::time::Duration::from_millis(50);

fn lock_session(directory: &Path, id: SessionId) -> Result<SessionLock, SessionError> {
    let lock = StdOpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(directory.join(LOCK_FILE))?;
    lock.try_lock()
        .or_else(|_| {
            std::thread::sleep(LOCK_RETRY);
            lock.try_lock()
        })
        .map_err(|_| SessionError::AlreadyOpen(id))?;
    Ok(SessionLock(lock))
}

struct StoreInner {
    shared: Arc<core::Shared>,
    db: SharedDb,
    journal: Journal,
    core: core::Core,
}

#[derive(Clone)]
pub struct SessionStore {
    inner: Arc<StoreInner>,
}

/// Tells a session's observers when a run's captured output changes; see
/// [`crate::agent::RuntimeEvent::OutputChanged`]. It keeps nothing of the
/// session alive.
#[derive(Clone)]
pub(crate) struct OutputChanges(std::sync::Weak<core::Shared>);

impl OutputChanges {
    /// `run` captured output that is now readable.
    pub(crate) fn changed(&self, run: crate::job::Run) {
        if let Some(shared) = self.0.upgrade() {
            shared.output_changed(run);
        }
    }

    /// `run`'s output is being read: what it captures from now on is news.
    pub(crate) fn read(&self, run: crate::job::Run) {
        if let Some(shared) = self.0.upgrade() {
            shared.observed().output_read(run);
        }
    }
}

/// The committed journal, read by index: what the core reads back of the jobs it
/// no longer holds. Each read runs in one snapshot of what was committed.
#[derive(Clone)]
pub(crate) struct Journal {
    session: SessionId,
    reads: Reads,
}

impl Journal {
    /// The status rows of those of `jobs` that exist, in id order.
    pub(crate) fn jobs(&self, jobs: &[JobId]) -> Result<Vec<JobRow>, SessionError> {
        let session = self.session;
        Ok(self.reads.read(|db| db::job_rows(db, session, jobs))?)
    }

    /// The jobs `agent` created, in id order.
    pub(crate) fn created_by(&self, agent: &AgentId) -> Result<Vec<JobId>, SessionError> {
        Ok(self.reads.read(|db| db::created_by(db, agent))?)
    }

    /// The records `agent`'s contract is journaled in, as its loop starts: those of
    /// the `profiled` and `granted` entries and the agent model `context` its state names.
    pub(crate) fn agent(
        &self,
        agent: &AgentId,
        entries: [RecordSeq; 2],
        context: Option<RecordSeq>,
    ) -> Result<Option<db::AgentRecords>, SessionError> {
        let session = self.session;
        Ok(self
            .reads
            .read(|db| db::agent(db, session, agent, entries, context))?)
    }
}

/// The state the journal in `db` opens to, as its rows hold it.
fn open_state(db: &db::Db, id: SessionId) -> Result<(CoreState, core::Observed), SessionError> {
    core::CoreState::open(db::counters(db)?, db::opening(db, id)?)
}

/// `work` for the core, run once `jobs` are held, in the same call.
fn held<T, E: From<SessionError>>(
    jobs: impl IntoIterator<Item = JobId>,
    work: impl FnOnce(&mut core::Writer) -> Result<T, E> + Send + 'static,
) -> impl FnOnce(&mut core::Writer) -> Result<T, E> + Send + 'static {
    let jobs: Vec<_> = jobs.into_iter().collect();
    move |writer| {
        writer.hold(jobs)?;
        work(writer)
    }
}

/// A store reference that keeps neither the session's core nor its lock.
pub(crate) struct WeakStore(std::sync::Weak<StoreInner>);

impl WeakStore {
    pub(crate) fn upgrade(&self) -> Option<SessionStore> {
        self.0.upgrade().map(|inner| SessionStore { inner })
    }
}

impl SessionStore {
    /// Fail the next commit once it committed, before it applies: durable, yet
    /// neither applied nor acknowledged.
    #[cfg(test)]
    pub(crate) async fn fail_committed(&self) {
        let fail = |writer: &mut core::Writer| writer.fail_committed = true;
        self.inner.core.call(fail).await.unwrap();
    }

    /// Replace a stored blob's bytes, or delete it, as disk corruption would.
    #[cfg(test)]
    pub(crate) fn corrupt_blob(&self, blob: crate::media::BlobDigest, bytes: Option<Vec<u8>>) {
        let key = blob.to_bytes().to_vec();
        let db = self.outputs();
        let db = db.lock();
        match bytes {
            Some(bytes) => db.execute(
                "UPDATE blob SET bytes = ?1 WHERE sha256 = ?2",
                db::params![bytes, key],
            ),
            None => db.execute("DELETE FROM blob WHERE sha256 = ?1", db::params![key]),
        }
        .unwrap();
    }

    #[cfg(test)]
    pub(crate) async fn inherit_lock(&self) -> Option<std::fs::File> {
        let inherit = |writer: &mut core::Writer| {
            (writer.owner.as_ref()).and_then(|owner| owner.lock.0.try_clone().ok())
        };
        self.inner.core.call(inherit).await.unwrap()
    }

    /// The session as a reopen opens it, over the same database. Its appends do
    /// not reach this store's state.
    #[cfg(test)]
    pub(crate) fn replica(&self) -> Self {
        let id = self.id();
        let state = self.test_reads().read(|db| Ok(open_state(db, id)));
        let state = state.unwrap().unwrap();
        let (db, directory) = (self.inner.db.clone(), self.directory().to_owned());
        let journal = self.test_journal();
        // Its source owns the log they share.
        let connections = (db.clone(), journal.clone());
        let started = core::Core::start(self.id(), directory, state, connections, None);
        let (shared, core) = started.unwrap();
        Self {
            inner: Arc::new(StoreInner {
                shared,
                db,
                journal,
                core,
            }),
        }
    }

    /// Read a session without taking its lock; a live owner may keep writing.
    pub async fn read_records(
        root: &Path,
        id: SessionId,
    ) -> Result<Vec<Arc<EventRecord>>, SessionError> {
        let read = move |db: &db::Db| {
            let records = db::decode_records(db, id, db::Entries::All)?;
            Ok(request::admit_records(db, records))
        };
        Self::read_only(root, id, read).await?
    }

    /// Whether a store, in this process or another, holds the session open. The
    /// probe shares the lock for an instant; an open racing it waits that out.
    pub async fn is_open(root: &Path, id: SessionId) -> Result<bool, SessionError> {
        let path = root.join(id.to_string()).join(LOCK_FILE);
        blocking(move || {
            let lock = match StdOpenOptions::new().read(true).open(&path) {
                Ok(lock) => lock,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(error.into()),
            };
            match lock.try_lock_shared() {
                Ok(()) => {
                    lock.unlock()?;
                    Ok(false)
                }
                Err(std::fs::TryLockError::WouldBlock) => Ok(true),
                Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
            }
        })
        .await
    }

    /// A session list row, without decoding the session or taking its lock.
    pub async fn summary(root: &Path, id: SessionId) -> Result<SessionSummary, SessionError> {
        Self::read_only(root, id, db::summary).await
    }

    /// Run `read` against one snapshot of a session that may not be open here.
    async fn read_only<T: Send + 'static>(
        root: &Path,
        id: SessionId,
        read: impl FnOnce(&db::Db) -> Result<T, DbError> + Send + 'static,
    ) -> Result<T, SessionError> {
        let path = root.join(id.to_string()).join(DATABASE_FILE);
        blocking(move || {
            if !path.is_file() {
                return Err(SessionError::NotFound(id));
            }
            Ok(Reads::Own(Arc::new(db::Own::new(path))).read(read)?)
        })
        .await
    }

    pub(crate) async fn create(root: &Path) -> Result<Self, SessionError> {
        Self::create_with_durability(root, true).await
    }

    #[cfg(test)]
    pub(crate) async fn create_ephemeral(root: &Path) -> Result<Self, SessionError> {
        Self::create_with_durability(root, false).await
    }

    async fn create_with_durability(root: &Path, durable: bool) -> Result<Self, SessionError> {
        let root = root.to_owned();
        // One blocking task creates the directory and the store whose close
        // removes it again, so a caller that gives up leaves nothing behind.
        let create = move || {
            std::fs::create_dir_all(&root)?;
            let id = SessionId::generate()?;
            let directory = root.join(id.to_string());
            std::fs::create_dir(&directory)?;
            Self::start(id, directory, durable, false)
        };
        blocking(create).await
    }

    pub async fn open(root: &Path, id: SessionId) -> Result<Self, SessionError> {
        let directory = root.join(id.to_string());
        blocking(move || Self::start(id, directory, true, true)).await
    }

    fn start(
        id: SessionId,
        directory: PathBuf,
        durable: bool,
        existing: bool,
    ) -> Result<Self, SessionError> {
        let path = directory.join(DATABASE_FILE);
        if existing && !path.is_file() {
            return Err(SessionError::NotFound(id));
        }
        // Acquire ownership before opening a potentially active database.
        let lock = durable.then(|| lock_session(&directory, id)).transpose()?;
        let mode = match (durable, existing) {
            (false, _) => db::OpenMode::Memory,
            (true, false) => db::OpenMode::Create,
            (true, true) => db::OpenMode::Open,
        };
        let log = durable.then(|| Arc::new(db::Log::of(&path)));
        let db = SharedDb::new(db::Db::open(&path, mode)?, log.clone());
        let owner = lock.zip(log).map(|(lock, log)| {
            let checkpointer = db::Checkpointer::start(&path, log, db.clone())?;
            Ok::<_, SessionError>(core::Owner { lock, checkpointer })
        });
        let owner = owner.transpose()?;
        // The core reads on a connection of its own, so that it never waits behind
        // other reads.
        let (reads, core_reads) = match durable {
            true => (
                Reads::Pool(Arc::new(db::ReadPool::new(path.clone()))),
                Reads::Own(Arc::new(db::Own::new(path))),
            ),
            false => {
                let reads = Reads::writer(db.clone());
                (reads.clone(), reads)
            }
        };
        let (mut state, observed) = open_state(&db.lock(), id)?;
        // A reopened session runs nothing until something changes it.
        if existing {
            state.set_phase(SessionPhase::Viewing);
        }
        let state = (state, observed);
        let core_journal = Journal {
            session: id,
            reads: core_reads,
        };
        let journal = Journal { session: id, reads };
        let connections = (db.clone(), core_journal);
        let (shared, core) = core::Core::start(id, directory, state, connections, owner)?;
        let inner = Arc::new(StoreInner {
            shared,
            db,
            journal,
            core,
        });
        Ok(Self { inner })
    }

    pub fn id(&self) -> SessionId {
        self.inner.shared.id
    }

    pub fn directory(&self) -> &Path {
        &self.inner.shared.directory
    }

    /// The session's live part and the updates after it, without a gap between
    /// them. It copies nothing of the history, which pages read.
    pub fn observe(&self) -> crate::agent::Observation {
        let shared = &self.inner.shared;
        // The core publishes only under the state's write lock, and posts take
        // only `observed`: subscribing under both, then copying the committed
        // state before the read lock drops, leaves every update either in the
        // snapshot or after it.
        let state = shared.read();
        let (posted, updates) = {
            let observed = shared.observed();
            (observed.posted(), shared.events.subscribe())
        };
        let mut snapshot = state.snapshot(posted);
        drop(state);
        snapshot.read_phase();
        crate::agent::Observation { snapshot, updates }
    }

    /// The committed journal, read by index.
    pub(crate) fn journal(&self) -> Journal {
        self.inner.journal.clone()
    }

    /// The revision of the newest update published to observers.
    pub fn revision(&self) -> u64 {
        self.inner.shared.observed().revision()
    }

    /// Fold and publish an update the journal does not carry. Nothing the core
    /// plans reads these views, so they wait neither behind its commands nor
    /// behind readers of its state, only on the observation's lock: a commit holds
    /// it from its fold to its publication.
    pub(crate) fn post(&self, event: crate::agent::RuntimeEvent) {
        self.inner.shared.post(event);
    }

    /// Read the core's state.
    pub(crate) fn read<T>(&self, read: impl FnOnce(&CoreState) -> T) -> T {
        read(&self.inner.shared.read())
    }

    /// Every committed record, including an ephemeral session's.
    #[cfg(test)]
    pub(crate) fn records(&self) -> Vec<Arc<EventRecord>> {
        let id = self.id();
        let read = |db: &db::Db| db::decode_records(db, id, db::Entries::All);
        self.test_reads().read(read).unwrap()
    }

    /// [`until`](Self::until) the wait `wait` builds from what `read` reads of the
    /// journal on the core, in the command that registers it: a wait reads no journal
    /// under the state's lock, and no change lands between the read and its first
    /// decision.
    #[cfg(test)]
    pub(crate) async fn until_read<R: Send + 'static, T: Send + 'static>(
        &self,
        read: impl FnOnce(&Journal) -> R + Send + 'static,
        wait: impl FnOnce(R) -> Wait<T> + Send + 'static,
    ) -> Result<T, SessionError> {
        let (journal, (reply, outcome)) = (self.test_journal(), tokio::sync::oneshot::channel());
        let register = move |writer: &mut core::Writer| {
            let wait = wait(read(&journal));
            writer.hold(wait.jobs())?;
            writer.change(|state| state.wait(wait, reply));
            Ok::<_, SessionError>(())
        };
        self.inner.core.call(register).await??;
        match outcome.await {
            Ok(outcome) => Ok(outcome),
            Err(_) => Err(self.behind(|state| state.phase().refusal()).await),
        }
    }

    /// The committed journal, for a test reading it on any thread.
    #[cfg(test)]
    pub(crate) fn test_journal(&self) -> Journal {
        Journal {
            session: self.id(),
            reads: self.test_reads(),
        }
    }

    /// Reads a test may run on its own thread: a connection of their own, or an
    /// in-memory session's writer.
    #[cfg(test)]
    pub(crate) fn test_reads(&self) -> Reads {
        match self.reads() {
            Reads::Pool(_) => {
                Reads::Own(Arc::new(db::Own::new(self.directory().join(DATABASE_FILE))))
            }
            reads => reads,
        }
    }

    /// Append one event; dropping this waiter does not cancel persistence.
    pub(crate) async fn append(
        &self,
        agent: AgentId,
        event: SessionEvent,
    ) -> Result<Arc<EventRecord>, SessionError> {
        Ok(self.append_all(vec![(agent, event)]).await?.remove(0))
    }

    /// Append several events committed in one transaction.
    pub(crate) async fn append_all(
        &self,
        events: Vec<(AgentId, SessionEvent)>,
    ) -> Result<Vec<Arc<EventRecord>>, SessionError> {
        let records = |_: &mut CoreState, records: &[Arc<EventRecord>]| records.to_vec();
        let plan = move |_: &CoreState| Ok(Plan::new(events, records));
        self.commit(Command::Journal, plan).await
    }

    /// Move the phase as `command` does, journaling nothing.
    #[cfg(test)]
    pub(crate) async fn enter(&self, command: Command) -> Result<(), SessionError> {
        let nothing = |_: &CoreState| Ok(Plan::journal(Vec::new()));
        self.commit(command, nothing).await
    }

    /// Commit `event` with the events `follow` builds from its sequence, in one
    /// transaction.
    #[cfg(test)]
    pub(crate) async fn append_then(
        &self,
        agent: AgentId,
        event: SessionEvent,
        follow: impl FnOnce(RecordSeq) -> Vec<(AgentId, SessionEvent)> + Send + 'static,
    ) -> Result<Vec<Arc<EventRecord>>, SessionError> {
        let records = |_: &mut CoreState, records: &[Arc<EventRecord>]| records.to_vec();
        self.commit(Command::Journal, move |state| {
            let mut events = vec![(agent, event)];
            events.extend(follow(state.next_sequence()));
            Ok(Plan::new(events, records))
        })
        .await
    }

    /// Run `plan` on the core thread against the state it commits to, if the
    /// session's phase admits `command`; dropping this waiter does not cancel it.
    pub(crate) async fn commit<T: Send + 'static, E: From<SessionError> + Send + 'static>(
        &self,
        command: Command,
        plan: impl FnOnce(&CoreState) -> Result<Plan<T>, E> + Send + 'static,
    ) -> Result<T, E> {
        self.commit_on(None, command, plan).await
    }

    /// [`commit`](Self::commit) a plan that reads `jobs` by id: each is held first,
    /// as the journal leaves it, if it was ever created.
    pub(crate) async fn commit_on<T: Send + 'static, E: From<SessionError> + Send + 'static>(
        &self,
        jobs: impl IntoIterator<Item = JobId>,
        command: Command,
        plan: impl FnOnce(&CoreState) -> Result<Plan<T>, E> + Send + 'static,
    ) -> Result<T, E> {
        let run = move |writer: &mut core::Writer| writer.run(command, plan);
        self.inner.core.call(held(jobs, run)).await?
    }

    /// Change facts the journal does not record, in order with every commit.
    pub(crate) async fn live<T: Send + 'static>(
        &self,
        change: impl FnOnce(&mut CoreState) -> T + Send + 'static,
    ) -> T {
        let live = move |writer: &mut core::Writer| writer.change(change);
        let changed = self.inner.core.call(live).await;
        changed.expect("only a panicking change fails")
    }

    /// Change facts the journal does not record, behind every command queued
    /// before it, without waiting for the change.
    pub(crate) fn enqueue(&self, change: impl FnOnce(&mut CoreState) + Send + 'static) {
        self.inner.core.post(move |writer| writer.change(change));
    }

    /// Resolve once `wait` decides, as it reads the state when registered and
    /// after every change to what it declares, consuming what it decides on as
    /// it decides: its waiter must lose nothing by abandoning the reply. Refused
    /// as closed, or with its failure, once the session decides nothing more.
    /// The jobs it declares are held as it registers, so it first decides on
    /// their facts; one let go after is as it was then.
    pub(crate) async fn until<T: Send + 'static>(&self, wait: Wait<T>) -> Result<T, SessionError> {
        self.register(wait.jobs(), |_| (), |()| wait).await
    }

    /// [`until`](Self::until) the wait `wait` builds from what `first` returns,
    /// registered in the change that applies `first`, so it and every other wait
    /// first read the state as `first` leaves it, which holds the jobs it reads.
    pub(crate) async fn until_after<F: Send + 'static, T: Send + 'static>(
        &self,
        first: impl FnOnce(&mut CoreState) -> F + Send + 'static,
        wait: impl FnOnce(F) -> Wait<T> + Send + 'static,
    ) -> Result<T, SessionError> {
        self.register(Vec::new(), first, wait).await
    }

    /// Hold `jobs`, then register the wait `wait` builds from what `first` returns.
    async fn register<F: Send + 'static, T: Send + 'static>(
        &self,
        jobs: Vec<JobId>,
        first: impl FnOnce(&mut CoreState) -> F + Send + 'static,
        wait: impl FnOnce(F) -> Wait<T> + Send + 'static,
    ) -> Result<T, SessionError> {
        let (reply, outcome) = tokio::sync::oneshot::channel();
        let register = move |writer: &mut core::Writer| {
            writer.change(|state| {
                let first = first(state);
                state.wait(wait(first), reply);
            });
            Ok::<_, SessionError>(())
        };
        self.inner.core.call(held(jobs, register)).await??;
        match outcome.await {
            Ok(outcome) => Ok(outcome),
            Err(_) => Err(self.behind(|state| state.phase().refusal()).await),
        }
    }

    /// Queues changes on this session's core without keeping it open.
    pub(crate) fn weak_core(&self) -> core::WeakCore {
        self.inner.core.downgrade()
    }

    pub(crate) fn downgrade(&self) -> WeakStore {
        WeakStore(Arc::downgrade(&self.inner))
    }

    /// Read the state behind the command that dropped a reply, which may not have
    /// moved the phase yet, as a command that panics drops it first; as it
    /// stands once the core has stopped.
    pub(crate) async fn behind<T: Send + 'static>(&self, read: fn(&CoreState) -> T) -> T {
        let behind = move |writer: &mut core::Writer| read(&writer.shared.read());
        let stopped = self.inner.core.call(behind).await;
        stopped.unwrap_or_else(|_| self.read(read))
    }

    /// Await accepted appends and report the failure that stopped the session,
    /// if its journal failed, without closing admission.
    pub(crate) async fn drain(&self) -> Result<(), SessionError> {
        // Its reply follows the sync of every commit queued before it.
        self.inner.core.call(|_| ()).await?;
        self.read(|state| state.phase().failed())
    }

    /// The token whose cancellation stops everything the session runs, as its
    /// journal failing does.
    pub(crate) fn root(&self) -> crate::job::CancellationToken {
        self.inner.shared.root.clone()
    }

    /// Close behind every command queued before this, without waiting.
    pub(crate) fn close_soon(&self) {
        self.inner.core.post(|writer| drop(writer.close()));
    }

    /// Stop admission, wait out operations already queued, release the session
    /// for another open, and report the failure that stopped the session, if its
    /// journal failed. Retained handles, including output streams, can still read
    /// but can no longer write.
    pub async fn close(&self) -> Result<(), SessionError> {
        self.inner.core.call(core::Writer::close).await?
    }

    /// The title the session list shows: see [`SessionSummary::title`].
    pub async fn title(&self) -> Result<Option<SessionTitle>, SessionError> {
        self.snapshot(|db| Ok(db::title(db)?)).await
    }

    /// Run `read` against one snapshot of the committed session, off the async workers.
    async fn snapshot<T: Send + 'static>(
        &self,
        read: impl FnOnce(&db::Db) -> Result<T, SessionError> + Send + 'static,
    ) -> Result<T, SessionError> {
        let reads = self.reads();
        self.reads()
            .hold(move || reads.read(|db| Ok(read(db)))?)
            .await
    }

    /// The entries that created those of `jobs` that `agent` created, with their
    /// creations, in id order.
    pub(crate) async fn job_creations(
        &self,
        jobs: Vec<JobId>,
        agent: &AgentId,
    ) -> Result<Vec<(RecordSeq, SessionEvent)>, SessionError> {
        let session = self.id();
        let read = move |db: &db::Db, row| db::creations(db, session, &jobs, row);
        self.read_agent(agent, Vec::new(), read).await
    }

    /// The connection job output streams write through.
    pub(crate) fn outputs(&self) -> SharedDb {
        self.inner.db.clone()
    }

    /// Where reads that must not wait on the writer run.
    pub(crate) fn reads(&self) -> Reads {
        self.inner.journal.reads.clone()
    }

    /// What output streams tell observers through.
    pub(crate) fn output_changes(&self) -> OutputChanges {
        OutputChanges(Arc::downgrade(&self.inner.shared))
    }

    /// Store bytes in the content-addressed blob store.
    pub(crate) async fn store_blob(
        &self,
        bytes: &[u8],
    ) -> Result<crate::media::BlobRef, SessionError> {
        let blob = crate::media::BlobRef::of(bytes);
        let bytes = bytes.to_vec();
        // A blob the journal cannot take stops the session, as an append does.
        let insert = move |writer: &mut core::Writer| {
            writer.shared.read().phase().admits(Command::Journal)?;
            writer.write(|db, _| Ok(db::insert_blob(db, blob.sha256, &bytes)?))
        };
        self.inner.core.call(insert).await??;
        Ok(blob)
    }

    /// Read a blob bounded by `limit`, verified against its digest and size.
    #[cfg(test)]
    pub(crate) async fn read_blob(
        &self,
        blob: &crate::media::BlobRef,
        limit: usize,
    ) -> Result<Vec<u8>, SessionError> {
        let blob = *blob;
        self.snapshot(move |db| read_blob(db, blob, limit)).await
    }

    pub(crate) async fn store_image(
        &self,
        file: Option<String>,
        image: &crate::media::Image,
    ) -> Result<crate::media::ImageRef, SessionError> {
        Ok(crate::media::ImageRef {
            file,
            format: image.format(),
            blob: self.store_blob(image.bytes()).await?,
        })
    }

    pub(crate) async fn store_attachment(
        &self,
        attachment: &crate::media::Attachment,
    ) -> Result<crate::media::AttachmentRef, SessionError> {
        use crate::media::{Attachment, AttachmentRef, TextRef};
        let file = attachment.file().map(|file| file.display().to_string());
        Ok(match attachment {
            Attachment::Text { content, .. } => AttachmentRef::Text(TextRef {
                file,
                blob: self.store_blob(content.as_bytes()).await?,
            }),
            Attachment::Image { image, .. } => {
                AttachmentRef::Image(self.store_image(file, image).await?)
            }
        })
    }

    /// Load every blob a request references so providers can encode it. Blobs in
    /// `cache` are not read again; afterwards it holds exactly this request's blobs.
    pub async fn load_blobs(
        &self,
        request: &mut ModelRequest,
        cache: &mut crate::media::LoadedBlobs,
    ) -> Result<(), SessionError> {
        use crate::media::{AttachmentRef, ImageFormat, MAX_IMAGE_BYTES};
        // Image blobs carry the format they must sniff as; text blobs carry none.
        let mut blobs = Vec::new();
        use crate::provider::protocol::{Message, UserContent};
        for message in request.messages() {
            match message {
                Message::User(content) => {
                    blobs.extend(content.iter().filter_map(|item| match item {
                        UserContent::Attachment {
                            attachment: AttachmentRef::Text(text),
                        } => Some((text.blob, None)),
                        UserContent::Attachment {
                            attachment: AttachmentRef::Image(image),
                        } => Some((image.blob, Some(image.format))),
                        _ => None,
                    }));
                }
                Message::Tool(results) => blobs.extend(
                    results
                        .iter()
                        .flat_map(|result| &result.images)
                        .map(|image| (image.blob, Some(image.format))),
                ),
                Message::Assistant(_) => {}
            }
        }
        // A blob a text also references is exempt from sniffing.
        blobs.sort_by_key(|(_, format)| format.is_none());
        let blobs: std::collections::BTreeMap<_, _> = blobs.into_iter().collect();
        let missing: Vec<_> = blobs
            .keys()
            .filter(|blob| !cache.contains(blob))
            .copied()
            .collect();
        if !missing.is_empty() {
            // One bound for every blob loaded into a request, text or image.
            let limit = MAX_IMAGE_BYTES as usize;
            let read = move |db: &db::Db| {
                let read = |blob| Ok((blob, read_blob(db, blob, limit)?));
                missing.into_iter().map(read).collect::<Result<Vec<_>, _>>()
            };
            for (blob, bytes) in self.snapshot(read).await? {
                cache.insert(blob, bytes);
            }
        }
        let mut loaded = crate::media::LoadedBlobs::default();
        for (blob, format) in blobs {
            let bytes = cache.shared(&blob).expect("referenced blobs are loaded");
            if format.is_some_and(|format| ImageFormat::sniff(&bytes) != Some(format)) {
                return Err(BlobError::UnsupportedImage.at(blob.sha256));
            }
            loaded.insert(blob, bytes);
        }
        cache.clone_from(&loaded);
        request.blobs = loaded;
        Ok(())
    }
}

fn read_blob(
    db: &db::Db,
    blob: crate::media::BlobRef,
    limit: usize,
) -> Result<Vec<u8>, SessionError> {
    usize::try_from(blob.bytes)
        .ok()
        .filter(|&length| length <= limit)
        .ok_or(BlobError::TooLarge.at(blob.sha256))?;
    let bytes = db::verified_blob(db, blob.sha256)?;
    let bytes = bytes.map_err(|reason| reason.at(blob.sha256))?;
    if bytes.len() as u64 != blob.bytes {
        return Err(BlobError::LengthMismatch.at(blob.sha256));
    }
    Ok(bytes)
}

/// Why a stored blob cannot be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum BlobError {
    #[error("is missing")]
    Missing,
    #[error("failed its content hash check")]
    HashMismatch,
    #[error("exceeds its byte limit")]
    TooLarge,
    #[error("does not match its byte length")]
    LengthMismatch,
    #[error("is not the image format it was stored as")]
    UnsupportedImage,
}

impl BlobError {
    fn at(self, digest: crate::media::BlobDigest) -> SessionError {
        SessionError::Blob {
            digest,
            reason: self,
        }
    }
}

/// What a session stopped by its journal's failure reports.
pub(crate) const JOURNAL_FAILED: &str = "Skyhook could not write the session journal and stopped the session (a Skyhook bug or a storage problem)";

#[derive(Debug, Error)]
pub enum SessionError {
    /// Committed, or it may have: the database's report when the commit itself
    /// failed.
    #[error("the journal could not confirm an accepted append")]
    Unconfirmed(#[source] Option<DbError>),
    #[error("{JOURNAL_FAILED}: {0}")]
    JournalFailed(JournalFailure),
    #[error("the session is {0}")]
    Refused(SessionPhase),
    #[error("cannot reconstruct model request at sequence {sequence}: {reason}")]
    ModelRequestReplay {
        sequence: RequestSeq,
        reason: ReplayError,
    },
    #[error("invalid compaction at sequence {sequence}: {reason}")]
    InvalidCompaction {
        sequence: RecordSeq,
        reason: CheckpointError,
    },
    #[error("session writer was lost before accepting the append")]
    WriterLost,
    #[error("session task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
    #[error("session {0} was not found")]
    NotFound(SessionId),
    #[error("session I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Database(DbError),
    #[error("session identifier generation failed: {0}")]
    Random(#[from] getrandom::Error),
    #[error("session {0} is already open")]
    AlreadyOpen(SessionId),
    #[error("unsupported session version {0}")]
    UnsupportedVersion(i64),
    #[error("event belongs to another session")]
    WrongSession,
    #[error("journal entry {sequence} changes a job's phase illegally")]
    IllegalTransition { sequence: RecordSeq },
    #[error("blob `{digest}` {reason}")]
    Blob {
        digest: crate::media::BlobDigest,
        reason: BlobError,
    },
}

/// The first writer failure, which stopped the session: every later write
/// reports it.
#[derive(Clone, Debug)]
pub struct JournalFailure(Arc<SessionError>);

impl JournalFailure {
    pub fn cause(&self) -> &SessionError {
        &self.0
    }
}

/// The same failure, not merely an equal one.
impl PartialEq for JournalFailure {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl std::fmt::Display for JournalFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl From<DbError> for SessionError {
    fn from(error: DbError) -> Self {
        match error {
            DbError::Unsupported(version) => Self::UnsupportedVersion(version),
            error => Self::Database(error),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    //! Session tests, and fixtures other modules reuse: every journaled entry references a
    //! started agent, so tests start the agents they write for, usually in an in-memory
    //! database.

    use super::*;
    use crate::{
        agent::TurnFailure,
        execution::ExecutionLocation,
        identity::JobId,
        provider::{
            profile::ModelProfile,
            protocol::{HistoryLifetime, SystemSegment, ToolDefinition, Usage},
        },
        tests::TestRuntime,
        tool::policy::Capability,
    };
    use serde_json::json;

    /// Refuses, while held, every write `rule` matches, as a failing journal does.
    /// `rule` is a trigger's timing and event, such as `BEFORE INSERT ON job_finish`.
    pub(crate) struct Refusal(SharedDb, String);

    pub(crate) fn refuse(store: &SessionStore, rule: &str) -> Refusal {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let name = format!("refuse_{}", NEXT.fetch_add(1, Ordering::Relaxed));
        let db = store.outputs();
        let trigger = format!(
            "CREATE TEMP TRIGGER {name} {rule} BEGIN SELECT RAISE(ABORT, 'injected fault'); END;"
        );
        db.lock().batch(&trigger).unwrap();
        Refusal(db, name)
    }

    impl Drop for Refusal {
        fn drop(&mut self) {
            // A session closed since takes no writes, so it has nothing to refuse.
            let _ = self.0.lock().batch(&format!("DROP TRIGGER {}", self.1));
        }
    }

    pub(crate) fn journal_failure(cause: SessionError) -> JournalFailure {
        JournalFailure(Arc::new(cause))
    }

    /// Every request `records` journal, as their rows leave it, and what folding each
    /// record decided.
    #[derive(Default)]
    pub(crate) struct Folded {
        pub(crate) requests: std::collections::BTreeMap<RequestSeq, RequestRecord>,
        pub(crate) facts: std::collections::HashMap<RecordSeq, Fact>,
    }

    pub(crate) fn fold(records: &[Arc<EventRecord>]) -> Folded {
        let mut ledger = ResidentLedger::default();
        let mut agents = agent_state::AgentStates::default();
        let facts = (records.iter())
            .filter_map(|record| Some((record.sequence, agents.apply(record, &mut ledger)?)))
            .collect();
        let requests = agents.take_unwritten().requests;
        Folded { requests, facts }
    }

    /// [`fold`] of `store`'s whole journal.
    pub(crate) fn ledger(store: &SessionStore) -> Folded {
        fold(&store.records())
    }

    /// The record journaled at `sequence`. Records must be in sequence order.
    pub(crate) fn record_at(
        records: &[Arc<EventRecord>],
        sequence: RecordSeq,
    ) -> Option<&EventRecord> {
        let index = records
            .binary_search_by_key(&sequence, |record| record.sequence)
            .ok()?;
        Some(&records[index])
    }

    /// The purpose of the context the request journaled as `record` was issued under,
    /// as `records` hold it.
    pub(crate) fn request_purpose(
        records: &[Arc<EventRecord>],
        record: &EventRecord,
    ) -> Option<ModelPurpose> {
        let SessionEvent::ModelRequested { context, .. } = &record.event else {
            return None;
        };
        match &record_at(records, *context)?.event {
            SessionEvent::ModelContext { context } => Some(context.purpose),
            _ => None,
        }
    }

    /// A wait on `agent`'s records, those journaled so far and each applied after,
    /// that resolves with what `decide` makes of them once it decides.
    pub(crate) fn on_records<T: Send + 'static>(
        store: &SessionStore,
        agent: &AgentId,
        decide: impl Fn(&[Arc<EventRecord>]) -> Option<T> + Send + Sync + 'static,
    ) -> Wait<T> {
        let (through, feed) = store.inner.shared.observed().subscribe(agent, |_| true);
        let journaled = (store.records().into_iter())
            .filter(|record| record.sequence <= through && &record.agent == agent)
            .collect::<Vec<_>>();
        let fed = std::sync::Mutex::new((feed, journaled));
        Wait::on([Key::Agent(agent.clone())], move |_| {
            let (feed, records) = &mut *fed.lock().unwrap();
            records.extend(feed.try_iter());
            decide(records)
        })
    }

    /// Start `start`'s future in the change that applies the first record of
    /// `agent` from here on that `matches` accepts, before that commit's caller
    /// hears back or its effects run, so what the future queues on the core
    /// first runs right behind that commit, and returns the future's output. Its
    /// first poll runs on the core, where it must queue work rather than read
    /// the state.
    pub(crate) async fn on_commit<F, S>(
        store: &SessionStore,
        agent: &AgentId,
        matches: fn(&SessionEvent) -> bool,
        start: S,
    ) -> impl std::future::Future<Output = F::Output> + use<F, S>
    where
        F: std::future::Future + Send + 'static,
        S: FnOnce() -> F + Send + 'static,
    {
        use tokio::sync::oneshot::channel;
        let ((armed, arming), (started, starting)) = (channel(), channel());
        let start = std::sync::Mutex::new(Some((start, started)));
        let (_, feed) = store.inner.shared.observed().subscribe(agent, |_| true);
        // What the agent's records applied so far hold, kept once one matched.
        let fed = std::sync::Mutex::new((feed, false));
        let from = move |_: &mut CoreState| {
            let _ = armed.send(());
        };
        let key = Key::Agent(agent.clone());
        let applied = move |()| {
            let decide = move |_: &CoreState| {
                let (feed, matched) = &mut *fed.lock().unwrap();
                *matched |= feed.try_iter().any(|record| matches(&record.event));
                matched.then_some(())
            };
            let take = move |_: &mut CoreState, ()| {
                let Some((start, started)) = start.lock().unwrap().take() else {
                    return;
                };
                // Erased, which keeps proving the wait `Send` and `Sync` shallow.
                let mut future: std::pin::Pin<Box<dyn std::future::Future<Output = _> + Send>> =
                    Box::pin(start());
                let mut context = std::task::Context::from_waker(std::task::Waker::noop());
                assert!(std::future::Future::poll(future.as_mut(), &mut context).is_pending());
                let _ = started.send(future);
            };
            Wait::taking([key], decide, take)
        };
        let store = store.clone();
        tokio::spawn(async move { store.until_after(from, applied).await });
        arming.await.unwrap();
        async move { starting.await.unwrap().await }
    }

    /// Holds the session's connection from another thread until dropped: output
    /// writes wait for it, and so does the core at its next commit, with every
    /// command queued behind that commit, in order.
    pub(crate) struct HeldConnection {
        _release: std::sync::mpsc::Sender<()>,
    }

    pub(crate) fn hold_connection(store: &SessionStore) -> HeldConnection {
        let db = store.outputs();
        let (held, holding) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            let _connection = db.lock();
            held.send(()).unwrap();
            let _ = released.recv();
        });
        holding.recv().unwrap();
        HeldConnection { _release: release }
    }

    /// Reopen the session `id` and activate it, as a reopened session's first
    /// change does, so it takes appends.
    pub(crate) async fn reopen(root: &Path, id: SessionId) -> SessionStore {
        let store = SessionStore::open(root, id).await.unwrap();
        store.enter(Command::Activate).await.unwrap();
        store
    }

    /// `agent`'s history as the journal holds it now, read as an opening context reads it.
    pub(crate) async fn projected(store: &SessionStore, agent: &AgentId) -> Projection {
        store.open_history(agent).await.unwrap().projection
    }

    /// Journal a session start and its root agent; returns the root.
    pub(crate) async fn started(store: &SessionStore, workspace: &Path) -> AgentId {
        let agent = AgentId::root(store.id());
        store
            .append_all(start_events(&agent, workspace))
            .await
            .unwrap();
        agent
    }

    /// Start `parent`'s child `index` in any store.
    pub(crate) async fn start_child(
        store: &SessionStore,
        parent: &AgentId,
        index: u32,
        owner_job: Option<JobId>,
        workspace: &Path,
    ) -> AgentId {
        let child = parent.child(index);
        let location = ExecutionLocation::root(workspace.to_path_buf());
        store
            .append(child.clone(), child_started(owner_job, location))
            .await
            .unwrap();
        child
    }

    pub(crate) fn start_events(agent: &AgentId, workspace: &Path) -> Vec<(AgentId, SessionEvent)> {
        vec![
            (
                agent.clone(),
                SessionEvent::SessionStarted {
                    targets: Vec::new(),
                },
            ),
            (agent.clone(), agent_started(workspace)),
        ]
    }

    pub(crate) fn profile() -> ProfileSnapshot {
        ProfileSnapshot {
            name: "test/test".parse().unwrap(),
            profile: ModelProfile {
                hint: Some("Test model.".parse().unwrap()),
                ..crate::tests::profile("test", false)
            },
        }
    }

    pub(crate) fn usage(input: u64, cached: u64, output: u64) -> Usage {
        Usage::prompted(input, cached, 0, output)
    }

    /// A request against `context` with no checkpoint or history range.
    pub(crate) fn requested(context: RecordSeq) -> SessionEvent {
        SessionEvent::ModelRequested {
            context,
            checkpoint: None,
            through: None,
            from: None,
            tail: Vec::new(),
            history_lifetime: Default::default(),
        }
    }

    pub(crate) fn attempt(request: RequestSeq, attempt: u64) -> SessionEvent {
        SessionEvent::ModelAttemptStarted(AttemptRef { request, attempt })
    }

    pub(crate) fn record(agent: &AgentId, sequence: u64, event: SessionEvent) -> Arc<EventRecord> {
        Arc::new(EventRecord {
            id: crate::identity::EventId::generate().unwrap(),
            sequence: sequence.into(),
            timestamp_millis: 0,
            agent: agent.clone(),
            event,
        })
    }

    /// Job `job` created for `role` in the root workspace; only an agent job
    /// accepts input.
    pub(crate) fn job_created(job: JobId, role: crate::job::JobRole) -> SessionEvent {
        SessionEvent::JobCreated {
            origin: None,
            job,
            parent: None,
            tool: role.as_str().into(),
            role,
            name: None,
            arguments: serde_json::Value::Null,
            output_schema: None,
            accepts_input: role == crate::job::JobRole::Agent,
            background: false,
            location: ExecutionLocation::root(".".into()),
        }
    }

    pub(crate) fn agent_started(workspace: &Path) -> SessionEvent {
        child_started(None, ExecutionLocation::root(workspace.to_path_buf()))
    }

    /// An agent start owned by `owner_job` at `location`, for fixtures that script children.
    pub(crate) fn child_started(
        owner_job: Option<JobId>,
        location: ExecutionLocation,
    ) -> SessionEvent {
        SessionEvent::AgentStarted {
            owner_job,
            profile: profile(),
            available_depth: 0,
            mode: None,
            capabilities: vec![Capability::Read],
            location,
        }
    }

    /// An on-disk session journaled under `root/sessions`, its id and its root.
    async fn fresh() -> (tempfile::TempDir, SessionStore, SessionId, AgentId) {
        let TestRuntime {
            root, store, agent, ..
        } = TestRuntime::on_disk().await;
        let id = store.id();
        (root, store, id, agent)
    }

    pub(crate) fn text_message(text: &str) -> Message {
        Message::User(vec![UserPart::Text { text: text.into() }])
    }

    pub(crate) fn committed(message: Message) -> SessionEvent {
        SessionEvent::MessageCommitted { message }
    }

    pub(crate) fn requested_history(
        context: RecordSeq,
        checkpoint: Option<RecordSeq>,
        history: &[RecordSeq],
        tail: &str,
        history_lifetime: HistoryLifetime,
    ) -> SessionEvent {
        SessionEvent::ModelRequested {
            context,
            checkpoint,
            through: history.last().map(|source| source.message()),
            from: None,
            tail: vec![text_message(tail)],
            history_lifetime,
        }
    }

    pub(crate) fn context(
        purpose: ModelPurpose,
        provider: &str,
        model: &str,
        system: &str,
    ) -> SessionEvent {
        let mut profile = profile();
        profile.name.provider = provider.parse().unwrap();
        profile.profile.model = model.parse().unwrap();
        profile.profile.reasoning = Some("high".into());
        SessionEvent::ModelContext {
            context: crate::session::ModelContext {
                purpose,
                profile,
                system: vec![SystemSegment {
                    text: system.into(),
                    cache: true,
                }],
                tools: vec![ToolDefinition {
                    name: "read".into(),
                    description: "original description".into(),
                    input_schema: json!({"type":"object"}),
                }],
                response_schema: Some(crate::provider::protocol::ResponseSchema::new("answer", json!({"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"],"additionalProperties":false})).unwrap()),
            },
        }
    }

    /// Whether the journal takes `event` as `agent`'s next record, by the rules an
    /// append validates it with.
    pub(crate) fn admitted(
        store: &SessionStore,
        agent: &AgentId,
        event: SessionEvent,
    ) -> Result<(), SessionError> {
        let sequence = store.read(CoreState::next_sequence);
        let record = record(agent, sequence.get(), event);
        request::validate(&store.outputs().lock(), &record)
    }

    pub(crate) fn sequences(projection: &Projection) -> Vec<u64> {
        projection
            .messages
            .iter()
            .map(|projected| projected.sequence.get())
            .collect()
    }

    /// A command queued before its caller's runtime shuts down still commits:
    /// the shutdown drops only the waiter, which reports nothing.
    #[test]
    fn a_command_outlives_its_runtime() {
        let runtime = || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
        };
        let first = runtime().unwrap();
        let (root, store, id, agent) = first.block_on(fresh());
        let sessions = root.path().join("sessions");
        let held = hold_connection(&store);
        let mut appending = Box::pin(store.append(agent, SessionEvent::AgentInterrupted));
        first.block_on(async { assert!(futures_util::poll!(&mut appending).is_pending()) });
        drop(appending);
        drop(first);
        drop(held);
        drop(store);
        let replay = runtime()
            .unwrap()
            .block_on(SessionStore::open(&sessions, id));
        let last = replay.unwrap().records().pop().unwrap();
        assert!(matches!(last.event, SessionEvent::AgentInterrupted));
    }

    #[tokio::test]
    async fn accepted_append_survives_lost_waiter_and_close_drains() {
        let (root, store, id, agent) = fresh().await;
        let sessions = root.path().join("sessions");
        let mut events = store.observe().updates;
        let sequence = RecordSeq::from(store.records().len() as u64 + 1);
        let held = hold_connection(&store);
        let mut appending = Box::pin(store.append(agent.clone(), SessionEvent::AgentInterrupted));
        assert!(futures_util::poll!(&mut appending).is_pending());
        drop(appending);
        // Queued behind the append, so closing drains it first.
        let mut closing = Box::pin(store.close());
        assert!(futures_util::poll!(&mut closing).is_pending());
        drop(held);
        crate::tests::bounded(closing).await.unwrap();
        let crate::agent::RuntimeEvent::Record(record) = events.recv().await.unwrap().event else {
            panic!("only records were published");
        };
        assert_eq!(record.sequence, sequence);
        assert_eq!(store.records().last(), Some(&record));
        let closed = store.append(agent, SessionEvent::AgentCompleted).await;
        assert!(matches!(
            closed,
            Err(SessionError::Refused(SessionPhase::Closed))
        ));
        drop(store);
        let replay = SessionStore::open(&sessions, id).await.unwrap().records();
        assert_eq!(replay.last(), Some(&record));
    }

    /// Output writes share the connection, and wait for a commit only up to its COMMIT.
    #[tokio::test]
    async fn the_connection_is_free_while_a_commit_publishes() {
        let (_root, store, _id, agent) = fresh().await;
        let (reached, publishing) = tokio::sync::oneshot::channel();
        let (resume, resumed) = tokio::sync::oneshot::channel::<()>();
        let entries = vec![(agent, SessionEvent::AgentInterrupted)];
        let live = move |_: &mut CoreState, _: &[Arc<EventRecord>]| {
            let _ = reached.send(());
            let _ = resumed.blocking_recv();
        };
        let plan = move |_: &CoreState| Ok::<_, SessionError>(Plan::new(entries, live));
        let appending = tokio::spawn({
            let store = store.clone();
            async move { store.commit(Command::Journal, plan).await }
        });
        publishing.await.unwrap();
        crate::tests::bounded(store.title()).await.unwrap();
        resume.send(()).unwrap();
        appending.await.unwrap().unwrap();
    }

    /// Park the core until the returned sender is used, which then leaves the
    /// commands queued meanwhile to run as one group; the handle ends with the park.
    async fn parked(
        store: &SessionStore,
    ) -> (std::sync::mpsc::Sender<()>, tokio::task::JoinHandle<()>) {
        let (parked, parking) = tokio::sync::oneshot::channel();
        let (resume, resumed) = std::sync::mpsc::channel::<()>();
        let store = store.clone();
        let park = tokio::spawn(async move {
            let park = move |_: &mut CoreState| {
                let _ = parked.send(());
                let _ = resumed.recv();
            };
            store.live(park).await
        });
        parking.await.unwrap();
        (resume, park)
    }

    /// The commands queued together commit as one group, which one sync makes
    /// durable, each planned against the state the ones before it leave: one its
    /// plan refuses is refused alone, and the others commit and reply in order.
    #[tokio::test]
    async fn a_refused_plan_in_a_group_leaves_its_neighbours_committed() {
        let (_root, store, _id, agent) = fresh().await;
        let syncs = || {
            store
                .inner
                .core
                .call(|writer: &mut core::Writer| writer.syncs)
        };
        let before = syncs().await.unwrap();
        let (resume, park) = parked(&store).await;
        // Each queues as it is first polled.
        let mut first = Box::pin(store.append(agent.clone(), SessionEvent::AgentInterrupted));
        assert!(futures_util::poll!(&mut first).is_pending());
        let (seen, saw) = tokio::sync::oneshot::channel();
        let refusing = move |state: &CoreState| {
            let _ = seen.send(state.next_sequence());
            Err::<Plan<()>, _>(SessionError::WrongSession)
        };
        let mut refused = Box::pin(store.commit(Command::Journal, refusing));
        assert!(futures_util::poll!(&mut refused).is_pending());
        let mut last = Box::pin(store.append(agent.clone(), SessionEvent::AgentCompleted));
        assert!(futures_util::poll!(&mut last).is_pending());
        resume.send(()).unwrap();
        crate::tests::bounded(park).await.unwrap();
        let first = crate::tests::bounded(first).await.unwrap();
        assert!(matches!(
            crate::tests::bounded(refused).await,
            Err(SessionError::WrongSession)
        ));
        let last = crate::tests::bounded(last).await.unwrap();
        assert_eq!(saw.await.unwrap(), first.sequence.next());
        assert_eq!(last.sequence, first.sequence.next());
        let records = store.records();
        assert_eq!(&records[records.len() - 2..], [first, last]);
        assert_eq!(syncs().await.unwrap(), before + 1);
    }

    /// A group whose sync fails stops the session: what committed in it reports
    /// the failure, and so do a drain and a close queued behind it.
    #[tokio::test]
    async fn a_failed_group_sync_fails_the_drain_and_close_behind_its_commits() {
        let (_root, store, _id, agent) = fresh().await;
        let (resume, park) = parked(&store).await;
        store.inner.db.fail_syncs();
        let mut appended = Box::pin(store.append(agent, SessionEvent::AgentInterrupted));
        assert!(futures_util::poll!(&mut appended).is_pending());
        let mut drained = Box::pin(store.drain());
        assert!(futures_util::poll!(&mut drained).is_pending());
        let mut closed = Box::pin(store.close());
        assert!(futures_util::poll!(&mut closed).is_pending());
        resume.send(()).unwrap();
        crate::tests::bounded(park).await.unwrap();
        let Err(SessionError::JournalFailed(failure)) = crate::tests::bounded(appended).await
        else {
            panic!("the append reported success");
        };
        assert!(matches!(
            failure.cause(),
            SessionError::Unconfirmed(Some(_))
        ));
        let same =
            |result| matches!(result, Err(SessionError::JournalFailed(later)) if later == failure);
        assert!(same(crate::tests::bounded(drained).await));
        assert!(same(crate::tests::bounded(closed).await));
    }

    /// Checkpoints run beside the writer and never block it below the restart threshold
    /// or a reader: a read's snapshot keeps the frames committed after it in the log,
    /// while every commit meanwhile completes; once it ends, the log is copied and
    /// starts over.
    #[tokio::test]
    async fn checkpoints_keep_the_log_bounded_beside_a_held_reader() {
        let (_root, store, _id, agent) = fresh().await;
        let passed = || store.inner.db.test_passed();
        let append = || store.append(agent.clone(), SessionEvent::AgentInterrupted);
        let (held, holding) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel::<()>();
        let reads = store.test_reads();
        let reader = std::thread::spawn(move || {
            reads.read(|db| {
                db.query_row("SELECT count(*) FROM entry", Vec::new(), |_| Ok(()))?;
                held.send(()).unwrap();
                let _ = released.recv();
                Ok(())
            })
        });
        holding.recv().unwrap();
        // Checkpoints keep running, and copy none of what the reader may still read.
        let held_back = |frames: Option<db::Frames>| {
            frames.is_some_and(|frames| frames.uncopied() > 2 * db::CHECKPOINT_FRAMES)
        };
        crate::tests::bounded(async {
            while !held_back(passed()) {
                append().await.unwrap();
            }
        })
        .await;
        release.send(()).unwrap();
        reader.join().unwrap().unwrap();
        // The next checkpoint copies the whole log, and a commit after it starts it over.
        let restarted = |frames: Option<db::Frames>| {
            frames.is_some_and(|frames| frames.logged < db::CHECKPOINT_FRAMES)
        };
        crate::tests::bounded(async {
            while !restarted(passed()) {
                append().await.unwrap();
            }
        })
        .await;
    }

    /// Every writer failure stops the session: the command that met it reports
    /// it, as does every later write, a blob's too, and closing, which still
    /// frees the session; every wait resolves with it. The connection takes no
    /// more writes, and the failed change applies nothing live, not even what it
    /// deferred. Reopening sees exactly what committed, which the failed session
    /// already shows, and a session whose only append failed is kept exactly when
    /// that append committed.
    #[tokio::test]
    async fn every_writer_failure_stops_the_journal_and_reopen_sees_what_committed() {
        use std::sync::atomic::{AtomicBool, Ordering};
        enum Fault {
            /// Committed, then failed before it applied.
            Unconfirmed,
            /// A row its fold writes is refused, after the fold.
            Rejected,
            /// Lost while planning, before it was accepted.
            Lost,
            /// Lost applying what it committed.
            Applied,
            Blob,
        }
        for fault in [
            Fault::Unconfirmed,
            Fault::Rejected,
            Fault::Lost,
            Fault::Applied,
            Fault::Blob,
        ] {
            // Whether a new session's first append commits, failing so.
            let first = match fault {
                Fault::Unconfirmed => Some(true),
                Fault::Rejected => Some(false),
                _ => None,
            };
            let (root, store, id, agent) = fresh().await;
            let sessions = root.path().join("sessions");
            let before = store.records();
            let mut attached = store.observe().updates;
            let interrupted = || SessionEvent::AgentInterrupted;
            let waiting = store.until(Wait::on([], |_| None::<()>));
            let mut waiting = Box::pin(waiting);
            assert!(futures_util::poll!(&mut waiting).is_pending());
            let live = Arc::new(AtomicBool::new(false));
            let journal = |agent: AgentId| {
                let (ran, entries) = (live.clone(), vec![(agent, interrupted())]);
                let live = move |_: &mut CoreState, _: &[Arc<EventRecord>]| {
                    ran.store(true, Ordering::SeqCst);
                };
                store.commit::<(), SessionError>(Command::Journal, move |_| {
                    Ok(Plan::new(entries, live))
                })
            };
            let (failed, durable) = match fault {
                Fault::Unconfirmed => {
                    store.fail_committed().await;
                    (journal(agent.clone()).await, true)
                }
                Fault::Rejected => {
                    let _refused = refuse(&store, "BEFORE INSERT ON agent_state");
                    (journal(agent.clone()).await, false)
                }
                Fault::Lost => {
                    let lost = |_| panic!("injected loss before acceptance");
                    let failed = store.append_then(agent.clone(), interrupted(), lost).await;
                    (failed.map(drop), false)
                }
                Fault::Applied => {
                    let entries = vec![(agent.clone(), interrupted())];
                    let ran = live.clone();
                    let applied = store.commit::<(), SessionError>(Command::Journal, move |_| {
                        Ok(Plan::new(entries, move |state, _| {
                            state.defer(move || ran.store(true, Ordering::SeqCst));
                            panic!("injected loss after commit")
                        }))
                    });
                    (applied.await, true)
                }
                Fault::Blob => {
                    let _refused = refuse(&store, "BEFORE INSERT ON blob");
                    (store.store_blob(b"refused").await.map(drop), false)
                }
            };
            let Err(SessionError::JournalFailed(failure)) = failed else {
                panic!("expected a journal failure: {failed:?}");
            };
            match failure.cause() {
                SessionError::Unconfirmed(_)
                | SessionError::WriterLost
                | SessionError::Database(_) => {}
                cause => panic!("unexpected cause: {cause:?}"),
            }
            let same = |result: Result<(), SessionError>| matches!(result, Err(SessionError::JournalFailed(later)) if later == failure);
            assert!(same(crate::tests::bounded(waiting).await));
            assert!(same(
                store.append(agent.clone(), interrupted()).await.map(drop)
            ));
            assert!(same(store.store_blob(b"later").await.map(drop)));
            assert!(same(store.drain().await));
            assert!(store.root().is_cancelled());
            let written = store
                .outputs()
                .lock()
                .batch("UPDATE blob SET bytes = bytes");
            assert!(written.is_err(), "the connection still takes writes");
            assert!(!live.load(Ordering::SeqCst), "a failed change applied live");
            assert_eq!(
                store.read(|state| state.phase().clone()),
                SessionPhase::Failed(failure.clone())
            );
            // An observation attached before the failure is told it no longer follows
            // the session; observing again shows what reopening does.
            let told = std::iter::from_fn(|| attached.try_recv().ok()).any(|update| {
                matches!(
                    update.event,
                    crate::agent::RuntimeEvent::Phase(SessionPhase::Failed(_))
                )
            });
            assert!(told, "the failure is published");
            let shown = |store: &SessionStore| {
                let root = |state: &CoreState| state.agent_states.get(&agent).cloned();
                let snapshot = store.observe().snapshot;
                (store.read(root), snapshot.applied, snapshot.requests)
            };
            let failed = shown(&store);
            assert!(same(store.close().await));
            let reopened = reopen(&sessions, id).await;
            assert_eq!(shown(&reopened), failed);
            let replay = reopened.records();
            assert_eq!(replay.len(), before.len() + usize::from(durable));
            let appended = reopened.append(agent.clone(), interrupted()).await.unwrap();
            assert_eq!(appended.sequence.get(), replay.len() as u64 + 1);
            assert!(same(store.append(agent, interrupted()).await.map(drop)));
            drop(reopened);
            if let Some(committed) = first {
                let first = SessionStore::create(&sessions).await.unwrap();
                let refused = (!committed).then(|| refuse(&first, "BEFORE INSERT ON entry"));
                if committed {
                    first.fail_committed().await;
                }
                let start = start_events(&AgentId::root(first.id()), root.path());
                assert!(first.append_all(start).await.is_err());
                assert!(first.close().await.is_err());
                assert_eq!(first.directory().exists(), committed);
                drop(refused);
            }
        }
    }

    /// A wait that a panicking predicate drops resolves with the failure the
    /// panic leaves, though it drops before the session reads failed: here,
    /// while the failure waits for the connection.
    #[tokio::test]
    async fn a_wait_a_panic_drops_resolves_with_its_failure() {
        struct Woken(tokio::sync::Notify);
        impl std::task::Wake for Woken {
            fn wake(self: Arc<Self>) {
                self.0.notify_one();
            }
        }
        let (_root, store, _, agent) = tests::fresh().await;
        let mut waiting = Box::pin(store.until(Wait::on([], |_| None::<()>)));
        assert!(futures_util::poll!(&mut waiting).is_pending());
        store.drain().await.unwrap();
        let woken = Arc::new(Woken(tokio::sync::Notify::new()));
        let waker = std::task::Waker::from(woken.clone());
        let mut poll = || (waiting.as_mut()).poll(&mut std::task::Context::from_waker(&waker));
        assert!(poll().is_pending(), "registered");
        // The predicate holds the connection, from another thread, before it panics.
        let (db, before) = (store.outputs(), store.read(CoreState::next_sequence));
        let held = Arc::new(std::sync::Barrier::new(2));
        let (release, released) = std::sync::mpsc::channel::<()>();
        let holding = held.clone();
        let holder = std::thread::spawn(move || {
            holding.wait();
            let _db = db.lock();
            holding.wait();
            let _ = released.recv();
        });
        let mut panicking = Box::pin(store.until(Wait::on(
            [Key::Agent(agent.clone())],
            move |state| {
                if state.next_sequence() > before {
                    held.wait();
                    held.wait();
                    panic!("injected predicate panic");
                }
                None::<()>
            },
        )));
        assert!(futures_util::poll!(&mut panicking).is_pending());
        store.drain().await.unwrap();
        let mut appending = Box::pin(store.append(agent, SessionEvent::AgentInterrupted));
        assert!(futures_util::poll!(&mut appending).is_pending());
        crate::tests::bounded(woken.0.notified()).await;
        assert!(poll().is_pending(), "answered before the failure");
        release.send(()).unwrap();
        holder.join().unwrap();
        let failed = |result| matches!(result, Err(SessionError::JournalFailed(_)));
        assert!(failed(crate::tests::bounded(waiting).await));
        assert!(failed(appending.await.map(drop)));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_under_traffic_stops_admission_and_frees_the_lease_for_reopen() {
        let (root, store, id, agent) = fresh().await;
        let sessions = root.path().join("sessions");
        let traffic = async {
            for _ in 0..3 {
                // Dropped waiters: one poll queues each commit ahead of `close`.
                let mut appending =
                    Box::pin(store.append(agent.clone(), SessionEvent::AgentInterrupted));
                let _ = futures_util::poll!(&mut appending);
            }
            store.close().await
        };
        let (blob, closed) = tokio::join!(store.store_blob(b"racing"), traffic);
        let blob = blob.unwrap();
        closed.unwrap();
        let late = store.append(agent, SessionEvent::AgentCompleted).await;
        assert!(matches!(
            late,
            Err(SessionError::Refused(SessionPhase::Closed))
        ));
        let reopened = reopen(&sessions, id).await;
        assert_eq!(reopened.records().len(), 5);
        let late = store.store_blob(b"late").await;
        assert!(matches!(
            late,
            Err(SessionError::Refused(SessionPhase::Closed))
        ));
        let current = reopened.store_blob(b"current").await.unwrap();
        for (blob, bytes) in [
            (blob, b"racing".as_slice()),
            (current, b"current".as_slice()),
        ] {
            assert_eq!(store.read_blob(&blob, usize::MAX).await.unwrap(), bytes);
        }
    }

    #[tokio::test]
    async fn owner_lock_excludes_competing_writers_but_not_readers() {
        let (root, store, id, agent) = fresh().await;
        let sessions = root.path().join("sessions");
        let missing = SessionId::from_bytes([9; 16]);
        assert!(!SessionStore::is_open(&sessions, missing).await.unwrap());
        store
            .append(agent.clone(), SessionEvent::AgentInterrupted)
            .await
            .unwrap();
        let path = sessions.join(id.to_string()).join(DATABASE_FILE);
        let before = std::fs::read(&path).unwrap();
        assert!(matches!(
            SessionStore::open(&sessions, id).await,
            Err(SessionError::AlreadyOpen(locked)) if locked == id
        ));
        assert!(SessionStore::is_open(&sessions, id).await.unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        // Readers see committed records while the owner is live.
        let read = SessionStore::read_records(&sessions, id).await.unwrap();
        assert_eq!(read, store.records());
        // Model a descriptor briefly inherited by a concurrently spawned process.
        let inherited_lock = store.inherit_lock().await.unwrap();
        drop(store);
        let store = reopen(&sessions, id).await;
        drop(inherited_lock);
        assert!(SessionStore::is_open(&sessions, id).await.unwrap());
        assert_eq!(store.records(), read);
        let appended = store
            .append(agent, SessionEvent::AgentCompleted)
            .await
            .unwrap();
        assert_eq!(store.records().last(), Some(&appended));
        drop(store);
        assert!(!SessionStore::is_open(&sessions, id).await.unwrap());
    }

    /// Resume continues a root whose conversation still awaits the model, whose
    /// latest request of any purpose never completed, or whose turn failed;
    /// otherwise it is idle. A child's conversation is its own.
    #[tokio::test]
    async fn summary_reads_how_the_root_would_resume() {
        use crate::agent::Failure;
        use crate::provider::protocol::{AssistantItem, ToolCall, ToolResult};
        let (root, store, id, agent) = fresh().await;
        let sessions = root.path().join("sessions");
        // The core, as resume reads it, agrees with the session list throughout.
        let stopped = async || {
            let listed = SessionStore::summary(&sessions, id).await.unwrap();
            let read = store.read(|state| state.stopped_turn(&agent));
            assert_eq!(read, listed.stopped);
            listed.stopped
        };
        let append = async |event| store.append(agent.clone(), event).await.unwrap().sequence;
        let commit = async |message| append(SessionEvent::MessageCommitted { message }).await;
        let text = |text: &str| Message::User(vec![UserPart::Text { text: text.into() }]);
        let answer = || Message::Assistant(vec![AssistantItem::text("answer", 0, "done")]);
        let interrupted = Some(TurnFailure::Interrupted);
        assert_eq!(stopped().await, None);
        commit(text("prompt")).await;
        assert_eq!(stopped().await, interrupted);
        let context = ModelContext::test(ModelPurpose::Agent, profile());
        let context = append(SessionEvent::ModelContext { context }).await;
        let request = async || {
            let request = append(requested(context)).await.request();
            append(attempt(request, 1)).await;
            request
        };
        request().await;
        let call = ToolCall::new("call", "exec", serde_json::json!({})).unwrap();
        commit(Message::Assistant(vec![AssistantItem::tool_call(
            "tool", 0, call,
        )]))
        .await;
        assert_eq!(stopped().await, interrupted);
        let result = ToolResult {
            call_id: "call".into(),
            name: "exec".into(),
            result: serde_json::json!({}),
            images: vec![],
            is_error: false,
        };
        commit(Message::Tool(result)).await;
        assert_eq!(stopped().await, interrupted);
        let second = request().await;
        let answered = commit(answer()).await;
        // An answer whose request never completed was cut short.
        assert_eq!(stopped().await, interrupted);
        let completed = SessionEvent::ResponseCompleted {
            attempt: AttemptRef {
                request: second,
                attempt: 1,
            },
            message: answered.message(),
            outcome: CompletedOutcome::Answer,
        };
        append(completed).await;
        assert_eq!(stopped().await, None);

        let context = ModelContext::test(ModelPurpose::Compaction, profile());
        let context = append(SessionEvent::ModelContext { context }).await;
        let summary = append(requested(context)).await.request();
        append(attempt(summary, 1)).await;
        assert_eq!(stopped().await, interrupted);
        let checkpoint = CompactionCheckpoint {
            frontier: answered,
            message: text("summary"),
            todos: Vec::new(),
            retained: Vec::new(),
            attempt: AttemptRef {
                request: summary,
                attempt: 1,
            },
            trigger: crate::session::CompactionTrigger::Occupancy(100),
        };
        append(SessionEvent::Compaction { checkpoint }).await;
        assert_eq!(stopped().await, None);
        let child = start_child(&store, &agent, 1, None, root.path()).await;
        let message = text("child prompt");
        let prompt = SessionEvent::MessageCommitted { message };
        store.append(child, prompt).await.unwrap();
        assert_eq!(stopped().await, None);

        // A failure stops only a turn still open: with nothing to continue it
        // leaves the root idle.
        let failure = Failure::Other("boom".into());
        let failed = || SessionEvent::AgentFailed {
            failure: failure.clone(),
        };
        append(failed()).await;
        assert_eq!(stopped().await, None);
        commit(text("again")).await;
        assert_eq!(stopped().await, interrupted);
        append(failed()).await;
        assert_eq!(stopped().await, Some(TurnFailure::Failed(failure.clone())));
    }

    /// The core validates a request on a connection of its own, so the request
    /// commits while long reads hold every pooled connection and more wait behind them.
    #[tokio::test]
    async fn a_request_commits_while_long_reads_hold_every_read_connection() {
        let (_root, store, _, agent) = fresh().await;
        let context = ModelContext::test(ModelPurpose::Agent, profile());
        let event = SessionEvent::ModelContext { context };
        let context = store.append(agent.clone(), event).await.unwrap().sequence;
        let gate = Arc::new(tokio::sync::RwLock::new(()));
        let closed = gate.clone().write_owned().await;
        let (started, mut reading) = tokio::sync::mpsc::unbounded_channel();
        let long_reads: Vec<_> = (0..=db::CONNECTIONS)
            .map(|_| {
                let (reads, gate, started) = (store.reads(), gate.clone(), started.clone());
                let read = move |_: &db::Db| {
                    started.send(()).unwrap();
                    drop(gate.blocking_read());
                    Ok(())
                };
                let held = reads.clone();
                tokio::spawn(async move {
                    held.hold(move || Ok::<_, SessionError>(reads.read(read)?))
                        .await
                })
            })
            .collect();
        for _ in 0..db::CONNECTIONS {
            crate::tests::bounded(reading.recv()).await.unwrap();
        }
        crate::tests::bounded(store.append(agent, requested(context)))
            .await
            .unwrap();
        drop(closed);
        for read in long_reads {
            read.await.unwrap().unwrap();
        }
    }

    /// The user's title holds until cleared, whatever automatic titles follow;
    /// otherwise the newest automatic title shows, and before any, the first prompt.
    /// Each reports where it came from.
    #[tokio::test]
    async fn summary_title_prefers_the_users_then_the_newest_automatic_one() {
        use TitleSource::{Generated, Prompt, User};
        let session = crate::tests::TestRuntime::new().await;
        let title = async |event| {
            let store = &session.store;
            store.append(session.agent.clone(), event).await.unwrap();
            store.title().await.unwrap()
        };
        let set = |title: &str, source| SessionEvent::TitleSet {
            title: title.into(),
            source,
        };
        let shown = |text: &str, source| {
            let text = text.into();
            Some(SessionTitle { text, source })
        };
        assert_eq!(session.store.title().await.unwrap(), None);
        let message = Message::User(vec![UserPart::Text {
            text: "first prompt".into(),
        }]);
        let committed = SessionEvent::MessageCommitted { message };
        assert_eq!(title(committed).await, shown("first prompt", Prompt));
        for (event, expected) in [
            (set("prompt", Prompt), shown("prompt", Prompt)),
            (set("generated", Generated), shown("generated", Generated)),
            (set("user", User), shown("user", User)),
            (set("later prompt", Prompt), shown("user", User)),
            (set("regenerated", Generated), shown("user", User)),
            (SessionEvent::TitleCleared, shown("regenerated", Generated)),
            (set("renamed", User), shown("renamed", User)),
        ] {
            assert_eq!(title(event).await, expected);
        }
    }

    #[tokio::test]
    async fn unsupported_databases_are_rejected_without_being_rewritten() {
        let (root, store, id, _agent) = fresh().await;
        let sessions = root.path().join("sessions");
        drop(store);
        let path = sessions.join(id.to_string()).join(DATABASE_FILE);
        for pragma in [
            "user_version = 23",
            "user_version = 3",
            "application_id = 1",
        ] {
            {
                let raw = libsql::Builder::new_local(&path).build();
                let raw = futures_util::FutureExt::now_or_never(raw).unwrap().unwrap();
                let connection = raw.connect().unwrap();
                futures_util::FutureExt::now_or_never(
                    connection.execute_batch(&format!("PRAGMA {pragma}")),
                )
                .unwrap()
                .unwrap();
            }
            let archive = std::fs::read(&path).unwrap();
            assert!(matches!(
                SessionStore::open(&sessions, id).await,
                Err(SessionError::UnsupportedVersion(_))
            ));
            assert!(matches!(
                SessionStore::read_records(&sessions, id).await,
                Err(SessionError::UnsupportedVersion(_))
            ));
            assert_eq!(std::fs::read(&path).unwrap(), archive);
        }
    }

    /// A job's status row says whether it stays resident from the transaction that
    /// changes it, through a restart, and the next open holds only the jobs it names.
    #[tokio::test]
    async fn status_rows_name_the_jobs_reopening_holds() {
        use crate::job::{JobEnd, JobRole, JobTransition};
        let (root, store, id, agent) = fresh().await;
        let resident = |store: &SessionStore| {
            let sql = "SELECT job FROM job_status WHERE resident ORDER BY job";
            let read = |db: &db::Db| db.query(sql, Vec::new(), |row| Ok(row.get::<u64>(0)?));
            store.test_reads().read(read).unwrap()
        };
        let held =
            |store: &SessionStore, job| store.read(|state| state.jobs.unheld([job]).is_empty());
        let job = JobId::new(1).unwrap();
        let append = async |event| store.append(agent.clone(), event).await.unwrap();
        let moved = |state| SessionEvent::JobStateChanged { job, state };
        let completed = || SessionEvent::JobFinished {
            job,
            state: JobEnd::Completed,
            diagnostic: None,
            images: Vec::new(),
        };
        append(job_created(job, JobRole::Tool)).await;
        append(moved(JobTransition::AwaitingApproval)).await;
        append(moved(JobTransition::Running)).await;
        append(completed()).await;
        assert_eq!(resident(&store), [1]);
        append(SessionEvent::JobClaimed { job }).await;
        assert_eq!(resident(&store), Vec::<u64>::new());
        assert!(!held(&store, job));
        append(moved(JobTransition::Running)).await;
        assert_eq!(resident(&store), [1]);
        append(completed()).await;
        append(SessionEvent::JobClaimed { job }).await;
        let running = JobId::new(2).unwrap();
        append(job_created(running, JobRole::Tool)).await;
        store.close().await.unwrap();
        let reopened = reopen(&root.path().join("sessions"), id).await;
        assert_eq!(resident(&reopened), [2]);
        assert!(!held(&reopened, job) && held(&reopened, running));
    }

    /// Each agent's entries count from 0 without gaps, across a reopen.
    #[tokio::test]
    async fn agent_ordinals_count_each_agents_entries_across_reopening() {
        let (root, store, id, agent) = fresh().await;
        let status = || SessionEvent::Status {
            message: "status".into(),
        };
        let child = start_child(&store, &agent, 1, None, root.path()).await;
        for agent in [&agent, &child, &agent] {
            store.append(agent.clone(), status()).await.unwrap();
        }
        store.close().await.unwrap();
        let store = reopen(&root.path().join("sessions"), id).await;
        let both = vec![(child, status()), (agent, status())];
        store.append_all(both).await.unwrap();
        let ordinals = "SELECT agent, agent_ordinal FROM entry ORDER BY seq";
        let read = |db: &db::Db| {
            db.query(ordinals, Vec::new(), |row| {
                Ok((row.get::<i64>(0)?, row.get::<u64>(1)?))
            })
        };
        let mut by_agent = std::collections::BTreeMap::<_, Vec<_>>::new();
        for (agent, ordinal) in store.test_reads().read(read).unwrap() {
            by_agent.entry(agent).or_default().push(ordinal);
        }
        assert_eq!(by_agent.len(), 2);
        for ordinals in by_agent.values() {
            assert!(
                ordinals.iter().copied().eq(0..ordinals.len() as u64),
                "{ordinals:?}"
            );
        }
    }
}
