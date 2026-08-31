use std::{collections::HashMap, io::Write as _, path::Path, process::Stdio, sync::Arc};

use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
};

use crate::{
    remote::{
        ArtifactError, EmbeddedShim, EmbeddedShimCatalog, RejectSensitivePrompts,
        SensitivePromptHandler,
    },
    target::{TargetAuth, TargetDefinition, TargetError, TargetRegistry},
    tool::builtins::ProcessOutput,
};

use super::protocol::{
    PROTOCOL_VERSION, ProcessRequest, Request, Response, read_frame, write_frame,
};
use super::protocol::{RemoteAgentSpec, RemoteAgentStep};

#[derive(Clone)]
pub struct RemoteManager {
    inner: Arc<RemoteInner>,
}

struct RemoteInner {
    targets: TargetRegistry,
    catalog: EmbeddedShimCatalog,
    pool: Mutex<HashMap<String, Arc<PooledConnection>>>,
    prompts: Arc<dyn SensitivePromptHandler>,
}

struct PooledConnection {
    fingerprint: String,
    io: Mutex<ConnectionIo>,
    _config: tempfile::TempDir,
}

struct ConnectionIo {
    child: Child,
    input: ChildStdin,
    output: ChildStdout,
}

impl RemoteManager {
    #[must_use]
    pub fn new(targets: TargetRegistry) -> Self {
        Self {
            inner: Arc::new(RemoteInner {
                targets,
                catalog: EmbeddedShimCatalog,
                pool: Mutex::new(HashMap::new()),
                prompts: Arc::new(RejectSensitivePrompts),
            }),
        }
    }

