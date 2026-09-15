use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Arc,
};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::RwLock;

use super::{SshOptions, TargetConfig, TargetType};

pub const ROOT_TARGET: &str = "root";

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, Serialize, PartialEq, Eq)]
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
    pub r#type: TargetType,
    pub host: String,
    pub origin: String,
    pub ssh: SshOptions,
    pub ssh_alias: String,
    pub resolved: Option<crate::remote::ssh::ResolvedSsh>,
    pub workspace: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    pub source: TargetSource,
    pub revision: u64,
    pub generated_key: Option<String>,
}

impl TargetDefinition {
    pub fn from_config(
        name: String,
        config: TargetConfig,
        source: TargetSource,
    ) -> Result<Self, TargetError> {
        validate_name(&name)?;
        validate_endpoint(&config.host, config.ssh.user.as_deref())?;
        if config.ssh.port == Some(0) {
            return Err(TargetError::InvalidPort);
        }
        if config.via.as_deref() == Some(ROOT_TARGET) {
            return Err(TargetError::RootCannotBeJump);
        }
        Ok(Self {
            name,
            r#type: config.r#type.into(),
            ssh_alias: config.host.clone(),
            host: config.host,
            origin: ROOT_TARGET.to_owned(),
            ssh: config.ssh,
            resolved: None,
            workspace: config.workspace,
            via: config.via,
            source,
            revision: 1,
            generated_key: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn test(name: &str, workspace: impl Into<PathBuf>, via: Option<&str>) -> Self {
        Self::from_config(
            name.to_owned(),
            TargetConfig {
                r#type: super::TargetConfigType::Ssh,
                host: format!("{name}.example.com"),
                ssh: SshOptions::default(),
                workspace: workspace.into(),
                via: via.map(str::to_owned),
            },
            TargetSource::Config,
        )
        .unwrap()
    }
}

#[derive(Clone, Debug, JsonSchema, Serialize, PartialEq, Eq)]
pub struct TargetRecord {
    pub name: String,
    /// local identifies the Skyhook session host; ssh identifies a remote target.
    pub r#type: TargetType,
    pub origin: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh_alias: Option<String>,
    pub source: TargetSource,
    pub host: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    pub workspace: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    pub auth: &'static str,
}

impl From<&TargetDefinition> for TargetRecord {
    fn from(value: &TargetDefinition) -> Self {
        Self {
            name: value.name.clone(),
            r#type: value.r#type,
            origin: value.origin.clone(),
            ssh_alias: Some(value.ssh_alias.clone()),
            source: value.source,
            host: value.host.clone(),
            user: value
                .resolved
                .as_ref()
                .map(|r| r.user.clone())
                .or_else(|| value.ssh.user.clone()),
            port: value.resolved.as_ref().map(|r| r.port).or(value.ssh.port),
            workspace: value.workspace.clone(),
            via: value.via.clone(),
            auth: value.ssh.auth.kind(),
        }
    }
}

#[derive(Clone, Default)]
pub struct TargetRegistry {
    entries: Arc<RwLock<BTreeMap<String, TargetDefinition>>>,
    /// Serializes updates from preparation through publication, so readers only
    /// wait for the in-memory install, never for the journal append.
    updates: Arc<tokio::sync::Mutex<()>>,
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
            updates: Arc::default(),
        })
    }

