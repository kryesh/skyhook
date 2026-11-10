//! What a call needs authorized: resources, the permissions over them, and the
//! grants an approval proposes.
use serde::Serialize;

use skyhook_base::enums::{Kinds, NamedEnum};
use skyhook_tools::target::TargetName;
use skyhook_tools::target::TargetRef;

use super::Capability;

/// The wire format is `{namespace, segments}`.
#[derive(Clone, Debug, Serialize, PartialEq, Eq, Hash, Kinds)]
#[serde(into = "ResourceWire")]
#[kinds(name = ResourceKind, serde(rename_all = "snake_case"))]
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

#[derive(Serialize)]
struct ResourceWire {
    namespace: ResourceKind,
    segments: Vec<String>,
}

impl ResourceId {
    #[must_use]
    pub fn workspace(target: &TargetRef, workspace: &skyhook_worker::op::TargetPath) -> Self {
        Self::Workspace {
            target: target.clone(),
            path: workspace.as_str().to_owned(),
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

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq, Hash, NamedEnum)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalCoverage {
    Exact,
    Descendants,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq, Hash)]
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

    pub fn covers(&self, capability: &Capability, resource: &ResourceId) -> bool {
        self.capability == *capability
            && match self.coverage {
                ApprovalCoverage::Exact => self.resource == *resource,
                ApprovalCoverage::Descendants => self.resource.contains(resource),
            }
    }

    pub fn permits(&self, grant: &Self) -> bool {
        self.covers(&grant.capability, &grant.resource)
            && (self.coverage == ApprovalCoverage::Descendants
                || grant.coverage == ApprovalCoverage::Exact)
    }
}

/// A permission an invocation uses, and the coverage of the grant it proposes
/// for it. The proposal is derived from this permission's own capability and
/// resource, so it can never name a broader grant than the one shown.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
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

    pub fn proposed_grant(&self) -> Option<ApprovalGrant> {
        self.proposed.map(|coverage| ApprovalGrant {
            capability: self.capability.clone(),
            resource: self.resource.clone(),
            coverage,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    impl ResourceId {
        /// A test's own path, split as this host, the session's, splits it.
        pub(crate) fn path(target: &TargetRef, path: impl AsRef<std::path::Path>) -> Self {
            let components = (path.as_ref().components())
                .map(|component| component.as_os_str().to_str().unwrap().to_owned())
                .collect();
            Self::Path {
                target: target.clone(),
                components,
            }
        }
    }

    #[test]
    fn descendant_matching_preserves_vector_boundaries_and_route_identity() {
        let path = |path: &str| ResourceId::path(&skyhook_tools::target::TargetRef::Root, path);
        let (parent, child) = (path("/a"), path("/a/b"));
        let exact = ApprovalGrant::exact(Capability::Read, parent.clone());
        let descendants = ApprovalGrant::descendants(Capability::Read, parent);
        assert!(!exact.covers(&Capability::Read, &child));
        assert!(descendants.covers(&Capability::Read, &child));
        assert!(!descendants.covers(&Capability::Write, &child));
        assert!(!descendants.covers(&Capability::Read, &path("/ab")));
        assert!(exact.permits(&exact));
        assert!(descendants.permits(&exact));
        assert!(!exact.permits(&descendants));
        let name = |name: &str| name.parse::<TargetName>().unwrap();
        let hops = vec![(name("jump"), 1), (name("build"), 2)];
        let route = ResourceId::route(name("build"), hops.clone());
        let grant = ApprovalGrant::descendants(Capability::Targets, route.clone());
        assert!(grant.covers(&Capability::Targets, &route));
        let longer = ResourceId::route(
            name("build"),
            vec![(name("jump"), 1), (name("build"), 2), (name("next"), 3)],
        );
        assert!(grant.covers(&Capability::Targets, &longer));
        assert!(
            !ApprovalGrant::descendants(Capability::Targets, longer)
                .covers(&Capability::Targets, &route)
        );
        assert!(!grant.covers(
            &Capability::Targets,
            &ResourceId::route(name("build"), vec![(name("jump"), 2), (name("build"), 2)])
        ));
    }

    #[test]
    fn network_resources_are_scoped_by_target_and_origin() {
        let resource = ResourceId::network(
            &skyhook_tools::target::TargetRef::Root,
            "https://example.test",
        );
        let grant = ApprovalGrant::exact(Capability::Network, resource.clone());
        assert!(grant.covers(&Capability::Network, &resource));
        for other in [
            ResourceId::network(&"build".parse().unwrap(), "https://example.test"),
            ResourceId::network(
                &skyhook_tools::target::TargetRef::Root,
                "http://example.test",
            ),
            ResourceId::network(
                &skyhook_tools::target::TargetRef::Root,
                "https://example.test:8443",
            ),
            ResourceId::network(
                &skyhook_tools::target::TargetRef::Root,
                "https://other.test",
            ),
        ] {
            assert!(!grant.covers(&Capability::Network, &other));
        }
        assert!(!grant.covers(&Capability::Read, &resource));
    }
}
