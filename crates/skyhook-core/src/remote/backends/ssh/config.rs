//! OpenSSH configuration generation and destination resolution.
use crate::remote::{RemoteError, SensitivePromptHandler};
use crate::target::{TargetAuth, TargetDefinition};
use serde::{Deserialize, Serialize};
use std::{io::Write as _, path::Path, process::Stdio, sync::Arc};
use tokio::process::Command;
pub(crate) struct SshConfig {
    pub _directory: tempfile::TempDir,
    path: std::path::PathBuf,
    pub destination: String,
    pub(super) askpass: super::askpass::AskpassServer,
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
