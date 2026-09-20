//! Job/field-bound capture streams. Completion proves successful IO, not valid JSON.
//!
//! Streamed captures (search, remote artifacts) keep failed or abandoned partial
//! output readable as incomplete output, and an explicitly streamed empty capture
//! is materialized. Builtin text captures delete their row when abandoned;
//! `finish_nonempty` is the process/console contract that omits empty streams.
use std::{
    io::{self, Seek, Write},
    sync::{Arc, Mutex, PoisonError},
};

use super::{CaptureKind, Output};
use crate::{identity::JobId, job::JobManager, session::CaptureExtent, tool::ToolError};

/// Buffered bytes are committed once they reach this size, or on flush.
const FLUSH_BYTES: usize = 64 * 1024;

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
/// reports completion only after the terminal document references this capture.
#[derive(Clone, Debug)]
pub(crate) struct CompletedCapture {
    job: JobId,
    capture: i64,
    field: String,
    kind: CaptureKind,
}

impl CompletedCapture {
    /// Bound to `job` and still the row registered at its pointer.
    pub(crate) fn belongs_to(&self, job: JobId, capture: i64) -> bool {
        self.job == job && self.capture == capture
    }

    #[cfg(test)]
    pub(crate) fn matches(&self, job: JobId, field: &str) -> bool {
        self.job == job && self.field == field
    }

    pub(crate) fn field(&self) -> &str {
        &self.field
    }

    pub(crate) fn capture_id(&self) -> i64 {
        self.capture
    }

    pub(crate) fn kind(&self) -> CaptureKind {
        self.kind
    }
}

fn database(error: crate::session::DbError) -> io::Error {
    io::Error::other(error)
}

/// A reserved capture row, not yet opened for writing.
pub(crate) struct PendingCapture {
    writer: CaptureWriter,
}

impl PendingCapture {
    pub(crate) fn create(output: &Output, field: &str, kind: CaptureKind) -> io::Result<Self> {
        Self::reserve(output, field, kind, false)
    }

    /// Reserve a cached rendering of a saved value, discarded unless finished.
    pub(crate) fn rendering(output: &Output, field: &str) -> io::Result<Self> {
        let mut pending = Self::reserve(output, field, CaptureKind::Unknown, true)?;
        pending.writer.remove_on_abandon = true;
        Ok(pending)
    }

    fn reserve(
        output: &Output,
        field: &str,
        kind: CaptureKind,
        rendered: bool,
    ) -> io::Result<Self> {
        super::validate_capture_field(field)?;
        // Reservation is atomic: a losing producer, even with a different kind,
        // neither rewrites nor truncates the existing owner's capture.
        let capture = output
            .db
            .create_capture(output.job.get(), field, kind.as_str(), rendered)
            .map_err(database)?
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::AlreadyExists, "capture field is reserved")
            })?;
        Ok(Self {
            writer: CaptureWriter {
                output: output.clone(),
                completed: CompletedCapture {
                    job: output.job,
                    capture,
                    field: field.into(),
                    kind,
                },
                remove_on_abandon: false,
                extent: CaptureExtent::default(),
                buffer: Vec::new(),
                failure: None,
            },
        })
    }

    pub(crate) fn open(self) -> CaptureWriter {
        self.writer
    }

    pub(crate) fn open_async(self) -> AsyncCapture {
        AsyncCapture {
            writer: Arc::new(Mutex::new(self.writer)),
            pending: None,
        }
    }
}

/// Appends to one capture row. Seek/truncate serve grep's binary-match rollback but
/// cannot change the bound job or field. An IO failure poisons completion.
pub(crate) struct CaptureWriter {
    output: Output,
    completed: CompletedCapture,
    /// Builtin text captures delete their row unless finished.
    remove_on_abandon: bool,
    extent: CaptureExtent,
    buffer: Vec<u8>,
    failure: Option<io::ErrorKind>,
}

impl CaptureWriter {
    fn healthy(&self) -> io::Result<()> {
        match self.failure {
            Some(kind) => Err(io::Error::new(
                kind,
                "capture cannot finish after an IO error",
            )),
            None => Ok(()),
        }
    }

    fn track<T>(&mut self, result: io::Result<T>) -> io::Result<T> {
        if let Err(error) = &result {
            self.failure = Some(error.kind());
        }
        result
    }

    fn position(&self) -> u64 {
        self.extent.bytes + self.buffer.len() as u64
    }

    /// Append text and commit it, so a live console capture is pageable at once.
    pub(crate) fn write_text(&mut self, text: &str) -> io::Result<()> {
        self.write_all(text.as_bytes())?;
        self.flush()
    }

