//! Session persistence in one normalized SQLite database per session.
use chrono::Utc;
use fs2::FileExt;
use std::{
    fs::OpenOptions as StdOpenOptions,
    path::{Path, PathBuf},
    sync::{Arc, RwLock as StdRwLock},
};
use thiserror::Error;
use tokio::sync::{Mutex, broadcast, oneshot};

use crate::{
    agent::TurnFailure,
    identity::{AgentId, EventId, SessionId},
    provider::protocol::ModelRequest,
};

mod content;
mod db;
mod event;
mod ledger;
mod request;
pub mod stats;
mod template;
mod turns;

pub(crate) use template::ModelRequestTemplate;

pub use content::{JobEvent, Message, RuntimeState, StateJob, StateJobKind, UserPart};
pub(crate) use db::{CaptureExtent, CaptureRow, SharedDb};
pub use db::{DbError, SessionSummary, SessionTitle};
pub(crate) use event::EntryKind;
pub use event::{
    AttemptRef, CompactionCheckpoint, CompactionFailure, CompletedOutcome, EventRecord, MessageSeq,
    ModeSelection, ModelCallOrigin, ModelContext, ModelPurpose, ProfileSnapshot, RecordSeq,
    RequestSeq, SessionEvent, TitleSource, Truncation,
};
pub use ledger::{RequestChanges, RequestFailure, RequestLedger, RequestPhase, RequestRecord};
pub use request::{
    CheckpointError, Projection, ReplayError, project_history, reconstruct_model_request,
    record_at, render_history, request_context,
};
pub use turns::Turns;

/// The database schema version; earlier formats are intentionally unsupported.
pub const SESSION_FORMAT_VERSION: i64 = db::USER_VERSION;
const DATABASE_FILE: &str = "session.db";
const LOCK_FILE: &str = "lock";
/// Committed records an observer may fall behind by before it lags.
const PUBLICATION_CAPACITY: usize = 512;

/// The mode definitions the session has pinned, in order of first use.
pub fn pinned_modes(
    records: &[EventRecord],
) -> impl Iterator<Item = (&crate::tool::policy::ModeName, &crate::tool::policy::Mode)> {
    let selections = records.iter().filter_map(|record| match &record.event {
        SessionEvent::AgentStarted { mode, .. } => mode.as_ref(),
        SessionEvent::ModeChanged { mode, .. } => Some(mode),
        _ => None,
    });
    selections.filter_map(|mode| Some((&mode.name, mode.definition.as_ref()?)))
}

struct SessionLock(std::fs::File);

impl Drop for SessionLock {
    fn drop(&mut self) {
        // Ownership ends with the store, not with a transient fork-inherited fd.
        let _ = FileExt::unlock(&self.0);
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
    lock.try_lock_exclusive()
        .or_else(|_| {
            std::thread::sleep(LOCK_RETRY);
            lock.try_lock_exclusive()
        })
        .map_err(|_| SessionError::AlreadyOpen(id))?;
    Ok(SessionLock(lock))
}

enum WriterHealth {
    Healthy,
    NeedsRecovery(AppendRecovery),
}

/// Reconciliation key for one accepted append. A failed receipt is not
/// permission to resubmit: reopening must resolve the exact event first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AppendIdentity {
    pub event: EventId,
    pub session: SessionId,
    pub sequence: RecordSeq,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[error(
    "session {} event {} sequence {}: {reason}",
    identity.session,
    identity.event,
    identity.sequence
)]
pub struct AppendRecovery {
    pub identity: AppendIdentity,
    pub reason: RecoveryReason,
}

/// Why an accepted append's outcome is unknown.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum RecoveryReason {
    #[error("accepted append did not finish publication")]
    Unpublished,
    #[error("accepted writer lost its receipt")]
    ReceiptLost,
    #[error("accepted append failed to commit")]
    Commit,
}

/// Receipt for an accepted append; dropping it does not cancel the writer. A
/// retrying producer keeps its identity until a record or reopen resolves it.
#[derive(Debug)]
pub struct AcceptedAppend {
    identity: AppendIdentity,
    committed: Receipt,
}

impl AcceptedAppend {
    #[must_use]
    pub fn identity(&self) -> AppendIdentity {
        self.identity
    }

    pub async fn committed(self) -> Result<EventRecord, SessionError> {
        let mut records = receipt(self.identity, self.committed).await?;
        Ok(records.pop().expect("one record per accepted event"))
    }
}

type Receipt = oneshot::Receiver<Result<Vec<EventRecord>, SessionError>>;

async fn receipt(
    identity: AppendIdentity,
    committed: Receipt,
) -> Result<Vec<EventRecord>, SessionError> {
    committed.await.unwrap_or_else(|_| {
        Err(SessionError::AppendIndeterminate {
            recovery: AppendRecovery {
                identity,
                reason: RecoveryReason::ReceiptLost,
            },
            source: None,
        })
    })
}

/// Test hook for the next accepted append.
#[cfg(test)]
pub(crate) enum CommitFault {
    /// Stop at the boundary until resumed.
    Pause(AppendBoundary, oneshot::Sender<()>, oneshot::Receiver<()>),
    /// Report failure at the boundary.
    Fail(AppendBoundary),
    /// Lose the writer thread before COMMIT.
    Panic,
}

