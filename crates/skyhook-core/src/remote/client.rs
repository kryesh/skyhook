//! Shim protocol client, response routing, and relayed SSH streams.
use std::{collections::HashMap, sync::Arc};

use tokio::{
    io::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _},
    sync::{Mutex, oneshot},
    task::JoinHandle,
};

use crate::{
    remote::SensitivePromptHandler,
    target::TargetDefinition,
    tool::{
        ToolContext, ToolOutput,
        authorization::{AuthorizationCoordinator, AuthorizationError},
        policy::PermissionUse,
    },
};

use super::{
    manager::RemoteError,
    protocol::{RemoteToolError, RemoteToolOutput, Request, Response, read_frame, write_frame},
};

pub(super) type Session = Arc<PooledConnection>;

/// A live fake shim transport for manager/router tests, including the real handshake.
#[cfg(test)]
pub(crate) fn test_transport() -> super::transport::Transport {
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
            Ok(Some(Request::Hello))
        ) {
            return;
        }
        if write_frame(&mut shim, &Response::Ready).await.is_err() {
            return;
        }
        while let Ok(Some(_)) = read_frame::<_, Request>(&mut shim).await {}
    }));
    let (output, input) = tokio::io::split(client);
    super::transport::Transport {
        input: Box::new(input),
        output: Box::new(output),
        owner: Box::new(owner),
    }
}

pub(crate) struct PooledConnection {
    writer: Arc<Mutex<RequestWriter>>,
    state: Arc<Mutex<ConnectionState>>,
    _owner: std::sync::Mutex<Box<dyn Send>>,
    reader: JoinHandle<()>,
}

struct RequestWriter {
    input: super::transport::Writer,
    next_request_id: u64,
}

type RemoteToolResult = Result<RemoteToolOutput, RemoteToolError>;
type PendingResult = Result<RemoteToolResult, RemoteError>;

struct PendingCall {
    sender: oneshot::Sender<PendingResult>,
    context: ToolContext,
}

struct ClientStream {
    output: tokio::sync::mpsc::Sender<Result<Vec<u8>, RemoteError>>,
    credit: Arc<tokio::sync::Semaphore>,
}
impl Drop for ClientStream {
    fn drop(&mut self) {
        self.credit.close();
    }
}

#[derive(Default)]
struct ConnectionState {
    pending: HashMap<u64, PendingCall>,
    failure: Option<RemoteError>,
    resolutions:
        HashMap<u64, oneshot::Sender<Result<super::backends::ssh::ResolvedSsh, RemoteError>>>,
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

