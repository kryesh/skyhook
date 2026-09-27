//! Shim protocol client, response routing, and relayed SSH streams.
use std::{collections::HashMap, sync::Arc};

use tokio::sync::{Mutex, OwnedMutexGuard, oneshot};

use crate::{
    execution::ExecutionLocation,
    job::CancellationToken,
    remote::{
        error::{ProtocolError, RemoteError},
        flow::{self, Credits},
        prompt::SensitivePromptHandler,
        protocol::{
            PromptId, Request, RequestId, Response, Sequence, read_frame, spawn_owned_write,
            write_frame,
        },
        transport::{Transport, Writer},
    },
    tool::{
        ToolContext, ToolOutput,
        diagnostic::{Effects, FailureSite, Operation, PartialContext, Subject},
        source::Source,
    },
};

pub(in crate::remote) type Session = Arc<PooledConnection>;

mod permissions;
mod results;
mod routing;
mod streams;
use routing::route_responses;
use streams::ClientStream;
#[cfg(test)]
pub(crate) use tests::test_transport;

pub(crate) struct PooledConnection {
    writer: Arc<Mutex<Writer>>,
    ids: Sequence<RequestId>,
    state: Arc<Mutex<ConnectionState>>,
    shutdown: CancellationToken,
}

type PendingResult = Result<results::Received, RemoteError>;

struct PendingCall {
    sender: oneshot::Sender<PendingResult>,
    context: ToolContext,
    /// Credits for the call's source upload, returned by the worker's `SourceAck`s.
    upload: Option<OwnedCredits>,
}

/// Credits a sender waits for; dropping their owner releases the sender.
struct OwnedCredits(Credits);

impl Drop for OwnedCredits {
    fn drop(&mut self) {
        self.0.close();
    }
}

#[derive(Default)]
struct ConnectionState {
    pending: HashMap<RequestId, PendingCall>,
    failure: Option<RemoteError>,
    streams: HashMap<RequestId, ClientStream>,
}

impl ConnectionState {
    /// Keep the first failure and end every relayed stream with it.
    fn fail(&mut self, failure: RemoteError) -> RemoteError {
        let failure = self.failure.get_or_insert(failure).clone();
        for (_, stream) in self.streams.drain() {
            let _ = stream.closed.send(Err(failure.clone()));
        }
        failure
    }
}

impl PooledConnection {
    pub(in crate::remote) async fn is_failed(&self) -> bool {
        self.state.lock().await.failure.is_some()
    }

    /// Serialize registration and sending so every RPC observes the same failure state.
    async fn submit<T>(
        &self,
        register: impl FnOnce(RequestId, &mut ConnectionState) -> (Request, T),
    ) -> Result<(RequestId, T), RemoteError> {
        let writer = self.writer.clone().lock_owned().await;
        let mut state = self.state.lock().await;
        if let Some(failure) = &state.failure {
            // This attempt never registered, even if earlier calls may have executed.
            return Err(not_started(failure.clone()));
        }
        let Some(request_id) = self.ids.next() else {
            drop(state);
            drop(writer);
            let failure = transport_error(
                ProtocolError::Violation("request ID space exhausted"),
                Operation::Send,
            );
            fail_connection(&self.state, failure.clone()).await;
            return Err(not_started(failure));
        };
        let (request, result) = register(request_id, &mut state);
        drop(state);
        write_request(writer, &self.state, request).await?;
        Ok((request_id, result))
    }

