use std::{collections::HashMap, io::Write as _, path::Path, process::Stdio, sync::Arc};

use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _},
    process::{Child, ChildStdin, Command},
    sync::{Mutex, oneshot},
    task::JoinHandle,
};

use crate::{
    remote::{
        ArtifactError, EmbeddedShim, EmbeddedShimCatalog, RejectSensitivePrompts,
        SensitivePromptHandler,
    },
    target::{TargetAuth, TargetDefinition, TargetError, TargetRegistry},
    tool::{
        ToolContext, ToolError, ToolOutput,
        policy::{AllowAll, AuthorizationRequest, Policy, PolicyDecision},
    },
};

use super::protocol::{
    RemoteToolError, RemoteToolOutput, Request, Response, read_frame, write_frame,
};

#[derive(Clone)]
pub struct RemoteManager {
    inner: Arc<RemoteInner>,
}

struct RemoteInner {
    targets: TargetRegistry,
    catalog: EmbeddedShimCatalog,
    pool: Mutex<HashMap<String, Arc<PooledConnection>>>,
    prompts: Arc<dyn SensitivePromptHandler>,
    policy: Arc<dyn Policy>,
}

struct PooledConnection {
    fingerprint: String,
    writer: Arc<Mutex<RequestWriter>>,
    state: Arc<Mutex<ConnectionState>>,
    child: Mutex<Child>,
    reader: JoinHandle<()>,
    _config: tempfile::TempDir,
}

struct RequestWriter {
    input: ChildStdin,
    next_request_id: u64,
}

type RemoteToolResult = Result<RemoteToolOutput, RemoteToolError>;
type PendingResult = Result<RemoteToolResult, ConnectionFailure>;

struct PendingCall {
    sender: oneshot::Sender<PendingResult>,
    context: Option<ToolContext>,
}

struct ConnectionState {
    pending: HashMap<u64, PendingCall>,
    failure: Option<ConnectionFailure>,
}

#[derive(Clone, Debug)]
enum ConnectionFailure {
    Io {
        kind: std::io::ErrorKind,
        message: String,
    },
    Protocol(String),
}

impl ConnectionFailure {
    fn io(error: std::io::Error) -> Self {
        Self::Io {
            kind: error.kind(),
            message: error.to_string(),
        }
    }

    fn into_remote_error(self) -> RemoteError {
        match self {
            Self::Io { kind, message } => RemoteError::Io(std::io::Error::new(kind, message)),
            Self::Protocol(message) => RemoteError::Protocol(message),
        }
    }
}

impl RemoteManager {
    #[must_use]
    pub fn new(targets: TargetRegistry) -> Self {
        Self {
            inner: Arc::new(RemoteInner {
                targets,
                catalog: EmbeddedShimCatalog::default(),
                pool: Mutex::new(HashMap::new()),
                prompts: Arc::new(RejectSensitivePrompts),
                policy: Arc::new(AllowAll),
            }),
        }
    }

    /// Replaces the remote shim artifacts available to new connections.
    #[must_use]
    pub fn with_shim_catalog(mut self, catalog: EmbeddedShimCatalog) -> Self {
        if let Some(inner) = Arc::get_mut(&mut self.inner) {
            inner.catalog = catalog;
        } else {
            self = Self {
                inner: Arc::new(RemoteInner {
                    targets: self.inner.targets.clone(),
                    catalog,
                    pool: Mutex::new(HashMap::new()),
                    prompts: self.inner.prompts.clone(),
                    policy: self.inner.policy.clone(),
                }),
            };
        }
        self
    }

    #[must_use]
    pub fn with_prompt_handler(mut self, prompts: Arc<dyn SensitivePromptHandler>) -> Self {
        if let Some(inner) = Arc::get_mut(&mut self.inner) {
            inner.prompts = prompts;
        } else {
            self = Self {
                inner: Arc::new(RemoteInner {
                    targets: self.inner.targets.clone(),
                    catalog: self.inner.catalog.clone(),
                    pool: Mutex::new(HashMap::new()),
                    prompts,
                    policy: self.inner.policy.clone(),
                }),
            };
        }
        self
    }

    #[must_use]
    pub fn with_policy(mut self, policy: Arc<dyn Policy>) -> Self {
        if let Some(inner) = Arc::get_mut(&mut self.inner) {
            inner.policy = policy;
        } else {
            self = Self {
                inner: Arc::new(RemoteInner {
                    targets: self.inner.targets.clone(),
                    catalog: self.inner.catalog.clone(),
                    pool: Mutex::new(HashMap::new()),
                    prompts: self.inner.prompts.clone(),
                    policy,
                }),
            };
        }
        self
    }

