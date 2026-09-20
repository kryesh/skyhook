//! Host capture storage and publication receipts.
use std::{
    collections::HashMap,
    io,
    sync::{Arc, Mutex, PoisonError},
};

use super::{CaptureKind, Output};
pub(crate) use crate::tool::output::TextCaptureField;
use crate::{
    identity::JobId,
    job::JobManager,
    session::{CaptureExtent, SessionStore},
    tool::output::{
        Abandon, Abandonment, CaptureEvent, CaptureId, CaptureTarget, OutputContext, OutputEvent,
        OutputSink, PendingProducer, ProducedOutput, Producer,
    },
    tool::{ToolError, ToolOutput},
};

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

pub(crate) type PendingCapture = PendingProducer<StoredCapture>;
pub(crate) type CaptureWriter = Producer<StoredCapture>;

impl PendingCapture {
    pub(crate) fn create(output: &Output, field: &str, kind: CaptureKind) -> io::Result<Self> {
        Ok(Self::new(
            StoredCapture::reserve(output, field, kind, false)?,
            Abandon::Retain,
        ))
    }

    pub(crate) fn rendering(output: &Output, field: &str) -> io::Result<Self> {
        Ok(Self::new(
            StoredCapture::reserve(output, field, CaptureKind::Unknown, true)?,
            Abandon::Discard,
        ))
    }
}

pub(crate) struct StoredCapture {
    output: Output,
    completed: CompletedCapture,
    extent: CaptureExtent,
}

impl StoredCapture {
    fn reserve(
        output: &Output,
        field: &str,
        kind: CaptureKind,
        rendered: bool,
    ) -> io::Result<Self> {
        super::validate_capture_field(field)?;
        let capture = output
            .db
            .create_capture(output.job.get(), field, kind.as_str(), rendered)
            .map_err(database)?
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::AlreadyExists, "capture field is reserved")
            })?;
        Ok(Self {
            output: output.clone(),
            completed: CompletedCapture {
                job: output.job,
                capture,
                field: field.into(),
                kind,
            },
            extent: CaptureExtent::default(),
        })
    }
}

impl CaptureTarget for StoredCapture {
    type Finished = CompletedCapture;

    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.extent = self
            .output
            .db
            .append_capture(self.completed.capture, self.extent, bytes)
            .map_err(database)?;
        Ok(())
    }

    fn truncate(&mut self, length: u64) -> io::Result<()> {
        if length > self.extent.bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "capture cannot be extended by truncation",
            ));
        }
        self.extent = self
            .output
            .db
            .truncate_capture(self.completed.capture, length)
            .map_err(database)?;
        Ok(())
    }

    fn finish(&mut self) -> io::Result<CompletedCapture> {
        self.output
            .db
            .finish_capture(self.completed.capture)
            .map_err(database)?;
        Ok(self.completed.clone())
    }

    fn discard(&mut self) -> io::Result<()> {
        self.output
            .db
            .delete_capture(self.completed.capture)
            .map_err(database)
    }
}

impl JobManager {
    pub(crate) async fn pending_capture(
        &self,
        job: JobId,
        field: String,
        kind: CaptureKind,
        remove_on_abandon: bool,
    ) -> Result<PendingCapture, ToolError> {
        let output = self.output(job);
        crate::job::output::blocking(move || {
            let abandon = if remove_on_abandon {
                Abandon::Discard
            } else {
                Abandon::Retain
            };
            Ok(PendingCapture::new(
                StoredCapture::reserve(&output, &field, kind, false)?,
                abandon,
            ))
        })
        .await
    }
}

/// Imports locally produced output without exposing persistence to its producer.
pub(crate) struct HostOutput {
    context: OutputContext,
    collector: Arc<CaptureCollector>,
}

pub(crate) struct CaptureCollector {
    output: Output,
    store: SessionStore,
    runtime: tokio::runtime::Handle,
    captures: Mutex<HashMap<CaptureId, CollectedCapture>>,
}

enum CollectedCapture {
    Writing(StoredCapture),
    Finished(CompletedCapture),
    Closed,
}

impl HostOutput {
    pub(crate) fn new(store: SessionStore, job: JobId) -> Self {
        let collector = Arc::new(CaptureCollector::new(store, job));
        Self {
            context: OutputContext::new(collector.clone()),
            collector,
        }
    }

    pub(crate) fn context(&self) -> OutputContext {
        self.context.clone()
    }