    /// Serve a shim's responses; the shim runs at `location`.
    pub(in crate::remote) async fn from_transport(
        transport: Transport,
        location: ExecutionLocation,
        prompts: Arc<dyn SensitivePromptHandler>,
        tasks: &tokio_util::task::TaskTracker,
        shutdown: CancellationToken,
    ) -> Result<Self, RemoteError> {
        let Transport {
            mut input,
            mut output,
            owner,
        } = transport;
        write_frame(&mut input, &Request::Hello)
            .await
            .map_err(|error| transport_error(error, Operation::Send))
            .map_err(not_started)?;
        if !matches!(
            read_frame::<_, Response>(&mut output)
                .await
                .map_err(|error| transport_error(error, Operation::Receive))
                .map_err(not_started)?,
            Some(Response::Ready)
        ) {
            return Err(not_started(transport_error(
                ProtocolError::Violation("invalid shim handshake"),
                Operation::Receive,
            )));
        }
        let state = Arc::new(Mutex::new(ConnectionState::default()));
        let writer = Arc::new(Mutex::new(input));
        let reader_state = state.clone();
        let reader_writer = writer.clone();
        let reader_shutdown = shutdown.clone();
        tasks.spawn(async move {
            route_responses(
                output,
                &reader_state,
                &reader_writer,
                location,
                prompts,
                reader_shutdown,
                owner,
            )
            .await;
        });
        Ok(PooledConnection {
            writer,
            ids: Sequence::default(),
            state,
            shutdown,
        })
    }
}

/// Submit one request, stream its source, and await its result, cancelling it
/// with the call.
async fn call(
    connection: &PooledConnection,
    context: &ToolContext,
    request: impl FnOnce(RequestId) -> Request + Send,
    source: Option<Source>,
) -> Result<results::Received, RemoteError> {
    if context.is_cancelled() {
        return Err(RemoteError::Cancelled);
    }
    let credits = source.as_ref().map(|_| Credits::default());
    let upload = credits.clone().map(OwnedCredits);
    // Armed at registration: a call dropped while its request is still being
    // written is released too.
    let (request_id, (receiver, mut unfinished)) = connection
        .submit(move |request_id, state| {
            let (sender, receiver) = oneshot::channel();
            state.pending.insert(
                request_id,
                PendingCall {
                    sender,
                    context: context.clone(),
                    upload,
                },
            );
            let unfinished = CancelOnDrop::new(connection, request_id);
            (request(request_id), (receiver, unfinished))
        })
        .await?;
    if let Some((source, credits)) = source.zip(credits) {
        // The worker holds the call until its source ends, so a failed upload
        // relies on the guard to release it.
        tokio::select! {
            result = send_source(connection, request_id, source, credits) => result,
            () = context.cancelled() => Err(RemoteError::Cancelled),
        }?;
    }
    let received = async {
        receiver.await.map_err(|_| {
            transport_error(
                ProtocolError::Violation("remote response dispatcher stopped unexpectedly"),
                Operation::Receive,
            )
            .or(PartialContext::default().effects(Effects::MayHaveExecuted))
        })?
    };
    tokio::select! {
        result = received => {
            unfinished.defuse();
            result
        }
        () = context.cancelled() => Err(RemoteError::Cancelled),
    }
}

/// Send a source's contents in credited chunks; the worker starts the call at
/// the end. Closed credits mean the call already ended, so its result follows.
async fn send_source(
    connection: &PooledConnection,
    request_id: RequestId,
    source: Source,
    credits: Credits,
) -> Result<(), RemoteError> {
    let reader = tokio::fs::File::from_std(source.reader().map_err(host_source_error)?);
    flow::pump(reader, Some(&credits), |data| async move {
        let request = match data {
            Some(data) => Request::SourceData { request_id, data },
            None => Request::SourceEnd { request_id },
        };
        let writer = connection.writer.clone().lock_owned().await;
        write_request(writer, &connection.state, request).await
    })
    .await
    // Send failures already carry their full context, so this only completes read failures.
    .map_err(host_source_error)
}

fn host_source_error(error: impl Into<RemoteError>) -> RemoteError {
    error.into().or(
        PartialContext::new(Operation::Read, Subject::Label("source".into()))
            .at(FailureSite::Host)
            .effects(Effects::NotStarted),
    )
}

fn not_started(error: RemoteError) -> RemoteError {
    let (diagnostic, output) = error
        .into_tool_error()
        .effects(Effects::NotStarted)
        .into_facts();
    RemoteError::Remote {
        diagnostic: Box::new(diagnostic),
        output: output.map(Box::new),
    }
}

