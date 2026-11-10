//! Captures a call's fields stream into, in order and with backpressure, and the
//! producers that write them.

use std::{
    io::{self, Write},
    num::NonZeroU64,
    sync::{Arc, Mutex, PoisonError},
};

use serde::Serialize;
use skyhook_base::enums::NamedEnum;
use skyhook_media::{BlobRef, Image, ImageRef};
use skyhook_worker::op::CHUNK_BYTES;

use super::{FieldPointer, FinishedOutput, TextCaptureField};

#[derive(Clone, Copy, Debug, Serialize, schemars::JsonSchema, PartialEq, Eq, NamedEnum)]
#[serde(rename_all = "snake_case")]
pub enum CaptureKind {
    Text,
    Json,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Abandon {
    Retain,
    Discard,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CaptureId(NonZeroU64);

impl CaptureId {
    pub const FIRST: Self = Self(NonZeroU64::MIN);

    pub(crate) fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

#[derive(Debug)]
pub enum CaptureEvent {
    Open {
        id: CaptureId,
        field: FieldPointer,
        kind: CaptureKind,
    },
    Write {
        id: CaptureId,
        data: Vec<u8>,
    },
    /// The latest of a stream whose target keeps what follows the bytes written here:
    /// what it counted of the stream so far, and its last bytes. Each replaces the
    /// one before.
    Latest {
        id: CaptureId,
        extent: skyhook_worker::op::Extent,
        text: String,
    },
    Discard {
        id: CaptureId,
    },
    Finish {
        id: CaptureId,
    },
}

#[derive(Debug)]
pub enum OutputEvent {
    Capture(CaptureEvent),
    Image { file: Option<String>, image: Image },
}

pub enum Abandonment {
    /// The sink is writing it already; settling waits for the write.
    Writing(tokio::task::JoinHandle<()>),
    /// Settling writes it.
    Deferred(OutputEvent),
}

/// Sends in order, applying backpressure before accepting the next event.
pub trait OutputSink: Send + Sync {
    fn send(&self, event: OutputEvent) -> io::Result<()>;

    /// Handle cleanup once, or return ownership for the settlement barrier.
    fn abandon(self: Arc<Self>, event: OutputEvent) -> Abandonment {
        Abandonment::Deferred(event)
    }
}

#[derive(Clone)]
pub struct OutputContext {
    inner: Arc<Emitter>,
}

struct Emitter {
    sink: Arc<dyn OutputSink>,
    state: Mutex<EmitterState>,
    changed: tokio::sync::Notify,
    settling: Arc<tokio::sync::Mutex<()>>,
}

struct EmitterState {
    next: CaptureId,
    active: usize,
    abandoned: Vec<OutputEvent>,
    /// Abandoned output its sink is writing.
    writing: Vec<tokio::task::JoinHandle<()>>,
}

impl OutputContext {
    pub fn new(sink: Arc<dyn OutputSink>) -> Self {
        Self {
            inner: Arc::new(Emitter {
                sink,
                state: Mutex::new(EmitterState {
                    next: CaptureId::FIRST,
                    active: 0,
                    abandoned: Vec::new(),
                    writing: Vec::new(),
                }),
                changed: tokio::sync::Notify::new(),
                settling: Arc::new(tokio::sync::Mutex::new(())),
            }),
        }
    }

    pub async fn pending_stream_capture(
        &self,
        field: FieldPointer,
        kind: CaptureKind,
    ) -> io::Result<PendingOutput> {
        self.pending_capture(field, kind, Abandon::Retain).await
    }

    pub async fn text_capture(&self, field: TextCaptureField) -> io::Result<PendingOutput> {
        self.pending_capture(field.pointer(), CaptureKind::Text, Abandon::Discard)
            .await
    }

    pub(crate) async fn pending_capture(
        &self,
        field: FieldPointer,
        kind: CaptureKind,
        abandon: Abandon,
    ) -> io::Result<PendingOutput> {
        let inner = self.inner.clone();
        let id = {
            let mut state = inner.state.lock().unwrap_or_else(PoisonError::into_inner);
            let id = state.next;
            state.next = id.next();
            state.active += 1;
            id
        };
        let target = EventCapture {
            activity: Activity { inner },
            id,
        };
        tokio::task::spawn_blocking(move || {
            target
                .activity
                .inner
                .sink
                .send(OutputEvent::Capture(CaptureEvent::Open { id, field, kind }))?;
            Ok(PendingProducer::new(target, abandon))
        })
        .await
        .map_err(io::Error::other)?
    }

    /// Drain accepted operations and abandoned output before publishing a terminal result.
    pub async fn settle(&self) -> io::Result<()> {
        let settling = self.inner.settling.clone().lock_owned().await;
        let (writing, abandoned) = loop {
            let changed = self.inner.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let mut state = self
                    .inner
                    .state
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                if state.active == 0 {
                    let writing = std::mem::take(&mut state.writing);
                    break (writing, std::mem::take(&mut state.abandoned));
                }
            }
            changed.await;
        };
        for written in writing {
            written.await.map_err(io::Error::other)?;
        }
        if abandoned.is_empty() {
            return Ok(());
        }
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let _settling = settling;
            for event in abandoned {
                inner.sink.send(event)?;
            }
            Ok(())
        })
        .await
        .map_err(io::Error::other)?
    }

