use std::{
    collections::BTreeSet,
    fmt,
    future::Future,
    path::{Component, Path},
    pin::Pin,
    str::FromStr,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::identity::{AgentId, JobId};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Read,
    Write,
    Exec,
    Network,
    Targets,
    /// Forward an SSH agent Skyhook does not own (`ssh.external_agent`).
    SshAgent,
    Agents,
    Interactive,
    Mcp,
}

impl Capability {
    pub const ALL: [Self; 9] = [
        Self::Read,
        Self::Write,
        Self::Exec,
        Self::Network,
        Self::Targets,
        Self::SshAgent,
        Self::Agents,
        Self::Interactive,
        Self::Mcp,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Exec => "exec",
            Self::Network => "network",
            Self::Targets => "targets",
            Self::SshAgent => "ssh_agent",
            Self::Agents => "agents",
            Self::Interactive => "interactive",
            Self::Mcp => "mcp",
        }
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "unknown capability {0:?}; expected read, write, exec, network, targets, ssh_agent, agents, interactive, or mcp"
)]
pub struct ParseCapabilityError(String);

impl FromStr for Capability {
    type Err = ParseCapabilityError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|capability| capability.as_str() == value)
            .ok_or_else(|| ParseCapabilityError(value.to_owned()))
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

/// The wire format is `{namespace, segments}`; unknown namespaces and malformed
/// shapes are rejected. Decoding never normalizes paths.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(try_from = "ResourceWire", into = "ResourceWire")]
pub enum ResourceId {
    Workspace {
        target: String,
        path: String,
    },
    Path {
        target: String,
        components: Vec<String>,
    },
    Network {
        target: String,
        origin: String,
    },
    Route {
        destination: String,
        hops: Vec<(String, u64)>,
    },
    Session {
        name: String,
    },
    Mcp {
        server: String,
        tool: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("unknown or malformed permission resource: {0}")]
pub struct ResourceError(String);

#[derive(Deserialize, Serialize)]
struct ResourceWire {
    namespace: String,
    segments: Vec<String>,
}

impl ResourceId {
    #[must_use]
    pub fn workspace(target: &str, workspace: &Path) -> Self {
        Self::Workspace {
            target: target.into(),
            path: workspace.to_string_lossy().into_owned(),
        }
    }

    #[must_use]
    pub fn path(target: &str, path: &Path) -> Self {
        let components = path
            .components()
            .map(|component| match component {
                Component::Prefix(prefix) => prefix.as_os_str().to_string_lossy().into_owned(),
                Component::RootDir => "/".to_owned(),
                Component::CurDir => ".".to_owned(),
                Component::ParentDir => "..".to_owned(),
                Component::Normal(value) => value.to_string_lossy().into_owned(),
            })
            .collect();
        Self::Path {
            target: target.into(),
            components,
        }
    }

    /// Callers obtain the normalized HTTP(S) origin from a validated URL parser,
    /// omitting path, query, user information and default ports.
    #[must_use]
    pub fn network(target: &str, normalized_origin: &str) -> Self {
        Self::Network {
            target: target.into(),
            origin: normalized_origin.into(),
        }
    }

    #[must_use]
    pub fn session(name: impl Into<String>) -> Self {
        Self::Session { name: name.into() }
    }

    #[must_use]
    pub fn route(destination: impl Into<String>, hops: Vec<(String, u64)>) -> Self {
        Self::Route {
            destination: destination.into(),
            hops,
        }
    }

    #[must_use]
    pub fn mcp(server: impl Into<String>, tool: impl Into<String>) -> Self {
        Self::Mcp {
            server: server.into(),
            tool: tool.into(),
        }
    }

    /// Only these three resource kinds are scoped to an execution target.
    pub fn execution_target_mut(&mut self) -> Option<&mut String> {
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
        // Arity is validated here only. Empty strings, relative components and
        // whole workspace paths retain their historical opaque spelling.
        match (namespace.as_str(), segments.as_slice()) {
            ("workspace", [target, path]) => Ok(Self::Workspace {
                target: target.clone(),
                path: path.clone(),
            }),
            ("path", [target, components @ ..]) => Ok(Self::Path {
                target: target.clone(),
                components: components.to_vec(),
            }),
            ("network", [target, origin]) => Ok(Self::network(target, origin)),
            ("session", [name]) => Ok(Self::session(name)),
            ("mcp", [server, tool]) => Ok(Self::mcp(server, tool)),
            ("route", [destination, hops @ ..]) if hops.len() % 2 == 0 => {
                let hops = hops
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|pair| {
                        let revision = pair[1]
                            .parse::<u64>()
                            .map_err(|_| ResourceError("route revision must be a u64".into()))?;
                        Ok((pair[0].clone(), revision))
                    })
                    .collect::<Result<_, ResourceError>>()?;
                Ok(Self::route(destination, hops))
            }
            _ => Err(ResourceError(namespace)),
        }
    }
}