fn transport_error(error: impl Into<RemoteError>, operation: Operation) -> RemoteError {
    error.into().or(
        PartialContext::new(operation, Subject::Label("remote transport".into()))
            .at(FailureSite::Host),
    )
}

/// Once registered, the frame is finished even if this await is dropped;
/// a failed write quarantines the connection.
async fn write_request(
    writer: OwnedMutexGuard<Writer>,
    state: &Mutex<ConnectionState>,
    request: Request,
) -> Result<(), RemoteError> {
    if let Err(error) = spawn_owned_write(writer, request).await {
        let failure = transport_error(error, Operation::Send);
        fail_connection(state, failure.clone()).await;
        return Err(failure.or(PartialContext::default().effects(Effects::MayHaveExecuted)));
    }
    Ok(())
}

/// Cancels a submitted call that ends or is dropped before its terminal result,
/// so the worker always releases it (and any source upload it is holding). The
/// Cancel is written on its own task: a caller dropped while another frame holds
/// the writer cannot lose it.
struct CancelOnDrop {
    writer: Arc<Mutex<Writer>>,
    state: Arc<Mutex<ConnectionState>>,
    request_id: Option<RequestId>,
}

impl CancelOnDrop {
    fn new(connection: &PooledConnection, request_id: RequestId) -> Self {
        Self {
            writer: connection.writer.clone(),
            state: connection.state.clone(),
            request_id: Some(request_id),
        }
    }

    /// The call reached its terminal result.
    fn defuse(&mut self) {
        self.request_id = None;
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        let (Some(request_id), Ok(runtime)) =
            (self.request_id, tokio::runtime::Handle::try_current())
        else {
            return;
        };
        let (writer, state) = (self.writer.clone(), self.state.clone());
        runtime.spawn(async move {
            if state.lock().await.failure.is_some() {
                return;
            }
            let writer = writer.lock_owned().await;
            let _ = write_request(writer, &state, Request::Cancel { request_id }).await;
        });
    }
}

async fn fail_connection(state: &Mutex<ConnectionState>, failure: RemoteError) {
    let (failure, pending) = {
        let mut state = state.lock().await;
        (state.fail(failure), std::mem::take(&mut state.pending))
    };
    for pending in pending.into_values() {
        let _ = pending.sender.send(Err(failure
            .clone()
            .or(PartialContext::default().effects(Effects::MayHaveExecuted))));
    }
}

impl Drop for PooledConnection {
    fn drop(&mut self) {
        // The session tracks the reader until accepted payloads have drained.
        self.shutdown.cancel();
    }
}

impl PooledConnection {
    pub(in crate::remote) async fn execute(
        &self,
        name: String,
        arguments: serde_json::Value,
        context: &ToolContext,
    ) -> Result<ToolOutput, RemoteError> {
        let capabilities = context.capabilities().iter().collect();
        let source = context.source().cloned();
        let streams_source = source.is_some();
        let request = move |request_id| Request::Tool {
            request_id,
            name,
            arguments,
            capabilities,
            source: streams_source,
        };
        let received = call(self, context, request, source).await?;
        if received.source.is_some() {
            return Err(ProtocolError::Violation("source contents for a tool call").into());
        }
        Ok(received.output)
    }

    /// Read a source file on this connection's machine into a local spool.
    pub(in crate::remote) async fn read_source(
        &self,
        tool: String,
        path: String,
        context: &ToolContext,
    ) -> Result<Source, RemoteError> {
        let capabilities = context.capabilities().iter().collect();
        let request = move |request_id| Request::ReadSource {
            request_id,
            tool,
            path,
            capabilities,
        };
        call(self, context, request, None)
            .await?
            .source
            .ok_or_else(|| ProtocolError::Violation("source read without contents").into())
    }
}

