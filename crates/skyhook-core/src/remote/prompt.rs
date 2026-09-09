use std::{future::Future, pin::Pin};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use zeroize::Zeroizing;

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
}

pub struct SecretValue(Zeroizing<String>);

#[derive(Debug, Deserialize, Serialize)]
pub(crate) enum PromptAnswer {
    Accepted(SecretValue),
    Rejected,
}

impl SecretValue {
    #[must_use]
    pub fn new(value: String) -> Self {
        Self(Zeroizing::new(value))
    }

    #[must_use]
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
    Pin<Box<dyn Future<Output = Result<SecretValue, SensitivePromptError>> + Send + 'static>>;

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
