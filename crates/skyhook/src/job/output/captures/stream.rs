//! Host capture storage and publication receipts.
use std::{
    collections::HashMap,
    io,
    sync::{Arc, Mutex, PoisonError},
};

use super::{CaptureKind, Output};
use crate::{
    job::JobManager,
    session::{CaptureExtent, SessionStore},
    tool::ToolOutput,
};
use skyhook_tools::output::{
    Abandonment, CaptureEvent, CaptureFile, CaptureId, CaptureTarget, FieldPointer, OutputContext,
    OutputEvent, OutputSink, ProducedOutput,
};
use skyhook_worker::{fs::spill::SpillError, job::JobId};

/// Only successful consuming finalization constructs this evidence. Finalizing a
/// writer is not publication: discovery reports completion only after the
/// terminal document references this capture.
#[derive(Clone, Debug)]
pub struct CompletedCapture {
    capture: i64,
    field: FieldPointer,
    kind: CaptureKind,
    /// The file on the job's target that holds all of what this holds the first of.
    file: Option<CaptureFile>,
}

impl CompletedCapture {
    pub(crate) fn field(&self) -> &FieldPointer {
        &self.field
    }
    pub(crate) fn kind(&self) -> CaptureKind {
        self.kind
    }
    pub(crate) fn file(&self) -> Option<&CaptureFile> {
        self.file.as_ref()
    }
}

fn database(error: crate::db::DbError) -> io::Error {
    io::Error::other(error)
}

#[cfg(test)]
pub(crate) type PendingCapture = skyhook_tools::output::PendingProducer<StoredCapture>;

/// A capture of `field` reserved in `output`, which an abandoned producer retains.
#[cfg(test)]
pub(crate) fn create_capture(
    output: &Output,
    field: &FieldPointer,
    kind: CaptureKind,
) -> io::Result<PendingCapture> {
    Ok(PendingCapture::new(
        StoredCapture::reserve(output, field, kind)?,
        skyhook_tools::output::Abandon::Retain,
    ))
}

pub(crate) struct StoredCapture {
    output: Output,
    completed: CompletedCapture,
    extent: CaptureExtent,
    /// The file that holds it, once it passed the saved-field limit.
    spilled: Option<(std::fs::File, skyhook_worker::op::TargetPath)>,
}

impl StoredCapture {
    fn reserve(output: &Output, field: &FieldPointer, kind: CaptureKind) -> io::Result<Self> {
        let capture = output
            .db
            .create_capture(output.run(), field, kind)
            .map_err(database)?
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::AlreadyExists, "capture field is reserved")
            })?;
        Ok(Self {
            output: output.clone(),
            completed: CompletedCapture {
                capture,
                field: field.clone(),
                kind,
                file: None,
            },
            extent: CaptureExtent::default(),
            spilled: None,
        })
    }

    /// Move the capture's rows to a new file in the session host's spill directory.
    fn spill(&mut self) -> io::Result<()> {
        let (capture, extent) = (self.completed.capture, self.extent);
        let (mut file, path) = self.output.spill_file(capture)?;
        let moved = (self.output.db).spill_capture(capture, extent, (&mut file, &path));
        if let Err(error) = moved {
            let _ = std::fs::remove_file(path.as_str());
            return Err(error);
        }
        self.spilled = Some((file, path));
        Ok(())
    }

    fn changed(&self) {
        self.output.changes.changed(self.output.run());
    }

    /// The latest of the stream the capture holds the first bytes of, as its target
    /// told it: kept in memory, where each replaces the one before.
    fn latest(&mut self, extent: CaptureExtent, text: String) {
        let latest = crate::session::LiveTail {
            extent,
            text: text.into(),
        };
        (self.output.changes).latest(self.completed.capture, Some(latest));
        self.changed();
    }
}

/// What a capture's target last told of its stream goes with its writer: finished,
/// its job records what the target kept.
impl Drop for StoredCapture {
    fn drop(&mut self) {
        self.output.changes.latest(self.completed.capture, None);
    }
}

