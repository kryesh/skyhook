//! OpenSSH configuration generated only from a route's checked hops.
use super::route::{Hop, HopAuth, SshRoute, SshValue};
use crate::origin::{
    ProcessEnvironment,
    error::{SshError, TransportError},
};
use std::{ffi::OsString, io::Write as _};

use super::authentication::{AUTH_SOCK, user_agent};

pub(crate) struct SshConfig {
    pub _directory: tempfile::TempDir,
    path: std::path::PathBuf,
    destination: String,
}

impl SshConfig {
    /// Write one host block per hop. `-F` excludes user and system SSH configuration,
    /// so every setting comes from the route. The hop typing the route's secret
    /// prompts once for it, which the SSH process's terminal answers; every other hop
    /// runs in batch mode, so authentication that would prompt fails. No hop asks
    /// whether to trust a host key.
    pub fn create(route: &SshRoute, agent: &OriginAgent) -> Result<Self, TransportError> {
        let directory = tempfile::Builder::new().prefix("skyhook-ssh-").tempdir()?;
        let path = directory.path().join("config");
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)?;
        // A Windows file keeps the access list of the temporary directory, the user's own.
        #[cfg(unix)]
        file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
        let mut previous = None::<String>;
        for (index, (hop, auth)) in route.hops().enumerate() {
            let alias = format!("skyhook-target-{index}");
            writeln!(file, "Host {alias}\n  HostName {}", ssh_token(&hop.host))?;
            if let Some(user) = &hop.user {
                writeln!(file, "  User {}", token(user.as_str()))?;
            }
            if let Some(port) = hop.port {
                writeln!(file, "  Port {port}")?;
            }
            if let Some(previous) = &previous {
                writeln!(file, "  ProxyJump {previous}")?;
            }
            write_auth(&mut file, hop, &auth, agent)?;
            let prompts = match auth.prompts() {
                true => {
                    "BatchMode no\n  NumberOfPasswordPrompts 1\n  KbdInteractiveAuthentication no"
                }
                false => "BatchMode yes",
            };
            writeln!(
                file,
                "  {prompts}\n  StrictHostKeyChecking {}\n  LogLevel ERROR",
                hop.host_keys
            )?;
            if let Some(updates) = hop.host_key_updates {
                writeln!(file, "  UpdateHostKeys {updates}")?;
            }
            // Written verbatim, as in an ssh_config file. The first value wins, so
            // options add settings but cannot replace those above.
            for (key, value) in &hop.options {
                writeln!(file, "  {} {}", key.as_str(), value.as_str())?;
            }
            previous = Some(alias);
        }
        file.flush()?;
        Ok(Self {
            _directory: directory,
            path,
            destination: previous.ok_or(SshError::InvalidValue)?,
        })
    }

    /// SSH's arguments, running `remote_command` at the destination. -F selects only
    /// this configuration (including for ProxyJump), so neither user nor system config
    /// can reintroduce SendEnv/SetEnv.
    pub(crate) fn arguments(&self, remote_command: &str) -> [OsString; 6] {
        [
            "-F".into(),
            self.path.clone().into(),
            "-T".into(),
            "--".into(),
            self.destination.as_str().into(),
            remote_command.into(),
        ]
    }
}

/// The SSH agent of the machine starting SSH: the user's own, otherwise Skyhook's
/// private agent there once a route needs it, where the origin offers one.
pub(crate) enum OriginAgent {
    User(String),
    Private(String),
    Absent,
}

impl OriginAgent {
    /// This machine's agent, the private one as `environment` names it.
    pub(crate) fn of(environment: &ProcessEnvironment) -> Self {
        match (user_agent(), environment.get(AUTH_SOCK)) {
            (Some(socket), _) => Self::User(socket),
            (None, Some(socket)) => Self::Private(socket.clone()),
            (None, None) => Self::Absent,
        }
    }
}

