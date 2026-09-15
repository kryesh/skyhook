//! OpenSSH configuration generation and destination resolution.
use crate::remote::{RemoteError, SensitivePromptHandler};
use crate::target::{TargetAuth, TargetDefinition};
use serde::{Deserialize, Serialize};
use std::{io::Write as _, path::Path, process::Stdio, sync::Arc};
use tokio::process::Command;
/// OpenSSH config and forwarded environment are UTF-8 protocols. Reject a
/// native path that cannot be represented instead of redirecting authority.
pub(super) fn wire_path(path: &Path) -> std::io::Result<&str> {
    path.to_str().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "native path cannot be represented losslessly in SSH configuration",
        )
    })
}

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
                // Also enforce this for pre-resolved routes received over shim RPC.
                if forwards_environment(key) {
                    continue;
                }
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
                "  ControlMaster no\n  ControlPath none\n  CanonicalizeHostname no\n  ForwardX11 no\n  ForwardX11Trusted no"
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

// SSH clients retain their local environment for authentication and proxy commands,
// but must not export it to a target. In particular, SendEnv * would include secrets
// loaded from the invocation directory's .env. Drop SetEnv before resolved options
// enter a route/RPC as well: its literal values can themselves contain local secrets.
// X11 forwarding exports a derived DISPLAY and is not part of our worker protocol.
fn forwards_environment(key: &str) -> bool {
    ["sendenv", "setenv", "forwardx11", "forwardx11trusted"]
        .iter()
        .any(|option| key.eq_ignore_ascii_case(option))
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
    parse_resolved_ssh(&String::from_utf8_lossy(&output.stdout))
}

fn parse_resolved_ssh(output: &str) -> Result<ResolvedSsh, RemoteError> {
    let mut host = None;
    let mut user = None;
    let mut port = None;
    let mut identity_files = Vec::new();
    let mut options = std::collections::BTreeMap::<String, Vec<String>>::new();
    let mut proxy_jump = None;
    let mut proxy_command = None;
    for line in output.lines() {
        let Some((key, value)) = line.split_once(' ') else {
            continue;
        };
        if forwards_environment(key) {
            continue;
        }
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
                ssh_token(wire_path(path)?)?
            )?;
        }
    }
    Ok(())
}

