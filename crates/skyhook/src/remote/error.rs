//! Remote connection, deployment and authorization errors, with tool-facing conversion.
use crate::target::ConnectionKind;
use crate::tool::authorization::AuthorizationError;
use skyhook_tools::{
    diagnostic::{ConnectionFault, PartialContext},
    error::{AdmissionError, OperationError},
};
use skyhook_worker::diagnostic::{Effects, Operation, Subject};
use skyhook_worker::target::{Os, Platform, SettingsError};
use skyhook_worker::{
    origin::error::{SshError, TransportError},
    protocol::Violation,
};
use std::{io, sync::Arc};
use thiserror::Error;

/// Why a target could not be reached, or its connection failed, with the facts its
/// call reports of it, such as whether it started. The facts are boxed so every
/// `Result` it fails stays small.
#[derive(Clone, Debug, Error)]
#[error("{fault}")]
pub(crate) struct RemoteError {
    pub(crate) fault: RemoteFault,
    facts: Box<PartialContext>,
}

#[derive(Clone, Debug, Error)]
pub(crate) enum RemoteFault {
    #[error("target connection authorization failed: {0}")]
    Authorization(AuthorizationError),
    #[error("remote connection startup task failed: {0}")]
    StartupFailed(#[source] Arc<tokio::task::JoinError>),
    #[error("remote shim deployment failed: {0}")]
    Deployment(#[from] DeploymentError),
    #[error(transparent)]
    PasswordUnconfigured(#[from] crate::target::PasswordUnconfigured),
    /// The worker refused a connection setting.
    #[error("{error}")]
    Refused {
        transport: ConnectionKind,
        error: SettingsError,
    },
    /// The host stopped the connection: its session, startup or caller was cancelled.
    #[error("target connection was cancelled")]
    Cancelled,
    #[error(transparent)]
    Transport(TransportError),
}

/// Why no shim could be installed on the remote host.
#[derive(Clone, Debug, Error)]
pub(crate) enum DeploymentError {
    #[error("platform probe returned invalid output")]
    Probe,
    #[error("unsupported remote platform `{os}-{arch}`")]
    UnsupportedPlatform { os: String, arch: String },
    /// The target's shell ran the commands of the system it declares, and named another.
    #[error("the target declares os {declared} but runs {reported}")]
    OsMismatch { declared: Os, reported: Os },
    #[error("no shims are embedded in this build, so remote targets are unavailable")]
    NoShims,
    #[error("no shim is embedded for remote platform {platform}")]
    NoShim { platform: Platform },
    #[error("embedded shim `{name}` could not be read")]
    Unreadable { name: Box<str> },
    #[error("shim installation failed")]
    Install,
    #[error("upload name generation failed: {0}")]
    Random(getrandom::Error),
}

impl RemoteError {
    /// Fill the facts still unset without replacing those already reported.
    #[must_use]
    pub(crate) fn or(mut self, context: PartialContext) -> Self {
        *self.facts = std::mem::take(&mut *self.facts).or(context);
        self
    }

    /// This failure of a call that never started.
    #[must_use]
    pub(crate) fn not_started(mut self) -> Self {
        *self.facts = std::mem::take(&mut *self.facts).effects(Effects::NotStarted);
        self
    }
}

/// A target that could not be reached fails the operation sent to it: with what its
/// fault says of itself, then the facts its call reported.
impl<O> From<RemoteError> for OperationError<O> {
    fn from(RemoteError { fault, facts }: RemoteError) -> Self {
        use ConnectionFault as Fault;
        let connect = |failed: Self, subject: &str| {
            failed.operation(Operation::Connect, Subject::Label(subject.into()))
        };
        let failed = match fault {
            RemoteFault::Authorization(error) => AdmissionError::from(error).widen(),
            RemoteFault::Cancelled | RemoteFault::Transport(TransportError::Cancelled) => {
                Self::cancelled()
            }
            RemoteFault::Transport(TransportError::Start(error) | TransportError::Io(error)) => {
                error.into()
            }
            RemoteFault::Refused { transport, error } => {
                connect(Self::connection(Fault::Refused, error), transport.as_str())
                    .effects(Effects::NotStarted)
            }
            RemoteFault::PasswordUnconfigured(error) => {
                connect(Self::connection(Fault::Password, error), "target route")
                    .effects(Effects::NotStarted)
            }
            RemoteFault::Deployment(error) => {
                connect(Self::connection(Fault::Deployment, error), "remote shim")
                    .effects(Effects::NotStarted)
            }
            error @ RemoteFault::StartupFailed(_) => Self::connection(Fault::Startup, error),
            RemoteFault::Transport(TransportError::Ssh(error)) => {
                connect(Self::connection(Fault::Ssh, error), "ssh")
            }
            RemoteFault::Transport(TransportError::Winrm(error)) => {
                connect(Self::connection(Fault::Winrm, error), "winrm")
            }
            RemoteFault::Transport(error @ TransportError::Protocol(_)) => {
                Self::connection(Fault::Protocol, error)
            }
        };
        failed.or(*facts)
    }
}

impl<F: Into<RemoteFault>> From<F> for RemoteError {
    fn from(fault: F) -> Self {
        Self {
            fault: fault.into(),
            facts: Box::default(),
        }
    }
}

impl From<std::convert::Infallible> for RemoteFault {
    fn from(never: std::convert::Infallible) -> Self {
        match never {}
    }
}

/// A transport stream reports its producer's failure as its read error.
impl From<io::Error> for RemoteError {
    fn from(error: io::Error) -> Self {
        error
            .downcast()
            .unwrap_or_else(|error| TransportError::from(error).into())
    }
}

impl From<SshError> for RemoteFault {
    fn from(error: SshError) -> Self {
        TransportError::from(error).into()
    }
}

impl From<Violation> for RemoteFault {
    fn from(violation: Violation) -> Self {
        TransportError::from(violation).into()
    }
}

impl From<TransportError> for RemoteFault {
    fn from(error: TransportError) -> Self {
        match error {
            TransportError::Cancelled => Self::Cancelled,
            error => Self::Transport(error),
        }
    }
}

impl From<AuthorizationError> for RemoteFault {
    fn from(error: AuthorizationError) -> Self {
        match error {
            AuthorizationError::Cancelled => Self::Cancelled,
            error => Self::Authorization(error),
        }
    }
}
