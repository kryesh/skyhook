use std::{future::Future, path::PathBuf, pin::Pin};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use zeroize::Zeroizing;

use crate::{
    identity::JobId,
    target::{TargetName, TargetRef},
};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum SensitivePromptKind {
    Password,
    KeyboardInteractive,
    KeyPassphrase,
    HostConfirmation,
    AgentConfirmation,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SensitivePrompt {
    pub kind: SensitivePromptKind,
    pub message: String,
    /// The destination of the SSH connection that prompted, even when a jump hop
    /// is authenticating; none for an agent's own prompt.
    pub target: Option<TargetName>,
    /// Where the prompting process runs.
    pub origin: TargetRef,
    pub requester: Requester,
}

/// The process that ran the askpass helper, as far as the machine serving the
/// prompt could tell. Best effort: any process running as the server's user can
/// connect as the helper, and is then attributed to its own parent.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Requester {
    pub executable: RequesterExecutable,
    /// The running job whose process tree it belongs to.
    pub job: Option<JobId>,
}

/// What the requesting program is. A process chooses its own name, arguments and
/// environment, but not its executable or, without a set-user-ID program, its user.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum RequesterExecutable {
    Path(PathBuf),
    /// A program running as another user, such as set-user-ID `sudo`, whose
    /// executable the serving user may not read; only its name and user are known.
    Privileged {
        name: String,
        uid: u32,
    },
    #[default]
    Unknown,
}

#[cfg(test)]
impl SensitivePrompt {
    pub(crate) fn test(kind: SensitivePromptKind) -> Self {
        Self {
            kind,
            message: "fixture".into(),
            target: None,
            origin: TargetRef::Root,
            requester: Requester::default(),
        }
    }
}

impl SensitivePromptKind {
    /// Host-key and agent-key prompts are answered `Confirmed` or `Rejected`;
    /// every other kind is answered with a `Secret`.
    pub const fn is_confirmation(self) -> bool {
        matches!(self, Self::HostConfirmation | Self::AgentConfirmation)
    }
}

pub struct SecretValue(Zeroizing<String>);

/// A handler's answer to a sensitive prompt.
#[derive(Debug, Deserialize, Serialize)]
pub enum PromptAnswer {
    Secret(SecretValue),
    Confirmed,
    Rejected,
}

impl SecretValue {
    #[must_use]
    pub fn new(value: String) -> Self {
        Self(Zeroizing::new(value))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretValue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SecretValue([REDACTED])")
    }
}

pub type SensitivePromptFuture =
    Pin<Box<dyn Future<Output = Result<PromptAnswer, SensitivePromptError>> + Send + 'static>>;

pub trait SensitivePromptHandler: Send + Sync {
    fn prompt(&self, prompt: SensitivePrompt) -> SensitivePromptFuture;
}

pub struct RejectSensitivePrompts;

impl SensitivePromptHandler for RejectSensitivePrompts {
    fn prompt(&self, _prompt: SensitivePrompt) -> SensitivePromptFuture {
        Box::pin(async { Err(SensitivePromptError::Unavailable) })
    }
}

#[derive(Debug, Error)]
pub enum SensitivePromptError {
    #[error("interactive authentication is unavailable")]
    Unavailable,
    #[error("interactive authentication was cancelled")]
    Cancelled,
    #[error("interactive authentication failed: {0}")]
    Failed(String),
}

impl Serialize for SecretValue {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.expose())
    }
}
impl<'de> Deserialize<'de> for SecretValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self::new)
    }
}
