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
    identity::{AgentId, EventId, QueueAttemptId, SessionId},
    provider::protocol::{Message, ModelRequest, UserContent},
};

mod db;
mod event;
mod queue;
mod request;
mod template;

pub(crate) use template::ModelRequestTemplate;

pub(crate) use db::{CaptureExtent, CaptureRow, Presentation, SharedDb};
pub use db::{DbError, SessionSummary};
pub use event::{
    CompactionCheckpoint, EventRecord, ModelCallOrigin, ModelContext, ModelFailureKind,
    ModelPurpose, ProfileSnapshot, QueueIntent, QueueSettlement, SessionEvent,
};
pub use queue::QueueIntentRecord;
pub use request::{merge_tool_results, project_history, reconstruct_model_request};

/// The database schema version; earlier formats are intentionally unsupported.
pub const SESSION_FORMAT_VERSION: i64 = db::USER_VERSION;
const DATABASE_FILE: &str = "session.db";
const LOCK_FILE: &str = "lock";

/// Restore the agent's last applied model.
pub fn agent_selection(records: &[EventRecord], agent: &AgentId) -> Option<String> {
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
    pub queue_attempt: Option<QueueAttemptId>,
    pub session: SessionId,
    pub sequence: u64,
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

/// Non-clonable receipt for an accepted operation. Dropping the receipt does not
/// cancel the writer. Its identity must be retained by a retrying producer until
/// a committed record or explicit reopen/recovery resolves the attempt.
#[derive(Debug)]
pub struct AcceptedAppend {
    identity: AppendIdentity,
    committed: oneshot::Receiver<Result<Vec<EventRecord>, SessionError>>,
}

impl AcceptedAppend {
    #[must_use]
    pub fn identity(&self) -> AppendIdentity {
        self.identity
    }

    pub async fn committed(self) -> Result<EventRecord, SessionError> {
        let identity = self.identity;
        match self.committed.await {
            Ok(result) => result.and_then(|mut records| {
                records.pop().ok_or_else(|| {
                    SessionError::AppendIndeterminate(AppendRecovery {
                        identity,
                        reason: "accepted append committed no record".into(),
                    })
                })
            }),
            Err(_) => Err(SessionError::AppendIndeterminate(AppendRecovery {
                identity,
                reason: "accepted writer lost its receipt; recovery is required".into(),
            })),
        }
    }
}

/// Test hooks at the commit boundary of the next accepted append.
#[cfg(test)]
pub(crate) enum CommitFault {
    /// Stop after acceptance, before COMMIT, until resumed.
    Pause {
        reached: oneshot::Sender<()>,
        resume: oneshot::Receiver<()>,
    },
    /// Roll back after acceptance: nothing is durable.
    RollBack,
    /// Commit, then report failure: the append is durable.
    ReportAfterCommit,
    /// Lose the writer thread before COMMIT.
    Panic,
    /// Stop after COMMIT, before publication, until resumed.
    PauseCommitted {
        reached: oneshot::Sender<()>,
        resume: oneshot::Receiver<()>,
    },
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
type Follow = Box<dyn FnOnce(u64) -> Vec<(AgentId, SessionEvent)> + Send>;

struct Pending {
    agent: AgentId,
    queue_attempt: Option<QueueAttemptId>,
    event: SessionEvent,
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
        entries: Vec<Pending>,
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
        if let Some(fault) = self.fault.take() {
            let shared = self.shared.clone();
            let injected = |reason: &str| {
                let recovery = AppendRecovery {
                    identity,
                    reason: reason.into(),
                };
                shared.write().health = WriterHealth::NeedsRecovery(recovery.clone());
                Ok(Err(SessionError::AppendIndeterminate(recovery)))
            };
            match fault {
                CommitFault::Pause { reached, resume } => {
                    let _ = reached.send(());
                    let _ = resume.blocking_recv();
                }
                CommitFault::RollBack => {
                    let _ = db.rollback();
                    self.encoder.reset();
                    return injected("injected rollback");
                }
                CommitFault::ReportAfterCommit => {
                    let _ = db.commit();
                    return injected("injected failure after commit");
                }
                CommitFault::Panic => panic!("injected writer loss"),
                CommitFault::PauseCommitted { reached, resume } => {
                    self.fault = Some(CommitFault::PauseCommitted { reached, resume });
                }
            }
        }
        Ok(match db.commit() {
            Ok(()) => {
                #[cfg(test)]
                if let Some(CommitFault::PauseCommitted { reached, resume }) = self.fault.take() {
                    let _ = reached.send(());
                    let _ = resume.blocking_recv();
                }
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

    /// Validate and encode inside an open transaction; any error is a definite rejection.
    fn accept(
        &mut self,
        db: &db::Db,
        mut entries: Vec<Pending>,
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
            .map_or(1, |record| record.sequence.saturating_add(1));
        let timestamp_millis = Utc::now().timestamp_millis();
        if let Some(follow) = follow {
            entries.extend(follow(first).into_iter().map(|(agent, event)| Pending {
                agent,
                queue_attempt: None,
                event,
            }));
        }
        let mut records = Vec::with_capacity(entries.len());
        for (offset, pending) in entries.into_iter().enumerate() {
            if pending.agent.session() != self.shared.id {
                return Err(SessionError::WrongSession);
            }
            records.push(EventRecord {
                id: EventId::generate()?,
                queue_attempt: pending.queue_attempt.or(pending.event.queue_attempt()),
                sequence: first.saturating_add(offset as u64),
                timestamp_millis,
                agent: pending.agent,
                event: pending.event,
            });
        }
        // Order-dependent rules validate against the committed prefix plus this batch.
        // Only a multi-entry batch copies the prefix.
        let mut prefix = std::borrow::Cow::Borrowed(state.records.as_slice());
        for (index, record) in records.iter().enumerate() {
            queue::validate_queue_record(&prefix, record)?;
            request::validate_compaction(&prefix, record)?;
            if index + 1 < records.len() {
                prefix.to_mut().push(record.clone());
            }
        }
        drop(prefix);
        drop(state);
        let encoder = &mut self.encoder;
        let result = db.transaction(|| {
            let tx = encoder.begin_tx(db, timestamp_millis)?;
            encoder.records(db, tx, &records)
        });
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
}

#[derive(Clone)]
pub struct SessionStore {
    inner: Arc<StoreInner>,
}

impl SessionStore {
    #[cfg(test)]
    pub(crate) async fn fault_next_commit(&self, fault: CommitFault) {
        self.inner.writer.lock().await.fault = Some(fault);
    }

    #[cfg(test)]
    pub(crate) async fn pause_append_at(
        &self,
        boundary: AppendBoundary,
    ) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (reached, waiting) = oneshot::channel();
        let (resume_sender, resume) = oneshot::channel();
        self.fault_next_commit(match boundary {
            AppendBoundary::Write => CommitFault::Pause { reached, resume },
            AppendBoundary::Publication => CommitFault::PauseCommitted { reached, resume },
        })
        .await;
        (waiting, resume_sender)
    }

    #[cfg(test)]
    pub(crate) async fn fail_append_at(&self, boundary: AppendBoundary) {
        self.fault_next_commit(match boundary {
            AppendBoundary::Write => CommitFault::RollBack,
            AppendBoundary::Publication => CommitFault::ReportAfterCommit,
        })
        .await;
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
        let writer = self.inner.writer.lock().await;
        writer
            ._lock
            .as_ref()
            .and_then(|lock| lock.0.try_clone().ok())
    }

    /// Read a session without taking its lock; a live owner may keep writing.
    pub async fn read_records(
        root: &Path,
        id: SessionId,
    ) -> Result<Vec<EventRecord>, SessionError> {
        Self::read_only(root, id, move |db| db::decode_records(db, id)).await
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
        tokio::task::spawn_blocking(move || {
            if !path.is_file() {
                return Err(std::io::Error::from(std::io::ErrorKind::NotFound).into());
            }
            let db = db::Db::open(&path, db::OpenMode::ReadOnly)?;
            db.batch("BEGIN")?;
            let value = read(&db);
            db.batch("COMMIT")?;
            Ok(value?)
        })
        .await
        .map_err(|error| SessionError::Io(std::io::Error::other(error)))?
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
        let (db, lock, records) = tokio::task::spawn_blocking(move || {
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
            let records = db::decode_records(&db, id)?;
            Ok::<_, SessionError>((db, lock, records))
        })
        .await
        .map_err(|error| SessionError::Io(std::io::Error::other(error)))??;
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
        let mut writer = self.inner.writer.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || work(&mut writer))
            .await
            .map_err(|error| SessionError::Io(std::io::Error::other(error.to_string())))
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
    pub async fn reconciled_records(&self) -> Result<Vec<EventRecord>, SessionError> {
        let state = self.inner.shared.read();
        state.require_healthy()?;
        Ok(state.records.clone())
    }

    /// Visit a consistent committed suffix without cloning event payloads.
    pub(crate) async fn visit_records_after(
        &self,
        sequence: u64,
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
        follow: impl FnOnce(u64) -> Vec<(AgentId, SessionEvent)> + Send + 'static,
    ) -> Result<Vec<EventRecord>, SessionError> {
        self.commit_batch(vec![(agent, event)], Some(Box::new(follow)))
            .await
    }

    async fn commit_batch(
        &self,
        events: Vec<(AgentId, SessionEvent)>,
        follow: Option<Follow>,
    ) -> Result<Vec<EventRecord>, SessionError> {
        let entries = events
            .into_iter()
            .map(|(agent, event)| Pending {
                agent,
                queue_attempt: None,
                event,
            })
            .collect();
        let (identities, committed) = self.accept(entries, follow).await?;
        let identity = *identities.last().expect("appends carry at least one entry");
        committed.await.unwrap_or_else(|_| {
            Err(SessionError::AppendIndeterminate(AppendRecovery {
                identity,
                reason: "accepted writer lost its receipt; recovery is required".into(),
            }))
        })
    }

    /// Validate and accept one append; an error means it was not accepted.
    pub async fn accept_append(
        &self,
        agent: AgentId,
        event: SessionEvent,
    ) -> Result<AcceptedAppend, SessionError> {
        self.accept_append_bound(agent, None, event).await
    }

    /// Acceptance validates and encodes in FIFO order on the writer thread; the
    /// commit completes there regardless of the caller. `queue_attempt` binds a
    /// queued model or user-message event to its intent.
    pub(crate) async fn accept_append_bound(
        &self,
        agent: AgentId,
        queue_attempt: Option<QueueAttemptId>,
        event: SessionEvent,
    ) -> Result<AcceptedAppend, SessionError> {
        let (mut identities, committed) = self
            .accept(
                vec![Pending {
                    agent,
                    queue_attempt,
                    event,
                }],
                None,
            )
            .await?;
        Ok(AcceptedAppend {
            identity: identities.pop().expect("one identity per entry"),
            committed,
        })
    }

    async fn accept(
        &self,
        entries: Vec<Pending>,
        follow: Option<Follow>,
    ) -> Result<
        (
            Vec<AppendIdentity>,
            oneshot::Receiver<Result<Vec<EventRecord>, SessionError>>,
        ),
        SessionError,
    > {
        if entries
            .iter()
            .any(|entry| entry.agent.session() != self.id())
        {
            return Err(SessionError::WrongSession);
        }
        let (accepted, acceptance) = oneshot::channel();
        let (committed, receipt) = oneshot::channel();
        // FIFO admission; the owned task holds the writer through commit and
        // publication, releasing it before the receipt so an immediate reopen succeeds.
        let mut writer = self.inner.writer.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || {
            // Release the writer before any report, even when the writer panics, so
            // a caller that closes and reopens on the answer finds the lock free.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                writer.append(entries, follow, accepted)
            }));
            drop(writer);
            match result {
                Ok(Ok(result)) => drop(committed.send(result)),
                Ok(Err((accepted, error))) => drop(accepted.send(Err(error))),
                Err(panic) => {
                    drop(committed);
                    std::panic::resume_unwind(panic)
                }
            }
        });
        let identities =
            acceptance
                .await
                .map_err(|_| match &self.inner.shared.read().health {
                    WriterHealth::NeedsRecovery(recovery) => {
                        SessionError::AppendUnavailable(recovery.clone())
                    }
                    WriterHealth::Healthy => SessionError::Closed,
                })??;
        Ok((identities, receipt))
    }

    /// Await accepted appends and report recovery-required state without closing admission.
    pub async fn drain(&self) -> Result<(), SessionError> {
        drop(self.inner.writer.lock().await);
        self.inner.shared.read().require_healthy()
    }

    /// Stop admission and report recovery-required state; the lease lasts until the store drops.
    pub async fn close(&self) -> Result<(), SessionError> {
        let _writer = self.inner.writer.lock().await;
        let mut state = self.inner.shared.write();
        state.closed = true;
        state.require_healthy()
    }

    /// The connection job output streams read and write through.
    pub(crate) fn outputs(&self) -> SharedDb {
        self.inner.db.clone()
    }

    pub(crate) async fn remove_job_artifacts(
        &self,
        job: crate::identity::JobId,
    ) -> Result<(), SessionError> {
        let db = self.outputs();
        tokio::task::spawn_blocking(move || db.remove_job_output(job.get()))
            .await
            .map_err(|error| SessionError::Io(std::io::Error::other(error)))??;
        Ok(())
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

    /// Load a stored attachment back into memory, such as a recovered draft's.
    pub async fn load_attachment(
        &self,
        reference: &crate::media::AttachmentRef,
    ) -> Result<crate::media::Attachment, SessionError> {
        use crate::media::{Attachment, AttachmentRef, Image, MAX_IMAGE_BYTES, MediaError};
        let limit = MAX_IMAGE_BYTES as usize;
        Ok(match reference {
            AttachmentRef::Text(text) => Attachment::Text {
                file: text.file.as_ref().map(PathBuf::from),
                content: String::from_utf8(self.read_blob(&text.blob, limit).await?)
                    .map_err(|_| invalid_data(MediaError::InvalidText))?,
            },
            AttachmentRef::Image(image) => Attachment::Image {
                file: image.file.as_ref().map(PathBuf::from),
                image: Image::new(self.read_blob(&image.blob, limit).await?)
                    .map_err(invalid_data)?,
            },
        })
    }

    /// Load every blob a request references so providers can encode it.
    pub async fn load_blobs(&self, request: &mut ModelRequest) -> Result<(), SessionError> {
        use crate::media::{AttachmentRef, ImageFormat, MAX_IMAGE_BYTES, MediaError};
        // Image blobs carry the format they must sniff as; text blobs carry none.
        let mut blobs = Vec::new();
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
    #[error("session JSON failed: {0}")]
    Json(#[from] serde_json::Error),
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
    #[error("invalid queue journal record: {0}")]
    InvalidQueue(&'static str),
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
pub(crate) mod fixture;
#[cfg(test)]
mod tests;
