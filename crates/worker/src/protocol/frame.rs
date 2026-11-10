//! Frames on the wire: each a big-endian `u32` length, then its MessagePack bytes,
//! fields in order. Same-source builds need no field names. A frame of a call's
//! stream too large for one is sent as pieces of its bytes on that stream, other
//! streams' frames going between them. A frame may carry a credential, so its bytes
//! are held only where they are zeroed once written or decoded, in buffers that
//! never grow and so leave no copy behind.
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex, MutexGuard, PoisonError, Weak},
};

use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _},
    sync::{mpsc, oneshot},
};
use zeroize::{Zeroize as _, Zeroizing};

use super::{CallId, Frame};

/// The most bytes a frame of a call's stream takes before it is sent in pieces.
const PART_BYTES: usize = 64 * 1024;
/// The most bytes a frame read takes: a piece and what wraps it, with room to spare.
const MAX_FRAME_BYTES: usize = 2 * PART_BYTES;
/// How many bytes of waiting frames a writer gathers into one write.
const WRITE_BYTES: usize = 64 * 1024;

fn too_large() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, "RPC frame is too large")
}

fn invalid(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, error)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What one frame on the wire carries: a frame whole, or a piece of the bytes of a
/// call's frame too large for one, `rest` more of them to follow on its stream.
#[derive(Deserialize, Serialize)]
enum Sent<'a, F> {
    Whole(F),
    Piece {
        call: CallId,
        #[serde(with = "serde_bytes")]
        bytes: &'a [u8],
        rest: usize,
    },
}

/// Append `value`'s bytes to `buffer`.
fn encode(buffer: &mut Vec<u8>, value: &impl Serialize) -> std::io::Result<()> {
    rmp_serde::encode::write(buffer, value)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))
}

/// Append `sent` to `buffer` as one frame on the wire.
fn encode_into<F: Serialize>(buffer: &mut Vec<u8>, sent: &Sent<F>) -> std::io::Result<()> {
    let start = buffer.len();
    buffer.extend_from_slice(&[0; 4]);
    encode(buffer, sent)?;
    let length = u32::try_from(buffer.len() - start - 4).map_err(|_| too_large())?;
    buffer[start..start + 4].copy_from_slice(&length.to_be_bytes());
    Ok(())
}

/// Append `frame` to `buffer`, or when it is too large for one frame its first
/// piece, what is left of it returned to follow on its stream's later turns.
fn encode_frame<F: Frame>(buffer: &mut Vec<u8>, frame: F) -> std::io::Result<Option<Pieces>> {
    let length = encoded_bytes(&frame);
    if length <= PART_BYTES {
        return encode_into(buffer, &Sent::Whole(frame)).map(|()| None);
    }
    let call = frame.call().ok_or_else(too_large)?;
    let mut bytes = Zeroizing::new(Vec::with_capacity(length));
    encode(&mut bytes, &frame)?;
    let sent = 0;
    Pieces { call, bytes, sent }.encode_next(buffer)
}

/// The bytes of a frame being sent in pieces, held once, and how many are sent.
struct Pieces {
    call: CallId,
    bytes: Zeroizing<Vec<u8>>,
    sent: usize,
}

impl Pieces {
    /// Append the next piece to `buffer`; what is left to follow it, if any is.
    fn encode_next(mut self, buffer: &mut Vec<u8>) -> std::io::Result<Option<Self>> {
        let left = &self.bytes[self.sent..];
        let bytes = &left[..left.len().min(PART_BYTES)];
        let (call, rest) = (self.call, left.len() - bytes.len());
        encode_into(buffer, &Sent::<()>::Piece { call, bytes, rest })?;
        self.sent += bytes.len();
        Ok((rest > 0).then_some(self))
    }
}

/// Write `value` as one frame, in one write.
pub async fn write_frame<W, T>(writer: &mut W, value: &T) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let whole = Sent::Whole(value);
    let mut frame = Zeroizing::new(Vec::with_capacity(4 + encoded_bytes(&whole)));
    encode_into(&mut frame, &whole)?;
    writer.write_all(&frame).await?;
    writer.flush().await
}

