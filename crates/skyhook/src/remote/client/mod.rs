//! The protocol client: operations run on a shim's machine, their events and
//! input flow-controlled per call, and the SSH and WinRM streams it relays as an origin.
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use tokio::{
    io::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _},
    sync::{mpsc, oneshot},
};

use crate::{
    job::CancellationToken,
    remote::error::{RemoteError, RemoteFault},
};
use skyhook_tools::diagnostic::{FailureSite, PartialContext, RequestEffects};
use skyhook_tools::target::ExecutionLocation;
use skyhook_worker::diagnostic::{Operation, Subject};
use skyhook_worker::target::Facts;
use skyhook_worker::{
    op::{
        CHUNK_BYTES, Input, Open, Resolve, Resolved, Running, StreamOutput, TargetPath, Workspace,
    },
    origin::{
        Leg,
        transport::{ReportingReader, Transport},
    },
    protocol::{
        CallId, CallIds, ClientFrame, EventFrame, FrameReader, Outbox, Outcome, Violation, Wire,
        WorkerFrame,
        flow::{Credits, QUEUE},
        write_frame,
    },
};

mod routing;
use routing::route_responses;

/// The in-process pipe between a relayed stream's caller and its relay.
const PIPE_BYTES: usize = 64 * 1024;

pub(crate) struct PooledConnection {
    /// What the worker reported about its machine as the connection opened.
    facts: Facts,
    /// The revision of each target on the route this connection was admitted at.
    admitted: crate::target::Revisions,
    /// Where the worker runs, at its root, which an origin's failures name.
    location: ExecutionLocation,
    /// What carries the connection, which a refused workspace names.
    kind: crate::target::ConnectionKind,
    outbox: Outbox<ClientFrame>,
    ids: CallIds,
    state: Arc<Mutex<ConnectionState>>,
    shutdown: CancellationToken,
}

/// A call awaiting its events and end.
struct CallSlot {
    events: mpsc::Sender<EventFrame>,
    done: oneshot::Sender<Result<Outcome, RemoteError>>,
    /// Credits for the call's input, returned by the worker's `InputAck`s.
    input: Option<Credits>,
    /// Where a contained call's paths resolved, once they have.
    landing: Option<oneshot::Sender<Landed>>,
}

/// How a contained call's paths resolved where it runs: all inside the
/// authorization root, its operation performing on them, or not, the call held.
pub(crate) enum Landed {
    Performing(Vec<Resolved>),
    Held(Vec<Resolved>),
}

/// A contained call's operation: performing, or held where it runs until resumed.
/// Dropping it cancels the call.
pub(crate) enum Flown<O: Wire> {
    Performing(Running<O, RemoteError>),
    Held {
        running: Running<O, RemoteError>,
        connection: Arc<PooledConnection>,
        id: CallId,
    },
}

impl<O: Wire> Flown<O> {
    /// The operation's run, a held one resumed to perform on its paths' resolutions.
    pub(crate) fn running(self) -> Running<O, RemoteError> {
        match self {
            Self::Performing(running) => running,
            Self::Held {
                running,
                connection,
                id,
            } => {
                connection.sending(id, |outbox| outbox.send(ClientFrame::Resume { id }));
                running
            }
        }
    }
}

/// Never held across an await, so a call can register, and a dropped one cancel,
/// without a runtime.
#[derive(Default)]
struct ConnectionState {
    calls: HashMap<CallId, CallSlot>,
    failure: Option<RemoteError>,
    /// How much longer than the end grace the worker may take to acknowledge the
    /// connection's end, given the legs it has relayed.
    ending: std::time::Duration,
    /// Each workspace a call named, as the worker resolved it.
    workspaces: HashMap<Vec<TargetPath>, TargetPath>,
}

fn lock(state: &Mutex<ConnectionState>) -> MutexGuard<'_, ConnectionState> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

impl PooledConnection {
    pub(crate) fn facts(&self) -> &Facts {
        &self.facts
    }

    pub(crate) fn admitted(&self) -> &crate::target::Revisions {
        &self.admitted
    }

    pub(in crate::remote) fn is_failed(&self) -> bool {
        lock(&self.state).failure.is_some()
    }

