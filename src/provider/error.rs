use std::time::Duration;

use thiserror::Error;

crate::named_enum::named_enum! {
    /// Normalized failure classes, spelled as displayed. The runtime retries on the
    /// kind alone.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
    pub enum ProviderErrorKind {
        Authentication = "Authentication",
        /// A command-sourced value the server accepted before was refused with
        /// HTTP 401 and discarded: a retry runs the command again.
        CredentialExpired = "CredentialExpired",
        /// Quota, credit, or spend limit exhausted: never retried.
        Billing = "Billing",
        InvalidRequest = "InvalidRequest",
        ContextWindowExceeded = "ContextWindowExceeded",
        Protocol = "Protocol",
        Timeout = "Timeout",
        Transport = "Transport",
        RateLimited = "RateLimited",
        /// A retryable server-side failure: 5xx, overloaded, or an in-stream error.
        Unavailable = "Unavailable",
    }
}

impl ProviderErrorKind {
    #[must_use]
    pub fn error(self, message: impl Into<String>) -> ProviderError {
        ProviderError {
            kind: self,
            message: message.into(),
            retry_after: None,
        }
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[error("{kind}: {message}")]
pub struct ProviderError {
    kind: ProviderErrorKind,
    pub message: String,
    /// The server's retry hint, which only the kinds that retry on one carry.
    retry_after: Option<Duration>,
}

impl ProviderError {
    #[must_use]
    pub fn kind(&self) -> ProviderErrorKind {
        self.kind
    }

    /// Whether the runtime may retry an *uncommitted* response after this error.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(
            self.kind,
            ProviderErrorKind::CredentialExpired
                | ProviderErrorKind::RateLimited
                | ProviderErrorKind::Timeout
                | ProviderErrorKind::Transport
                | ProviderErrorKind::Unavailable
        )
    }

    /// Keep a server's retry hint, on the kinds that carry one.
    #[must_use]
    pub fn with_retry_after(mut self, hint: Option<Duration>) -> Self {
        use ProviderErrorKind::{RateLimited, Unavailable};
        if matches!(self.kind, RateLimited | Unavailable) {
            self.retry_after = hint;
        }
        self
    }

    /// The server-directed delay for a runtime-owned retry, when one was sent.
    #[must_use]
    pub fn retry_after(&self) -> Option<Duration> {
        self.retry_after
    }
}