    pub async fn invalidate(&self, names: &[String]) {
        let mut pool = self.inner.pool.lock().await;
        pool.retain(|key, _| {
            !names
                .iter()
                .any(|name| key == name || key.starts_with(&format!("{name}\0")))
        });
    }

    pub async fn execute_tool(
        &self,
        target: &str,
        workspace: Option<&Path>,
        name: String,
        arguments: serde_json::Value,
    ) -> Result<ToolOutput, RemoteError> {
        let key = connection_key(target, workspace);
        let connection = self.connection(target, workspace).await?;
        let result = call_tool(&connection, name, arguments, None).await;
        self.finish_call(key, connection, result).await
    }

    pub(crate) async fn execute_tool_cancellable(
        &self,
        target: &str,
        workspace: Option<&Path>,
        name: String,
        arguments: serde_json::Value,
        context: &ToolContext,
    ) -> Result<ToolOutput, RemoteError> {
        let key = connection_key(target, workspace);
        let connection = self.connection(target, workspace).await?;
        let result = call_tool(&connection, name, arguments, Some(context)).await;
        self.finish_call(key, connection, result).await
    }

    async fn finish_call(
        &self,
        key: String,
        connection: Arc<PooledConnection>,
        result: Result<ToolOutput, RemoteError>,
    ) -> Result<ToolOutput, RemoteError> {
        if matches!(result, Err(RemoteError::Io(_) | RemoteError::Protocol(_))) {
            let mut pool = self.inner.pool.lock().await;
            if pool
                .get(&key)
                .is_some_and(|current| Arc::ptr_eq(current, &connection))
            {
                pool.remove(&key);
            }
        }
        result
    }

    async fn connection(
        &self,
        target: &str,
        workspace: Option<&Path>,
    ) -> Result<Arc<PooledConnection>, RemoteError> {
        let route = self.inner.targets.route(target).await?;
        let workspace_key =
            workspace.map_or_else(String::new, |path| path.to_string_lossy().into_owned());
        let key = connection_key(target, workspace);
        let fingerprint = format!("{}:{workspace_key}", route_fingerprint(&route)?);
        let mut pool = self.inner.pool.lock().await;
        if let Some(existing) = pool.get(&key)
            && existing.fingerprint == fingerprint
        {
            return Ok(existing.clone());
        }
        let connection = Arc::new(self.connect(target, &route, workspace, fingerprint).await?);
        pool.insert(key, connection.clone());
        Ok(connection)
    }