    pub(crate) fn finish(&self, output: ProducedOutput) -> io::Result<ToolOutput> {
        if output
            .captures
            .iter()
            .any(|capture| !self.context.owns(capture))
        {
            return Err(io::Error::other("capture belongs to another producer"));
        }
        let captures = self
            .collector
            .select(output.captures.iter().map(|capture| capture.id()))?;
        let mut imported = ToolOutput::new(output.value)
            .with_captures(captures)
            .with_images(output.images);
        imported.streams = output.streams;
        Ok(imported)
    }
}

impl CaptureCollector {
    pub(crate) fn new(store: SessionStore, job: JobId) -> Self {
        Self {
            output: Output {
                db: store.outputs(),
                job,
            },
            store,
            runtime: tokio::runtime::Handle::current(),
            captures: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn select(
        &self,
        ids: impl IntoIterator<Item = CaptureId>,
    ) -> io::Result<Vec<CompletedCapture>> {
        let mut captures = self.captures.lock().unwrap_or_else(PoisonError::into_inner);
        ids.into_iter()
            .map(|id| match captures.remove(&id) {
                Some(CollectedCapture::Finished(capture)) => Ok(capture),
                _ => Err(io::Error::other("selected capture is not finished")),
            })
            .collect()
    }
}

impl OutputSink for CaptureCollector {
    fn abandon(&self, event: OutputEvent) -> Abandonment {
        // Preserve local capture durability even when execution is forcibly dropped.
        let _ = self.send(event);
        Abandonment::Handled
    }

    fn send(&self, event: OutputEvent) -> io::Result<()> {
        let event = match event {
            OutputEvent::Capture(event) => event,
            OutputEvent::Image { file, image } => {
                self.runtime
                    .block_on(self.store.store_image(file, &image))
                    .map_err(io::Error::other)?;
                return Ok(());
            }
        };
        let mut captures = self.captures.lock().unwrap_or_else(PoisonError::into_inner);
        match event {
            CaptureEvent::Open { id, field, kind } => {
                let std::collections::hash_map::Entry::Vacant(entry) = captures.entry(id) else {
                    return Err(io::Error::other("duplicate capture"));
                };
                entry.insert(CollectedCapture::Writing(StoredCapture::reserve(
                    &self.output,
                    &field,
                    kind,
                    false,
                )?));
            }
            CaptureEvent::Write { id, data } => {
                let Some(CollectedCapture::Writing(target)) = captures.get_mut(&id) else {
                    return Err(io::Error::other("capture is not open"));
                };
                target.append(&data)?;
            }
            CaptureEvent::Truncate { id, length } => {
                let Some(CollectedCapture::Writing(target)) = captures.get_mut(&id) else {
                    return Err(io::Error::other("capture is not open"));
                };
                target.truncate(length)?;
            }
            CaptureEvent::Discard { id } => {
                let Some(CollectedCapture::Writing(target)) = captures.get_mut(&id) else {
                    return Err(io::Error::other("capture is not open"));
                };
                target.discard()?;
                captures.insert(id, CollectedCapture::Closed);
            }
            CaptureEvent::Finish { id } => {
                let Some(CollectedCapture::Writing(target)) = captures.get_mut(&id) else {
                    return Err(io::Error::other("capture is not open"));
                };
                let completed = target.finish()?;
                captures.insert(id, CollectedCapture::Finished(completed));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::output::{Saved, captures::available_captures, tests::fixture};
    use std::{
        future::{Future, poll_fn},
        io::{Seek, Write},
        task::Poll,
    };

    fn text(output: &Output, field: TextCaptureField) -> PendingCapture {
        PendingCapture::new(
            StoredCapture::reserve(output, &field.pointer(), CaptureKind::Text, false).unwrap(),
            Abandon::Discard,
        )
    }

    fn captures(output: &Output, terminal: bool) -> Vec<super::super::CaptureDescriptor> {
        available_captures(&Saved::load(output).unwrap(), terminal)
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
    async fn host_output_preserves_abandonment_and_validates_origin() {
        let (_root, manager, job) = fixture(None).await;
        let host = HostOutput::new(manager.store().clone(), job);
        let context = host.context();
        let mut retained = context
            .pending_stream_capture("/result/partial", CaptureKind::Text)
            .await
            .unwrap()
            .open();
        retained.write_all(b"buffered tail").unwrap();
        let mut discarded = context
            .text_capture(TextCaptureField::Content)
            .await
            .unwrap()
            .open();
        discarded.write_all(b"unpublished text").unwrap();
        // A forced abort can drop the outer context before it reaches settlement.
        drop(context);
        drop(host);
        drop(retained);
        drop(discarded);
        let output = manager.output(job);
        assert_eq!(
            output.test_bytes("/result/partial").unwrap(),
            b"buffered tail"
        );
        assert!(output.test_bytes("/result/content").is_none());
        let host = HostOutput::new(manager.store().clone(), job);
        let context = host.context();
        assert!(
            context
                .text_capture(TextCaptureField::Stdout)
                .await
                .unwrap()
                .open_async()
                .finish_nonempty()
                .await
                .unwrap()
                .is_none()
        );

        let mut writer = context
            .pending_stream_capture("/result/matches", CaptureKind::Json)
            .await
            .unwrap()
            .open();
        writer.write_all(b"[1,2").unwrap();
        writer.truncate(2).unwrap();
        writer.write_all(b"]").unwrap();
        let selected = writer.finish().unwrap();
        let image = crate::media::Image::new(b"\x89PNG\r\n\x1a\nsource".to_vec()).unwrap();
        let reference = context
            .store_image(Some("image.png".into()), &image)
            .await
            .unwrap();
        context.settle().await.unwrap();
        assert_eq!(
            manager
                .store()
                .read_blob(&reference.blob, image.bytes().len())
                .await
                .unwrap(),
            image.bytes()
        );
        let foreign = HostOutput::new(manager.store().clone(), job);
        assert!(
            foreign
                .finish(
                    ProducedOutput::new(serde_json::Value::Null)
                        .with_captures(vec![selected.clone()])
                )
                .is_err()
        );
        let output = host
            .finish(
                ProducedOutput::new(serde_json::json!({"matches": []}))
                    .with_captures(vec![selected])
                    .with_images(vec![reference]),
            )
            .unwrap();
        assert!(output.captures[0].matches(job, "/result/matches"));
        assert_eq!(
            manager.output(job).test_bytes("/result/matches").unwrap(),
            b"[1]"
        );
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
        assert_eq!(output.test_bytes("/result/matches").unwrap(), b"[\"a\"]");
        let empty = text(&output, TextCaptureField::Console)
            .open()
            .finish()
            .unwrap();
        let descriptors = captures(&output, true);
        assert_eq!(descriptors.len(), 2);
        assert!(descriptors.iter().all(|capture| !capture.complete));
        let document = serde_json::json!({"result": {}, "capture_complete": true});
        crate::job::output::save_completed(&output, &document, vec![completed, empty]).unwrap();
        assert!(
            captures(&output, true)
                .iter()
                .all(|capture| capture.complete)
        );
        assert!(
            captures(&output, false)
                .iter()
                .all(|capture| !capture.complete)
        );
    }

    #[tokio::test]
    async fn writer_failures_and_interrupted_writes_preserve_capture_policy() {
        let (_root, manager, job) = fixture(None).await;
        let output = manager.output(job);
        let console = TextCaptureField::Console.pointer();
        let mut capture = text(&output, TextCaptureField::Console).open();
        capture.write_text("before failure\n").unwrap();
        let collision = PendingCapture::create(&output, &console, CaptureKind::Text);
        assert!(collision.is_err());
        assert_eq!(
            output.test_bytes(&console).unwrap(),
            b"before failure\n",
            "collision must not truncate or delete a live writer"
        );
        drop(capture);
        assert!(output.test_bytes(&console).is_none());

        let mut capture = text(&output, TextCaptureField::Console).open();
        // Deterministic storage fault: the capture row disappears under the writer.
        let capture_id = Saved::load(&output).unwrap().captures[&console].id;
        output.db.delete_capture(capture_id).unwrap();
        assert!(capture.write_text("cannot write").is_err());
        assert!(capture.finish().is_err(), "an IO error poisons completion");
        assert!(output.test_bytes(&console).is_none());
        let mut capture = text(&output, TextCaptureField::Stdout).open_async();
        let payload = "é🦀".repeat(700_000);
        poll_once(capture.write_text(&payload)).await;
        assert!(capture.finish_nonempty().await.unwrap().is_some());
        let stdout = output
            .test_bytes(&TextCaptureField::Stdout.pointer())
            .unwrap();
        assert_eq!(stdout, payload.as_bytes());

        let mut capture = text(&output, TextCaptureField::Stderr).open_async();
        poll_once(capture.write_text(&"x".repeat(4 * 1024 * 1024))).await;
        drop(capture);
        // The in-flight write finishes, then the abandoned capture is discarded.
        let stderr = TextCaptureField::Stderr.pointer();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while output.test_bytes(&stderr).is_some() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("abandoned async writes are discarded");
    }
}
