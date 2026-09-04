use std::{
    collections::HashMap,
    io::Write as _,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
};

use futures_util::future::{BoxFuture, FutureExt as _, Shared};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _},
    process::{Child, ChildStdin, Command},
    sync::{Mutex, oneshot},
    task::JoinHandle,
};

use crate::{
    job::CancellationToken,
    remote::{ArtifactError, EmbeddedShim, EmbeddedShimCatalog, SensitivePromptHandler},
    target::{ResolvedRoute, RouteIdentity, TargetAuth, TargetDefinition, TargetError},
    tool::{
        ToolContext, ToolError, ToolOutput,
        authorization::{AuthorizationCoordinator, AuthorizationError},
        policy::{PermissionUse, PolicyDecision},
    },
};

use super::protocol::{
    RemoteToolError, RemoteToolOutput, Request, Response, read_frame, write_frame,
};

#[derive(Clone)]
pub(crate) struct RemoteManager {
    inner: Arc<RemoteInner>,
}

struct RemoteInner {
    catalog: EmbeddedShimCatalog,
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
    child: Mutex<Child>,
    reader: JoinHandle<()>,
    _config: tempfile::TempDir,
}

struct PooledSlot {
    connection: Shared<BoxFuture<'static, Result<Arc<PooledConnection>, RemoteError>>>,
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
    connection: Arc<PooledConnection>,
}

impl PreparedConnection {
    pub(crate) async fn execute(
        self,
        name: String,
        arguments: serde_json::Value,
        context: &ToolContext,
    ) -> Result<ToolOutput, RemoteError> {
        let result = call_tool(&self.connection, name, arguments, context).await;
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
    input: ChildStdin,
    next_request_id: u64,
}

type RemoteToolResult = Result<RemoteToolOutput, RemoteToolError>;
type PendingResult = Result<RemoteToolResult, RemoteError>;

struct PendingCall {
    sender: oneshot::Sender<PendingResult>,
    context: Option<ToolContext>,
}

struct ConnectionState {
    pending: HashMap<u64, PendingCall>,
    failure: Option<RemoteError>,
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
                pool: Mutex::new(HashMap::new()),
                prompts,
                authorization,
                #[cfg(test)]
                factory: None,
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_connection_factory(mut self, factory: Arc<dyn ConnectionFactory>) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("connection factories are installed before sharing a manager")
            .factory = Some(factory);
        self
    }