    /// The workspace `segments` name on the worker's machine, resolved there once for
    /// the connection; one the worker cannot use is refused.
    pub(in crate::remote) async fn workspace(
        self: &Arc<Self>,
        segments: &[TargetPath],
    ) -> Result<TargetPath, RemoteError> {
        if let Some(workspace) = lock(&self.state).workspaces.get(segments) {
            return Ok(workspace.clone());
        }
        let segments = segments.to_vec();
        let named = Workspace {
            segments: segments.clone(),
        };
        let workspace = (self.run(named, || {})?).end().await?.map_err(|error| {
            let transport = self.kind;
            RemoteError::from(RemoteFault::Refused { transport, error }).not_started()
        })?;
        lock(&self.state)
            .workspaces
            .insert(segments, workspace.clone());
        Ok(workspace)
    }

    /// Send on call `id`'s stream while the call is live, under the lock it ends
    /// under: a call that has ended, its waiting frames taken back, sends no more.
    fn sending<T>(&self, id: CallId, send: impl FnOnce(&Outbox<ClientFrame>) -> T) -> Option<T> {
        (lock(&self.state).calls.contains_key(&id)).then(|| send(&self.outbox))
    }

    /// Register a call and send its frame under one lock, so every call observes the
    /// same failure state.
    fn submit(
        self: &Arc<Self>,
        register: impl FnOnce(CallId, &mut ConnectionState) -> ClientFrame,
    ) -> Result<CancelOnDrop, RemoteError> {
        let mut state = lock(&self.state);
        if let Some(failure) = &state.failure {
            // This attempt never registered, even if earlier calls may have executed.
            return Err(failure.clone().not_started());
        }
        let id = self.ids.next();
        self.outbox.send(register(id, &mut state));
        Ok(CancelOnDrop {
            connection: self.clone(),
            id,
        })
    }

    /// Open a protocol session over `transport`, which `kind` carries: the shim runs at
    /// `target`, reached along a route `admitted` at those target revisions.
    pub(in crate::remote) async fn from_transport(
        transport: Transport,
        kind: crate::target::ConnectionKind,
        target: skyhook_tools::target::TargetName,
        admitted: crate::target::Revisions,
        tasks: &tokio_util::task::TaskTracker,
        shutdown: CancellationToken,
    ) -> Result<Self, RemoteError> {
        let Transport {
            mut input,
            output,
            owner,
        } = transport;
        let mut output = FrameReader::new(output);
        write_frame(&mut input, &ClientFrame::Hello)
            .await
            .map_err(|error| transport_error(error, Operation::Send).not_started())?;
        let answer = (output.next::<WorkerFrame>().await)
            .map_err(|error| transport_error(error, Operation::Receive).not_started())?;
        let facts = match answer {
            Some(WorkerFrame::Ready(facts)) => facts,
            Some(WorkerFrame::Refused(error)) => {
                let transport = kind;
                let refused = RemoteFault::Refused { transport, error };
                return Err(RemoteError::from(refused).not_started());
            }
            _ => {
                let invalid = transport_error(Violation::InvalidHandshake, Operation::Receive);
                return Err(invalid.not_started());
            }
        };
        let location = ExecutionLocation::named(target, facts.authorization_root.clone());
        let state = Arc::new(Mutex::new(ConnectionState::default()));
        let (outbox, writer) = Outbox::new();
        let routing = (state.clone(), outbox.clone(), shutdown.clone());
        tasks.spawn(async move {
            let (state, outbox, shutdown) = routing;
            route_responses(output, writer.spawn(input), outbox, &state, shutdown, owner).await;
        });
        Ok(PooledConnection {
            facts,
            admitted,
            location,
            kind,
            outbox,
            ids: CallIds::default(),
            state,
            shutdown,
        })
    }

    /// Start `operation` on this connection's machine. Dropping what it returns
    /// cancels the call there.
    pub(crate) fn run<O: Wire>(
        self: &Arc<Self>,
        operation: O,
        unreachable: impl FnOnce() + Send + 'static,
    ) -> Result<Running<O, RemoteError>, RemoteError> {
        Ok(self.start(operation, None, unreachable)?.1)
    }