/// Where one end of a connection sends its frames, for its [`FrameWriter`] alone to
/// write. Sending never waits, so dropping a sender can never cut a frame.
pub struct Outbox<F> {
    waiting: Weak<Mutex<Waiting<F>>>,
    wake: mpsc::Sender<()>,
}

/// A frame waiting for the writer, and what it drops once it has written the frame.
type Queued<F> = (F, Option<oneshot::Sender<()>>);

impl<F> Clone for Outbox<F> {
    fn clone(&self) -> Self {
        Self {
            waiting: self.waiting.clone(),
            wake: self.wake.clone(),
        }
    }
}

impl<F: Frame> Outbox<F> {
    /// A connection end's outbox and the writer of what it sends.
    pub fn new() -> (Self, FrameWriter<F>) {
        let waiting = Arc::new(Mutex::new(Waiting {
            acknowledgements: VecDeque::new(),
            streams: VecDeque::new(),
        }));
        let (wake, woken) = mpsc::channel(1);
        let outbox = Self {
            waiting: Arc::downgrade(&waiting),
            wake,
        };
        (outbox, FrameWriter { waiting, woken })
    }

    /// Change what waits for the writer, and wake it. Once the writer has stopped,
    /// the connection is failing and nothing waits.
    fn queue<T>(&self, change: impl FnOnce(&mut Waiting<F>) -> T) -> Option<T> {
        let waiting = self.waiting.upgrade()?;
        let changed = change(&mut lock(&waiting));
        let _ = self.wake.try_send(());
        Some(changed)
    }

    /// Send `frame` after every frame sent before it on its stream. Once the writer
    /// has stopped, the frame is dropped.
    pub fn send(&self, frame: F) {
        self.queue(|waiting| waiting.push((frame, None)));
    }

    /// [`send`](Self::send) a frame of a stream's data; what this returns waits until
    /// the writer has written it, or it was taken back. What the stream produces
    /// meanwhile gathers into its next frame, rather than each piece of it taking a
    /// frame and a credit of its own.
    pub fn send_data(&self, frame: F) -> impl Future<Output = ()> + use<F> {
        let (taken, written) = oneshot::channel::<()>();
        self.queue(|waiting| waiting.push((frame, Some(taken))));
        async move {
            let _ = written.await;
        }
    }

    /// Send credit for a call's stream ahead of the frames waiting: returning
    /// credit never waits behind another call's data.
    pub fn acknowledge(&self, frame: F) {
        self.queue(|waiting| waiting.acknowledgements.push_back(frame));
    }

    /// Take back the frames of `call`'s stream that still wait whole for the writer,
    /// its acknowledgements dropped with them. A frame the writer has begun is
    /// written to its end.
    pub fn withdraw(&self, call: CallId) -> Vec<F> {
        (self.queue(|waiting| waiting.withdraw(call))).unwrap_or_default()
    }
}

/// The frames waiting to be written: acknowledgements, then the streams with any, in
/// the order they take turns. Each turn writes one frame of a stream, or one piece
/// of a frame it sends in pieces, so a large frame holds up no other stream's.
struct Waiting<F> {
    acknowledgements: VecDeque<F>,
    streams: VecDeque<Stream<F>>,
}

/// A stream's waiting frames: what is left of the one it is sending in pieces, then
/// those sent after it.
struct Stream<F> {
    call: Option<CallId>,
    pieces: Option<Pieces>,
    frames: VecDeque<Queued<F>>,
}

/// What the writer takes to write next: a frame, or the next piece of one.
enum Taken<F> {
    Frame(Queued<F>),
    Pieces(Pieces),
}

impl<F: Frame> Waiting<F> {
    /// `call`'s stream, which takes its turn last when nothing of it was waiting.
    fn stream(&mut self, call: Option<CallId>) -> &mut Stream<F> {
        let waiting = self.streams.iter().position(|stream| stream.call == call);
        let at = waiting.unwrap_or_else(|| {
            let (pieces, frames) = (None, VecDeque::new());
            self.streams.push_back(Stream {
                call,
                pieces,
                frames,
            });
            self.streams.len() - 1
        });
        &mut self.streams[at]
    }

    fn push(&mut self, queued: Queued<F>) {
        self.stream(queued.0.call()).frames.push_back(queued);
    }