    async fn connect(
        &self,
        target: &str,
        route: &[TargetDefinition],
        workspace_override: Option<&Path>,
        fingerprint: String,
    ) -> Result<PooledConnection, RemoteError> {
        let config = SshConfig::create(route, self.inner.prompts.clone()).await?;
        let destination = config.destination.clone();
        let probe = run_ssh_output(&config, &destination, "uname -s; uname -m").await?;
        let mut lines = probe.lines();
        let os = lines.next().ok_or(RemoteError::InvalidProbe)?.trim();
        let arch = lines.next().ok_or(RemoteError::InvalidProbe)?.trim();
        let shim = self.inner.catalog.find(arch, os)?.ok_or_else(|| {
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
        let workspace = workspace_override
            .unwrap_or(target_workspace)
            .to_string_lossy();
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
        let mut child = command.spawn().map_err(RemoteError::Start)?;
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
        let reader_policy = self.inner.policy.clone();
        let reader_target = target.to_owned();
        let reader = tokio::spawn(async move {
            route_responses(
                output,
                &reader_state,
                Some(&reader_writer),
                Some(reader_policy),
                reader_target,
            )
            .await;
        });
        Ok(PooledConnection {
            fingerprint,
            writer,
            state,
            child: Mutex::new(child),
            reader,
            _config: config.directory,
        })
    }
}

fn connection_key(target: &str, workspace: Option<&Path>) -> String {
    workspace.map_or_else(
        || target.to_owned(),
        |path| format!("{target}\0{}", path.to_string_lossy()),
    )
}

async fn call_tool(
    connection: &PooledConnection,
    name: String,
    arguments: serde_json::Value,
    context: Option<&ToolContext>,
) -> Result<ToolOutput, RemoteError> {
    let (request_id, receiver) = {
        let mut writer = connection.writer.lock().await;
        let Some(next_request_id) = writer.next_request_id.checked_add(1) else {
            drop(writer);
            let failure = ConnectionFailure::Protocol("request ID space exhausted".to_owned());
            fail_connection(&connection.state, failure.clone()).await;
            return Err(failure.into_remote_error());
        };
        let request_id = writer.next_request_id;
        writer.next_request_id = next_request_id;
        let (sender, receiver) = oneshot::channel();
        {
            let mut state = connection.state.lock().await;
            if let Some(failure) = state.failure.clone() {
                return Err(failure.into_remote_error());
            }
            state.pending.insert(
                request_id,
                PendingCall {
                    sender,
                    context: context.cloned(),
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
            fail_connection(&connection.state, ConnectionFailure::io(error)).await;
        }
        (request_id, receiver)
    };
    let received = async {
        receiver
            .await
            .map_err(|_| {
                RemoteError::Protocol("remote response dispatcher stopped unexpectedly".to_owned())
            })?
            .map_err(ConnectionFailure::into_remote_error)
    };
    let result = if let Some(context) = context {
        tokio::pin!(received);
        tokio::select! {
            result = &mut received => result?,
            () = context.cancelled() => {
                send_cancel(connection, request_id).await?;
                return Err(RemoteError::Remote {
                    message: "tool was cancelled".to_owned(),
                    output: None,
                });
            }
        }
    } else {
        received.await?
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
        let failure = ConnectionFailure::io(error);
        fail_connection(&connection.state, failure.clone()).await;
        return Err(failure.into_remote_error());
    }
    Ok(())
}

async fn route_responses<R>(
    mut output: R,
    state: &Mutex<ConnectionState>,
    writer: Option<&Mutex<RequestWriter>>,
    policy: Option<Arc<dyn Policy>>,
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
                    ConnectionFailure::Protocol("shim closed before replying".to_owned()),
                )
                .await;
                return;
            }
            Err(error) => {
                fail_connection(state, ConnectionFailure::io(error)).await;
                return;
            }
        };
        match response {
            Response::Tool { request_id, result } => {
                let pending = state.lock().await.pending.remove(&request_id);
                let Some(pending) = pending else {
                    fail_connection(
                        state,
                        ConnectionFailure::Protocol(format!(
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
                effects,
                arguments,
            } => {
                let context = state
                    .lock()
                    .await
                    .pending
                    .get(&request_id)
                    .and_then(|pending| pending.context.clone());
                let decision = if let (Some(context), Some(policy)) = (context, policy.as_ref()) {
                    let authorization = policy.authorize(AuthorizationRequest {
                        agent: context.agent.clone(),
                        job: context.job,
                        parent: None,
                        scope: None,
                        tool,
                        target: target.clone(),
                        effects,
                        arguments,
                    });
                    tokio::pin!(authorization);
                    tokio::select! {
                        decision = &mut authorization => decision,
                        () = context.cancelled() => PolicyDecision::Deny {
                            reason: "tool was cancelled".to_owned(),
                        },
                    }
                } else {
                    PolicyDecision::Deny {
                        reason: "remote path authorization requires a host tool context".to_owned(),
                    }
                };
                let (allowed, reason) = match decision {
                    PolicyDecision::Allow => (true, None),
                    PolicyDecision::Deny { reason } => (false, Some(reason)),
                };
                let Some(writer) = writer else {
                    fail_connection(
                        state,
                        ConnectionFailure::Protocol(
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
                    fail_connection(state, ConnectionFailure::io(error)).await;
                    return;
                }
            }
            Response::Ready => {
                fail_connection(
                    state,
                    ConnectionFailure::Protocol(
                        "received a second remote ready response".to_owned(),
                    ),
                )
                .await;
                return;
            }
        }
    }
}

async fn fail_connection(state: &Mutex<ConnectionState>, failure: ConnectionFailure) {
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
    let output = command.output().await.map_err(RemoteError::Start)?;
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
    let output = command.output().await.map_err(RemoteError::Start)?;
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
    let hash = shim.clone().sha256();
    let name = shim.clone().installed_name();
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
    let mut child = command.spawn().map_err(RemoteError::Start)?;
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

fn route_fingerprint(route: &[TargetDefinition]) -> Result<String, RemoteError> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(route)?)))
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

#[derive(Debug, Error)]
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
    #[error("could not start SSH: {0}")]
    Start(std::io::Error),
    #[error("SSH configuration resolution failed: {0}")]
    Resolution(String),
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
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl RemoteError {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn output(value: &str) -> RemoteToolResult {
        Ok(RemoteToolOutput {
            value: serde_json::json!(value),
            images: Vec::new(),
        })
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
            route_responses(stream, &reader_state, None, None, "test".to_owned()).await;
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
            route_responses(stream, &reader_state, None, None, "test".to_owned()).await;
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
            Err(ConnectionFailure::Protocol(message))
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
            route_responses(stream, &reader_state, None, None, "test".to_owned()).await;
        });

        drop(peer);

        assert!(matches!(
            receiver.await.unwrap(),
            Err(ConnectionFailure::Protocol(message))
                if message.contains("closed before replying")
        ));
        reader.await.unwrap();
    }
}
