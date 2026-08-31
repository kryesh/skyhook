use std::{future::Future, pin::Pin};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::identity::{AgentId, JobId};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolEffect {
    ReadWorkspace,
    WriteWorkspace,
    ReadHostResource,
    ExecuteProcess,
    Network,
    Interaction,
    SessionState,
    ManageTargets,
    RemoteAccess,
    Custom(String),
}

#[derive(Clone, Debug, Serialize)]
pub struct AuthorizationRequest {
    pub agent: AgentId,
    pub job: JobId,
    pub tool: String,
    pub effects: Vec<ToolEffect>,
    pub arguments: Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PolicyDecision {
    Allow,
    Deny { reason: String },
}

pub type PolicyFuture<'a> = Pin<Box<dyn Future<Output = PolicyDecision> + Send + 'a>>;

pub trait Policy: Send + Sync {
    fn authorize(&self, request: AuthorizationRequest) -> PolicyFuture<'_>;
}

pub struct AllowAll;

impl Policy for AllowAll {
    fn authorize(&self, _request: AuthorizationRequest) -> PolicyFuture<'_> {
        Box::pin(async { PolicyDecision::Allow })
    }
}
