//! OpenSSH-specific resolution, authentication and shim bootstrap.
use super::{RemoteError, SensitivePromptHandler};
use crate::target::{TargetAuth, TargetDefinition};
use serde::{Deserialize, Serialize};
use std::{io::Write as _, path::Path, process::Stdio, sync::Arc};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    process::Command,
};
pub(crate) struct SshConfig {
    pub _directory: tempfile::TempDir,
    path: std::path::PathBuf,
    pub destination: String,
    askpass: super::askpass::AskpassServer,
}

impl SshConfig {
    pub async fn create(
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
            let resolved = match &target.resolved {
                Some(value) => value.clone(),
                None => resolve_openssh(target, &source_path).await?,
            };
            writeln!(file, "Host {alias}")?;
            writeln!(file, "  HostName {}", ssh_token(&resolved.host)?)?;
            writeln!(file, "  User {}", ssh_token(&resolved.user)?)?;
            writeln!(file, "  Port {}", resolved.port)?;
            for (key, values) in &resolved.options {
                if !matches!(target.ssh.auth, TargetAuth::Openssh)
                    && matches!(key.as_str(), "identitiesonly" | "preferredauthentications")
                {
                    continue;
                }
                if matches!(
                    key.as_str(),
                    "host"
                        | "hostname"
                        | "user"
                        | "port"
                        | "identityfile"
                        | "identityagent"
                        | "addkeystoagent"
                        | "batchmode"
                        | "proxyjump"
                        | "proxycommand"
                        | "controlmaster"
                        | "controlpath"
                        | "controlpersist"
                        | "forwardagent"
                        | "remotecommand"
                        | "requesttty"
                        | "sessiontype"
                        | "canonicalizehostname"
                ) {
                    continue;
                }
                for value in values {
                    if value.chars().any(char::is_control) {
                        return Err(RemoteError::InvalidSshValue);
                    }
                    let value = value.replace("%n", &target.ssh_alias);
                    writeln!(file, "  {key} {value}")?;
                }
            }
            writeln!(
                file,
                "  ControlMaster no\n  ControlPath none\n  CanonicalizeHostname no"
            )?;
            if let Some(previous) = &previous {
                writeln!(file, "  ProxyJump {previous}")?;
            } else if let Some(proxy_command) = &resolved.proxy_command {
                if proxy_command.chars().any(char::is_control) {
                    return Err(RemoteError::InvalidSshValue);
                }
                writeln!(
                    file,
                    "  ProxyCommand {}",
                    proxy_command.replace("%n", &target.ssh_alias)
                )?;
            } else {
                writeln!(file, "  ProxyJump none")?;
            }
            write_auth(&mut file, target, &resolved)?;
            writeln!(file, "  LogLevel ERROR")?;
            previous = Some(alias);
        }