    /// Start `operation` on this connection's machine in the call that resolves
    /// `paths` there, its operation naming them as requested: it performs on their
    /// resolutions at once when they all resolve inside the authorization root, and
    /// is otherwise held there until resumed. Dropping what it returns cancels it.
    pub(crate) async fn contain<O: Wire>(
        self: &Arc<Self>,
        paths: Resolve,
        operation: O,
        unreachable: impl FnOnce() + Send + 'static,
    ) -> Result<(Landed, Flown<O>), RemoteError> {
        let (landing, landed) = oneshot::channel();
        let (id, running) = self.start(operation, Some((paths, landing)), unreachable)?;
        match landed.await {
            Ok(landed @ Landed::Performing(_)) => Ok((landed, Flown::Performing(running))),
            Ok(landed @ Landed::Held(_)) => {
                let connection = self.clone();
                let held = Flown::Held {
                    running,
                    connection,
                    id,
                };
                Ok((landed, held))
            }
            Err(_) => Err((running.end().await.err())
                .unwrap_or_else(|| transport_error(Violation::ForeignAnswer, Operation::Receive))),
        }
    }

    fn start<O: Wire>(
        self: &Arc<Self>,
        mut operation: O,
        contained: Option<(Resolve, oneshot::Sender<Landed>)>,
        unreachable: impl FnOnce() + Send + 'static,
    ) -> Result<(CallId, Running<O, RemoteError>), RemoteError> {
        let input = operation.input().map(std::mem::take);
        let credits = input.as_ref().map(|_| Credits::default());
        let (events, mut frames) = mpsc::channel(QUEUE);
        let (done, ended) = oneshot::channel();
        let slot_credits = credits.clone();
        let cancel = self.submit(move |id, state| {
            let op = Box::new(operation.into_op());
            let (contained, landing) = contained.unzip();
            let slot = CallSlot {
                events,
                done,
                input: slot_credits,
                landing,
            };
            state.calls.insert(id, slot);
            ClientFrame::Call { id, op, contained }
        })?;
        let id = cancel.id;
        let pump = input.zip(credits).map(|(input, credits)| {
            let connection = self.clone();
            tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
                connection.send_input(id, input, credits).await
            }))
        });
        // Each frame of events is acknowledged once its consumer has room for its last.
        let (typed, received) = mpsc::channel(1);
        let connection = self.clone();
        let forward = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
            while let Some(frame) = frames.recv().await {
                let Some(events) = O::events(frame) else {
                    let foreign = transport_error(Violation::ForeignEvent, Operation::Receive);
                    return fail_connection(&connection.state, foreign);
                };
                for event in events {
                    if typed.send(event).await.is_err() {
                        return;
                    }
                }
                connection.sending(id, |outbox| {
                    outbox.acknowledge(ClientFrame::EventAck { id })
                });
            }
        }));
        let end = async move {
            let outcome = ended.await.map_err(|_| {
                transport_error(Violation::DispatcherStopped, Operation::Receive)
                    .or(PartialContext::default().request(RequestEffects::MayHaveExecuted))
            })??;
            O::answer(outcome)
                .ok_or_else(|| transport_error(Violation::ForeignAnswer, Operation::Receive))
        };
        let end = async move {
            let answer = end.await;
            if answer.is_err() {
                unreachable();
            }
            answer
        };
        Ok((id, Running::new(received, end, (cancel, pump, forward))))
    }

    /// Send a call's input in credited chunks, then its end; input that fails to read
    /// fails the call and stops it. Each chunk is read once its credit is held, so it
    /// takes whatever arrived while the credit was awaited.
    async fn send_input(&self, id: CallId, input: Input, credits: Credits) {
        let mut reader = input.into_reader();
        let mut buffer = vec![0; CHUNK_BYTES];
        loop {
            credits.take().await;
            let frame = match reader.read(&mut buffer).await {
                Ok(0) => {
                    self.sending(id, |outbox| outbox.send(ClientFrame::InputEnd { id }));
                    return;
                }
                Ok(read) => ClientFrame::Input {
                    id,
                    data: buffer[..read].to_vec(),
                },
                Err(error) => {
                    let failure = RemoteError::from(error)
                        .or(PartialContext::default().request(RequestEffects::MayHaveExecuted));
                    return self.stop(id, Some(failure));
                }
            };
            let Some(written) = self.sending(id, |outbox| outbox.send_data(frame)) else {
                return;
            };
            written.await;
        }
    }

    /// Stop call `id` unless it already ended, ending it with `failure` when one is
    /// given: the frames it still had waiting to be written are taken back, and the
    /// worker is told to stop it unless its request was among them.
    fn stop(&self, id: CallId, failure: Option<RemoteError>) {
        let call = {
            let mut state = lock(&self.state);
            if state.failure.is_some() {
                return;
            }
            let Some(call) = state.calls.remove(&id) else {
                return;
            };
            let unsent = self.outbox.withdraw(id);
            if !(unsent.iter()).any(|frame| matches!(frame, ClientFrame::Call { .. })) {
                self.outbox.send(ClientFrame::Cancel { id });
            }
            call
        };
        if let Some(failure) = failure {
            let _ = call.done.send(Err(failure));
        }
    }

    /// Open `leg` from this connection's machine, running `command` at its
    /// destination, and relay its byte stream.
    pub(in crate::remote) async fn open(
        self: &Arc<Self>,
        leg: Leg,
        command: String,
    ) -> Result<Transport, RemoteError> {
        {
            let mut state = lock(&self.state);
            state.ending = state.ending.max(leg.ending());
        }
        let (client, peer) = tokio::io::duplex(PIPE_BYTES);
        let (input, mut output) = tokio::io::split(peer);
        let input = Input::new(input);
        let open = Open {
            leg,
            command,
            input,
        };
        let mut running = self.run(open, || {})?;
        let (sender, result) = oneshot::channel();
        let location = self.location.clone();
        let relay = tokio::spawn(async move {
            while let Some(StreamOutput(data)) = running.event().await {
                if output.write_all(&data).await.is_err() {
                    return;
                }
            }
            let _ = output.shutdown().await;
            let ended = match running.end().await {
                Ok(Ok(())) => Ok(()),
                // An origin's failure names the machine it happened on.
                Ok(Err(failure)) => {
                    let site = PartialContext::default().at(FailureSite::Execution(location));
                    Err(RemoteError::from(failure).or(site))
                }
                Err(error) => Err(error),
            };
            let _ = sender.send(ended);
        });
        let (output, input) = tokio::io::split(client);
        Ok(Transport {
            input: Box::new(input),
            output: Box::new(ReportingReader::new(output, result)),
            owner: Box::new(tokio_util::task::AbortOnDropHandle::new(relay)),
        })
    }
}

