//! Remote platform artifacts and SSH execution support.

mod artifact;
mod askpass;
mod manager;
mod prompt;
mod protocol;
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