    pub(super) async fn from_transport(
        transport: super::transport::Transport,
        target: &str,
        authorization: AuthorizationCoordinator,
        prompts: Arc<dyn SensitivePromptHandler>,
    ) -> Result<Self, RemoteError> {
        let super::transport::Transport {
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

async fn route_responses<R>(
    mut output: R,
    state: &Mutex<ConnectionState>,
    host: (&Arc<Mutex<RequestWriter>>, &AuthorizationCoordinator),
    target: String,
    prompts: Arc<dyn SensitivePromptHandler>,
) where
    R: AsyncRead + Unpin,
{
    let mut callbacks = tokio::task::JoinSet::<Result<Option<u64>, RemoteError>>::new();
    let mut prompt_tasks = HashMap::<u64, tokio::task::AbortHandle>::new();
    let mut transfers = HashMap::<u64, (std::fs::File, u64)>::new();
    let mut artifacts = CapturedArtifacts::new();
    loop {
        // Keep the frame read alive while servicing callbacks: read_exact is not
        // cancellation-safe, so restarting it could lose part of a frame.
        let response = {
            let reading = read_frame::<_, Response>(&mut output);
            tokio::pin!(reading);
            loop {
                tokio::select! {
                    response = &mut reading => break response,
                    completed = callbacks.join_next(), if !callbacks.is_empty() => {
                        match completed {
                            Some(Ok(Ok(Some(id)))) => { prompt_tasks.remove(&id); }
                            Some(Ok(Ok(None))) => {}
                            Some(Err(error)) if error.is_cancelled() => {}
                            Some(Ok(Err(error))) => {
                                fail_connection(state, error).await;
                                return;
                            }
                            Some(Err(error)) => {
                                fail_connection(state, RemoteError::ConnectionTask(error.to_string())).await;
                                return;
                            }
                            None => unreachable!("nonempty callback set"),
                        }
                    }
                }
            }
        };
        let response = match response {
            Ok(Some(response)) => response,
            Ok(None) => {
                fail_connection(
                    state,
                    RemoteError::Protocol("shim closed before replying".to_owned()),
                )
                .await;
                return;
            }
            Err(error) => {
                fail_connection(state, RemoteError::io(error)).await;
                return;
            }
        };
        match response {
            Response::ToolArtifact {
                request_id,
                field,
                offset,
                data,
                finished,
            } => {
                let context = state
                    .lock()
                    .await
                    .pending
                    .get(&request_id)
                    .map(|pending| pending.context.clone());
                let Some(context) = context else {
                    fail_connection(
                        state,
                        RemoteError::Protocol("artifact for unknown request".into()),
                    )
                    .await;
                    return;
                };
                if !(field == "/error" || field == "/result" || field.starts_with("/result/"))
                    || data.len() > 64 * 1024
                    || (finished && !data.is_empty())
                {
                    fail_connection(
                        state,
                        RemoteError::Protocol("invalid artifact frame".into()),
                    )
                    .await;
                    return;
                }
                let key = (request_id, field.clone());
                let received = async {
                    use tokio::io::AsyncWriteExt as _;
                    if let std::collections::hash_map::Entry::Vacant(entry) =
                        artifacts.entry(key.clone())
                    {
                        if offset != 0 {
                            return Err(std::io::Error::other("invalid initial artifact offset"));
                        }
                        let path = context
                            .capture_path(&field)
                            .await
                            .map_err(|e| std::io::Error::other(e.to_string()))?;
                        entry.insert((tokio::fs::File::create(path).await?, 0, false));
                    }
                    let (file, expected, done) =
                        artifacts.get_mut(&key).expect("inserted artifact");
                    if *done || offset != *expected {
                        return Err(std::io::Error::other("invalid artifact offset"));
                    }
                    file.write_all(&data).await?;
                    file.flush().await?;
                    *expected += data.len() as u64;
                    if finished {
                        file.sync_data().await?;
                        *done = true;
                    }
                    Ok::<_, std::io::Error>(())
                }
                .await;
                if let Err(error) = received {
                    fail_connection(state, RemoteError::io(error)).await;
                    return;
                }
            }
            Response::ToolChunk {
                request_id,
                offset,
                data,
                finished,
            } => {
                let result = (|| -> std::io::Result<Option<RemoteToolResult>> {
                    use std::io::{Seek as _, Write as _};
                    if let std::collections::hash_map::Entry::Vacant(entry) =
                        transfers.entry(request_id)
                    {
                        if offset != 0 {
                            return Err(std::io::Error::other("invalid initial result offset"));
                        }
                        entry.insert((tempfile::tempfile()?, 0));
                    }
                    let (file, expected) =
                        transfers.get_mut(&request_id).expect("inserted transfer");
                    if offset != *expected
                        || data.len() > 64 * 1024
                        || (finished && !data.is_empty())
                    {
                        return Err(std::io::Error::other("invalid result chunk"));
                    }
                    file.write_all(&data)?;
                    *expected += data.len() as u64;
                    if !finished {
                        return Ok(None);
                    }
                    let (mut file, _) = transfers.remove(&request_id).expect("completed transfer");
                    file.rewind()?;
                    Ok(Some(serde_json::from_reader(std::io::BufReader::new(
                        file,
                    ))?))
                })();
                match result {
                    Ok(Some(result)) => {
                        if let Err(error) =
                            finish_tool(state, &mut artifacts, request_id, result).await
                        {
                            fail_connection(state, error).await;
                            return;
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        fail_connection(state, RemoteError::io(error)).await;
                        return;
                    }
                }
            }
            Response::SensitiveCancelled { prompt_id } => {
                if let Some(task) = prompt_tasks.remove(&prompt_id) {
                    task.abort();
                }
            }
            Response::ResolvedSsh { request_id, result } => {
                if let Some(sender) = state.lock().await.resolutions.remove(&request_id) {
                    let _ = sender.send(result.map_err(RemoteError::Resolution));
                }
            }
            Response::StreamData { channel, data } => {
                let invalid = data.len() > 32 * 1024;
                let overflow = {
                    let state = state.lock().await;
                    state
                        .streams
                        .get(&channel)
                        .is_some_and(|sender| sender.output.try_send(Ok(data)).is_err())
                };
                if invalid || overflow {
                    fail_connection(
                        state,
                        RemoteError::Protocol(
                            "invalid stream output or flow-control overflow".into(),
                        ),
                    )
                    .await;
                    return;
                }
            }
            Response::StreamClosed { channel, error } => {
                if let Some(sender) = state.lock().await.streams.remove(&channel)
                    && let Some(error) = error
                {
                    let _ = sender.output.try_send(Err(RemoteError::Ssh(error)));
                }
            }
            Response::StreamAck { channel } => {
                let invalid = {
                    let state = state.lock().await;
                    state.streams.get(&channel).is_some_and(|stream| {
                        if stream.credit.available_permits() >= 16 {
                            true
                        } else {
                            stream.credit.add_permits(1);
                            false
                        }
                    })
                };
                if invalid {
                    fail_connection(state, RemoteError::Protocol("invalid stream credit".into()))
                        .await;
                    return;
                }
            }
            Response::SensitivePrompt {
                prompt_id,
                mut prompt,
            } => {
                prompt.message = format!("[origin={target}] {}", prompt.message);
                let writer = host.0.clone();
                let prompts = prompts.clone();
                let task = callbacks.spawn(async move {
                    let answer = match prompts.prompt(prompt).await {
                        Ok(value) => super::prompt::PromptAnswer::Accepted(value),
                        Err(_) => super::prompt::PromptAnswer::Rejected,
                    };
                    write_frame(
                        &mut writer.lock().await.input,
                        &Request::SensitiveAnswer { prompt_id, answer },
                    )
                    .await?;
                    Ok(Some(prompt_id))
                });
                prompt_tasks.insert(prompt_id, task);
            }
            Response::Tool { request_id, result } => {
                if let Err(error) = finish_tool(state, &mut artifacts, request_id, result).await {
                    fail_connection(state, error).await;
                    return;
                }
            }
            Response::Authorization {
                request_id,
                authorization_id,
                tool,
                mut permissions,
                arguments,
            } => {
                if let Err(error) = rebase_remote_permissions(&target, &mut permissions) {
                    fail_connection(state, error).await;
                    return;
                }
                let context = state
                    .lock()
                    .await
                    .pending
                    .get(&request_id)
                    .map(|pending| pending.context.clone());
                let writer = host.0.clone();
                let coordinator = host.1.clone();
                callbacks.spawn(async move {
                    let decision = if let Some(context) = context {
                        coordinator
                            .authorize(&context.authorization, tool, permissions, arguments)
                            .await
                    } else {
                        Err(AuthorizationError::Denied(
                            "remote path authorization requires a host tool context".into(),
                        ))
                    };
                    let (allowed, reason) = match decision {
                        Ok(()) => (true, None),
                        Err(
                            AuthorizationError::Denied(reason)
                            | AuthorizationError::InvalidGrant(reason),
                        ) => (false, Some(reason)),
                        Err(AuthorizationError::Cancelled) => {
                            (false, Some("tool was cancelled".into()))
                        }
                        Err(AuthorizationError::Unavailable) => {
                            (false, Some("capability is unavailable".into()))
                        }
                    };
                    write_frame(
                        &mut writer.lock().await.input,
                        &Request::AuthorizationDecision {
                            request_id,
                            authorization_id,
                            allowed,
                            reason,
                        },
                    )
                    .await?;
                    Ok(None)
                });
            }
            Response::Ready => {
                fail_connection(
                    state,
                    RemoteError::Protocol("received a second remote ready response".to_owned()),
                )
                .await;
                return;
            }
        }
    }
}

type CapturedArtifacts = HashMap<(u64, String), (tokio::fs::File, u64, bool)>;

async fn finish_tool(
    state: &Mutex<ConnectionState>,
    artifacts: &mut CapturedArtifacts,
    request_id: u64,
    result: RemoteToolResult,
) -> Result<(), RemoteError> {
    if artifacts
        .iter()
        .any(|((id, _), (_, _, done))| *id == request_id && !done)
    {
        return Err(RemoteError::Protocol(
            "result before artifact completion".into(),
        ));
    }
    artifacts.retain(|(id, _), _| *id != request_id);
    let pending = state
        .lock()
        .await
        .pending
        .remove(&request_id)
        .ok_or_else(|| {
            RemoteError::Protocol(format!("response used unknown request ID {request_id}"))
        })?;
    let _ = pending.sender.send(Ok(result));
    Ok(())
}

fn rebase_remote_permissions(
    target: &str,
    permissions: &mut [PermissionUse],
) -> Result<(), RemoteError> {
    fn rebase_resource(
        target: &str,
        resource: &mut crate::tool::policy::ResourceId,
    ) -> Result<(), RemoteError> {
        if resource.namespace != "path" {
            return Ok(());
        }
        let Some(origin) = resource.segments.first_mut() else {
            return Err(RemoteError::Protocol(
                "remote path permission omitted its execution target".to_owned(),
            ));
        };
        if origin != "root" {
            return Err(RemoteError::Protocol(format!(
                "remote path permission used unexpected execution target `{origin}`"
            )));
        }
        target.clone_into(origin);
        Ok(())
    }

    for permission in permissions {
        rebase_resource(target, &mut permission.resource)?;
        if let Some(grant) = &mut permission.proposed_grant {
            rebase_resource(target, &mut grant.resource)?;
        }
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
    pub(super) async fn execute(
        &self,
        name: String,
        arguments: serde_json::Value,
        context: &ToolContext,
    ) -> Result<ToolOutput, RemoteError> {
        call_tool(self, name, arguments, context).await
    }
    pub(super) async fn resolve_ssh(
        &self,
        target: TargetDefinition,
    ) -> Result<super::backends::ssh::ResolvedSsh, RemoteError> {
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
    pub(super) async fn open_ssh(
        self: Arc<Self>,
        route: Vec<TargetDefinition>,
        command: String,
    ) -> Result<super::transport::Transport, RemoteError> {
        self.open_stream(route, command).await
    }
}
struct StreamOwner {
    parent: Arc<PooledConnection>,
    channel: u64,
    tasks: Vec<tokio::task::AbortHandle>,
}
impl Drop for StreamOwner {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        let parent = self.parent.clone();
        let channel = self.channel;
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                parent.state.lock().await.streams.remove(&channel);
                let _ = write_frame(
                    &mut parent.writer.lock().await.input,
                    &Request::StreamClose { channel },
                )
                .await;
            });
        }
    }
}
impl PooledConnection {
    async fn open_stream(
        self: &Arc<Self>,
        route: Vec<TargetDefinition>,
        command: String,
    ) -> Result<super::transport::Transport, RemoteError> {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(256);
        let credit = Arc::new(tokio::sync::Semaphore::new(16));
        let (channel, ()) = self
            .submit(|channel, state| {
                state.streams.insert(
                    channel,
                    ClientStream {
                        output: sender,
                        credit: credit.clone(),
                    },
                );
                (
                    Request::OpenSsh {
                        channel,
                        route,
                        command,
                    },
                    (),
                )
            })
            .await?;
        let (client, peer) = tokio::io::duplex(64 * 1024);
        let (mut input, mut output) = tokio::io::split(peer);
        let parent = self.clone();
        let write_task = tokio::spawn(async move {
            let mut bytes = vec![0; 32 * 1024];
            while let Ok(count) = input.read(&mut bytes).await {
                let Ok(permit) = credit.acquire().await else {
                    break;
                };
                permit.forget();
                let request = if count == 0 {
                    Request::StreamEnd { channel }
                } else {
                    Request::StreamData {
                        channel,
                        data: bytes[..count].to_vec(),
                    }
                };
                if write_frame(&mut parent.writer.lock().await.input, &request)
                    .await
                    .is_err()
                    || count == 0
                {
                    break;
                }
            }
        });
        let parent = self.clone();
        let failure = Arc::new(std::sync::Mutex::new(None));
        let read_failure = failure.clone();
        let read_task = tokio::spawn(async move {
            while let Some(result) = receiver.recv().await {
                let bytes = match result {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        *read_failure
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            Some(error.to_string());
                        break;
                    }
                };
                if output.write_all(&bytes).await.is_err() {
                    break;
                }
                if write_frame(
                    &mut parent.writer.lock().await.input,
                    &Request::StreamAck { channel },
                )
                .await
                .is_err()
                {
                    break;
                }
            }
            let _ = output.shutdown().await;
        });
        let (output, input) = tokio::io::split(client);
        Ok(super::transport::Transport {
            input: Box::new(input),
            output: Box::new(super::transport::RelayedReader { output, failure }),
            owner: Box::new(StreamOwner {
                parent: self.clone(),
                channel,
                tasks: vec![write_task.abort_handle(), read_task.abort_handle()],
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::CancellationToken;
    use crate::tool::ToolError;

    async fn test_connection() -> PooledConnection {
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

    use crate::tool::policy::{ApprovalGrant, Capability, ResourceId};

    fn fixture_context(runtime: &crate::test_support::TestRuntime) -> ToolContext {
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

    async fn route_fixture<R: AsyncRead + Unpin>(
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

    fn output(value: &str) -> RemoteToolResult {
        Ok(RemoteToolOutput {
            value: serde_json::json!(value),
            images: Vec::new(),
        })
    }

    #[tokio::test]
    async fn fake_transport_completes_the_client_handshake() {
        let connection = PooledConnection::from_transport(
            test_transport(),
            "test",
            AuthorizationCoordinator::new(Arc::new(crate::tool::policy::AllowAll)),
            Arc::new(crate::remote::RejectSensitivePrompts),
        )
        .await
        .unwrap();
        assert!(connection.state.lock().await.failure.is_none());
        assert_eq!(connection.writer.lock().await.next_request_id, 1);
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

    #[tokio::test]
    async fn authorization_callbacks_do_not_block_dispatch_and_fail_the_connection_on_write_error()
    {
        use crate::tool::policy::{AuthorizationRequest, Policy, PolicyDecision, PolicyFuture};
        struct WaitingPolicy {
            entered: Arc<tokio::sync::Notify>,
            release: Arc<tokio::sync::Notify>,
        }
        impl Policy for WaitingPolicy {
            fn authorize(&self, _: AuthorizationRequest) -> PolicyFuture<'_> {
                Box::pin(async move {
                    self.entered.notify_one();
                    self.release.notified().await;
                    PolicyDecision::allow()
                })
            }
        }
        let runtime = crate::test_support::TestRuntime::new().await;
        let context = fixture_context(&runtime);
        let connection = test_connection().await;
        let (mut requests, output) = tokio::io::duplex(4096);
        let (input, mut replies) = tokio::io::duplex(4096);
        let writer = Arc::new(Mutex::new(RequestWriter {
            input: Box::new(input),
            next_request_id: 1,
        }));
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let authorization = AuthorizationCoordinator::new(Arc::new(WaitingPolicy {
            entered: entered.clone(),
            release: release.clone(),
        }));
        let (sender, receiver) = oneshot::channel();
        let (first, first_result) = oneshot::channel();
        connection.state.lock().await.pending.insert(
            1,
            PendingCall {
                sender: first,
                context: context.clone(),
            },
        );
        connection
            .state
            .lock()
            .await
            .pending
            .insert(2, PendingCall { sender, context });
        let state = connection.state.clone();
        let reader = tokio::spawn(async move {
            route_responses(
                output,
                &state,
                (&writer, &authorization),
                "build".into(),
                Arc::new(crate::remote::RejectSensitivePrompts),
            )
            .await;
        });
        let prompt = Response::SensitivePrompt {
            prompt_id: 9,
            prompt: crate::remote::SensitivePrompt {
                kind: crate::remote::SensitivePromptKind::Password,
                message: "fixture".into(),
            },
        };
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            write_frame(
                &mut requests,
                &Response::Authorization {
                    request_id: 1,
                    authorization_id: 7,
                    tool: "read".into(),
                    permissions: vec![PermissionUse::new(
                        Capability::Read,
                        ResourceId::new("path", ["root", "/", "outside"]),
                    )],
                    arguments: serde_json::json!({}),
                },
            )
            .await
            .unwrap();
            entered.notified().await;
            write_frame(
                &mut requests,
                &Response::Tool {
                    request_id: 2,
                    result: super::tests::output("second"),
                },
            )
            .await
            .unwrap();
            assert_eq!(receiver.await.unwrap().unwrap().unwrap().value, "second");
            write_frame(&mut requests, &prompt).await.unwrap();
            assert!(matches!(
                read_frame::<_, Request>(&mut replies).await.unwrap(),
                Some(Request::SensitiveAnswer {
                    prompt_id: 9,
                    answer: super::super::prompt::PromptAnswer::Rejected
                })
            ));
            release.notify_one();
            assert!(matches!(
                read_frame::<_, Request>(&mut replies).await.unwrap(),
                Some(Request::AuthorizationDecision {
                    authorization_id: 7,
                    allowed: true,
                    ..
                })
            ));
        })
        .await
        .unwrap();
        // No further incoming frame should be required to observe callback failure.
        drop(replies);
        write_frame(&mut requests, &prompt).await.unwrap();
        let failure = tokio::time::timeout(std::time::Duration::from_secs(2), first_result)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(failure, Err(RemoteError::Io { .. })));
        reader.await.unwrap();
    }

    #[tokio::test]
    async fn artifact_transfer_hydrates_native_results_and_preserves_interrupted_prefixes() {
        for complete in [true, false] {
            let runtime = crate::test_support::TestRuntime::new().await;
            let payload = "line\n".repeat(250_000);
            let source = runtime.root.path().join("payload.txt");
            tokio::fs::write(&source, &payload).await.unwrap();
            let mut builder = crate::tool::ToolRegistryBuilder::default();
            builder.register_dynamic("remote_fixture", "remote fixture", serde_json::json!({"type":"object","properties":{},"additionalProperties":false}), crate::tool::ToolOptions::default().output_schema(serde_json::to_value(schemars::schema_for!(crate::tool::builtins::ProcessOutput)).unwrap()), move |context, _| {
                let source = source.clone();
                async move {
                    let (peer, stream) = tokio::io::duplex(64 * 1024);
                    let (sender, receiver) = oneshot::channel();
                    let state = Arc::new(Mutex::new(ConnectionState { pending:HashMap::from([(1, PendingCall {sender,context})]), failure:None,resolutions:HashMap::new(),streams:HashMap::new() }));
                    let reader_state = state.clone();
                    let reader = tokio::spawn(async move { route_fixture(stream, &reader_state, "fixture").await; });
                    let writer = tokio::spawn(async move {
                        let peer = Mutex::new(peer);
                        if complete {
                            super::super::protocol::write_artifact(&peer,1,"/result/stdout".into(),&source).await.unwrap();
                            write_frame(&mut *peer.lock().await,&Response::Tool {request_id:1,result:Ok(RemoteToolOutput {value:serde_json::json!({"stdout":"","exit_code":0}),images:Vec::new()})}).await.unwrap();
                        } else {
                            write_frame(&mut *peer.lock().await,&Response::ToolArtifact {request_id:1,field:"/result/stdout".into(),offset:0,data:b"retained prefix\n".to_vec(),finished:false}).await.unwrap();
                        }
                    });
                    let result = receiver.await.map_err(|e| ToolError::Failed(e.to_string()))?;
                    writer.await.map_err(|e| ToolError::Failed(e.to_string()))?;
                    reader.await.map_err(|e| ToolError::Failed(e.to_string()))?;
                    result.map_err(RemoteError::into_tool_error)?.map(Into::into).map_err(|e| ToolError::Failed(e.message))
                }
            }).unwrap();
            let script_slot = Arc::new(std::sync::OnceLock::new());
            crate::tool::builtins::install_script_tool(&mut builder, Arc::downgrade(&script_slot))
                .unwrap();
            let executor = runtime.executor(builder);
            script_slot.set(executor.clone()).ok().unwrap();
            let result = executor
                .execute(
                    runtime.agent.clone(),
                    "remote_fixture",
                    serde_json::json!({}),
                    None,
                )
                .await;
            if complete {
                let result = result.unwrap();
                assert_eq!(result.output.value["stdout"], payload);
                let view = runtime
                    .jobs
                    .present_output(
                        crate::job::output::OutputArgs::new(result.job),
                        &Default::default(),
                    )
                    .await
                    .unwrap();
                assert_eq!(view["result"]["stdout"], "line\n".repeat(100));
                assert_eq!(view["result"]["exit_code"], 0);
                assert_eq!(view["truncated"][0]["field"], "/result/stdout");
                let script = executor.execute_model(runtime.agent.clone(), "script", serde_json::json!({
                    "source":"const remote = await tool.remote_fixture({}); if (remote.stdout.length !== 1250000) throw new Error('truncated inside script'); return {remote};"
                }), None).await.unwrap();
                let child = &script.output.value["result"]["value"]["remote"];
                assert_eq!(child["tool"], "remote_fixture");
                assert_eq!(child["result"]["stdout"], "line\n".repeat(100));
                let mut query = crate::job::output::OutputArgs::new(
                    serde_json::from_value(child["id"].clone()).unwrap(),
                );
                query.field = Some(child["truncated"][0]["field"].as_str().unwrap().into());
                query.start = Some(child["truncated"][0]["next_start"].as_u64().unwrap() as usize);
                query.offset =
                    Some(child["truncated"][0]["next_offset"].as_u64().unwrap_or(0) as usize);
                let page = runtime
                    .jobs
                    .present_output(query, &Default::default())
                    .await
                    .unwrap();
                assert_eq!(child["truncated"][0]["next_start"], 101);
                assert_eq!(page["preview"]["lines"][0], "line");
            } else {
                assert!(result.is_err());
                let job = runtime.jobs.list(&runtime.agent).await[0].id;
                let mut args = crate::job::output::OutputArgs::new(job);
                args.field = Some("/result/stdout".into());
                let view = runtime
                    .jobs
                    .present_output(args, &Default::default())
                    .await
                    .unwrap();
                assert_eq!(view["state"], "failed");
                assert_eq!(view["preview"]["lines"][0], "retained prefix");
                assert_eq!(view["notice"], "Output incomplete.");
                assert!(view["preview"].get("capture_complete").is_none());
            }
        }
    }

    #[test]
    fn forwarded_path_permissions_are_rebased_to_the_destination() {
        let path = ResourceId::new("path", ["root", "/", "outside"]);
        let mut permissions = vec![
            PermissionUse::new(Capability::Write, path.clone())
                .with_grant(ApprovalGrant::descendants(Capability::Write, path)),
        ];

        rebase_remote_permissions("build", &mut permissions).unwrap();

        assert_eq!(permissions[0].resource.segments[0], "build");
        assert_eq!(
            permissions[0]
                .proposed_grant
                .as_ref()
                .unwrap()
                .resource
                .segments[0],
            "build"
        );
    }

    #[test]
    fn forwarded_path_permissions_cannot_claim_another_target() {
        let mut permissions = vec![PermissionUse::new(
            Capability::Read,
            ResourceId::new("path", ["other", "/", "outside"]),
        )];
        assert!(matches!(
            rebase_remote_permissions("build", &mut permissions),
            Err(RemoteError::Protocol(_))
        ));
    }

    #[tokio::test]
    async fn invalid_or_closed_response_fails_pending_requests() {
        let runtime = crate::test_support::TestRuntime::new().await;
        let context = fixture_context(&runtime);
        for orphan in [true, false] {
            let (mut peer, stream) = tokio::io::duplex(4096);
            let state = Arc::new(Mutex::new(ConnectionState::default()));
            let (sender, receiver) = oneshot::channel();
            state.lock().await.pending.insert(
                1,
                PendingCall {
                    sender,
                    context: context.clone(),
                },
            );
            let reader = tokio::spawn(async move {
                route_fixture(stream, &state, "test").await;
            });
            if orphan {
                write_frame(
                    &mut peer,
                    &Response::Tool {
                        request_id: 99,
                        result: output("orphaned"),
                    },
                )
                .await
                .unwrap();
            }
            drop(peer);
            let expected = if orphan {
                "unknown request ID 99"
            } else {
                "closed before replying"
            };
            assert!(
                matches!(receiver.await.unwrap(), Err(RemoteError::Protocol(message)) if message.contains(expected))
            );
            reader.await.unwrap();
        }
    }
}
