//! Session persistence in one normalized SQLite database per session.
use fs2::FileExt;
use std::{
    fs::OpenOptions as StdOpenOptions,
    path::{Path, PathBuf},
    sync::Arc,
};
use thiserror::Error;
#[cfg(test)]
use tokio::sync::oneshot;

use crate::{
    identity::{AgentId, SessionId},
    provider::protocol::ModelRequest,
};

mod content;
mod core;
mod db;
mod event;
mod ledger;
mod projection;
mod request;
pub mod stats;
mod template;
mod turns;

pub(crate) use template::ModelRequestTemplate;

pub use content::{JobEvent, Message, RuntimeState, StateJob, StateJobKind, UserPart};
pub use core::SessionPhase;

pub(crate) use core::{Command, CoreState, Plan, Prepared};
pub(crate) use db::{CaptureExtent, CaptureRow, OffloadedCapture, OutputRows, SharedDb};
pub use db::{DbError, SessionSummary, SessionTitle};
pub(crate) use event::EntryKind;
pub use event::{
    AttemptFailure, AttemptRef, CompactionCheckpoint, CompactionFailure, CompactionTrigger,
    CompletedOutcome, EventRecord, MessageSeq, ModeSelection, ModelCallOrigin, ModelContext,
    ModelPurpose, ProfileSnapshot, RecordSeq, Refused, RequestSeq, RetryDelay, SessionEvent,
    TitleSource, Truncation,
};
pub use ledger::{
    CommittedUsage, RequestChanges, RequestFailure, RequestLedger, RequestPhase, RequestRecord,
};
pub use projection::{Projected, Projection, project_history, render_history};
pub(crate) use request::stale_todos;
pub use request::{
    CheckpointError, ReplayError, reconstruct_model_request, record_at, request_context,
};
pub use turns::Turns;

/// The database schema version; earlier formats are intentionally unsupported.
pub const SESSION_FORMAT_VERSION: i64 = db::USER_VERSION;
const DATABASE_FILE: &str = "session.db";
const LOCK_FILE: &str = "lock";

