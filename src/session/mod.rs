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
    identity::{AgentId, EventId, SessionId},
    provider::{profile::ModelRef, protocol::ModelRequest},
};

mod content;
mod db;
mod event;
mod ledger;
mod request;
pub mod stats;
mod template;

pub(crate) use template::ModelRequestTemplate;

pub use content::{JobEvent, Message, RuntimeState, StateJob, StateJobKind, UserPart};
pub(crate) use db::{CaptureExtent, CaptureRow, Presentation, SharedDb};
pub use db::{DbError, SessionSummary};
pub(crate) use event::EntryKind;
pub use event::{
    AttemptRef, CompactionCheckpoint, CompactionFailure, CompletedOutcome, EventRecord, MessageSeq,
    ModeSelection, ModelCallOrigin, ModelContext, ModelFailureKind, ModelPurpose, ProfileSnapshot,
    RecordSeq, RequestSeq, SessionEvent, Truncation,
};
pub use ledger::{RequestChanges, RequestLedger, RequestPhase, RequestRecord};
pub use request::{
    Projection, project_history, reconstruct_model_request, record_at, render_history,
    request_context,
};

/// The database schema version; earlier formats are intentionally unsupported.
pub const SESSION_FORMAT_VERSION: i64 = db::USER_VERSION;
const DATABASE_FILE: &str = "session.db";
const LOCK_FILE: &str = "lock";

/// Restore the agent's last applied model.
pub fn agent_selection(records: &[EventRecord], agent: &AgentId) -> Option<ModelRef> {
    let mut selection = None;
    for record in records.iter().filter(|record| &record.agent == agent) {
        match &record.event {
            SessionEvent::AgentStarted { profile, .. } => {
                selection = profile.as_ref().map(|profile| profile.name.clone());
            }
            SessionEvent::ModelChanged { profile } => {
                if let Some(model) = &mut selection {
                    model.clone_from(&profile.name);
                }
            }
            _ => {}
        }
    }
    selection
}

/// The mode definitions the session has pinned, in order of first use.
pub fn pinned_modes(
    records: &[EventRecord],
) -> indexmap::IndexMap<String, crate::tool::policy::Mode> {
    let selections = records.iter().filter_map(|record| match &record.event {
        SessionEvent::AgentStarted { mode, .. } => mode.as_ref(),
        SessionEvent::ModeChanged { mode, .. } => Some(mode),
        _ => None,
    });
    selections
        .filter_map(|mode| Some((mode.name.clone(), mode.definition.clone()?)))
        .collect()
}

/// Restore the agent's last applied mode.
pub fn agent_mode(records: &[EventRecord], agent: &AgentId) -> Option<String> {
    let records = records.iter().rev();
    records
        .filter(|record| &record.agent == agent)
        .find_map(|record| match &record.event {
            SessionEvent::AgentStarted { mode, .. } => Some(mode.as_ref()),
            SessionEvent::ModeChanged { mode, .. } => Some(Some(mode)),
            _ => None,
        })?
        .map(|mode| mode.name.clone())
}

struct SessionLock(std::fs::File);

impl Drop for SessionLock {
    fn drop(&mut self) {
        // Ownership ends with the store, not with a transient fork-inherited fd.
        let _ = FileExt::unlock(&self.0);
    }
}

fn lock_session(directory: &Path, id: SessionId) -> Result<SessionLock, SessionError> {
    let lock = StdOpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(directory.join(LOCK_FILE))?;
    lock.try_lock_exclusive()
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppendRecovery {
    pub identity: AppendIdentity,
    pub reason: String,
}

impl std::fmt::Display for AppendRecovery {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "session {} event {} sequence {}: {}",
            self.identity.session, self.identity.event, self.identity.sequence, self.reason
        )
    }
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
        let identity = self.identity;
        let mut records = receipt(identity, self.committed).await?;
        records.pop().ok_or_else(|| {
            SessionError::AppendIndeterminate(AppendRecovery {
                identity,
                reason: "accepted append committed no record".into(),
            })
        })
    }
}

type Receipt = oneshot::Receiver<Result<Vec<EventRecord>, SessionError>>;