/// Where a test pauses or fails the next accepted append.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AppendBoundary {
    /// Accepted, before COMMIT: failure leaves nothing durable.
    Write,
    /// Committed, before publication: failure leaves the append durable.
    Publication,
}

#[cfg(test)]
impl AppendBoundary {
    pub(crate) const ALL: [Self; 2] = [Self::Write, Self::Publication];
}

type Acceptance = oneshot::Sender<Result<Vec<AppendIdentity>, SessionError>>;

/// Builds events that reference the first entry's sequence, in the same transaction.
type Follow = Box<dyn FnOnce(RecordSeq) -> Vec<(AgentId, SessionEvent)> + Send>;

/// When an append's entries are dated.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Dated {
    /// When the writer accepts them.
    Now,
    /// At the session's newest activity entry, so they do not advance its last
    /// activity; entry times need not follow sequence order. What reopening or
    /// closing a session settles is dated so, as the stopped work it ends.
    LastActivity,
}

struct State {
    records: Vec<EventRecord>,
    health: WriterHealth,
    closed: bool,
}

impl State {
    /// A poisoned writer's cached records cannot resolve an uncertain attempt.
    fn require_healthy(&self) -> Result<(), SessionError> {
        match &self.health {
            WriterHealth::Healthy => Ok(()),
            WriterHealth::NeedsRecovery(recovery) => {
                Err(SessionError::AppendUnavailable(*recovery))
            }
        }
    }
}

struct Shared {
    id: SessionId,
    state: StdRwLock<State>,
    events: broadcast::Sender<EventRecord>,
}