    #[must_use]
    pub fn with_prompt_handler(mut self, prompts: Arc<dyn SensitivePromptHandler>) -> Self {
        if let Some(inner) = Arc::get_mut(&mut self.inner) {
            inner.prompts = prompts;
        } else {
            self = Self {
                inner: Arc::new(RemoteInner {
                    targets: self.inner.targets.clone(),
                    catalog: self.inner.catalog,
                    pool: Mutex::new(HashMap::new()),
                    prompts,
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

    pub async fn execute(
        &self,
        target: &str,
        request: ProcessRequest,
    ) -> Result<ProcessOutput, RemoteError> {
        let connection = self.connection(target, None).await?;
        let result = call_process(&connection, request).await;
        if result.is_err() {
            self.inner.pool.lock().await.remove(target);
        }
        result
    }

    pub async fn agent_start(
        &self,
        target: &str,
        workspace: Option<&Path>,
        id: String,
        spec: RemoteAgentSpec,
    ) -> Result<RemoteAgentStep, RemoteError> {
        self.agent_call(target, workspace, Request::AgentStart { id, spec })
            .await
    }

    pub async fn execute_tool(
        &self,
        target: &str,
        workspace: Option<&Path>,
        name: String,
        arguments: serde_json::Value,
    ) -> Result<crate::tool::ToolOutput, RemoteError> {
        let connection = self.connection(target, workspace).await?;
        let mut io = connection.io.lock().await;
        write_frame(&mut io.input, &Request::Tool { name, arguments }).await?;
        match read_frame::<_, Response>(&mut io.output).await? {
            Some(Response::Tool { result: Ok(output) }) => Ok(output.into()),
            Some(Response::Tool { result: Err(error) } | Response::Error { message: error }) => {
                Err(RemoteError::Remote(error))
            }
            Some(response) => Err(RemoteError::Protocol(format!(
                "unexpected tool response: {response:?}"
            ))),
            None => Err(RemoteError::Protocol(
                "shim closed before replying".to_owned(),
            )),
        }
    }

    pub async fn agent_provider(
        &self,
        target: &str,
        workspace: Option<&Path>,
        id: String,
        message: Option<crate::provider::protocol::Message>,
        chunks: Vec<crate::provider::protocol::ResponseChunk>,
    ) -> Result<RemoteAgentStep, RemoteError> {
        self.agent_call(
            target,
            workspace,
            Request::AgentProvider {
                id,
                message,
                chunks,
            },
        )
        .await
    }

    pub async fn agent_tools(
        &self,
        target: &str,
        workspace: Option<&Path>,
        id: String,
        results: Vec<crate::provider::protocol::ToolResult>,
    ) -> Result<RemoteAgentStep, RemoteError> {
        self.agent_call(target, workspace, Request::AgentTools { id, results })
            .await
    }

    async fn agent_call(
        &self,
        target: &str,
        workspace: Option<&Path>,
        request: Request,
    ) -> Result<RemoteAgentStep, RemoteError> {
        let connection = self.connection(target, workspace).await?;
        let mut io = connection.io.lock().await;
        write_frame(&mut io.input, &request).await?;
        match read_frame::<_, Response>(&mut io.output).await? {
            Some(Response::Agent { result: Ok(step) }) => Ok(step),
            Some(Response::Agent { result: Err(error) } | Response::Error { message: error }) => {
                Err(RemoteError::Remote(error))
            }
            Some(response) => Err(RemoteError::Protocol(format!(
                "unexpected agent response: {response:?}"
            ))),
            None => Err(RemoteError::Protocol(
                "shim closed before replying".to_owned(),
            )),
        }
    }

    async fn connection(
        &self,
        target: &str,
        workspace: Option<&Path>,
    ) -> Result<Arc<PooledConnection>, RemoteError> {
        let route = self.inner.targets.route(target).await?;
        let workspace_key =
            workspace.map_or_else(String::new, |path| path.to_string_lossy().into_owned());
        let key = if workspace_key.is_empty() {
            target.to_owned()
        } else {
            format!("{target}\0{workspace_key}")
        };
        let fingerprint = format!("{}:{workspace_key}", route_fingerprint(&route)?);
        let mut pool = self.inner.pool.lock().await;
        if let Some(existing) = pool.get(&key)
            && existing.fingerprint == fingerprint
        {
            return Ok(existing.clone());
        }
        let connection = Arc::new(self.connect(&route, workspace, fingerprint).await?);
        pool.insert(key, connection.clone());
        Ok(connection)
    }

    async fn connect(
        &self,
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
        let shim =
            self.inner
                .catalog
                .find(arch, os)?
                .ok_or_else(|| RemoteError::UnsupportedPlatform {
                    arch: arch.to_owned(),
                    os: os.to_owned(),
                })?;
        let remote_path = ensure_shim(&config, &destination, &shim).await?;
        let workspace = workspace_override
            .unwrap_or(&route.last().ok_or(RemoteError::EmptyRoute)?.workspace)
            .to_string_lossy();
        let remote_command = format!(
            "cd -- {} && exec \"$HOME/{}\" --serve",
            shell_quote(&workspace),
            remote_path
        );
        let mut command = ssh_command(&config, &destination);
        command
            .arg(remote_command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(RemoteError::Start)?;
        let input = child
            .stdin
            .take()
            .ok_or(RemoteError::MissingPipe("stdin"))?;
        let output = child
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
        let connection = PooledConnection {
            fingerprint,
            io: Mutex::new(ConnectionIo {
                child,
                input,
                output,
            }),
            _config: config.directory,
        };
        {
            let mut io = connection.io.lock().await;
            write_frame(
                &mut io.input,
                &Request::Hello {
                    version: PROTOCOL_VERSION,
                },
            )
            .await?;
            match read_frame::<_, Response>(&mut io.output).await? {
                Some(Response::Hello {
                    version: PROTOCOL_VERSION,
                    ..
                }) => {}
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
        }
        Ok(connection)
    }
}

async fn call_process(
    connection: &PooledConnection,
    request: ProcessRequest,
) -> Result<ProcessOutput, RemoteError> {
    let mut io = connection.io.lock().await;
    if let Some(status) = io.child.try_wait()? {
        return Err(RemoteError::Protocol(format!(
            "SSH connection exited with {status}"
        )));
    }
    write_frame(&mut io.input, &Request::Execute(request)).await?;
    match read_frame::<_, Response>(&mut io.output).await? {
        Some(Response::Process { result: Ok(output) }) => Ok(output),
        Some(Response::Process { result: Err(error) } | Response::Error { message: error }) => {
            Err(RemoteError::Remote(error))
        }
        Some(response) => Err(RemoteError::Protocol(format!(
            "unexpected process response: {response:?}"
        ))),
        None => Err(RemoteError::Protocol(
            "shim closed before replying".to_owned(),
        )),
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
    let directory = format!(".cache/skyhook/shims/{PROTOCOL_VERSION}/{hash}");
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
    #[error("remote process failed: {0}")]
    Remote(String),
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
