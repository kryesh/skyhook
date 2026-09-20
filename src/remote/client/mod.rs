//! Shim protocol client, response routing, and relayed SSH streams.
use std::{collections::HashMap, sync::Arc};

use tokio::{
    sync::{Mutex, OwnedMutexGuard, oneshot},
    task::JoinHandle,
};

use crate::{
    remote::SensitivePromptHandler,
    target::TargetDefinition,
    tool::{ToolContext, ToolOutput, authorization::AuthorizationCoordinator},
};

use crate::remote::{
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
    _owner: std::sync::Mutex<Box<dyn Send>>,
    reader: JoinHandle<()>,
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
}

#[derive(Default)]
struct ConnectionState {
    pending: HashMap<RequestId, PendingCall>,
    failure: Option<RemoteError>,
    streams: HashMap<RequestId, ClientStream>,
}

impl PooledConnection {
    /// Serialize registration and sending so every RPC observes the same failure state.
    async fn submit<T>(
        &self,
        register: impl FnOnce(RequestId, &mut ConnectionState) -> (Request, T),
    ) -> Result<(RequestId, T), RemoteError> {
        let mut writer = self.writer.clone().lock_owned().await;
        let mut state = self.state.lock().await;
        if let Some(failure) = &state.failure {
            return Err(failure.clone());
        }
        let Some(request_id) = writer.next_request_id else {
            drop(state);
            drop(writer);
            let failure = RemoteError::Protocol("request ID space exhausted".into());
            fail_connection(&self.state, failure.clone()).await;
            return Err(failure);
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
        let writer = OwnedMutexGuard::map(writer, |writer| &mut writer.input);
        if let Err(error) = spawn_owned_write(writer, request).await {
            let failure = RemoteError::io(error);
            fail_connection(&self.state, failure.clone()).await;
            return Err(failure);
        }
        Ok(())
    }

    pub(in crate::remote) async fn from_transport(
        transport: crate::remote::transport::Transport,
        target: &str,
        authorization: AuthorizationCoordinator,
        prompts: Arc<dyn SensitivePromptHandler>,
    ) -> Result<Self, RemoteError> {
        let crate::remote::transport::Transport {
            mut input,
            mut output,
            owner,
        } = transport;
        write_frame(&mut input, &Request::Hello).await?;
        if !matches!(
            read_frame::<_, Response>(&mut output).await?,
            Some(Response::Ready)
        ) {
            return Err(RemoteError::Protocol("invalid shim handshake".into()));
        }
        let state = Arc::new(Mutex::new(ConnectionState::default()));
        let writer = Arc::new(Mutex::new(RequestWriter {
            input,
            next_request_id: Some(RequestId::FIRST),
        }));
        let reader_state = state.clone();
        let reader_writer = writer.clone();
        let reader_target = target.to_owned();
        let reader = tokio::spawn(async move {
            route_responses(
                output,
                &reader_state,
                (&reader_writer, &authorization),
                reader_target,
                prompts,
            )
            .await;
        });
        Ok(PooledConnection {
            writer,
            state,
            _owner: std::sync::Mutex::new(owner),
            reader,
        })
    }
}

async fn call_tool(
    connection: &PooledConnection,
    name: String,
    arguments: serde_json::Value,
    context: &ToolContext,
) -> Result<ToolOutput, RemoteError> {
    if context.is_cancelled() {
        return Err(RemoteError::Cancelled);
    }
    let (request_id, receiver) = connection
        .submit(move |request_id, state| {
            let (sender, receiver) = oneshot::channel();
            state.pending.insert(
                request_id,
                PendingCall {
                    sender,
                    context: context.clone(),
                },
            );
            (
                Request::Tool {
                    request_id,
                    name,
                    arguments,
                    capabilities: context.capabilities().iter().collect(),
                },
                receiver,
            )
        })
        .await?;
    let received = async {
        receiver.await.map_err(|_| {
            RemoteError::Protocol("remote response dispatcher stopped unexpectedly".to_owned())
        })?
    };
    tokio::pin!(received);
    let result = tokio::select! {
        result = &mut received => result?,
        () = context.cancelled() => {
            send_cancel(connection, request_id).await?;
            return Err(RemoteError::Cancelled);
        }
    };
    result.0
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

async fn fail_connection(state: &Mutex<ConnectionState>, failure: RemoteError) {
    let (failure, pending) = {
        let mut state = state.lock().await;
        let failure = state.failure.get_or_insert(failure).clone();
        state.streams.clear();
        (failure, std::mem::take(&mut state.pending))
    };
    for pending in pending.into_values() {
        let _ = pending.sender.send(Err(failure.clone()));
    }
}

impl Drop for PooledConnection {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

impl PooledConnection {
    pub(in crate::remote) async fn execute(
        &self,
        name: String,
        arguments: serde_json::Value,
        context: &ToolContext,
    ) -> Result<ToolOutput, RemoteError> {
        call_tool(self, name, arguments, context).await
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
mod tests {
    use super::*;
    use crate::job::CancellationToken;
    use crate::tool::policy::{AllowAll, Capability};

    /// A live fake shim transport for manager/router tests, including the real handshake.
    pub(crate) fn test_transport() -> crate::remote::transport::Transport {
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
            while let Ok(Some(_)) = read_frame::<_, Request>(&mut shim).await {}
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
            _owner: std::sync::Mutex::new(Box::new(())),
            reader: tokio::spawn(std::future::pending()),
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
        let result = connection.execute("read".into(), serde_json::json!({}), &context);
        assert!(matches!(result.await, Err(RemoteError::Cancelled)));
        assert!(connection.state.lock().await.pending.is_empty());
        let next = connection.writer.lock().await.next_request_id;
        assert_eq!(next, Some(RequestId::FIRST));
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

    pub(super) fn fixture_context(runtime: &crate::tests::TestRuntime) -> ToolContext {
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
            streams: Default::default(),
            value: serde_json::json!(value),
            images: Vec::new(),
            captures: Vec::new(),
        })
    }

    pub(super) async fn write_result<W: tokio::io::AsyncWrite + Unpin>(
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
            target.into(),
            prompts,
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
        let arguments = serde_json::json!({"argv":["true"]});
        let call = call_tool(&connection, "exec".into(), arguments, &context);
        let inspect = async {
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
            let _ = pending
                .sender
                .send(Err(RemoteError::OperationDenied("fixture".into())));
        };
        let (result, ()) = tokio::join!(call, inspect);
        assert!(matches!(result, Err(RemoteError::OperationDenied(_))));
    }

    #[tokio::test]
    async fn stream_submission_rejects_failed_connections_and_drains_on_write_failure() {
        let (connection, peer) = wired_connection(1).await;
        drop(peer);
        let target = TargetDefinition::test("build", ".", None);
        let stream = connection.open_stream(vec![target.clone()], "true".into());
        assert!(matches!(
            stream.await.map(drop),
            Err(RemoteError::Io { .. })
        ));
        // A writable pipe must not allow registration once the dispatcher has failed.
        connection.writer.lock().await.input = Box::new(tokio::io::sink());
        let retry = connection.open_stream(vec![target], "true".into());
        let retry = tokio::time::timeout(std::time::Duration::from_secs(2), retry).await;
        assert!(retry.unwrap().is_err());
        assert!(connection.state.lock().await.streams.is_empty());
    }
}
