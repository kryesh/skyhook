//! Remote transport sessions, shim protocol, and platform artifacts.

mod artifact;
mod askpass;
pub(crate) mod authentication;
mod backend;
mod manager;
mod prompt;
mod protocol;
mod service;
pub mod ssh;
mod transport;
pub mod worker;

pub use artifact::{ArtifactError, EmbeddedShim, EmbeddedShimCatalog};
pub use manager::RemoteError;
#[cfg(test)]
pub(crate) use manager::{ConnectionFactory, ConnectionRequest, PooledConnection, test_connection};
pub(crate) use manager::{PreparedConnection, RemoteManager};
pub use prompt::{
    RejectSensitivePrompts, SecretValue, SensitivePrompt, SensitivePromptError,
    SensitivePromptFuture, SensitivePromptHandler, SensitivePromptKind,
};

pub fn run_askpass_helper(
    socket: &std::path::Path,
    prompt: String,
) -> Result<(), Box<dyn std::error::Error>> {
    askpass::run_helper(socket, prompt)
}
