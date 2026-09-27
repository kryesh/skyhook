use std::{
    collections::BTreeSet,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    identity::{AgentId, JobId},
    named_enum::named_enum,
    newtype::{Blank, Prose, nonblank, string_newtype},
    target::{TargetName, TargetRef},
    tool::{
        AdmissionError,
        diagnostic::{Operation, Subject},
    },
};

named_enum! {
    #[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum Capability {
        Read = "read",
        Write = "write",
        Exec = "exec",
        Network = "network",
        Targets = "targets",
        /// Forward an SSH agent Skyhook does not own (`ssh.external_agent`).
        SshAgent = "ssh_agent",
        Agents = "agents",
        Interactive = "interactive",
        Mcp = "mcp",
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapabilitySet(BTreeSet<Capability>);

impl CapabilitySet {
    /// Construct an exact empty set, unlike the enabled-by-default session set.
    #[must_use]
    pub const fn empty() -> Self {
        Self(BTreeSet::new())
    }

    #[must_use]
    pub fn contains(&self, capability: Capability) -> bool {
        self.0.contains(&capability)
    }

    pub fn insert(&mut self, capability: Capability) {
        self.0.insert(capability);
    }

    pub fn remove(&mut self, capability: Capability) {
        self.0.remove(&capability);
    }

    #[must_use]
    pub fn for_agent(&self, available_depth: usize) -> Self {
        let mut capabilities = self.clone();
        if available_depth == 0 {
            capabilities.remove(Capability::Agents);
        }
        capabilities
    }

    pub fn iter(&self) -> impl Iterator<Item = Capability> + '_ {
        self.0.iter().copied()
    }

    /// The target as a viewer holding these capabilities may see it: only one
    /// that can select targets learns which target anything ran on.
    pub(crate) fn visible_target<'a>(
        &self,
        target: &'a crate::target::TargetRef,
    ) -> Option<&'a crate::target::TargetRef> {
        self.contains(Capability::Targets).then_some(target)
    }
}

impl FromIterator<Capability> for CapabilitySet {
    fn from_iter<T: IntoIterator<Item = Capability>>(iter: T) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl Default for CapabilitySet {
    fn default() -> Self {
        Self(BTreeSet::from([
            Capability::Read,
            Capability::Write,
            Capability::Exec,
            Capability::Network,
            Capability::Agents,
            Capability::Interactive,
            Capability::Mcp,
        ]))
    }
}

impl std::ops::BitAnd for &CapabilitySet {
    type Output = CapabilitySet;

    fn bitand(self, other: Self) -> CapabilitySet {
        CapabilitySet(&self.0 & &other.0)
    }
}

string_newtype! {
    /// The name a mode is declared under and selected by.
    #[derive(PartialOrd, Ord)]
    pub struct ModeName(Blank) = |name| nonblank("mode name", name);
}

/// A named permission preset. Interaction is supplied by the runtime host, never listed.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Mode {
    #[serde(deserialize_with = "deserialize_policy_capabilities")]
    pub capabilities: Vec<Capability>,
    /// Added to the system prompt of an agent running in the mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<Prose>,
    /// Describes the mode to agents choosing one for a child; without it they cannot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<Prose>,
}

fn deserialize_policy_capabilities<'de, D>(deserializer: D) -> Result<Vec<Capability>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let capabilities = Vec::<Capability>::deserialize(deserializer)?;
    if capabilities.contains(&Capability::Interactive) {
        return Err(serde::de::Error::custom(
            "interactive is controlled by the runtime host, not the capability allowlist",
        ));
    }
    Ok(capabilities)
}

/// The wire format is `{namespace, segments}`; unknown namespaces and malformed
/// shapes are rejected. Decoding never normalizes paths.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(try_from = "ResourceWire", into = "ResourceWire")]
pub enum ResourceId {
    Workspace {
        target: TargetRef,
        path: String,
    },
    Path {
        target: TargetRef,
        components: Vec<String>,
    },
    Network {
        target: TargetRef,
        origin: String,
    },
    Route {
        destination: TargetName,
        hops: Vec<(TargetName, u64)>,
    },
    Session {
        name: String,
    },
    Mcp {
        server: String,
        tool: String,
    },
}

