use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Arc,
};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::RwLock;

use super::{SshAuth, SshOptions, TargetConfig, TargetName, TargetRef, Transport};
use crate::{
    named_enum::named_enum,
    tool::{
        AdmissionError,
        diagnostic::{Operation, Subject},
        policy::{Capability, CapabilitySet},
    },
};

named_enum! {
    #[derive(Clone, Copy, Debug, Deserialize, JsonSchema, Serialize, PartialEq, Eq)]
    pub enum TargetSource {
        Builtin = "builtin",
        Config = "config",
        Session = "session",
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TargetDefinition {
    pub name: TargetName,
    pub host: String,
    pub ssh: SshOptions,
    pub workspace: PathBuf,
    /// A native jump (ProxyJump) within the SSH connection `origin` starts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<TargetName>,
    /// The target whose shim starts the SSH connection; `None` is root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<TargetName>,
    pub source: TargetSource,
    pub revision: u64,
}

impl TargetDefinition {
    /// A configured definition; the registry stamps session-added ones when it
    /// installs them.
    pub fn from_config(name: String, config: TargetConfig) -> Result<Self, TargetError> {
        let Transport::Ssh = config.r#type;
        let link = |edge, link: Option<String>| {
            link.map(|link| {
                TargetRef::try_from(link).map_err(|_| TargetError::InvalidReference { edge })
            })
            .transpose()
        };
        // `origin: root` spells the default; a jump through root is no route.
        let origin =
            link(TargetEdge::Origin, config.origin)?.and_then(|origin| origin.name().cloned());
        let via = match link(TargetEdge::Via, config.via)? {
            Some(TargetRef::Root) => return Err(TargetError::RootCannotBeJump),
            via => via.and_then(|via| via.name().cloned()),
        };
        let definition = Self {
            name: TargetName::try_from(name)?,
            host: config.host,
            ssh: config.ssh,
            workspace: config.workspace,
            via,
            origin,
            source: TargetSource::Config,
            revision: 1,
        };
        definition.validate()?;
        Ok(definition)
    }

    /// The previous target in this target's route: its jump, otherwise its origin.
    pub fn parent(&self) -> Option<&TargetName> {
        self.parent_edge().map(|(_, name)| name)
    }

    pub(crate) fn parent_edge(&self) -> Option<(TargetEdge, &TargetName)> {
        self.via
            .as_ref()
            .map(|name| (TargetEdge::Via, name))
            .or_else(|| self.origin.as_ref().map(|name| (TargetEdge::Origin, name)))
    }

    pub(crate) fn validate(&self) -> Result<(), TargetError> {
        validate_endpoint(&self.host, self.ssh.user.as_deref())?;
        if self.via.is_some() && self.via == self.origin {
            return Err(TargetError::ViaIsOrigin(self.name.to_string()));
        }
        for (key, value) in &self.ssh.options {
            crate::remote::ssh::validate_option(key, value)?;
        }
        let proxy_command =
            (self.ssh.options.keys()).any(|key| key.eq_ignore_ascii_case("proxycommand"));
        if proxy_command && self.via.is_some() {
            return Err(TargetError::ProxyCommandWithVia(self.name.to_string()));
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn test(name: &str, workspace: impl Into<PathBuf>, via: Option<&str>) -> Self {
        Self::from_config(
            name.to_owned(),
            TargetConfig {
                r#type: Transport::Ssh,
                host: format!("{name}.example.com"),
                ssh: SshOptions::default(),
                workspace: workspace.into(),
                via: via.map(str::to_owned),
                origin: None,
            },
        )
        .unwrap()
    }
}

/// A listed target without its key path: the session host, or an SSH target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TargetRecord {
    Root,
    Ssh {
        name: TargetName,
        source: TargetSource,
        host: String,
        user: Option<String>,
        port: Option<u16>,
        workspace: PathBuf,
        via: Option<TargetName>,
        origin: Option<TargetName>,
        auth: SshAuth,
        external_agent: bool,
    },
}

impl From<&TargetDefinition> for TargetRecord {
    fn from(value: &TargetDefinition) -> Self {
        Self::Ssh {
            name: value.name.clone(),
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
    entries: Arc<RwLock<BTreeMap<TargetName, TargetDefinition>>>,
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
        std::iter::once(TargetRecord::Root)
            .chain(entries.values().filter(visible).map(TargetRecord::from))
            .collect()
    }

    /// Whether reaching `name` forwards an agent Skyhook does not own. Such targets
    /// are invisible to callers without the ssh_agent capability.
    pub async fn needs_ssh_agent(&self, name: &TargetName) -> bool {
        needs_ssh_agent(&*self.entries.read().await, name)
    }

    pub async fn get(&self, name: &TargetName) -> Result<TargetDefinition, TargetError> {
        self.entries
            .read()
            .await
            .get(name)
            .cloned()
            .ok_or_else(|| TargetError::Unknown(name.to_string()))
    }

    pub async fn route(&self, name: &TargetName) -> Result<Vec<TargetDefinition>, TargetError> {
        let entries = self.entries.read().await;
        let mut route = walk_route(&entries, name)?
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
    ) -> Result<(Vec<TargetDefinition>, Vec<TargetName>), TargetError> {
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
    entries: Arc<RwLock<BTreeMap<TargetName, TargetDefinition>>>,
    next: BTreeMap<TargetName, TargetDefinition>,
    saved: Vec<TargetDefinition>,
    invalidated: Vec<TargetName>,
}

impl PreparedTargetUpdate {
    pub(super) fn definitions(&self) -> &[TargetDefinition] {
        &self.saved
    }

    pub(super) async fn publish(self) -> (Vec<TargetDefinition>, Vec<TargetName>) {
        *self.entries.write().await = self.next;
        (self.saved, self.invalidated)
    }
}

fn needs_ssh_agent(entries: &BTreeMap<TargetName, TargetDefinition>, name: &TargetName) -> bool {
    walk_route(entries, name).is_ok_and(|route| route.iter().any(|hop| hop.ssh.external_agent))
}

fn dependants(
    entries: &BTreeMap<TargetName, TargetDefinition>,
    changed: &TargetName,
) -> Vec<TargetName> {
    entries
        .keys()
        .filter(|name| {
            walk_route(entries, name)
                .is_ok_and(|route| route.iter().any(|target| &target.name == changed))
        })
        .cloned()
        .collect()
}

fn validate_graph(entries: &BTreeMap<TargetName, TargetDefinition>) -> Result<(), TargetError> {
    let origin_of = |target: &TargetDefinition| {
        (target.origin.clone()).map_or(TargetRef::Root, TargetRef::Named)
    };
    for (name, target) in entries {
        let route = walk_route(entries, name)?;
        target.validate()?;
        // Jumps belong to the connection their origin starts, so a jump's key paths
        // and agent are never reinterpreted on another machine. The origin ends the
        // chain: jumps without via link to it.
        let origin = target.origin.as_ref();
        for jump in (route.iter().skip(1)).take_while(|hop| Some(&hop.name) != origin) {
            if jump.origin != target.origin {
                return Err(TargetError::OriginMismatch {
                    target: name.to_string(),
                    jump: jump.name.to_string(),
                    origin: origin_of(target).to_string(),
                    jump_origin: origin_of(jump).to_string(),
                });
            }
        }
    }
    Ok(())
}

/// Validate known route edges while permitting references supplied at runtime.
pub(super) fn validate_route_cycles(
    entries: &BTreeMap<TargetName, TargetDefinition>,
) -> Result<(), TargetError> {
    for name in entries.keys() {
        walk_partial_route(entries, name, MissingReference::Allow)?;
    }
    Ok(())
}

enum MissingReference {
    Allow,
    Reject,
}

fn walk_route<'a>(
    entries: &'a BTreeMap<TargetName, TargetDefinition>,
    name: &TargetName,
) -> Result<Vec<&'a TargetDefinition>, TargetError> {
    walk_partial_route(entries, name, MissingReference::Reject)
}

fn walk_partial_route<'a>(
    entries: &'a BTreeMap<TargetName, TargetDefinition>,
    name: &TargetName,
    missing: MissingReference,
) -> Result<Vec<&'a TargetDefinition>, TargetError> {
    let mut route = Vec::new();
    let mut current = name;
    let mut visited = BTreeSet::new();
    loop {
        if !visited.insert(current) {
            return Err(TargetError::Cycle(current.to_string()));
        }
        let Some(target) = entries.get(current) else {
            return match missing {
                MissingReference::Reject => Err(TargetError::Unknown(current.to_string())),
                MissingReference::Allow => Ok(route),
            };
        };
        route.push(target);
        let Some((edge, parent)) = target.parent_edge() else {
            return Ok(route);
        };
        if !entries.contains_key(parent) {
            return match missing {
                MissingReference::Reject => Err(TargetError::UnknownReference {
                    target: target.name.to_string(),
                    edge,
                    reference: parent.to_string(),
                }),
                MissingReference::Allow => Ok(route),
            };
        }
        current = parent;
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

/// The field that introduces a target reference, retained when resolution fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetEdge {
    Origin,
    Via,
}

impl std::fmt::Display for TargetEdge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Origin => "origin",
            Self::Via => "via",
        })
    }
}

