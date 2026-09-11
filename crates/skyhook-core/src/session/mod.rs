//! Versioned append-only session persistence.

use base64::Engine as _;
use chrono::Utc;
use fs2::FileExt;
use serde::Deserialize;
use std::{
    fs::OpenOptions as StdOpenOptions,
    path::{Path, PathBuf},
    sync::Arc,
};
use thiserror::Error;
use tokio::{
    fs::{self, File, OpenOptions},
    io::{AsyncWriteExt, BufWriter},
    sync::{Mutex, broadcast},
};

use crate::{
    identity::{AgentId, JobId, SessionId},
    media::ImageReference,
    provider::protocol::{Message, ModelRequest, UserContent},
};

mod event;
mod request;

pub(crate) use event::is_safe_artifact_path;
pub use event::{
    CompactionCheckpoint, ContextMessage, EventRecord, ModelCallOrigin, ModelPurpose, SessionEvent,
};
pub use request::{project_history, reconstruct_model_request};

// Version 2 preserves assistant item/block identities and item-scoped replay.
// The former flat assistant content format is intentionally not migrated.
pub const SESSION_FORMAT_VERSION: u16 = 2;

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

struct SessionWriter {
    file: Option<BufWriter<File>>,
    next_sequence: u64,
    records: Vec<EventRecord>,
}

struct StoreInner {
    id: SessionId,
    directory: PathBuf,
    writer: Mutex<SessionWriter>,
    events: broadcast::Sender<EventRecord>,
    lock: Option<SessionLock>,
}

#[derive(Clone)]
pub struct SessionStore {
    inner: Arc<StoreInner>,
}

