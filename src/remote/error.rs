//! Remote transport and authorization errors, with tool-facing conversion.
use crate::{
    remote::{ArtifactError, Platform, ShimProtocol},
    target::TargetError,
    tool::{
        AdmissionError, ToolError, ToolOutput,
        authorization::AuthorizationError,
        diagnostic::{Cause, Effects, Operation, PartialContext, PartialDiagnostic, Subject},
    },
};
use std::{io, sync::Arc};
use thiserror::Error;

#[derive(Clone, Debug, Error)]
pub(crate) enum RemoteError {
    #[error(transparent)]
    Target(#[from] TargetError),
    #[error(transparent)]
    Artifact(#[from] ArtifactError),
    #[error(transparent)]
    Session(std::sync::Arc<crate::session::SessionError>),
    #[error("could not start transport process: {source}")]
    Start { source: Arc<io::Error> },
    #[error("target connection authorization failed: {0}")]
    Authorization(AuthorizationError),
    #[error("target connection was cancelled")]
    Cancelled,
    #[error("remote connection startup task failed: {0}")]
    ConnectionTask(String),
    #[error("SSH failed: {0}")]
    Ssh(#[from] SshError),
    #[error("remote shim deployment failed: {0}")]
    Deployment(#[from] DeploymentError),
    #[error("remote protocol failed: {0}")]
    Protocol(#[from] ProtocolError),
    #[error("{}", diagnostic.clone().resolve().render(&Default::default()))]
    Remote {
        diagnostic: Box<PartialDiagnostic>,
        output: Option<Box<ToolOutput>>,
    },
    #[error("target route is empty")]
    EmptyRoute,
    #[error("{source}")]
    Io { source: Arc<io::Error> },
}

/// Why no shim could be installed on the remote host.
#[derive(Clone, Debug, Error)]
pub(crate) enum DeploymentError {
    #[error("platform probe returned invalid output")]
    Probe,
    #[error("unsupported remote platform `{os}-{arch}`")]
    UnsupportedPlatform { os: String, arch: String },
    #[error("this Skyhook build contains no remote shims (SSH targets are unavailable)")]
    NoShims,
    #[error("no {protocol} shim is embedded for remote platform {platform}")]
    NoShim {
        protocol: ShimProtocol,
        platform: Platform,
    },
    #[error("shim installation failed")]
    Install,
    #[error("upload name generation failed: {0}")]
    Random(getrandom::Error),
}

/// An SSH connection could not be configured or its stream failed.
#[derive(Clone, Debug, Error)]
pub(crate) enum SshError {
    #[error(
        "target `{target}` uses external_agent, but SSH_AUTH_SOCK is not set where its SSH connection starts"
    )]
    ExternalAgentUnavailable { target: String },
    #[error("SSH values cannot be empty or contain control characters")]
    InvalidValue,
    #[error("{0}")]
    Stream(String),
}

/// The peer broke the frame contract; the connection is unusable.
#[derive(Clone, Debug, Error)]
pub(crate) enum ProtocolError {
    #[error("{0}")]
    Violation(&'static str),
    #[error("remote scoped permission used unexpected execution target `{0}`")]
    UnexpectedPermissionTarget(String),
    #[error("remote result could not be decoded: {0}")]
    Decode(String),
}

impl RemoteError {
    /// Fill the facts still unset without replacing those the source selected.
    #[must_use]
    pub(crate) fn or(self, context: PartialContext) -> Self {
        let (diagnostic, output) = self.into_tool_error().or(context).into_facts();
        Self::Remote {
            diagnostic: Box::new(diagnostic),
            output: output.map(Box::new),
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
    pub(crate) fn into_tool_error(self) -> ToolError {
        match self {
            Self::Target(error) => error.into_admission_error().into(),
            Self::Authorization(error) => AdmissionError::from(error).into(),
            Self::Cancelled => ToolError::cancelled(),
            Self::Session(source) => ToolError::from(source),
            Self::Remote { diagnostic, output } => {
                ToolError::from_facts(*diagnostic, output.map(|output| *output))
            }
            Self::Start { source } | Self::Io { source } => ToolError::cause(Cause::io(&source)),
            Self::Deployment(error) => ToolError::failed(error)
                .operation(Operation::Connect, Subject::Label("remote shim".into()))
                .effects(Effects::NotStarted),
            Self::Ssh(error) => {
                ToolError::failed(error).operation(Operation::Connect, Subject::Label("ssh".into()))
            }
            error @ (Self::Artifact(_)
            | Self::ConnectionTask(_)
            | Self::Protocol(_)
            | Self::EmptyRoute) => ToolError::failed(error),
        }
    }
}

impl From<io::Error> for RemoteError {
    fn from(error: io::Error) -> Self {
        Self::io(error)
    }
}

impl From<AuthorizationError> for RemoteError {
    fn from(error: AuthorizationError) -> Self {
        match error {
            AuthorizationError::Cancelled => Self::Cancelled,
            error => Self::Authorization(error),
        }
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
