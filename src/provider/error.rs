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
        /// Skyhook's own work on the request failed, such as a panic: never retried.
        Internal = "Internal",
    }
}

impl ProviderErrorKind {
    /// The kind an HTTP status names on its own.
    pub(crate) fn of_status(status: u16) -> Self {
        match status {
            401 | 403 => Self::Authentication,
            402 => Self::Billing,
            408 | 504 => Self::Timeout,
            429 => Self::RateLimited,
            409 | 425 => Self::Unavailable,
            300..=499 => Self::InvalidRequest,
            _ => Self::Unavailable,
        }
    }

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
    pub fn kind(&self) -> ProviderErrorKind {
        self.kind
    }

    /// Whether the runtime may retry an *uncommitted* response after this error.
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
    pub fn retry_after(&self) -> Option<Duration> {
        self.retry_after
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::named_enum::NamedEnum;

    #[test]
    fn only_transient_kinds_are_retryable() {
        use ProviderErrorKind::{CredentialExpired, RateLimited, Timeout, Transport, Unavailable};
        let transient = [
            CredentialExpired,
            RateLimited,
            Timeout,
            Transport,
            Unavailable,
        ];
        for &kind in <ProviderErrorKind as NamedEnum>::ALL {
            let retryable = kind.error("retry this request").is_retryable();
            assert_eq!(retryable, transient.contains(&kind), "{kind}");
        }
    }
}