fn write_auth(
    file: &mut std::fs::File,
    hop: &Hop,
    auth: &HopAuth<'_>,
    agent: &OriginAgent,
) -> Result<(), TransportError> {
    // Name sockets explicitly: OpenSSH replaces its own SSH_AUTH_SOCK before
    // starting jump hops, so an inherited variable is not stable across hops.
    // Only Skyhook's own agent is given keys.
    let socket =
        |socket: &str| SshValue::try_from(socket.to_owned()).map(|socket| path_token(&socket));
    let identity_agent = match agent {
        OriginAgent::User(path) => format!("{}\n  AddKeysToAgent no", socket(path)?),
        OriginAgent::Private(path) => format!("{}\n  AddKeysToAgent yes", socket(path)?),
        OriginAgent::Absent if matches!(auth, HopAuth::Agent) => {
            let target = hop.target.clone();
            return Err(SshError::NoAgent { target }.into());
        }
        OriginAgent::Absent if hop.forward_agent => {
            let target = hop.target.clone();
            return Err(SshError::NoAgentToForward { target }.into());
        }
        OriginAgent::Absent => "none".to_owned(),
    };
    let forward = if hop.forward_agent { "yes" } else { "no" };
    writeln!(
        file,
        "  IdentityAgent {identity_agent}\n  ForwardAgent {forward}"
    )?;
    match auth {
        HopAuth::Agent => {
            writeln!(
                file,
                "  IdentityFile none\n  IdentitiesOnly no\n  PreferredAuthentications publickey"
            )?;
        }
        HopAuth::Key(path) | HopAuth::EncryptedKey(path) => {
            writeln!(
                file,
                "  IdentityFile {}\n  IdentitiesOnly yes\n  PreferredAuthentications publickey",
                path_token(path)
            )?;
        }
        HopAuth::Password => {
            writeln!(
                file,
                "  IdentityFile none\n  PreferredAuthentications password"
            )?;
        }
    }
    Ok(())
}

/// A checked value as one quoted configuration token.
fn ssh_token(value: &SshValue) -> String {
    token(value.as_str())
}

fn token(value: &str) -> String {
    let value = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{value}\"")
}

