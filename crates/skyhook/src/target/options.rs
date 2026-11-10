//! The `ssh.options` a target definition may set.
use serde::Serialize;
use skyhook_base::enums::NamedEnum;

/// A target field that an `ssh.options` key would duplicate.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq, NamedEnum)]
#[serde(rename_all = "snake_case")]
pub enum TargetField {
    Host,
    #[serde(rename = "ssh.user")]
    SshUser,
    #[serde(rename = "ssh.port")]
    SshPort,
    #[serde(rename = "ssh.key")]
    SshKey,
    Via,
}

/// Options refused because they forward local environment (SendEnv * would include
/// .env secrets) or X11, multiplex connections, replace the worker command,
/// allow prompts or keyboard-interactive authentication, or read other configuration.
const MANAGED_OPTIONS: &[&str] = &[
    "host",
    "match",
    "include",
    "identityagent",
    "addkeystoagent",
    "batchmode",
    "kbdinteractiveauthentication",
    "challengeresponseauthentication",
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

/// What an `ssh.options` key sets.
pub enum SshOption {
    Dedicated(TargetField),
    /// A setting Skyhook manages.
    Reserved,
    ProxyCommand,
    StrictHostKeyChecking,
    UpdateHostKeys,
    ConnectTimeout,
    Free,
}

impl SshOption {
    /// What `key` sets.
    pub fn parse(key: &skyhook_worker::origin::ssh::SshOptionName) -> Self {
        match key.as_str().to_ascii_lowercase().as_str() {
            "user" => Self::Dedicated(TargetField::SshUser),
            "hostname" => Self::Dedicated(TargetField::Host),
            "port" => Self::Dedicated(TargetField::SshPort),
            "identityfile" => Self::Dedicated(TargetField::SshKey),
            "proxyjump" => Self::Dedicated(TargetField::Via),
            "proxycommand" => Self::ProxyCommand,
            "stricthostkeychecking" => Self::StrictHostKeyChecking,
            "updatehostkeys" => Self::UpdateHostKeys,
            "connecttimeout" => Self::ConnectTimeout,
            key if MANAGED_OPTIONS.contains(&key) => Self::Reserved,
            _ => Self::Free,
        }
    }
}
