//! Versioned append-only session persistence.

use chrono::Utc;
use fs2::FileExt;
use serde::Deserialize;
use std::{
    collections::HashMap,
    fs::OpenOptions as StdOpenOptions,
    path::{Path, PathBuf},
    sync::Arc,
};
use thiserror::Error;
use tokio::{
    fs::{self, File, OpenOptions},
    io::{AsyncWriteExt, BufWriter},
    sync::{Mutex, broadcast, oneshot},
};

use crate::{
    identity::{AgentId, EventId, JobId, QueueAttemptId, SessionId},
    provider::protocol::{Message, ModelRequest, UserContent},
};

mod event;
mod queue;
mod request;
mod template;

pub(crate) use template::ModelRequestTemplate;

pub(crate) use event::is_safe_artifact_path;
pub use event::{
    CompactionCheckpoint, EventRecord, ModelCallOrigin, ModelFailureKind, ModelPurpose,
    QueueIntent, QueueSettlement, SessionEvent,
};
pub use queue::QueueIntentRecord;
pub use request::{project_history, reconstruct_model_request};

// Version 3 binds durable random event/queue identities and immutable queue intents.
// Earlier formats are intentionally unsupported; no migration or compatibility reader.
pub const SESSION_FORMAT_VERSION: u16 = 3;

