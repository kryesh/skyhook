//! Transport errors: SSH and WinRM connections, their processes, and the frame contract.
use std::io;
use thiserror::Error;

use crate::op::OsError;

/// Why a transport could not be opened or failed while it ran, here or on the origin
/// that started it.
#[derive(Clone, Debug, Error, serde::Deserialize, serde::Serialize)]
pub enum TransportError {
    #[error("could not start transport process: {0}")]
    Start(OsError),
    #[error("target connection was cancelled")]
    Cancelled,
    #[error("SSH failed: {0}")]
    Ssh(#[from] SshError),
    #[error("WinRM failed: {0}")]
    Winrm(#[from] WinrmStreamError),
    #[error("remote protocol failed: {0}")]
    Protocol(#[from] crate::protocol::Violation),
    #[error("{0}")]
    Io(OsError),
}

/// A WinRM connection could not be made, or the command it ran failed.
#[derive(Clone, Debug, Error, serde::Deserialize, serde::Serialize)]
pub enum WinrmStreamError {
    #[error("target `{target}` password {error}")]
    Password {
        target: String,
        error: crate::secret::ResolveError,
    },
    #[error("target `{target}`: {error}")]
    Connection {
        target: String,
        error: crate::origin::winrm::WinrmError,
    },
    #[error("the command exited with {status}: {stderr}")]
    Exited {
        status: crate::op::ExitStatus,
        stderr: String,
    },
}

/// An SSH connection could not be configured or its stream failed.
#[derive(Clone, Debug, Error, serde::Deserialize, serde::Serialize)]
pub enum SshError {
    #[error(
        "target `{target}` offers only SSH agent keys, but no agent is available where its SSH connection starts; set ssh.key or ssh.password, or provide an agent in SSH_AUTH_SOCK"
    )]
    NoAgent { target: String },
    #[error(
        "target `{target}` forwards an SSH agent, but no agent is available where its SSH connection starts; provide one in SSH_AUTH_SOCK"
    )]
    NoAgentToForward { target: String },
    #[error("SSH values cannot be empty or contain control characters")]
    InvalidValue,
    #[error("SSH option names must be nonempty ASCII letters and digits")]
    InvalidOptionName,
    #[error("SSH users cannot be empty or contain whitespace or control characters")]
    InvalidUser,
    #[error("SSH exited with {status}: {stderr}")]
    Exited {
        status: crate::op::ExitStatus,
        stderr: String,
    },
    #[error("managed ssh-agent startup timed out")]
    AgentTimeout,
    #[error("native path cannot be represented losslessly over SSH")]
    UnrepresentablePath,
    #[error("target `{target}` password {error}")]
    Password {
        target: String,
        error: HopPasswordError,
    },
    #[error(transparent)]
    Prompt(#[from] PromptFailure),
}

/// The longest password OpenSSH reads at a prompt.
pub(crate) const MAX_PASSWORD_BYTES: usize = 1023;

/// Why the terminal ended the connection.
#[derive(Clone, Debug, Error, serde::Deserialize, serde::Serialize)]
pub enum PromptFailure {
    /// `prompt` is what the terminal showed before the secret was typed, so never
    /// the secret.
    #[error("SSH prompted again after target `{target}`'s secret answered: {prompt}")]
    Repeated { target: String, prompt: String },
    #[error("SSH terminal failed: {0}")]
    Terminal(crate::op::OsError),
}

/// Why a hop's password could not be had or typed.
#[derive(Clone, Debug, PartialEq, Eq, Error, serde::Deserialize, serde::Serialize)]
pub enum HopPasswordError {
    #[error(transparent)]
    Unavailable(#[from] crate::secret::ResolveError),
    #[error("contains a line break or control character, which a prompt cannot take")]
    Control,
    #[error("exceeds {MAX_PASSWORD_BYTES} bytes")]
    TooLong,
}

impl TransportError {
    pub(crate) fn start(error: io::Error) -> Self {
        Self::Start(error.into())
    }
}

/// A transport stream reports its producer's failure as its read error.
impl From<io::Error> for TransportError {
    fn from(error: io::Error) -> Self {
        error
            .downcast()
            .unwrap_or_else(|error| Self::Io(error.into()))
    }
}