impl SessionStore {
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
        let file = if lock.is_some() {
            Some(BufWriter::new(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(directory.join("events.jsonl"))
                    .await?,
            ))
        } else {
            None
        };
        let (events, _) = broadcast::channel(512);
        Ok(Self {
            inner: Arc::new(StoreInner {
                id,
                directory,
                writer: Mutex::new(SessionWriter {
                    file,
                    next_sequence: records.last().map_or(1, |record| record.sequence + 1),
                    records,
                }),
                events,
                lock,
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

    pub async fn append(
        &self,
        agent: AgentId,
        event: SessionEvent,
    ) -> Result<EventRecord, SessionError> {
        if agent.session() != self.id() {
            return Err(SessionError::WrongSession);
        }
        let mut writer = self.inner.writer.lock().await;
        let record = EventRecord {
            version: SESSION_FORMAT_VERSION,
            sequence: writer.next_sequence,
            timestamp_millis: Utc::now().timestamp_millis(),
            agent,
            event,
        };
        request::validate_compaction(&writer.records, &record)?;
        if matches!(record.event, SessionEvent::ModelRequested { .. }) {
            request::validate_request(&writer.records, &record)?;
        }
        if let Some(file) = &mut writer.file {
            let mut bytes = serde_json::to_vec(&record)?;
            bytes.push(b'\n');
            file.write_all(&bytes).await?;
            file.flush().await?;
            file.get_ref().sync_data().await?;
        }
        writer.next_sequence = writer.next_sequence.saturating_add(1);
        writer.records.push(record.clone());
        let _ = self.inner.events.send(record.clone());
        Ok(record)
    }

    pub(crate) async fn remove_job_artifacts(&self, job: JobId) -> Result<(), SessionError> {
        let directory = self.inner.directory.join("jobs").join(job.to_string());
        match fs::remove_dir_all(directory).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub async fn import_blob(
        &self,
        bytes: &[u8],
        name: String,
        media_type: String,
    ) -> Result<ImageReference, SessionError> {
        let hash = crate::sha256_hex(bytes);
        let data_base64 = if self.inner.lock.is_some() {
            let destination = self.inner.directory.join("blobs").join(&hash);
            if !fs::try_exists(&destination).await? {
                atomic_write(&destination, bytes).await?;
            }
            None
        } else {
            Some(base64::engine::general_purpose::STANDARD.encode(bytes))
        };
        Ok(ImageReference {
            sha256: hash,
            media_type,
            name,
            bytes: u64::try_from(bytes.len()).map_err(|_| SessionError::FileTooLarge)?,
            data_base64,
        })
    }

    pub async fn read_blob(&self, reference: &ImageReference) -> Result<Vec<u8>, SessionError> {
        let bytes = if let Some(data) = &reference.data_base64 {
            base64::engine::general_purpose::STANDARD
                .decode(data)
                .map_err(|error| {
                    SessionError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, error))
                })?
        } else {
            fs::read(self.inner.directory.join("blobs").join(&reference.sha256)).await?
        };
        let actual = crate::sha256_hex(&bytes);
        if actual != reference.sha256 {
            return Err(SessionError::BlobHashMismatch(reference.sha256.clone()));
        }
        Ok(bytes)
    }

    /// Restore image payloads in a reconstructed or newly assembled provider-neutral request.
    pub async fn hydrate_model_request(
        &self,
        request: &mut ModelRequest,
    ) -> Result<(), SessionError> {
        for message in &mut request.messages {
            match message {
                Message::User(content) => {
                    for item in content {
                        if let UserContent::Image { image } = item {
                            self.hydrate_image(image).await?;
                        }
                    }
                }
                Message::Tool(results) => {
                    for result in results {
                        for image in &mut result.images {
                            self.hydrate_image(image).await?;
                        }
                    }
                }
                Message::Assistant(_) => {}
            }
        }
        Ok(())
    }

    async fn hydrate_image(&self, image: &mut ImageReference) -> Result<(), SessionError> {
        if image.data_base64.is_none() {
            image.data_base64 = Some(
                base64::engine::general_purpose::STANDARD.encode(self.read_blob(image).await?),
            );
        }
        Ok(())
    }
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
    Ok(crate::fs::atomic_write(path, bytes, crate::fs::AtomicWriteOptions::default()).await?)
}

#[derive(Debug, Error)]
pub enum SessionError {
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

    #[tokio::test]
    async fn append_and_resume_complete_prefix() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::create(root.path()).await.unwrap();
        let id = store.id();
        let agent = AgentId::root(id);
        store
            .append(agent, SessionEvent::AgentInterrupted)
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
        let inherited_lock = store.inner.lock.as_ref().unwrap().0.try_clone().unwrap();
        drop(store);
        let (store, records) = SessionStore::open(root.path(), id).await.unwrap();
        drop(inherited_lock);
        assert_eq!(records.len(), 1);
        assert_eq!(store.records().await, records);
        let appended = store
            .append(AgentId::root(id), SessionEvent::AgentCompleted)
            .await
            .unwrap();
        assert_eq!(store.records().await, vec![records[0].clone(), appended]);
        assert!(fs::read_to_string(path).await.unwrap().ends_with('\n'));
    }

    #[tokio::test]
    async fn unsupported_event_versions_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::create(root.path()).await.unwrap();
        let id = store.id();
        let path = store.directory().join("events.jsonl");
        let record = store
            .append(AgentId::root(id), SessionEvent::AgentInterrupted)
            .await
            .unwrap();
        drop(store);
        let mut record = serde_json::to_value(record).unwrap();
        for version in [1, SESSION_FORMAT_VERSION + 1] {
            record["version"] = version.into();
            // The old flat enum cannot deserialize as an item. Put the body
            // before the version to ensure field ordering cannot obscure the
            // actionable format-version error.
            record["event"] = serde_json::json!({
                "type": "message_committed",
                "message": {"role": "assistant", "content": [
                    {"type": "reasoning", "text": "old", "opaque": {"signature": "old"}}
                ]}
            });
            let mut ordered = serde_json::Map::new();
            ordered.insert("event".into(), record["event"].clone());
            for (key, value) in record.as_object().unwrap() {
                if key != "event" && key != "version" {
                    ordered.insert(key.clone(), value.clone());
                }
            }
            ordered.insert("version".into(), version.into());
            let serialized = serde_json::to_string(&ordered).unwrap();
            fs::write(&path, format!("{serialized}\n")).await.unwrap();
            assert!(matches!(
                SessionStore::open(root.path(), id).await,
                Err(SessionError::UnsupportedVersion(found)) if found == version
            ));
            assert!(matches!(
                SessionStore::read_records(root.path(), id).await,
                Err(SessionError::UnsupportedVersion(found)) if found == version
            ));
        }
    }

    #[tokio::test]
    async fn ephemeral_store_keeps_events_and_images_out_of_the_journal() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::create_ephemeral(root.path()).await.unwrap();
        let directory = store.directory().to_path_buf();
        store
            .append(AgentId::root(store.id()), SessionEvent::AgentInterrupted)
            .await
            .unwrap();
        assert_eq!(store.records().await.len(), 1);
        let image = store
            .import_blob(
                b"image",
                "image.bin".to_owned(),
                "application/octet-stream".to_owned(),
            )
            .await
            .unwrap();
        assert!(image.data_base64.is_some());
        assert!(
            !fs::try_exists(directory.join("events.jsonl"))
                .await
                .unwrap()
        );
        assert!(
            !fs::try_exists(directory.join("session.lock"))
                .await
                .unwrap()
        );
        assert!(
            fs::read_dir(directory.join("blobs"))
                .await
                .unwrap()
                .next_entry()
                .await
                .unwrap()
                .is_none()
        );
    }
}
