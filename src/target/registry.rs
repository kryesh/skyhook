use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{
    MAX_TARGET_NAME_BYTES, Route, SshOptions, TargetConfig, TargetName, TargetRef, Transport,
};
use crate::{
    named_enum::named_enum,
    remote::SshOption,
    session::SessionEvent,
    tool::{
        AdmissionError,
        diagnostic::{Operation, Subject},
        policy::{Capability, CapabilitySet, ResourceId},
    },
};

named_enum! {
    #[derive(Clone, Copy, Debug, JsonSchema, Serialize, PartialEq, Eq)]
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
    pub(crate) fn parent_edge(&self) -> Option<(TargetEdge, &TargetName)> {
        self.via
            .as_ref()
            .map(|name| (TargetEdge::Via, name))
            .or_else(|| self.origin.as_ref().map(|name| (TargetEdge::Origin, name)))
    }

    pub(crate) fn validate(&self) -> Result<(), TargetError> {
        validate_endpoint(&self.host, self.ssh.user.as_deref())?;
        if self.via.is_some() && self.via == self.origin {
            return Err(TargetError::ViaIsOrigin(self.name.clone()));
        }
        // Option values are never retained in errors: they may be sensitive.
        for (key, value) in &self.ssh.options {
            match SshOption::parse(key)? {
                SshOption::Dedicated(field) => {
                    return Err(TargetError::DedicatedSshOption(key.clone(), field));
                }
                SshOption::Reserved => return Err(TargetError::ReservedSshOption(key.clone())),
                SshOption::ProxyCommand if self.via.is_some() => {
                    return Err(TargetError::ProxyCommandWithVia(self.name.clone()));
                }
                SshOption::ProxyCommand | SshOption::Free => {}
            }
            if value.is_empty() {
                return Err(TargetError::EmptySshOptionValue(key.clone()));
            }
            if value.chars().any(char::is_control) {
                return Err(TargetError::InvalidSshOptionValue(key.clone()));
            }
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

/// The session's targets as its journal defines them: those it started with
/// and those it added since, each at its revision.
#[derive(Clone, Default)]
pub(crate) struct TargetRegistry {
    entries: BTreeMap<TargetName, TargetDefinition>,
}

impl TargetRegistry {
    pub(crate) fn from_definitions(
        definitions: impl IntoIterator<Item = TargetDefinition>,
    ) -> Result<Self, TargetError> {
        let entries = (definitions.into_iter())
            .map(|definition| (definition.name.clone(), definition))
            .collect();
        validate_graph(&entries)?;
        Ok(Self { entries })
    }

    /// Fold the definitions a journaled event records, as their revisions stand.
    pub(crate) fn apply(&mut self, event: &SessionEvent) {
        if let SessionEvent::SessionStarted { targets, .. }
        | SessionEvent::TargetsUpserted { targets } = event
        {
            let named = targets
                .iter()
                .map(|target| (target.name.clone(), target.clone()));
            self.entries.extend(named);
        }
    }

    /// Named targets visible with `capabilities`.
    pub(crate) fn list(&self, capabilities: &CapabilitySet) -> Vec<TargetDefinition> {
        let visible = |target: &&TargetDefinition| {
            capabilities.contains(Capability::SshAgent)
                || !needs_ssh_agent(&self.entries, &target.name)
        };
        self.entries.values().filter(visible).cloned().collect()
    }

    /// Whether reaching `name` forwards an agent Skyhook does not own. Such targets
    /// are invisible to callers without the ssh_agent capability.
    pub(crate) fn needs_ssh_agent(&self, name: &TargetName) -> bool {
        needs_ssh_agent(&self.entries, name)
    }

    pub(crate) fn get(&self, name: &TargetName) -> Result<TargetDefinition, TargetError> {
        (self.entries.get(name).cloned()).ok_or_else(|| TargetError::Unknown(name.clone()))
    }

    pub(crate) fn route(&self, name: &TargetName) -> Result<Route, TargetError> {
        let walked = walk_route(&self.entries, name)?;
        let Some((destination, hops)) = walked.split_first() else {
            return Err(TargetError::Unknown(name.clone()));
        };
        Ok(Route {
            hops: hops.iter().rev().map(|&hop| hop.clone()).collect(),
            destination: (*destination).clone(),
        })
    }

    /// `definitions` as the session would add them, each at its next revision.
    pub(crate) fn upsert(
        &self,
        definitions: Vec<TargetDefinition>,
    ) -> Result<Vec<TargetDefinition>, TargetError> {
        let mut next = self.entries.clone();
        let mut saved = Vec::new();
        for mut definition in definitions {
            definition.source = TargetSource::Session;
            // Allocate against the staged batch as well as the registry: repeated
            // names in one batch must not reuse a route revision.
            definition.revision = next
                .get(&definition.name)
                .map_or(1, |old| old.revision.saturating_add(1));
            next.insert(definition.name.clone(), definition.clone());
            saved.push(definition);
        }
        validate_graph(&next)?;
        Ok(saved)
    }

    /// The revision of every target `resource` depends on, as those targets now
    /// stand: a named target and those it is routed through. None for a route
    /// that names another revision, or a target the session does not have.
    pub(crate) fn revisions(&self, resource: &ResourceId) -> Option<Revisions> {
        let route = |name: &TargetName| {
            let route = walk_route(&self.entries, name).ok()?;
            Some(
                route
                    .iter()
                    .rev()
                    .map(|hop| (hop.name.clone(), hop.revision))
                    .collect(),
            )
        };
        match resource {
            ResourceId::Workspace { target, .. }
            | ResourceId::Path { target, .. }
            | ResourceId::Network { target, .. } => target.name().map_or(Some(Vec::new()), route),
            ResourceId::Route { destination, hops } => {
                route(destination).filter(|current: &Revisions| current == hops)
            }
            ResourceId::Session { .. } | ResourceId::Mcp { .. } => Some(Vec::new()),
        }
    }

    /// Whether every target in `revisions` is still at its revision.
    pub(crate) fn current(&self, revisions: &[(TargetName, u64)]) -> bool {
        (revisions.iter()).all(|(name, revision)| {
            (self.entries.get(name)).is_some_and(|target| target.revision == *revision)
        })
    }
}

/// The revision of each target something depends on, outermost first.
pub(crate) type Revisions = Vec<(TargetName, u64)>;

/// Whether `redefined`, as a redefinition saves them, replaces any of `revisions`.
pub(crate) fn replaces(redefined: &[TargetDefinition], revisions: &[(TargetName, u64)]) -> bool {
    (revisions.iter()).any(|(name, revision)| {
        (redefined.iter()).any(|target| &target.name == name && *revision < target.revision)
    })
}

fn needs_ssh_agent(entries: &BTreeMap<TargetName, TargetDefinition>, name: &TargetName) -> bool {
    walk_route(entries, name).is_ok_and(|route| route.iter().any(|hop| hop.ssh.external_agent))
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
                    target: name.clone(),
                    jump: jump.name.clone(),
                    origin: origin_of(target),
                    jump_origin: origin_of(jump),
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
            return Err(TargetError::Cycle(current.clone()));
        }
        let Some(target) = entries.get(current) else {
            return match missing {
                MissingReference::Reject => Err(TargetError::Unknown(current.clone())),
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
                    target: target.name.clone(),
                    edge,
                    reference: parent.clone(),
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

named_enum! {
    /// A target field that an `ssh.options` key would duplicate.
    #[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
    pub enum TargetField {
        Host = "host",
        SshUser = "ssh.user",
        SshPort = "ssh.port",
        SshAuth = "ssh.auth",
        Via = "via",
    }
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
    DedicatedSshOption(String, TargetField),
    #[error("target `{0}` sets ProxyCommand, which cannot be combined with via")]
    ProxyCommandWithVia(TargetName),
    #[error("target `{0}` sets via to its origin; omit via to connect directly from the origin")]
    ViaIsOrigin(TargetName),
    #[error("target name `{0}` is unavailable")]
    NameUnavailable(TargetName),
    #[error(
        "target `{target}` starts SSH from `{origin}`, but its jump `{jump}` starts from `{jump_origin}`; give them the same origin"
    )]
    OriginMismatch {
        target: TargetName,
        jump: TargetName,
        origin: TargetRef,
        jump_origin: TargetRef,
    },
    #[error("invalid SSH username")]
    InvalidUser,
    #[error("unknown target `{0}`")]
    Unknown(TargetName),
    #[error("invalid {edge} target name")]
    InvalidReference { edge: TargetEdge },
    #[error("target `{target}` references unknown {edge} target `{reference}`")]
    UnknownReference {
        target: TargetName,
        edge: TargetEdge,
        reference: TargetName,
    },
    #[error("target route contains a cycle at `{0}`")]
    Cycle(TargetName),
    #[error("`root` cannot be used as a jump target")]
    RootCannotBeJump,
}

impl TargetError {
    /// Agent-facing reasons never carry aliases or submitted values: a saved
    /// diagnostic can later be read with fewer capabilities than its first reader.
    pub(crate) fn into_admission_error(self) -> AdmissionError {
        let argument = |name: &str| Subject::argument(name.split('.'));
        let fixed = |reason: &'static str| Some(Cow::Borrowed(reason));
        let (subject, reason) = match &self {
            Self::ReservedName => (argument("name"), None),
            Self::InvalidName(_) => (
                argument("name"),
                Some(format!(
                    "target name must contain 1 to {MAX_TARGET_NAME_BYTES} ASCII letters, digits, underscores, hyphens, or periods"
                ).into()),
            ),
            Self::InvalidHost(_) => (
                argument("host"),
                fixed("hostname must be nonempty and contain no whitespace or control characters"),
            ),
            Self::InvalidSshOptionName => (argument("ssh.options"), None),
            Self::ReservedSshOption(_) => (
                argument("ssh.options"),
                fixed("SSH option is reserved by Skyhook"),
            ),
            Self::EmptySshOptionValue(_) => (
                argument("ssh.options"),
                fixed("SSH option requires a nonempty value"),
            ),
            Self::InvalidSshOptionValue(_) => (
                argument("ssh.options"),
                fixed("SSH option value cannot contain control characters"),
            ),
            Self::DedicatedSshOption(_, field) => (
                argument("ssh.options"),
                Some(format!("SSH option must be set through the {field} field, not ssh.options").into()),
            ),
            Self::ProxyCommandWithVia(_) => (
                argument("ssh.options"),
                fixed("ProxyCommand cannot be combined with via"),
            ),
            Self::ViaIsOrigin(_) => (
                argument("via"),
                fixed(
                    "via cannot be the connection origin; omit via to connect directly from the origin",
                ),
            ),
            Self::NameUnavailable(_) => (argument("name"), fixed("target name is unavailable")),
            Self::OriginMismatch { .. } => (
                argument("origin"),
                fixed("target and its jump must start SSH from the same origin"),
            ),
            Self::InvalidUser => (argument("ssh.user"), None),
            Self::Unknown(_) => (argument("target"), fixed("unknown target")),
            Self::InvalidReference { edge } => (argument(&edge.to_string()), None),
            Self::UnknownReference { edge, .. } => match edge {
                TargetEdge::Origin => (argument("origin"), fixed("unknown origin target")),
                TargetEdge::Via => (argument("via"), fixed("unknown jump target")),
            },
            Self::Cycle(_) => (
                Subject::Label("target route".into()),
                fixed("target route contains a cycle"),
            ),
            Self::RootCannotBeJump => (argument("via"), None),
        };
        // Variants without a payload render nothing private.
        AdmissionError::invalid_arguments(reason.unwrap_or_else(|| self.to_string().into()))
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

    #[test]
    fn routes_are_outermost_first_and_cycles_are_rejected() {
        let registry = TargetRegistry::from_definitions([
            target("edge", None),
            target("bastion", Some("edge")),
            target("build", Some("bastion")),
        ])
        .unwrap();
        let route = registry.route(&name("build")).unwrap();
        let names: Vec<_> = route.iter().map(|target| target.name.as_str()).collect();
        assert_eq!(names, ["edge", "bastion", "build"]);
        let mut changed = target("edge", Some("build"));
        changed.source = TargetSource::Session;
        let result = registry.upsert(vec![changed]);
        assert!(matches!(result, Err(TargetError::Cycle(_))));
    }

    #[test]
    fn repeated_names_in_a_batch_receive_distinct_revisions() {
        let registry = TargetRegistry::default();
        let batch = vec![target("build", None), target("build", None)];
        let saved = registry.upsert(batch).unwrap();
        let revisions: Vec<_> = saved.iter().map(|target| target.revision).collect();
        assert_eq!(revisions, [1, 2]);
    }

    #[test]
    fn admission_errors_never_carry_submitted_values_or_aliases() {
        let private = || "private-submitted-target".to_owned();
        let named = || name(&private());
        for error in [
            TargetError::InvalidName(private()),
            TargetError::InvalidHost(private()),
            TargetError::ReservedSshOption(private()),
            TargetError::EmptySshOptionValue(private()),
            TargetError::InvalidSshOptionValue(private()),
            TargetError::DedicatedSshOption(private(), TargetField::SshAuth),
            TargetError::ProxyCommandWithVia(named()),
            TargetError::ViaIsOrigin(named()),
            TargetError::NameUnavailable(named()),
            TargetError::OriginMismatch {
                target: named(),
                jump: named(),
                origin: named().into(),
                jump_origin: named().into(),
            },
            TargetError::Unknown(named()),
            TargetError::UnknownReference {
                target: named(),
                edge: TargetEdge::Via,
                reference: named(),
            },
            TargetError::Cycle(named()),
        ] {
            let diagnostic = error.into_admission_error().diagnostic();
            let saved = serde_json::to_string(&diagnostic).unwrap();
            assert!(!saved.contains(&private()), "{saved}");
        }
    }

    #[test]
    fn missing_route_references_identify_the_edge() {
        let registry = TargetRegistry::from_definitions([target("first", None)]).unwrap();
        for edge in [TargetEdge::Via, TargetEdge::Origin] {
            let mut invalid = target("invalid", None);
            match edge {
                TargetEdge::Via => invalid.via = Some(name("missing")),
                TargetEdge::Origin => invalid.origin = Some(name("missing")),
            }
            let result = registry.upsert(vec![target("added", None), invalid]);
            assert!(matches!(result, Err(TargetError::UnknownReference {
                target,
                edge: actual,
                reference,
            }) if target == name("invalid") && actual == edge && reference == name("missing")));
        }
        assert!(
            matches!(registry.route(&name("absent")), Err(TargetError::Unknown(unknown)) if unknown == name("absent"))
        );
    }

    #[test]
    fn jumps_share_the_origin_that_starts_their_connection() {
        let shim = target("shim", None);
        let mut jump = target("jump", None);
        jump.origin = Some(name("shim"));
        let mut destination = target("destination", Some("jump"));
        let mismatched = [shim.clone(), jump.clone(), destination.clone()];
        assert!(matches!(
            TargetRegistry::from_definitions(mismatched),
            Err(TargetError::OriginMismatch { jump_origin, .. }) if jump_origin == name("shim").into()
        ));
        // A jump reached from root cannot continue a connection started on shim.
        let mut remote = target("remote", Some("root-jump"));
        remote.origin = Some(name("shim"));
        let mismatched = [shim.clone(), target("root-jump", None), remote];
        assert!(matches!(
            TargetRegistry::from_definitions(mismatched),
            Err(TargetError::OriginMismatch {
                jump_origin: TargetRef::Root,
                ..
            })
        ));
        destination.origin = Some(name("shim"));
        // Origins nest: deep's connection starts on destination, itself reached from shim.
        let mut deep = target("deep", None);
        deep.origin = Some(name("destination"));
        let registry = TargetRegistry::from_definitions([shim, jump, destination, deep]).unwrap();
        let route = registry.route(&name("deep")).unwrap();
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
