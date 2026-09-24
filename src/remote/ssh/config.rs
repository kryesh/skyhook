//! OpenSSH configuration generated only from target definitions.
use crate::remote::{
    ProtocolError, RemoteError, SensitivePromptHandler, SshError, backend::ProcessEnvironment,
};
use crate::target::{TargetAuth, TargetDefinition, TargetError};
use std::{io::Write as _, path::Path, sync::Arc};
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
    /// Write one host block per hop. `-F` excludes user and system SSH configuration,
    /// so every setting comes from registry-validated target definitions.
    /// `external_agent` is the SSH_AUTH_SOCK this process inherited, if any: Skyhook's
    /// own on root, or the agent forwarded to a remote origin's shim.
    pub fn create(
        route: &[TargetDefinition],
        environment: &ProcessEnvironment,
        external_agent: Option<&str>,
        prompts: Arc<dyn SensitivePromptHandler>,
    ) -> Result<Self, RemoteError> {
        if route.is_empty() {
            return Err(RemoteError::EmptyRoute);
        }
        // A route deserialized from the wire has not passed through the registry.
        if route.iter().any(|hop| hop.validate().is_err()) {
            return Err(ProtocolError::Violation("unsupported transport route").into());
        }
        let directory = tempfile::Builder::new().prefix("skyhook-ssh-").tempdir()?;
        let path = directory.path().join("config");
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
            writeln!(
                file,
                "Host {alias}\n  HostName {}",
                ssh_token(&target.host)?
            )?;
            if let Some(user) = &target.ssh.user {
                writeln!(file, "  User {}", ssh_token(user)?)?;
            }
            if let Some(port) = target.ssh.port {
                writeln!(file, "  Port {port}")?;
            }
            if let Some(previous) = &previous {
                writeln!(file, "  ProxyJump {previous}")?;
            }
            write_auth(&mut file, target, environment, external_agent)?;
            writeln!(file, "  LogLevel ERROR")?;
            // Written verbatim, as in an ssh_config file. The first value wins, so
            // options add settings but cannot replace those above.
            for (key, value) in &target.ssh.options {
                writeln!(file, "  {key} {value}")?;
            }
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

/// Options refused because they forward local environment (SendEnv * would include
/// .env secrets) or X11, multiplex connections, replace the worker command,
/// disable prompts, or read other configuration.
const MANAGED_OPTIONS: &[&str] = &[
    "host",
    "match",
    "include",
    "identityagent",
    "addkeystoagent",
    "batchmode",
    "controlmaster",
    "controlpath",
    "controlpersist",
    "forwardagent",
    "remotecommand",
    "requesttty",
    "sessiontype",
    "canonicalizehostname",
    "sendenv",
    "setenv",
    "forwardx11",
    "forwardx11trusted",
];

/// Validate target options without retaining their potentially sensitive values.
pub(crate) fn validate_option(key: &str, value: &str) -> Result<(), TargetError> {
    if key.is_empty() || !key.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return Err(TargetError::InvalidSshOptionName);
    }
    let field = match key.to_ascii_lowercase().as_str() {
        "user" => Some("ssh.user"),
        "hostname" => Some("host"),
        "port" => Some("ssh.port"),
        "identityfile" => Some("ssh.auth"),
        "proxyjump" => Some("via"),
        _ => None,
    };
    if let Some(field) = field {
        return Err(TargetError::DedicatedSshOption(key.to_owned(), field));
    }
    if MANAGED_OPTIONS
        .iter()
        .any(|option| key.eq_ignore_ascii_case(option))
    {
        return Err(TargetError::ReservedSshOption(key.to_owned()));
    }
    if value.is_empty() {
        return Err(TargetError::EmptySshOptionValue(key.to_owned()));
    }
    if value.chars().any(char::is_control) {
        return Err(TargetError::InvalidSshOptionValue(key.to_owned()));
    }
    Ok(())
}

