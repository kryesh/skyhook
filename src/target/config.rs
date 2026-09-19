use std::{collections::BTreeMap, num::NonZeroU16, path::PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{TargetDefinition, TargetError, TargetSource};

/// Named targets. A later configuration layer replaces a same-named target whole.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(transparent)]
pub struct TargetsConfig {
    pub entries: BTreeMap<String, TargetConfig>,
}

impl TargetsConfig {
    /// Validate only the supplied definitions.
    /// Missing route references may be supplied by later config layers or at runtime.
    pub(crate) fn validate_structure(&self) -> Result<(), TargetError> {
        let definitions = (self.definitions()?.into_iter())
            .map(|definition| (definition.name.clone(), definition))
            .collect();
        super::registry::validate_route_cycles(&definitions)
    }

    pub(crate) fn definitions(&self) -> Result<Vec<TargetDefinition>, TargetError> {
        self.entries
            .iter()
            .map(|(name, config)| {
                TargetDefinition::from_config(name.clone(), config.clone(), TargetSource::Config)
            })
            .collect()
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TargetConfig {
    pub r#type: TargetConfigType,
    /// Hostname or IP address.
    pub host: String,
    #[serde(default)]
    pub ssh: SshOptions,
    /// Default directory on the destination; defaults to its login directory.
    #[serde(default = "default_workspace")]
    pub workspace: PathBuf,
    /// Jump through this target (ProxyJump) within the SSH connection origin starts; the jump must share that origin.
    pub via: Option<String>,
    /// Target whose Skyhook shim starts the SSH connection, using its key paths and agents; defaults to root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

/// Transports supported when creating a named target.
#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, Serialize, PartialEq, Eq)]
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
    /// Remote user; defaults to the username on the SSH origin.
    pub user: Option<String>,
    /// SSH port; defaults to 22.
    pub port: Option<NonZeroU16>,
    /// default offers OpenSSH's default key files; agent offers only keys already in the agent; key offers one private key.
    #[serde(default)]
    pub auth: TargetAuth,
    /// Authenticate with, and forward, the agent the origin inherited in SSH_AUTH_SOCK
    /// instead of a private agent Skyhook runs on the origin; keys are never added to it.
    /// Tool schemas offer it only with the ssh_agent capability.
    #[schemars(skip)]
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub external_agent: bool,
    /// Extra ssh_config options written verbatim, e.g. {"ProxyCommand": "sudo -n -u deploy ssh -W %h:%p gateway.internal"}. User, Port and IdentityFile belong in user, port and auth; ProxyJump in the target's via.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub options: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TargetAuth {
    #[default]
    Default,
    Agent,
    Key {
        /// Private-key path on the SSH origin.
        path: PathBuf,
    },
}

impl TargetAuth {
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Agent => "agent",
            Self::Key { .. } => "key",
        }
    }
}

fn default_workspace() -> PathBuf {
    PathBuf::from(".")
}