impl CaptureTarget for StoredCapture {
    type Finished = CompletedCapture;

    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        let after = self.extent.after(bytes);
        if self.spilled.is_none() && after.bytes > self.output.spill.bytes {
            self.spill()?;
        }
        let (db, capture) = (&self.output.db, self.completed.capture);
        self.extent = match &mut self.spilled {
            Some((file, _)) => {
                io::Write::write_all(file, bytes).map_err(SpillError::At)?;
                db.extend_spill(capture, after).map_err(database)?;
                after
            }
            None => (db.append_capture(capture, self.extent, bytes)).map_err(database)?,
        };
        self.changed();
        Ok(())
    }

    fn finish(&mut self) -> io::Result<CompletedCapture> {
        if let Some((file, _)) = &self.spilled {
            file.sync_data().map_err(SpillError::At)?;
        }
        Ok(self.completed.clone())
    }

    /// A discarded capture takes the file it was moved to with it.
    fn discard(&mut self) -> io::Result<()> {
        let spilled = self.output.db.delete_capture(self.completed.capture);
        if let Some(path) = spilled.map_err(database)? {
            let _ = std::fs::remove_file(path);
        }
        self.changed();
        Ok(())
    }
}

/// Imports locally produced output without exposing persistence to its producer.
pub(crate) struct HostOutput {
    context: OutputContext,
    collector: Arc<CaptureCollector>,
}

struct CaptureCollector {
    output: Output,
    store: SessionStore,
    supervisor: crate::job::Supervisor,
    runtime: tokio::runtime::Handle,
    captures: Mutex<HashMap<CaptureId, CollectedCapture>>,
}

enum CollectedCapture {
    Writing(StoredCapture),
    Finished(CompletedCapture),
    Closed,
}

/// The open capture `id` names.
fn writing(
    captures: &mut HashMap<CaptureId, CollectedCapture>,
    id: CaptureId,
) -> io::Result<&mut StoredCapture> {
    match captures.get_mut(&id) {
        Some(CollectedCapture::Writing(target)) => Ok(target),
        _ => Err(io::Error::other("capture is not open")),
    }
}

impl HostOutput {
    pub(crate) async fn new(jobs: &JobManager, job: JobId) -> Self {
        let collector = Arc::new(CaptureCollector::new(jobs, job).await);
        Self {
            context: OutputContext::new(collector.clone()),
            collector,
        }
    }

    pub(crate) fn context(&self) -> OutputContext {
        self.context.clone()
    }

    pub(crate) fn finish(&self, output: ProducedOutput) -> io::Result<ToolOutput> {
        let captures = self.collector.select(&output.captures)?;
        let mut imported = ToolOutput::new(output.value)
            .with_captures(captures)
            .with_images(output.images)
            .with_presented(output.presented);
        imported.streams = output.streams;
        Ok(imported)
    }

    /// [`finish`](Self::finish) what a call produced on the machine its connection
    /// reached along `route`, none for the session host. The files that machine keeps
    /// of the output are recorded as on that route, and the spill files it reports
    /// creating as the session's own, before the output is the job's.
    pub(crate) async fn finish_on(
        &self,
        route: &crate::target::Revisions,
        mut output: ProducedOutput,
    ) -> io::Result<ToolOutput> {
        let spilled = std::mem::take(&mut output.spilled);
        let imported = self.finish(output)?;
        let routed = |capture: &CompletedCapture| capture.file.is_some() && !route.is_empty();
        if !spilled.is_empty() || imported.captures.iter().any(routed) {
            let (store, run) = (&self.collector.store, self.collector.output.run());
            let kept = store.keep(run, route.clone(), spilled).await;
            kept.map_err(io::Error::other)?;
        }
        Ok(imported)
    }
}

impl CaptureCollector {
    async fn new(jobs: &JobManager, job: JobId) -> Self {
        Self {
            output: jobs.output(job).await,
            store: jobs.store().clone(),
            supervisor: jobs.supervisor().clone(),
            runtime: tokio::runtime::Handle::current(),
            captures: Mutex::new(HashMap::new()),
        }
    }