/// Restore the agent's last applied model.
pub fn agent_selection(records: &[EventRecord], agent: &AgentId) -> Option<String> {
    let mut selection = None;
    for record in records.iter().filter(|record| &record.agent == agent) {
        match &record.event {
            SessionEvent::AgentStarted { model_profile, .. } => {
                selection = Some(model_profile.clone());
            }
            SessionEvent::ModelChanged { model_profile, .. } => {
                if let Some(model) = &mut selection {
                    model.clone_from(model_profile);
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
        .open(directory.join("session.lock"))?;
    lock.try_lock_exclusive()
        .map_err(|_| SessionError::AlreadyOpen(id))?;
    Ok(SessionLock(lock))
}

// The writer is dropped before its lock. Durability has one authority shared by
// append and blob import; no durable writer can exist without its session lease.
struct DurableBackend {
    file: BufWriter<File>,
    _lock: SessionLock,
}

struct SessionWriter {
    /// `None` for an ephemeral session.
    durable: Option<DurableBackend>,
    next_sequence: u64,
    records: Vec<EventRecord>,
    health: WriterHealth,
    closed: bool,
    #[cfg(test)]
    fault: Option<AppendFault>,
}

impl SessionWriter {
    /// A poisoned writer's cached records cannot resolve an uncertain attempt.
    fn require_healthy(&self) -> Result<(), SessionError> {
        match &self.health {
            WriterHealth::Healthy => Ok(()),
            WriterHealth::NeedsRecovery(recovery) => {
                Err(SessionError::AppendUnavailable(recovery.clone()))
            }
        }
    }

    async fn write_record(&mut self, bytes: &[u8]) -> Result<(), std::io::Error> {
        #[cfg(test)]
        self.at_boundary(AppendBoundary::Write).await?;
        if let Some(backend) = &mut self.durable {
            backend.file.write_all(bytes).await?;
        }
        #[cfg(test)]
        self.at_boundary(AppendBoundary::Flush).await?;
        if let Some(backend) = &mut self.durable {
            backend.file.flush().await?;
        }
        #[cfg(test)]
        self.at_boundary(AppendBoundary::Sync).await?;
        if let Some(backend) = &mut self.durable {
            backend.file.get_ref().sync_data().await?;
        }
        #[cfg(test)]
        self.at_boundary(AppendBoundary::Publication).await?;
        Ok(())
    }

    #[cfg(test)]
    async fn at_boundary(&mut self, boundary: AppendBoundary) -> Result<(), std::io::Error> {
        if self
            .fault
            .as_ref()
            .is_some_and(|fault| fault.boundary == boundary)
        {
            match self.fault.take().unwrap().action {
                AppendFaultAction::Pause { reached, resume } => {
                    let _ = reached.send(());
                    let _ = resume.await;
                }
                AppendFaultAction::Fail => {
                    return Err(std::io::Error::other("injected append failure"));
                }
                AppendFaultAction::Panic => panic!("injected accepted writer loss"),
                AppendFaultAction::PartialWrite => {
                    if let Some(backend) = &mut self.durable {
                        backend.file.write_all(b"{partial").await?;
                        backend.file.flush().await?;
                    }
                    return Err(std::io::Error::other("injected partial write"));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AppendBoundary {
    Write,
    Flush,
    Sync,
    Publication,
}

#[cfg(test)]
impl AppendBoundary {
    pub(crate) const ALL: [Self; 4] = [Self::Write, Self::Flush, Self::Sync, Self::Publication];
}

#[cfg(test)]
struct AppendFault {
    boundary: AppendBoundary,
    action: AppendFaultAction,
}

#[cfg(test)]
enum AppendFaultAction {
    Pause {
        reached: oneshot::Sender<()>,
        resume: oneshot::Receiver<()>,
    },
    Fail,
    Panic,
    PartialWrite,
}

enum WriterHealth {
    Healthy,
    NeedsRecovery(AppendRecovery),
}

/// Reconciliation key for one accepted journal append. A failed receipt is not
/// permission to resubmit: reopen/replay must resolve the exact event first.
/// Sequence is ordering metadata and may be reused after a torn tail is repaired.
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
    committed: oneshot::Receiver<Result<EventRecord, SessionError>>,
}

impl AcceptedAppend {
    #[must_use]
    pub fn identity(&self) -> AppendIdentity {
        self.identity
    }

    pub async fn committed(self) -> Result<EventRecord, SessionError> {
        self.committed.await.unwrap_or_else(|_| {
            Err(SessionError::AppendIndeterminate(AppendRecovery {
                identity: self.identity,
                reason: "accepted writer lost its receipt; recovery is required".into(),
            }))
        })
    }
}

struct StoreInner {
    id: SessionId,
    directory: PathBuf,
    writer: Arc<Mutex<SessionWriter>>,
    events: broadcast::Sender<EventRecord>,
    /// Blobs for ephemeral stores, which have no blob directory.
    blobs: std::sync::Mutex<HashMap<crate::media::BlobDigest, Arc<[u8]>>>,
}

#[derive(Clone)]
pub struct SessionStore {
    inner: Arc<StoreInner>,
}

impl SessionStore {
    #[cfg(test)]
    pub(crate) async fn fail_append_at(&self, boundary: AppendBoundary) {
        self.inner.writer.lock().await.fault = Some(AppendFault {
            boundary,
            action: AppendFaultAction::Fail,
        });
    }

    #[cfg(test)]
    pub(crate) async fn pause_append_at(
        &self,
        boundary: AppendBoundary,
    ) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (reached, waiting) = oneshot::channel();
        let (resume, resumed) = oneshot::channel();
        self.inner.writer.lock().await.fault = Some(AppendFault {
            boundary,
            action: AppendFaultAction::Pause {
                reached,
                resume: resumed,
            },
        });
        (waiting, resume)
    }

    /// Read an archive without opening a writer, taking a lock, or repairing a partial tail.
    pub async fn read_records(
        root: &Path,
        id: SessionId,
    ) -> Result<Vec<EventRecord>, SessionError> {
        let bytes = fs::read(root.join(id.to_string()).join("events.jsonl")).await?;
        let end = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1);
        let records = parse_lines(&bytes[..end])?;
        event::validate_records(&records, id)?;
        Ok(records)
    }

    pub async fn create(root: &Path) -> Result<Self, SessionError> {
        Self::create_with_durability(root, true).await
    }

    pub(crate) async fn create_ephemeral(root: &Path) -> Result<Self, SessionError> {
        Self::create_with_durability(root, false).await
    }

    async fn create_with_durability(root: &Path, durable: bool) -> Result<Self, SessionError> {
        fs::create_dir_all(root).await?;
        for _ in 0..16 {
            let id = SessionId::generate()?;
            let directory = root.join(id.to_string());
            match fs::create_dir(&directory).await {
                Ok(()) => {
                    let lock = durable.then(|| lock_session(&directory, id)).transpose()?;
                    return Self::initialize(id, directory, Vec::new(), lock).await;
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
        let directory = root.join(id.to_string());
        // Acquire ownership before inspecting or repairing a potentially active journal.
        let lock = lock_session(&directory, id)?;
        let path = directory.join("events.jsonl");
        let bytes = fs::read(&path).await?;
        let complete_len = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |position| position + 1);
        let complete = &bytes[..complete_len];
        let records = parse_lines(complete)?;
        event::validate_records(&records, id)?;
        if complete_len != bytes.len() {
            let file = OpenOptions::new().write(true).open(&path).await?;
            file.set_len(u64::try_from(complete_len).map_err(|_| SessionError::FileTooLarge)?)
                .await?;
            file.sync_all().await?;
        }
        Ok((
            Self::initialize(id, directory, records.clone(), Some(lock)).await?,
            records,
        ))
    }

    async fn initialize(
        id: SessionId,
        directory: PathBuf,
        records: Vec<EventRecord>,
        lock: Option<SessionLock>,
    ) -> Result<Self, SessionError> {
        fs::create_dir_all(directory.join("blobs")).await?;
        fs::create_dir_all(directory.join("jobs")).await?;
        let durable = match lock {
            Some(lock) => Some(DurableBackend {
                file: BufWriter::new(
                    OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(directory.join("events.jsonl"))
                        .await?,
                ),
                _lock: lock,
            }),
            None => None,
        };
        // Reopening an uncertain complete prefix is an explicit recovery
        // decision. Sync the validated prefix before exposing a healthy writer;
        // a previous failed sync is not silently upgraded to a durable receipt.
        if let Some(backend) = &durable {
            backend.file.get_ref().sync_data().await?;
            // Persist the journal and blob directory names before any event can
            // acknowledge a durable image reference or queue intent.
            File::open(&directory).await?.sync_all().await?;
            if let Some(parent) = directory.parent() {
                File::open(parent).await?.sync_all().await?;
            }
        }
        let (events, _) = broadcast::channel(512);
        Ok(Self {
            inner: Arc::new(StoreInner {
                id,
                directory,
                writer: Arc::new(Mutex::new(SessionWriter {
                    durable,
                    next_sequence: records
                        .last()
                        .map_or(1, |record| record.sequence.saturating_add(1)),
                    records,
                    health: WriterHealth::Healthy,
                    closed: false,
                    #[cfg(test)]
                    fault: None,
                })),
                events,
                blobs: Default::default(),
            }),
        })
    }

    #[must_use]
    pub fn id(&self) -> SessionId {
        self.inner.id
    }

    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.inner.directory
    }

    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<EventRecord> {
        self.inner.events.subscribe()
    }

    /// A consistent snapshot of all successfully committed events, including ephemeral sessions.
    pub async fn records(&self) -> Vec<EventRecord> {
        self.inner.writer.lock().await.records.clone()
    }

    /// Committed records, refused while an accepted append needs recovery (reopen first).
    pub async fn reconciled_records(&self) -> Result<Vec<EventRecord>, SessionError> {
        let writer = self.inner.writer.lock().await;
        writer.require_healthy()?;
        Ok(writer.records.clone())
    }

    /// Visit a consistent committed suffix without cloning event payloads.
    pub(crate) async fn visit_records_after(
        &self,
        sequence: u64,
        visit: impl FnOnce(&[EventRecord]),
    ) {
        let writer = self.inner.writer.lock().await;
        let start = writer
            .records
            .partition_point(|record| record.sequence <= sequence);
        visit(&writer.records[start..]);
    }

    /// Accept and await one append; dropping this waiter does not cancel persistence.
    pub async fn append(
        &self,
        agent: AgentId,
        event: SessionEvent,
    ) -> Result<EventRecord, SessionError> {
        self.accept_append(agent, event).await?.committed().await
    }

    /// Validate and accept one append; an error means it was not accepted.
    pub async fn accept_append(
        &self,
        agent: AgentId,
        event: SessionEvent,
    ) -> Result<AcceptedAppend, SessionError> {
        self.accept_append_bound(agent, None, event).await
    }

    /// Acceptance linearizes after validation under the writer mutex; an owned task
    /// then holds it through write, flush, sync and publication. `queue_attempt`
    /// binds a queued model or user-message event to its intent.
    pub(crate) async fn accept_append_bound(
        &self,
        agent: AgentId,
        queue_attempt: Option<QueueAttemptId>,
        event: SessionEvent,
    ) -> Result<AcceptedAppend, SessionError> {
        if agent.session() != self.id() {
            return Err(SessionError::WrongSession);
        }
        // Attempt binding is validated against the journal below; only the
        // inherent identity of queue events is adopted here.
        let queue_attempt = queue_attempt.or(event.queue_attempt());
        let mut writer = self.inner.writer.clone().lock_owned().await;
        writer.require_healthy()?;
        if writer.closed {
            return Err(SessionError::Closed);
        }
        let record = EventRecord {
            id: EventId::generate()?,
            queue_attempt,
            version: SESSION_FORMAT_VERSION,
            sequence: writer.next_sequence,
            timestamp_millis: Utc::now().timestamp_millis(),
            agent,
            event,
        };
        queue::validate_queue_record(&writer.records, &record)?;
        request::validate_compaction(&writer.records, &record)?;
        if matches!(record.event, SessionEvent::ModelRequested { .. }) {
            request::validate_request(&writer.records, &record)?;
        }
        // Serialization failure is definite rejection, before any write.
        let mut bytes = serde_json::to_vec(&record)?;
        bytes.push(b'\n');
        let identity = record.append_identity();
        // Pessimistic poison also survives an unexpected task panic/abort. It
        // cannot be observed until the owned guard is released.
        writer.health = WriterHealth::NeedsRecovery(AppendRecovery {
            identity,
            reason: "accepted append did not finish publication".into(),
        });
        let (committed, receipt) = oneshot::channel();
        let inner = self.inner.clone();
        tokio::spawn(async move {
            let result = writer.write_record(&bytes).await;
            let result = match result {
                Ok(()) => {
                    writer.next_sequence = record.sequence.saturating_add(1);
                    writer.records.push(record.clone());
                    let _ = inner.events.send(record.clone());
                    writer.health = WriterHealth::Healthy;
                    Ok(record)
                }
                Err(error) => {
                    let recovery = AppendRecovery {
                        identity,
                        reason: error.to_string(),
                    };
                    writer.health = WriterHealth::NeedsRecovery(recovery.clone());
                    Err(SessionError::AppendIndeterminate(recovery))
                }
            };
            // A receipt also ends this operation's resource ownership; callers
            // may drop their last store and immediately reopen after awaiting it.
            drop(writer);
            drop(inner);
            let _ = committed.send(result);
        });
        Ok(AcceptedAppend {
            identity,
            committed: receipt,
        })
    }

    /// Await accepted appends and report recovery-required state without closing admission.
    pub async fn drain(&self) -> Result<(), SessionError> {
        self.inner.writer.lock().await.require_healthy()
    }

    /// Stop admission and report recovery-required state; the lease lasts until the store drops.
    pub async fn close(&self) -> Result<(), SessionError> {
        let mut writer = self.inner.writer.lock().await;
        writer.closed = true;
        writer.require_healthy()
    }

    pub(crate) async fn remove_job_artifacts(&self, job: JobId) -> Result<(), SessionError> {
        let directory = self.inner.directory.join("jobs").join(job.to_string());
        match fs::remove_dir_all(directory).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    /// Store bytes in the content-addressed blob store.
    pub async fn store_blob(&self, bytes: &[u8]) -> Result<crate::media::BlobRef, SessionError> {
        let blob = crate::media::BlobRef::of(bytes);
        if self.inner.writer.lock().await.durable.is_none() {
            self.inner
                .blobs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(blob.sha256, bytes.into());
            return Ok(blob);
        }
        let hash = blob.sha256.to_string();
        let destination = self.inner.directory.join("blobs").join(&hash);
        if !fs::try_exists(&destination).await? {
            atomic_write(&destination, bytes).await?;
        } else {
            if fs::read(&destination).await? != bytes {
                return Err(SessionError::BlobHashMismatch(hash));
            }
            File::open(&destination).await?.sync_all().await?;
            File::open(self.inner.directory.join("blobs"))
                .await?
                .sync_all()
                .await?;
        }
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
        let length = usize::try_from(blob.bytes)
            .ok()
            .filter(|&length| length <= limit)
            .ok_or_else(|| invalid(MediaError::TooLarge))?;
        let stored = self
            .inner
            .blobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&blob.sha256)
            .cloned();
        let bytes = match stored {
            Some(bytes) => bytes.to_vec(),
            None => {
                // The path comes from the parsed digest, never an untrusted string.
                let path = self
                    .inner
                    .directory
                    .join("blobs")
                    .join(blob.sha256.to_string());
                let mut file = fs::File::open(path).await?;
                crate::bounded_io::read_bounded(&mut file, length)
                    .await
                    .map_err(|error| match error {
                        crate::bounded_io::BoundedReadError::Io(error) => SessionError::Io(error),
                        error => invalid_data(error),
                    })?
            }
        };
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

fn parse_lines(bytes: &[u8]) -> Result<Vec<EventRecord>, SessionError> {
    #[derive(Deserialize)]
    struct Header {
        version: u16,
    }

    bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            // Check the header before deserializing the version-specific event
            // body. An old assistant enum must report a version mismatch, not
            // a misleading missing item/block field error, regardless of JSON
            // object field order.
            let header: Header = serde_json::from_slice(line)?;
            if header.version != SESSION_FORMAT_VERSION {
                return Err(SessionError::UnsupportedVersion(header.version));
            }
            Ok(serde_json::from_slice(line)?)
        })
        .collect()
}

async fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), SessionError> {
    Ok(crate::fs::atomic_write(
        path,
        bytes,
        crate::fs::AtomicWriteOptions {
            sync_parent: true,
            ..Default::default()
        },
    )
    .await?)
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
    #[error("session identifier generation failed: {0}")]
    Random(#[from] getrandom::Error),
    #[error("could not allocate a unique session identifier")]
    IdCollisions,
    #[error("session {0} is already open")]
    AlreadyOpen(SessionId),
    #[error("unsupported session version {0}")]
    UnsupportedVersion(u16),
    #[error("invalid event sequence: expected {expected}, found {actual}")]
    InvalidSequence { expected: u64, actual: u64 },
    #[error("event belongs to another session")]
    WrongSession,
    #[error("duplicate event identity {0}")]
    DuplicateEvent(EventId),
    #[error("invalid queue journal record: {0}")]
    InvalidQueue(&'static str),
    #[error("duplicate job {0}")]
    DuplicateJob(JobId),
    #[error("unknown job {0}")]
    UnknownJob(JobId),
    #[error("duplicate or invalid terminal event for job {0}")]
    DuplicateTerminal(JobId),
    #[error("artifact path is unsafe")]
    UnsafeArtifactPath,
    #[error("file is too large for this platform")]
    FileTooLarge,
    #[error("blob `{0}` failed its content hash check")]
    BlobHashMismatch(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fresh() -> (tempfile::TempDir, SessionStore, SessionId, AgentId) {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::create(root.path()).await.unwrap();
        let id = store.id();
        (root, store, id, AgentId::root(id))
    }

    #[tokio::test]
    async fn accepted_append_survives_lost_waiter_at_every_boundary_and_close_drains() {
        for boundary in [AppendBoundary::Write, AppendBoundary::Publication] {
            let (root, store, id, agent) = fresh().await;
            let mut events = store.subscribe();
            let (reached, resume) = store.pause_append_at(boundary).await;
            let accepted = store.accept_append(agent.clone(), SessionEvent::AgentInterrupted);
            let accepted = accepted.await.unwrap();
            let identity = accepted.identity();
            reached.await.unwrap();
            assert!(
                events.try_recv().is_err(),
                "publication must follow durable IO"
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
            assert_eq!(store.records().await, vec![record.clone()]);
            let closed = store.append(agent, SessionEvent::AgentCompleted).await;
            assert!(matches!(closed, Err(SessionError::Closed)));
            drop(store);
            let (_, replay) = SessionStore::open(root.path(), id).await.unwrap();
            assert_eq!(replay, vec![record]);
        }
    }

    /// Failed and lost (panicking) accepted writers both poison the store with
    /// the exact recovery identity; reopen reconciles the durable prefix. A
    /// partial write is likewise repaired without reusing the lost event id.
    #[tokio::test]
    async fn failures_poison_the_writer_and_reopen_reconciles_the_complete_prefix() {
        let cases = AppendBoundary::ALL
            .into_iter()
            .flat_map(|boundary| {
                [AppendFaultAction::Fail, AppendFaultAction::Panic].map(|action| (boundary, action))
            })
            .chain([(AppendBoundary::Write, AppendFaultAction::PartialWrite)]);
        for (boundary, action) in cases {
            let partial = matches!(action, AppendFaultAction::PartialWrite);
            let (root, store, id, agent) = fresh().await;
            store.inner.writer.lock().await.fault = Some(AppendFault { boundary, action });
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
            assert!(store.records().await.is_empty());
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
            let durable = matches!(boundary, AppendBoundary::Sync | AppendBoundary::Publication);
            assert!(replay.len() <= 1);
            if partial || durable {
                let expected = usize::from(durable);
                assert_eq!(
                    replay.len(),
                    expected,
                    "flushed complete record resolves the attempt"
                );
            }
            assert_eq!(reopened.reconciled_records().await.unwrap(), replay);
            let next = reopened
                .append(agent, SessionEvent::AgentCompleted)
                .await
                .unwrap();
            assert_eq!(next.sequence, replay.len() as u64 + 1);
            if partial {
                assert_eq!(next.sequence, identity.sequence);
                assert_ne!(next.id, identity.event);
                let records = reopened.records().await;
                assert!(!records.iter().any(|record| record.id == identity.event));
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn acknowledged_append_releases_owned_store_before_immediate_reopen() {
        for _ in 0..32 {
            let (root, store, id, agent) = fresh().await;
            store
                .append(agent, SessionEvent::AgentInterrupted)
                .await
                .unwrap();
            drop(store);
            let (_, records) = SessionStore::open(root.path(), id).await.unwrap();
            assert_eq!(records.len(), 1);
        }
    }

    #[tokio::test]
    async fn append_and_resume_complete_prefix() {
        let (root, store, id, agent) = fresh().await;
        store
            .append(agent.clone(), SessionEvent::AgentInterrupted)
            .await
            .unwrap();
        let path = root.path().join(id.to_string()).join("events.jsonl");
        let mut file = OpenOptions::new().append(true).open(&path).await.unwrap();
        file.write_all(b"{incomplete").await.unwrap();
        file.flush().await.unwrap();
        drop(file);
        // A competing opener must not repair a journal whose owner is still alive.
        let before = fs::read(&path).await.unwrap();
        assert!(matches!(
            SessionStore::open(root.path(), id).await,
            Err(SessionError::AlreadyOpen(locked)) if locked == id
        ));
        assert_eq!(fs::read(&path).await.unwrap(), before);
        // Model a descriptor briefly inherited by a concurrently spawned process.
        let inherited_lock = {
            let writer = store.inner.writer.lock().await;
            let Some(backend) = &writer.durable else {
                panic!("durable store required");
            };
            backend._lock.0.try_clone().unwrap()
        };
        drop(store);
        let (store, records) = SessionStore::open(root.path(), id).await.unwrap();
        drop(inherited_lock);
        assert_eq!(records.len(), 1);
        assert_eq!(store.records().await, records);
        let appended = store
            .append(agent, SessionEvent::AgentCompleted)
            .await
            .unwrap();
        assert_eq!(store.records().await, vec![records[0].clone(), appended]);
        assert!(fs::read_to_string(path).await.unwrap().ends_with('\n'));
    }

    #[tokio::test]
    async fn unsupported_event_versions_are_rejected() {
        let (root, store, id, agent) = fresh().await;
        let path = store.directory().join("events.jsonl");
        let record = store
            .append(agent, SessionEvent::AgentInterrupted)
            .await
            .unwrap();
        drop(store);
        let mut record = serde_json::to_value(record).unwrap();
        for version in [1, 2, SESSION_FORMAT_VERSION + 1] {
            // The old flat enum cannot deserialize as an item. Put the body
            // before the version so field ordering cannot obscure the error.
            let mut ordered = serde_json::Map::new();
            ordered.insert(
                "event".into(),
                serde_json::json!({
                    "type": "message_committed",
                    "message": {"role": "assistant", "content": [
                        {"type": "reasoning", "text": "old", "opaque": {"signature": "old"}}
                    ]}
                }),
            );
            record.as_object_mut().unwrap().remove("event");
            record["version"] = version.into();
            ordered.extend(record.as_object().unwrap().clone());
            let archive = format!("{}\n", serde_json::to_string(&ordered).unwrap());
            fs::write(&path, &archive).await.unwrap();
            assert!(matches!(
                SessionStore::open(root.path(), id).await,
                Err(SessionError::UnsupportedVersion(found)) if found == version
            ));
            assert!(matches!(
                SessionStore::read_records(root.path(), id).await,
                Err(SessionError::UnsupportedVersion(found)) if found == version
            ));
            // Unsupported archives fail closed without being rewritten.
            assert_eq!(fs::read(&path).await.unwrap(), archive.as_bytes());
        }
        // A genuine old record shape has no durable event or queue identity; the
        // version header alone rejects it, and version three itself requires the
        // identity: there is no permissive v2 fallback.
        let old = format!(
            r#"{{"version":2,"sequence":1,"timestamp_millis":0,"agent":{{"session":"{id}","path":[]}},"event":{{"type":"agent_completed"}}}}
"#
        );
        assert!(matches!(
            parse_lines(old.as_bytes()),
            Err(SessionError::UnsupportedVersion(2))
        ));
        let v3 = old.replace("\"version\":2", "\"version\":3");
        assert!(matches!(
            parse_lines(v3.as_bytes()),
            Err(SessionError::Json(_))
        ));
    }

    #[tokio::test]
    async fn ephemeral_store_keeps_events_and_images_out_of_the_journal() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::create_ephemeral(root.path()).await.unwrap();
        let directory = store.directory().to_path_buf();
        let agent = AgentId::root(store.id());
        store
            .append(agent, SessionEvent::AgentInterrupted)
            .await
            .unwrap();
        assert_eq!(store.records().await.len(), 1);
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
        for journal in ["events.jsonl", "session.lock"] {
            assert!(!fs::try_exists(directory.join(journal)).await.unwrap());
        }
        let mut blobs = fs::read_dir(directory.join("blobs")).await.unwrap();
        assert!(blobs.next_entry().await.unwrap().is_none());
    }
}
