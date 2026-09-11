//! Shim protocol client, response routing, and relayed SSH streams.
use std::{collections::HashMap, sync::Arc};

use tokio::{
    sync::{Mutex, oneshot},
    task::JoinHandle,
};

use crate::{
    remote::SensitivePromptHandler,
    target::TargetDefinition,
    tool::{ToolContext, ToolOutput, authorization::AuthorizationCoordinator},
};

use super::{
    manager::RemoteError,
    protocol::{
        PROTOCOL_VERSION, RemoteToolError, RemoteToolOutput, Request, Response, read_frame,
        write_frame,
    },
};

pub(super) type Session = Arc<PooledConnection>;

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
    next_request_id: u64,
}

type RemoteToolResult = Result<RemoteToolOutput, RemoteToolError>;
type PendingResult = Result<RemoteToolResult, RemoteError>;

struct PendingCall {
    sender: oneshot::Sender<PendingResult>,
    context: ToolContext,
}

#[derive(Default)]
struct ConnectionState {
    pending: HashMap<u64, PendingCall>,
    failure: Option<RemoteError>,
    resolutions: HashMap<
        u64,
        oneshot::Sender<Result<crate::remote::backends::ssh::ResolvedSsh, RemoteError>>,
    >,
    streams: HashMap<u64, ClientStream>,
}

