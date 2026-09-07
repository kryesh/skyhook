use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use futures_util::future::{BoxFuture, FutureExt as _, Shared};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _},
    sync::{Mutex, oneshot},
    task::JoinHandle,
};

use crate::{
    job::CancellationToken,
    remote::{ArtifactError, EmbeddedShimCatalog, SensitivePromptHandler},
    target::{ResolvedRoute, RouteIdentity, TargetDefinition, TargetError},
    tool::{
        ToolContext, ToolError, ToolOutput,
        authorization::{AuthorizationCoordinator, AuthorizationError},
        policy::{PermissionUse, PolicyDecision},
    },
};

use super::backend::{Session, TargetSession};
use super::protocol::{
    RemoteToolError, RemoteToolOutput, Request, Response, read_frame, write_frame,
};

#[derive(Clone)]
pub(crate) struct RemoteManager {
    inner: Arc<RemoteInner>,
}

struct RemoteInner {
    catalog: EmbeddedShimCatalog,
    authentication: super::authentication::Authentication,
    shutdown: CancellationToken,
    pool: Mutex<HashMap<ConnectionKey, Arc<PooledSlot>>>,
    prompts: Arc<dyn SensitivePromptHandler>,
    authorization: AuthorizationCoordinator,
    #[cfg(test)]
    factory: Option<Arc<dyn ConnectionFactory>>,
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct ConnectionRequest {
    pub target: String,
    pub route: Vec<TargetDefinition>,
    pub workspace: PathBuf,
}

#[cfg(test)]
pub(crate) trait ConnectionFactory: Send + Sync {
    fn connect(
        &self,
        request: ConnectionRequest,
    ) -> BoxFuture<'static, Result<PooledConnection, RemoteError>>;
}

pub(crate) struct PooledConnection {
    writer: Arc<Mutex<RequestWriter>>,
    state: Arc<Mutex<ConnectionState>>,
    _owner: std::sync::Mutex<Box<dyn Send>>,
    reader: JoinHandle<()>,
}

struct PooledSlot {
    connection: Shared<BoxFuture<'static, Result<Session, RemoteError>>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ConnectionKey {
    route: RouteIdentity,
    workspace: PathBuf,
}

#[derive(Clone)]
pub(crate) struct PreparedConnection {
    manager: RemoteManager,
    key: ConnectionKey,
    slot: Arc<PooledSlot>,
    connection: Session,
}

impl PreparedConnection {
    pub(crate) async fn execute(
        self,
        name: String,
        arguments: serde_json::Value,
        context: &ToolContext,
    ) -> Result<ToolOutput, RemoteError> {
        let result = self.connection.execute(name, arguments, context).await;
        if matches!(
            result,
            Err(RemoteError::Io { .. } | RemoteError::Protocol(_))
        ) {
            self.manager.discard(&self).await;
        }
        result
    }
}

struct RequestWriter {
    input: super::transport::Writer,
    next_request_id: u64,
}

type RemoteToolResult = Result<RemoteToolOutput, RemoteToolError>;
type PendingResult = Result<RemoteToolResult, RemoteError>;

struct PendingCall {
    sender: oneshot::Sender<PendingResult>,
    context: Option<ToolContext>,
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

struct ConnectionState {
    pending: HashMap<u64, PendingCall>,
    failure: Option<RemoteError>,
    resolutions: HashMap<u64, oneshot::Sender<Result<super::ssh::ResolvedSsh, RemoteError>>>,
    streams: HashMap<u64, ClientStream>,
}

impl RemoteManager {
    pub(crate) fn new(
        catalog: EmbeddedShimCatalog,
        prompts: Arc<dyn SensitivePromptHandler>,
        authorization: AuthorizationCoordinator,
    ) -> Self {
        Self {
            inner: Arc::new(RemoteInner {
                catalog,
                authentication: super::authentication::Authentication::new(prompts.clone()),
                shutdown: CancellationToken::new(),
                pool: Mutex::new(HashMap::new()),
                prompts,
                authorization,
                #[cfg(test)]
                factory: None,
            }),
        }
    }

    pub(crate) async fn environment(
        &self,
    ) -> Result<super::authentication::ProcessEnvironment, RemoteError> {
        if self.inner.shutdown.is_cancelled() {
            return Err(RemoteError::Cancelled);
        }
        self.inner.authentication.environment().await
    }
    pub(crate) async fn shutdown(&self) {
        self.inner.shutdown.cancel();
        self.inner.pool.lock().await.clear();
        self.inner.authentication.shutdown().await;
    }

