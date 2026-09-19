//! The transport factory interface shared by the SSH backend and test doubles.
use std::{collections::BTreeMap, path::PathBuf};

use futures_util::future::BoxFuture;

use super::{RemoteError, client::Session};
use crate::target::TargetDefinition;

pub(crate) use super::transport::Transport;

/// Authentication variables added to processes started on a credential origin.
pub(crate) type ProcessEnvironment = BTreeMap<String, String>;

#[derive(Clone)]
pub(crate) struct ConnectionRequest {
    /// Only the hops after `origin`; credentials belong to that origin, not `via`.
    pub route: Vec<TargetDefinition>,
    pub workspace: PathBuf,
    pub origin: Option<Session>,
}

/// Session-owned transport construction and shared authentication.
///
/// Dropping a `connect` future or a transport's `owner` must release every
/// resource of that connection.
pub(crate) trait ConnectionFactory: Send + Sync {
    /// Open the shim byte stream; the manager performs the common handshake.
    fn connect(&self, request: ConnectionRequest) -> BoxFuture<'_, Result<Transport, RemoteError>>;

    /// Obtain the session's process authentication environment, lazily if needed.
    fn environment(&self) -> BoxFuture<'_, Result<ProcessEnvironment, RemoteError>> {
        Box::pin(async { Ok(ProcessEnvironment::new()) })
    }

    /// Release session-owned authentication resources after startup cancellation.
    fn shutdown(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}