fn write_auth(
    file: &mut std::fs::File,
    target: &TargetDefinition,
    environment: &ProcessEnvironment,
    external_agent: Option<&str>,
) -> Result<(), RemoteError> {
    // Name sockets explicitly: OpenSSH replaces its own SSH_AUTH_SOCK before
    // starting jump hops, so an inherited variable is not stable across hops.
    let agent = if target.ssh.external_agent {
        let socket = external_agent.ok_or_else(|| SshError::ExternalAgentUnavailable {
            target: target.name.to_string(),
        })?;
        // Never add keys to an agent Skyhook does not own.
        format!("{}\n  AddKeysToAgent no", ssh_token(socket)?)
    } else if let Some(socket) = environment.get("SSH_AUTH_SOCK") {
        format!("{}\n  AddKeysToAgent yes", ssh_token(socket)?)
    } else {
        "none".to_owned()
    };
    writeln!(file, "  IdentityAgent {agent}")?;
    match &target.ssh.auth {
        TargetAuth::Default => {}
        TargetAuth::Agent => {
            writeln!(
                file,
                "  IdentityFile none\n  IdentitiesOnly no\n  PreferredAuthentications publickey,password,keyboard-interactive"
            )?;
        }
        TargetAuth::Key { path } => {
            writeln!(
                file,
                "  IdentityFile {}\n  IdentitiesOnly yes\n  PreferredAuthentications publickey,password,keyboard-interactive",
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
    // -A forwards the destination's configured agent, not arbitrary environment values.
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
        return Err(SshError::InvalidValue.into());
    }
    Ok(format!(
        "\"{}\"",
        value.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

pub(crate) fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::prompt::RejectSensitivePrompts;
    use serde_json::json;

    fn target(name: &str, config: serde_json::Value) -> Result<TargetDefinition, TargetError> {
        let config = serde_json::from_value(config).unwrap();
        TargetDefinition::from_config(name.into(), config)
    }

    fn managed() -> ProcessEnvironment {
        ProcessEnvironment::from([("SSH_AUTH_SOCK".into(), "/managed.sock".into())])
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

    #[tokio::test]
    async fn generated_config_comes_only_from_target_definitions() {
        let jump =
            json!({"type": "ssh", "host": "jump.test", "ssh": {"user": "gate", "port": 2222}});
        let destination = json!({"type": "ssh", "host": "db.test", "via": "jump", "ssh": {
            "auth": {"kind": "key", "path": "/keys/db"},
            "options": {"IdentitiesOnly": "no", "ServerAliveInterval": "15"},
        }});
        let route = [
            target("jump", jump).unwrap(),
            target("db", destination).unwrap(),
        ];
        let prompts = Arc::new(RejectSensitivePrompts);
        let config = SshConfig::create(&route, &managed(), None, prompts).unwrap();
        let text = std::fs::read_to_string(&config.path).unwrap();
        for required in [
            "HostName \"jump.test\"",
            "User \"gate\"",
            "Port 2222",
            "IdentityAgent \"/managed.sock\"\n  AddKeysToAgent yes",
            "ProxyJump skyhook-target-0",
            "IdentityFile \"/keys/db\"",
            "ServerAliveInterval 15",
        ] {
            assert!(text.contains(required), "{required}: {text}");
        }
        // Generated settings precede options, so an option cannot weaken them.
        assert!(text.find("IdentitiesOnly yes") < text.find("IdentitiesOnly no"));
        let command = ssh_command(&config, &config.destination);
        let args: Vec<_> = command.as_std().get_args().collect();
        assert_eq!(
            args[..2],
            [std::ffi::OsStr::new("-F"), config.path.as_os_str()]
        );
        assert!(args.contains(&std::ffi::OsStr::new("-A")));
        // A route from the wire has not passed the registry; unvalidated hops are refused.
        let mut unvalidated = route[0].clone();
        unvalidated.host = "jump host".into();
        let rejected = SshConfig::create(
            &[unvalidated],
            &managed(),
            None,
            Arc::new(RejectSensitivePrompts),
        );
        assert!(matches!(rejected, Err(RemoteError::Protocol(_))));

        let proxy = json!({"type": "ssh", "host": "x", "via": "jump", "ssh": {"options": {"ProxyCommand": "nc %h %p"}}});
        assert!(matches!(
            target("x", proxy),
            Err(TargetError::ProxyCommandWithVia(_))
        ));
        let forwarding = json!({"type": "ssh", "host": "x", "ssh": {"options": {"SendEnv": "*"}}});
        assert!(matches!(
            target("x", forwarding),
            Err(TargetError::ReservedSshOption(_))
        ));
        assert!(matches!(
            validate_option("proxyjump", "a"),
            Err(TargetError::DedicatedSshOption(_, "via"))
        ));
        assert!(matches!(
            validate_option("Server Alive", "1"),
            Err(TargetError::InvalidSshOptionName)
        ));
        assert!(matches!(
            validate_option("LogLevel", ""),
            Err(TargetError::EmptySshOptionValue(_))
        ));
        assert!(matches!(
            validate_option("ProxyCommand", "nc\n"),
            Err(TargetError::InvalidSshOptionValue(_))
        ));
        // Managed options with a field of their own name it.
        let user = json!({"type": "ssh", "host": "x", "ssh": {"options": {"User": "debian"}}});
        let error = target("x", user).unwrap_err().to_string();
        assert!(error.contains("`ssh.user"), "{error}");
    }

    #[tokio::test]
    async fn external_agent_hops_use_the_session_host_agent_without_adding_keys() {
        let route = [
            json!({"type": "ssh", "host": "jump.test", "ssh": {"external_agent": true}}),
            json!({"type": "ssh", "host": "managed.test", "via": "jump"}),
            json!({"type": "ssh", "host": "db.test", "via": "managed", "ssh": {"external_agent": true}}),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, config)| target(&format!("hop{index}"), config).unwrap())
        .collect::<Vec<_>>();
        let prompts = || Arc::new(RejectSensitivePrompts);
        let config = SshConfig::create(&route, &managed(), Some("/user.sock"), prompts()).unwrap();
        let text = std::fs::read_to_string(&config.path).unwrap();
        let blocks: Vec<_> = text.split("Host ").skip(1).collect();
        for (block, agent) in blocks.iter().zip([
            "\"/user.sock\"\n  AddKeysToAgent no",
            "\"/managed.sock\"\n  AddKeysToAgent yes",
            "\"/user.sock\"\n  AddKeysToAgent no",
        ]) {
            assert!(block.contains(&format!("IdentityAgent {agent}")), "{block}");
        }
        assert!(SshConfig::create(&route, &managed(), None, prompts()).is_err());
    }
}
