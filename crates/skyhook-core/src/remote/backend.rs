//! Transport-neutral session interface. Protocol and process ownership belong to each backend.
use super::{RemoteError, ssh::ResolvedSsh, transport::Transport};
use crate::{
    target::TargetDefinition,
    tool::{ToolContext, ToolOutput},
};
use std::sync::Arc;
pub(crate) type Session = Arc<dyn TargetSession>;

#[async_trait::async_trait]
pub(crate) trait TargetSession: Send + Sync {
    async fn execute(
        &self,
        name: String,
        arguments: serde_json::Value,
        context: &ToolContext,
    ) -> Result<ToolOutput, RemoteError>;
    async fn resolve_ssh(&self, _target: TargetDefinition) -> Result<ResolvedSsh, RemoteError> {
        Err(RemoteError::Protocol(
            "this target backend cannot resolve SSH configuration".into(),
        ))
    }
    async fn open_ssh(
        self: Arc<Self>,
        _route: Vec<TargetDefinition>,
        _command: String,
    ) -> Result<Transport, RemoteError> {
        Err(RemoteError::Protocol(
            "this target backend cannot launch SSH connections".into(),
        ))
    }
}
