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
    pub host: String,
    pub user: Option<String>,
    pub port: Option<u16>,
    #[serde(default = "default_workspace")]
    pub workspace: PathBuf,
    pub via: Option<String>,
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
        path: PathBuf,
    },
    Interactive {
        path: Option<PathBuf>,
    },
}

impl TargetAuth {
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Openssh => "openssh",
            Self::Agent => "agent",
            Self::Key { .. } => "key",
            Self::Interactive { .. } => "interactive",
        }
    }

    #[must_use]
    pub const fn permits_secret_prompt(&self) -> bool {
        matches!(self, Self::Interactive { .. })
    }
}

fn default_workspace() -> PathBuf {
    PathBuf::from(".")
}