    pub async fn list(&self) -> Vec<TargetRecord> {
        let mut records = vec![TargetRecord {
            name: ROOT_TARGET.to_owned(),
            r#type: TargetType::Local,
            origin: ROOT_TARGET.to_owned(),
            ssh_alias: None,
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
        let mut route = walk_route(&entries, name, TargetError::Unknown)?
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        route.reverse();
        Ok(route)
    }

    pub async fn definitions(&self) -> Vec<TargetDefinition> {
        self.entries.read().await.values().cloned().collect()
    }

    pub async fn upsert_many(
        &self,
        definitions: Vec<TargetDefinition>,
    ) -> Result<(Vec<TargetDefinition>, Vec<String>), TargetError> {
        Ok(self.prepare_upsert_many(definitions).await?.publish().await)
    }

    /// Retain the update gate through durable acceptance so publication cannot
    /// fail or allocate a second, different revision after the append.
    pub(super) async fn prepare_upsert_many(
        &self,
        definitions: Vec<TargetDefinition>,
    ) -> Result<PreparedTargetUpdate, TargetError> {
        let update = self.updates.clone().lock_owned().await;
        let mut next = self.entries.read().await.clone();
        let mut saved = Vec::new();
        for mut definition in definitions {
            definition.source = TargetSource::Session;
            // Allocate against the staged batch as well as the live registry:
            // repeated names in one batch must not reuse a route revision.
            definition.revision = next
                .get(&definition.name)
                .map_or(1, |old| old.revision.saturating_add(1));
            next.insert(definition.name.clone(), definition.clone());
            saved.push(definition);
        }
        validate_graph(&next)?;
        let invalidated = saved
            .iter()
            .flat_map(|d| dependants(&next, &d.name))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        Ok(PreparedTargetUpdate {
            _update: update,
            entries: self.entries.clone(),
            next,
            saved,
            invalidated,
        })
    }
}

/// Prepared target definitions and the sole right to install them. Dropping an
/// unaccepted update leaves the registry unchanged. An accepted append owner
/// must retain this value until its corresponding live publication completes.
pub(super) struct PreparedTargetUpdate {
    _update: tokio::sync::OwnedMutexGuard<()>,
    entries: Arc<RwLock<BTreeMap<String, TargetDefinition>>>,
    next: BTreeMap<String, TargetDefinition>,
    saved: Vec<TargetDefinition>,
    invalidated: Vec<String>,
}

impl PreparedTargetUpdate {
    pub(super) fn definitions(&self) -> &[TargetDefinition] {
        &self.saved
    }

    pub(super) async fn publish(self) -> (Vec<TargetDefinition>, Vec<String>) {
        *self.entries.write().await = self.next;
        (self.saved, self.invalidated)
    }
}

fn dependants(entries: &BTreeMap<String, TargetDefinition>, changed: &str) -> Vec<String> {
    entries
        .keys()
        .filter(|name| {
            walk_route(entries, name, TargetError::UnknownJump)
                .is_ok_and(|route| route.iter().any(|target| target.name == changed))
        })
        .cloned()
        .collect()
}

fn validate_graph(entries: &BTreeMap<String, TargetDefinition>) -> Result<(), TargetError> {
    for name in entries.keys() {
        let route = walk_route(entries, name, TargetError::UnknownJump)?;
        let target = &entries[name];
        validate_name(&target.name)?;
        if target.r#type != TargetType::Ssh {
            return Err(TargetError::BuiltinOnly);
        }
        validate_endpoint(&target.host, target.ssh.user.as_deref())?;
        if target.ssh.port == Some(0) {
            return Err(TargetError::InvalidPort);
        }
        for hop in route
            .iter()
            .skip(1)
            .take_while(|hop| hop.name != target.origin)
        {
            if hop.origin != target.origin {
                return Err(TargetError::Origin(format!(
                    "jump {} uses configuration on {}; select origin {} for a shim-owned continuation, or define a jump using configuration on {}",
                    hop.name, hop.origin, hop.name, target.origin
                )));
            }
        }
        if target.origin != ROOT_TARGET
            && !route.iter().skip(1).any(|hop| hop.name == target.origin)
        {
            return Err(TargetError::Origin(format!(
                "{} must occur before {} in its via route",
                target.origin, target.name
            )));
        }
    }
    Ok(())
}

/// Validate known route edges while permitting references supplied at runtime.
pub(super) fn validate_route_cycles(
    entries: &BTreeMap<String, TargetDefinition>,
) -> Result<(), TargetError> {
    for name in entries.keys() {
        walk_partial_route(entries, name, None)?;
    }
    Ok(())
}

fn walk_route<'a>(
    entries: &'a BTreeMap<String, TargetDefinition>,
    name: &str,
    unknown: fn(String) -> TargetError,
) -> Result<Vec<&'a TargetDefinition>, TargetError> {
    walk_partial_route(entries, name, Some(unknown))
}