impl Shared {
    fn read(&self) -> std::sync::RwLockReadGuard<'_, State> {
        self.state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, State> {
        self.state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The only read-write connection. Operations take it FIFO and run on the
/// blocking pool, so no tokio worker waits on fsync and a dropped caller
/// cannot cancel accepted work.
struct Writer {
    db: db::SharedDb,
    lock: Option<SessionLock>,
    shared: Arc<Shared>,
    encoder: db::Encoder,
    #[cfg(test)]
    fault: Option<CommitFault>,
}

impl Writer {
    /// Accept, then commit; returns the commit result for the receipt, or a
    /// rejection for the caller to report once the writer is released.
    fn append(
        &mut self,
        entries: Vec<(AgentId, SessionEvent)>,
        follow: Option<Follow>,
        dated: Dated,
        accepted: Acceptance,
    ) -> Result<Result<Vec<EventRecord>, SessionError>, (Acceptance, SessionError)> {
        // Output writers share the connection; hold it from BEGIN through COMMIT.
        let connection = self.db.clone();
        let db = connection.lock();
        let records = match self.accept(&db, entries, follow, dated) {
            Ok(records) => records,
            Err(error) => return Err((accepted, error)),
        };
        let identities: Vec<_> = records.iter().map(EventRecord::append_identity).collect();
        let identity = *identities.last().expect("appends carry at least one entry");
        let _ = accepted.send(Ok(identities));
        // Pessimistic poison also survives a lost writer task.
        self.shared.write().health = WriterHealth::NeedsRecovery(AppendRecovery {
            identity,
            reason: RecoveryReason::Unpublished,
        });
        #[cfg(test)]
        {
            self.pause_at(AppendBoundary::Write);
            match self.fault.take() {
                // Without its transaction, the COMMIT below fails for real.
                Some(CommitFault::Fail(AppendBoundary::Write)) => drop(db.rollback()),
                Some(CommitFault::Fail(AppendBoundary::Publication)) => {
                    let _ = db.commit();
                    let reason = RecoveryReason::Unpublished;
                    return Ok(Err(SessionError::AppendIndeterminate {
                        recovery: AppendRecovery { identity, reason },
                        source: None,
                    }));
                }
                Some(CommitFault::Panic) => panic!("injected writer loss"),
                pause => self.fault = pause,
            }
        }
        Ok(match db.commit() {
            Ok(()) => {
                #[cfg(test)]
                self.pause_at(AppendBoundary::Publication);
                let mut state = self.shared.write();
                for record in &records {
                    state.records.push(record.clone());
                    let _ = self.shared.events.send(record.clone());
                }
                state.health = WriterHealth::Healthy;
                Ok(records)
            }
            Err(source) => {
                let _ = db.rollback();
                self.encoder.reset();
                let recovery = AppendRecovery {
                    identity,
                    reason: RecoveryReason::Commit,
                };
                self.shared.write().health = WriterHealth::NeedsRecovery(recovery);
                Err(SessionError::AppendIndeterminate {
                    recovery,
                    source: Some(source),
                })
            }
        })
    }

    #[cfg(test)]
    fn pause_at(&mut self, boundary: AppendBoundary) {
        if matches!(&self.fault, Some(CommitFault::Pause(at, ..)) if *at == boundary)
            && let Some(CommitFault::Pause(_, reached, resume)) = self.fault.take()
        {
            let _ = reached.send(());
            let _ = resume.blocking_recv();
        }
    }

    fn read_blob(
        &self,
        blob: crate::media::BlobRef,
        limit: usize,
    ) -> Result<Vec<u8>, SessionError> {
        usize::try_from(blob.bytes)
            .ok()
            .filter(|&length| length <= limit)
            .ok_or(BlobError::TooLarge.at(blob.sha256))?;
        let bytes = self
            .db
            .lock()
            .query_row(
                "SELECT bytes FROM blob WHERE sha256 = ?1",
                db::params![blob.sha256.to_bytes().to_vec()],
                |row| Ok(row.get::<Vec<u8>>(0)?),
            )?
            .ok_or(BlobError::Missing.at(blob.sha256))?;
        if crate::media::BlobDigest::of(&bytes) != blob.sha256 {
            return Err(BlobError::HashMismatch.at(blob.sha256));
        }
        if bytes.len() as u64 != blob.bytes {
            return Err(BlobError::LengthMismatch.at(blob.sha256));
        }
        Ok(bytes)
    }

    /// Validate and encode inside an open transaction; any error is a definite rejection.
    fn accept(
        &mut self,
        db: &db::Db,
        mut entries: Vec<(AgentId, SessionEvent)>,
        follow: Option<Follow>,
        dated: Dated,
    ) -> Result<Vec<EventRecord>, SessionError> {
        let state = self.shared.read();
        state.require_healthy()?;
        if state.closed {
            return Err(SessionError::Closed);
        }
        let first = state
            .records
            .last()
            .map_or(RecordSeq::new(1), |record| record.sequence.next());
        let now = || Utc::now().timestamp_millis();
        let timestamp_millis = match dated {
            Dated::Now => now(),
            Dated::LastActivity => (state.records.iter().rev())
                .find(|record| record.event.is_activity())
                .map_or_else(now, |record| record.timestamp_millis),
        };
        if let Some(follow) = follow {
            entries.extend(follow(first));
        }
        let mut records = Vec::with_capacity(entries.len());
        // A mode's first use pins its definition. Appends are serialized here, so
        // agents applying one mode at once still pin it once.
        let mut pinned: Option<std::collections::HashSet<crate::tool::policy::ModeName>> = None;
        for (offset, (agent, mut event)) in entries.into_iter().enumerate() {
            if agent.session() != self.shared.id {
                return Err(SessionError::WrongSession);
            }
            if let SessionEvent::AgentStarted {
                mode: Some(mode), ..
            }
            | SessionEvent::ModeChanged { mode, .. } = &mut event
                && mode.definition.is_some()
            {
                let pinned = pinned.get_or_insert_with(|| {
                    pinned_modes(&state.records)
                        .map(|(name, _)| name.clone())
                        .collect()
                });
                if !pinned.insert(mode.name.clone()) {
                    mode.definition = None;
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
        drop(state);
        let encoder = &mut self.encoder;
        let result = db.transaction(|| encoder.records(db, &records));
        if result.is_err() {
            self.encoder.reset();
        }
        result?;
        Ok(records)
    }
}

struct StoreInner {
    shared: Arc<Shared>,
    db: SharedDb,
    directory: PathBuf,
    writer: Arc<Mutex<Writer>>,
    turn: Arc<Mutex<()>>,
}

/// The writer, taken FIFO. The writer reference drops before the turn ends, so
/// the next turn (notably `close`) never sees an earlier holder keep the lease alive.
struct Turn {
    writer: tokio::sync::OwnedMutexGuard<Writer>,
    _turn: tokio::sync::OwnedMutexGuard<()>,
}

/// Keeps closures from capturing the writer without the turn.
impl Drop for Turn {
    fn drop(&mut self) {}
}

#[derive(Clone)]
pub struct SessionStore {
    inner: Arc<StoreInner>,
}

impl SessionStore {
    #[cfg(test)]
    pub(crate) async fn fault_next_commit(&self, fault: CommitFault) {
        self.turn().await.writer.fault = Some(fault);
    }

    #[cfg(test)]
    pub(crate) async fn pause_append_at(
        &self,
        boundary: AppendBoundary,
    ) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (reached, waiting) = oneshot::channel();
        let (resume_sender, resume) = oneshot::channel();
        self.fault_next_commit(CommitFault::Pause(boundary, reached, resume))
            .await;
        (waiting, resume_sender)
    }

    #[cfg(test)]
    pub(crate) async fn fail_append_at(&self, boundary: AppendBoundary) {
        self.fault_next_commit(CommitFault::Fail(boundary)).await;
    }

    /// Replace a stored blob's bytes, or delete it, as disk corruption would.
    #[cfg(test)]
    pub(crate) async fn corrupt_blob(
        &self,
        blob: crate::media::BlobDigest,
        bytes: Option<Vec<u8>>,
    ) {
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
        let turn = self.turn().await;
        turn.writer
            .lock
            .as_ref()
            .and_then(|lock| lock.0.try_clone().ok())
    }

    /// Read a session without taking its lock; a live owner may keep writing.
    pub async fn read_records(
        root: &Path,
        id: SessionId,
    ) -> Result<Vec<EventRecord>, SessionError> {
        Self::read_only(root, id, move |db| db::decode_records(db, id))
            .await
            .and_then(request::admit_records)
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
            match FileExt::try_lock_shared(&lock) {
                Ok(()) => {
                    FileExt::unlock(&lock)?;
                    Ok(false)
                }
                Err(error) if error.kind() == fs2::lock_contended_error().kind() => Ok(true),
                Err(error) => Err(error.into()),
            }
        })
        .await?
    }

    /// A session list row, without decoding the session or taking its lock.
    pub async fn summary(root: &Path, id: SessionId) -> Result<SessionSummary, SessionError> {
        Self::read_only(root, id, db::summary).await
    }

    /// Run `read` against one snapshot: a live owner may commit between its queries.
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
            let db = db::Db::open(&path, db::OpenMode::ReadOnly)?;
            db.batch("BEGIN")?;
            let value = read(&db);
            db.batch("COMMIT")?;
            Ok(value?)
        })
        .await?
    }

    pub async fn create(root: &Path) -> Result<Self, SessionError> {
        Self::create_with_durability(root, true).await
    }

    #[cfg(test)]
    pub(crate) async fn create_ephemeral(root: &Path) -> Result<Self, SessionError> {
        Self::create_with_durability(root, false).await
    }

    async fn create_with_durability(root: &Path, durable: bool) -> Result<Self, SessionError> {
        tokio::fs::create_dir_all(root).await?;
        let id = SessionId::generate()?;
        let directory = root.join(id.to_string());
        tokio::fs::create_dir(&directory).await?;
        let (store, _) = Self::start(id, directory, durable, false).await?;
        Ok(store)
    }

    pub async fn open(
        root: &Path,
        id: SessionId,
    ) -> Result<(Self, Vec<EventRecord>), SessionError> {
        Self::start(id, root.join(id.to_string()), true, true).await
    }

    async fn start(
        id: SessionId,
        directory: PathBuf,
        durable: bool,
        existing: bool,
    ) -> Result<(Self, Vec<EventRecord>), SessionError> {
        let (shared_directory, open_directory) = (directory.clone(), directory.clone());
        let (db, lock, records) = blocking(move || {
            let path = open_directory.join(DATABASE_FILE);
            if existing && !path.is_file() {
                return Err(SessionError::NotFound(id));
            }
            // Acquire ownership before opening a potentially active database.
            let lock = durable
                .then(|| lock_session(&open_directory, id))
                .transpose()?;
            let mode = match (durable, existing) {
                (false, _) => db::OpenMode::Memory,
                (true, false) => db::OpenMode::Create,
                (true, true) => db::OpenMode::Open,
            };
            let db = db::Db::open(&path, mode)?;
            let records = request::admit_records(db::decode_records(&db, id)?)?;
            Ok::<_, SessionError>((db, lock, records))
        })
        .await??;
        let (events, _) = broadcast::channel(PUBLICATION_CAPACITY);
        let shared = Arc::new(Shared {
            id,
            state: StdRwLock::new(State {
                records: records.clone(),
                health: WriterHealth::Healthy,
                closed: false,
            }),
            events,
        });
        let db = SharedDb::new(db);
        let writer = Writer {
            db: db.clone(),
            lock,
            shared: shared.clone(),
            encoder: db::Encoder::default(),
            #[cfg(test)]
            fault: None,
        };
        Ok((
            Self {
                inner: Arc::new(StoreInner {
                    shared,
                    db,
                    directory: shared_directory,
                    writer: Arc::new(Mutex::new(writer)),
                    turn: Arc::default(),
                }),
            },
            records,
        ))
    }

    /// Run `work` on the writer in FIFO order on the blocking pool.
    async fn with_writer<T: Send + 'static>(
        &self,
        work: impl FnOnce(&mut Writer) -> T + Send + 'static,
    ) -> Result<T, SessionError> {
        let turn = self.turn().await;
        blocking(move || {
            // Own the whole turn: a cancelled caller must not end it early.
            let mut turn = turn;
            work(&mut turn.writer)
        })
        .await
    }

    async fn turn(&self) -> Turn {
        let _turn = self.inner.turn.clone().lock_owned().await;
        let writer = self.inner.writer.clone().try_lock_owned();
        let writer = writer.expect("only the turn holder locks the writer");
        Turn { writer, _turn }
    }

    #[must_use]
    pub fn id(&self) -> SessionId {
        self.inner.shared.id
    }

    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.inner.directory
    }

    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<EventRecord> {
        self.inner.shared.events.subscribe()
    }

    /// A consistent snapshot of all successfully committed events, including ephemeral sessions.
    pub async fn records(&self) -> Vec<EventRecord> {
        self.inner.shared.read().records.clone()
    }

    /// Committed records, refused while an accepted append needs recovery (reopen first).
    /// Waits out in-flight appends: their pessimistic poison lasts until publication.
    #[cfg(test)]
    async fn reconciled_records(&self) -> Result<Vec<EventRecord>, SessionError> {
        let _turn = self.inner.turn.lock().await;
        let state = self.inner.shared.read();
        state.require_healthy()?;
        Ok(state.records.clone())
    }

    /// Visit a consistent committed suffix without cloning event payloads; the
    /// default sequence visits the whole journal. Publication waits for `visit`.
    pub(crate) async fn visit_records_after<T>(
        &self,
        sequence: RecordSeq,
        visit: impl FnOnce(&[EventRecord]) -> T,
    ) -> T {
        let state = self.inner.shared.read();
        let start = state
            .records
            .partition_point(|record| record.sequence <= sequence);
        visit(&state.records[start..])
    }

    /// Accept and await one append; dropping this waiter does not cancel persistence.
    pub async fn append(
        &self,
        agent: AgentId,
        event: SessionEvent,
    ) -> Result<EventRecord, SessionError> {
        self.accept_append(agent, event).await?.committed().await
    }

    /// Accept and await several events committed in one transaction.
    pub async fn append_all(
        &self,
        events: Vec<(AgentId, SessionEvent)>,
    ) -> Result<Vec<EventRecord>, SessionError> {
        self.append_dated(events, Dated::Now).await
    }

    /// Accept and await several events committed in one transaction, dated `dated`.
    pub(crate) async fn append_dated(
        &self,
        events: Vec<(AgentId, SessionEvent)>,
        dated: Dated,
    ) -> Result<Vec<EventRecord>, SessionError> {
        self.commit_batch(events, None, dated).await
    }

    /// Commit `event` with the events `follow` builds from its sequence, in one
    /// transaction; a response and the records that reference it land together.
    pub async fn append_then(
        &self,
        agent: AgentId,
        event: SessionEvent,
        follow: impl FnOnce(RecordSeq) -> Vec<(AgentId, SessionEvent)> + Send + 'static,
    ) -> Result<Vec<EventRecord>, SessionError> {
        self.commit_batch(vec![(agent, event)], Some(Box::new(follow)), Dated::Now)
            .await
    }

    async fn commit_batch(
        &self,
        events: Vec<(AgentId, SessionEvent)>,
        follow: Option<Follow>,
        dated: Dated,
    ) -> Result<Vec<EventRecord>, SessionError> {
        let (identities, committed) = self.accept(events, follow, dated).await?;
        let identity = *identities.last().expect("appends carry at least one entry");
        receipt(identity, committed).await
    }

    /// Validate and accept one append; an error means it was not accepted. The
    /// commit completes regardless of the caller.
    pub async fn accept_append(
        &self,
        agent: AgentId,
        event: SessionEvent,
    ) -> Result<AcceptedAppend, SessionError> {
        let (mut identities, committed) =
            self.accept(vec![(agent, event)], None, Dated::Now).await?;
        Ok(AcceptedAppend {
            identity: identities.pop().expect("one identity per entry"),
            committed,
        })
    }

    async fn accept(
        &self,
        entries: Vec<(AgentId, SessionEvent)>,
        follow: Option<Follow>,
        dated: Dated,
    ) -> Result<(Vec<AppendIdentity>, Receipt), SessionError> {
        let (accepted, acceptance) = oneshot::channel();
        let (committed, receipt) = oneshot::channel();
        let mut turn = self.turn().await;
        tokio::task::spawn_blocking(move || {
            // Release the writer before any report, even when the writer panics, so
            // a caller that closes and reopens on the answer finds the lock free.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                turn.writer.append(entries, follow, dated, accepted)
            }));
            drop(turn);
            match result {
                Ok(Ok(result)) => drop(committed.send(result)),
                Ok(Err((accepted, error))) => drop(accepted.send(Err(error))),
                Err(panic) => {
                    drop(committed);
                    std::panic::resume_unwind(panic)
                }
            }
        });
        // A dropped sender means the writer was lost before acceptance: nothing is durable.
        let identities = acceptance.await.map_err(|_| SessionError::WriterLost)??;
        Ok((identities, receipt))
    }

    /// Await accepted appends and report recovery-required state without closing admission.
    pub async fn drain(&self) -> Result<(), SessionError> {
        let _turn = self.inner.turn.lock().await;
        self.inner.shared.read().require_healthy()
    }

    /// Stop admission, wait out operations already queued, release the session
    /// for another open, and report recovery-required state. Retained handles,
    /// including output streams, can still read but can no longer write.
    pub async fn close(&self) -> Result<(), SessionError> {
        self.with_writer(|writer| {
            // Taking the connection rolls back a transaction a panicked holder left
            // open, which would otherwise keep the next owner from writing.
            let db = writer.db.lock();
            db.close_writes()?;
            let mut state = writer.shared.write();
            state.closed = true;
            writer.lock = None;
            state.require_healthy()
        })
        .await?
    }

    /// The title the session list shows: see [`SessionSummary::title`].
    pub async fn title(&self) -> Result<Option<SessionTitle>, SessionError> {
        let db = self.inner.db.clone();
        Ok(blocking(move || db::title(&db.lock())).await??)
    }

    /// Why the root agent's journaled turn stopped short of its answer, as resuming
    /// continues it: see [`SessionSummary::stopped`].
    pub async fn stopped_turn(&self) -> Result<Option<TurnFailure>, SessionError> {
        let db = self.inner.db.clone();
        Ok(blocking(move || db::stopped_turn(&db.lock())).await??)
    }

    /// The connection job output streams read and write through.
    pub(crate) fn outputs(&self) -> SharedDb {
        self.inner.db.clone()
    }

    /// Store bytes in the content-addressed blob store.
    pub async fn store_blob(&self, bytes: &[u8]) -> Result<crate::media::BlobRef, SessionError> {
        let blob = crate::media::BlobRef::of(bytes);
        let bytes = bytes.to_vec();
        self.with_writer(move |writer| {
            writer.db.lock().execute(
                "INSERT INTO blob (sha256, bytes) VALUES (?1, ?2) ON CONFLICT (sha256) DO NOTHING",
                db::params![blob.sha256.to_bytes().to_vec(), bytes],
            )
        })
        .await??;
        Ok(blob)
    }

    /// Read a blob bounded by `limit`, verified against its digest and size.
    pub async fn read_blob(
        &self,
        blob: &crate::media::BlobRef,
        limit: usize,
    ) -> Result<Vec<u8>, SessionError> {
        let blob = *blob;
        self.with_writer(move |writer| writer.read_blob(blob, limit))
            .await?
    }

    pub async fn store_image(
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

    pub async fn store_attachment(
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
            let read = move |writer: &mut Writer| -> Result<Vec<_>, SessionError> {
                let read = |blob| Ok((blob, writer.read_blob(blob, limit)?));
                missing.into_iter().map(read).collect()
            };
            for (blob, bytes) in self.with_writer(read).await?? {
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

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, SessionError> {
    let joined = tokio::task::spawn_blocking(work).await;
    joined.map_err(SessionError::Task)
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

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("model request template must not contain conversation history")]
    TemplateHistory,
    #[error("accepted append is indeterminate; recovery required: {recovery}")]
    AppendIndeterminate {
        recovery: AppendRecovery,
        /// The database's report when the commit itself failed.
        #[source]
        source: Option<DbError>,
    },
    #[error("append rejected: writer requires recovery of prior attempt: {0}")]
    AppendUnavailable(AppendRecovery),
    #[error("session writer is closed")]
    Closed,
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
    Task(#[source] tokio::task::JoinError),
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
    #[error("blob `{digest}` {reason}")]
    Blob {
        digest: crate::media::BlobDigest,
        reason: BlobError,
    },
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
        execution::ExecutionLocation,
        identity::JobId,
        provider::{profile::ModelProfile, protocol::Usage},
        tool::policy::Capability,
    };

    /// An in-memory session whose root agent has started.
    pub(crate) struct MemorySession {
        /// Workspace directory; the database itself is in memory.
        pub root: tempfile::TempDir,
        pub store: SessionStore,
        pub agent: AgentId,
    }

    impl MemorySession {
        pub(crate) async fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let store = SessionStore::create_ephemeral(root.path()).await.unwrap();
            let agent = started(&store, root.path()).await;
            Self { root, store, agent }
        }

        /// Start `parent`'s child `index`, optionally owned by `owner_job`.
        pub(crate) async fn start_child(
            &self,
            parent: &AgentId,
            index: u32,
            owner_job: Option<JobId>,
        ) -> AgentId {
            start_child(&self.store, parent, index, owner_job, self.root.path()).await
        }
    }

    /// An on-disk session whose root agent has started, for tests that reopen it.
    pub(crate) async fn on_disk() -> (tempfile::TempDir, SessionStore, AgentId) {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::create(root.path()).await.unwrap();
        let agent = started(&store, root.path()).await;
        (root, store, agent)
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
                    capabilities: Capability::ALL.to_vec(),
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
        Usage {
            input_tokens: input,
            cached_input_tokens: cached,
            cache_write_input_tokens: 0,
            output_tokens: output,
        }
    }

    /// A request against `context` with no checkpoint or history range.
    pub(crate) fn requested(context: RecordSeq) -> SessionEvent {
        SessionEvent::ModelRequested {
            context,
            checkpoint: None,
            through: None,
            tail: Vec::new(),
            history_lifetime: Default::default(),
        }
    }

    pub(crate) fn attempt(request: RequestSeq, attempt: u64) -> SessionEvent {
        SessionEvent::ModelAttemptStarted(AttemptRef { request, attempt })
    }

    pub(crate) fn record(agent: &AgentId, sequence: u64, event: SessionEvent) -> EventRecord {
        EventRecord {
            id: EventId::generate().unwrap(),
            sequence: sequence.into(),
            timestamp_millis: 0,
            agent: agent.clone(),
            event,
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
            profile: Some(profile()),
            available_depth: 0,
            mode: None,
            capabilities: vec![Capability::Read],
            location,
        }
    }

    async fn fresh() -> (tempfile::TempDir, SessionStore, SessionId, AgentId) {
        let (root, store, agent) = on_disk().await;
        let id = store.id();
        (root, store, id, agent)
    }

    #[tokio::test]
    async fn accepted_append_survives_lost_waiter_and_close_drains() {
        let (root, store, id, agent) = fresh().await;
        let mut events = store.subscribe();
        let (reached, resume) = store.pause_append_at(AppendBoundary::Write).await;
        let accepted = store.accept_append(agent.clone(), SessionEvent::AgentInterrupted);
        let accepted = accepted.await.unwrap();
        let identity = accepted.identity();
        reached.await.unwrap();
        assert!(
            events.try_recv().is_err(),
            "publication must follow the commit"
        );
        drop(accepted);
        let mut closing = Box::pin(store.close());
        let pending = futures_util::poll!(&mut closing).is_pending();
        assert!(pending, "close must drain accepted work");
        resume.send(()).unwrap();
        crate::tests::bounded(closing).await.unwrap();
        let record = events.recv().await.unwrap();
        assert_eq!(record.sequence, identity.sequence);
        assert_eq!(store.records().await.last(), Some(&record));
        let closed = store.append(agent, SessionEvent::AgentCompleted).await;
        assert!(matches!(closed, Err(SessionError::Closed)));
        drop(store);
        let (_, replay) = SessionStore::open(root.path(), id).await.unwrap();
        assert_eq!(replay.last(), Some(&record));
    }

    /// An in-flight append's pessimistic poison is not a failure: reconciled reads
    /// wait for publication instead of reporting recovery.
    #[tokio::test]
    async fn reconciled_records_wait_out_in_flight_append() {
        for boundary in AppendBoundary::ALL {
            let (_root, store, _id, agent) = fresh().await;
            let (reached, resume) = store.pause_append_at(boundary).await;
            let accepted = store
                .accept_append(agent.clone(), SessionEvent::AgentInterrupted)
                .await
                .unwrap();
            reached.await.unwrap();
            let mut reading = Box::pin(store.reconciled_records());
            let mut draining = Box::pin(store.drain());
            assert!(
                futures_util::poll!(&mut reading).is_pending(),
                "{boundary:?}: read must wait for publication"
            );
            assert!(
                futures_util::poll!(&mut draining).is_pending(),
                "{boundary:?}: drain must wait for publication"
            );
            resume.send(()).unwrap();
            let record = accepted.committed().await.unwrap();
            let records = crate::tests::bounded(reading).await.unwrap();
            assert_eq!(records.last(), Some(&record), "{boundary:?}");
            crate::tests::bounded(draining).await.unwrap();
        }
    }

    /// A writer lost before acceptance leaves nothing durable and must not blame
    /// another append's recovery; the store keeps accepting afterwards.
    #[tokio::test]
    async fn writer_lost_before_acceptance_is_not_a_recovery() {
        let (_root, store, _id, agent) = fresh().await;
        let lost = store
            .append_then(agent.clone(), SessionEvent::AgentInterrupted, |_| {
                panic!("injected loss before acceptance")
            })
            .await;
        assert!(
            matches!(lost, Err(SessionError::WriterLost)),
            "unexpected result: {lost:?}"
        );
        let before = store.reconciled_records().await.unwrap().len();
        store
            .append(agent, SessionEvent::AgentCompleted)
            .await
            .unwrap();
        assert_eq!(store.reconciled_records().await.unwrap().len(), before + 1);
    }

    /// Failures after acceptance poison the writer with the exact recovery identity,
    /// keeping a failed commit's database error; reopening resolves the append from
    /// what the database actually committed, and the new owner can write while the
    /// closed handle lives on.
    #[tokio::test]
    async fn failures_poison_the_writer_and_reopen_resolves_the_commit() {
        use {AppendBoundary::*, RecoveryReason::*};
        for (fault, reason) in [
            (CommitFault::Fail(Write), Commit),
            (CommitFault::Fail(Publication), Unpublished),
            (CommitFault::Panic, ReceiptLost),
        ] {
            let durable = reason == Unpublished;
            let (root, store, id, agent) = fresh().await;
            let before = store.records().await.len();
            store.fault_next_commit(fault).await;
            let accepted = store.accept_append(agent.clone(), SessionEvent::AgentInterrupted);
            let accepted = accepted.await.unwrap();
            let identity = accepted.identity();
            let recovery_is = |result: Result<_, SessionError>, indeterminate: bool| match result {
                Err(SessionError::AppendIndeterminate { recovery, source }) if indeterminate => {
                    recovery == AppendRecovery { identity, reason }
                        && source.is_some() == (reason == Commit)
                }
                Err(SessionError::AppendUnavailable(recovery)) if !indeterminate => {
                    recovery.identity == identity
                }
                _ => false,
            };
            assert!(recovery_is(accepted.committed().await.map(drop), true));
            assert_eq!(store.records().await.len(), before);
            assert!(recovery_is(
                store.reconciled_records().await.map(drop),
                false
            ));
            assert!(matches!(
                store.close().await,
                Err(SessionError::AppendUnavailable(_))
            ));
            let (reopened, replay) = SessionStore::open(root.path(), id).await.unwrap();
            assert_eq!(replay.len(), before + usize::from(durable));
            assert_eq!(
                replay.iter().any(|record| record.id == identity.event),
                durable
            );
            assert_eq!(reopened.reconciled_records().await.unwrap(), replay);
            let next = reopened
                .append(agent.clone(), SessionEvent::AgentCompleted)
                .await
                .unwrap();
            assert_eq!(next.sequence.get(), replay.len() as u64 + 1);
            assert_ne!(next.id, identity.event);
            let retry = store.append(agent, SessionEvent::AgentCompleted).await;
            assert!(recovery_is(retry.map(drop), false));
        }
    }

    #[tokio::test]
    async fn rejected_appends_are_definite_and_leave_the_writer_healthy() {
        let (_root, store, _id, agent) = fresh().await;
        let before = store.records().await;
        // A child that never started owns no rows; its entry is rejected by the schema.
        let unknown = store
            .append(agent.child(9), SessionEvent::AgentCompleted)
            .await;
        assert!(matches!(
            unknown,
            Err(SessionError::Database(DbError::Sql(_))
                | SessionError::Database(DbError::Rejected(_)))
        ));
        assert_eq!(store.records().await, before);
        let next = store
            .append(agent, SessionEvent::AgentCompleted)
            .await
            .unwrap();
        assert_eq!(next.sequence.get(), before.len() as u64 + 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn acknowledged_append_releases_owned_store_before_immediate_reopen() {
        let (root, store, id, agent) = fresh().await;
        store
            .append(agent, SessionEvent::AgentInterrupted)
            .await
            .unwrap();
        drop(store);
        let (_, records) = SessionStore::open(root.path(), id).await.unwrap();
        assert_eq!(records.len(), 3);
    }

    /// A cancelled caller leaves its operation running: later work waits for it.
    #[tokio::test]
    async fn cancelled_writer_operation_keeps_its_turn_until_it_finishes() {
        let (_root, store, _id, _) = fresh().await;
        let (entered, running) = oneshot::channel();
        let (release, released) = std::sync::mpsc::channel::<()>();
        let caller = tokio::spawn({
            let store = store.clone();
            async move {
                let work = move |_: &mut Writer| {
                    let _ = entered.send(());
                    let _ = released.recv();
                };
                store.with_writer(work).await
            }
        });
        running.await.unwrap();
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        // `drain` completes on its first poll exactly when the turn is free.
        let mut draining = Box::pin(store.drain());
        let pending = futures_util::poll!(&mut draining).is_pending();
        assert!(pending, "the operation still owns the writer");
        release.send(()).unwrap();
        crate::tests::bounded(draining).await.unwrap();
    }

    /// `close` waits out queued work, then frees the session for another open
    /// while the closed handle lives on, its admission closed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_under_traffic_stops_admission_and_frees_the_lease_for_reopen() {
        let (root, store, id, agent) = fresh().await;
        let traffic = async {
            for _ in 0..3 {
                // Dropped receipts: the commits are still in flight when `close` queues.
                let accepted = store.accept_append(agent.clone(), SessionEvent::AgentInterrupted);
                drop(accepted.await.unwrap());
            }
            store.close().await
        };
        let (blob, closed) = tokio::join!(store.store_blob(b"racing"), traffic);
        let blob = blob.unwrap();
        closed.unwrap();
        let late = store.append(agent, SessionEvent::AgentCompleted).await;
        assert!(matches!(late, Err(SessionError::Closed)));
        let (reopened, records) = SessionStore::open(root.path(), id).await.unwrap();
        assert_eq!(records.len(), 5);
        assert!(store.store_blob(b"late").await.is_err());
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
        let missing = SessionId::from_bytes([9; 16]);
        assert!(!SessionStore::is_open(root.path(), missing).await.unwrap());
        store
            .append(agent.clone(), SessionEvent::AgentInterrupted)
            .await
            .unwrap();
        let path = root.path().join(id.to_string()).join(DATABASE_FILE);
        let before = std::fs::read(&path).unwrap();
        assert!(matches!(
            SessionStore::open(root.path(), id).await,
            Err(SessionError::AlreadyOpen(locked)) if locked == id
        ));
        assert!(SessionStore::is_open(root.path(), id).await.unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        // Readers see committed records while the owner is live.
        let read = SessionStore::read_records(root.path(), id).await.unwrap();
        assert_eq!(read, store.records().await);
        // Model a descriptor briefly inherited by a concurrently spawned process.
        let inherited_lock = store.inherit_lock().await.unwrap();
        drop(store);
        let (store, records) = SessionStore::open(root.path(), id).await.unwrap();
        drop(inherited_lock);
        assert!(SessionStore::is_open(root.path(), id).await.unwrap());
        assert_eq!(records, read);
        let appended = store
            .append(agent, SessionEvent::AgentCompleted)
            .await
            .unwrap();
        assert_eq!(store.records().await.last(), Some(&appended));
        drop(store);
        assert!(!SessionStore::is_open(root.path(), id).await.unwrap());
    }

    /// Resume continues a root whose conversation still awaits the model, whose
    /// latest request of any purpose never completed, or whose turn failed;
    /// otherwise it is idle. A child's conversation is its own.
    #[tokio::test]
    async fn summary_reads_how_the_root_would_resume() {
        use crate::agent::Failure;
        use crate::provider::protocol::{AssistantItem, ToolCall, ToolResult};
        let (root, store, id, agent) = fresh().await;
        let stopped = async || {
            SessionStore::summary(root.path(), id)
                .await
                .unwrap()
                .stopped
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
        commit(Message::Tool(vec![result])).await;
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
            before_tokens: 10,
            after_tokens: 5,
        };
        append(SessionEvent::Compaction { checkpoint }).await;
        assert_eq!(stopped().await, None);
        let child = start_child(&store, &agent, 1, None, root.path()).await;
        let message = text("child prompt");
        let prompt = SessionEvent::MessageCommitted { message };
        store.append(child, prompt).await.unwrap();
        assert_eq!(stopped().await, None);

        let failure = Failure::Other("boom".into());
        append(SessionEvent::AgentFailed {
            failure: failure.clone(),
        })
        .await;
        assert_eq!(stopped().await, Some(TurnFailure::Failed(failure)));
        commit(text("again")).await;
        assert_eq!(stopped().await, interrupted);
    }

    /// The user's title holds until cleared, whatever automatic titles follow;
    /// otherwise the newest automatic title shows, and before any, the first prompt.
    /// Each reports where it came from.
    #[tokio::test]
    async fn summary_title_prefers_the_users_then_the_newest_automatic_one() {
        use TitleSource::{Generated, Prompt, User};
        let session = MemorySession::new().await;
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
        drop(store);
        let path = root.path().join(id.to_string()).join(DATABASE_FILE);
        for pragma in [
            "user_version = 11",
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
                SessionStore::open(root.path(), id).await,
                Err(SessionError::UnsupportedVersion(_))
            ));
            assert!(matches!(
                SessionStore::read_records(root.path(), id).await,
                Err(SessionError::UnsupportedVersion(_))
            ));
            assert_eq!(std::fs::read(&path).unwrap(), archive);
        }
    }
}
