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
use crate::tool::policy::{Capability, CapabilitySet};

pub const ROOT_TARGET: &str = "root";

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TargetSource {
    Builtin,
    Config,
    Session,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TargetDefinition {
    pub name: String,
    pub r#type: TargetType,
    pub host: String,
    pub ssh: SshOptions,
    pub workspace: PathBuf,
    /// A native jump (ProxyJump) within the SSH connection `origin` starts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    /// The target whose shim starts the SSH connection; `None` is root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    pub source: TargetSource,
    pub revision: u64,
}

impl TargetDefinition {
    pub fn from_config(
        name: String,
        config: TargetConfig,
        source: TargetSource,
    ) -> Result<Self, TargetError> {
        let definition = Self {
            name,
            r#type: config.r#type.into(),
            host: config.host,
            ssh: config.ssh,
            workspace: config.workspace,
            via: config.via,
            origin: config.origin.filter(|origin| origin != ROOT_TARGET),
            source,
            revision: 1,
        };
        definition.validate()?;
        Ok(definition)
    }

    /// The previous target in this target's route: its jump, otherwise its origin.
    pub fn parent(&self) -> Option<&str> {
        self.via.as_deref().or(self.origin.as_deref())
    }

    fn validate(&self) -> Result<(), TargetError> {
        validate_name(&self.name)?;
        if self.r#type != TargetType::Ssh {
            return Err(TargetError::BuiltinOnly);
        }
        validate_endpoint(&self.host, self.ssh.user.as_deref())?;
        if [&self.via, &self.origin]
            .into_iter()
            .any(|link| link.as_deref() == Some(ROOT_TARGET))
        {
            return Err(TargetError::RootCannotBeJump);
        }
        if self.via.is_some() && self.via == self.origin {
            return Err(TargetError::ViaIsOrigin(self.name.clone()));
        }
        if let Some((key, _)) = (self.ssh.options.iter())
            .find(|(key, value)| !crate::remote::ssh::configurable_option(key, value))
        {
            return Err(TargetError::InvalidSshOption(key.clone()));
        }
        let proxy_command =
            (self.ssh.options.keys()).any(|key| key.eq_ignore_ascii_case("proxycommand"));
        if proxy_command && self.via.is_some() {
            return Err(TargetError::ProxyCommandWithVia(self.name.clone()));
        }
        Ok(())
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
                origin: None,
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
    pub source: TargetSource,
    pub host: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    pub workspace: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    pub auth: &'static str,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub external_agent: bool,
}

impl From<&TargetDefinition> for TargetRecord {
    fn from(value: &TargetDefinition) -> Self {
        Self {
            name: value.name.clone(),
            r#type: value.r#type,
            source: value.source,
            host: value.host.clone(),
            user: value.ssh.user.clone(),
            port: value.ssh.port.map(std::num::NonZeroU16::get),
            workspace: value.workspace.clone(),
            via: value.via.clone(),
            origin: value.origin.clone(),
            auth: value.ssh.auth.kind(),
            external_agent: value.ssh.external_agent,
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

    /// Targets visible with `capabilities`, starting with root.
    pub async fn list(&self, capabilities: &CapabilitySet) -> Vec<TargetRecord> {
        let entries = self.entries.read().await;
        let visible = |target: &&TargetDefinition| {
            capabilities.contains(Capability::SshAgent) || !needs_ssh_agent(&entries, &target.name)
        };
        let mut records = vec![TargetRecord {
            name: ROOT_TARGET.to_owned(),
            r#type: TargetType::Local,
            source: TargetSource::Builtin,
            host: "localhost".to_owned(),
            user: None,
            port: None,
            workspace: PathBuf::from("."),
            via: None,
            origin: None,
            auth: "local",
            external_agent: false,
        }];
        records.extend(entries.values().filter(visible).map(TargetRecord::from));
        records
    }

    /// Whether reaching `name` forwards an agent Skyhook does not own. Such targets
    /// are invisible to callers without the ssh_agent capability.
    pub async fn needs_ssh_agent(&self, name: &str) -> bool {
        needs_ssh_agent(&*self.entries.read().await, name)
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

fn needs_ssh_agent(entries: &BTreeMap<String, TargetDefinition>, name: &str) -> bool {
    walk_route(entries, name, TargetError::UnknownJump)
        .is_ok_and(|route| route.iter().any(|hop| hop.ssh.external_agent))
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
    for (name, target) in entries {
        let route = walk_route(entries, name, TargetError::UnknownJump)?;
        target.validate()?;
        // Jumps belong to the connection their origin starts, so a jump's key paths
        // and agent are never reinterpreted on another machine. The origin ends the
        // chain: jumps without via link to it.
        let origin = target.origin.as_deref();
        for jump in (route.iter().skip(1)).take_while(|hop| Some(hop.name.as_str()) != origin) {
            if jump.origin != target.origin {
                return Err(TargetError::OriginMismatch {
                    target: name.clone(),
                    jump: jump.name.clone(),
                    origin: origin.unwrap_or(ROOT_TARGET).to_owned(),
                    jump_origin: jump.origin.as_deref().unwrap_or(ROOT_TARGET).to_owned(),
                });
            }
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
        let Some(parent) = target.parent() else {
            return Ok(route);
        };
        // Validated definitions never link to root.
        current = parent;
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
    #[error("invalid target name `{0}`")]
    InvalidName(String),
    #[error("invalid target hostname `{0}`")]
    InvalidHost(String),
    #[error("SSH option `{0}` is invalid or reserved by Skyhook")]
    InvalidSshOption(String),
    #[error("target `{0}` sets ProxyCommand, which cannot be combined with via")]
    ProxyCommandWithVia(String),
    #[error("target `{0}` sets via to its origin; omit via to connect directly from the origin")]
    ViaIsOrigin(String),
    #[error("target name `{0}` is unavailable")]
    NameUnavailable(String),
    #[error(
        "target `{target}` starts SSH from `{origin}`, but its jump `{jump}` starts from `{jump_origin}`; give them the same origin"
    )]
    OriginMismatch {
        target: String,
        jump: String,
        origin: String,
        jump_origin: String,
    },
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
        let records = registry.list(&CapabilitySet::default()).await;
        let json = serde_json::to_string(&records).unwrap();
        assert!(!json.contains("secret-key"));
        assert!(json.contains("\"auth\":\"key\""));
    }

    #[tokio::test]
    async fn batch_registration_is_atomic() {
        let registry = TargetRegistry::from_definitions([target("first", None)]).unwrap();
        let before = registry.definitions().await;
        let invalid = target("invalid", Some("missing"));
        let result = registry
            .upsert_many(vec![target("added", None), invalid])
            .await;
        assert!(result.is_err());
        assert_eq!(registry.definitions().await, before);
    }

    #[tokio::test]
    async fn jumps_share_the_origin_that_starts_their_connection() {
        let shim = target("shim", None);
        let mut jump = target("jump", None);
        jump.origin = Some("shim".into());
        let mut destination = target("destination", Some("jump"));
        let mismatched = [shim.clone(), jump.clone(), destination.clone()];
        assert!(matches!(
            TargetRegistry::from_definitions(mismatched),
            Err(TargetError::OriginMismatch { jump_origin, .. }) if jump_origin == "shim"
        ));
        // A jump reached from root cannot continue a connection started on shim.
        let mut remote = target("remote", Some("root-jump"));
        remote.origin = Some("shim".into());
        let mismatched = [shim.clone(), target("root-jump", None), remote];
        assert!(matches!(
            TargetRegistry::from_definitions(mismatched),
            Err(TargetError::OriginMismatch { jump_origin, .. }) if jump_origin == ROOT_TARGET
        ));
        destination.origin = Some("shim".into());
        // Origins nest: deep's connection starts on destination, itself reached from shim.
        let mut deep = target("deep", None);
        deep.origin = Some("destination".into());
        let registry = TargetRegistry::from_definitions([shim, jump, destination, deep]).unwrap();
        let route = registry.route("deep").await.unwrap();
        let names: Vec<_> = route.iter().map(|target| target.name.as_str()).collect();
        assert_eq!(names, ["shim", "jump", "destination", "deep"]);
        let config =
            serde_json::json!({"type": "ssh", "host": "h", "via": "shim", "origin": "shim"});
        let config = serde_json::from_value(config).unwrap();
        let via_origin = TargetDefinition::from_config("h".into(), config, TargetSource::Config);
        assert!(matches!(via_origin, Err(TargetError::ViaIsOrigin(_))));
    }
}