pub(crate) fn ssh_command(config: &SshConfig, destination: &str) -> Command {
    // -F selects only our generated configuration (including for ProxyJump), so
    // neither user nor system config can reintroduce SendEnv/SetEnv. Do not clear
    // the local client environment: HOME, PATH and authentication helpers need it.
    // -A deliberately forwards the session agent, not arbitrary environment values.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        remote::prompt::RejectSensitivePrompts,
        target::{SshOptions, TargetConfig, TargetConfigType, TargetSource},
    };

    fn resolved() -> ResolvedSsh {
        parse_resolved_ssh(
            "hostname example.test\nuser remote-user\nport 22\n\
         identityfile ~/.ssh/id_ed25519\nidentitiesonly yes\n\
         sendenv *\nSendEnv SKYHOOK_TEST_DOTENV_KEY\n\
         setenv SKYHOOK_TEST_CONFIG_KEY=local-config-secret\n\
         SeTeNv SKYHOOK_TEST_DOTENV_KEY=local-dotenv-secret\n\
         forwardx11 yes\nForwardX11Trusted yes\n",
        )
        .unwrap()
    }

    fn target(name: &str) -> TargetDefinition {
        let mut target = TargetDefinition::from_config(
            name.into(),
            TargetConfig {
                r#type: TargetConfigType::Ssh,
                host: "example.test".into(),
                workspace: "/remote/work".into(),
                via: None,
                ssh: SshOptions::default(),
            },
            TargetSource::Config,
        )
        .unwrap();
        target.resolved = Some(resolved());
        target
    }

    #[cfg(unix)]
    #[test]
    fn ssh_wire_paths_reject_unrepresentable_native_bytes() {
        use std::os::unix::ffi::OsStringExt as _;
        let path =
            std::path::PathBuf::from(std::ffi::OsString::from_vec(b"/tmp/key-\xff".to_vec()));
        assert_eq!(
            wire_path(&path).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
        assert_eq!(wire_path(Path::new("/tmp/key-é")).unwrap(), "/tmp/key-é");
    }

    #[test]
    fn resolved_options_never_put_environment_forwarding_on_the_wire() {
        let target = target("destination");
        let resolved = target.resolved.as_ref().unwrap();
        assert_eq!(resolved.identity_files, ["~/.ssh/id_ed25519"]);
        assert_eq!(resolved.options["identitiesonly"], ["yes"]);
        assert!(
            resolved
                .options
                .keys()
                .all(|key| !forwards_environment(key))
        );

        // Resolved targets travel in both ResolveSsh responses and OpenSsh routes.
        // Local SetEnv literals must not appear in either serialized representation.
        for wire in [
            serde_json::to_string(resolved).unwrap(),
            serde_json::to_string(&crate::remote::protocol::Request::OpenSsh {
                channel: crate::remote::protocol::RequestId::FIRST,
                route: vec![target],
                command: "exec remote-worker --serve /remote/work".into(),
            })
            .unwrap(),
        ] {
            for forbidden in [
                "SKYHOOK_TEST_",
                "local-config-secret",
                "local-dotenv-secret",
            ] {
                assert!(!wire.contains(forbidden), "{wire}");
            }
        }
    }

    #[tokio::test]
    async fn generated_config_filters_pre_resolved_forwarding_for_every_hop() {
        let mut route = vec![target("jump"), target("destination")];
        for target in &mut route {
            // Defense in depth for externally supplied/pre-resolved options. SSH
            // option names are case-insensitive even if ssh -G normally lowers them.
            let options = &mut target.resolved.as_mut().unwrap().options;
            for key in ["SendEnv", "sendenv", "SENDENV"] {
                options.insert(key.into(), vec!["* SKYHOOK_TEST_DOTENV_KEY".into()]);
            }
            for key in ["SetEnv", "setenv", "SETENV"] {
                options.insert(key.into(), vec!["SKYHOOK_TEST_CONFIG_KEY=secret".into()]);
            }
            options.insert("ForwardX11".into(), vec!["yes".into()]);
            options.insert("ForwardX11Trusted".into(), vec!["yes".into()]);
        }
        let config = SshConfig::create(&route, Arc::new(RejectSensitivePrompts))
            .await
            .unwrap();
        let text = std::fs::read_to_string(&config.path).unwrap();
        let lower = text.to_ascii_lowercase();
        for forbidden in [
            "sendenv",
            "setenv",
            "secret",
            "skyhook_test_",
            "include",
            "forwardx11 yes",
        ] {
            assert!(!lower.contains(forbidden), "{forbidden}: {text}");
        }
        assert_eq!(lower.matches("forwardx11 no").count(), route.len());
        assert_eq!(lower.matches("forwardx11trusted no").count(), route.len());
        for required in [
            "ProxyJump skyhook-target-0",
            "IdentityFile \"~/.ssh/id_ed25519\"",
            "IdentityAgent SSH_AUTH_SOCK",
        ] {
            assert!(text.contains(required), "{required}: {text}");
        }

        let command = ssh_command(&config, &config.destination);
        let args: Vec<_> = command.as_std().get_args().collect();
        assert_eq!(args[0], "-F");
        assert_eq!(args[1], config.path.as_os_str());
        // Agent forwarding is a deliberate internal protocol, not SendEnv.
        assert!(args.contains(&std::ffi::OsStr::new("-A")));
        assert!(args.contains(&std::ffi::OsStr::new("-T")));
        // No local environment values or secrets belong in remote command arguments.
        assert!(
            args.iter()
                .all(|arg| !arg.to_string_lossy().contains("secret"))
        );
    }
}
