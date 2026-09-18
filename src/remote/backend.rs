//! Transport protocol selection. Shim RPC (`protocol`) is independent of this registry.
//!
//! The manager splits routes at the credential-owning origin before asking a factory
//! for a byte stream. All factories, including test doubles, use the same handshake
//! and client setup; backend implementations own authentication and bootstrap.
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use futures_util::future::BoxFuture;

use super::{
    EmbeddedShimCatalog, RemoteError, SensitivePromptHandler, backends::ssh, client::Session,
};
use crate::target::{TargetDefinition, TargetError, TargetType};

pub(crate) use super::transport::Transport;

/// Backend-owned authentication variables for a process on its credential origin.
/// This is not a copy of the controller environment and is never an OpenSsh wire
/// argument. Remote workers use their own environment plus these internal values.
pub(crate) type ProcessEnvironment = BTreeMap<String, String>;

#[derive(Clone)]
pub(crate) struct ConnectionRequest {
    pub target: String,
    /// Only the hops after `origin`; credentials belong to that origin, not `via`.
    pub route: Vec<TargetDefinition>,
    pub workspace: PathBuf,
    pub origin: Option<Session>,
}

/// Session-owned transport construction and shared authentication resources.
///
/// The manager shares connection startup between callers: cancelling one waiter
/// must not cancel another waiter's startup. Session shutdown instead drops the
/// startup future, so dropping `connect` must release incomplete connection
/// resources rather than leave detached processes or channels running.
///
/// A successful transport must retain its backend resources in `Transport.owner`;
/// dropping that owner must terminate/release them, including when the common
/// shim handshake fails or is cancelled. Shared authentication belongs to this
/// factory's session lifetime, not to an individual connection or workspace.
/// The manager separately retains the credential-owning origin session.
pub(crate) trait ConnectionFactory: Send + Sync {
    /// Open the shim byte stream; the manager performs the common handshake.
    fn connect(&self, request: ConnectionRequest) -> BoxFuture<'_, Result<Transport, RemoteError>>;

    /// Obtain the session's process authentication environment, lazily if needed.
    fn environment(&self) -> BoxFuture<'_, Result<ProcessEnvironment, RemoteError>> {
        Box::pin(async { Ok(ProcessEnvironment::new()) })
    }

    /// Release session-owned authentication resources after startup cancellation.
    fn shutdown(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}

pub(crate) struct Backends {
    ssh: ssh::Backend,
}

impl Backends {
    pub fn new(catalog: EmbeddedShimCatalog, prompts: Arc<dyn SensitivePromptHandler>) -> Self {
        Self {
            ssh: ssh::Backend::new(catalog, prompts),
        }
    }
}

impl ConnectionFactory for Backends {
    fn connect(&self, request: ConnectionRequest) -> BoxFuture<'_, Result<Transport, RemoteError>> {
        Box::pin(async move {
            let destination = request.route.last().ok_or(RemoteError::EmptyRoute)?;
            debug_assert_eq!(request.target, destination.name);
            match destination.r#type {
                TargetType::Ssh => {
                    validate_route(&request.route)?;
                    self.ssh.connect(request).await
                }
                TargetType::Local => Err(TargetError::BuiltinOnly.into()),
            }
        })
    }

    fn environment(&self) -> BoxFuture<'_, Result<ProcessEnvironment, RemoteError>> {
        self.ssh.environment()
    }

    fn shutdown(&self) -> BoxFuture<'_, ()> {
        self.ssh.shutdown()
    }
}

fn validate_route(route: &[TargetDefinition]) -> Result<(), RemoteError> {
    if route.is_empty() {
        return Err(RemoteError::EmptyRoute);
    }
    // Do not silently feed other protocol hops to OpenSSH when protocols expand.
    if route.iter().any(|hop| hop.r#type != TargetType::Ssh) {
        return Err(RemoteError::Protocol("unsupported transport route".into()));
    }
    Ok(())
}

/// Adapter for the SSH-specific OpenSsh wire request, not a universal command API.
pub(crate) async fn open_ssh_request(
    route: &[TargetDefinition],
    command: &str,
    agent: &ssh::Authentication,
    prompts: Arc<dyn SensitivePromptHandler>,
) -> Result<Transport, RemoteError> {
    validate_route(route)?;
    let environment = agent.route_environment(route).await?;
    ssh::open(route, command, &environment, prompts).await
}

/// Backend-owned worker credentials stay alive for the worker's entire lifetime.
pub(super) struct WorkerBackends {
    ssh: ssh::WorkerAuthentication,
}

impl WorkerBackends {
    pub fn new(prompts: Arc<dyn SensitivePromptHandler>) -> std::io::Result<Self> {
        Ok(Self {
            ssh: ssh::WorkerAuthentication::new(prompts)?,
        })
    }

    pub fn environment(&self) -> &ProcessEnvironment {
        self.ssh.environment()
    }

    /// The worker's private agent for SSH processes it starts as an origin.
    pub fn ssh_agent(&self) -> Arc<ssh::Authentication> {
        self.ssh.agent()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_dispatch_rejects_empty_and_mixed_routes() {
        assert!(matches!(validate_route(&[]), Err(RemoteError::EmptyRoute)));
        let ssh = TargetDefinition::test("ssh", "/ssh", None);
        let mut local = TargetDefinition::test("local", "/local", None);
        local.r#type = TargetType::Local;
        assert!(matches!(
            validate_route(&[local, ssh.clone()]),
            Err(RemoteError::Protocol(_))
        ));
        assert!(validate_route(&[ssh]).is_ok());
    }
}
