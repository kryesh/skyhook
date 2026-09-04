//! Shared execution-location identity.

use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::target::ROOT_TARGET;

/// Canonical execution target and workspace.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize, PartialEq, Eq, Hash)]
pub struct ExecutionLocation {
    pub target: String,
    pub workspace: PathBuf,
}

impl ExecutionLocation {
    #[must_use]
    pub fn root(workspace: PathBuf) -> Self {
        Self {
            target: ROOT_TARGET.to_owned(),
            workspace,
        }
    }

    #[must_use]
    pub fn named(target: impl Into<String>, workspace: PathBuf) -> Self {
        Self {
            target: target.into(),
            workspace,
        }
    }

    pub fn is_root(&self) -> bool {
        self.target == ROOT_TARGET
    }

    pub fn kind(&self) -> &'static str {
        if self.is_root() { "local" } else { "ssh" }
    }
}
