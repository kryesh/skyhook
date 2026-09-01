use std::{collections::BTreeMap, path::PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, de};

use super::{ROOT_TARGET, TargetDefinition, TargetError, TargetSource};

#[derive(Clone, Debug, Default, Serialize)]
pub struct TargetsConfig {
    pub import_ssh_config: bool,
    #[serde(flatten)]
    pub entries: BTreeMap<String, TargetConfig>,
}

impl<'de> Deserialize<'de> for TargetsConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let mut table = BTreeMap::<String, toml::Value>::deserialize(deserializer)?;
        let import_ssh_config = table
            .remove("import_ssh_config")
            .map_or(Ok(false), |value| {
                value
                    .as_bool()
                    .ok_or_else(|| de::Error::custom("targets.import_ssh_config must be a boolean"))
            })?;
        let mut entries = BTreeMap::new();
        for (name, value) in table {
            if name == ROOT_TARGET {
                return Err(de::Error::custom("target name `root` is reserved"));
            }
            let target = value
                .try_into::<TargetConfig>()
                .map_err(de::Error::custom)?;
            entries.insert(name, target);
        }
        Ok(Self {
            import_ssh_config,
            entries,
        })
    }
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

#[derive(Clone, Debug, Deserialize, Serialize)]
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

    #[must_use]
    pub fn key_path(&self) -> Option<&std::path::Path> {
        match self {
            Self::Key { path } | Self::Interactive { path: Some(path) } => Some(path),
            Self::Openssh | Self::Agent | Self::Interactive { path: None } => None,
        }
    }
}

fn default_workspace() -> PathBuf {
    PathBuf::from(".")
}
