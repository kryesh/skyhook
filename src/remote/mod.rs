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
mod ssh;
mod transport;
pub mod worker;

pub use artifact::{
    Arch, ArtifactError, EmbeddedShim, EmbeddedShimCatalog, Os, Platform, ShimProtocol,
};
#[cfg(test)]
pub(crate) use backend::ConnectionFactory;
pub(crate) use error::RemoteError;
#[cfg(test)]
pub(crate) use manager::tests::ShimFactory;
pub(crate) use manager::{PreparedConnection, RemoteManager};
pub use prompt::{
    PromptAnswer, RejectSensitivePrompts, Requester, RequesterExecutable, SecretValue,
    SensitivePrompt, SensitivePromptError, SensitivePromptFuture, SensitivePromptHandler,
    SensitivePromptKind,
};
pub use ssh::askpass_main;
pub(crate) use ssh::{AskpassServer, SshOption, apply_environment};