#[derive(Clone, Debug, Error)]
pub enum TargetError {
    #[error("target name `root` is reserved for the Skyhook session host; choose a different name")]
    ReservedName,
    #[error("invalid target name `{0}`")]
    InvalidName(String),
    #[error("invalid target hostname `{0}`")]
    InvalidHost(String),
    #[error("SSH option names must be nonempty ASCII letters and digits")]
    InvalidSshOptionName,
    #[error("SSH option `{0}` is reserved by Skyhook")]
    ReservedSshOption(String),
    #[error("SSH option `{0}` requires a nonempty value")]
    EmptySshOptionValue(String),
    #[error("SSH option `{0}` cannot contain control characters")]
    InvalidSshOptionValue(String),
    #[error("SSH option `{0}` is set through the `{1}` field, not ssh.options")]
    DedicatedSshOption(String, &'static str),
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
    #[error("invalid {edge} target name")]
    InvalidReference { edge: TargetEdge },
    #[error("target `{target}` references unknown {edge} target `{reference}`")]
    UnknownReference {
        target: String,
        edge: TargetEdge,
        reference: String,
    },
    #[error("target route contains a cycle at `{0}`")]
    Cycle(String),
    #[error("`root` cannot be used as a jump target")]
    RootCannotBeJump,
}

impl TargetError {
    /// Agent-facing reasons never carry aliases or submitted values: a saved
    /// diagnostic can later be read with fewer capabilities than its first reader.
    pub(crate) fn into_admission_error(self) -> AdmissionError {
        let argument = |name: &str| Subject::argument(name.split('.'));
        let (subject, reason) = match &self {
            Self::ReservedName => (argument("name"), None),
            Self::InvalidName(_) => (
                argument("name"),
                Some(
                    "target name must contain 1 to 128 ASCII letters, digits, underscores, hyphens, or periods",
                ),
            ),
            Self::InvalidHost(_) => (
                argument("host"),
                Some("hostname must be nonempty and contain no whitespace or control characters"),
            ),
            Self::InvalidSshOptionName => (argument("ssh.options"), None),
            Self::ReservedSshOption(_) => (
                argument("ssh.options"),
                Some("SSH option is reserved by Skyhook"),
            ),
            Self::EmptySshOptionValue(_) => (
                argument("ssh.options"),
                Some("SSH option requires a nonempty value"),
            ),
            Self::InvalidSshOptionValue(_) => (
                argument("ssh.options"),
                Some("SSH option value cannot contain control characters"),
            ),
            Self::DedicatedSshOption(_, field) => {
                return AdmissionError::invalid_arguments(format!(
                    "SSH option must be set through the {field} field, not ssh.options"
                ))
                .operation(Operation::Validate, argument("ssh.options"));
            }
            Self::ProxyCommandWithVia(_) => (
                argument("ssh.options"),
                Some("ProxyCommand cannot be combined with via"),
            ),
            Self::ViaIsOrigin(_) => (
                argument("via"),
                Some(
                    "via cannot be the connection origin; omit via to connect directly from the origin",
                ),
            ),
            Self::NameUnavailable(_) => (argument("name"), Some("target name is unavailable")),
            Self::OriginMismatch { .. } => (
                argument("origin"),
                Some("target and its jump must start SSH from the same origin"),
            ),
            Self::InvalidUser => (argument("ssh.user"), None),
            Self::Unknown(_) => (argument("target"), Some("unknown target")),
            Self::InvalidReference { edge } => (argument(&edge.to_string()), None),
            Self::UnknownReference { edge, .. } => match edge {
                TargetEdge::Origin => (argument("origin"), Some("unknown origin target")),
                TargetEdge::Via => (argument("via"), Some("unknown jump target")),
            },
            Self::Cycle(_) => (
                Subject::Label("target route".into()),
                Some("target route contains a cycle"),
            ),
            Self::RootCannotBeJump => (argument("via"), None),
        };
        // Variants without a payload render nothing private.
        AdmissionError::invalid_arguments(reason.map_or_else(|| self.to_string(), str::to_owned))
            .operation(Operation::Validate, subject)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(name: &str, via: Option<&str>) -> TargetDefinition {
        TargetDefinition::test(name, ".", via)
    }

    fn name(name: &str) -> TargetName {
        name.parse().unwrap()
    }

    #[tokio::test]
    async fn routes_are_outermost_first_and_cycles_are_rejected() {
        let registry = TargetRegistry::from_definitions([
            target("edge", None),
            target("bastion", Some("edge")),
            target("build", Some("bastion")),
        ])
        .unwrap();
        let route = registry.route(&name("build")).await.unwrap();
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
        assert_eq!(registry.get(&name("build")).await.unwrap().revision, 2);
    }

    #[test]
    fn admission_errors_never_carry_submitted_values_or_aliases() {
        let private = || "private-submitted-target".to_owned();
        for error in [
            TargetError::InvalidName(private()),
            TargetError::InvalidHost(private()),
            TargetError::ReservedSshOption(private()),
            TargetError::EmptySshOptionValue(private()),
            TargetError::InvalidSshOptionValue(private()),
            TargetError::DedicatedSshOption(private(), "ssh.auth"),
            TargetError::ProxyCommandWithVia(private()),
            TargetError::ViaIsOrigin(private()),
            TargetError::NameUnavailable(private()),
            TargetError::OriginMismatch {
                target: private(),
                jump: private(),
                origin: private(),
                jump_origin: private(),
            },
            TargetError::Unknown(private()),
            TargetError::UnknownReference {
                target: private(),
                edge: TargetEdge::Via,
                reference: private(),
            },
            TargetError::Cycle(private()),
        ] {
            let diagnostic = error.into_admission_error().diagnostic();
            let saved = serde_json::to_string(&diagnostic).unwrap();
            assert!(!saved.contains(&private()), "{saved}");
        }
    }

    #[tokio::test]
    async fn missing_route_references_identify_the_edge_and_leave_batch_unchanged() {
        let registry = TargetRegistry::from_definitions([target("first", None)]).unwrap();
        let before = registry.definitions().await;
        for edge in [TargetEdge::Via, TargetEdge::Origin] {
            let mut invalid = target("invalid", None);
            match edge {
                TargetEdge::Via => invalid.via = Some(name("missing")),
                TargetEdge::Origin => invalid.origin = Some(name("missing")),
            }
            let result = registry
                .upsert_many(vec![target("added", None), invalid])
                .await;
            assert!(matches!(result, Err(TargetError::UnknownReference {
                target,
                edge: actual,
                reference,
            }) if target == "invalid" && actual == edge && reference == "missing"));
            assert_eq!(registry.definitions().await, before);
        }
        assert!(
            matches!(registry.route(&name("absent")).await, Err(TargetError::Unknown(unknown)) if unknown == "absent")
        );
    }

    #[tokio::test]
    async fn jumps_share_the_origin_that_starts_their_connection() {
        let shim = target("shim", None);
        let mut jump = target("jump", None);
        jump.origin = Some(name("shim"));
        let mut destination = target("destination", Some("jump"));
        let mismatched = [shim.clone(), jump.clone(), destination.clone()];
        assert!(matches!(
            TargetRegistry::from_definitions(mismatched),
            Err(TargetError::OriginMismatch { jump_origin, .. }) if jump_origin == "shim"
        ));
        // A jump reached from root cannot continue a connection started on shim.
        let mut remote = target("remote", Some("root-jump"));
        remote.origin = Some(name("shim"));
        let mismatched = [shim.clone(), target("root-jump", None), remote];
        assert!(matches!(
            TargetRegistry::from_definitions(mismatched),
            Err(TargetError::OriginMismatch { jump_origin, .. }) if jump_origin == "root"
        ));
        destination.origin = Some(name("shim"));
        // Origins nest: deep's connection starts on destination, itself reached from shim.
        let mut deep = target("deep", None);
        deep.origin = Some(name("destination"));
        let registry = TargetRegistry::from_definitions([shim, jump, destination, deep]).unwrap();
        let route = registry.route(&name("deep")).await.unwrap();
        let names: Vec<_> = route.iter().map(|target| target.name.as_str()).collect();
        assert_eq!(names, ["shim", "jump", "destination", "deep"]);
        let from_config = |config: serde_json::Value| {
            let config = serde_json::from_value(config).unwrap();
            TargetDefinition::from_config("h".into(), config)
        };
        let via_origin = from_config(
            serde_json::json!({"type": "ssh", "host": "h", "via": "shim", "origin": "shim"}),
        );
        assert!(matches!(via_origin, Err(TargetError::ViaIsOrigin(_))));
        // `origin: root` spells the default; root is never a jump.
        let from_root =
            from_config(serde_json::json!({"type": "ssh", "host": "h", "origin": "root"}));
        assert_eq!(from_root.unwrap().origin, None);
        let via_root = from_config(serde_json::json!({"type": "ssh", "host": "h", "via": "root"}));
        assert!(matches!(via_root, Err(TargetError::RootCannotBeJump)));
        // A bad link is the link's fault, not the valid name's.
        for edge in [TargetEdge::Origin, TargetEdge::Via] {
            let bad = from_config(
                serde_json::json!({"type": "ssh", "host": "h", edge.to_string(): "bad target"}),
            );
            let error = bad.unwrap_err();
            assert!(
                matches!(error, TargetError::InvalidReference { edge: found } if found == edge)
            );
            let diagnostic = error.into_admission_error().diagnostic();
            assert_eq!(
                diagnostic.context.subject,
                Subject::argument([edge.to_string()])
            );
        }
    }
}