/// A path on this machine as one configuration token. Win32-OpenSSH 8.1 reads a
/// quoted backslash literally and later releases read it as an escape, while every
/// release reads `/` as a separator, so Windows paths are written with `/`.
fn path_token(path: &SshValue) -> String {
    #[cfg(windows)]
    return token(&path.as_str().replace('\\', "/"));
    #[cfg(unix)]
    ssh_token(path)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    /// A hop decoded as a route on the wire carries it.
    pub(crate) fn hop(name: &str, fields: serde_json::Value) -> Hop {
        let mut hop = json!({"name": name, "host": format!("{name}.test"), "port": null,
            "user": null, "key": null, "forward_agent": false, "host_keys": "yes", "options": []});
        hop.as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        serde_json::from_value(hop).unwrap()
    }

    fn private() -> OriginAgent {
        OriginAgent::Private("/managed.sock".into())
    }

    fn blocks(route: &SshRoute, agent: &OriginAgent) -> Vec<String> {
        let config = SshConfig::create(route, agent).unwrap();
        let text = std::fs::read_to_string(&config.path).unwrap();
        text.split("Host ").skip(1).map(str::to_owned).collect()
    }

    #[tokio::test]
    async fn generated_config_comes_only_from_the_route() {
        let jump = hop("jump", json!({"user": "gate", "port": 2222}));
        let options = [["IdentitiesOnly", "no"], ["ServerAliveInterval", "15"]];
        let db = hop(
            "db",
            json!({"key": "/keys/db", "host_keys": "accept-new", "options": options}),
        );
        let route = SshRoute::Plain(vec![jump, db]);
        let config = SshConfig::create(&route, &private()).unwrap();
        let text = std::fs::read_to_string(&config.path).unwrap();
        for required in [
            "HostName \"jump.test\"",
            "User \"gate\"",
            "Port 2222",
            "ProxyJump skyhook-target-0",
            "IdentityFile \"/keys/db\"",
            "ServerAliveInterval 15",
            "StrictHostKeyChecking accept-new",
        ] {
            assert!(text.contains(required), "{required}: {text}");
        }
        // Generated settings precede options, so an option cannot weaken them.
        let key = text.split("Host ").last().unwrap();
        assert!(key.find("IdentitiesOnly yes") < key.find("IdentitiesOnly no"));
        let args = config.arguments("true");
        assert_eq!(
            args[..2],
            [OsString::from("-F"), config.path.clone().into()]
        );
        // A route decoded from the wire cannot carry what a configuration line cannot
        // hold.
        let decodes = |fields| {
            let mut hop = serde_json::to_value(hop("jump", json!({}))).unwrap();
            hop.as_object_mut()
                .unwrap()
                .extend(serde_json::from_value::<serde_json::Map<_, _>>(fields).unwrap());
            serde_json::from_value::<Hop>(hop).is_ok()
        };
        assert!(!decodes(json!({"host": "jump\nHost evil"})));
        assert!(!decodes(json!({"options": [["Send Env", "*"]]})));
        assert!(!decodes(json!({"options": [["SendEnv", "a\nb"]]})));
        assert!(decodes(json!({"options": [["ProxyCommand", "nc %h %p"]]})));
    }

    /// Each hop offers only what its fields name, prompts only for its secret,
    /// gives keys only to Skyhook's own agent and forwards one only when asked.
    #[tokio::test]
    async fn each_hop_authenticates_as_its_fields_say() {
        let passphrase = serde_json::from_value(json!({"env": "PASSPHRASE"})).unwrap();
        let route = SshRoute::Secret {
            before: vec![hop("agent", json!({}))],
            hop: hop("key", json!({"key": "/k", "forward_agent": true})),
            credential: passphrase,
            after: Vec::new(),
        };
        let user = OriginAgent::User("/user.sock".into());
        let expected = [
            "IdentityAgent \"/user.sock\"\n  AddKeysToAgent no\n  ForwardAgent no\n  IdentityFile none\n  IdentitiesOnly no\n  PreferredAuthentications publickey\n  BatchMode yes\n  StrictHostKeyChecking yes",
            "ForwardAgent yes\n  IdentityFile \"/k\"\n  IdentitiesOnly yes\n  PreferredAuthentications publickey\n  BatchMode no\n  NumberOfPasswordPrompts 1\n  KbdInteractiveAuthentication no\n  StrictHostKeyChecking yes",
        ];
        for (block, expected) in blocks(&route, &user).iter().zip(expected) {
            assert!(block.contains(expected), "{expected}: {block}");
        }
        let private = blocks(&route, &private());
        assert!(private[1].contains("\"/managed.sock\"\n  AddKeysToAgent yes"));
        // Agent keys and forwarding need an agent; a key or a password needs none.
        let rejected = SshConfig::create(&route, &OriginAgent::Absent);
        assert!(matches!(
            rejected,
            Err(TransportError::Ssh(SshError::NoAgent { .. }))
        ));
        let forwarding = SshRoute::Plain(vec![hop(
            "key",
            json!({"key": "/k", "forward_agent": true}),
        )]);
        let rejected = SshConfig::create(&forwarding, &OriginAgent::Absent);
        assert!(matches!(
            rejected,
            Err(TransportError::Ssh(SshError::NoAgentToForward { .. }))
        ));
        let key = SshRoute::Plain(vec![hop("key", json!({"key": "/k"}))]);
        assert!(blocks(&key, &OriginAgent::Absent)[0].contains("IdentityAgent none"));
        let password = serde_json::from_value(json!("pw")).unwrap();
        let alone = SshRoute::Secret {
            before: Vec::new(),
            hop: hop("password", json!({})),
            credential: password,
            after: Vec::new(),
        };
        let expected = "IdentityAgent none\n  ForwardAgent no\n  IdentityFile none\n  PreferredAuthentications password\n  BatchMode no\n  NumberOfPasswordPrompts 1\n  KbdInteractiveAuthentication no";
        assert!(blocks(&alone, &OriginAgent::Absent)[0].contains(expected));
    }
}
