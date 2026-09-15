//! Job/field-bound capture streams. Completion proves successful IO, not valid JSON.
//!
//! Streamed captures (search, remote artifacts) keep failed or abandoned partial
//! output readable as incomplete output, and an explicitly streamed empty capture
//! is materialized. Builtin text captures remove their file when abandoned;
//! `finish_nonempty` is the process/console contract that omits empty streams.
use std::{
    io::{self, Seek, Write},
    path::{Path, PathBuf},
};

use tokio::io::AsyncWriteExt as _;

use super::{CaptureKind, register_capture};
use crate::{identity::JobId, job::JobManager, tool::ToolError};

/// The current builtin text fields. JSON/remote captures retain their distinct
/// admission APIs; this is not an unchecked arbitrary-pointer capture token.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TextCaptureField {
    Stdout,
    Stderr,
    Content,
    Console,
}

impl TextCaptureField {
    pub(crate) fn pointer(self) -> String {
        format!(
            "/result/{}",
            match self {
                Self::Stdout => "stdout",
                Self::Stderr => "stderr",
                Self::Content => "content",
                Self::Console => "console",
            }
        )
    }
}

/// Only successful consuming finalization constructs this evidence; it has no
/// Serialize implementation. Finalizing a writer is not publication: discovery
/// reports completion only after the terminal snapshot references this capture.
#[derive(Clone, Debug)]
pub(crate) struct CompletedCapture {
    job: JobId,
    directory: PathBuf,
    field: String,
    kind: CaptureKind,
}

impl CompletedCapture {
    pub(crate) fn belongs_to(&self, job: JobId, directory: &Path) -> bool {
        self.job == job && self.directory == directory
    }

    #[cfg(test)]
    pub(crate) fn matches(&self, job: JobId, field: &str) -> bool {
        self.job == job && self.field == field
    }

    pub(crate) fn field(&self) -> &str {
        &self.field
    }

    pub(crate) fn kind(&self) -> CaptureKind {
        self.kind
    }

    fn path(&self) -> PathBuf {
        super::super::field_file(&self.directory, &self.field)
    }
}

/// The uncommitted file's owner. Text captures remove an abandoned file; streams
/// leave partial output readable. Committing or discarding ends the obligation.
struct Reservation {
    completed: CompletedCapture,
    remove_on_abandon: bool,
}

impl Reservation {
    fn commit(&mut self) -> CompletedCapture {
        self.remove_on_abandon = false;
        self.completed.clone()
    }

    /// Omit and remove empty streams (process and console contract).
    fn commit_nonempty(&mut self, length: u64) -> io::Result<Option<CompletedCapture>> {
        if length == 0 {
            std::fs::remove_file(self.completed.path())?;
            self.remove_on_abandon = false;
            return Ok(None);
        }
        Ok(Some(self.commit()))
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if self.remove_on_abandon {
            // Best effort on OS failure. Discovery already filters missing
            // files; retaining the registration does not advertise data.
            let _ = std::fs::remove_file(self.completed.path());
        }
    }
}

fn healthy(failure: Option<io::ErrorKind>) -> io::Result<()> {
    match failure {
        Some(kind) => Err(io::Error::new(
            kind,
            "capture cannot finish after an IO error",
        )),
        None => Ok(()),
    }
}

fn track<T>(failure: &mut Option<io::ErrorKind>, result: io::Result<T>) -> io::Result<T> {
    if let Err(error) = &result {
        *failure = Some(error.kind());
    }
    result
}

/// Reservation and newly created file have one owner. No raw pathname escapes.
pub(crate) struct PendingCapture {
    file: std::fs::File,
    reservation: Reservation,
}

impl PendingCapture {
    pub(crate) fn create(
        job: JobId,
        directory: &Path,
        field: &str,
        kind: CaptureKind,
    ) -> io::Result<Self> {
        super::validate_capture_field(field)?;
        std::fs::create_dir_all(directory)?;
        let path = super::super::field_file(directory, field);
        // Acquire the file before publishing registration: even a losing producer
        // with a different kind must not rewrite the existing owner's metadata.
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        if let Err(error) = register_capture(directory, field, kind) {
            drop(file);
            let _ = std::fs::remove_file(path);
            return Err(error);
        }
        Ok(Self {
            file,
            reservation: Reservation {
                completed: CompletedCapture {
                    job,
                    directory: directory.to_owned(),
                    field: field.into(),
                    kind,
                },
                remove_on_abandon: false,
            },
        })
    }

    pub(crate) fn open(self) -> CaptureWriter {
        CaptureWriter {
            file: io::BufWriter::new(self.file),
            reservation: self.reservation,
            failure: None,
        }
    }

    pub(crate) fn open_async(self) -> AsyncCapture {
        AsyncCapture {
            file: tokio::fs::File::from_std(self.file),
            reservation: self.reservation,
            failure: None,
            pending: Vec::new(),
            written: 0,
        }
    }
}