/// Every mode edition the session has recorded, oldest first: a name's last is its
/// latest.
pub fn mode_editions(
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

/// Test hook: what the first append with an entry `0` accepts does once
/// committed, before it applies. A fault before COMMIT is a refused write
/// (`tests::refuse`).
#[cfg(test)]
pub(crate) struct Hold(fn(&SessionEvent) -> bool, Held);

#[cfg(test)]
pub(crate) enum Held {
    /// Stop until resumed.
    Pause(oneshot::Sender<()>, oneshot::Receiver<()>),
    /// Fail, leaving the append durable.
    Fail,
}

struct StoreInner {
    shared: Arc<core::Shared>,
    db: SharedDb,
    core: core::Core,
}

#[derive(Clone)]
pub struct SessionStore {
    inner: Arc<StoreInner>,
}

/// A store reference that keeps neither the session's core nor its lock.
pub(crate) struct WeakStore(std::sync::Weak<StoreInner>);

impl WeakStore {
    pub(crate) fn upgrade(&self) -> Option<SessionStore> {
        self.0.upgrade().map(|inner| SessionStore { inner })
    }
}

impl SessionStore {
    #[cfg(test)]
    async fn hold(&self, hold: Hold) {
        let set = move |writer: &mut core::Writer| writer.hold = Some(hold);
        self.inner.core.call(set).await.unwrap();
    }

    /// Pause the first append with an entry `matches` accepts once it committed,
    /// before it applies, until resumed.
    #[cfg(test)]
    pub(crate) async fn pause_committed(
        &self,
        matches: fn(&SessionEvent) -> bool,
    ) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (reached, waiting) = oneshot::channel();
        let (resume_sender, resume) = oneshot::channel();
        self.hold(Hold(matches, Held::Pause(reached, resume))).await;
        (waiting, resume_sender)
    }

    /// Fail the next append once it committed, before it applies.
    #[cfg(test)]
    pub(crate) async fn fail_committed(&self) {
        self.hold(Hold(|_| true, Held::Fail)).await;
    }

    /// Hold the core, and the connection with it, until resumed.
    #[cfg(test)]
    pub(crate) fn hold_connection(&self) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (reached, waiting) = oneshot::channel();
        let (resume_sender, resume) = oneshot::channel::<()>();
        self.inner.core.post(move |writer| {
            let _connection = writer.db.lock();
            let _ = reached.send(());
            let _ = resume.blocking_recv();
        });
        (waiting, resume_sender)
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
            (writer.lock.as_ref()).and_then(|lock| lock.0.try_clone().ok())
        };
        self.inner.core.call(inherit).await.unwrap()
    }

    /// The journal as a reopen would replay it, over the same database. Its
    /// appends do not reach this store's state.
    #[cfg(test)]
    pub(crate) fn replica(&self) -> Self {
        let state = core::CoreState::replay(&self.records()).unwrap();
        let (db, directory) = (self.inner.db.clone(), self.directory().to_owned());
        let started = core::Core::start(self.id(), directory, state, db.clone(), None);
        let (shared, core) = started.unwrap();
        Self {
            inner: Arc::new(StoreInner { shared, db, core }),
        }
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
        Ok(blocking(create).await??.0)
    }

    pub async fn open(
        root: &Path,
        id: SessionId,
    ) -> Result<(Self, Vec<EventRecord>), SessionError> {
        let directory = root.join(id.to_string());
        blocking(move || Self::start(id, directory, true, true)).await?
    }

    fn start(
        id: SessionId,
        directory: PathBuf,
        durable: bool,
        existing: bool,
    ) -> Result<(Self, Vec<EventRecord>), SessionError> {
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
        let db = SharedDb::new(db::Db::open(&path, mode)?);
        let records = request::admit_records(db::decode_records(&db.lock(), id)?)?;
        let (mut state, observed) = core::CoreState::replay(&records)?;
        // A reopened session runs nothing until something changes it.
        if existing {
            state.phase = SessionPhase::Viewing;
        }
        let state = (state, observed);
        let (shared, core) = core::Core::start(id, directory, state, db.clone(), lock)?;
        let inner = Arc::new(StoreInner { shared, db, core });
        Ok((Self { inner }, records))
    }

    #[must_use]
    pub fn id(&self) -> SessionId {
        self.inner.shared.id
    }

    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.inner.shared.directory
    }

    /// Observe without a gap between the snapshot and the updates after it.
    #[must_use]
    pub fn observe(&self) -> crate::agent::Observation {
        let shared = &self.inner.shared;
        let state = shared.read();
        let observed = shared.observed();
        crate::agent::Observation {
            snapshot: state.snapshot(&observed),
            updates: shared.events.subscribe(),
        }
    }

    /// Fold and publish an update the journal does not carry. Nothing the core
    /// plans reads these views, so they wait neither behind its commands nor
    /// behind readers of its state.
    pub(crate) fn post(&self, event: crate::agent::RuntimeEvent) {
        let mut observed = self.inner.shared.observed();
        observed.post(event.clone());
        let _ = self.inner.shared.events.send(observed.publish(event));
    }

    /// Read the core's state.
    pub(crate) fn read<T>(&self, read: impl FnOnce(&CoreState) -> T) -> T {
        read(&self.inner.shared.read())
    }

    /// A consistent snapshot of all successfully committed events, including ephemeral sessions.
    #[must_use]
    pub fn records(&self) -> Vec<EventRecord> {
        self.inner.shared.read().records.clone()
    }

    /// Append one event; dropping this waiter does not cancel persistence.
    pub(crate) async fn append(
        &self,
        agent: AgentId,
        event: SessionEvent,
    ) -> Result<EventRecord, SessionError> {
        Ok(self.append_all(vec![(agent, event)]).await?.remove(0))
    }

    /// Append several events committed in one transaction.
    pub(crate) async fn append_all(
        &self,
        events: Vec<(AgentId, SessionEvent)>,
    ) -> Result<Vec<EventRecord>, SessionError> {
        let records = |_: &mut CoreState, records: &[EventRecord]| records.to_vec();
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
    ) -> Result<Vec<EventRecord>, SessionError> {
        let records = |_: &mut CoreState, records: &[EventRecord]| records.to_vec();
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
        let run = move |writer: &mut core::Writer| writer.run(command, plan);
        self.inner.core.call(run).await?
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

    /// Resolve once `resolve` decides, as it reads the state when registered and
    /// after every change, consuming what it decides on as it decides: its waiter
    /// must lose nothing by abandoning the reply. Refused as closed, or with its
    /// failure, once the session decides nothing more.
    pub(crate) async fn until<T: Send + 'static>(
        &self,
        resolve: impl FnMut(&mut CoreState) -> Option<T> + Send + Sync + 'static,
    ) -> Result<T, SessionError> {
        self.until_after(|_| (), resolve).await
    }

    /// [`until`](Self::until), registered in the change that applies `first`, so
    /// it and every other wait first read the state as `first` leaves it.
    pub(crate) async fn until_after<T: Send + 'static>(
        &self,
        first: impl FnOnce(&mut CoreState) + Send + 'static,
        resolve: impl FnMut(&mut CoreState) -> Option<T> + Send + Sync + 'static,
    ) -> Result<T, SessionError> {
        let (reply, outcome) = tokio::sync::oneshot::channel();
        let register = move |writer: &mut core::Writer| {
            writer.change(|state| {
                first(state);
                state.wait(resolve, reply);
            });
        };
        self.inner.core.call(register).await?;
        match outcome.await {
            Ok(outcome) => Ok(outcome),
            Err(_) => Err(self.behind(|state| state.phase.refusal()).await),
        }
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

    /// Whether the session's phase admits `command` now.
    pub(crate) fn admits(&self, command: Command) -> Result<(), SessionError> {
        self.read(|state| state.phase.admits(command).map(drop))
    }

    /// Await accepted appends and report the failure that stopped the session,
    /// if its journal failed, without closing admission.
    pub(crate) async fn drain(&self) -> Result<(), SessionError> {
        let drain = |writer: &mut core::Writer| writer.shared.read().phase.failed();
        self.inner.core.call(drain).await?
    }

    /// Where the session is in its life.
    #[must_use]
    pub(crate) fn phase(&self) -> SessionPhase {
        self.read(|state| state.phase.clone())
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
        let db = self.inner.db.clone();
        Ok(blocking(move || db::title(&db.lock())).await??)
    }

    /// The connection job output streams read and write through.
    pub(crate) fn outputs(&self) -> SharedDb {
        self.inner.db.clone()
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
            writer.shared.read().phase.admits(Command::Journal)?;
            let inserted = writer.db.lock().execute(
                "INSERT INTO blob (sha256, bytes) VALUES (?1, ?2) ON CONFLICT (sha256) DO NOTHING",
                db::params![blob.sha256.to_bytes().to_vec(), bytes],
            );
            inserted.map_err(|cause| writer.fail(cause.into()))
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
        let (blob, db) = (*blob, self.inner.db.clone());
        blocking(move || read_blob(&db, blob, limit)).await?
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
            let db = self.inner.db.clone();
            let read = move || -> Result<Vec<_>, SessionError> {
                let read = |blob| Ok((blob, read_blob(&db, blob, limit)?));
                missing.into_iter().map(read).collect()
            };
            for (blob, bytes) in blocking(read).await?? {
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
    db: &SharedDb,
    blob: crate::media::BlobRef,
    limit: usize,
) -> Result<Vec<u8>, SessionError> {
    usize::try_from(blob.bytes)
        .ok()
        .filter(|&length| length <= limit)
        .ok_or(BlobError::TooLarge.at(blob.sha256))?;
    let bytes = db
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

/// What a session stopped by its journal's failure reports.
pub(crate) const JOURNAL_FAILED: &str = "Skyhook could not write the session journal and stopped the session (a Skyhook bug or a storage problem)";

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("model request template must not contain conversation history")]
    TemplateHistory,
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
    #[must_use]
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

    /// Reopen the session `id` and activate it, as a reopened session's first
    /// change does, so it takes appends.
    pub(crate) async fn reopen(root: &Path, id: SessionId) -> (SessionStore, Vec<EventRecord>) {
        let (store, records) = SessionStore::open(root, id).await.unwrap();
        store.enter(Command::Activate).await.unwrap();
        (store, records)
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

    pub(crate) fn record(agent: &AgentId, sequence: u64, event: SessionEvent) -> EventRecord {
        EventRecord {
            id: crate::identity::EventId::generate().unwrap(),
            sequence: sequence.into(),
            timestamp_millis: 0,
            agent: agent.clone(),
            event,
        }
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
                response_schema: Some(crate::provider::protocol::ResponseSchema {
                    name: "answer".into(),
                    schema: json!({"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"],"additionalProperties":false}),
                }),
            },
        }
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
        let (reached, resume) = first.block_on(store.pause_committed(|_| true));
        let writer = store.clone();
        first
            .spawn(async move { drop(writer.append(agent, SessionEvent::AgentInterrupted).await) });
        first.block_on(reached).unwrap();
        drop(first);
        resume.send(()).unwrap();
        drop(store);
        let replay = runtime()
            .unwrap()
            .block_on(SessionStore::open(root.path(), id));
        let last = replay.unwrap().1.pop().unwrap();
        assert!(matches!(last.event, SessionEvent::AgentInterrupted));
    }

    #[tokio::test]
    async fn accepted_append_survives_lost_waiter_and_close_drains() {
        let (root, store, id, agent) = fresh().await;
        let mut events = store.observe().updates;
        let (reached, resume) = store.pause_committed(|_| true).await;
        let sequence = RecordSeq::from(store.records().len() as u64 + 1);
        let mut appending = Box::pin(store.append(agent.clone(), SessionEvent::AgentInterrupted));
        assert!(futures_util::poll!(&mut appending).is_pending());
        reached.await.unwrap();
        assert!(
            events.try_recv().is_err(),
            "publication must follow the commit"
        );
        drop(appending);
        let mut closing = Box::pin(store.close());
        let pending = futures_util::poll!(&mut closing).is_pending();
        assert!(pending, "close must drain accepted work");
        resume.send(()).unwrap();
        crate::tests::bounded(closing).await.unwrap();
        let crate::agent::RuntimeEvent::Record(record) = events.recv().await.unwrap().event else {
            panic!("only records were published");
        };
        let record = *record;
        assert_eq!(record.sequence, sequence);
        assert_eq!(store.records().last(), Some(&record));
        let closed = store.append(agent, SessionEvent::AgentCompleted).await;
        assert!(matches!(
            closed,
            Err(SessionError::Refused(SessionPhase::Closed))
        ));
        drop(store);
        let (_, replay) = SessionStore::open(root.path(), id).await.unwrap();
        assert_eq!(replay.last(), Some(&record));
    }

    /// Output writes share the connection, and wait for a commit only up to its COMMIT.
    #[tokio::test]
    async fn the_connection_is_free_while_a_commit_publishes() {
        let (_root, store, _id, agent) = fresh().await;
        let (reached, resume) = store.pause_committed(|_| true).await;
        let appending = tokio::spawn({
            let store = store.clone();
            async move { store.append(agent, SessionEvent::AgentInterrupted).await }
        });
        reached.await.unwrap();
        crate::tests::bounded(store.title()).await.unwrap();
        resume.send(()).unwrap();
        appending.await.unwrap().unwrap();
    }

    /// Every writer failure stops the session: the command that met it reports
    /// it, as does every later write, a blob's too, and closing, which still
    /// frees the session; every wait resolves with it. The connection takes no
    /// more writes, and what the failed change deferred never runs. Reopening sees exactly
    /// what committed, and a session whose only append failed is kept exactly
    /// when that append committed.
    #[tokio::test]
    async fn every_writer_failure_stops_the_journal_and_reopen_sees_what_committed() {
        use std::sync::atomic::{AtomicBool, Ordering};
        enum Fault {
            /// Committed, then failed before it applied.
            Unconfirmed,
            /// A child that never started owns no rows: the schema rejects it.
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
            let before = store.records();
            let interrupted = || SessionEvent::AgentInterrupted;
            let waiting = store.until(|_| None::<()>);
            let mut waiting = Box::pin(waiting);
            assert!(futures_util::poll!(&mut waiting).is_pending());
            let applied = matches!(fault, Fault::Applied);
            let deferred = Arc::new(AtomicBool::new(false));
            let (failed, durable) = match fault {
                Fault::Unconfirmed => {
                    store.fail_committed().await;
                    let failed = store.append(agent.clone(), interrupted()).await;
                    (failed.map(drop), true)
                }
                Fault::Rejected => {
                    let failed = store.append(agent.child(9), interrupted()).await;
                    (failed.map(drop), false)
                }
                Fault::Lost => {
                    let lost = |_| panic!("injected loss before acceptance");
                    let failed = store.append_then(agent.clone(), interrupted(), lost).await;
                    (failed.map(drop), false)
                }
                Fault::Applied => {
                    let entries = vec![(agent.clone(), interrupted())];
                    let ran = deferred.clone();
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
            assert!(
                !deferred.load(Ordering::SeqCst),
                "a failed change's effect ran"
            );
            assert_eq!(store.phase(), SessionPhase::Failed(failure.clone()));
            assert_eq!(store.records().len(), before.len() + usize::from(applied));
            assert!(same(store.close().await));
            let (reopened, replay) = reopen(root.path(), id).await;
            assert_eq!(replay.len(), before.len() + usize::from(durable));
            let appended = reopened.append(agent.clone(), interrupted()).await.unwrap();
            assert_eq!(appended.sequence.get(), replay.len() as u64 + 1);
            assert!(same(store.append(agent, interrupted()).await.map(drop)));
            drop(reopened);
            if let Some(committed) = first {
                let first = SessionStore::create(root.path()).await.unwrap();
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
        let (_root, store, agent) = tests::on_disk().await;
        let mut waiting = Box::pin(store.until(|_| None::<()>));
        assert!(futures_util::poll!(&mut waiting).is_pending());
        store.drain().await.unwrap();
        let woken = Arc::new(Woken(tokio::sync::Notify::new()));
        let waker = std::task::Waker::from(woken.clone());
        let mut poll = || (waiting.as_mut()).poll(&mut std::task::Context::from_waker(&waker));
        assert!(poll().is_pending(), "registered");
        // The predicate holds the connection, from another thread, before it panics.
        let (db, before) = (store.outputs(), store.records().len());
        let held = Arc::new(std::sync::Barrier::new(2));
        let (release, released) = std::sync::mpsc::channel::<()>();
        let holding = held.clone();
        let holder = std::thread::spawn(move || {
            holding.wait();
            let _db = db.lock();
            holding.wait();
            let _ = released.recv();
        });
        let mut panicking = Box::pin(store.until(move |state: &mut CoreState| {
            if state.records.len() > before {
                held.wait();
                held.wait();
                panic!("injected predicate panic");
            }
            None::<()>
        }));
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

    /// `close` waits out queued work, then frees the session for another open
    /// while the closed handle lives on, its admission closed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_under_traffic_stops_admission_and_frees_the_lease_for_reopen() {
        let (root, store, id, agent) = fresh().await;
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
        let (reopened, records) = reopen(root.path(), id).await;
        assert_eq!(records.len(), 5);
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
        assert_eq!(read, store.records());
        // Model a descriptor briefly inherited by a concurrently spawned process.
        let inherited_lock = store.inherit_lock().await.unwrap();
        drop(store);
        let (store, records) = reopen(root.path(), id).await;
        drop(inherited_lock);
        assert!(SessionStore::is_open(root.path(), id).await.unwrap());
        assert_eq!(records, read);
        let appended = store
            .append(agent, SessionEvent::AgentCompleted)
            .await
            .unwrap();
        assert_eq!(store.records().last(), Some(&appended));
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
        // The core, as resume reads it, agrees with the session list throughout.
        let stopped = async || {
            let listed = SessionStore::summary(root.path(), id).await.unwrap();
            let read = store.read(|state| crate::agent::stopped_turn(state, &agent));
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
            trigger: crate::session::CompactionTrigger::Occupancy(100),
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
