//! Remote transport sessions to shims over SSH and WinRM, and the platform artifacts
//! deployed as them.

mod artifact;
pub(crate) mod backend;
mod bootstrap;
mod client;
mod error;
mod launch;
mod manager;
mod route;

pub use artifact::{ArtifactError, EmbeddedShim, EmbeddedShimCatalog};
#[cfg(test)]
pub(crate) use backend::ConnectionFactory;
pub(crate) use client::{Flown, Landed};
pub(crate) use error::{RemoteError, RemoteFault};
#[cfg(test)]
pub(crate) use manager::tests::{ServingFactory, ShimFactory};
pub(crate) use manager::{PreparedConnection, RemoteManager};