/// Seek/truncate are needed for grep's binary-match rollback, but cannot change
/// the bound job, field, or output file. An IO failure poisons completion.
pub(crate) struct CaptureWriter {
    file: io::BufWriter<std::fs::File>,
    reservation: Reservation,
    failure: Option<io::ErrorKind>,
}

impl CaptureWriter {
    /// Append text and flush it, so a live console capture is pageable at once.
    pub(crate) fn write_text(&mut self, text: &str) -> io::Result<()> {
        self.write_all(text.as_bytes())?;
        self.flush()
    }

    pub(crate) fn truncate(&mut self, length: u64) -> io::Result<()> {
        self.flush()?;
        track(&mut self.failure, self.file.get_ref().set_len(length))
    }

    pub(crate) fn finish(mut self) -> io::Result<CompletedCapture> {
        self.flush()?;
        Ok(self.reservation.commit())
    }

    pub(crate) fn finish_nonempty(mut self) -> io::Result<Option<CompletedCapture>> {
        self.flush()?;
        let length = self.file.get_ref().metadata()?.len();
        self.reservation.commit_nonempty(length)
    }
}

// The default `write_all` loops through `write`, so every chunk is failure-checked.
impl Write for CaptureWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        healthy(self.failure)?;
        track(&mut self.failure, self.file.write(bytes))
    }

    fn flush(&mut self) -> io::Result<()> {
        healthy(self.failure)?;
        track(&mut self.failure, self.file.flush())
    }
}

impl Seek for CaptureWriter {
    fn seek(&mut self, position: io::SeekFrom) -> io::Result<u64> {
        healthy(self.failure)?;
        track(&mut self.failure, self.file.seek(position))
    }
}

/// Retains accepted bytes and partial-write progress across dropped write
/// futures, so an interrupted write finishes without loss or duplication.
/// Dropping the owner discards uncommitted text captures.
pub(crate) struct AsyncCapture {
    file: tokio::fs::File,
    reservation: Reservation,
    failure: Option<io::ErrorKind>,
    pending: Vec<u8>,
    written: usize,
}

impl AsyncCapture {
    pub(crate) async fn write_text(&mut self, text: &str) -> io::Result<()> {
        self.write_all(text.as_bytes()).await
    }

    pub(crate) async fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        healthy(self.failure)?;
        self.pending.extend_from_slice(bytes);
        self.flush().await
    }

    pub(crate) async fn flush(&mut self) -> io::Result<()> {
        healthy(self.failure)?;
        let result = async {
            while self.written < self.pending.len() {
                // write (not write_all) is cancellation safe; retain its offset
                // in the owner before the next suspension point.
                let written = self.file.write(&self.pending[self.written..]).await?;
                if written == 0 {
                    return Err(io::ErrorKind::WriteZero.into());
                }
                self.written += written;
            }
            self.file.flush().await?;
            self.pending.clear();
            self.written = 0;
            Ok(())
        }
        .await;
        track(&mut self.failure, result)
    }

    /// Materialize even an empty capture (the read snapshot contract).
    pub(crate) async fn finish(mut self) -> io::Result<CompletedCapture> {
        self.flush().await?;
        Ok(self.reservation.commit())
    }

    pub(crate) async fn finish_nonempty(mut self) -> io::Result<Option<CompletedCapture>> {
        self.flush().await?;
        let length = self.file.metadata().await?.len();
        self.reservation.commit_nonempty(length)
    }
}