fn walk_partial_route<'a>(
    entries: &'a BTreeMap<String, TargetDefinition>,
    name: &str,
    unknown: Option<fn(String) -> TargetError>,
) -> Result<Vec<&'a TargetDefinition>, TargetError> {
    let mut route = Vec::new();
    let mut current = name;
    let mut visited = BTreeSet::new();
    loop {
        if !visited.insert(current) {
            return Err(TargetError::Cycle(current.to_owned()));
        }
        let Some(target) = entries.get(current) else {
            return match unknown {
                Some(error) => Err(error(current.to_owned())),
                None => Ok(route),
            };
        };
        route.push(target);
        let Some(via) = target.via.as_deref() else {
            return Ok(route);
        };
        if via == ROOT_TARGET {
            return Err(TargetError::RootCannotBeJump);
        }
        current = via;
    }
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

#[derive(Clone, Debug, Error)]
pub enum TargetError {
    #[error("only the session host root may use type = local; named targets require type = ssh")]
    BuiltinOnly,
    #[error("invalid target origin: {0}")]
    Origin(String),
    #[error("invalid target name `{0}`")]
    InvalidName(String),
    #[error("invalid target hostname `{0}`")]
    InvalidHost(String),
    #[error("SSH port must be between 1 and 65535")]
    InvalidPort,
    #[error("invalid SSH username")]
    InvalidUser,
    #[error("unknown target `{0}`")]
    Unknown(String),
    #[error("target references unknown jump target `{0}`")]
    UnknownJump(String),
    #[error("target route contains a cycle at `{0}`")]
    Cycle(String),
    #[error("`root` identifies the Skyhook session host")]
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
        TargetDefinition::test(name, ".", via)
    }

    #[tokio::test]
    async fn routes_are_outermost_first_and_cycles_are_rejected() {
        let registry = TargetRegistry::from_definitions([
            target("edge", None),
            target("bastion", Some("edge")),
            target("build", Some("bastion")),
        ])
        .unwrap();
        let route = registry.route("build").await.unwrap();
        let names: Vec<_> = route.iter().map(|target| target.name.as_str()).collect();
        assert_eq!(names, ["edge", "bastion", "build"]);
        let mut changed = target("edge", Some("build"));
        changed.source = TargetSource::Session;
        let result = registry.upsert_many(vec![changed]).await;
        assert!(matches!(result, Err(TargetError::Cycle(_))));
    }

    #[tokio::test]
    async fn repeated_names_in_a_batch_receive_distinct_revisions() {
        let registry = TargetRegistry::default();
        let batch = vec![target("build", None), target("build", None)];
        let (saved, _) = registry.upsert_many(batch).await.unwrap();
        let revisions: Vec<_> = saved.iter().map(|target| target.revision).collect();
        assert_eq!(revisions, [1, 2]);
        assert_eq!(registry.get("build").await.unwrap().revision, 2);
    }

    #[tokio::test]
    async fn records_redact_key_paths() {
        let mut value = target("build", None);
        value.ssh.auth = super::super::TargetAuth::Key {
            path: PathBuf::from("secret-key"),
        };
        let registry = TargetRegistry::from_definitions([value]).unwrap();
        let json = serde_json::to_string(&registry.list().await).unwrap();
        assert!(!json.contains("secret-key"));
        assert!(json.contains("\"auth\":\"key\""));
    }

    #[tokio::test]
    async fn batch_registration_is_atomic_and_validates_origins() {
        let registry = TargetRegistry::from_definitions([target("first", None)]).unwrap();
        let before = registry.definitions().await;
        let mut invalid = target("invalid", None);
        invalid.origin = "first".into();
        let result = registry
            .upsert_many(vec![target("added", None), invalid])
            .await;
        assert!(result.is_err());
        assert_eq!(registry.definitions().await, before);
    }

    #[test]
    fn native_segments_cannot_silently_reinterpret_remote_credential_paths() {
        let first = target("first", None);
        let mut remote = target("remote", Some("first"));
        remote.origin = "first".into();
        let invalid = target("destination", Some("remote"));
        let result = TargetRegistry::from_definitions([first, remote, invalid]);
        assert!(matches!(result, Err(TargetError::Origin(_))));
    }
}
