use std::{future::Future, path::PathBuf, pin::Pin};

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
    ExternalPath {
        path: PathBuf,
        access: PathAccess,
        directory: bool,
    },
    Custom(String),
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum PathAccess {
    Read,
    Write,
}

#[derive(Clone, Debug, Serialize)]
pub struct AuthorizationRequest {
    pub agent: AgentId,
    pub job: JobId,
    pub parent: Option<JobId>,
    #[serde(skip)]
    pub(crate) scope: Option<u64>,
    pub tool: String,
    /// Local root or the named SSH target on which the effect occurs.
    pub target: String,
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