impl From<ResourceId> for ResourceWire {
    fn from(resource: ResourceId) -> Self {
        let (namespace, segments) = match resource {
            ResourceId::Workspace { target, path } => ("workspace", vec![target, path]),
            ResourceId::Path { target, components } => {
                ("path", std::iter::once(target).chain(components).collect())
            }
            ResourceId::Network { target, origin } => ("network", vec![target, origin]),
            ResourceId::Route { destination, hops } => (
                "route",
                std::iter::once(destination)
                    .chain(
                        hops.into_iter()
                            .flat_map(|(target, revision)| [target, revision.to_string()]),
                    )
                    .collect(),
            ),
            ResourceId::Session { name } => ("session", vec![name]),
            ResourceId::Mcp { server, tool } => ("mcp", vec![server, tool]),
        };
        Self {
            namespace: namespace.into(),
            segments,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalCoverage {
    Exact,
    Descendants,
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

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PermissionUse {
    pub capability: Capability,
    pub resource: ResourceId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proposed_grant: Option<ApprovalGrant>,
}

impl PermissionUse {
    #[must_use]
    pub const fn new(capability: Capability, resource: ResourceId) -> Self {
        Self {
            capability,
            resource,
            proposed_grant: None,
        }
    }

    #[must_use]
    pub fn with_grant(mut self, grant: ApprovalGrant) -> Self {
        self.proposed_grant = Some(grant);
        self
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
    #[serde(skip)]
    pub(crate) scope: Option<u64>,
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
    fn capability_names_round_trip_and_reject_unknown_names() {
        assert_eq!(Capability::Network.as_str(), "network");
        for capability in Capability::ALL {
            let name = capability.as_str();
            assert_eq!(name.parse::<Capability>().unwrap(), capability);
            assert_eq!(serde_json::to_value(capability).unwrap(), name);
            assert_eq!(
                serde_json::from_value::<Capability>(serde_json::json!(name)).unwrap(),
                capability
            );
        }
        for invalid in ["", "READ", " read", "read,write", "unknown"] {
            assert!(invalid.parse::<Capability>().is_err());
        }
    }

    #[test]
    fn exact_sets_do_not_inherit_defaults() {
        assert_eq!(CapabilitySet::empty().iter().count(), 0);
        let exact: CapabilitySet = [Capability::Mcp].into_iter().collect();
        assert_eq!(exact.iter().collect::<Vec<_>>(), [Capability::Mcp]);
        assert_eq!(
            std::iter::empty::<Capability>().collect::<CapabilitySet>(),
            CapabilitySet::empty()
        );
        let defaults = CapabilitySet::default();
        for capability in Capability::ALL {
            assert_eq!(
                defaults.contains(capability),
                !matches!(capability, Capability::Targets | Capability::SshAgent)
            );
        }
        let child = defaults.for_agent(0);
        assert!(!child.contains(Capability::Agents));
        assert!(child.contains(Capability::Interactive));
        assert!(child.contains(Capability::Mcp));
    }

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
        let parent = ResourceId::path("root", Path::new("/a"));
        let child = ResourceId::path("root", Path::new("/a/b"));
        let exact = ApprovalGrant::exact(Capability::Read, parent.clone());
        let descendants = ApprovalGrant::descendants(Capability::Read, parent);
        assert!(!exact.covers(Capability::Read, &child));
        assert!(descendants.covers(Capability::Read, &child));
        assert!(!descendants.covers(Capability::Write, &child));
        assert!(!descendants.covers(
            Capability::Read,
            &ResourceId::path("root", Path::new("/ab"))
        ));
        let hops = vec![("jump".into(), 1), ("build".into(), 2)];
        let route = ResourceId::route("build", hops.clone());
        let grant = ApprovalGrant::descendants(Capability::Targets, route.clone());
        assert!(grant.covers(Capability::Targets, &route));
        let longer = ResourceId::route(
            "build",
            vec![("jump".into(), 1), ("build".into(), 2), ("next".into(), 3)],
        );
        assert!(grant.covers(Capability::Targets, &longer));
        assert!(
            !ApprovalGrant::descendants(Capability::Targets, longer)
                .covers(Capability::Targets, &route)
        );
        assert!(!grant.covers(
            Capability::Targets,
            &ResourceId::route("build", vec![("jump".into(), 2), ("build".into(), 2)])
        ));
    }

    #[test]
    fn network_resources_are_scoped_by_target_and_origin() {
        let resource = ResourceId::network("root", "https://example.test");
        let grant = ApprovalGrant::exact(Capability::Network, resource.clone());
        assert!(grant.covers(Capability::Network, &resource));
        for other in [
            ResourceId::network("build", "https://example.test"),
            ResourceId::network("root", "http://example.test"),
            ResourceId::network("root", "https://example.test:8443"),
            ResourceId::network("root", "https://other.test"),
        ] {
            assert!(!grant.covers(Capability::Network, &other));
        }
        assert!(!grant.covers(Capability::Read, &resource));
    }
}