#[cfg(test)]
pub(in crate::remote) mod tests {
    use super::*;
    use crate::job::CancellationToken;
    use crate::remote::protocol::{ControlRequest, RemoteToolOutput, RemoteToolResult};
    use crate::target::TargetDefinition;
    use crate::tests::bounded;
    use crate::tool::{
        authorization::{AuthorizationArguments, AuthorizationCoordinator, AuthorizationError},
        policy::{AllowAll, Capability},
    };

    /// A live fake shim transport for manager/router tests, including the real handshake.
    pub(crate) fn test_transport(
        mut reply: Option<crate::tool::invocation::LocalError>,
    ) -> Transport {
        struct FakeShim(tokio::task::JoinHandle<()>);
        impl Drop for FakeShim {
            fn drop(&mut self) {
                self.0.abort();
            }
        }

        let (client, mut shim) = tokio::io::duplex(4096);
        let owner = FakeShim(tokio::spawn(async move {
            let hello = read_frame::<_, Request>(&mut shim).await;
            if !matches!(hello, Ok(Some(Request::Hello)))
                || write_frame(&mut shim, &Response::Ready).await.is_err()
            {
                return;
            }
            while let Ok(Some(request)) = read_frame::<_, Request>(&mut shim).await {
                if let Request::Tool { request_id, .. } = request
                    && let Some(error) = reply.take()
                {
                    write_result(
                        &mut shim,
                        request_id,
                        Err(crate::remote::worker::remote_error(error)),
                    )
                    .await;
                }
            }
        }));
        let (output, input) = tokio::io::split(client);
        Transport {
            input: Box::new(input),
            output: Box::new(output),
            owner: Box::new(owner),
        }
    }

    pub(super) fn sink() -> Arc<Mutex<Writer>> {
        Arc::new(Mutex::new(Box::new(tokio::io::sink())))
    }

    pub(super) fn test_connection() -> PooledConnection {
        PooledConnection {
            writer: sink(),
            ids: Sequence::default(),
            state: Arc::new(Mutex::new(ConnectionState::default())),
            shutdown: CancellationToken::new(),
        }
    }

    /// A test connection whose request writer is the returned in-memory peer.
    pub(super) async fn wired_connection(
        buffer: usize,
    ) -> (Arc<PooledConnection>, tokio::io::DuplexStream) {
        let connection = test_connection();
        let (input, peer) = tokio::io::duplex(buffer);
        *connection.writer.lock().await = Box::new(input);
        (Arc::new(connection), peer)
    }

    fn allow_all() -> AuthorizationCoordinator {
        AuthorizationCoordinator::new(Arc::new(AllowAll))
    }

    #[tokio::test]
    async fn cancellation_before_submission_creates_no_remote_registration() {
        let runtime = crate::tests::TestRuntime::new().await;
        let context = fixture_context(&runtime);
        context.cancellation_token().cancel();
        let connection = test_connection();
        let result = connection.execute("read".into(), serde_json::json!({}), &context);
        assert!(matches!(result.await, Err(RemoteError::Cancelled)));
        assert!(connection.state.lock().await.pending.is_empty());
        assert_eq!(connection.ids.next(), Some(RequestId::new(1)));
    }

    /// A call dropped before its terminal result, without cancelling its
    /// context, still tells the worker to release it.
    #[tokio::test]
    async fn dropped_calls_send_cancel() {
        let runtime = crate::tests::TestRuntime::new().await;
        let context = fixture_context(&runtime);
        let (connection, mut peer) = wired_connection(4096).await;
        let call = {
            let (connection, context) = (connection.clone(), context.clone());
            tokio::spawn(async move {
                connection
                    .execute("read".into(), serde_json::json!({}), &context)
                    .await
            })
        };
        let Some(Request::Tool { request_id, .. }) =
            read_frame::<_, Request>(&mut peer).await.unwrap()
        else {
            panic!("expected the tool request");
        };
        call.abort();
        assert!(call.await.unwrap_err().is_cancelled());
        assert!(!context.is_cancelled());
        let cancel = bounded(read_frame::<_, Request>(&mut peer)).await.unwrap();
        assert!(matches!(cancel, Some(Request::Cancel { request_id: id }) if id == request_id));
    }

