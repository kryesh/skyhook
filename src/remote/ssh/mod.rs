//! OpenSSH backend: configuration, authentication, process transport, and bootstrap.
mod askpass;
mod authentication;
mod config;
mod process;

pub(crate) use askpass::AskpassServer;
pub(crate) use authentication::WorkerAuthentication;
pub(crate) use config::validate_option;
pub(crate) use process::open;

pub(crate) fn run_askpass_helper(
    socket: &std::path::Path,
    prompt: String,
) -> Result<(), Box<dyn std::error::Error>> {
    askpass::run_helper(socket, prompt)
}

use crate::remote::{
    EmbeddedShimCatalog, RemoteError, SensitivePromptHandler,
    backend::{ConnectionFactory, ConnectionRequest, ProcessEnvironment, Transport},
};
use futures_util::future::BoxFuture;
use std::sync::Arc;

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
            let destination = request.route.last().ok_or(RemoteError::EmptyRoute)?;
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
                route: request.route.clone(),
                environment,
                prompts: self.prompts.clone(),
            }
            .connect(destination, &request.workspace, &self.catalog)
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
