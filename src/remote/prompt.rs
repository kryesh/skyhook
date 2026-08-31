use std::{future::Future, pin::Pin};

use thiserror::Error;
use zeroize::Zeroizing;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SensitivePromptKind {
    Password,
    KeyboardInteractive,
    KeyPassphrase,
    HostConfirmation,
}

#[derive(Debug)]
pub struct SensitivePrompt {
    pub kind: SensitivePromptKind,
    pub message: String,
}

pub struct SecretValue(Zeroizing<String>);

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
