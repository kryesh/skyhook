//! The transport factory interface, and the backend that deploys shims over SSH and
//! WinRM.
use std::sync::Arc;

use crate::target::Route;
use futures_util::future::BoxFuture;

use super::{
    artifact::EmbeddedShimCatalog,
    client::PooledConnection,
    error::RemoteError,
    launch::{Channel, Launcher},
    route::leg,
};

use skyhook_worker::origin::{
    Leg, ProcessEnvironment,
    ssh::{Authentication, ConnectionSecret},
    transport::Transport,
};

#[derive(Clone)]
pub(crate) struct ConnectionRequest {
    /// The route to connect along, whose origin, if it has one, is already `origin`.
    pub route: Route,
    pub origin: Option<Arc<PooledConnection>>,
}

/// Session-owned transport construction and shared authentication.
///
/// Dropping a `connect` future or a transport's `owner` must release every
/// resource of that connection.
pub(crate) trait ConnectionFactory: Send + Sync {
    /// Open the shim byte stream; the manager performs the common handshake.
    fn connect(&self, request: ConnectionRequest) -> BoxFuture<'_, Result<Transport, RemoteError>>;

    /// The environment naming the session's managed SSH agent, started on first use.
    fn agent_environment(&self) -> BoxFuture<'_, Result<ProcessEnvironment, RemoteError>> {
        Box::pin(async { Ok(ProcessEnvironment::new()) })
    }

    /// Release session-owned resources once the session's connections have ended:
    /// its authentication, and the processes and shells those connections left to
    /// kill and delete.
    fn shutdown(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}

/// The session's backend: it deploys and starts a destination's shim over the
/// destination's transport, from root or the origin the request names. SSH
/// authentication stays lazy and shared across every connection in the session.
pub(crate) struct Backend {
    catalog: EmbeddedShimCatalog,
    passwords: crate::target::ConfiguredPasswords,
    authentication: Authentication,
    /// The SSH processes and WinRM shells this session's connections from root
    /// started, each until it is killed or deleted: finished before its targets shut
    /// down.
    work: skyhook_base::Work,
}

impl Backend {
    pub fn new(
        catalog: EmbeddedShimCatalog,
        passwords: crate::target::ConfiguredPasswords,
    ) -> Self {
        Self {
            catalog,
            passwords,
            authentication: Authentication::default(),
            work: Default::default(),
        }
    }
}

impl ConnectionFactory for Backend {
    fn connect(&self, request: ConnectionRequest) -> BoxFuture<'_, Result<Transport, RemoteError>> {
        Box::pin(async move {
            let destination = &request.route.destination;
            let channel = match (leg(&request.route, &self.passwords)?, request.origin) {
                (leg, Some(origin)) => Channel::Origin { leg, origin },
                (Leg::Ssh(route), None) => Channel::Ssh {
                    environment: self.authentication.route_environment(&route).await?,
                    secret: ConnectionSecret::default(),
                    work: self.work.clone(),
                    route,
                },
                (Leg::Winrm(route), None) => {
                    let client = route.connect(&self.work).await?;
                    Channel::Winrm { route, client }
                }
            };
            Launcher {
                root: destination.workspace.clone(),
                os: destination.os,
                channel,
            }
            .connect(&self.catalog)
            .await
        })
    }

    fn agent_environment(&self) -> BoxFuture<'_, Result<ProcessEnvironment, RemoteError>> {
        Box::pin(async { Ok(self.authentication.agent_environment().await?) })
    }

    fn shutdown(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {
            tokio::join!(self.authentication.shutdown(), self.work.finished());
        })
    }
}