    pub(crate) fn truncate(&mut self, length: u64) -> io::Result<()> {
        self.flush()?;
        let result = if length > self.extent.bytes {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "capture cannot be extended by truncation",
            ))
        } else {
            self.output
                .db
                .truncate_capture(self.completed.capture, length)
                .map_err(database)
        };
        self.extent = self.track(result)?;
        Ok(())
    }

    fn close(&mut self) -> io::Result<CompletedCapture> {
        self.flush()?;
        let result = self
            .output
            .db
            .finish_capture(self.completed.capture)
            .map_err(database);
        self.track(result)?;
        self.remove_on_abandon = false;
        Ok(self.completed.clone())
    }

    pub(crate) fn finish(mut self) -> io::Result<CompletedCapture> {
        self.close()
    }

    /// Omit and delete empty streams (process and console contract).
    pub(crate) fn finish_nonempty(mut self) -> io::Result<Option<CompletedCapture>> {
        self.close_nonempty()
    }

    fn close_nonempty(&mut self) -> io::Result<Option<CompletedCapture>> {
        self.flush()?;
        if self.extent.bytes == 0 {
            self.output
                .db
                .delete_capture(self.completed.capture)
                .map_err(database)?;
            self.remove_on_abandon = false;
            return Ok(None);
        }
        self.close().map(Some)
    }
}

impl Drop for CaptureWriter {
    fn drop(&mut self) {
        if self.remove_on_abandon {
            // Best effort: a retained row is only reported as an incomplete capture.
            let _ = self.output.db.delete_capture(self.completed.capture);
        } else {
            // An abandoned stream keeps its partial output readable.
            let _ = self.flush();
        }
    }
}

impl Write for CaptureWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.healthy()?;
        self.buffer.extend_from_slice(bytes);
        if self.buffer.len() >= FLUSH_BYTES {
            self.flush()?;
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.healthy()?;
        if self.buffer.is_empty() {
            return Ok(());
        }
        let result = self
            .output
            .db
            .append_capture(self.completed.capture, self.extent, &self.buffer)
            .map_err(database);
        self.extent = self.track(result)?;
        self.buffer.clear();
        Ok(())
    }
}

/// Only the current end is addressable; rollback truncates before seeking to it.
impl Seek for CaptureWriter {
    fn seek(&mut self, position: io::SeekFrom) -> io::Result<u64> {
        self.healthy()?;
        let end = self.position();
        match position {
            io::SeekFrom::Start(offset) if offset == end => Ok(end),
            io::SeekFrom::Current(0) | io::SeekFrom::End(0) => Ok(end),
            _ => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "captures only append at their end",
            )),
        }
    }
}

/// Async producers' view of a capture. Writes run in order on the blocking pool;
/// one whose caller is dropped still completes before the next starts, so accepted
/// bytes are neither lost nor duplicated.
pub(crate) struct AsyncCapture {
    writer: Arc<Mutex<CaptureWriter>>,
    pending: Option<tokio::task::JoinHandle<()>>,
}

impl AsyncCapture {
    async fn run<T: Send + 'static>(
        &mut self,
        operation: impl FnOnce(&mut CaptureWriter) -> io::Result<T> + Send + 'static,
    ) -> io::Result<T> {
        if let Some(pending) = &mut self.pending {
            let _ = pending.await;
        }
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let writer = self.writer.clone();
        self.pending = Some(tokio::task::spawn_blocking(move || {
            let mut writer = writer.lock().unwrap_or_else(PoisonError::into_inner);
            let _ = sender.send(operation(&mut writer));
        }));
        let result = receiver.await.map_err(io::Error::other)?;
        self.pending = None;
        result
    }

    pub(crate) async fn write_text(&mut self, text: &str) -> io::Result<()> {
        self.write_all(text.as_bytes()).await
    }

    pub(crate) async fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        let bytes = bytes.to_vec();
        self.run(move |writer| {
            writer.write_all(&bytes)?;
            writer.flush()
        })
        .await
    }

    /// Materialize even an empty capture (the read snapshot contract).
    pub(crate) async fn finish(mut self) -> io::Result<CompletedCapture> {
        self.run(CaptureWriter::close).await
    }

    pub(crate) async fn finish_nonempty(mut self) -> io::Result<Option<CompletedCapture>> {
        self.run(CaptureWriter::close_nonempty).await
    }
}

