use std::{collections::BTreeMap, path::PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{TargetDefinition, TargetError, TargetSource};

#[derive(Clone, Debug, Default, Deserialize)]
pub struct TargetsConfig {
    #[serde(default)]
    pub import_ssh_config: bool,
    #[serde(flatten)]
    pub entries: BTreeMap<String, TargetConfig>,
}

impl TargetsConfig {
    pub(crate) fn definitions(&self) -> Result<Vec<TargetDefinition>, TargetError> {
        self.entries
            .iter()
            .map(|(name, config)| {
                TargetDefinition::from_config(name.clone(), config.clone(), TargetSource::Config)
            })
            .collect()
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TargetConfig {
    pub r#type: TargetConfigType,
    /// Hostname, IP, or SSH alias resolved on origin.
    pub host: String,
    #[serde(default)]
    pub ssh: SshOptions,
    /// Default directory on the destination; defaults to its login directory.
    #[serde(default = "default_workspace")]
    pub workspace: PathBuf,
    /// Reach the destination through this target. Omit to infer routing from origin and SSH ProxyJump.
    pub via: Option<String>,
}

/// Transports supported when creating a named target.
#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TargetConfigType {
    Ssh,
}

impl From<TargetConfigType> for TargetType {
    fn from(value: TargetConfigType) -> Self {
        match value {
            TargetConfigType::Ssh => Self::Ssh,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TargetType {
    Local,
    Ssh,
}

impl TargetType {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Ssh => "ssh",
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SshOptions {
    /// Override the SSH configuration's user.
    pub user: Option<String>,
    /// Override the SSH configuration's port.
    pub port: Option<u16>,
    /// openssh uses SSH configuration; agent uses Skyhook's agent; key selects a private-key path on origin.
    #[serde(default)]
    pub auth: TargetAuth,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TargetAuth {
    #[default]
    Openssh,
    Agent,
    Key {
        /// Private-key path on origin.
        path: PathBuf,
    },
}

impl TargetAuth {
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Openssh => "openssh",
            Self::Agent => "agent",
            Self::Key { .. } => "key",
        }
    }
}

fn default_workspace() -> PathBuf {
    PathBuf::from(".")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn types_are_required_and_interactivity_is_not_target_configuration() {
        for input in [
            r#"{"host":"host"}"#,
            r#"{"type":"local","host":"host"}"#,
            r#"{"type":"winrm","host":"host"}"#,
            r#"{"type":"ssh","host":"host","ssh":{"auth":{"kind":"interactive"}}}"#,
            r#"{"type":"ssh","host":"host","interactive":true}"#,
        ] {
            assert!(
                serde_json::from_str::<TargetConfig>(input).is_err(),
                "accepted {input}"
            );
        }
        let config: TargetConfig = serde_json::from_str(
            r#"{"type":"ssh","host":"alias","ssh":{"user":"user","port":2222}}"#,
        )
        .unwrap();
        assert_eq!(config.ssh.port, Some(2222));
        assert!(
            toml::from_str::<TargetsConfig>("[host]\ntype = 'local'\nhost = 'localhost'").is_err()
        );
        assert!(toml::from_str::<TargetsConfig>("[host]\ntype = 'ssh'\nhost = 'alias'").is_ok());
    }
}
