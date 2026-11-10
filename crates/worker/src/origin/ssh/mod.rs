//! OpenSSH connections: configuration, authentication, terminals and processes.
mod authentication;
mod config;
mod process;
mod route;
mod terminal;

use super::{error::TransportError, transport::Transport};
pub use route::{Hop, HostKeyChecking, HostKeyUpdates, SshOptionName, SshRoute, SshUser, SshValue};

pub use authentication::Authentication;
pub use process::open;
pub use terminal::ConnectionSecret;

/// The SSH origin a worker is. Its connections use the agents the worker offers and
/// read their hop's secret on the worker.
#[derive(Default)]
pub(crate) struct Origin(Authentication);

impl Origin {
    /// Start SSH along `route`, running `command` at its destination, its process
    /// supervised by a task of `work`.
    pub(crate) async fn open(
        &self,
        route: &SshRoute,
        command: &str,
        work: &skyhook_base::Work,
    ) -> Result<Transport, TransportError> {
        let environment = self.0.route_environment(route).await?;
        let secret = ConnectionSecret::default();
        open(route, command, &environment, &secret, work).await
    }
}
