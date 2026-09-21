//! Remote transport sessions, shim protocol, and platform artifacts.

mod artifact;
pub(crate) mod backend;
mod client;
mod error;
mod flow;
mod manager;
mod payload;
mod prompt;
mod protocol;
mod service;
pub mod ssh;
mod transport;
pub mod worker;

pub use artifact::{ArtifactError, EmbeddedShim, EmbeddedShimCatalog};
#[cfg(test)]
pub(crate) use backend::{ConnectionFactory, ConnectionRequest};
#[cfg(test)]
pub(crate) use client::test_transport;
pub use manager::RemoteError;
#[cfg(test)]
pub(crate) use manager::tests::PendingHandshakeFactory;
pub(crate) use manager::{PreparedConnection, RemoteManager};
pub use prompt::{
    RejectSensitivePrompts, SecretValue, SensitivePrompt, SensitivePromptError,
    SensitivePromptFuture, SensitivePromptHandler, SensitivePromptKind,
};
pub(crate) use ssh::AskpassServer;

pub fn run_askpass_helper(
    socket: &std::path::Path,
    prompt: String,
) -> Result<(), Box<dyn std::error::Error>> {
    ssh::run_askpass_helper(socket, prompt)
}