    /// The next to write: an acknowledgement, else what the next stream writes in
    /// its turn, which passes on. What is left of a frame taken to send in pieces is
    /// [`kept`](Self::keep) before the next is taken.
    fn take(&mut self) -> Option<Taken<F>> {
        if let Some(acknowledgement) = self.acknowledgements.pop_front() {
            return Some(Taken::Frame((acknowledgement, None)));
        }
        let mut stream = self.streams.pop_front()?;
        let taken = match stream.pieces.take() {
            Some(pieces) => Taken::Pieces(pieces),
            None => Taken::Frame(stream.frames.pop_front()?),
        };
        if !stream.frames.is_empty() {
            self.streams.push_back(stream);
        }
        Some(taken)
    }

    /// Keep what is left of the frame a stream is sending in pieces, ahead of the
    /// frames sent after it.
    fn keep(&mut self, pieces: Pieces) {
        let stream = self.stream(Some(pieces.call));
        stream.pieces = Some(pieces);
    }

    fn withdraw(&mut self, call: CallId) -> Vec<F> {
        (self.acknowledgements).retain(|frame| frame.call() != Some(call));
        let waiting = |stream: &Stream<F>| stream.call == Some(call);
        let Some(at) = self.streams.iter().position(waiting) else {
            return Vec::new();
        };
        let frames = std::mem::take(&mut self.streams[at].frames);
        if self.streams[at].pieces.is_none() {
            self.streams.remove(at);
        }
        frames.into_iter().map(|(frame, _)| frame).collect()
    }
}

/// The one writer of a connection end's frames, and the owner of those waiting to
/// be written: they go with it when it stops.
pub struct FrameWriter<F> {
    waiting: Arc<Mutex<Waiting<F>>>,
    woken: mpsc::Receiver<()>,
}

impl<F: Frame> FrameWriter<F> {
    /// Write what the outbox sends to `output`, acknowledgements first, then each
    /// stream's frames in turn, gathering the frames already waiting into each write,
    /// until every outbox is dropped and its frames are written.
    async fn write(mut self, mut output: impl AsyncWrite + Unpin) -> std::io::Result<()> {
        // Never grown: it is written once it holds a write's bytes, and no frame takes
        // more than a read does.
        let mut buffer = Zeroizing::new(Vec::with_capacity(WRITE_BYTES + 4 + MAX_FRAME_BYTES));
        let mut written = Vec::new();
        loop {
            let taken = lock(&self.waiting).take();
            let idle = taken.is_none();
            let left = match taken {
                Some(Taken::Frame((frame, sender))) => {
                    written.extend(sender);
                    encode_frame(&mut buffer, frame)?
                }
                Some(Taken::Pieces(pieces)) => pieces.encode_next(&mut buffer)?,
                None => None,
            };
            if let Some(pieces) = left {
                lock(&self.waiting).keep(pieces);
            }
            if idle && buffer.is_empty() {
                if self.woken.recv().await.is_none() {
                    return Ok(());
                }
            } else if idle || buffer.len() >= WRITE_BYTES {
                write(&mut output, &mut buffer).await?;
                written.clear();
            }
        }
    }
}

impl<F: Frame + Send + 'static> FrameWriter<F> {
    /// [`write`](Self::write) on a task of its own, so that encoding and writing frames
    /// never waits on the work of the connection end it writes for; dropping what this
    /// returns stops it.
    pub fn spawn(
        self,
        output: impl AsyncWrite + Unpin + Send + 'static,
    ) -> impl Future<Output = std::io::Result<()>> + Send {
        let task = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(self.write(output)));
        async move { task.await.map_err(std::io::Error::other)? }
    }
}

/// Write `buffer` whole, then zero and empty it.
async fn write(
    output: &mut (impl AsyncWrite + Unpin),
    buffer: &mut Vec<u8>,
) -> std::io::Result<()> {
    output.write_all(buffer).await?;
    output.flush().await?;
    buffer.as_mut_slice().zeroize();
    buffer.clear();
    Ok(())
}

