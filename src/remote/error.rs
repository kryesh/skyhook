//! Remote transport and authorization errors, with tool-facing conversion.
use crate::{
    remote::ArtifactError,
    target::TargetError,
    tool::{
        ToolError, ToolOutput,
        authorization::AuthorizationError,
        diagnostic::{Cause, Diagnostic, DiagnosticContext},
    },
};
use std::{io, sync::Arc};
use thiserror::Error;

#[derive(Clone, Debug, Error)]
pub enum RemoteError {
    #[error(transparent)]
    Target(#[from] TargetError),
    #[error(transparent)]
    Artifact(#[from] ArtifactError),
    #[error(transparent)]
    Session(std::sync::Arc<crate::session::SessionError>),
    #[error("could not start transport process: {source}")]
    Start { source: Arc<io::Error> },
    #[error("target connection was denied: {0}")]
    ApprovalDenied(String),
    #[error("target connection returned an invalid approval grant: {0}")]
    ApprovalInvalidGrant(String),
    #[error("target connection requires an unavailable capability")]
    ApprovalUnavailable,
    #[error("target connection was cancelled")]
    Cancelled,
    #[error("remote connection startup task failed: {0}")]
    ConnectionTask(String),
    #[error("SSH failed: {0}")]
    Ssh(String),
    #[error("remote shim deployment failed: {0}")]
    Deployment(String),
    #[error("remote protocol failed: {0}")]
    Protocol(String),
    #[error("{}", diagnostic.render(&Default::default()))]
    Remote {
        diagnostic: Box<Diagnostic>,
        output: Option<Box<ToolOutput>>,
    },
    #[error("target route is empty")]
    EmptyRoute,
    #[error("{source}")]
    Io { source: Arc<io::Error> },
    #[error("{0}")]
    Json(String),
}

impl RemoteError {
    /// Fill missing boundary facts without replacing those the source selected.
    #[must_use]
    pub(crate) fn fallback_context(self, context: DiagnosticContext) -> Self {
        let (diagnostic, output) = self
            .into_tool_error()
            .fallback_context(context)
            .into_parts();
        Self::Remote {
            diagnostic: Box::new(diagnostic),
            output: output.map(Box::new),
        }
    }

    pub(crate) fn authorization(error: AuthorizationError) -> Self {
        match error {
            AuthorizationError::Denied(reason) => Self::ApprovalDenied(reason),
            AuthorizationError::Cancelled => Self::Cancelled,
            AuthorizationError::InvalidGrant(reason) => Self::ApprovalInvalidGrant(reason),
            AuthorizationError::Unavailable => Self::ApprovalUnavailable,
        }
    }

    pub(crate) fn start(error: io::Error) -> Self {
        Self::Start {
            source: Arc::new(error),
        }
    }

    pub(super) fn io(error: io::Error) -> Self {
        Self::Io {
            source: Arc::new(error),
        }
    }

    #[must_use]
    pub fn into_tool_error(self) -> ToolError {
        match self {
            Self::Target(error) => error.into_admission_error().into(),
            Self::ApprovalDenied(reason) => ToolError::Denied(reason),
            Self::Cancelled => ToolError::Cancelled,
            Self::Session(source) => ToolError::from(source),
            Self::Remote { diagnostic, output } => ToolError::from_diagnostic(*diagnostic, output),
            Self::Start { source } | Self::Io { source } => ToolError::from_diagnostic(
                Diagnostic::new(DiagnosticContext::default(), Cause::io(&source)),
                None,
            ),
            error => ToolError::Failed(error.to_string()),
        }
    }
}

impl From<io::Error> for RemoteError {
    fn from(error: io::Error) -> Self {
        Self::io(error)
    }
}

impl From<serde_json::Error> for RemoteError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_io_cause_survives_remote_error_conversion() {
        for boundary in [RemoteError::start, RemoteError::io] {
            for error in [
                io::Error::from_raw_os_error(libc::ENOENT),
                io::Error::new(io::ErrorKind::PermissionDenied, "transport denied"),
            ] {
                let expected = Cause::io(&error);
                assert_eq!(
                    boundary(error).into_tool_error().diagnostic().cause,
                    expected
                );
            }
        }
    }
}
