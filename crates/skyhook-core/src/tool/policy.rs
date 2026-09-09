use std::{
    collections::BTreeSet,
    future::Future,
    path::{Component, Path},
    pin::Pin,
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
}

impl Capability {
    pub(crate) const ALL: [Self; 6] = [
        Self::Read,
        Self::Write,
        Self::Exec,
        Self::Network,
        Self::Targets,
        Self::Agents,
    ];
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapabilitySet(BTreeSet<Capability>);

impl CapabilitySet {
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

impl Default for CapabilitySet {
    fn default() -> Self {
        Self(BTreeSet::from([
            Capability::Read,
            Capability::Write,
            Capability::Exec,
            Capability::Network,
            Capability::Agents,
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