fn transport_error(error: impl Into<RemoteError>, operation: Operation) -> RemoteError {
    error.into().or(
        PartialContext::new(operation, Subject::Label("remote transport".into()))
            .at(FailureSite::Host),
    )
}

/// Cancels a call dropped before its end, so the worker stops it, whether or not a
/// runtime is there to drop it on.
struct CancelOnDrop {
    connection: Arc<PooledConnection>,
    id: CallId,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.connection.stop(self.id, None);
    }
}

/// Keep the first failure and end every call with it.
fn fail_connection(state: &Mutex<ConnectionState>, failure: RemoteError) {
    let (failure, calls) = {
        let mut state = lock(state);
        let failure = state.failure.get_or_insert(failure).clone();
        (failure, std::mem::take(&mut state.calls))
    };
    for call in calls.into_values() {
        let failure = failure.clone();
        let failure =
            failure.or(PartialContext::default().request(RequestEffects::MayHaveExecuted));
        let _ = call.done.send(Err(failure));
    }
}

impl Drop for PooledConnection {
    fn drop(&mut self) {
        // The session tracks the reader, which ends the connection.
        self.shutdown.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::bounded;
    use skyhook_tools::diagnostic::Cause;
    use skyhook_worker::diagnostic::Effects;
    use skyhook_worker::{
        op::{Content, Read, ReadEnd, ReadEvent, ReadMode, Resolve, TargetPath, Write, Written},
        origin::error::{TransportError, WinrmStreamError},
        protocol::flow::WINDOW,
        secret::ResolveError,
    };
    use std::assert_matches;

    fn location() -> ExecutionLocation {
        ExecutionLocation::named("build".parse().unwrap(), "/build".into())
    }

    /// A worker's end of a connection no worker answers: the frames it receives, and
    /// where it sends its own.
    struct Peer {
        frames: FrameReader<tokio::io::DuplexStream>,
        sent: tokio::io::DuplexStream,
    }

    /// A connection to a peer, whose frames reach it through a pipe of `buffer` bytes.
    async fn wired(buffer: usize) -> (Arc<PooledConnection>, Peer) {
        wired_owning(buffer, Box::new(()), &tokio_util::task::TaskTracker::new()).await
    }

    /// [`wired`], its transport owned by `owner` and its frames routed on a task
    /// `tasks` tracks.
    async fn wired_owning(
        buffer: usize,
        owner: Box<dyn Send>,
        tasks: &tokio_util::task::TaskTracker,
    ) -> (Arc<PooledConnection>, Peer) {
        let (input, frames) = tokio::io::duplex(buffer);
        let (mut sent, output) = tokio::io::duplex(64 * 1024);
        let ready = WorkerFrame::Ready(crate::remote::manager::tests::facts());
        write_frame(&mut sent, &ready).await.unwrap();
        let transport = Transport {
            input: Box::new(input),
            output: Box::new(output),
            owner,
        };
        let connecting = PooledConnection::from_transport(
            transport,
            crate::target::ConnectionKind::Ssh,
            "build".parse().unwrap(),
            Default::default(),
            tasks,
            CancellationToken::new(),
        );
        let mut peer = Peer {
            frames: FrameReader::new(frames),
            sent,
        };
        let (connection, hello) = tokio::join!(connecting, next(&mut peer));
        assert_matches!(hello, ClientFrame::Hello);
        (Arc::new(connection.unwrap()), peer)
    }

    fn route() -> skyhook_worker::origin::ssh::SshRoute {
        serde_json::from_value(
            serde_json::json!({"Plain": [{"name": "build", "host": "build.test",
            "port": null, "user": null, "key": null, "forward_agent": false, "host_keys": "yes",
            "options": []}]}),
        )
        .unwrap()
    }

    fn resolve() -> Resolve {
        Resolve {
            workspace: TargetPath::new("/build"),
            paths: Vec::new(),
        }
    }

    async fn next(peer: &mut Peer) -> ClientFrame {
        bounded(peer.frames.next::<ClientFrame>())
            .await
            .unwrap()
            .unwrap()
    }

    /// A call dropped while its request still waits to be written, behind one the
    /// transport is slow to take, is taken back, nothing of it sent; one dropped once
    /// its request is written, here off any runtime, is cancelled where it runs.
    /// Neither keeps its slot.
    #[tokio::test]
    async fn dropped_calls_are_taken_back_or_cancelled() {
        let (connection, mut peer) = wired(1).await;
        let running = connection.run(resolve(), || {}).unwrap();
        drop(connection.run(resolve(), || {}).unwrap());
        let ClientFrame::Call { id, .. } = next(&mut peer).await else {
            panic!("expected the call");
        };
        std::thread::spawn(move || drop(running)).join().unwrap();
        assert_matches!(next(&mut peer).await, ClientFrame::Cancel { id: cancelled } if cancelled == id);
        assert!(lock(&connection.state).calls.is_empty());
    }

    /// A call answered as another operation fails as its transport's failure, not as
    /// its tool's own. A frame the protocol does not allow fails the connection, and
    /// every call on it ends with that failure.
    #[tokio::test]
    async fn invalid_frames_fail_the_calls_on_their_connection() {
        let (connection, mut peer) = wired(4096).await;
        let running = connection.run(resolve(), || {}).unwrap();
        let ClientFrame::Call { id, .. } = next(&mut peer).await else {
            panic!("expected the call");
        };
        let outcome = <Write as Wire>::outcome(Ok(Written { bytes: 0 }));
        let foreign = WorkerFrame::Done { id, outcome };
        write_frame(&mut peer.sent, &foreign).await.unwrap();
        let foreign = bounded(running.end()).await.unwrap_err();
        assert_matches!(
            foreign.fault,
            RemoteFault::Transport(TransportError::Protocol(Violation::ForeignAnswer))
        );
        let foreign = crate::tool::ToolError::from(foreign).diagnostic();
        assert_eq!(foreign.context.operation, Operation::Receive);
        let protocol = skyhook_tools::diagnostic::ConnectionFault::Protocol;
        assert_matches!(foreign.cause, Cause::Connection { fault, .. } if fault == protocol);
        let running = connection.run(resolve(), || {}).unwrap();
        let ready = WorkerFrame::Ready(connection.facts().clone());
        write_frame(&mut peer.sent, &ready).await.unwrap();
        let ended = bounded(running.end()).await.unwrap_err();
        assert_matches!(
            ended.fault,
            RemoteFault::Transport(TransportError::Protocol(Violation::SecondHandshake))
        );
        assert!(connection.is_failed());
    }

    /// A shim of another build answers the hello with what this build cannot decode:
    /// the connection fails as it opens, nothing started.
    #[tokio::test]
    async fn a_handshake_this_build_cannot_decode_fails_the_connection() {
        let (mut sent, output) = tokio::io::duplex(4096);
        let ready = std::collections::BTreeMap::from([("Ready", (5, "facts"))]);
        write_frame(&mut sent, &ready).await.unwrap();
        let transport = Transport {
            input: Box::new(tokio::io::sink()),
            output: Box::new(output),
            owner: Box::new(()),
        };
        let tasks = tokio_util::task::TaskTracker::new();
        let opening = PooledConnection::from_transport(
            transport,
            crate::target::ConnectionKind::Ssh,
            "build".parse().unwrap(),
            Default::default(),
            &tasks,
            CancellationToken::new(),
        );
        let failed = bounded(opening).await.err().unwrap();
        let failed = crate::tool::ToolError::from(failed).diagnostic();
        assert_eq!(failed.context.operation, Operation::Receive);
        assert_eq!(failed.context.effects, Effects::NotStarted);
        assert_matches!(failed.cause, Cause::Io(_));
    }

    /// Ending a connection whose frames the transport never takes still ends: the
    /// transport is released and the connection fails.
    #[tokio::test(start_paused = true)]
    async fn an_end_stalled_behind_a_write_still_releases_the_transport() {
        struct Released(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for Released {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let tasks = tokio_util::task::TaskTracker::new();
        let owner = Box::new(Released(released.clone()));
        let (connection, _peer) = wired_owning(1, owner, &tasks).await;
        let stalled = connection.run(resolve(), || {}).unwrap();
        connection.shutdown.cancel();
        tasks.close();
        bounded(tasks.wait()).await;
        assert!(released.load(std::sync::atomic::Ordering::SeqCst));
        assert!(connection.is_failed());
        drop(stalled);
    }

    /// Input runs one credit window ahead of the worker's acknowledgements, and
    /// stops once its call has ended.
    #[tokio::test]
    async fn input_waits_for_credit_and_stops_with_its_call() {
        let (connection, mut peer) = wired(64 * 1024).await;
        let bytes = vec![7; CHUNK_BYTES * (WINDOW + 2)];
        let write = Write {
            path: TargetPath::new("/build/file"),
            content: Content::Streamed(Input::new(std::io::Cursor::new(bytes))),
        };
        let running = connection.run(write, || {}).unwrap();
        let ClientFrame::Call { id, .. } = next(&mut peer).await else {
            panic!("expected the call");
        };
        for _ in 0..WINDOW {
            assert_matches!(next(&mut peer).await, ClientFrame::Input { .. });
        }
        assert!(
            lock(&connection.state).calls[&id]
                .input
                .as_ref()
                .unwrap()
                .acknowledge()
        );
        assert_matches!(next(&mut peer).await, ClientFrame::Input { .. });
        let CallSlot { done, .. } = lock(&connection.state).calls.remove(&id).unwrap();
        let ended = <Write as Wire>::outcome(Ok(Written { bytes: 0 }));
        done.send(Ok(ended)).unwrap();
        assert_matches!(bounded(running.end()).await, Ok(Ok(Written { bytes: 0 })));
        // The input sent nothing more, not even its end.
        drop(connection);
        let rest = bounded(peer.frames.next::<ClientFrame>()).await;
        assert_matches!(rest.unwrap(), Some(ClientFrame::End) | None);
    }

    /// Input whose reading fails part way fails its call with that failure and cancels
    /// it where it runs, never ending the input as though it were complete.
    #[tokio::test]
    async fn input_that_fails_to_read_fails_and_cancels_its_call() {
        let (connection, mut peer) = wired(64 * 1024).await;
        let chunks = futures_util::stream::iter([
            Ok(bytes::Bytes::from_static(b"prefix")),
            Err(std::io::Error::other("the source failed")),
        ]);
        let write = Write {
            path: TargetPath::new("/build/file"),
            content: Content::Streamed(Input::new(tokio_util::io::StreamReader::new(chunks))),
        };
        let running = connection.run(write, || {}).unwrap();
        let ClientFrame::Call { id, .. } = next(&mut peer).await else {
            panic!("expected the call");
        };
        assert_matches!(next(&mut peer).await, ClientFrame::Input { .. });
        assert_matches!(next(&mut peer).await, ClientFrame::Cancel { id: cancelled } if cancelled == id);
        let ended = bounded(running.end()).await.unwrap_err();
        assert_matches!(&ended.fault, RemoteFault::Transport(TransportError::Io(failed))
            if failed.detail.as_deref() == Some("the source failed"));
        assert!(lock(&connection.state).calls.is_empty());
    }

    /// A failed write fails the calls waiting on it as ones that may have run,
    /// quarantines the connection so later calls never start, and ends its relayed
    /// streams.
    #[tokio::test]
    async fn failed_connections_refuse_calls_and_end_relayed_streams() {
        use tokio::io::AsyncReadExt as _;
        let (connection, mut peer) = wired(4096).await;
        let mut stream = connection
            .open(Leg::Ssh(route()), "true".into())
            .await
            .unwrap();
        assert_matches!(next(&mut peer).await, ClientFrame::Call { .. });
        let Peer {
            frames,
            sent: _sent,
        } = peer;
        drop(frames);
        let running = connection.run(resolve(), || {}).unwrap();
        let error = bounded(running.end()).await.unwrap_err();
        let error = crate::tool::ToolError::from(error).diagnostic();
        assert_eq!(error.context.operation, Operation::Send);
        assert_eq!(error.context.site, FailureSite::Host);
        assert_eq!(error.context.request, RequestEffects::MayHaveExecuted);
        assert_matches!(error.cause, Cause::Io(_));
        let retry = connection.run(resolve(), || {}).err().unwrap();
        let retry = crate::tool::ToolError::from(retry).diagnostic();
        assert_eq!(retry.context.effects, Effects::NotStarted);
        let ended = bounded(stream.output.read_to_end(&mut Vec::new())).await;
        let ended = RemoteError::from(ended.unwrap_err());
        let ended = crate::tool::ToolError::from(ended).diagnostic();
        assert_eq!(ended.context.operation, Operation::Send);
        assert!(lock(&connection.state).calls.is_empty());
    }

    type Worker = tokio::task::JoinHandle<
        Result<skyhook_worker::protocol::serve::Ended, skyhook_worker::protocol::serve::ServeError>,
    >;

    /// A connection to a worker serving `root` in this process, the worker, and the
    /// tasks that route its frames.
    async fn served(root: &str) -> (Arc<PooledConnection>, Worker, tokio_util::task::TaskTracker) {
        let settings = skyhook_worker::target::Settings {
            authorization_root: root.to_owned(),
        };
        let (client, shim) = tokio::io::duplex(64 * 1024);
        let (shim_input, shim_output) = tokio::io::split(shim);
        let worker = tokio::spawn(skyhook_worker::protocol::serve::serve(
            shim_input,
            shim_output,
            settings,
        ));
        let (mut output, input) = tokio::io::split(client);
        let preamble = &mut [0; skyhook_worker::protocol::PREAMBLE.len()];
        bounded(output.read_exact(preamble)).await.unwrap();
        let transport = Transport {
            input: Box::new(input),
            output: Box::new(output),
            owner: Box::new(()),
        };
        let tasks = tokio_util::task::TaskTracker::new();
        let connection = PooledConnection::from_transport(
            transport,
            crate::target::ConnectionKind::Ssh,
            "build".parse().unwrap(),
            Default::default(),
            &tasks,
            CancellationToken::new(),
        );
        let connection = Arc::new(bounded(connection).await.unwrap());
        (connection, worker, tasks)
    }

    /// A WinRM stream an origin opens reads its password there, and the origin's
    /// failure comes back as the stream's, naming the machine it happened on.
    #[tokio::test]
    async fn winrm_streams_read_their_password_on_the_origin() {
        let root = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();
        let (connection, _worker, _tasks) = served(root.to_str().unwrap()).await;
        let unset = "SKYHOOK_TEST_WINRM_PASSWORD_NEVER_SET";
        let route = serde_json::json!({"name": "win", "endpoint": {"host": "127.0.0.1", "port": 9},
            "auth": {"Negotiate": {"user": "u", "password": {"env": unset}}}, "tls": "off"});
        let route = serde_json::from_value(route).unwrap();
        let mut stream = connection
            .open(Leg::Winrm(route), "ignored".into())
            .await
            .unwrap();
        let read = bounded(stream.output.read_to_end(&mut Vec::new())).await;
        let error = RemoteError::from(read.unwrap_err());
        assert_matches!(
            &error.fault,
            RemoteFault::Transport(TransportError::Winrm(WinrmStreamError::Password {
                target,
                error: ResolveError::NotSet(variable),
            })) if target == "win" && variable == unset
        );
        let diagnostic = crate::tool::ToolError::from(error).diagnostic();
        assert_matches!(&diagnostic.context.site, FailureSite::Execution(origin)
            if origin.target == location().target);
    }

    /// A request and an end too large for one frame cross whole, and a call whose
    /// consumer takes nothing holds up no other call on its connection.
    #[tokio::test]
    async fn large_frames_and_stalled_calls_leave_other_calls_running() {
        let root = tempfile::tempdir().unwrap();
        let root_path = std::fs::canonicalize(root.path()).unwrap();
        let large = vec![1; CHUNK_BYTES * (WINDOW + 2)];
        std::fs::write(root_path.join("large"), &large).unwrap();
        std::fs::write(root_path.join("small"), b"small").unwrap();
        let text = root_path.to_str().unwrap().to_owned();
        let (connection, _worker, _tasks) = served(&text).await;
        let read = |name: &str| Read {
            path: TargetPath::new(format!("{text}/{name}")),
            workspace: TargetPath::new(&text),
            mode: ReadMode::File,
            from: Default::default(),
        };
        let _stalled = connection.run(read("large"), || {}).unwrap();
        let paths = (0..5000).map(|index| {
            let path = format!("missing/{index:0>32}");
            (
                TargetPath::new(path),
                skyhook_worker::op::PathKind::Writable,
            )
        });
        let resolve = Resolve {
            workspace: TargetPath::new(&text),
            paths: paths.collect(),
        };
        let resolved = connection.run(resolve, || {}).unwrap();
        let resolved = bounded(resolved.end()).await.unwrap().unwrap();
        assert_eq!(resolved.len(), 5000);
        let mut small = connection.run(read("small"), || {}).unwrap();
        let mut received = Vec::new();
        while let Some(event) = bounded(small.event()).await {
            if let ReadEvent::Bytes(chunk) = event {
                received.extend(chunk);
            }
        }
        assert_eq!(received, b"small");
    }

    /// Calls run on the machine a worker serves, their events crossing in more
    /// chunks than one credit window, and dropping the connection ends it.
    #[tokio::test]
    async fn calls_run_where_the_worker_serves_them() {
        let root = tempfile::tempdir().unwrap();
        let root_path = std::fs::canonicalize(root.path()).unwrap();
        let bytes: Vec<u8> = (0..CHUNK_BYTES * (WINDOW + 2) + 11)
            .map(|index| (index % 253) as u8)
            .collect();
        std::fs::write(root_path.join("file"), &bytes).unwrap();
        let text = root_path.to_str().unwrap().to_owned();
        let (connection, worker, tasks) = served(&text).await;
        let segments = [TargetPath::new(&text)];
        let workspace = connection.workspace(&segments);
        assert_eq!(
            bounded(workspace).await.unwrap(),
            TargetPath::new(text.clone())
        );
        let read = Read {
            path: TargetPath::new(format!("{text}/file")),
            workspace: TargetPath::new(text),
            mode: ReadMode::File,
            from: Default::default(),
        };
        let mut running = connection.run(read, || {}).unwrap();
        let mut received = Vec::new();
        while let Some(event) = bounded(running.event()).await {
            if let ReadEvent::Bytes(chunk) = event {
                received.extend(chunk);
            }
        }
        assert_matches!(bounded(running.end()).await, Ok(Ok(ReadEnd::File { .. })));
        assert_eq!(received, bytes);
        drop(connection);
        let ended = bounded(worker).await.unwrap().unwrap();
        assert_eq!(ended, skyhook_worker::protocol::serve::Ended::Requested);
        tasks.close();
        bounded(tasks.wait()).await;
    }
}
