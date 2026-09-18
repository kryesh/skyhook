//! Remote transport and authorization errors, with tool-facing conversion.
use crate::{
    remote::ArtifactError,
    target::TargetError,
    tool::{ToolError, ToolOutput, authorization::AuthorizationError},
};
use thiserror::Error;

#[derive(Clone, Debug, Error)]
pub enum RemoteError {
    #[error(transparent)]
    Target(#[from] TargetError),
    #[error(transparent)]
    Artifact(#[from] ArtifactError),
    #[error(
        "this Skyhook build contains no remote shims; install with default features or provide an EmbeddedShimCatalog"
    )]
    MissingShims,
    #[error(
        "unsupported remote platform {os}-{protocol}-{arch}: no matching {protocol} shim is embedded"
    )]
    UnsupportedPlatform {
        protocol: String,
        arch: String,
        os: String,
    },
    #[error("could not start transport process: {message}")]
    Start {
        kind: std::io::ErrorKind,
        message: String,
    },
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
    #[error("remote operation denied: {0}")]
    OperationDenied(String),
    #[error("remote tool failed: {message}")]
    Remote {
        message: String,
        output: Option<Box<ToolOutput>>,
    },
    #[error("remote connection is missing {0}")]
    MissingPipe(&'static str),
    #[error("target route is empty")]
    EmptyRoute,
    #[error("remote platform probe returned invalid output")]
    InvalidProbe,
    #[error("SSH values cannot be empty or contain control characters")]
    InvalidSshValue,
    #[error("{message}")]
    Io {
        kind: std::io::ErrorKind,
        message: String,
    },
    #[error("{0}")]
    Json(String),
}

impl RemoteError {
    pub(crate) fn authorization(error: AuthorizationError) -> Self {
        match error {
            AuthorizationError::Denied(reason) => Self::ApprovalDenied(reason),
            AuthorizationError::Cancelled => Self::Cancelled,
            AuthorizationError::InvalidGrant(reason) => Self::ApprovalInvalidGrant(reason),
            AuthorizationError::Unavailable => Self::ApprovalUnavailable,
        }
    }

    pub(crate) fn start(error: std::io::Error) -> Self {
        Self::Start {
            kind: error.kind(),
            message: error.to_string(),
        }
    }

    pub(super) fn io(error: std::io::Error) -> Self {
        Self::Io {
            kind: error.kind(),
            message: error.to_string(),
        }
    }

    #[must_use]
    pub fn into_tool_error(self) -> ToolError {
        match self {
            Self::OperationDenied(reason) => ToolError::Denied(reason),
            Self::Remote {
                message,
                output: Some(output),
            } => ToolError::with_output(message, *output),
            Self::Remote {
                message,
                output: None,
            } => ToolError::Failed(message),
            error => ToolError::Failed(error.to_string()),
        }
    }
}

impl From<std::io::Error> for RemoteError {
    fn from(error: std::io::Error) -> Self {
        Self::io(error)
    }
}

impl From<serde_json::Error> for RemoteError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error.to_string())
    }
}