/// How many bytes `value` takes in a frame.
pub(crate) fn encoded_bytes(value: &impl Serialize) -> usize {
    let mut counted = skyhook_base::limits::ByteCount {
        bytes: 0,
        limit: usize::MAX,
    };
    let _ = rmp_serde::encode::write(&mut counted, value);
    counted.bytes
}

/// The frames a stream carries, read in large reads, those sent in pieces put back
/// together.
pub struct FrameReader<R> {
    reader: R,
    buffer: Zeroizing<Vec<u8>>,
    /// The bytes read and not yet decoded; every other byte of the buffer is zero.
    start: usize,
    end: usize,
    /// The pieces so far of each call's frame sent in pieces.
    parts: HashMap<CallId, Zeroizing<Vec<u8>>>,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            buffer: Zeroizing::new(vec![0; 4 + MAX_FRAME_BYTES]),
            start: 0,
            end: 0,
            parts: HashMap::new(),
        }
    }

    /// The next frame, or none once the stream ends between frames. Dropping the
    /// future loses nothing: what it read stays for the next call.
    pub async fn next<F: Frame>(&mut self) -> std::io::Result<Option<F>> {
        loop {
            if let Some(frame) = self.decoded()? {
                return Ok(Some(frame));
            }
            if !self.fill().await? {
                return match self.start == self.end {
                    true => Ok(None),
                    false => Err(std::io::ErrorKind::UnexpectedEof.into()),
                };
            }
        }
    }

    /// Read more of the stream into the buffer; false at its end.
    async fn fill(&mut self) -> std::io::Result<bool> {
        if self.end == self.buffer.len() {
            let kept = self.end - self.start;
            self.buffer.copy_within(self.start..self.end, 0);
            self.buffer[kept..self.end].zeroize();
            (self.end, self.start) = (kept, 0);
        }
        let read = self.reader.read(&mut self.buffer[self.end..]).await?;
        self.end += read;
        Ok(read > 0)
    }

    /// The next frame the bytes read hold, if they hold one whole or the last piece
    /// of one, the pieces before it gathered.
    fn decoded<F: Frame>(&mut self) -> std::io::Result<Option<F>> {
        loop {
            let read = &self.buffer[self.start..self.end];
            let Some(length) = read.first_chunk::<4>() else {
                return Ok(None);
            };
            let length = usize::try_from(u32::from_be_bytes(*length)).map_err(|_| too_large())?;
            if length > MAX_FRAME_BYTES {
                return Err(too_large());
            }
            let whole = 4 + length;
            if read.len() < whole {
                return Ok(None);
            }
            let sent = &mut self.buffer[self.start..self.start + whole];
            self.start += whole;
            let frame = match rmp_serde::from_slice(&sent[4..]) {
                Ok(Sent::Whole(frame)) => Ok(Some(frame)),
                Ok(Sent::Piece { call, bytes, rest }) => gather(&mut self.parts, call, bytes, rest),
                Err(error) => Err(invalid(error)),
            };
            sent.zeroize();
            if let Some(frame) = frame? {
                return Ok(Some(frame));
            }
        }
    }
}

