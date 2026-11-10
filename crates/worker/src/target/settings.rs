//! The settings one connection to a worker runs under.
use serde::{Deserialize, Serialize};

use crate::op::OsError;

/// What one connection to a worker runs under. A relative path is in the home
/// directory (`%USERPROFILE%` on Windows), where a login starts.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Settings {
    pub authorization_root: String,
}

/// A setting, the connection's or a call's workspace, that names nothing the target
/// can use.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, thiserror::Error)]
pub enum SettingsError {
    #[error("there is no home directory for relative paths")]
    NoHome,
    #[error("authorization root {path}: {error}")]
    AuthorizationRoot { path: String, error: OsError },
    #[error("workspace {path}: {error}")]
    Workspace { path: String, error: OsError },
    #[error("{path} is not text")]
    Unspelled { path: String },
}