    pub async fn store_image(&self, file: Option<String>, image: &Image) -> io::Result<ImageRef> {
        let reference = ImageRef {
            file: file.clone(),
            format: image.format(),
            blob: BlobRef::of(image.bytes()),
        };
        let image = image.clone();
        let inner = self.inner.clone();
        inner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .active += 1;
        let activity = Activity { inner };
        tokio::task::spawn_blocking(move || {
            let result = activity.inner.sink.send(OutputEvent::Image { file, image });
            drop(activity);
            result
        })
        .await
        .map_err(io::Error::other)??;
        Ok(reference)
    }
}

pub(crate) type PendingOutput = PendingProducer<EventCapture>;
pub type OutputWriter = Producer<EventCapture>;
pub(crate) type AsyncOutput = AsyncProducer<EventCapture>;

pub struct EventCapture {
    activity: Activity,
    id: CaptureId,
}

impl AsyncOutput {
    /// Tell the sink the latest of the stream this capture holds the first bytes of,
    /// after the writes accepted before it.
    pub(crate) async fn latest(
        &mut self,
        extent: skyhook_worker::op::Extent,
        text: String,
    ) -> io::Result<()> {
        self.run(move |writer| {
            writer.healthy()?;
            let (sink, id) = (&writer.target.activity.inner.sink, writer.target.id);
            let told = sink.send(OutputEvent::Capture(CaptureEvent::Latest {
                id,
                extent,
                text,
            }));
            writer.track(told)
        })
        .await
    }
}

impl CaptureTarget for EventCapture {
    type Finished = FinishedOutput;

    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.activity
            .inner
            .sink
            .send(OutputEvent::Capture(CaptureEvent::Write {
                id: self.id,
                data: bytes.to_vec(),
            }))
    }

    fn finish(&mut self) -> io::Result<Self::Finished> {
        self.activity
            .inner
            .sink
            .send(OutputEvent::Capture(CaptureEvent::Finish { id: self.id }))?;
        Ok(FinishedOutput {
            id: self.id,
            file: None,
        })
    }

    fn discard(&mut self) -> io::Result<()> {
        self.activity
            .inner
            .sink
            .send(OutputEvent::Capture(CaptureEvent::Discard { id: self.id }))
    }

    fn abandon(&mut self, buffer: Vec<u8>, abandon: Abandon) {
        let event = match abandon {
            Abandon::Discard => CaptureEvent::Discard { id: self.id },
            Abandon::Retain if !buffer.is_empty() => CaptureEvent::Write {
                id: self.id,
                data: buffer,
            },
            Abandon::Retain => return,
        };
        let inner = &self.activity.inner;
        let abandonment = Arc::clone(&inner.sink).abandon(OutputEvent::Capture(event));
        let mut state = inner.state.lock().unwrap_or_else(PoisonError::into_inner);
        match abandonment {
            Abandonment::Writing(written) => {
                state.writing.retain(|written| !written.is_finished());
                state.writing.push(written);
            }
            Abandonment::Deferred(event) => state.abandoned.push(event),
        }
    }
}

