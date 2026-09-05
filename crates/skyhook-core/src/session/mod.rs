//! Versioned append-only session persistence.

use base64::Engine as _;
use chrono::Utc;
use fs2::FileExt;
use serde::Deserialize;
use serde_json::Value;
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
    job::JobProgressRecord,
    media::ImageReference,
};

mod event;

pub(crate) use event::is_safe_artifact_path;
pub use event::{EventRecord, SessionEvent};

pub const SESSION_FORMAT_VERSION: u16 = 1;

struct SessionWriter {
    file: Option<BufWriter<File>>,
    next_sequence: u64,
}

struct StoreInner {
    id: SessionId,
    directory: PathBuf,
    durable: bool,
    writer: Mutex<SessionWriter>,
    events: broadcast::Sender<EventRecord>,
    _lock: Mutex<Option<std::fs::File>>,
}

#[derive(Clone)]
pub struct SessionStore {
    inner: Arc<StoreInner>,
}

impl SessionStore {
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
                Ok(()) => return Self::initialize(id, directory, 1, durable).await,
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
        let next_sequence = records.last().map_or(1, |record| record.sequence + 1);
        Ok((
            Self::initialize(id, directory, next_sequence, true).await?,
            records,
        ))
    }

    async fn initialize(
        id: SessionId,
        directory: PathBuf,
        next_sequence: u64,
        durable: bool,
    ) -> Result<Self, SessionError> {
        fs::create_dir_all(directory.join("blobs")).await?;
        fs::create_dir_all(directory.join("jobs")).await?;
        let (lock, file) = if durable {
            let lock_path = directory.join("session.lock");
            let lock = StdOpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(lock_path)?;
            lock.try_lock_exclusive()
                .map_err(|_| SessionError::AlreadyOpen(id))?;
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(directory.join("events.jsonl"))
                .await?;
            (Some(lock), Some(BufWriter::new(file)))
        } else {
            (None, None)
        };
        let (events, _) = broadcast::channel(512);
        Ok(Self {
            inner: Arc::new(StoreInner {
                id,
                directory,
                durable,
                writer: Mutex::new(SessionWriter {
                    file,
                    next_sequence,
                }),
                events,
                _lock: Mutex::new(lock),
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
        let mut bytes = serde_json::to_vec(&record)?;
        bytes.push(b'\n');
        if let Some(file) = &mut writer.file {
            file.write_all(&bytes).await?;
            file.flush().await?;
            file.get_ref().sync_data().await?;
        }
        writer.next_sequence = writer.next_sequence.saturating_add(1);
        let _ = self.inner.events.send(record.clone());
        Ok(record)
    }

    /// Flushes this handle and releases its durable session lock even when
    /// read-only clones remain alive briefly in supervised state.
    #[cfg(test)]
    pub(crate) async fn close(&self) -> Result<(), SessionError> {
        let mut writer = self.inner.writer.lock().await;
        if let Some(mut file) = writer.file.take() {
            file.flush().await?;
            file.get_ref().sync_data().await?;
        }
        if let Some(lock) = self.inner._lock.lock().await.take() {
            FileExt::unlock(&lock)?;
        }
        Ok(())
    }

    pub async fn write_job_output(
        &self,
        job: JobId,
        value: &Value,
    ) -> Result<PathBuf, SessionError> {
        let relative = PathBuf::from("jobs")
            .join(job.to_string())
            .join("output.json");
        if !self.inner.durable {
            return Ok(relative);
        }
        let directory = self.job_directory(job).await?;
        atomic_write(&directory.join("output.json"), &serde_json::to_vec(value)?).await?;
        Ok(relative)
    }

    pub async fn append_job_event(
        &self,
        job: JobId,
        event: &JobProgressRecord,
    ) -> Result<(), SessionError> {
        let directory = self.job_directory(job).await?;
        let path = directory.join("events.jsonl");
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await?;
        let mut bytes = serde_json::to_vec(event)?;
        bytes.push(b'\n');
        file.write_all(&bytes).await?;
        file.flush().await?;
        Ok(())
    }

    pub async fn read_job_events(
        &self,
        job: JobId,
        after: u64,
        limit: usize,
    ) -> Result<Vec<JobProgressRecord>, SessionError> {
        let path = self
            .inner
            .directory
            .join("jobs")
            .join(job.to_string())
            .join("events.jsonl");
        let bytes = match fs::read(path).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let records = parse_lines::<JobProgressRecord>(&bytes)?;
        Ok(records
            .into_iter()
            .filter(|record| record.sequence > after)
            .take(limit)
            .collect())
    }

    async fn job_directory(&self, job: JobId) -> Result<PathBuf, SessionError> {
        let directory = self.inner.directory.join("jobs").join(job.to_string());
        fs::create_dir_all(&directory).await?;
        Ok(directory)
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
        let data_base64 = if self.inner.durable {
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
}

fn parse_lines<T>(bytes: &[u8]) -> Result<Vec<T>, SessionError>
where
    T: for<'de> Deserialize<'de>,
{
    bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(serde_json::from_slice)
        .collect::<Result<_, _>>()
        .map_err(SessionError::from)
}

async fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), SessionError> {
    Ok(crate::fs::atomic_write(path, bytes, crate::fs::AtomicWriteOptions::default()).await?)
}

#[derive(Debug, Error)]
pub enum SessionError {
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
        store.close().await.unwrap();
        drop(store);
        let path = root.path().join(id.to_string()).join("events.jsonl");
        let mut file = OpenOptions::new().append(true).open(&path).await.unwrap();
        file.write_all(b"{incomplete").await.unwrap();
        drop(file);
        let (_store, records) = SessionStore::open(root.path(), id).await.unwrap();
        assert_eq!(records.len(), 1);
        assert!(fs::read_to_string(path).await.unwrap().ends_with('\n'));
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