    /// A cancelled call dropped while another frame holds the writer still
    /// sends its Cancel once the writer is free.
    #[tokio::test]
    async fn cancellation_waiting_for_the_writer_survives_the_call() {
        let runtime = crate::tests::TestRuntime::new().await;
        let context = fixture_context(&runtime);
        let (connection, mut peer) = wired_connection(4096).await;
        let request_id = {
            let mut call =
                std::pin::pin!(connection.execute("read".into(), serde_json::json!({}), &context));
            assert!(futures_util::poll!(&mut call).is_pending());
            let Some(Request::Tool { request_id, .. }) = read_frame(&mut peer).await.unwrap()
            else {
                panic!("expected the tool request");
            };
            let held = connection.writer.clone().lock_owned().await;
            assert!(futures_util::poll!(&mut call).is_pending());
            context.cancellation_token().cancel();
            let _ = futures_util::poll!(&mut call);
            drop(held);
            request_id
        };
        drop(connection);
        let cancel = bounded(read_frame::<_, Request>(&mut peer)).await.unwrap();
        assert!(matches!(cancel, Some(Request::Cancel { request_id: id }) if id == request_id));
    }

    /// Drop `submission` once its frame has started on a one-byte pipe,
    /// returning that frame and the next.
    async fn drop_mid_write(
        peer: tokio::io::DuplexStream,
        submission: impl Future,
    ) -> (Request, Request) {
        let mut peer = tokio::io::BufReader::new(peer);
        {
            let mut submission = std::pin::pin!(submission);
            assert!(futures_util::poll!(&mut submission).is_pending());
            bounded(tokio::io::AsyncBufReadExt::fill_buf(&mut peer))
                .await
                .unwrap();
        }
        let mut next = async || {
            let frame = bounded(read_frame::<_, Request>(&mut peer)).await;
            frame.unwrap().unwrap()
        };
        (next().await, next().await)
    }

    #[tokio::test]
    async fn submissions_dropped_mid_write_are_released() {
        let runtime = crate::tests::TestRuntime::new().await;
        let context = fixture_context(&runtime);
        let (connection, peer) = wired_connection(1).await;
        let call = connection.execute("read".into(), serde_json::json!({}), &context);
        let (
            Request::Tool { request_id, .. },
            Request::Cancel {
                request_id: cancelled,
            },
        ) = drop_mid_write(peer, call).await
        else {
            panic!("expected the tool request and its cancel");
        };
        assert_eq!(cancelled, request_id);

        let (connection, peer) = wired_connection(1).await;
        let open = connection.open_ssh(route(), "true".into());
        let (
            Request::Control(ControlRequest::OpenSsh { channel, .. }),
            Request::Control(ControlRequest::StreamClose { channel: closed }),
        ) = drop_mid_write(peer, open).await
        else {
            panic!("expected the stream's open and close");
        };
        assert_eq!(closed, channel);
        assert!(connection.state.lock().await.streams.is_empty());
    }

    #[tokio::test]
    async fn channels_share_request_ids() {
        let (connection, mut peer) = wired_connection(4096).await;
        let _stream = connection.open_ssh(route(), String::new()).await.unwrap();
        let (request_id, ()) = connection
            .submit(|request_id, _| (Request::Cancel { request_id }, ()))
            .await
            .unwrap();
        assert_eq!(request_id, RequestId::new(2));
        assert!(matches!(
            read_frame::<_, Request>(&mut peer).await.unwrap(),
            Some(Request::Control(ControlRequest::OpenSsh { channel, .. })) if channel == RequestId::new(1)
        ));
        assert!(matches!(read_frame::<_, Request>(&mut peer).await.unwrap(),
            Some(Request::Cancel { request_id: id }) if id == request_id));
    }

    pub(in crate::remote) fn fixture_context(runtime: &crate::tests::TestRuntime) -> ToolContext {
        fixture_context_with_capabilities(runtime, [Capability::Read].into_iter().collect())
    }