    pub(crate) async fn connection(
        &self,
        route: ResolvedRoute,
        workspace: &Path,
        cancellation: &CancellationToken,
    ) -> Result<PreparedConnection, RemoteError> {
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
                    #[cfg(test)]
                    if let Some(factory) = manager.inner.factory.clone() {
                        return factory
                            .connect(ConnectionRequest {
                                target: target.clone(),
                                route: definitions.clone(),
                                workspace: workspace.clone(),
                            })
                            .await
                            .map(Arc::new);
                    }
                    manager
                        .connect(&target, &definitions, &workspace)
                        .await
                        .map(Arc::new)
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
    ) -> Result<PooledConnection, RemoteError> {
        let config = SshConfig::create(route, self.inner.prompts.clone()).await?;
        let destination = config.destination.clone();
        let probe = run_ssh_output(&config, &destination, "uname -s; uname -m").await?;
        let mut lines = probe.lines();
        let os = lines.next().ok_or(RemoteError::InvalidProbe)?.trim();
        let arch = lines.next().ok_or(RemoteError::InvalidProbe)?.trim();
        let shim = self.inner.catalog.find(arch, os).ok_or_else(|| {
            if self.inner.catalog.is_empty() {
                RemoteError::MissingShims
            } else {
                RemoteError::UnsupportedPlatform {
                    arch: arch.to_owned(),
                    os: os.to_owned(),
                }
            }
        })?;
        let remote_path = ensure_shim(&config, &destination, &shim).await?;
        let target_workspace = &route.last().ok_or(RemoteError::EmptyRoute)?.workspace;
        let workspace = workspace.to_string_lossy();
        let remote_command = format!(
            "r=$(cd -- {} && pwd -P) && cd -- {} && exec \"$HOME/{}\" --serve \"$r\"",
            shell_quote(&target_workspace.to_string_lossy()),
            shell_quote(&workspace),
            remote_path,
        );
        let mut command = ssh_command(&config, &destination);
        command
            .arg(remote_command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(RemoteError::start)?;
        let mut input = child
            .stdin
            .take()
            .ok_or(RemoteError::MissingPipe("stdin"))?;
        let mut output = child
            .stdout
            .take()
            .ok_or(RemoteError::MissingPipe("stdout"))?;
        if let Some(mut stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut diagnostics = Vec::new();
                let _ = stderr.read_to_end(&mut diagnostics).await;
                if !diagnostics.is_empty() {
                    eprintln!(
                        "remote shim: {}",
                        String::from_utf8_lossy(&diagnostics).trim()
                    );
                }
            });
        }
        write_frame(&mut input, &Request::Hello).await?;
        match read_frame::<_, Response>(&mut output).await? {
            Some(Response::Ready) => {}
            Some(response) => {
                return Err(RemoteError::Protocol(format!(
                    "unexpected handshake: {response:?}"
                )));
            }
            None => {
                return Err(RemoteError::Protocol(
                    "shim closed during handshake".to_owned(),
                ));
            }
        }
        let state = Arc::new(Mutex::new(ConnectionState {
            pending: HashMap::new(),
            failure: None,
        }));
        let writer = Arc::new(Mutex::new(RequestWriter {
            input,
            next_request_id: 1,
        }));
        let reader_state = state.clone();
        let reader_writer = writer.clone();
        let reader_authorization = self.inner.authorization.clone();
        let reader_target = target.to_owned();
        let reader = tokio::spawn(async move {
            route_responses(
                output,
                &reader_state,
                Some((&reader_writer, &reader_authorization)),
                reader_target,
            )
            .await;
        });
        Ok(PooledConnection {
            writer,
            state,
            child: Mutex::new(child),
            reader,
            _config: config.directory,
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
        Err(error) => Err(RemoteError::Remote {
            message: error.message,
            output: error.output.map(Into::into),
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
    host: Option<(&Mutex<RequestWriter>, &AuthorizationCoordinator)>,
    target: String,
) where
    R: AsyncRead + Unpin,
{
    loop {
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
            Response::Tool { request_id, result } => {
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
        std::mem::take(&mut state.pending)
    };
    for pending in pending.into_values() {
        let _ = pending.sender.send(Err(failure.clone()));
    }
}

impl Drop for PooledConnection {
    fn drop(&mut self) {
        self.reader.abort();
        let _ = self.child.get_mut().start_kill();
    }
}

#[cfg(test)]
pub(crate) async fn test_connection() -> PooledConnection {
    let directory = tempfile::tempdir().unwrap();
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
            input,
            next_request_id: 1,
        })),
        state: Arc::new(Mutex::new(ConnectionState {
            pending: HashMap::new(),
            failure: None,
        })),
        child: Mutex::new(child),
        reader: tokio::spawn(std::future::pending()),
        _config: directory,
    }
}

struct SshConfig {
    directory: tempfile::TempDir,
    path: std::path::PathBuf,
    destination: String,
    askpass: super::askpass::AskpassServer,
}

impl SshConfig {
    async fn create(
        route: &[TargetDefinition],
        prompts: Arc<dyn SensitivePromptHandler>,
    ) -> Result<Self, RemoteError> {
        if route.is_empty() {
            return Err(RemoteError::EmptyRoute);
        }
        let directory = tempfile::Builder::new().prefix("skyhook-ssh-").tempdir()?;
        let path = directory.path().join("config");
        let source_path = directory.path().join("user-config");
        {
            let mut source = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&source_path)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                source.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            }
            writeln!(source, "Include ~/.ssh/config")?;
            source.flush()?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        let mut previous = None::<String>;
        for (index, target) in route.iter().enumerate() {
            let alias = format!("skyhook-target-{index}");
            let resolved = resolve_openssh(target, &source_path).await?;
            writeln!(file, "Host {alias}")?;
            writeln!(file, "  HostName {}", ssh_token(&resolved.host)?)?;
            writeln!(file, "  User {}", ssh_token(&resolved.user)?)?;
            writeln!(file, "  Port {}", resolved.port)?;
            writeln!(file, "  HostKeyAlias {}", ssh_token(&resolved.host)?)?;
            if let Some(previous) = &previous {
                writeln!(file, "  ProxyJump {previous}")?;
            } else if route.len() == 1 {
                if let Some(proxy_jump) = &resolved.proxy_jump {
                    writeln!(file, "  ProxyJump {}", ssh_token(proxy_jump)?)?;
                } else if let Some(proxy_command) = &resolved.proxy_command {
                    writeln!(file, "  ProxyCommand {proxy_command}")?;
                }
            }
            write_auth(&mut file, target, &resolved)?;
            writeln!(file, "  LogLevel ERROR")?;
            previous = Some(alias);
        }
        writeln!(file, "Host *\n  Include ~/.ssh/config")?;
        file.flush()?;
        let allow_secrets = route
            .iter()
            .any(|target| target.auth.permits_secret_prompt());
        let askpass = super::askpass::AskpassServer::start(prompts, allow_secrets)?;
        Ok(Self {
            directory,
            path,
            destination: previous.expect("nonempty route"),
            askpass,
        })
    }
}