named_enum! {
    #[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
    pub(crate) enum ResourceKind {
        Workspace = "workspace",
        Path = "path",
        Network = "network",
        Route = "route",
        Session = "session",
        Mcp = "mcp",
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("unknown or malformed permission resource: {0}")]
pub struct ResourceError(String);

#[derive(Deserialize, Serialize)]
struct ResourceWire {
    namespace: ResourceKind,
    segments: Vec<String>,
}

/// A native path whose text spells it exactly. Permission resources and wire
/// frames are text, so a non-Unicode path is refused rather than authorized as
/// its replacement-character alias.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathText(String);

impl PathText {
    pub fn new(path: impl Into<PathBuf>) -> Result<Self, AdmissionError> {
        path.into()
            .into_os_string()
            .into_string()
            .map(Self)
            .map_err(|path| {
                AdmissionError::invalid_arguments(
                    "native path cannot be represented losslessly by the permission or wire format",
                )
                .operation(Operation::Validate, Subject::path(path))
            })
    }

    #[must_use]
    pub fn as_path(&self) -> &Path {
        Path::new(&self.0)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<PathText> for PathBuf {
    fn from(path: PathText) -> Self {
        path.0.into()
    }
}

impl ResourceId {
    #[must_use]
    pub fn workspace(target: &TargetRef, workspace: &PathText) -> Self {
        Self::Workspace {
            target: target.clone(),
            path: workspace.0.clone(),
        }
    }

    #[must_use]
    pub fn path(target: &TargetRef, path: &PathText) -> Self {
        // Every component of Unicode text is itself Unicode, so this is lossless.
        let components = path
            .as_path()
            .components()
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .collect();
        Self::Path {
            target: target.clone(),
            components,
        }
    }

    /// Callers obtain the normalized HTTP(S) origin from a validated URL parser,
    /// omitting path, query, user information and default ports.
    #[must_use]
    pub fn network(target: &TargetRef, normalized_origin: &str) -> Self {
        Self::Network {
            target: target.clone(),
            origin: normalized_origin.into(),
        }
    }

    #[must_use]
    pub fn session(name: impl Into<String>) -> Self {
        Self::Session { name: name.into() }
    }

    #[must_use]
    pub fn route(destination: TargetName, hops: Vec<(TargetName, u64)>) -> Self {
        Self::Route { destination, hops }
    }

    #[must_use]
    pub fn mcp(server: impl Into<String>, tool: impl Into<String>) -> Self {
        Self::Mcp {
            server: server.into(),
            tool: tool.into(),
        }
    }

    pub(crate) fn kind(&self) -> ResourceKind {
        match self {
            Self::Workspace { .. } => ResourceKind::Workspace,
            Self::Path { .. } => ResourceKind::Path,
            Self::Network { .. } => ResourceKind::Network,
            Self::Route { .. } => ResourceKind::Route,
            Self::Session { .. } => ResourceKind::Session,
            Self::Mcp { .. } => ResourceKind::Mcp,
        }
    }

    /// Only an invocation's destination resolves its paths and network origins,
    /// so a forwarded call asks the host for these after the host admitted the rest.
    pub(crate) fn resolved_at_destination(&self) -> bool {
        matches!(self, Self::Path { .. } | Self::Network { .. })
    }

    /// Only these three resource kinds are scoped to an execution target.
    pub fn execution_target_mut(&mut self) -> Option<&mut TargetRef> {
        match self {
            Self::Workspace { target, .. }
            | Self::Path { target, .. }
            | Self::Network { target, .. } => Some(target),
            _ => None,
        }
    }

    fn contains(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Path { target, components },
                Self::Path {
                    target: other_target,
                    components: other_components,
                },
            ) => target == other_target && other_components.starts_with(components),
            (
                Self::Route { destination, hops },
                Self::Route {
                    destination: other_destination,
                    hops: other_hops,
                },
            ) => destination == other_destination && other_hops.starts_with(hops),
            _ => self == other,
        }
    }
}

impl TryFrom<ResourceWire> for ResourceId {
    type Error = ResourceError;

    fn try_from(wire: ResourceWire) -> Result<Self, Self::Error> {
        let ResourceWire {
            namespace,
            segments,
        } = wire;
        let target = |text: &String| {
            TargetRef::try_from(text.clone()).map_err(|error| ResourceError(error.to_string()))
        };
        let name = |text: &String| {
            TargetName::try_from(text.clone()).map_err(|error| ResourceError(error.to_string()))
        };
        // Arity and target names are validated here only. Empty strings, relative
        // components and whole workspace paths retain their historical opaque spelling.
        match (namespace, segments.as_slice()) {
            (ResourceKind::Workspace, [workspace, path]) => Ok(Self::Workspace {
                target: target(workspace)?,
                path: path.clone(),
            }),
            (ResourceKind::Path, [path, components @ ..]) => Ok(Self::Path {
                target: target(path)?,
                components: components.to_vec(),
            }),
            (ResourceKind::Network, [network, origin]) => Ok(Self::Network {
                target: target(network)?,
                origin: origin.clone(),
            }),
            (ResourceKind::Session, [session]) => Ok(Self::session(session)),
            (ResourceKind::Mcp, [server, tool]) => Ok(Self::mcp(server, tool)),
            (ResourceKind::Route, [destination, hops @ ..]) if hops.len() % 2 == 0 => {
                let hops = hops
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|[hop, revision]| {
                        let revision = revision
                            .parse::<u64>()
                            .map_err(|_| ResourceError("route revision must be a u64".into()))?;
                        Ok((name(hop)?, revision))
                    })
                    .collect::<Result<_, ResourceError>>()?;
                Ok(Self::route(name(destination)?, hops))
            }
            _ => Err(ResourceError(namespace.to_string())),
        }
    }
}

