//! Remote transport sessions, shim protocol, and platform artifacts.

mod artifact;
pub(crate) mod backend;
pub mod backends;
mod client;
mod manager;
mod prompt;
mod protocol;
mod service;
pub use backends::ssh;
pub(crate) use backends::ssh::AskpassServer;
mod transport;
pub mod worker;

pub use artifact::{ArtifactError, EmbeddedShim, EmbeddedShimCatalog};
#[cfg(test)]
pub(crate) use backend::{ConnectionFactory, ConnectionRequest};
#[cfg(test)]
pub(crate) use client::test_transport;
pub use manager::RemoteError;
pub(crate) use manager::{PreparedConnection, RemoteManager};
pub use prompt::{
    RejectSensitivePrompts, SecretValue, SensitivePrompt, SensitivePromptError,
    SensitivePromptFuture, SensitivePromptHandler, SensitivePromptKind,
};

pub fn run_askpass_helper(
    socket: &std::path::Path,
    prompt: String,
) -> Result<(), Box<dyn std::error::Error>> {
    backends::ssh::run_askpass_helper(socket, prompt)
}