struct Activity {
    inner: Arc<Emitter>,
}

impl Drop for Activity {
    fn drop(&mut self) {
        self.inner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .active -= 1;
        self.inner.changed.notify_waiters();
    }
}

pub trait CaptureTarget: Send + 'static {
    type Finished: Send + 'static;
    fn append(&mut self, bytes: &[u8]) -> io::Result<()>;
    fn finish(&mut self) -> io::Result<Self::Finished>;
    fn discard(&mut self) -> io::Result<()>;

    fn abandon(&mut self, buffer: Vec<u8>, abandon: Abandon) {
        match abandon {
            Abandon::Discard => {
                let _ = self.discard();
            }
            Abandon::Retain if !buffer.is_empty() => {
                let _ = self.append(&buffer);
            }
            Abandon::Retain => {}
        }
    }
}

pub struct PendingProducer<T: CaptureTarget> {
    writer: Producer<T>,
}

impl<T: CaptureTarget> PendingProducer<T> {
    pub fn new(target: T, abandon: Abandon) -> Self {
        Self {
            writer: Producer::new(target, abandon),
        }
    }

    pub fn open(self) -> Producer<T> {
        self.writer
    }

    pub fn open_async(self) -> AsyncProducer<T> {
        AsyncProducer {
            writer: Arc::new(Mutex::new(self.writer)),
            pending: None,
        }
    }
}

enum ProducerState {
    Writing,
    Failed(io::ErrorKind),
    Closed,
}

/// Buffered append-only output with consuming finalization.
pub struct Producer<T: CaptureTarget> {
    target: T,
    abandon: Abandon,
    position: u64,
    buffer: Vec<u8>,
    state: ProducerState,
}

impl<T: CaptureTarget> Producer<T> {
    fn new(target: T, abandon: Abandon) -> Self {
        Self {
            target,
            abandon,
            position: 0,
            buffer: Vec::new(),
            state: ProducerState::Writing,
        }
    }

    fn healthy(&self) -> io::Result<()> {
        match self.state {
            ProducerState::Writing => Ok(()),
            ProducerState::Failed(kind) => Err(io::Error::new(
                kind,
                "capture cannot finish after an IO error",
            )),
            ProducerState::Closed => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "capture is finished",
            )),
        }
    }

    fn track<R>(&mut self, result: io::Result<R>) -> io::Result<R> {
        if let Err(error) = &result {
            self.state = ProducerState::Failed(error.kind());
        }
        result
    }

    pub fn write_text(&mut self, text: &str) -> io::Result<()> {
        self.write_all(text.as_bytes())?;
        self.flush()
    }

    fn close(&mut self) -> io::Result<T::Finished> {
        self.flush()?;
        let result = self.target.finish();
        let finished = self.track(result)?;
        self.state = ProducerState::Closed;
        Ok(finished)
    }

    pub fn finish(mut self) -> io::Result<T::Finished> {
        self.close()
    }

    pub fn close_nonempty(&mut self) -> io::Result<Option<T::Finished>> {
        self.flush()?;
        if self.position == 0 {
            self.close_discard()?;
            Ok(None)
        } else {
            self.close().map(Some)
        }
    }

    fn close_discard(&mut self) -> io::Result<()> {
        let result = self.target.discard();
        self.buffer.clear();
        self.state = ProducerState::Closed;
        result
    }
}

impl<T: CaptureTarget> Drop for Producer<T> {
    fn drop(&mut self) {
        if matches!(self.state, ProducerState::Closed) {
            return;
        }
        let buffer = if matches!(self.state, ProducerState::Writing) {
            std::mem::take(&mut self.buffer)
        } else {
            Vec::new()
        };
        self.target.abandon(buffer, self.abandon);
    }
}