impl PooledConnection {
    /// Serialize registration and sending so every RPC observes the same failure state.
    async fn submit<T>(
        &self,
        register: impl FnOnce(u64, &mut ConnectionState) -> (Request, T),
    ) -> Result<(u64, T), RemoteError> {
        let mut writer = self.writer.lock().await;
        let mut state = self.state.lock().await;
        if let Some(failure) = &state.failure {
            return Err(failure.clone());
        }
        let request_id = writer.next_request_id;
        let Some(next) = request_id.checked_add(1) else {
            drop(state);
            drop(writer);
            let failure = RemoteError::Protocol("request ID space exhausted".into());
            fail_connection(&self.state, failure.clone()).await;
            return Err(failure);
        };
        writer.next_request_id = next;
        let (request, result) = register(request_id, &mut state);
        drop(state);
        if let Err(error) = write_frame(&mut writer.input, &request).await {
            drop(writer);
            let failure = RemoteError::io(error);
            fail_connection(&self.state, failure.clone()).await;
            return Err(failure);
        }
        Ok((request_id, result))
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
        write_frame(
            &mut input,
            &Request::Hello {
                version: PROTOCOL_VERSION,
            },
        )
        .await?;
        if !matches!(
            read_frame::<_, Response>(&mut output).await?,
            Some(Response::Ready {
                version: PROTOCOL_VERSION
            })
        ) {
            return Err(RemoteError::Protocol("invalid shim handshake".into()));
        }
        let state = Arc::new(Mutex::new(ConnectionState::default()));
        let writer = Arc::new(Mutex::new(RequestWriter {
            input,
            next_request_id: 1,
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
                    capabilities: context.capabilities.iter().collect(),
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
            return Err(RemoteError::Remote {
                message: "tool was cancelled".to_owned(),
                output: None,
            });
        }
    };
    match result {
        Ok(output) => Ok(output.into()),
        Err(error) if error.denial.is_some() => Err(RemoteError::OperationDenied(error.message)),
        Err(error) => Err(RemoteError::Remote {
            message: error.message,
            output: error.output.map(|output| Box::new((*output).into())),
        }),
    }
}

async fn send_cancel(connection: &PooledConnection, request_id: u64) -> Result<(), RemoteError> {
    let result = {
        let mut writer = connection.writer.lock().await;
        write_frame(&mut writer.input, &Request::Cancel { request_id }).await
    };
    if let Err(error) = result {
        let failure = RemoteError::io(error);
        fail_connection(&connection.state, failure.clone()).await;
        return Err(failure);
    }
    Ok(())
}

async fn fail_connection(state: &Mutex<ConnectionState>, failure: RemoteError) {
    let pending = {
        let mut state = state.lock().await;
        if state.failure.is_some() {
            return;
        }
        state.failure = Some(failure.clone());
        for (_, sender) in state.resolutions.drain() {
            let _ = sender.send(Err(failure.clone()));
        }
        state.streams.clear();
        std::mem::take(&mut state.pending)
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
    pub(in crate::remote) async fn resolve_ssh(
        &self,
        target: TargetDefinition,
    ) -> Result<crate::remote::backends::ssh::ResolvedSsh, RemoteError> {
        let (_, receiver) = self
            .submit(move |request_id, state| {
                let (sender, receiver) = oneshot::channel();
                state.resolutions.insert(request_id, sender);
                (
                    Request::ResolveSsh {
                        request_id,
                        target: Box::new(target),
                    },
                    receiver,
                )
            })
            .await?;
        receiver
            .await
            .map_err(|_| RemoteError::Protocol("configuration channel closed".into()))?
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
    use crate::tool::policy::Capability;
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
            if !matches!(
                read_frame::<_, Request>(&mut shim).await,
                Ok(Some(Request::Hello {
                    version: PROTOCOL_VERSION
                }))
            ) {
                return;
            }
            if write_frame(
                &mut shim,
                &Response::Ready {
                    version: PROTOCOL_VERSION,
                },
            )
            .await
            .is_err()
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
                next_request_id: 1,
            })),
            state: Arc::new(Mutex::new(ConnectionState::default())),
            _owner: std::sync::Mutex::new(Box::new(())),
            reader: tokio::spawn(std::future::pending()),
        }
    }

    pub(super) fn fixture_context(runtime: &crate::tests::TestRuntime) -> ToolContext {
        let mut capabilities = crate::tool::policy::CapabilitySet::default();
        capabilities.insert(Capability::Read);
        let subject = crate::tool::authorization::AuthorizationSubject {
            agent: runtime.agent.clone(),
            job: crate::identity::JobId::new(1).unwrap(),
            parent: None,
            scope: None,
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
    }
    pub(super) fn output(value: &str) -> RemoteToolResult {
        Ok(RemoteToolOutput {
            value: serde_json::json!(value),
            images: Vec::new(),
        })
    }

    pub(super) async fn route_fixture<R: tokio::io::AsyncRead + Unpin>(
        output: R,
        state: &Mutex<ConnectionState>,
        target: &str,
    ) {
        let writer = Arc::new(Mutex::new(RequestWriter {
            input: Box::new(tokio::io::sink()),
            next_request_id: 1,
        }));
        let authorization = AuthorizationCoordinator::new(Arc::new(crate::tool::policy::AllowAll));
        route_responses(
            output,
            state,
            (&writer, &authorization),
            target.into(),
            Arc::new(crate::remote::RejectSensitivePrompts),
        )
        .await;
    }

    #[tokio::test]
    async fn tool_wire_preserves_the_exact_noninteractive_caller_capabilities() {
        let runtime = crate::tests::TestRuntime::new().await;
        let mut context = fixture_context(&runtime);
        context.capabilities = [Capability::Exec, Capability::Targets]
            .into_iter()
            .collect();
        let (input, mut peer) = tokio::io::duplex(4096);
        let connection = test_connection().await;
        connection.writer.lock().await.input = Box::new(input);
        let call = call_tool(
            &connection,
            "exec".into(),
            serde_json::json!({"argv":["true"]}),
            &context,
        );
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
                context.capabilities.iter().collect::<Vec<_>>()
            );
            assert!(!capabilities.contains(&Capability::Interactive));
            assert!(!capabilities.contains(&Capability::Read));
            connection
                .state
                .lock()
                .await
                .pending
                .remove(&request_id)
                .unwrap()
                .sender
                .send(Ok(output("done")))
                .unwrap();
        };
        let (result, ()) = tokio::join!(call, inspect);
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn incompatible_worker_handshakes_are_rejected() {
        for reply in [
            serde_json::json!({"type":"ready"}),
            serde_json::json!({"type":"ready", "version": PROTOCOL_VERSION - 1}),
        ] {
            let (client, mut server) = tokio::io::duplex(4096);
            let peer = tokio::spawn(async move {
                assert!(matches!(
                    read_frame::<_, Request>(&mut server).await.unwrap(),
                    Some(Request::Hello {
                        version: PROTOCOL_VERSION
                    })
                ));
                write_frame(&mut server, &reply).await.unwrap();
            });
            let (output, input) = tokio::io::split(client);
            let result = PooledConnection::from_transport(
                crate::remote::transport::Transport {
                    input: Box::new(input),
                    output: Box::new(output),
                    owner: Box::new(()),
                },
                "test",
                AuthorizationCoordinator::new(Arc::new(crate::tool::policy::AllowAll)),
                Arc::new(crate::remote::RejectSensitivePrompts),
            )
            .await;
            assert!(result.is_err());
            peer.await.unwrap();
        }
    }

    #[tokio::test]
    async fn control_submission_rejects_failed_connections_and_drains_on_write_failure() {
        for resolution in [true, false] {
            let connection = Arc::new(test_connection().await);
            let (input, peer) = tokio::io::duplex(1);
            drop(peer);
            connection.writer.lock().await.input = Box::new(input);
            let (sender, receiver) = oneshot::channel();
            connection.state.lock().await.resolutions.insert(99, sender);
            let target = TargetDefinition::test("build", ".", None);
            let result = if resolution {
                connection.resolve_ssh(target.clone()).await.map(|_| ())
            } else {
                connection
                    .open_stream(vec![target.clone()], "true".into())
                    .await
                    .map(|_| ())
            };
            assert!(matches!(result, Err(RemoteError::Io { .. })));
            assert!(receiver.await.unwrap().is_err());
            // A writable pipe must not allow registration once the dispatcher has failed.
            connection.writer.lock().await.input = Box::new(tokio::io::sink());
            let retry = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                connection.resolve_ssh(target),
            )
            .await
            .unwrap();
            assert!(retry.is_err());
            let state = connection.state.lock().await;
            assert!(state.resolutions.is_empty());
            assert!(state.streams.is_empty());
        }
    }
}