struct ResolvedSsh {
    host: String,
    user: String,
    port: u16,
    identity_files: Vec<String>,
    identity_agent: Option<String>,
    proxy_jump: Option<String>,
    proxy_command: Option<String>,
}

async fn resolve_openssh(
    target: &TargetDefinition,
    source_config: &Path,
) -> Result<ResolvedSsh, RemoteError> {
    let mut command = Command::new("ssh");
    command.arg("-G").arg("-F").arg(source_config);
    if let Some(user) = &target.user {
        command.args(["-l", user]);
    }
    if let Some(port) = target.port {
        command.args(["-p", &port.to_string()]);
    }
    command
        .arg("--")
        .arg(&target.host)
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true);
    let output = command.output().await.map_err(RemoteError::start)?;
    if !output.status.success() {
        return Err(RemoteError::Resolution(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    let mut host = None;
    let mut user = None;
    let mut port = None;
    let mut identity_files = Vec::new();
    let mut identity_agent = None;
    let mut proxy_jump = None;
    let mut proxy_command = None;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some((key, value)) = line.split_once(' ') else {
            continue;
        };
        match key {
            "hostname" => host = Some(value.to_owned()),
            "user" => user = Some(value.to_owned()),
            "port" => port = value.parse().ok(),
            "identityfile" => identity_files.push(value.to_owned()),
            "identityagent" if value != "none" => identity_agent = Some(value.to_owned()),
            "proxyjump" if value != "none" => proxy_jump = Some(value.to_owned()),
            "proxycommand" if value != "none" => proxy_command = Some(value.to_owned()),
            _ => {}
        }
    }
    Ok(ResolvedSsh {
        host: host.ok_or_else(|| RemoteError::Resolution("ssh -G omitted hostname".to_owned()))?,
        user: user.ok_or_else(|| RemoteError::Resolution("ssh -G omitted user".to_owned()))?,
        port: port.ok_or_else(|| RemoteError::Resolution("ssh -G omitted port".to_owned()))?,
        identity_files,
        identity_agent,
        proxy_jump,
        proxy_command,
    })
}

fn write_auth(
    file: &mut std::fs::File,
    target: &TargetDefinition,
    resolved: &ResolvedSsh,
) -> Result<(), RemoteError> {
    match &target.auth {
        TargetAuth::Openssh => {
            writeln!(file, "  BatchMode yes")?;
            for path in &resolved.identity_files {
                writeln!(file, "  IdentityFile {}", ssh_token(path)?)?;
            }
            if let Some(agent) = &resolved.identity_agent {
                writeln!(file, "  IdentityAgent {}", ssh_token(agent)?)?;
            }
        }
        TargetAuth::Agent => {
            writeln!(
                file,
                "  BatchMode yes\n  IdentityFile none\n  IdentityAgent SSH_AUTH_SOCK\n  PreferredAuthentications publickey\n  PasswordAuthentication no\n  KbdInteractiveAuthentication no"
            )?;
        }
        TargetAuth::Key { path } => {
            writeln!(
                file,
                "  BatchMode yes\n  IdentityFile {}\n  IdentitiesOnly yes\n  PreferredAuthentications publickey",
                ssh_token(&path.to_string_lossy())?
            )?;
        }
        TargetAuth::Interactive { path } => {
            writeln!(file, "  BatchMode no")?;
            if let Some(path) = path {
                writeln!(
                    file,
                    "  IdentityFile {}\n  IdentitiesOnly yes",
                    ssh_token(&path.to_string_lossy())?
                )?;
            }
        }
    }
    Ok(())
}

fn ssh_command(config: &SshConfig, destination: &str) -> Command {
    let mut command = Command::new("ssh");
    command
        .args(["-F"])
        .arg(&config.path)
        .args(["-T", "--", destination]);
    if let Ok(executable) = std::env::current_exe() {
        command
            .env("SSH_ASKPASS", executable)
            .env("SSH_ASKPASS_REQUIRE", "force")
            .env("SKYHOOK_ASKPASS_SOCKET", &config.askpass.socket)
            .env("DISPLAY", "skyhook");
    }
    command
}

async fn run_ssh_output(
    config: &SshConfig,
    destination: &str,
    remote: &str,
) -> Result<String, RemoteError> {
    let mut command = ssh_command(config, destination);
    command
        .arg(remote)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = command.output().await.map_err(RemoteError::start)?;
    if !output.status.success() {
        return Err(RemoteError::Ssh(format!(
            "SSH exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

async fn ensure_shim(
    config: &SshConfig,
    destination: &str,
    shim: &EmbeddedShim,
) -> Result<String, RemoteError> {
    let hash = shim.sha256();
    let name = shim.installed_name();
    let directory = format!(".cache/skyhook/shims/{hash}");
    let path = format!("{directory}/{name}");
    let check = format!("test -x \"$HOME/{path}\" && \"$HOME/{path}\" --self-check {hash}");
    if run_ssh_output(config, destination, &check).await.is_ok() {
        return Ok(path);
    }
    let mut random = [0_u8; 8];
    getrandom::fill(&mut random).map_err(|error| RemoteError::Deployment(error.to_string()))?;
    let temporary = format!("{directory}/.upload-{:016x}", u64::from_ne_bytes(random));
    let remote = format!(
        "umask 077; d=\"$HOME/{directory}\"; t=\"$HOME/{temporary}\"; mkdir -p \"$d\" && trap 'rm -f \"$t\"' EXIT HUP INT TERM && cat > \"$t\" && chmod 700 \"$t\" && \"$t\" --self-check {hash} && mv -f \"$t\" \"$HOME/{path}\""
    );
    let mut command = ssh_command(config, destination);
    command
        .arg(remote)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(RemoteError::start)?;
    let mut input = child
        .stdin
        .take()
        .ok_or(RemoteError::MissingPipe("deployment stdin"))?;
    input.write_all(&shim.bytes).await?;
    input.shutdown().await?;
    drop(input);
    let output = child.wait_with_output().await?;
    if !output.status.success() {
        return Err(RemoteError::Deployment(format!(
            "upload exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(path)
}

fn ssh_token(value: &str) -> Result<String, RemoteError> {
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err(RemoteError::InvalidSshValue);
    }
    Ok(format!(
        "\"{}\"",
        value.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
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
    #[error("could not start SSH: {message}")]
    Start {
        kind: std::io::ErrorKind,
        message: String,
    },
    #[error("SSH configuration resolution failed: {0}")]
    Resolution(String),
    #[error("SSH target connection was denied: {0}")]
    ApprovalDenied(String),
    #[error("SSH target connection returned an invalid approval grant: {0}")]
    ApprovalInvalidGrant(String),
    #[error("SSH target connection requires an unavailable capability")]
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
    #[error("remote tool failed: {message}")]
    Remote {
        message: String,
        output: Option<ToolOutput>,
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

    fn start(error: std::io::Error) -> Self {
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
            Self::Remote {
                message,
                output: Some(output),
            } => ToolError::with_output(message, output),
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
            images: Vec::new(),
        })
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
            route_responses(stream, &reader_state, None, "test".to_owned()).await;
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
            route_responses(stream, &reader_state, None, "test".to_owned()).await;
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
            route_responses(stream, &reader_state, None, "test".to_owned()).await;
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