impl JobManager {
    /// Reserve one job-bound capture without exposing its file path. Builtin text
    /// captures set `remove_on_abandon`; streams keep partial output readable.
    pub(crate) async fn pending_capture(
        &self,
        job: JobId,
        field: String,
        kind: CaptureKind,
        remove_on_abandon: bool,
    ) -> Result<PendingCapture, ToolError> {
        let directory = self.output_directory(job);
        tokio::task::spawn_blocking(move || {
            let mut pending = PendingCapture::create(job, &directory, &field, kind)?;
            pending.reservation.remove_on_abandon = remove_on_abandon;
            Ok(pending)
        })
        .await
        .map_err(|error| ToolError::Failed(error.to_string()))?
        .map_err(io::Error::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        future::{Future, poll_fn},
        task::Poll,
    };

    fn job(id: u64) -> JobId {
        JobId::new(id).unwrap()
    }

    fn text(directory: &Path, field: TextCaptureField) -> (PendingCapture, PathBuf) {
        let mut pending =
            PendingCapture::create(job(1), directory, &field.pointer(), CaptureKind::Text).unwrap();
        pending.reservation.remove_on_abandon = true;
        let path = pending.reservation.completed.path();
        (pending, path)
    }

    /// Polls a write exactly once, then drops it mid-flight.
    async fn poll_once(writing: impl Future) {
        tokio::pin!(writing);
        poll_fn(|cx| {
            let _ = writing.as_mut().poll(cx);
            Poll::Ready(())
        })
        .await;
    }

    #[test]
    fn ownership_prevents_truncation_and_invalid_pointers_create_nothing() {
        let root = tempfile::tempdir().unwrap();
        for field in ["not-a-pointer", "/result/~", "/result/~2"] {
            let error = PendingCapture::create(job(1), root.path(), field, CaptureKind::Json)
                .err()
                .unwrap();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        let create =
            |kind| PendingCapture::create(job(u64::MAX), root.path(), "/result/matches", kind);
        let mut writer = create(CaptureKind::Json).unwrap().open();
        writer.write_all(b"[\"a\"").unwrap();
        let checkpoint = writer.stream_position().unwrap();
        writer.write_all(b",\"binary\"").unwrap();
        writer.truncate(checkpoint).unwrap();
        writer.seek(io::SeekFrom::Start(checkpoint)).unwrap();
        writer.write_all(b"]").unwrap();
        assert!(create(CaptureKind::Text).is_err());
        let inventory = super::super::available_captures(root.path(), false).unwrap();
        assert!(matches!(inventory[0].kind, CaptureKind::Json));
        let completed = writer.finish().unwrap();
        assert!(completed.matches(job(u64::MAX), "/result/matches"));
        assert!(!completed.matches(job(1), "/result/matches"));
        assert!(!completed.matches(job(u64::MAX), "/result/paths"));
        let file = super::super::super::field_file(root.path(), "/result/matches");
        assert_eq!(std::fs::read(file).unwrap(), b"[\"a\"]");
    }

    #[test]
    fn completion_is_bound_to_job_field_and_explicit_empty_policy() {
        let directory = tempfile::tempdir().unwrap();
        let (pending, path) = text(directory.path(), TextCaptureField::Console);
        let completed = pending.open().finish().unwrap();
        assert!(
            path.exists(),
            "always-materialized empty snapshots remain registered"
        );
        let captures =
            |terminal| super::super::available_captures(directory.path(), terminal).unwrap();
        let descriptors = captures(true);
        assert_eq!(descriptors.len(), 1);
        assert_eq!(descriptors[0].field, "/result/console");
        assert!(matches!(descriptors[0].kind, CaptureKind::Text));
        assert!(
            !descriptors[0].complete,
            "a finalized but unpublished capture is not a completed job result"
        );
        assert!(completed.matches(job(1), "/result/console"));
        assert!(!completed.matches(job(2), "/result/console"));
        assert!(completed.belongs_to(job(1), directory.path()));
        assert!(!completed.belongs_to(job(1), tempfile::tempdir().unwrap().path()));
        // Exercise the canonical producer publication path, not the terminal flag alone.
        let document = serde_json::json!({"result": {}, "capture_complete": true});
        crate::job::output::save_completed(directory.path(), job(1), &document, vec![completed])
            .unwrap();
        assert!(captures(true)[0].complete);
        assert!(!captures(false)[0].complete);
    }

    #[test]
    fn abandoned_and_failed_sync_writers_cannot_publish_completion() {
        let directory = tempfile::tempdir().unwrap();
        let (pending, path) = text(directory.path(), TextCaptureField::Console);
        let mut capture = pending.open();
        capture.write_text("before failure\n").unwrap();
        let collision = PendingCapture::create(
            job(1),
            directory.path(),
            "/result/console",
            CaptureKind::Text,
        );
        assert!(collision.is_err());
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            contents, "before failure\n",
            "collision must not truncate or unlink a live writer"
        );
        drop(capture);
        assert!(!path.exists());

        let (pending, path) = text(directory.path(), TextCaptureField::Console);
        let mut capture = pending.open();
        // Deterministic read-only writer fault.
        capture.file = io::BufWriter::new(std::fs::File::open(&path).unwrap());
        assert!(capture.write_text("cannot write").is_err());
        assert!(capture.finish().is_err(), "an IO error poisons completion");
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn interrupted_async_write_finishes_without_loss_or_duplication() {
        let directory = tempfile::tempdir().unwrap();
        let (pending, path) = text(directory.path(), TextCaptureField::Stdout);
        let mut capture = pending.open_async();
        let payload = "é🦀".repeat(700_000);
        poll_once(capture.write_text(&payload)).await;
        assert!(capture.finish_nonempty().await.unwrap().is_some());
        assert_eq!(std::fs::read_to_string(path).unwrap(), payload);

        let (pending, path) = text(directory.path(), TextCaptureField::Stderr);
        let mut capture = pending.open_async();
        poll_once(capture.write_text(&"x".repeat(4 * 1024 * 1024))).await;
        drop(capture);
        assert!(!path.exists(), "abandoned async writes are discarded");
    }
}
