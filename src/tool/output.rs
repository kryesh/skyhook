//! Output producers shared by local execution and the shim.
use std::{
    io::{self, Seek, Write},
    num::NonZeroU64,
    sync::{Arc, Mutex, PoisonError},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    media::{BlobRef, Image, ImageRef, MAX_IMAGE_BYTES},
    tool::StreamEnd,
};

#[derive(
    Clone, Copy, Debug, Default, Deserialize, Serialize, schemars::JsonSchema, PartialEq, Eq,
)]
#[serde(rename_all = "snake_case")]
pub enum CaptureKind {
    Text,
    Json,
    /// Transports may stream a capture before its final JSON type is known.
    #[default]
    Unknown,
}

pub(crate) const OUTPUT_CHUNK_BYTES: usize = 32 * 1024;

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Abandon {
    Retain,
    Discard,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
#[serde(transparent)]
pub(crate) struct CaptureId(NonZeroU64);

impl CaptureId {
    pub(crate) const FIRST: Self = Self(NonZeroU64::MIN);

    pub(crate) const fn new(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    pub(crate) const fn get(self) -> u64 {
        self.0.get()
    }

    pub(crate) fn next(self) -> Option<Self> {
        self.get().checked_add(1).and_then(Self::new)
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) enum CaptureEvent {
    Open {
        id: CaptureId,
        field: String,
        kind: CaptureKind,
    },
    Write {
        id: CaptureId,
        data: Vec<u8>,
    },
    Truncate {
        id: CaptureId,
        length: u64,
    },
    Discard {
        id: CaptureId,
    },
    Finish {
        id: CaptureId,
    },
}

#[derive(Debug)]
pub(crate) enum OutputEvent {
    Capture(CaptureEvent),
    Image { file: Option<String>, image: Image },
}

pub(crate) enum Abandonment {
    Handled,
    Deferred(OutputEvent),
}

/// Sends in order, applying backpressure before accepting the next event.
pub(crate) trait OutputSink: Send + Sync {
    fn send(&self, event: OutputEvent) -> io::Result<()>;

    /// Handle cleanup once, or return ownership for the settlement barrier.
    fn abandon(&self, event: OutputEvent) -> Abandonment {
        Abandonment::Deferred(event)
    }
}

#[derive(Clone)]
pub(crate) struct OutputContext {
    inner: Arc<Emitter>,
}

struct Emitter {
    origin: Arc<()>,
    sink: Arc<dyn OutputSink>,
    state: Mutex<EmitterState>,
    changed: tokio::sync::Notify,
    settling: Arc<tokio::sync::Mutex<()>>,
}

struct EmitterState {
    next: Option<CaptureId>,
    active: usize,
    abandoned: Vec<OutputEvent>,
}

impl OutputContext {
    pub(crate) fn new(sink: Arc<dyn OutputSink>) -> Self {
        Self {
            inner: Arc::new(Emitter {
                origin: Arc::new(()),
                sink,
                state: Mutex::new(EmitterState {
                    next: Some(CaptureId::FIRST),
                    active: 0,
                    abandoned: Vec::new(),
                }),
                changed: tokio::sync::Notify::new(),
                settling: Arc::new(tokio::sync::Mutex::new(())),
            }),
        }
    }

    pub(crate) async fn pending_stream_capture(
        &self,
        field: &str,
        kind: CaptureKind,
    ) -> io::Result<PendingOutput> {
        self.pending_capture(field, kind, Abandon::Retain).await
    }

    pub(crate) async fn text_capture(&self, field: TextCaptureField) -> io::Result<PendingOutput> {
        self.pending_capture(&field.pointer(), CaptureKind::Text, Abandon::Discard)
            .await
    }

    pub(crate) async fn pending_capture(
        &self,
        field: &str,
        kind: CaptureKind,
        abandon: Abandon,
    ) -> io::Result<PendingOutput> {
        validate_field(field)?;
        let inner = self.inner.clone();
        let id = {
            let mut state = inner.state.lock().unwrap_or_else(PoisonError::into_inner);
            let id = state
                .next
                .ok_or_else(|| io::Error::other("capture ID space exhausted"))?;
            state.next = id.next();
            state.active += 1;
            id
        };
        let target = EventCapture {
            activity: Activity { inner },
            id,
        };
        let field = field.to_owned();
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
    pub(crate) async fn settle(&self) -> io::Result<()> {
        let settling = self.inner.settling.clone().lock_owned().await;
        let abandoned = loop {
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
                    break std::mem::take(&mut state.abandoned);
                }
            }
            changed.await;
        };
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

    pub(crate) async fn store_image(
        &self,
        file: Option<String>,
        image: &Image,
    ) -> io::Result<ImageRef> {
        if image.bytes().len() as u64 > MAX_IMAGE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "image exceeds byte limit",
            ));
        }
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

    pub(crate) fn owns(&self, finished: &FinishedOutput) -> bool {
        Arc::ptr_eq(&self.inner.origin, &finished.origin)
    }
}

pub(crate) fn validate_field(field: &str) -> io::Result<()> {
    let mut characters = field.chars();
    let valid_root = field.is_empty() || field.starts_with('/');
    let mut valid_escapes = true;
    while let Some(character) = characters.next() {
        if character == '~' && !matches!(characters.next(), Some('0' | '1')) {
            valid_escapes = false;
            break;
        }
    }
    if !valid_root || !valid_escapes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "capture field must be a JSON Pointer",
        ));
    }
    Ok(())
}