    pub(in crate::remote) fn fixture_context_with_capabilities(
        runtime: &crate::tests::TestRuntime,
        capabilities: crate::tool::policy::CapabilitySet,
    ) -> ToolContext {
        let subject = crate::tool::authorization::AuthorizationSubject {
            agent: runtime.agent.clone(),
            job: crate::identity::JobId::new(1).unwrap(),
            parent: None,
            capabilities,
            cancellation: CancellationToken::new(),
        };
        let location = crate::execution::ExecutionLocation::root(runtime.root.path().to_owned());
        ToolContext::new(
            subject,
            location.clone(),
            location,
            tokio::sync::mpsc::channel(1).1,
            runtime.jobs.clone(),
        )
        .with_invocation_authority(
            allow_all(),
            "read".into(),
            AuthorizationArguments::default(),
        )
    }

    pub(super) fn output(value: &str) -> RemoteToolResult {
        Ok(RemoteToolOutput {
            diagnostic: None,
            streams: Default::default(),
            value: serde_json::json!(value),
            images: Vec::new(),
            captures: Vec::new(),
        })
    }

    pub(in crate::remote) async fn write_result<W: tokio::io::AsyncWrite + Unpin>(
        writer: &mut W,
        request_id: RequestId,
        result: RemoteToolResult,
    ) {
        use crate::remote::protocol::{PayloadEvent, PayloadId, PayloadOpen};
        let bytes = serde_json::to_vec(&result).unwrap();
        let events =
            std::iter::once(PayloadEvent::Open(PayloadOpen::Result))
                .chain(bytes.chunks(crate::remote::flow::CHUNK_BYTES).map(|data| {
                    PayloadEvent::Data {
                        id: PayloadId::Result,
                        data: data.to_vec(),
                    }
                }))
                .chain(std::iter::once(PayloadEvent::Finish {
                    id: PayloadId::Result,
                }));
        for event in events {
            write_frame(writer, &Response::Payload { request_id, event })
                .await
                .unwrap();
        }
        write_frame(writer, &Response::Tool { request_id })
            .await
            .unwrap();
    }

    pub(super) fn route() -> crate::target::Route {
        crate::target::Route {
            hops: Vec::new(),
            destination: TargetDefinition::test("build", ".", None),
        }
    }

    pub(super) fn build() -> ExecutionLocation {
        ExecutionLocation::named("build".parse().unwrap(), "/build".into())
    }

    pub(super) async fn route_fixture<R: tokio::io::AsyncRead + Unpin>(
        output: R,
        state: &Mutex<ConnectionState>,
    ) {
        let prompts = Arc::new(crate::remote::RejectSensitivePrompts);
        route_responses(
            output,
            state,
            &sink(),
            build(),
            prompts,
            CancellationToken::new(),
            Box::new(()),
        )
        .await;
    }

    #[tokio::test]
    async fn tool_wire_preserves_the_exact_noninteractive_caller_capabilities() {
        let runtime = crate::tests::TestRuntime::new().await;
        let capabilities = [Capability::Exec, Capability::Targets]
            .into_iter()
            .collect();
        let context = fixture_context_with_capabilities(&runtime, capabilities);
        let (connection, mut peer) = wired_connection(4096).await;
        let arguments = serde_json::json!({"command":["true"]});
        let call = connection.execute("exec".into(), arguments, &context);
        let inspect =
            async {
                let Request::Tool {
                    request_id,
                    capabilities,
                    ..
                } = read_frame::<_, Request>(&mut peer).await.unwrap().unwrap()
                else {
                    panic!("expected tool request")
                };
                assert_eq!(
                    capabilities,
                    context.capabilities().iter().collect::<Vec<_>>()
                );
                assert!(!capabilities.contains(&Capability::Interactive));
                assert!(!capabilities.contains(&Capability::Read));
                let pending = connection
                    .state
                    .lock()
                    .await
                    .pending
                    .remove(&request_id)
                    .unwrap();
                let _ = pending.sender.send(Err(RemoteError::Authorization(
                    AuthorizationError::Denied("fixture".into()),
                )));
            };
        let (result, ()) = tokio::join!(call, inspect);
        assert!(matches!(result, Err(RemoteError::Authorization(_))));
    }