    #[cfg(test)]
    pub(crate) fn with_connection_factory(mut self, factory: Arc<dyn ConnectionFactory>) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("connection factories are installed before sharing a manager")
            .factory = Some(factory);
        self
    }

    pub(crate) fn connection<'a>(
        &'a self,
        route: ResolvedRoute,
        workspace: &'a Path,
        cancellation: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<PreparedConnection, RemoteError>> {
        Box::pin(async move {
            let key = ConnectionKey {
                route: route.identity.clone(),
                workspace: workspace.to_path_buf(),
            };
            let slot = {
                let mut pool = self.inner.pool.lock().await;
                if let Some(existing) = pool.get(&key) {
                    existing.clone()
                } else {
                    let manager = self.clone();
                    let target = route.identity.destination.clone();
                    let definitions = route.definitions;
                    let workspace = workspace.to_path_buf();
                    let startup = tokio::spawn(async move {
                        let connect = async {
                            #[cfg(test)]
                            if let Some(factory) = manager.inner.factory.clone() {
                                return factory
                                    .connect(ConnectionRequest {
                                        target: target.clone(),
                                        route: definitions.clone(),
                                        workspace: workspace.clone(),
                                    })
                                    .await
                                    .map(|connection| Arc::new(connection) as Session);
                            }
                            manager.connect(&target, &definitions, &workspace).await
                        };
                        tokio::select! { result = connect => result, () = manager.inner.shutdown.cancelled() => Err(RemoteError::Cancelled) }
                    });
                    let connection = async move {
                        startup
                            .await
                            .map_err(|error| RemoteError::ConnectionTask(error.to_string()))?
                    }
                    .boxed()
                    .shared();
                    let slot = Arc::new(PooledSlot { connection });
                    pool.insert(key.clone(), slot.clone());
                    slot
                }
            };
            let result = tokio::select! {
                result = slot.connection.clone() => result,
                () = cancellation.cancelled() => return Err(RemoteError::Cancelled),
            };
            match result {
                Ok(connection) => Ok(PreparedConnection {
                    manager: self.clone(),
                    key,
                    slot,
                    connection,
                }),
                Err(error) => {
                    self.remove_slot(&key, &slot).await;
                    Err(error)
                }
            }
        })
    }

    pub(crate) async fn is_current(&self, prepared: &PreparedConnection) -> bool {
        self.inner
            .pool
            .lock()
            .await
            .get(&prepared.key)
            .is_some_and(|slot| Arc::ptr_eq(slot, &prepared.slot))
    }

    pub(crate) async fn discard(&self, prepared: &PreparedConnection) {
        self.remove_slot(&prepared.key, &prepared.slot).await;
    }

    async fn remove_slot(&self, key: &ConnectionKey, slot: &Arc<PooledSlot>) {
        let mut pool = self.inner.pool.lock().await;
        if pool
            .get(key)
            .is_some_and(|current| Arc::ptr_eq(current, slot))
        {
            pool.remove(key);
        }
    }

    pub(crate) async fn invalidate(&self, names: &[String]) {
        self.inner
            .pool
            .lock()
            .await
            .retain(|key, _| !names.contains(&key.route.destination));
    }

    async fn connect(
        &self,
        target: &str,
        route: &[TargetDefinition],
        workspace: &Path,
    ) -> Result<Session, RemoteError> {
        let destination = route.last().ok_or(RemoteError::EmptyRoute)?;
        let (origin, hops) = if destination.origin == crate::target::ROOT_TARGET {
            (None, route)
        } else {
            let index = route
                .iter()
                .position(|hop| hop.name == destination.origin)
                .ok_or_else(|| RemoteError::Protocol("origin missing from route".into()))?;
            let definitions = route[..=index].to_vec();
            let identity = RouteIdentity {
                destination: destination.origin.clone(),
                hops: definitions
                    .iter()
                    .map(|h| (h.name.clone(), h.revision))
                    .collect(),
            };
            let cancellation = CancellationToken::new();
            let prepared = self
                .connection(
                    ResolvedRoute {
                        identity,
                        definitions,
                    },
                    &route[index].workspace,
                    &cancellation,
                )
                .await?;
            (Some(prepared.connection), &route[index + 1..])
        };
        let environment = if origin.is_none() {
            self.environment().await?
        } else {
            Default::default()
        };
        match destination.r#type {
            crate::target::TargetType::Ssh => {
                let backend = super::ssh::SshLauncher {
                    origin,
                    route: hops.to_vec(),
                    environment,
                    prompts: self.inner.prompts.clone(),
                };
                let transport = backend
                    .connect(destination, workspace, &self.inner.catalog)
                    .await?;
                Ok(Arc::new(
                    PooledConnection::from_transport(
                        transport,
                        target,
                        self.inner.authorization.clone(),
                        self.inner.prompts.clone(),
                    )
                    .await?,
                ))
            }
            crate::target::TargetType::Local => Err(RemoteError::Target(TargetError::BuiltinOnly)),
        }
    }
}