impl From<ResourceId> for ResourceWire {
    fn from(resource: ResourceId) -> Self {
        let namespace = resource.kind();
        let segments = match resource {
            ResourceId::Workspace { target, path } => vec![target.into(), path],
            ResourceId::Path { target, components } => {
                std::iter::once(target.into()).chain(components).collect()
            }
            ResourceId::Network { target, origin } => vec![target.into(), origin],
            ResourceId::Route { destination, hops } => std::iter::once(destination.into())
                .chain(
                    hops.into_iter()
                        .flat_map(|(target, revision)| [target.into(), revision.to_string()]),
                )
                .collect(),
            ResourceId::Session { name } => vec![name],
            ResourceId::Mcp { server, tool } => vec![server, tool],
        };
        Self {
            namespace,
            segments,
        }
    }
}

named_enum! {
    #[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq, Hash)]
    pub enum ApprovalCoverage {
        Exact = "exact",
        Descendants = "descendants",
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
pub struct ApprovalGrant {
    pub capability: Capability,
    pub resource: ResourceId,
    pub coverage: ApprovalCoverage,
}

impl ApprovalGrant {
    #[must_use]
    pub const fn exact(capability: Capability, resource: ResourceId) -> Self {
        Self {
            capability,
            resource,
            coverage: ApprovalCoverage::Exact,
        }
    }

    #[must_use]
    pub const fn descendants(capability: Capability, resource: ResourceId) -> Self {
        Self {
            capability,
            resource,
            coverage: ApprovalCoverage::Descendants,
        }
    }

    #[must_use]
    pub fn covers(&self, capability: Capability, resource: &ResourceId) -> bool {
        self.capability == capability
            && match self.coverage {
                ApprovalCoverage::Exact => self.resource == *resource,
                ApprovalCoverage::Descendants => self.resource.contains(resource),
            }
    }

    #[must_use]
    pub fn permits(&self, grant: &Self) -> bool {
        self.covers(grant.capability, &grant.resource)
            && (self.coverage == ApprovalCoverage::Descendants
                || grant.coverage == ApprovalCoverage::Exact)
    }
}

/// A permission an invocation uses, and the coverage of the grant it proposes
/// for it. The proposal is derived from this permission's own capability and
/// resource, so it can never name a broader grant than the one shown.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PermissionUse {
    pub capability: Capability,
    pub resource: ResourceId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proposed: Option<ApprovalCoverage>,
}

impl PermissionUse {
    /// A permission reviewed each time, proposing no grant.
    #[must_use]
    pub const fn new(capability: Capability, resource: ResourceId) -> Self {
        Self {
            capability,
            resource,
            proposed: None,
        }
    }

    /// A permission proposing a grant for exactly its resource.
    #[must_use]
    pub const fn exact(capability: Capability, resource: ResourceId) -> Self {
        Self {
            capability,
            resource,
            proposed: Some(ApprovalCoverage::Exact),
        }
    }

    /// A permission proposing a grant for its resource and its descendants.
    #[must_use]
    pub const fn descendants(capability: Capability, resource: ResourceId) -> Self {
        Self {
            capability,
            resource,
            proposed: Some(ApprovalCoverage::Descendants),
        }
    }

    #[must_use]
    pub fn proposed_grant(&self) -> Option<ApprovalGrant> {
        self.proposed.map(|coverage| ApprovalGrant {
            capability: self.capability,
            resource: self.resource.clone(),
            coverage,
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum PathAccess {
    Read,
    Write,
}

impl PathAccess {
    #[must_use]
    pub const fn capability(self) -> Capability {
        match self {
            Self::Read => Capability::Read,
            Self::Write => Capability::Write,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct AuthorizationRequest {
    pub agent: AgentId,
    pub job: JobId,
    pub parent: Option<JobId>,
    pub tool: String,
    pub permissions: Vec<PermissionUse>,
    pub arguments: Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PolicyDecision {
    Allow { grants: Vec<ApprovalGrant> },
    Deny { reason: String },
}

impl PolicyDecision {
    #[must_use]
    pub const fn allow() -> Self {
        Self::Allow { grants: Vec::new() }
    }
}

pub type PolicyFuture<'a> = Pin<Box<dyn Future<Output = PolicyDecision> + Send + 'a>>;

pub trait Policy: Send + Sync {
    fn authorize(&self, request: AuthorizationRequest) -> PolicyFuture<'_>;
}

pub struct AllowAll;

impl Policy for AllowAll {
    fn authorize(&self, _request: AuthorizationRequest) -> PolicyFuture<'_> {
        Box::pin(async { PolicyDecision::allow() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_wire_admits_builtin_shapes_without_path_normalization() {
        for (namespace, segments, admitted) in [
            ("workspace", vec!["root", "relative//workspace/../"], true),
            ("path", vec!["root", "/", "", "a/b", "..", "."], true),
            ("network", vec!["root", "https://example.test:8443"], true),
            ("route", vec!["build", "jump", "0", "build", "7"], true),
            ("mcp", vec!["server", "native-tool"], true),
            ("session", vec!["name"], true),
            ("session", vec!["name", "extra"], false),
            ("mcp", vec!["server"], false),
            ("extension", vec!["root", "opaque"], false),
            ("workspace", vec!["root"], false),
            ("network", vec!["root", "origin", "extra"], false),
            ("route", vec!["destination", "hop"], false),
            ("route", vec!["destination", "hop", "-1"], false),
            ("route", vec!["root", "root", "1"], false),
            ("path", vec!["bad target", "/"], false),
        ] {
            let wire = serde_json::json!({"namespace": namespace, "segments": segments});
            let resource = serde_json::from_value::<ResourceId>(wire.clone());
            assert_eq!(resource.is_ok(), admitted, "{wire}");
            if let Ok(resource) = resource {
                assert_eq!(serde_json::to_value(resource).unwrap(), wire);
            }
        }
    }

    #[test]
    fn descendant_matching_preserves_vector_boundaries_and_route_identity() {
        let path = |path: &str| {
            ResourceId::path(
                &crate::target::TargetRef::Root,
                &PathText::new(path).unwrap(),
            )
        };
        let (parent, child) = (path("/a"), path("/a/b"));
        let exact = ApprovalGrant::exact(Capability::Read, parent.clone());
        let descendants = ApprovalGrant::descendants(Capability::Read, parent);
        assert!(!exact.covers(Capability::Read, &child));
        assert!(descendants.covers(Capability::Read, &child));
        assert!(!descendants.covers(Capability::Write, &child));
        assert!(!descendants.covers(Capability::Read, &path("/ab")));
        let name = |name: &str| name.parse::<TargetName>().unwrap();
        let hops = vec![(name("jump"), 1), (name("build"), 2)];
        let route = ResourceId::route(name("build"), hops.clone());
        let grant = ApprovalGrant::descendants(Capability::Targets, route.clone());
        assert!(grant.covers(Capability::Targets, &route));
        let longer = ResourceId::route(
            name("build"),
            vec![(name("jump"), 1), (name("build"), 2), (name("next"), 3)],
        );
        assert!(grant.covers(Capability::Targets, &longer));
        assert!(
            !ApprovalGrant::descendants(Capability::Targets, longer)
                .covers(Capability::Targets, &route)
        );
        assert!(!grant.covers(
            Capability::Targets,
            &ResourceId::route(name("build"), vec![(name("jump"), 2), (name("build"), 2)])
        ));
    }

    #[test]
    fn network_resources_are_scoped_by_target_and_origin() {
        let resource = ResourceId::network(&crate::target::TargetRef::Root, "https://example.test");
        let grant = ApprovalGrant::exact(Capability::Network, resource.clone());
        assert!(grant.covers(Capability::Network, &resource));
        for other in [
            ResourceId::network(&"build".parse().unwrap(), "https://example.test"),
            ResourceId::network(&crate::target::TargetRef::Root, "http://example.test"),
            ResourceId::network(&crate::target::TargetRef::Root, "https://example.test:8443"),
            ResourceId::network(&crate::target::TargetRef::Root, "https://other.test"),
        ] {
            assert!(!grant.covers(Capability::Network, &other));
        }
        assert!(!grant.covers(Capability::Read, &resource));
    }
}
