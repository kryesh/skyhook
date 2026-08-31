use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Arc,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::RwLock;

use super::{TargetAuth, TargetConfig};

pub const ROOT_TARGET: &str = "root";

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TargetSource {
    Builtin,
    SshConfig,
    Config,
    Session,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct TargetDefinition {
    pub name: String,
    pub host: String,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub workspace: PathBuf,
    pub via: Option<String>,
    pub auth: TargetAuth,
    pub source: TargetSource,
    pub revision: u64,
}

impl TargetDefinition {
    pub fn from_config(
        name: String,
        config: TargetConfig,
        source: TargetSource,
    ) -> Result<Self, TargetError> {
        validate_name(&name)?;
        validate_endpoint(&config.host, config.user.as_deref())?;
        if config.via.as_deref() == Some(ROOT_TARGET) {
            return Err(TargetError::RootCannotBeJump);
        }
        Ok(Self {
            name,
            host: config.host,
            user: config.user,
            port: config.port,
            workspace: config.workspace,
            via: config.via,
            auth: config.auth,
            source,
            revision: 1,
        })
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct TargetRecord {
    pub name: String,
    pub source: TargetSource,
    pub host: String,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub workspace: PathBuf,
    pub via: Option<String>,
    pub auth: &'static str,
}

impl From<&TargetDefinition> for TargetRecord {
    fn from(value: &TargetDefinition) -> Self {
        Self {
            name: value.name.clone(),
            source: value.source,
            host: value.host.clone(),
            user: value.user.clone(),
            port: value.port,
            workspace: value.workspace.clone(),
            via: value.via.clone(),
            auth: value.auth.kind(),
        }
    }
}

#[derive(Clone, Default)]
pub struct TargetRegistry {
    entries: Arc<RwLock<BTreeMap<String, TargetDefinition>>>,
}

impl TargetRegistry {
    pub fn from_definitions(
        definitions: impl IntoIterator<Item = TargetDefinition>,
    ) -> Result<Self, TargetError> {
        let mut entries = BTreeMap::new();
        for definition in definitions {
            entries.insert(definition.name.clone(), definition);
        }
        validate_graph(&entries)?;
        Ok(Self {
            entries: Arc::new(RwLock::new(entries)),
        })
    }

    pub async fn snapshot(&self) -> Vec<TargetDefinition> {
        self.entries.read().await.values().cloned().collect()
    }

    pub async fn list(&self) -> Vec<TargetRecord> {
        let mut records = vec![TargetRecord {
            name: ROOT_TARGET.to_owned(),
            source: TargetSource::Builtin,
            host: "localhost".to_owned(),
            user: None,
            port: None,
            workspace: PathBuf::from("."),
            via: None,
            auth: "local",
        }];
        records.extend(self.entries.read().await.values().map(TargetRecord::from));
        records
    }

    pub async fn get(&self, name: &str) -> Result<TargetDefinition, TargetError> {
        if name == ROOT_TARGET {
            return Err(TargetError::RootIsLocal);
        }
        self.entries
            .read()
            .await
            .get(name)
            .cloned()
            .ok_or_else(|| TargetError::Unknown(name.to_owned()))
    }

    pub async fn route(&self, name: &str) -> Result<Vec<TargetDefinition>, TargetError> {
        let entries = self.entries.read().await;
        let mut route = Vec::new();
        let mut current = name;
        let mut visited = BTreeSet::new();
        loop {
            if !visited.insert(current.to_owned()) {
                return Err(TargetError::Cycle(current.to_owned()));
            }
            let target = entries
                .get(current)
                .ok_or_else(|| TargetError::Unknown(current.to_owned()))?;
            route.push(target.clone());
            let Some(via) = target.via.as_deref() else {
                break;
            };
            current = via;
        }
        route.reverse();
        Ok(route)
    }

    pub async fn upsert(
        &self,
        mut definition: TargetDefinition,
    ) -> Result<Vec<String>, TargetError> {
        definition.source = TargetSource::Session;
        let mut entries = self.entries.write().await;
        definition.revision = entries
            .get(&definition.name)
            .map_or(1, |previous| previous.revision.saturating_add(1));
        let old = entries.insert(definition.name.clone(), definition.clone());
        if let Err(error) = validate_graph(&entries) {
            if let Some(old) = old {
                entries.insert(definition.name.clone(), old);
            } else {
                entries.remove(&definition.name);
            }
            return Err(error);
        }
        Ok(dependants(&entries, &definition.name))
    }
}

fn dependants(entries: &BTreeMap<String, TargetDefinition>, changed: &str) -> Vec<String> {
    entries
        .keys()
        .filter(|name| {
            let mut current = name.as_str();
            let mut visited = BTreeSet::new();
            while let Some(target) = entries.get(current) {
                if !visited.insert(current) {
                    break;
                }
                if current == changed {
                    return true;
                }
                let Some(via) = target.via.as_deref() else {
                    break;
                };
                current = via;
            }
            false
        })
        .cloned()
        .collect()
}

fn validate_graph(entries: &BTreeMap<String, TargetDefinition>) -> Result<(), TargetError> {
    for name in entries.keys() {
        let mut current = name.as_str();
        let mut visited = BTreeSet::new();
        loop {
            if !visited.insert(current) {
                return Err(TargetError::Cycle(current.to_owned()));
            }
            let target = entries
                .get(current)
                .ok_or_else(|| TargetError::UnknownJump(current.to_owned()))?;
            let Some(via) = target.via.as_deref() else {
                break;
            };
            if via == ROOT_TARGET {
                return Err(TargetError::RootCannotBeJump);
            }
            current = via;
        }
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<(), TargetError> {
    let valid = !name.is_empty()
        && name != ROOT_TARGET
        && name.len() <= 128
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'));
    if valid {
        Ok(())
    } else {
        Err(TargetError::InvalidName(name.to_owned()))
    }
}

fn validate_endpoint(host: &str, user: Option<&str>) -> Result<(), TargetError> {
    let valid = |value: &str| {
        !value.is_empty()
            && !value
                .chars()
                .any(|character| character.is_control() || character.is_whitespace())
    };
    if !valid(host) {
        return Err(TargetError::InvalidHost(host.to_owned()));
    }
    if user.is_some_and(|user| !valid(user)) {
        return Err(TargetError::InvalidUser);
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum TargetError {
    #[error("invalid target name `{0}`")]
    InvalidName(String),
    #[error("invalid target hostname `{0}`")]
    InvalidHost(String),
    #[error("invalid SSH username")]
    InvalidUser,
    #[error("unknown target `{0}`")]
    Unknown(String),
    #[error("target references unknown jump target `{0}`")]
    UnknownJump(String),
    #[error("target route contains a cycle at `{0}`")]
    Cycle(String),
    #[error("`root` is local and is not an SSH target")]
    RootIsLocal,
    #[error("`root` cannot be used as a jump target")]
    RootCannotBeJump,
    #[error("SSH configuration import failed: {0}")]
    Import(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(name: &str, via: Option<&str>) -> TargetDefinition {
        TargetDefinition::from_config(
            name.to_owned(),
            TargetConfig {
                host: format!("{name}.example.com"),
                user: None,
                port: None,
                workspace: PathBuf::from("."),
                via: via.map(str::to_owned),
                auth: TargetAuth::Openssh,
            },
            TargetSource::Config,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn routes_are_outermost_first_and_cycles_are_rejected() {
        let registry = TargetRegistry::from_definitions([
            target("edge", None),
            target("bastion", Some("edge")),
            target("build", Some("bastion")),
        ])
        .unwrap();
        assert_eq!(
            registry
                .route("build")
                .await
                .unwrap()
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>(),
            ["edge", "bastion", "build"]
        );
        let mut changed = target("edge", Some("build"));
        changed.source = TargetSource::Session;
        assert!(matches!(
            registry.upsert(changed).await,
            Err(TargetError::Cycle(_))
        ));
    }

    #[tokio::test]
    async fn records_redact_key_paths() {
        let mut value = target("build", None);
        value.auth = TargetAuth::Key {
            path: PathBuf::from("secret-key"),
        };
        let registry = TargetRegistry::from_definitions([value]).unwrap();
        let json = serde_json::to_string(&registry.list().await).unwrap();
        assert!(!json.contains("secret-key"));
        assert!(json.contains("\"auth\":\"key\""));
    }
}
