//! The operations a target performs for tools: each reads or changes target data in
//! one call, with its guards inside, and answers with typed facts, never text.

mod error;
mod exec;
mod fetch;
mod files;
mod path;
mod relay;
mod search;

pub use error::{FsError, IoKind, OsError};
pub use exec::{
    Exec, ExecEnd, ExecEvent, ExecStatus, ExitStatus, Extent, Pipe, Program, SpilledStream,
    StreamRest, StreamSpill,
};
pub use fetch::{
    ClientIdentity, ContentType, DownloadStep, Fetch, FetchBody, FetchEvent, FetchOutput,
    FetchStep, Fetched, Headers, HttpMethod, HttpRequestUrl, Redirect, RedirectPolicy, UrlError,
};
pub use files::{
    Content, CreateDirs, Directories, Entry, EntryKind, Read, ReadEnd, ReadEvent, ReadFrom,
    ReadMode, Remove, RemoveKind, Replace, SHOWN_LINES, Write, Written,
};
pub use path::{
    PathKind, Protected, Resolution, Resolve, Resolved, ResolvedPath, TargetPath, Workspace,
};
pub use relay::{Open, StreamOutput};
pub(crate) use search::MATCH_CONTEXT_BYTES;
pub use search::{
    Case, Excerpt, Glob, MatchLine, REGEX_SIZE_LIMIT, Search, SearchEvent, SkipReason, Skipped,
    excerpt, line_matcher,
};

use std::{future::Future, io, pin::Pin};

use serde::{Serialize, de::DeserializeOwned};
use tokio::io::AsyncRead;

/// The most bytes one input chunk of a stream carries, each sent as one frame; an
/// event carries about as many, and so do the events one frame gathers.
pub const CHUNK_BYTES: usize = 32 * 1024;

/// What a target answers: the operation's own result, inside whether the
/// target could be reached and answer at all.
pub type Answer<T, E, U> = Result<Result<T, E>, U>;

/// What an operation that emits no events, or cannot fail, has none of.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub enum Never {}

/// One tool's data-plane work on a target: a request, the events it streams back,
/// and the facts it ends with.
pub trait Operation: Serialize + DeserializeOwned + Send + Sized + 'static {
    type Event: Serialize + DeserializeOwned + Send + 'static;
    type End: Serialize + DeserializeOwned + Send + 'static;
    type Error: Serialize + DeserializeOwned + Send + 'static;

    /// Bytes the host streams to the target while the operation runs, if it takes any.
    fn input(&mut self) -> Option<&mut Input> {
        None
    }

    /// The paths the operation acts on, in the order its tool's path arguments name
    /// them, for a contained call to replace with their resolutions.
    fn paths(&mut self) -> Vec<&mut TargetPath> {
        Vec::new()
    }

    /// Perform the operation on this machine.
    fn perform(
        self,
        local: &crate::local::Local,
        events: Events<Self::Event>,
    ) -> impl Future<Output = Result<Self::End, Self::Error>> + Send;
}

/// Where an operation sends the events it streams. A closed receiver means
/// nobody waits for them any more; the operation is about to be dropped.
pub struct Events<E>(tokio::sync::mpsc::Sender<E>);

impl<E: Send> Events<E> {
    pub(crate) fn new(sender: tokio::sync::mpsc::Sender<E>) -> Self {
        Self(sender)
    }

    pub async fn send(&self, event: E) -> io::Result<()> {
        (self.0.send(event).await).map_err(|_| io::ErrorKind::BrokenPipe.into())
    }

    /// Send `event` unless those before it still wait to be taken: whether it was
    /// sent. Nothing waits for its reader.
    pub(crate) fn offer(&self, event: E) -> bool {
        self.0.try_send(event).is_ok()
    }

    /// Whether nobody waits for events any more, so the operation should stop.
    pub fn is_closed(&self) -> bool {
        self.0.is_closed()
    }

    /// [`send`](Self::send) from a blocking thread.
    pub(crate) fn send_blocking(&self, event: E) -> io::Result<()> {
        (self.0.blocking_send(event)).map_err(|_| io::ErrorKind::BrokenPipe.into())
    }
}

impl<E> Clone for Events<E> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

/// Bytes streamed from the host into a running operation. Decoded from the wire it
/// is empty until the connection attaches the stream its frames carry.
pub struct Input(std::sync::Mutex<Pin<Box<dyn AsyncRead + Send>>>);

impl Input {
    pub fn new(reader: impl AsyncRead + Send + 'static) -> Self {
        Self(std::sync::Mutex::new(Box::pin(reader)))
    }

    pub fn into_reader(self) -> Pin<Box<dyn AsyncRead + Send>> {
        (self.0.into_inner()).unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Default for Input {
    fn default() -> Self {
        Self::new(tokio::io::empty())
    }
}

impl std::fmt::Debug for Input {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Input")
    }
}

/// The bytes of an [`Input`] never travel inside the request that carries it.
pub(crate) mod input {
    pub fn serialize<S: serde::Serializer>(_: &super::Input, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_unit()
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<super::Input, D::Error> {
        <() as serde::Deserialize>::deserialize(d)?;
        Ok(super::Input::default())
    }
}

/// How a running operation ends, once it does.
type Ending<O, U> =
    Pin<Box<dyn Future<Output = Answer<<O as Operation>::End, <O as Operation>::Error, U>> + Send>>;

/// A running operation: its events in order, then its end. Dropping it before
/// its end cancels the operation.
pub struct Running<O: Operation, U> {
    events: tokio::sync::mpsc::Receiver<O::Event>,
    end: Ending<O, U>,
    _cancel: Box<dyn Send + Sync>,
}

impl<O: Operation, U> Running<O, U> {
    pub fn new(
        events: tokio::sync::mpsc::Receiver<O::Event>,
        end: impl Future<Output = Answer<O::End, O::Error, U>> + Send + 'static,
        cancel: impl Send + Sync + 'static,
    ) -> Self {
        Self {
            events,
            end: Box::pin(end),
            _cancel: Box::new(cancel),
        }
    }

    /// The next event, or none once the operation emits no more.
    pub async fn event(&mut self) -> Option<O::Event> {
        self.events.recv().await
    }

    /// The events waiting, at least one; none once the operation emits no more.
    pub async fn events(&mut self) -> Vec<O::Event> {
        let mut events = Vec::new();
        self.events.recv_many(&mut events, usize::MAX).await;
        events
    }

    /// How the operation ended, once its events are taken.
    pub async fn end(mut self) -> Answer<O::End, O::Error, U> {
        while self.events.recv().await.is_some() {}
        self.end.await
    }

    /// This operation as a target whose unreachable failures are `V`'s.
    pub fn widen<V: From<U> + 'static>(self) -> Running<O, V>
    where
        U: 'static,
    {
        let end = self.end;
        Running {
            events: self.events,
            end: Box::pin(async move { end.await.map_err(V::from) }),
            _cancel: self._cancel,
        }
    }
}