impl JobManager {
    /// Reserve one job-bound capture. Builtin text captures set `remove_on_abandon`;
    /// streams keep partial output readable.
    pub(crate) async fn pending_capture(
        &self,
        job: JobId,
        field: String,
        kind: CaptureKind,
        remove_on_abandon: bool,
    ) -> Result<PendingCapture, ToolError> {
        let output = self.output(job);
        crate::job::output::blocking(move || {
            let mut pending = PendingCapture::create(&output, &field, kind)?;
            pending.writer.remove_on_abandon = remove_on_abandon;
            Ok(pending)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::output::{Saved, captures::available_captures, tests::fixture};
    use std::{
        future::{Future, poll_fn},
        task::Poll,
    };

    fn text(output: &Output, field: TextCaptureField) -> PendingCapture {
        let mut pending =
            PendingCapture::create(output, &field.pointer(), CaptureKind::Text).unwrap();
        pending.writer.remove_on_abandon = true;
        pending
    }

    fn captures(output: &Output, terminal: bool) -> Vec<super::super::CaptureDescriptor> {
        available_captures(&Saved::load(output).unwrap(), terminal)
    }

    fn bytes(output: &Output, field: &str) -> Option<Vec<u8>> {
        Saved::load(output).unwrap().bytes(field).unwrap()
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

    #[tokio::test]
    async fn ownership_prevents_truncation_and_invalid_pointers_create_nothing() {
        let (_root, manager, job) = fixture(None).await;
        let output = manager.output(job);
        for field in ["not-a-pointer", "/result/~", "/result/~2"] {
            let error = PendingCapture::create(&output, field, CaptureKind::Json)
                .err()
                .unwrap();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
        assert!(captures(&output, false).is_empty());
        let create = |kind| PendingCapture::create(&output, "/result/matches", kind);
        let mut writer = create(CaptureKind::Json).unwrap().open();
        writer.write_all(b"[\"a\"").unwrap();
        let checkpoint = writer.stream_position().unwrap();
        writer.write_all(b",\"binary\"").unwrap();
        writer.flush().unwrap();
        writer.truncate(checkpoint).unwrap();
        writer.seek(io::SeekFrom::Start(checkpoint)).unwrap();
        writer.write_all(b"]").unwrap();
        assert!(create(CaptureKind::Text).is_err());
        let inventory = captures(&output, false);
        assert!(matches!(inventory[0].kind, CaptureKind::Json));
        let completed = writer.finish().unwrap();
        assert!(completed.matches(job, "/result/matches"));
        assert!(!completed.matches(JobId::new(job.get() + 1).unwrap(), "/result/matches"));
        assert!(!completed.matches(job, "/result/paths"));
        assert_eq!(bytes(&output, "/result/matches").unwrap(), b"[\"a\"]");
    }

    #[tokio::test]
    async fn completion_is_bound_to_job_field_and_explicit_empty_policy() {
        let (_root, manager, job) = fixture(None).await;
        let output = manager.output(job);
        let completed = text(&output, TextCaptureField::Console)
            .open()
            .finish()
            .unwrap();
        let descriptors = captures(&output, true);
        assert_eq!(
            descriptors.len(),
            1,
            "finished empty captures stay registered"
        );
        assert_eq!(descriptors[0].field, "/result/console");
        assert!(matches!(descriptors[0].kind, CaptureKind::Text));
        assert!(
            !descriptors[0].complete,
            "a finalized but unpublished capture is not a completed job result"
        );
        assert!(completed.matches(job, "/result/console"));
        assert!(!completed.matches(JobId::new(job.get() + 1).unwrap(), "/result/console"));
        // Exercise the canonical producer publication path, not the terminal flag alone.
        let document = serde_json::json!({"result": {}, "capture_complete": true});
        crate::job::output::save_completed(&output, &document, vec![completed]).unwrap();
        assert!(captures(&output, true)[0].complete);
        assert!(!captures(&output, false)[0].complete);
    }

    #[tokio::test]
    async fn abandoned_and_failed_sync_writers_cannot_publish_completion() {
        let (_root, manager, job) = fixture(None).await;
        let output = manager.output(job);
        let console = TextCaptureField::Console.pointer();
        let mut capture = text(&output, TextCaptureField::Console).open();
        capture.write_text("before failure\n").unwrap();
        let collision = PendingCapture::create(&output, &console, CaptureKind::Text);
        assert!(collision.is_err());
        assert_eq!(
            bytes(&output, &console).unwrap(),
            b"before failure\n",
            "collision must not truncate or delete a live writer"
        );
        drop(capture);
        assert!(bytes(&output, &console).is_none());

        let mut capture = text(&output, TextCaptureField::Console).open();
        // Deterministic storage fault: the capture row disappears under the writer.
        output.db.delete_capture(capture.completed.capture).unwrap();
        assert!(capture.write_text("cannot write").is_err());
        assert!(capture.finish().is_err(), "an IO error poisons completion");
        assert!(bytes(&output, &console).is_none());
    }

    #[tokio::test]
    async fn interrupted_async_write_finishes_without_loss_or_duplication() {
        let (_root, manager, job) = fixture(None).await;
        let output = manager.output(job);
        let mut capture = text(&output, TextCaptureField::Stdout).open_async();
        let payload = "é🦀".repeat(700_000);
        poll_once(capture.write_text(&payload)).await;
        assert!(capture.finish_nonempty().await.unwrap().is_some());
        let stdout = bytes(&output, &TextCaptureField::Stdout.pointer()).unwrap();
        assert_eq!(stdout, payload.as_bytes());

        let mut capture = text(&output, TextCaptureField::Stderr).open_async();
        poll_once(capture.write_text(&"x".repeat(4 * 1024 * 1024))).await;
        drop(capture);
        // The in-flight write finishes, then the abandoned capture is discarded.
        let stderr = TextCaptureField::Stderr.pointer();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while bytes(&output, &stderr).is_some() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("abandoned async writes are discarded");
    }
}