    /// An upload sends one credit window ahead of the worker's acknowledgements,
    /// and stops once its call has ended, returning that call's result.
    #[tokio::test]
    async fn uploads_wait_for_credit_and_stop_with_their_call() {
        use crate::remote::flow::{CHUNK_BYTES, WINDOW};
        let runtime = crate::tests::TestRuntime::new().await;
        let context = fixture_context(&runtime);
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), vec![7; CHUNK_BYTES * (WINDOW + 2)]).unwrap();
        let source = Source::open(file.path()).await.unwrap();
        let (connection, mut peer) = wired_connection(4096).await;
        let request = |request_id| Request::Tool {
            request_id,
            name: "write".into(),
            arguments: serde_json::json!({}),
            capabilities: Vec::new(),
            source: true,
        };
        let upload = call(&connection, &context, request, Some(source));
        let worker = async {
            let Some(Request::Tool { request_id, .. }) = read_frame(&mut peer).await.unwrap()
            else {
                panic!("expected tool request")
            };
            let mut next_chunk = async || {
                let frame = read_frame::<_, Request>(&mut peer).await.unwrap();
                assert!(matches!(frame, Some(Request::SourceData { .. })));
            };
            for _ in 0..WINDOW {
                next_chunk().await;
            }
            {
                let state = connection.state.lock().await;
                let credits = &state.pending[&request_id].upload.as_ref().unwrap().0;
                credits.acknowledge().unwrap();
            }
            next_chunk().await;
            let pending = connection
                .state
                .lock()
                .await
                .pending
                .remove(&request_id)
                .unwrap();
            let output = ToolOutput::new(serde_json::json!("ended"));
            let received = results::Received {
                output,
                source: None,
            };
            let _ = pending.sender.send(Ok(received));
        };
        let (received, ()) = bounded(async { tokio::join!(upload, worker) }).await;
        assert_eq!(received.unwrap().output.value, "ended");
        // The upload sent nothing more, not even its end.
        drop(connection);
        assert!(read_frame::<_, Request>(&mut peer).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn stream_submission_rejects_failed_connections_and_drains_on_write_failure() {
        let (connection, peer) = wired_connection(1).await;
        drop(peer);
        let stream = connection.open_ssh(route(), "true".into());
        let error = stream.await.err().unwrap().into_tool_error().diagnostic();
        assert_eq!(error.context.operation, Operation::Send);
        assert_eq!(error.context.site, FailureSite::Host);
        assert_eq!(error.context.effects, Effects::MayHaveExecuted);
        assert!(matches!(
            error.cause,
            crate::tool::diagnostic::Cause::Io { .. }
        ));
        // A writable pipe must not allow registration once the dispatcher has failed.
        *connection.writer.lock().await = Box::new(tokio::io::sink());
        let retry = bounded(connection.open_ssh(route(), "true".into())).await;
        let retry = retry.err().unwrap().into_tool_error().diagnostic();
        assert_eq!(retry.context.operation, Operation::Send);
        assert_eq!(retry.context.site, FailureSite::Host);
        assert_eq!(retry.context.effects, Effects::NotStarted);
        assert!(connection.state.lock().await.streams.is_empty());
    }

    #[tokio::test]
    async fn failed_connections_end_relayed_streams_with_their_failure() {
        use tokio::io::AsyncReadExt as _;
        let (connection, _peer) = wired_connection(4096).await;
        let mut stream = connection.open_ssh(route(), "true".into()).await.unwrap();
        let failure = ProtocolError::Violation("connection failed").into();
        fail_connection(&connection.state, failure).await;
        let error = bounded(stream.output.read_to_end(&mut Vec::new())).await;
        assert!(matches!(
            RemoteError::from(error.unwrap_err()),
            RemoteError::Protocol(ProtocolError::Violation("connection failed"))
        ));
    }
}