        file.flush()?;
        let askpass = super::askpass::AskpassServer::start(prompts)?;
        Ok(Self {
            _directory: directory,
            path,
            destination: previous.expect("nonempty route"),
            askpass,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolvedSsh {
    pub host: String,
    pub user: String,
    pub port: u16,
    pub identity_files: Vec<String>,
    pub options: std::collections::BTreeMap<String, Vec<String>>,
    pub proxy_jump: Option<String>,
    pub proxy_command: Option<String>,
}

pub(crate) async fn resolve_openssh(
    target: &TargetDefinition,
    source_config: &Path,
) -> Result<ResolvedSsh, RemoteError> {
    let mut command = Command::new("ssh");
    command.arg("-G").arg("-F").arg(source_config);
    if let Some(user) = &target.ssh.user {
        command.args(["-l", user]);
    }
    if let Some(port) = target.ssh.port {
        command.args(["-p", &port.to_string()]);
    }
    command
        .arg("--")
        .arg(&target.ssh_alias)
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
    let mut options = std::collections::BTreeMap::<String, Vec<String>>::new();
    let mut proxy_jump = None;
    let mut proxy_command = None;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some((key, value)) = line.split_once(' ') else {
            continue;
        };
        options
            .entry(key.to_owned())
            .or_default()
            .push(value.to_owned());
        match key {
            "hostname" => host = Some(value.to_owned()),
            "user" => user = Some(value.to_owned()),
            "port" => port = value.parse().ok(),
            "identityfile" => identity_files.push(value.to_owned()),
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
        options,
        proxy_jump,
        proxy_command,
    })
}

fn write_auth(
    file: &mut std::fs::File,
    target: &TargetDefinition,
    resolved: &ResolvedSsh,
) -> Result<(), RemoteError> {
    let loading = resolved
        .options
        .get("addkeystoagent")
        .and_then(|v| v.first())
        .map_or("yes", |v| {
            if v == "false" || v == "no" {
                "yes"
            } else {
                v.as_str()
            }
        });
    writeln!(
        file,
        "  IdentityAgent SSH_AUTH_SOCK\n  AddKeysToAgent {loading}"
    )?;
    match &target.ssh.auth {
        TargetAuth::Openssh => {
            writeln!(file, "  BatchMode no")?;
            for path in &resolved.identity_files {
                writeln!(
                    file,
                    "  IdentityFile {}",
                    ssh_token(&path.replace("%n", &target.ssh_alias))?
                )?;
            }
        }
        TargetAuth::Agent => {
            writeln!(
                file,
                "  BatchMode no\n  IdentityFile none\n  IdentitiesOnly no\n  IdentityAgent SSH_AUTH_SOCK\n  PreferredAuthentications publickey,password,keyboard-interactive"
            )?;
        }
        TargetAuth::Key { path } => {
            writeln!(
                file,
                "  BatchMode no\n  IdentityFile {}\n  IdentitiesOnly yes\n  PreferredAuthentications publickey,password,keyboard-interactive",
                ssh_token(&path.to_string_lossy())?
            )?;
        }
    }
    Ok(())
}

pub(crate) fn ssh_command(config: &SshConfig, destination: &str) -> Command {
    let mut command = Command::new("ssh");
    command
        .args(["-F"])
        .arg(&config.path)
        .args(["-A", "-T", "--", destination]);
    command.envs(config.askpass.environment());
    command
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

pub(crate) fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub(crate) async fn resolve_local(target: &TargetDefinition) -> Result<ResolvedSsh, RemoteError> {
    let directory = tempfile::tempdir()?;
    let source = directory.path().join("source");
    std::fs::write(
        &source,
        "Include ~/.ssh/config\nInclude /etc/ssh/ssh_config\n",
    )?;
    resolve_openssh(target, &source).await
}

pub(crate) async fn open(
    route: &[TargetDefinition],
    remote_command: &str,
    environment: &super::authentication::ProcessEnvironment,
    prompts: Arc<dyn SensitivePromptHandler>,
) -> Result<super::transport::Transport, RemoteError> {
    let target = route.last().ok_or(RemoteError::EmptyRoute)?;
    let prompts = Arc::new(ContextPrompts {
        inner: prompts,
        target: target.name.clone(),
        origin: target.origin.clone(),
    });
    let config = SshConfig::create(route, prompts).await?;
    let mut command = ssh_command(&config, &config.destination);
    command
        .arg(remote_command)
        .envs(environment)
        .envs(config.askpass.environment())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command.process_group(0);
    let mut child = command.spawn().map_err(RemoteError::start)?;
    let process_group = child.id().and_then(|id| i32::try_from(id).ok());
    let input = child
        .stdin
        .take()
        .ok_or(RemoteError::MissingPipe("stdin"))?;
    let output = child
        .stdout
        .take()
        .ok_or(RemoteError::MissingPipe("stdout"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or(RemoteError::MissingPipe("stderr"))?;
    let cancellation = tokio_util::sync::CancellationToken::new();
    let cancelled = cancellation.clone();
    let (sender, completion) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let diagnostics = async {
            let mut diagnostics = Vec::new();
            let mut bytes = [0; 4096];
            while let Ok(count) = stderr.read(&mut bytes).await {
                if count == 0 {
                    break;
                }
                let keep = count.min(8192usize.saturating_sub(diagnostics.len()));
                diagnostics.extend_from_slice(&bytes[..keep]);
            }
            diagnostics
        };
        let process = async {
            tokio::select! {
                status = child.wait() => status,
                () = cancelled.cancelled() => {
                    if let Some(group) = process_group {
                        // SAFETY: this SSH process was launched into its own process group.
                        unsafe { libc::kill(-group, libc::SIGKILL); }
                    }
                    let _ = child.kill().await; child.wait().await
                }
            }
        };
        let (status, diagnostics) = tokio::join!(process, diagnostics);
        let result = match status {
            Ok(status) if status.success() => Ok(()),
            Ok(status) => Err(format!(
                "SSH exited with {status}: {}",
                String::from_utf8_lossy(&diagnostics).trim()
            )),
            Err(error) => Err(error.to_string()),
        };
        let _ = sender.send(result);
    });
    Ok(super::transport::Transport {
        input: Box::new(input),
        output: Box::new(super::transport::ProcessReader {
            output,
            completion,
            ended: false,
        }),
        owner: Box::new(super::transport::ProcessOwner {
            cancellation,
            _config: config,
        }),
    })
}

pub(crate) struct SshLauncher {
    pub origin: Option<super::manager::Session>,
    pub route: Vec<TargetDefinition>,
    pub environment: super::authentication::ProcessEnvironment,
    pub prompts: Arc<dyn SensitivePromptHandler>,
}
impl SshLauncher {
    pub(crate) async fn connect(
        &self,
        destination: &TargetDefinition,
        workspace: &Path,
        catalog: &super::EmbeddedShimCatalog,
    ) -> Result<super::transport::Transport, RemoteError> {
        let probe = self.output("uname -s; uname -m", &[]).await?;
        let mut lines = probe.lines();
        let os = lines.next().ok_or(RemoteError::InvalidProbe)?.trim();
        let arch = lines.next().ok_or(RemoteError::InvalidProbe)?.trim();
        let shim = catalog.find(arch, os).ok_or_else(|| {
            if catalog.is_empty() {
                RemoteError::MissingShims
            } else {
                RemoteError::UnsupportedPlatform {
                    arch: arch.into(),
                    os: os.into(),
                }
            }
        })?;
        let hash = shim.sha256();
        let path = format!(".cache/skyhook/shims/{hash}/{}", shim.installed_name());
        let check = format!(
            "test -x \"$HOME/{path}\" && \"$HOME/{path}\" --self-check {hash} && echo skyhook-valid"
        );
        if !self
            .output(&check, &[])
            .await
            .is_ok_and(|s| s.trim() == "skyhook-valid")
        {
            let mut random = [0; 8];
            getrandom::fill(&mut random).map_err(|e| RemoteError::Deployment(e.to_string()))?;
            let temporary = format!(
                ".cache/skyhook/shims/{hash}/.upload-{:016x}",
                u64::from_ne_bytes(random)
            );
            let command = format!(
                "umask 077; d=\"$HOME/.cache/skyhook/shims/{hash}\"; t=\"$HOME/{temporary}\"; mkdir -p \"$d\" && trap 'rm -f \"$t\"' EXIT HUP INT TERM && cat > \"$t\" && chmod 700 \"$t\" && \"$t\" --self-check {hash} && mv -f \"$t\" \"$HOME/{path}\" && echo skyhook-installed"
            );
            if self.output(&command, &shim.bytes).await?.trim() != "skyhook-installed" {
                return Err(RemoteError::Deployment("shim installation failed".into()));
            }
        }
        let command = format!(
            "r=$(cd -- {} && pwd -P) && cd -- {} && exec \"$HOME/{path}\" --serve \"$r\"",
            shell_quote(&destination.workspace.to_string_lossy()),
            shell_quote(&workspace.to_string_lossy())
        );
        self.open(&command).await
    }

    async fn open(&self, command: &str) -> Result<super::transport::Transport, RemoteError> {
        if let Some(origin) = &self.origin {
            origin
                .clone()
                .open_ssh(self.route.clone(), command.to_owned())
                .await
        } else {
            super::ssh::open(
                &self.route,
                command,
                &self.environment,
                self.prompts.clone(),
            )
            .await
        }
    }
    async fn output(&self, command: &str, bytes: &[u8]) -> Result<String, RemoteError> {
        let super::transport::Transport {
            mut input,
            mut output,
            owner: _owner,
        } = self.open(command).await?;
        let write = async move {
            input.write_all(bytes).await?;
            input.shutdown().await?;
            drop(input);
            Ok::<(), std::io::Error>(())
        };
        let read = async {
            let mut bytes = Vec::new();
            (&mut output)
                .take(1024 * 1024)
                .read_to_end(&mut bytes)
                .await?;
            Ok::<_, std::io::Error>(String::from_utf8_lossy(&bytes).into_owned())
        };
        let (_, output) = tokio::try_join!(write, read)?;
        Ok(output)
    }
}

struct ContextPrompts {
    inner: Arc<dyn SensitivePromptHandler>,
    target: String,
    origin: String,
}
impl SensitivePromptHandler for ContextPrompts {
    fn prompt(&self, mut prompt: super::SensitivePrompt) -> super::SensitivePromptFuture {
        prompt.message = format!(
            "[target={} origin={}] {}",
            self.target, self.origin, prompt.message
        );
        self.inner.prompt(prompt)
    }
}