impl PooledConnection {
    async fn from_transport(
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
        let state = Arc::new(Mutex::new(ConnectionState {
            pending: HashMap::new(),
            failure: None,
            resolutions: HashMap::new(),
            streams: HashMap::new(),
        }));
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
                Some((&reader_writer, &authorization)),
                reader_target,
                Some(prompts),
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
    let (request_id, receiver) = {
        let mut writer = connection.writer.lock().await;
        let Some(next_request_id) = writer.next_request_id.checked_add(1) else {
            drop(writer);
            let failure = RemoteError::Protocol("request ID space exhausted".to_owned());
            fail_connection(&connection.state, failure.clone()).await;
            return Err(failure);
        };
        let request_id = writer.next_request_id;
        writer.next_request_id = next_request_id;
        let (sender, receiver) = oneshot::channel();
        {
            let mut state = connection.state.lock().await;
            if let Some(failure) = state.failure.clone() {
                return Err(failure);
            }
            state.pending.insert(
                request_id,
                PendingCall {
                    sender,
                    context: Some(context.clone()),
                },
            );
        }
        if let Err(error) = write_frame(
            &mut writer.input,
            &Request::Tool {
                request_id,
                name,
                arguments,
            },
        )
        .await
        {
            drop(writer);
            fail_connection(&connection.state, RemoteError::io(error)).await;
        }
        (request_id, receiver)
    };
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
    host: Option<(&Arc<Mutex<RequestWriter>>, &AuthorizationCoordinator)>,
    target: String,
    prompts: Option<Arc<dyn SensitivePromptHandler>>,
) where
    R: AsyncRead + Unpin,
{
    let mut callbacks = tokio::task::JoinSet::new();
    let mut prompt_tasks = HashMap::<u64, tokio::task::AbortHandle>::new();
    let mut transfers = HashMap::<u64, (std::fs::File, u64)>::new();
    let mut artifacts = HashMap::<(u64, String), (tokio::fs::File, u64, bool)>::new();
    loop {
        while let Some(result) = callbacks.try_join_next() {
            if let Ok(id) = result {
                prompt_tasks.remove(&id);
            }
        }
        let response = match read_frame::<_, Response>(&mut output).await {
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
                    .and_then(|pending| pending.context.clone());
                let Some(context) = context else {
                    fail_connection(
                        state,
                        RemoteError::Protocol("artifact for unknown request".into()),
                    )
                    .await;
                    return;
                };
                if !(field == "/console"
                    || field == "/error"
                    || field == "/result"
                    || field.starts_with("/result/"))
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
                        if artifacts
                            .iter()
                            .any(|((id, _), (_, _, done))| *id == request_id && !done)
                        {
                            fail_connection(
                                state,
                                RemoteError::Protocol("result before artifact completion".into()),
                            )
                            .await;
                            return;
                        }
                        artifacts.retain(|(id, _), _| *id != request_id);
                        let pending = state.lock().await.pending.remove(&request_id);
                        if let Some(pending) = pending {
                            let _ = pending.sender.send(Ok(result));
                        } else {
                            fail_connection(
                                state,
                                RemoteError::Protocol("result for unknown request".into()),
                            )
                            .await;
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
                if let Some((writer, _)) = host {
                    let writer = writer.clone();
                    let prompts = prompts.clone();
                    let task = callbacks.spawn(async move {
                        let answer = if let Some(prompts) = prompts {
                            match prompts.prompt(prompt).await {
                                Ok(value) => super::askpass::PromptAnswer::Accepted(value),
                                Err(_) => super::askpass::PromptAnswer::Rejected,
                            }
                        } else {
                            super::askpass::PromptAnswer::Rejected
                        };
                        let _ = write_frame(
                            &mut writer.lock().await.input,
                            &Request::SensitiveAnswer { prompt_id, answer },
                        )
                        .await;
                        prompt_id
                    });
                    prompt_tasks.insert(prompt_id, task);
                }
            }
            Response::Tool { request_id, result } => {
                if artifacts
                    .iter()
                    .any(|((id, _), (_, _, done))| *id == request_id && !done)
                {
                    fail_connection(
                        state,
                        RemoteError::Protocol("result before artifact completion".into()),
                    )
                    .await;
                    return;
                }
                artifacts.retain(|(id, _), _| *id != request_id);
                let pending = state.lock().await.pending.remove(&request_id);
                let Some(pending) = pending else {
                    fail_connection(
                        state,
                        RemoteError::Protocol(format!(
                            "response used unknown request ID {request_id}"
                        )),
                    )
                    .await;
                    return;
                };
                let _ = pending.sender.send(Ok(result));
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
                    .and_then(|pending| pending.context.clone());
                let decision = if let (Some(context), Some((_, coordinator))) = (context, host) {
                    match coordinator
                        .authorize(&context.authorization, tool, permissions, arguments)
                        .await
                    {
                        Ok(()) => PolicyDecision::allow(),
                        Err(AuthorizationError::Denied(reason)) => PolicyDecision::Deny { reason },
                        Err(AuthorizationError::Cancelled) => PolicyDecision::Deny {
                            reason: "tool was cancelled".to_owned(),
                        },
                        Err(AuthorizationError::InvalidGrant(reason)) => {
                            PolicyDecision::Deny { reason }
                        }
                        Err(AuthorizationError::Unavailable) => PolicyDecision::Deny {
                            reason: "capability is unavailable".to_owned(),
                        },
                    }
                } else {
                    PolicyDecision::Deny {
                        reason: "remote path authorization requires a host tool context".to_owned(),
                    }
                };
                let (allowed, reason) = match decision {
                    PolicyDecision::Allow { .. } => (true, None),
                    PolicyDecision::Deny { reason } => (false, Some(reason)),
                };
                let Some((writer, _)) = host else {
                    fail_connection(
                        state,
                        RemoteError::Protocol(
                            "remote authorization response writer is unavailable".to_owned(),
                        ),
                    )
                    .await;
                    return;
                };
                let mut writer = writer.lock().await;
                if let Err(error) = write_frame(
                    &mut writer.input,
                    &Request::AuthorizationDecision {
                        request_id,
                        authorization_id,
                        allowed,
                        reason,
                    },
                )
                .await
                {
                    fail_connection(state, RemoteError::io(error)).await;
                    return;
                }
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

#[cfg(test)]
pub(crate) async fn test_connection() -> PooledConnection {
    let directory = tempfile::tempdir().unwrap();
    use std::process::Stdio;
    use tokio::process::Command;
    let mut child = Command::new("sh")
        .args(["-c", "sleep 60"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let input = child.stdin.take().unwrap();
    PooledConnection {
        writer: Arc::new(Mutex::new(RequestWriter {
            input: Box::new(input),
            next_request_id: 1,
        })),
        state: Arc::new(Mutex::new(ConnectionState {
            pending: HashMap::new(),
            failure: None,
            resolutions: HashMap::new(),
            streams: HashMap::new(),
        })),
        _owner: std::sync::Mutex::new(Box::new((child, directory))),
        reader: tokio::spawn(std::future::pending()),
    }
}

#[derive(Clone, Debug, Error)]
pub enum RemoteError {
    #[error(transparent)]
    Target(#[from] TargetError),
    #[error(transparent)]
    Artifact(#[from] ArtifactError),
    #[error(
        "this Skyhook build contains no remote shims; install with default features or provide an EmbeddedShimCatalog"
    )]
    MissingShims,
    #[error("unsupported remote platform {arch}-{os}: no matching shim is embedded")]
    UnsupportedPlatform { arch: String, os: String },
    #[error("could not start transport process: {message}")]
    Start {
        kind: std::io::ErrorKind,
        message: String,
    },
    #[error("SSH configuration resolution failed: {0}")]
    Resolution(String),
    #[error("target connection was denied: {0}")]
    ApprovalDenied(String),
    #[error("target connection returned an invalid approval grant: {0}")]
    ApprovalInvalidGrant(String),
    #[error("target connection requires an unavailable capability")]
    ApprovalUnavailable,
    #[error("target connection was cancelled")]
    Cancelled,
    #[error("remote connection startup task failed: {0}")]
    ConnectionTask(String),
    #[error("SSH failed: {0}")]
    Ssh(String),
    #[error("remote shim deployment failed: {0}")]
    Deployment(String),
    #[error("remote protocol failed: {0}")]
    Protocol(String),
    #[error("remote operation denied: {0}")]
    OperationDenied(String),
    #[error("remote tool failed: {message}")]
    Remote {
        message: String,
        output: Option<Box<ToolOutput>>,
    },
    #[error("remote connection is missing {0}")]
    MissingPipe(&'static str),
    #[error("target route is empty")]
    EmptyRoute,
    #[error("remote platform probe returned invalid output")]
    InvalidProbe,
    #[error("SSH values cannot be empty or contain control characters")]
    InvalidSshValue,
    #[error("{message}")]
    Io {
        kind: std::io::ErrorKind,
        message: String,
    },
    #[error("{0}")]
    Json(String),
}

impl RemoteError {
    pub(crate) fn authorization(error: AuthorizationError) -> Self {
        match error {
            AuthorizationError::Denied(reason) => Self::ApprovalDenied(reason),
            AuthorizationError::Cancelled => Self::Cancelled,
            AuthorizationError::InvalidGrant(reason) => Self::ApprovalInvalidGrant(reason),
            AuthorizationError::Unavailable => Self::ApprovalUnavailable,
        }
    }

    pub(crate) fn start(error: std::io::Error) -> Self {
        Self::Start {
            kind: error.kind(),
            message: error.to_string(),
        }
    }

    fn io(error: std::io::Error) -> Self {
        Self::Io {
            kind: error.kind(),
            message: error.to_string(),
        }
    }

    #[must_use]
    pub fn into_tool_error(self) -> ToolError {
        match self {
            Self::OperationDenied(reason) => ToolError::Denied(reason),
            Self::Remote {
                message,
                output: Some(output),
            } => ToolError::with_output(message, *output),
            Self::Remote {
                message,
                output: None,
            } => ToolError::Failed(message),
            error => ToolError::Failed(error.to_string()),
        }
    }
}

impl From<std::io::Error> for RemoteError {
    fn from(error: std::io::Error) -> Self {
        Self::io(error)
    }
}

impl From<serde_json::Error> for RemoteError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::policy::{ApprovalGrant, Capability, ResourceId};

    fn output(value: &str) -> RemoteToolResult {
        Ok(RemoteToolOutput {
            value: serde_json::json!(value),
            console_output: String::new(),
            images: Vec::new(),
        })
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
                    let state = Arc::new(Mutex::new(ConnectionState { pending:HashMap::from([(1, PendingCall {sender,context:Some(context)})]), failure:None,resolutions:HashMap::new(),streams:HashMap::new() }));
                    let reader_state = state.clone();
                    let reader = tokio::spawn(async move { route_responses(stream,&reader_state,None,"fixture".into(),None).await; });
                    let writer = tokio::spawn(async move {
                        let peer = Mutex::new(peer);
                        if complete {
                            super::super::protocol::write_artifact(&peer,1,"/result/stdout".into(),&source).await.unwrap();
                            write_frame(&mut *peer.lock().await,&Response::Tool {request_id:1,result:Ok(RemoteToolOutput {value:serde_json::json!({"stdout":"","exit_code":0}),images:Vec::new(),console_output:String::new()})}).await.unwrap();
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
                let child = &script.output.value["result"]["remote"];
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
    async fn responses_are_dispatched_by_request_id() {
        let (mut peer, stream) = tokio::io::duplex(4096);
        let state = Arc::new(Mutex::new(ConnectionState {
            pending: HashMap::new(),
            failure: None,
            resolutions: HashMap::new(),
            streams: HashMap::new(),
        }));
        let (first_sender, first) = oneshot::channel();
        let (second_sender, second) = oneshot::channel();
        {
            let mut state = state.lock().await;
            state.pending.insert(
                1,
                PendingCall {
                    sender: first_sender,
                    context: None,
                },
            );
            state.pending.insert(
                2,
                PendingCall {
                    sender: second_sender,
                    context: None,
                },
            );
        }
        let reader_state = state.clone();
        let reader = tokio::spawn(async move {
            route_responses(stream, &reader_state, None, "test".to_owned(), None).await;
        });

        write_frame(
            &mut peer,
            &Response::Tool {
                request_id: 2,
                result: output("second"),
            },
        )
        .await
        .unwrap();
        write_frame(
            &mut peer,
            &Response::Tool {
                request_id: 1,
                result: output("first"),
            },
        )
        .await
        .unwrap();

        assert_eq!(first.await.unwrap().unwrap().unwrap().value, "first");
        assert_eq!(second.await.unwrap().unwrap().unwrap().value, "second");
        drop(peer);
        reader.await.unwrap();
    }

    #[tokio::test]
    async fn unknown_response_id_fails_every_pending_request() {
        let (mut peer, stream) = tokio::io::duplex(4096);
        let state = Arc::new(Mutex::new(ConnectionState {
            pending: HashMap::new(),
            failure: None,
            resolutions: HashMap::new(),
            streams: HashMap::new(),
        }));
        let (sender, receiver) = oneshot::channel();
        state.lock().await.pending.insert(
            1,
            PendingCall {
                sender,
                context: None,
            },
        );
        let reader_state = state.clone();
        let reader = tokio::spawn(async move {
            route_responses(stream, &reader_state, None, "test".to_owned(), None).await;
        });

        write_frame(
            &mut peer,
            &Response::Tool {
                request_id: 99,
                result: output("orphaned"),
            },
        )
        .await
        .unwrap();

        assert!(matches!(
            receiver.await.unwrap(),
            Err(RemoteError::Protocol(message))
                if message.contains("unknown request ID 99")
        ));
        reader.await.unwrap();
    }

    #[tokio::test]
    async fn response_eof_fails_every_pending_request() {
        let (peer, stream) = tokio::io::duplex(4096);
        let state = Arc::new(Mutex::new(ConnectionState {
            pending: HashMap::new(),
            failure: None,
            resolutions: HashMap::new(),
            streams: HashMap::new(),
        }));
        let (sender, receiver) = oneshot::channel();
        state.lock().await.pending.insert(
            1,
            PendingCall {
                sender,
                context: None,
            },
        );
        let reader_state = state.clone();
        let reader = tokio::spawn(async move {
            route_responses(stream, &reader_state, None, "test".to_owned(), None).await;
        });

        drop(peer);

        assert!(matches!(
            receiver.await.unwrap(),
            Err(RemoteError::Protocol(message))
                if message.contains("closed before replying")
        ));
        reader.await.unwrap();
    }
}

#[async_trait::async_trait]
impl TargetSession for PooledConnection {
    async fn execute(
        &self,
        name: String,
        arguments: serde_json::Value,
        context: &ToolContext,
    ) -> Result<ToolOutput, RemoteError> {
        call_tool(self, name, arguments, context).await
    }
    async fn resolve_ssh(
        &self,
        target: TargetDefinition,
    ) -> Result<super::ssh::ResolvedSsh, RemoteError> {
        let (sender, receiver) = oneshot::channel();
        let mut writer = self.writer.lock().await;
        let request_id = writer.next_request_id;
        writer.next_request_id = request_id
            .checked_add(1)
            .ok_or_else(|| RemoteError::Protocol("request IDs exhausted".into()))?;
        self.state
            .lock()
            .await
            .resolutions
            .insert(request_id, sender);
        write_frame(
            &mut writer.input,
            &Request::ResolveSsh {
                request_id,
                target: Box::new(target),
            },
        )
        .await?;
        drop(writer);
        receiver
            .await
            .map_err(|_| RemoteError::Protocol("configuration channel closed".into()))?
    }
    async fn open_ssh(
        self: Arc<Self>,
        route: Vec<TargetDefinition>,
        command: String,
    ) -> Result<super::transport::Transport, RemoteError> {
        self.open_stream(route, command).await
    }
}
impl PreparedConnection {
    pub(crate) async fn resolve_ssh(
        &self,
        target: TargetDefinition,
    ) -> Result<super::ssh::ResolvedSsh, RemoteError> {
        self.connection.resolve_ssh(target).await
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
        let mut writer = self.writer.lock().await;
        let channel = writer.next_request_id;
        writer.next_request_id = channel
            .checked_add(1)
            .ok_or_else(|| RemoteError::Protocol("request IDs exhausted".into()))?;
        let credit = Arc::new(tokio::sync::Semaphore::new(16));
        self.state.lock().await.streams.insert(
            channel,
            ClientStream {
                output: sender,
                credit: credit.clone(),
            },
        );
        write_frame(
            &mut writer.input,
            &Request::OpenSsh {
                channel,
                route,
                command,
            },
        )
        .await?;
        drop(writer);
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
#[path = "integration.rs"]
mod integration;