/// Add `bytes` to the pieces of `call`'s frame, `rest` more to follow: the frame,
/// once none do. Its first piece reserves room for it all.
fn gather<F: Frame>(
    parts: &mut HashMap<CallId, Zeroizing<Vec<u8>>>,
    call: CallId,
    bytes: &[u8],
    rest: usize,
) -> std::io::Result<Option<F>> {
    let mut whole = parts.remove(&call).unwrap_or_default();
    if whole.is_empty() {
        let length = bytes.len().saturating_add(rest);
        whole.try_reserve_exact(length).map_err(|_| too_large())?;
    }
    whole.extend_from_slice(bytes);
    if rest > 0 {
        parts.insert(call, whole);
        return Ok(None);
    }
    rmp_serde::from_slice(&whole).map(Some).map_err(invalid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::op::{Read, ReadEnd, ReadEvent};
    use crate::protocol::tests::call_id;
    use crate::protocol::{Wire as _, WorkerFrame};
    use crate::tests::bounded;

    const LARGE: usize = PART_BYTES + 5;

    /// What a worker sends for read `id`: `bytes` of its file, or its end naming the
    /// file in `bytes` characters, or credit for its input.
    fn events(id: u64, bytes: usize) -> WorkerFrame {
        let events = Read::event_frame(vec![ReadEvent::Bytes(vec![7; bytes])]);
        let id = call_id(id);
        WorkerFrame::Events { id, events }
    }

    fn done(id: u64, bytes: usize) -> WorkerFrame {
        let shown = "x".repeat(bytes);
        let outcome = Read::outcome(Ok(ReadEnd::File { shown }));
        let id = call_id(id);
        WorkerFrame::Done { id, outcome }
    }

    fn acknowledgement(id: u64) -> WorkerFrame {
        let id = call_id(id);
        WorkerFrame::InputAck { id }
    }

    /// Frames arrive whole and in order however the stream splits them, several in
    /// one read or one across many, events and ends too large for one frame in
    /// pieces put back together, and an acknowledgement sent after them first, the
    /// reader's copy of each zeroed once it is decoded; a stream ending inside a frame
    /// fails rather than ending.
    #[tokio::test]
    async fn frames_arrive_whole_however_they_are_split() {
        let sent = || [events(1, 1), done(1, 1), events(2, LARGE), done(2, LARGE)];
        let (outbox, writer) = Outbox::new();
        sent().into_iter().for_each(|frame| outbox.send(frame));
        outbox.acknowledge(acknowledgement(3));
        drop(outbox);
        let mut stream = Vec::new();
        bounded(writer.write(&mut stream)).await.unwrap();
        let expected = [acknowledgement(3)].into_iter().chain(sent());
        let expected: Vec<_> = expected.map(|frame| format!("{frame:?}")).collect();
        for chunk in [3, stream.len()] {
            let (mut writer, reader) = tokio::io::duplex(chunk);
            let writing = stream.clone();
            let written = tokio::spawn(async move {
                for piece in writing.chunks(chunk) {
                    writer.write_all(piece).await.unwrap();
                }
            });
            let mut frames = FrameReader::new(reader);
            let mut received = Vec::new();
            while let Some(frame) = bounded(frames.next::<WorkerFrame>()).await.unwrap() {
                received.push(format!("{frame:?}"));
            }
            assert_eq!(received, expected);
            assert!(frames.buffer.iter().all(|byte| *byte == 0));
            bounded(written).await.unwrap();
        }
        let mut frames = FrameReader::new(&stream[..stream.len() - 1]);
        let mut last = frames.next::<WorkerFrame>().await;
        while let Ok(Some(_)) = last {
            last = frames.next().await;
        }
        assert_eq!(last.unwrap_err().kind(), std::io::ErrorKind::UnexpectedEof);
    }

    /// A frame sent in parts holds up no other call's: the frames another call sends
    /// once its first piece is written go between its pieces, arriving before it does.
    /// Withdrawing its stream then takes back what waits behind it, and it arrives whole.
    #[tokio::test]
    async fn a_frame_in_parts_holds_up_no_other_call() {
        let large = || events(1, 16 * PART_BYTES);
        let small = || (1..=3).map(|bytes| events(2, bytes)).chain([done(2, 1)]);
        let (outbox, writer) = Outbox::new();
        // The writer waits on its output once it holds a piece or two.
        let (output, input) = tokio::io::duplex(2 * PART_BYTES);
        let writing = tokio::spawn(writer.write(output));
        bounded(outbox.send_data(large())).await;
        small().for_each(|frame| outbox.send(frame));
        outbox.send(done(1, 1));
        outbox.acknowledge(acknowledgement(1));
        assert_eq!(outbox.withdraw(call_id(1)).len(), 1);
        drop(outbox);
        let mut frames = FrameReader::new(input);
        let mut received = Vec::new();
        while let Some(frame) = bounded(frames.next::<WorkerFrame>()).await.unwrap() {
            received.push(frame);
        }
        let calls = received.iter().map(|frame| frame.call().unwrap());
        assert_eq!(calls.collect::<Vec<_>>(), [2, 2, 2, 2, 1].map(call_id));
        let mut whole = received.iter().zip(small().chain([large()]));
        assert!(whole.all(|(frame, sent)| format!("{frame:?}") == format!("{sent:?}")));
        bounded(writing).await.unwrap().unwrap();
    }
}