/// Producer completion selects an output; it is not a persistence receipt.
#[derive(Clone, Debug)]
pub(crate) struct FinishedOutput {
    origin: Arc<()>,
    id: CaptureId,
}

impl FinishedOutput {
    pub(crate) fn id(&self) -> CaptureId {
        self.id
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ProducedOutput {
    pub(crate) value: Value,
    pub(crate) images: Vec<ImageRef>,
    pub(crate) captures: Vec<FinishedOutput>,
    pub(crate) streams: StreamEnd,
}

impl ProducedOutput {
    pub(crate) const fn new(value: Value) -> Self {
        Self {
            value,
            images: Vec::new(),
            captures: Vec::new(),
            streams: StreamEnd::Finished,
        }
    }

    pub(crate) fn with_captures(mut self, captures: Vec<FinishedOutput>) -> Self {
        self.captures = captures;
        self
    }

    pub(crate) fn with_images(mut self, images: Vec<ImageRef>) -> Self {
        self.images = images;
        self
    }
}

pub(crate) type PendingOutput = PendingProducer<EventCapture>;
pub(crate) type OutputWriter = Producer<EventCapture>;
pub(crate) type AsyncOutput = AsyncProducer<EventCapture>;

pub(crate) struct EventCapture {
    activity: Activity,
    id: CaptureId,
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

    fn truncate(&mut self, length: u64) -> io::Result<()> {
        self.activity
            .inner
            .sink
            .send(OutputEvent::Capture(CaptureEvent::Truncate {
                id: self.id,
                length,
            }))
    }

    fn finish(&mut self) -> io::Result<Self::Finished> {
        self.activity
            .inner
            .sink
            .send(OutputEvent::Capture(CaptureEvent::Finish { id: self.id }))?;
        Ok(FinishedOutput {
            origin: self.activity.inner.origin.clone(),
            id: self.id,
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
        if let Abandonment::Deferred(event) = self
            .activity
            .inner
            .sink
            .abandon(OutputEvent::Capture(event))
        {
            self.activity
                .inner
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .abandoned
                .push(event);
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

pub(crate) trait CaptureTarget: Send + 'static {
    type Finished: Send + 'static;
    fn append(&mut self, bytes: &[u8]) -> io::Result<()>;
    fn truncate(&mut self, length: u64) -> io::Result<()>;
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

pub(crate) struct PendingProducer<T: CaptureTarget> {
    writer: Producer<T>,
}

impl<T: CaptureTarget> PendingProducer<T> {
    pub(crate) fn new(target: T, abandon: Abandon) -> Self {
        Self {
            writer: Producer::new(target, abandon),
        }
    }

    pub(crate) fn open(self) -> Producer<T> {
        self.writer
    }

    pub(crate) fn open_async(self) -> AsyncProducer<T> {
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

/// Buffered append-only output, with rollback and consuming finalization.
pub(crate) struct Producer<T: CaptureTarget> {
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

    pub(crate) fn write_text(&mut self, text: &str) -> io::Result<()> {
        self.write_all(text.as_bytes())?;
        self.flush()
    }

    pub(crate) fn truncate(&mut self, length: u64) -> io::Result<()> {
        self.flush()?;
        let result = if length > self.position {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "capture cannot be extended by truncation",
            ))
        } else {
            self.target.truncate(length)
        };
        self.track(result)?;
        self.position = length;
        Ok(())
    }

    fn close(&mut self) -> io::Result<T::Finished> {
        self.flush()?;
        let result = self.target.finish();
        let finished = self.track(result)?;
        self.state = ProducerState::Closed;
        Ok(finished)
    }

    pub(crate) fn finish(mut self) -> io::Result<T::Finished> {
        self.close()
    }

    pub(crate) fn finish_nonempty(mut self) -> io::Result<Option<T::Finished>> {
        self.close_nonempty()
    }

    fn close_nonempty(&mut self) -> io::Result<Option<T::Finished>> {
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
        let count = bytes.len().min(OUTPUT_CHUNK_BYTES - self.buffer.len());
        self.buffer.extend_from_slice(&bytes[..count]);
        if self.buffer.len() == OUTPUT_CHUNK_BYTES {
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

impl<T: CaptureTarget> Seek for Producer<T> {
    fn seek(&mut self, position: io::SeekFrom) -> io::Result<u64> {
        self.healthy()?;
        let end = self.position + self.buffer.len() as u64;
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

/// Accepted writes complete in order even when their awaiting future is dropped.
pub(crate) struct AsyncProducer<T: CaptureTarget> {
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

    pub(crate) async fn finish_nonempty(mut self) -> io::Result<Option<T::Finished>> {
        self.run(Producer::close_nonempty).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        future::{Future, poll_fn},
        task::Poll,
    };

    struct ChannelSink(tokio::sync::mpsc::Sender<OutputEvent>);

    impl OutputSink for ChannelSink {
        fn send(&self, event: OutputEvent) -> io::Result<()> {
            self.0.blocking_send(event).map_err(io::Error::other)
        }
    }

    async fn bounded<T>(future: impl Future<Output = T>) -> T {
        tokio::time::timeout(std::time::Duration::from_secs(10), future)
            .await
            .expect("output operation timed out")
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
            let bytes = vec![
                b'x';
                if interrupted {
                    OUTPUT_CHUNK_BYTES * 3 + 7
                } else {
                    7
                }
            ];
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
                        assert!(data.len() <= OUTPUT_CHUNK_BYTES);
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
        let image = Image::new(b"\x89PNG\r\n\x1a\nsource".to_vec()).unwrap();
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
}
