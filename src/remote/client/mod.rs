//! Shim protocol client, response routing, and relayed SSH streams.
use std::{collections::HashMap, sync::Arc};

use tokio::sync::{Mutex, OwnedMutexGuard, oneshot};

use crate::{
    execution::ExecutionLocation,
    job::CancellationToken,
    remote::SensitivePromptHandler,
    target::{TargetDefinition, TargetName},
    tool::{
        ToolContext, ToolOutput,
        authorization::AuthorizationCoordinator,
        diagnostic::{Effects, FailureSite, Operation, PartialContext, Subject},
        source::Source,
    },
};

use crate::remote::{
    ProtocolError,
    flow::{CHUNK_BYTES, Credits},
    manager::RemoteError,
    protocol::{
        PromptId, RemoteToolResult, Request, RequestId, Response, read_frame, spawn_owned_write,
        write_frame,
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
    writer: Arc<Mutex<RequestWriter>>,
    state: Arc<Mutex<ConnectionState>>,
    shutdown: CancellationToken,
}

struct RequestWriter {
    input: crate::remote::transport::Writer,
    next_request_id: Option<RequestId>,
}

#[cfg(test)]
use crate::remote::protocol::{RemoteToolError, RemoteToolOutput};
type PendingResult = Result<results::ReceivedResult, RemoteError>;

struct PendingCall {
    sender: oneshot::Sender<PendingResult>,
    context: ToolContext,
    // Host-owned tools retain their ownership context while invoking a remote worker.
    destination: ExecutionLocation,
    upload: Option<UploadCredits>,
}

/// Credits for a call's source upload, returned by the worker's `SourceAck`s.
/// Dropping them with the call releases an upload waiting for credit.
struct UploadCredits(Credits);

impl Drop for UploadCredits {
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

impl PooledConnection {
    pub(in crate::remote) async fn is_failed(&self) -> bool {
        self.state.lock().await.failure.is_some()
    }

    /// Serialize registration and sending so every RPC observes the same failure state.
    async fn submit<T>(
        &self,
        register: impl FnOnce(RequestId, &mut ConnectionState) -> (Request, T),
    ) -> Result<(RequestId, T), RemoteError> {
        let mut writer = self.writer.clone().lock_owned().await;
        let mut state = self.state.lock().await;
        if let Some(failure) = &state.failure {
            // This attempt never registered, even if earlier calls may have executed.
            return Err(not_started(failure.clone()));
        }
        let Some(request_id) = writer.next_request_id else {
            drop(state);
            drop(writer);
            let failure = transport_error(
                ProtocolError::Violation("request ID space exhausted"),
                Operation::Send,
            );
            fail_connection(&self.state, failure.clone()).await;
            return Err(not_started(failure));
        };
        writer.next_request_id = request_id.next();
        let (request, result) = register(request_id, &mut state);
        drop(state);
        self.write(writer, request).await?;
        Ok((request_id, result))
    }

    /// Once registered, the frame is finished even if this await is dropped;
    /// a failed write quarantines the connection.
    async fn write(
        &self,
        writer: OwnedMutexGuard<RequestWriter>,
        request: Request,
    ) -> Result<(), RemoteError> {
        write_request(writer, &self.state, request).await
    }

    pub(in crate::remote) async fn from_transport(
        transport: crate::remote::transport::Transport,
        target: &TargetName,
        authorization: AuthorizationCoordinator,
        prompts: Arc<dyn SensitivePromptHandler>,
        tasks: &tokio_util::task::TaskTracker,
        shutdown: CancellationToken,
    ) -> Result<Self, RemoteError> {
        let crate::remote::transport::Transport {
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
        let writer = Arc::new(Mutex::new(RequestWriter {
            input,
            next_request_id: Some(RequestId::FIRST),
        }));
        let reader_state = state.clone();
        let reader_writer = writer.clone();
        let reader_target = target.clone();
        let reader_shutdown = shutdown.clone();
        tasks.spawn(async move {
            route_responses(
                output,
                &reader_state,
                (&reader_writer, &authorization),
                reader_target,
                prompts,
                reader_shutdown,
                owner,
            )
            .await;
        });
        Ok(PooledConnection {
            writer,
            state,
            shutdown,
        })
    }
}

async fn call_tool(
    connection: &PooledConnection,
    name: String,
    arguments: serde_json::Value,
    context: &ToolContext,
    destination: ExecutionLocation,
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
    let received = call(connection, context, destination, request, source).await?;
    if received.source.is_some() {
        return Err(ProtocolError::Violation("source contents for a tool call").into());
    }
    Ok(received.output)
}

/// Submit one request, stream its source, and await its result, cancelling it
/// with the call.
async fn call(
    connection: &PooledConnection,
    context: &ToolContext,
    destination: ExecutionLocation,
    request: impl FnOnce(RequestId) -> Request + Send,
    source: Option<Source>,
) -> Result<results::Received, RemoteError> {
    if context.is_cancelled() {
        return Err(RemoteError::Cancelled);
    }
    let credits = source.as_ref().map(|_| Credits::default());
    let upload = credits.clone().map(UploadCredits);
    let (request_id, receiver) = connection
        .submit(move |request_id, state| {
            let (sender, receiver) = oneshot::channel();
            state.pending.insert(
                request_id,
                PendingCall {
                    sender,
                    context: context.clone(),
                    destination,
                    upload,
                },
            );
            (request(request_id), receiver)
        })
        .await?;
    let mut unfinished = CancelOnDrop::new(connection, request_id);
    if let Some((source, credits)) = source.zip(credits) {
        let uploaded = tokio::select! {
            result = send_source(connection, request_id, source, credits) => result,
            () = context.cancelled() => Err(RemoteError::Cancelled),
        };
        // The worker holds the call until its source ends; release it. The
        // upload's own failure is the one to report.
        if let Err(failure) = uploaded {
            unfinished.defuse();
            let _ = send_cancel(connection, request_id).await;
            return Err(failure);
        }
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
    tokio::pin!(received);
    let result = tokio::select! {
        result = &mut received => {
            unfinished.defuse();
            result?
        }
        () = context.cancelled() => {
            unfinished.defuse();
            send_cancel(connection, request_id).await?;
            return Err(RemoteError::Cancelled);
        }
    };
    result.0
}

/// Send a source's contents in credited chunks; the worker starts the call at
/// the end. Closed credits mean the call already ended, so its result follows.
async fn send_source(
    connection: &PooledConnection,
    request_id: RequestId,
    source: Source,
    credits: Credits,
) -> Result<(), RemoteError> {
    use tokio::io::AsyncReadExt as _;
    let mut reader = tokio::fs::File::from_std(source.reader().map_err(host_source_error)?);
    let mut buffer = vec![0; CHUNK_BYTES];
    loop {
        let read = reader.read(&mut buffer).await.map_err(host_source_error)?;
        if read > 0 && credits.take().await.is_err() {
            return Ok(());
        }
        let request = if read == 0 {
            Request::SourceEnd { request_id }
        } else {
            Request::SourceData {
                request_id,
                data: buffer[..read].to_vec(),
            }
        };
        let writer = connection.writer.clone().lock_owned().await;
        connection.write(writer, request).await?;
        if read == 0 {
            return Ok(());
        }
    }
}

fn host_source_error(error: std::io::Error) -> RemoteError {
    RemoteError::from(error).or(PartialContext::new(
        Operation::Read,
        Subject::Label("source".into()),
    )
    .at(FailureSite::Host)
    .effects(Effects::NotStarted))
}

async fn send_cancel(
    connection: &PooledConnection,
    request_id: RequestId,
) -> Result<(), RemoteError> {
    let writer = connection.writer.clone().lock_owned().await;
    connection
        .write(writer, Request::Cancel { request_id })
        .await
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

async fn write_request(
    writer: OwnedMutexGuard<RequestWriter>,
    state: &Mutex<ConnectionState>,
    request: Request,
) -> Result<(), RemoteError> {
    let writer = OwnedMutexGuard::map(writer, |writer| &mut writer.input);
    if let Err(error) = spawn_owned_write(writer, request).await {
        let failure = transport_error(error, Operation::Send);
        fail_connection(state, failure.clone()).await;
        return Err(failure.or(PartialContext::default().effects(Effects::MayHaveExecuted)));
    }
    Ok(())
}

/// Cancels a submitted call that is dropped before its terminal result, so the
/// worker always releases it (and any source upload it is holding).
struct CancelOnDrop {
    writer: Arc<Mutex<RequestWriter>>,
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

    /// The call reached its terminal result or sent its own Cancel.
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
        let failure = state.failure.get_or_insert(failure).clone();
        state.streams.clear();
        (failure, std::mem::take(&mut state.pending))
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
        destination: ExecutionLocation,
    ) -> Result<ToolOutput, RemoteError> {
        call_tool(self, name, arguments, context, destination).await
    }

    /// Read a source file on this connection's machine into a local spool.
    pub(in crate::remote) async fn read_source(
        &self,
        tool: String,
        path: String,
        context: &ToolContext,
        destination: ExecutionLocation,
    ) -> Result<Source, RemoteError> {
        let capabilities = context.capabilities().iter().collect();
        let request = move |request_id| Request::ReadSource {
            request_id,
            tool,
            path,
            capabilities,
        };
        call(self, context, destination, request, None)
            .await?
            .source
            .ok_or_else(|| ProtocolError::Violation("source read without contents").into())
    }

    pub(in crate::remote) async fn open_ssh(
        self: Arc<Self>,
        route: Vec<TargetDefinition>,
        command: String,
    ) -> Result<crate::remote::transport::Transport, RemoteError> {
        self.open_stream(route, command).await
    }
}

#[cfg(test)]
pub(in crate::remote) mod tests {
    use super::*;
    use crate::job::CancellationToken;
    use crate::tool::{
        authorization::AuthorizationError,
        policy::{AllowAll, Capability},
    };

    /// A live fake shim transport for manager/router tests, including the real handshake.
    pub(crate) fn test_transport(
        mut reply: Option<crate::tool::invocation::LocalError>,
    ) -> crate::remote::transport::Transport {
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
        crate::remote::transport::Transport {
            input: Box::new(input),
            output: Box::new(output),
            owner: Box::new(owner),
        }
    }

    pub(super) async fn test_connection() -> PooledConnection {
        PooledConnection {
            writer: Arc::new(Mutex::new(RequestWriter {
                input: Box::new(tokio::io::sink()),
                next_request_id: Some(RequestId::FIRST),
            })),
            state: Arc::new(Mutex::new(ConnectionState::default())),
            shutdown: CancellationToken::new(),
        }
    }

    /// A test connection whose request writer is the returned in-memory peer.
    async fn wired_connection(buffer: usize) -> (Arc<PooledConnection>, tokio::io::DuplexStream) {
        let connection = test_connection().await;
        let (input, peer) = tokio::io::duplex(buffer);
        connection.writer.lock().await.input = Box::new(input);
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
        let connection = test_connection().await;
        let result = connection.execute(
            "read".into(),
            serde_json::json!({}),
            &context,
            context.execution_location().clone(),
        );
        assert!(matches!(result.await, Err(RemoteError::Cancelled)));
        assert!(connection.state.lock().await.pending.is_empty());
        let next = connection.writer.lock().await.next_request_id;
        assert_eq!(next, Some(RequestId::FIRST));
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
                let destination = context.execution_location().clone();
                connection
                    .execute("read".into(), serde_json::json!({}), &context, destination)
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
        let cancel = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            read_frame::<_, Request>(&mut peer),
        )
        .await
        .expect("dropped call sent no cancel")
        .unwrap();
        assert!(matches!(cancel, Some(Request::Cancel { request_id: id }) if id == request_id));
    }

    #[tokio::test]
    async fn channels_share_request_ids() {
        let (connection, mut peer) = wired_connection(4096).await;
        let _stream = connection
            .open_stream(Vec::new(), String::new())
            .await
            .unwrap();
        let (request_id, ()) = connection
            .submit(|request_id, _| (Request::Cancel { request_id }, ()))
            .await
            .unwrap();
        assert_eq!(request_id.get(), 2);
        assert!(matches!(
            read_frame::<_, Request>(&mut peer).await.unwrap(),
            Some(Request::OpenSsh {
                channel: RequestId::FIRST,
                ..
            })
        ));
        assert!(matches!(read_frame::<_, Request>(&mut peer).await.unwrap(),
            Some(Request::Cancel { request_id: id }) if id == request_id));
    }

    pub(in crate::remote) fn fixture_context(runtime: &crate::tests::TestRuntime) -> ToolContext {
        fixture_context_with_capabilities(runtime, [Capability::Read].into_iter().collect())
    }

    fn fixture_context_with_capabilities(
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
        .with_invocation_authority(allow_all(), "read".into(), serde_json::json!({}))
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

    pub(super) async fn route_fixture<R: tokio::io::AsyncRead + Unpin>(
        output: R,
        state: &Mutex<ConnectionState>,
        target: &str,
    ) {
        let writer = Arc::new(Mutex::new(RequestWriter {
            input: Box::new(tokio::io::sink()),
            next_request_id: Some(RequestId::FIRST),
        }));
        let prompts = Arc::new(crate::remote::RejectSensitivePrompts);
        route_responses(
            output,
            state,
            (&writer, &allow_all()),
            target.parse().unwrap(),
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
        let call = call_tool(
            &connection,
            "exec".into(),
            arguments,
            &context,
            context.execution_location().clone(),
        );
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
        use crate::remote::flow::WINDOW;
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
        let destination = context.execution_location().clone();
        let upload = call(&connection, &context, destination, request, Some(source));
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
            let _ = pending
                .sender
                .send(Ok(results::ReceivedResult(Ok(received))));
        };
        let (received, ()) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::join!(upload, worker)
        })
        .await
        .unwrap();
        assert_eq!(received.unwrap().output.value, "ended");
        // The upload sent nothing more, not even its end.
        drop(connection);
        assert!(read_frame::<_, Request>(&mut peer).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn stream_submission_rejects_failed_connections_and_drains_on_write_failure() {
        let (connection, peer) = wired_connection(1).await;
        drop(peer);
        let target = TargetDefinition::test("build", ".", None);
        let stream = connection.open_stream(vec![target.clone()], "true".into());
        let error = stream.await.err().unwrap().into_tool_error().diagnostic();
        assert_eq!(error.context.operation, Operation::Send);
        assert_eq!(error.context.site, FailureSite::Host);
        assert_eq!(error.context.effects, Effects::MayHaveExecuted);
        assert!(matches!(
            error.cause,
            crate::tool::diagnostic::Cause::Io { .. }
        ));
        // A writable pipe must not allow registration once the dispatcher has failed.
        connection.writer.lock().await.input = Box::new(tokio::io::sink());
        let retry = connection.open_stream(vec![target], "true".into());
        let retry = tokio::time::timeout(std::time::Duration::from_secs(2), retry).await;
        let retry = retry.unwrap().err().unwrap().into_tool_error().diagnostic();
        assert_eq!(retry.context.operation, Operation::Send);
        assert_eq!(retry.context.site, FailureSite::Host);
        assert_eq!(retry.context.effects, Effects::NotStarted);
        assert!(connection.state.lock().await.streams.is_empty());
    }
}
