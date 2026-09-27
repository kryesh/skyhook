//! OpenSSH backend: configuration, authentication, process transport, and bootstrap.
mod askpass;
mod authentication;
mod config;
mod process;

pub(crate) use askpass::AskpassServer;
pub use askpass::askpass_main;
pub(crate) use authentication::WorkerAuthentication;
pub(crate) use config::SshOption;
pub(crate) use process::open;

use crate::remote::{
    artifact::EmbeddedShimCatalog,
    backend::{ConnectionFactory, ConnectionRequest, ProcessEnvironment, Transport},
    error::RemoteError,
    prompt::SensitivePromptHandler,
};
use futures_util::future::BoxFuture;
use std::sync::Arc;

const AUTH_SOCK: &str = "SSH_AUTH_SOCK";

/// Session-owned OpenSSH implementation of the common transport factory.
/// Authentication stays lazy and shared across every connection in the session.
pub(crate) struct Backend {
    catalog: EmbeddedShimCatalog,
    prompts: Arc<dyn SensitivePromptHandler>,
    authentication: authentication::Authentication,
}

impl Backend {
    pub fn new(catalog: EmbeddedShimCatalog, prompts: Arc<dyn SensitivePromptHandler>) -> Self {
        Self {
            catalog,
            authentication: authentication::Authentication::new(prompts.clone()),
            prompts,
        }
    }
}

impl ConnectionFactory for Backend {
    fn connect(&self, request: ConnectionRequest) -> BoxFuture<'_, Result<Transport, RemoteError>> {
        Box::pin(async move {
            // A remote origin's shim chooses agents for the SSH process it starts.
            let environment = if request.origin.is_some() {
                ProcessEnvironment::new()
            } else {
                self.authentication
                    .route_environment(&request.route)
                    .await?
            };
            process::SshLauncher {
                origin: request.origin,
                route: request.route,
                environment,
                prompts: self.prompts.clone(),
            }
            .connect(&request.workspace, &self.catalog)
            .await
        })
    }

    fn environment(&self) -> BoxFuture<'_, Result<ProcessEnvironment, RemoteError>> {
        Box::pin(self.authentication.environment())
    }

    fn shutdown(&self) -> BoxFuture<'_, ()> {
        Box::pin(self.authentication.shutdown())
    }
}
