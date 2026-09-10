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
    Agents,
    Interactive,
    Mcp,
}

impl Capability {
    pub const ALL: [Self; 8] = [
        Self::Read,
        Self::Write,
        Self::Exec,
        Self::Network,
        Self::Targets,
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
    "unknown capability {0:?}; expected read, write, exec, network, targets, agents, interactive, or mcp"
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

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
pub struct ResourceId {
    pub namespace: String,
    pub segments: Vec<String>,
}

impl ResourceId {
    #[must_use]
    pub fn new(
        namespace: impl Into<String>,
        segments: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            namespace: namespace.into(),
            segments: segments.into_iter().map(Into::into).collect(),
        }
    }

    #[must_use]
    pub fn workspace(target: &str, workspace: &Path) -> Self {
        Self::new(
            "workspace",
            [target.to_owned(), workspace.to_string_lossy().into_owned()],
        )
    }

    #[must_use]
    pub fn path(target: &str, path: &Path) -> Self {
        let mut segments = vec![target.to_owned()];
        segments.extend(path.components().map(|component| match component {
            Component::Prefix(prefix) => prefix.as_os_str().to_string_lossy().into_owned(),
            Component::RootDir => "/".to_owned(),
            Component::CurDir => ".".to_owned(),
            Component::ParentDir => "..".to_owned(),
            Component::Normal(value) => value.to_string_lossy().into_owned(),
        }));
        Self::new("path", segments)
    }

    /// A destination scoped to one execution target and normalized HTTP(S)
    /// origin. Callers obtain the origin from a validated URL parser; omit path,
    /// query, user information and default ports. A permission using this resource
    /// should normally not propose a persistent grant.
    #[must_use]
    pub fn network(target: &str, normalized_origin: &str) -> Self {
        Self::new("network", [target, normalized_origin])
    }

    #[must_use]
    pub fn session(name: impl Into<String>) -> Self {
        Self::new("session", [name.into()])
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
            && self.resource.namespace == resource.namespace
            && match self.coverage {
                ApprovalCoverage::Exact => self.resource.segments == resource.segments,
                ApprovalCoverage::Descendants => {
                    resource.segments.starts_with(&self.resource.segments)
                }
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
        for capability in Capability::ALL {
            let name = capability.as_str();
            assert_eq!(name.parse::<Capability>().unwrap(), capability);
            assert_eq!(capability.to_string(), name);
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
        let exact: CapabilitySet = [Capability::Mcp, Capability::Mcp].into_iter().collect();
        assert_eq!(exact.iter().collect::<Vec<_>>(), [Capability::Mcp]);
        assert_eq!(
            std::iter::empty::<Capability>().collect::<CapabilitySet>(),
            CapabilitySet::empty()
        );
        let defaults = CapabilitySet::default();
        for capability in Capability::ALL {
            assert_eq!(
                defaults.contains(capability),
                capability != Capability::Targets
            );
        }
        let child = defaults.for_agent(0);
        assert!(!child.contains(Capability::Agents));
        assert!(child.contains(Capability::Interactive));
        assert!(child.contains(Capability::Mcp));
    }

    #[test]
    fn network_capability_serializes_and_is_enabled_by_default() {
        assert!(CapabilitySet::default().contains(Capability::Network));
        assert_eq!(
            serde_json::to_value(Capability::Network).unwrap(),
            "network"
        );
        assert_eq!(
            serde_json::from_value::<Capability>(serde_json::json!("network")).unwrap(),
            Capability::Network
        );
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