async fn receipt(
    identity: AppendIdentity,
    committed: Receipt,
) -> Result<Vec<EventRecord>, SessionError> {
    committed.await.unwrap_or_else(|_| {
        Err(SessionError::AppendIndeterminate(AppendRecovery {
            identity,
            reason: "accepted writer lost its receipt; recovery is required".into(),
        }))
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
                Err(SessionError::AppendUnavailable(recovery.clone()))
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
    _lock: Option<SessionLock>,
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
        accepted: Acceptance,
    ) -> Result<Result<Vec<EventRecord>, SessionError>, (Acceptance, SessionError)> {
        // Output writers share the connection; hold it from BEGIN through COMMIT.
        let connection = self.db.clone();
        let db = connection.lock();
        let records = match self.accept(&db, entries, follow) {
            Ok(records) => records,
            Err(error) => return Err((accepted, error)),
        };
        let identities: Vec<_> = records.iter().map(EventRecord::append_identity).collect();
        let identity = *identities.last().expect("appends carry at least one entry");
        let _ = accepted.send(Ok(identities));
        // Pessimistic poison also survives a lost writer task.
        self.shared.write().health = WriterHealth::NeedsRecovery(AppendRecovery {
            identity,
            reason: "accepted append did not finish publication".into(),
        });
        #[cfg(test)]
        {
            self.pause_at(AppendBoundary::Write);
            match self.fault.take() {
                Some(CommitFault::Fail(boundary)) => {
                    let reason = if boundary == AppendBoundary::Write {
                        let _ = db.rollback();
                        self.encoder.reset();
                        "injected rollback"
                    } else {
                        let _ = db.commit();
                        "injected failure after commit"
                    };
                    let (identity, reason) = (identity, reason.into());
                    let recovery = AppendRecovery { identity, reason };
                    self.shared.write().health = WriterHealth::NeedsRecovery(recovery.clone());
                    return Ok(Err(SessionError::AppendIndeterminate(recovery)));
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
            Err(error) => {
                let _ = db.rollback();
                self.encoder.reset();
                let recovery = AppendRecovery {
                    identity,
                    reason: error.to_string(),
                };
                self.shared.write().health = WriterHealth::NeedsRecovery(recovery.clone());
                Err(SessionError::AppendIndeterminate(recovery))
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

    /// Validate and encode inside an open transaction; any error is a definite rejection.
    fn accept(
        &mut self,
        db: &db::Db,
        mut entries: Vec<(AgentId, SessionEvent)>,
        follow: Option<Follow>,
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
        let timestamp_millis = Utc::now().timestamp_millis();
        if let Some(follow) = follow {
            entries.extend(follow(first));
        }
        let mut records = Vec::with_capacity(entries.len());
        // A mode's first use pins its definition. Appends are serialized here, so
        // agents applying one mode at once still pin it once.
        let mut pinned: Option<std::collections::HashSet<String>> = None;
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
                let pinned = pinned
                    .get_or_insert_with(|| pinned_modes(&state.records).into_keys().collect());
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
        // Only a multi-entry batch copies the prefix.
        let mut prefix = std::borrow::Cow::Borrowed(state.records.as_slice());
        for (index, record) in records.iter().enumerate() {
            request::validate_compaction(&prefix, record)?;
            if index + 1 < records.len() {
                prefix.to_mut().push(record.clone());
            }
        }
        drop(prefix);
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
            ._lock
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

    /// A session list row, without decoding the session or taking its lock.
    pub async fn summary(root: &Path, id: SessionId) -> Result<SessionSummary, SessionError> {
        Self::read_only(root, id, db::summary).await
    }

    /// Every saved job output document and capture as text, for inspection.
    pub async fn read_output_text(root: &Path, id: SessionId) -> Result<Vec<String>, SessionError> {
        Self::read_only(root, id, db::output_text).await
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
                return Err(std::io::Error::from(std::io::ErrorKind::NotFound).into());
            }
            let db = db::Db::open(&path, db::OpenMode::ReadOnly)?;
            db.batch("BEGIN")?;
            let value = read(&db);
            db.batch("COMMIT")?;
            Ok(value?)
        })
        .await?
    }

    /// Attempts and tool calls a stopped process left without an outcome.
    pub(crate) async fn interrupted_work(&self) -> Result<db::InterruptedWork, SessionError> {
        let id = self.id();
        Ok(self
            .with_writer(move |writer| db::interrupted_work(&writer.db.lock(), id))
            .await??)
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
        for _ in 0..16 {
            let id = SessionId::generate()?;
            let directory = root.join(id.to_string());
            match tokio::fs::create_dir(&directory).await {
                Ok(()) => {
                    return Self::start(id, directory, durable, false)
                        .await
                        .map(|(store, _)| store);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        Err(SessionError::IdCollisions)
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
            // Acquire ownership before opening a potentially active database.
            let lock = durable
                .then(|| lock_session(&open_directory, id))
                .transpose()?;
            let path = open_directory.join(DATABASE_FILE);
            let mode = match (durable, existing) {
                (false, _) => db::OpenMode::Memory,
                (true, false) => db::OpenMode::Create,
                (true, true) if path.is_file() => db::OpenMode::Open,
                (true, true) => {
                    return Err(std::io::Error::from(std::io::ErrorKind::NotFound).into());
                }
            };
            let db = db::Db::open(&path, mode)?;
            let records = request::admit_records(db::decode_records(&db, id)?)?;
            Ok::<_, SessionError>((db, lock, records))
        })
        .await??;
        let (events, _) = broadcast::channel(512);
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
            _lock: lock,
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

    /// Visit a consistent committed suffix without cloning event payloads.
    pub(crate) async fn visit_records_after(
        &self,
        sequence: RecordSeq,
        visit: impl FnOnce(&[EventRecord]),
    ) {
        let state = self.inner.shared.read();
        let start = state
            .records
            .partition_point(|record| record.sequence <= sequence);
        visit(&state.records[start..]);
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
        self.commit_batch(events, None).await
    }

    /// Commit `event` with the events `follow` builds from its sequence, in one
    /// transaction; a response and the records that reference it land together.
    pub async fn append_then(
        &self,
        agent: AgentId,
        event: SessionEvent,
        follow: impl FnOnce(RecordSeq) -> Vec<(AgentId, SessionEvent)> + Send + 'static,
    ) -> Result<Vec<EventRecord>, SessionError> {
        self.commit_batch(vec![(agent, event)], Some(Box::new(follow)))
            .await
    }

    async fn commit_batch(
        &self,
        events: Vec<(AgentId, SessionEvent)>,
        follow: Option<Follow>,
    ) -> Result<Vec<EventRecord>, SessionError> {
        let (identities, committed) = self.accept(events, follow).await?;
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
        let (mut identities, committed) = self.accept(vec![(agent, event)], None).await?;
        Ok(AcceptedAppend {
            identity: identities.pop().expect("one identity per entry"),
            committed,
        })
    }

    async fn accept(
        &self,
        entries: Vec<(AgentId, SessionEvent)>,
        follow: Option<Follow>,
    ) -> Result<(Vec<AppendIdentity>, Receipt), SessionError> {
        if entries
            .iter()
            .any(|(agent, _)| agent.session() != self.id())
        {
            return Err(SessionError::WrongSession);
        }
        let (accepted, acceptance) = oneshot::channel();
        let (committed, receipt) = oneshot::channel();
        let mut turn = self.turn().await;
        tokio::task::spawn_blocking(move || {
            // Release the writer before any report, even when the writer panics, so
            // a caller that closes and reopens on the answer finds the lock free.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                turn.writer.append(entries, follow, accepted)
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
        let identities = acceptance.await.map_err(|_| {
            SessionError::Io(std::io::Error::other(
                "session writer lost before accepting the append",
            ))
        })??;
        Ok((identities, receipt))
    }

    /// Await accepted appends and report recovery-required state without closing admission.
    pub async fn drain(&self) -> Result<(), SessionError> {
        let _turn = self.inner.turn.lock().await;
        self.inner.shared.read().require_healthy()
    }

    /// Stop admission, wait out operations already queued, and report
    /// recovery-required state; the lease lasts until the store drops.
    pub async fn close(&self) -> Result<(), SessionError> {
        let _turn = self.inner.turn.lock().await;
        let mut state = self.inner.shared.write();
        state.closed = true;
        state.require_healthy()
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
        use crate::media::MediaError;
        let invalid = |error| match error {
            MediaError::HashMismatch => SessionError::BlobHashMismatch(blob.sha256.to_string()),
            error => invalid_data(error),
        };
        usize::try_from(blob.bytes)
            .ok()
            .filter(|&length| length <= limit)
            .ok_or_else(|| invalid(MediaError::TooLarge))?;
        let key = blob.sha256.to_bytes().to_vec();
        let bytes = self
            .with_writer(move |writer| {
                writer.db.lock().query_row(
                    "SELECT bytes FROM blob WHERE sha256 = ?1",
                    db::params![key],
                    |row| Ok(row.get::<Vec<u8>>(0)?),
                )
            })
            .await??
            .ok_or_else(|| SessionError::Io(std::io::Error::from(std::io::ErrorKind::NotFound)))?;
        if crate::media::BlobDigest::of(&bytes) != blob.sha256 {
            return Err(invalid(MediaError::HashMismatch));
        }
        if bytes.len() as u64 != blob.bytes {
            return Err(invalid(MediaError::LengthMismatch));
        }
        Ok(bytes)
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

    /// Load every blob a request references so providers can encode it.
    pub async fn load_blobs(&self, request: &mut ModelRequest) -> Result<(), SessionError> {
        use crate::media::{AttachmentRef, ImageFormat, MAX_IMAGE_BYTES, MediaError};
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
        // Texts load before images, as a shared blob is then exempt from sniffing.
        blobs.sort_by_key(|(_, format)| format.is_some());
        // One bound for every blob loaded into a request, text or image.
        let limit = MAX_IMAGE_BYTES as usize;
        for (blob, format) in blobs {
            if !request.blobs.contains(&blob) {
                let bytes = self.read_blob(&blob, limit).await?;
                if format.is_some_and(|format| ImageFormat::sniff(&bytes) != Some(format)) {
                    return Err(invalid_data(MediaError::UnsupportedImage));
                }
                request.blobs.insert(blob, bytes);
            }
        }
        Ok(())
    }
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, SessionError> {
    let joined = tokio::task::spawn_blocking(work).await;
    joined.map_err(|error| SessionError::Io(std::io::Error::other(error)))
}

fn invalid_data(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> SessionError {
    SessionError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("model request template must not contain conversation history")]
    TemplateHistory,
    #[error("accepted append is indeterminate; recovery required: {0}")]
    AppendIndeterminate(AppendRecovery),
    #[error("append rejected: writer requires recovery of prior attempt: {0}")]
    AppendUnavailable(AppendRecovery),
    #[error("session writer is closed")]
    Closed,
    #[error("cannot reconstruct model request at sequence {sequence}: {reason}")]
    ModelRequestReplay { sequence: u64, reason: &'static str },
    #[error("session I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Database(DbError),
    #[error("session identifier generation failed: {0}")]
    Random(#[from] getrandom::Error),
    #[error("could not allocate a unique session identifier")]
    IdCollisions,
    #[error("session {0} is already open")]
    AlreadyOpen(SessionId),
    #[error("unsupported session version {0}")]
    UnsupportedVersion(i64),
    #[error("event belongs to another session")]
    WrongSession,
    #[error("blob `{0}` failed its content hash check")]
    BlobHashMismatch(String),
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
                hint: Some("Test model.".into()),
                ..ModelProfile::new("test", None, 128_000, 4096, false)
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
        let closing = tokio::spawn({
            let store = store.clone();
            async move { store.close().await }
        });
        tokio::task::yield_now().await;
        assert!(!closing.is_finished(), "close must drain accepted work");
        resume.send(()).unwrap();
        closing.await.unwrap().unwrap();
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
            let reading = tokio::spawn({
                let store = store.clone();
                async move { store.reconciled_records().await }
            });
            let draining = tokio::spawn({
                let store = store.clone();
                async move { store.drain().await }
            });
            tokio::task::yield_now().await;
            assert!(
                !reading.is_finished(),
                "{boundary:?}: read must wait for publication"
            );
            assert!(
                !draining.is_finished(),
                "{boundary:?}: drain must wait for publication"
            );
            resume.send(()).unwrap();
            let record = accepted.committed().await.unwrap();
            let records = reading.await.unwrap().unwrap();
            assert_eq!(records.last(), Some(&record), "{boundary:?}");
            draining.await.unwrap().unwrap();
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
            matches!(lost, Err(SessionError::Io(_))),
            "unexpected result: {lost:?}"
        );
        let before = store.reconciled_records().await.unwrap().len();
        store
            .append(agent, SessionEvent::AgentCompleted)
            .await
            .unwrap();
        assert_eq!(store.reconciled_records().await.unwrap().len(), before + 1);
    }

    /// Failures after acceptance poison the writer with the exact recovery identity;
    /// reopening resolves the append from what the database actually committed.
    #[tokio::test]
    async fn failures_poison_the_writer_and_reopen_resolves_the_commit() {
        for (fault, durable) in [
            (CommitFault::Fail(AppendBoundary::Write), false),
            (CommitFault::Fail(AppendBoundary::Publication), true),
            (CommitFault::Panic, false),
        ] {
            let (root, store, id, agent) = fresh().await;
            let before = store.records().await.len();
            store.fault_next_commit(fault).await;
            let accepted = store.accept_append(agent.clone(), SessionEvent::AgentInterrupted);
            let accepted = accepted.await.unwrap();
            let identity = accepted.identity();
            let recovery_is = |result: Result<_, SessionError>, indeterminate: bool| match result {
                Err(SessionError::AppendIndeterminate(recovery)) if indeterminate => {
                    recovery.identity == identity
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
            let retry = store
                .append(agent.clone(), SessionEvent::AgentCompleted)
                .await;
            assert!(recovery_is(retry.map(drop), false));
            assert!(matches!(
                store.close().await,
                Err(SessionError::AppendUnavailable(_))
            ));
            drop(store);
            let (reopened, replay) = SessionStore::open(root.path(), id).await.unwrap();
            assert_eq!(replay.len(), before + usize::from(durable));
            assert_eq!(
                replay.iter().any(|record| record.id == identity.event),
                durable
            );
            assert_eq!(reopened.reconciled_records().await.unwrap(), replay);
            let next = reopened
                .append(agent, SessionEvent::AgentCompleted)
                .await
                .unwrap();
            assert_eq!(next.sequence.get(), replay.len() as u64 + 1);
            assert_ne!(next.id, identity.event);
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
        for _ in 0..28 {
            let (_root, store, id, agent) = fresh().await;
            let root = _root.path().to_path_buf();
            store
                .append(agent, SessionEvent::AgentInterrupted)
                .await
                .unwrap();
            drop(store);
            let (_, records) = SessionStore::open(&root, id).await.unwrap();
            assert_eq!(records.len(), 3);
        }
    }

    /// A cancelled caller leaves its operation running: later work waits for it.
    #[tokio::test]
    async fn cancelled_writer_operation_keeps_its_turn_until_it_finishes() {
        let (_root, store, _id, agent) = fresh().await;
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
        let append = tokio::spawn({
            let store = store.clone();
            async move { store.append(agent, SessionEvent::AgentInterrupted).await }
        });
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert!(!append.is_finished(), "the operation still owns the writer");
        release.send(()).unwrap();
        append.await.unwrap().unwrap();
    }

    /// `close` waits until queued work has let go of the lease, so dropping the
    /// last handle frees the session at once, and admission stays closed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_under_traffic_stops_admission_and_frees_the_lease_for_reopen() {
        for round in 0..16 {
            let (root, store, id, agent) = fresh().await;
            let traffic = async {
                for _ in 0..3 {
                    // Dropped receipts: the commits are still in flight when `close` queues.
                    let accepted =
                        store.accept_append(agent.clone(), SessionEvent::AgentInterrupted);
                    drop(accepted.await.unwrap());
                }
                store.close().await
            };
            let (blob, closed) = tokio::join!(store.store_blob(b"racing"), traffic);
            blob.unwrap();
            closed.unwrap();
            if round % 2 == 0 {
                let late = store.append(agent, SessionEvent::AgentCompleted).await;
                assert!(matches!(late, Err(SessionError::Closed)));
            }
            drop(store);
            let (_, records) = SessionStore::open(root.path(), id).await.unwrap();
            assert_eq!(records.len(), 5);
        }
    }

    #[tokio::test]
    async fn owner_lock_excludes_competing_writers_but_not_readers() {
        let (root, store, id, agent) = fresh().await;
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
        assert_eq!(std::fs::read(&path).unwrap(), before);
        // Readers see committed records while the owner is live.
        let read = SessionStore::read_records(root.path(), id).await.unwrap();
        assert_eq!(read, store.records().await);
        // Model a descriptor briefly inherited by a concurrently spawned process.
        let inherited_lock = store.inherit_lock().await.unwrap();
        drop(store);
        let (store, records) = SessionStore::open(root.path(), id).await.unwrap();
        drop(inherited_lock);
        assert_eq!(records, read);
        let appended = store
            .append(agent, SessionEvent::AgentCompleted)
            .await
            .unwrap();
        assert_eq!(store.records().await.last(), Some(&appended));
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

    #[tokio::test]
    async fn ephemeral_store_keeps_events_and_images_in_memory() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::create_ephemeral(root.path()).await.unwrap();
        let directory = store.directory().to_path_buf();
        let agent = started(&store, root.path()).await;
        store
            .append(agent, SessionEvent::AgentInterrupted)
            .await
            .unwrap();
        assert_eq!(store.records().await.len(), 3);
        let png = crate::tests::png(b"image");
        let image = store
            .store_image(Some("image.png".to_owned()), &png)
            .await
            .unwrap();
        let limit = crate::media::MAX_IMAGE_BYTES as usize;
        assert_eq!(
            store.read_blob(&image.blob, limit).await.unwrap(),
            png.bytes()
        );
        for file in [DATABASE_FILE, LOCK_FILE] {
            assert!(!directory.join(file).exists());
        }
    }
}