    /// The captures `finished` selects, each with the file its producer says holds
    /// all of it.
    pub(crate) fn select(
        &self,
        finished: &[skyhook_tools::output::FinishedOutput],
    ) -> io::Result<Vec<CompletedCapture>> {
        let mut captures = self.captures.lock().unwrap_or_else(PoisonError::into_inner);
        (finished.iter())
            .map(|selected| match captures.remove(&selected.id()) {
                Some(CollectedCapture::Finished(mut capture)) => {
                    capture.file = selected.file().cloned();
                    Ok(capture)
                }
                _ => Err(io::Error::other("selected capture is not finished")),
            })
            .collect()
    }
}

impl OutputSink for CaptureCollector {
    /// Write what a producer abandoned even when execution is forcibly dropped, on
    /// the blocking pool, as a producer can be dropped on an async worker; the
    /// session waits for it before it closes.
    fn abandon(self: Arc<Self>, event: OutputEvent) -> Abandonment {
        let (supervisor, runtime) = (self.supervisor.clone(), self.runtime.clone());
        let write = move || drop(self.send(event));
        Abandonment::Writing(supervisor.spawn_blocking(write, &runtime))
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
                )?));
            }
            CaptureEvent::Write { id, data } => writing(&mut captures, id)?.append(&data)?,
            CaptureEvent::Latest { id, extent, text } => {
                writing(&mut captures, id)?.latest(extent, text)
            }
            CaptureEvent::Discard { id } => {
                writing(&mut captures, id)?.discard()?;
                captures.insert(id, CollectedCapture::Closed);
            }
            CaptureEvent::Finish { id } => {
                let completed = writing(&mut captures, id)?.finish()?;
                captures.insert(id, CollectedCapture::Finished(completed));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::output::{Saved, tests::fixture};
    use skyhook_tools::output::{Abandon, TextCaptureField};
    use std::{
        future::{Future, poll_fn},
        io::Write,
        task::Poll,
    };

    impl CompletedCapture {
        pub(crate) fn capture_id(&self) -> i64 {
            self.capture
        }
    }

    fn text(output: &Output, field: TextCaptureField) -> PendingCapture {
        PendingCapture::new(
            StoredCapture::reserve(output, &field.pointer(), CaptureKind::Text).unwrap(),
            Abandon::Discard,
        )
    }

    fn captures(output: &Output, terminal: bool) -> Vec<super::super::CaptureDescriptor> {
        super::super::available_captures(&Saved::load(output).unwrap(), terminal)
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
    async fn host_output_preserves_abandonment_and_imports_selected_captures() {
        let (_root, manager, job) = fixture(None).await;
        let host = HostOutput::new(&manager, job).await;
        let context = host.context();
        let mut retained = context
            .pending_stream_capture(
                FieldPointer::result().property("partial"),
                CaptureKind::Text,
            )
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
        // Abandoned writes land on the blocking pool, which only the session's drain awaits.
        let output = manager.output(job).await;
        crate::tests::bounded(async {
            while output.test_bytes("/result/partial").as_deref() != Some(b"buffered tail")
                || output.test_bytes("/result/content").is_some()
            {
                tokio::task::yield_now().await;
            }
        })
        .await;
        let host = HostOutput::new(&manager, job).await;
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
            .pending_stream_capture(
                FieldPointer::result().property("matches"),
                CaptureKind::Json,
            )
            .await
            .unwrap()
            .open();
        writer.write_all(b"[1]").unwrap();
        let selected = writer.finish().unwrap();
        let image = crate::tests::png(b"source").await;
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
        let output = host
            .finish(
                ProducedOutput::new(serde_json::json!({"matches": []}))
                    .with_captures(vec![selected])
                    .with_images(vec![reference]),
            )
            .unwrap();
        assert_eq!(output.captures[0].field().as_str(), "/result/matches");
        assert_eq!(
            manager
                .output(job)
                .await
                .test_bytes("/result/matches")
                .unwrap(),
            b"[1]"
        );
    }

    /// Observers hear of a run's captured output once until it is read again.
    #[tokio::test]
    async fn captured_output_notifies_observers_once_until_it_is_read() {
        let (_root, manager, job) = fixture(None).await;
        let mut updates = manager.store().observe().updates;
        let output = manager.output(job).await;
        let mut notices = || {
            std::iter::from_fn(|| updates.try_recv().ok())
                .filter(|update| match update.event {
                    crate::session::RuntimeEvent::OutputChanged {
                        job: changed,
                        invocation,
                    } => {
                        assert_eq!((changed, invocation), (job, output.invocation));
                        true
                    }
                    _ => false,
                })
                .count()
        };
        let mut capture = text(&output, TextCaptureField::Stdout).open();
        for line in ["one\n", "two\n"] {
            capture.write_text(line).unwrap();
        }
        assert_eq!(notices(), 1);
        capture.write_text("three\n").unwrap();
        assert_eq!(notices(), 0);
        Saved::load(&output).unwrap();
        capture.write_text("four\n").unwrap();
        capture.write_text("five\n").unwrap();
        assert_eq!(notices(), 1);
    }

    /// A capture is complete only once its job is terminal and its result references it.
    #[tokio::test]
    async fn captures_complete_with_the_terminal_result_that_references_them() {
        let (_root, manager, job) = fixture(None).await;
        let output = manager.output(job).await;
        let matches = "/result/matches".parse().unwrap();
        let mut writer = crate::job::output::create_capture(&output, &matches, CaptureKind::Json)
            .unwrap()
            .open();
        writer.write_all(b"[\"a\"]").unwrap();
        let completed = writer.finish().unwrap();
        let empty = text(&output, TextCaptureField::Content)
            .open()
            .finish()
            .unwrap();
        let descriptors = captures(&output, true);
        assert_eq!(descriptors.len(), 2);
        assert!(descriptors.iter().all(|capture| !capture.complete));
        let completed =
            ToolOutput::new(serde_json::json!({})).with_captures(vec![completed, empty]);
        manager.test_complete(job, completed).await;
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

    #[test]
    fn writer_failures_and_interrupted_writes_preserve_capture_policy() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (_root, manager, job) = fixture(None).await;
            let output = manager.output(job).await;
            let content = TextCaptureField::Content.pointer();
            let mut capture = text(&output, TextCaptureField::Content).open();
            capture.write_text("before failure\n").unwrap();
            let collision =
                crate::job::output::create_capture(&output, &content, CaptureKind::Text);
            assert!(collision.is_err());
            assert_eq!(
                output.test_bytes(content.as_str()).unwrap(),
                b"before failure\n",
                "collision must not truncate or delete a live writer"
            );
            drop(capture);
            assert!(output.test_bytes(content.as_str()).is_none());

            let mut capture = text(&output, TextCaptureField::Content).open();
            // Deterministic storage fault: the capture row disappears under the writer.
            let capture_id = Saved::load(&output).unwrap().captures[&content].id;
            output.db.delete_capture(capture_id).unwrap();
            assert!(capture.write_text("cannot write").is_err());
            assert!(capture.finish().is_err(), "an IO error poisons completion");
            assert!(output.test_bytes(content.as_str()).is_none());
            let mut capture = text(&output, TextCaptureField::Stdout).open_async();
            // Several chunks, with characters across their boundaries.
            let payload = "é🦀".repeat(skyhook_worker::op::CHUNK_BYTES * 4 / 6);
            poll_once(capture.write_text(&payload)).await;
            assert!(capture.finish_nonempty().await.unwrap().is_some());
            let stdout = output
                .test_bytes(TextCaptureField::Stdout.pointer().as_str())
                .unwrap();
            assert_eq!(stdout, payload.as_bytes());
        });
    }
}
