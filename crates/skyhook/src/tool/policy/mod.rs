//! Capabilities, permissions, and the policy deciding authorization requests.
use std::{future::Future, pin::Pin};

use crate::identity::AgentId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use skyhook_worker::job::JobId;

mod capability;
mod resource;

pub use capability::{
    Capability, CapabilitySet, HostCapability, HostCapabilityDeclaration, InvalidHostCapability,
};
pub use resource::{ApprovalCoverage, ApprovalGrant, PermissionUse, ResourceId, ResourceKind};

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

    /// A person's refusal. A request that may not ask anyone is refused this
    /// way when its policy would ask, so the call cannot tell it went unasked.
    #[must_use]
    pub fn refused() -> Self {
        Self::Deny {
            reason: "denied by user".into(),
        }
    }
}

pub type PolicyFuture<'a> = Pin<Box<dyn Future<Output = PolicyDecision> + Send + 'a>>;

/// How a policy answers a request.
pub enum Approval<'a> {
    /// Settled by the policy itself.
    Decided(PolicyDecision),
    /// Asked of someone, who is asked only once the future is first polled.
    Ask(PolicyFuture<'a>),
}

pub trait Policy: Send + Sync {
    fn authorize(&self, request: AuthorizationRequest) -> Approval<'_>;
}

pub struct AllowAll;

impl Policy for AllowAll {
    fn authorize(&self, _request: AuthorizationRequest) -> Approval<'_> {
        Approval::Decided(PolicyDecision::allow())
    }
}

/// What a mode grants. Interaction follows the host, so a mode never lists it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "CapabilitySet", into = "CapabilitySet")]
pub struct ModeCapabilities(CapabilitySet);

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("interactive is controlled by the runtime host, not the capability allowlist")]
pub struct InteractiveListed;

impl TryFrom<CapabilitySet> for ModeCapabilities {
    type Error = InteractiveListed;

    fn try_from(capabilities: CapabilitySet) -> Result<Self, Self::Error> {
        match capabilities.contains(&Capability::Interactive) {
            true => Err(InteractiveListed),
            false => Ok(Self(capabilities)),
        }
    }
}

impl From<ModeCapabilities> for CapabilitySet {
    fn from(capabilities: ModeCapabilities) -> Self {
        capabilities.0
    }
}

impl ModeCapabilities {
    /// Every default session capability a host does not supply.
    pub fn general() -> Self {
        let mut capabilities = CapabilitySet::default();
        capabilities.remove(&Capability::Interactive);
        Self(capabilities)
    }

    pub fn set(&self) -> &CapabilitySet {
        &self.0
    }

    pub fn iter(&self) -> impl Iterator<Item = &Capability> {
        self.0.iter()
    }
}