impl<T: CaptureTarget> Write for Producer<T> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.healthy()?;
        let count = bytes.len().min(CHUNK_BYTES - self.buffer.len());
        self.buffer.extend_from_slice(&bytes[..count]);
        if self.buffer.len() == CHUNK_BYTES {
            self.flush()?;
        }
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.healthy()?;
        if self.buffer.is_empty() {
            return Ok(());
        }
        let result = self.target.append(&self.buffer);
        self.track(result)?;
        self.position += self.buffer.len() as u64;
        self.buffer.clear();
        Ok(())
    }
}

/// Accepted writes complete in order even when their awaiting future is dropped.
pub struct AsyncProducer<T: CaptureTarget> {
    writer: Arc<Mutex<Producer<T>>>,
    pending: Option<tokio::task::JoinHandle<()>>,
}

impl<T: CaptureTarget> AsyncProducer<T> {
    async fn run<R: Send + 'static>(
        &mut self,
        operation: impl FnOnce(&mut Producer<T>) -> io::Result<R> + Send + 'static,
    ) -> io::Result<R> {
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

    pub async fn write_text(&mut self, text: &str) -> io::Result<()> {
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

    pub async fn finish_nonempty(mut self) -> io::Result<Option<T::Finished>> {
        self.run(Producer::close_nonempty).await
    }

    /// Publish the capture, even when nothing was written.
    pub async fn finish(mut self) -> io::Result<T::Finished> {
        self.run(Producer::close).await
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::tests::bounded;
    use std::{
        future::poll_fn,
        sync::atomic::{AtomicUsize, Ordering},
        task::Poll,
    };

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum CaptureStage {
        Open,
        Write,
        Finish,
    }

    impl CaptureStage {
        fn matches(self, event: &OutputEvent) -> bool {
            matches!(
                (self, event),
                (Self::Open, OutputEvent::Capture(CaptureEvent::Open { .. }))
                    | (
                        Self::Write,
                        OutputEvent::Capture(CaptureEvent::Write { .. })
                    )
                    | (
                        Self::Finish,
                        OutputEvent::Capture(CaptureEvent::Finish { .. })
                    )
            )
        }
    }

    /// Forward events until the selected stage has occurred `skip` times, then fail
    /// that stage. An error factory preserves native OS codes or custom opaque detail
    /// so each consumer can assert its own handling without duplicating a sink.
    pub(crate) struct FailingCapture {
        sink: Arc<dyn OutputSink>,
        stage: CaptureStage,
        skip: usize,
        matches: AtomicUsize,
        error: Box<dyn Fn() -> io::Error + Send + Sync>,
    }

    impl FailingCapture {
        pub(crate) fn new(
            sink: Arc<dyn OutputSink>,
            stage: CaptureStage,
            skip: usize,
            error: impl Fn() -> io::Error + Send + Sync + 'static,
        ) -> Self {
            Self {
                sink,
                stage,
                skip,
                matches: AtomicUsize::new(0),
                error: Box::new(error),
            }
        }
    }

    impl OutputSink for FailingCapture {
        fn send(&self, event: OutputEvent) -> io::Result<()> {
            if self.stage.matches(&event)
                && self.matches.fetch_add(1, Ordering::SeqCst) >= self.skip
            {
                return Err((self.error)());
            }
            self.sink.send(event)
        }
    }

    struct ChannelSink(tokio::sync::mpsc::Sender<OutputEvent>);

    impl OutputSink for ChannelSink {
        fn send(&self, event: OutputEvent) -> io::Result<()> {
            self.0.blocking_send(event).map_err(io::Error::other)
        }
    }

    async fn poll_once(future: impl Future) {
        tokio::pin!(future);
        poll_fn(|context| {
            let _ = future.as_mut().poll(context);
            Poll::Ready(())
        })
        .await;
    }

    #[tokio::test]
    async fn settlement_orders_cancelled_writes_and_runtime_thread_abandonment() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let context = OutputContext::new(Arc::new(ChannelSink(sender.clone())));
        for interrupted in [false, true] {
            let pending = context
                .text_capture(TextCaptureField::Stdout)
                .await
                .unwrap();
            let Some(OutputEvent::Capture(CaptureEvent::Open { id, .. })) =
                bounded(receiver.recv()).await
            else {
                panic!("missing open")
            };
            let bytes = vec![b'x'; if interrupted { CHUNK_BYTES * 3 + 7 } else { 7 }];
            if interrupted {
                let mut writer = pending.open_async();
                poll_once(writer.write_all(&bytes)).await;
                bounded(async {
                    while sender.capacity() != 0 {
                        tokio::task::yield_now().await;
                    }
                })
                .await;
                drop(writer);
            } else {
                let bytes = bytes.clone();
                let writer = tokio::task::spawn_blocking(move || {
                    let mut writer = pending.open();
                    writer.write_all(&bytes).unwrap();
                    writer.flush().unwrap();
                    writer
                })
                .await
                .unwrap();
                // No background owner remains. Drop runs on the runtime while the sink is full.
                drop(writer);
            }
            let context = context.clone();
            let settling = tokio::spawn(async move { context.settle().await });
            let mut received = Vec::new();
            loop {
                match bounded(receiver.recv()).await.unwrap() {
                    OutputEvent::Capture(CaptureEvent::Write { id: written, data }) => {
                        assert_eq!(written, id);
                        assert!(data.len() <= CHUNK_BYTES);
                        received.extend(data);
                    }
                    OutputEvent::Capture(CaptureEvent::Discard { id: discarded }) => {
                        assert_eq!(discarded, id);
                        break;
                    }
                    event => panic!("unexpected event: {event:?}"),
                }
            }
            bounded(settling).await.unwrap().unwrap();
            assert_eq!(received, bytes);
            assert!(receiver.try_recv().is_err());
        }

        poll_once(context.text_capture(TextCaptureField::Content)).await;
        let image = crate::tests::png(b"source").await;
        poll_once(context.store_image(None, &image)).await;
        let settling = tokio::spawn(async move { context.settle().await });
        let mut opened = None;
        let mut discarded = None;
        let mut imported = None;
        for _ in 0..3 {
            match bounded(receiver.recv()).await.unwrap() {
                OutputEvent::Capture(CaptureEvent::Open { id, .. }) => opened = Some(id),
                OutputEvent::Capture(CaptureEvent::Discard { id }) => discarded = Some(id),
                OutputEvent::Image { image, .. } => imported = Some(image),
                event => panic!("unexpected event: {event:?}"),
            }
        }
        bounded(settling).await.unwrap().unwrap();
        assert!(opened.is_some());
        assert_eq!(opened, discarded);
        assert_eq!(imported, Some(image));
    }

    /// The latest of a stream reaches a sink that makes its sender wait, after the
    /// writes accepted before it.
    #[tokio::test]
    async fn the_latest_of_a_stream_follows_its_writes_into_a_waiting_sink() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let context = OutputContext::new(Arc::new(ChannelSink(sender)));
        let extent = skyhook_worker::op::Extent::default().after(b"first and latest");
        let told = async {
            let pending = context.text_capture(TextCaptureField::Stdout).await?;
            let mut writer = pending.open_async();
            writer.write_text("first").await?;
            writer.latest(extent, "latest".into()).await
        };
        let taken = async {
            let mut events = Vec::new();
            for _ in 0..3 {
                events.push(receiver.recv().await.unwrap());
            }
            events
        };
        let (told, events) = bounded(async { tokio::join!(told, taken) }).await;
        told.unwrap();
        use CaptureEvent::{Latest, Open, Write};
        let [
            OutputEvent::Capture(Open { .. }),
            OutputEvent::Capture(Write { data, .. }),
            OutputEvent::Capture(Latest { extent: told, .. }),
        ] = &events[..]
        else {
            panic!("an open, a write and the latest: {events:?}")
        };
        assert_eq!((&data[..], *told), (&b"first"[..], extent));
    }
}
