use std::{fmt, time::Duration};

use thiserror::Error;

use crate::provider::protocol::{ContextId, ModelRequest, ResponseEvent};

pub mod backends;
pub mod profile;
pub mod protocol;

/// An owned, movable response; consuming or dropping it releases invocation state.
/// An error is terminal: nothing follows it.
pub type ResponseStream =
    futures_util::stream::BoxStream<'static, Result<ResponseEvent, ProviderError>>;

pub trait Provider: Send + Sync {
    /// Create independently owned conversation state without making a model request.
    fn open_context(&self, id: ContextId) -> Result<Box<dyn ProviderContext>, ProviderError>;
}

/// A single conversation's provider state. Response streams are owned and movable,
/// not borrows of this context.
pub trait ProviderContext: Send {
    /// Streams reasoning independently of the final answer. Startup failures are the
    /// stream's first and only item. When `response_schema` is supplied, transmit it
    /// as a structured-output constraint or return `InvalidRequest`; do not silently
    /// ignore it or replace it with a prompt.
    fn invoke(&mut self, request: ModelRequest) -> ResponseStream;
}

/// Normalized failure classes. The runtime retries on the kind alone; server
/// retry hints ride on the kinds that can carry them and never appear in Display.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderErrorKind {
    Authentication,
    InvalidRequest,
    ContextWindowExceeded,
    Protocol,
    Timeout,
    Transport,
    RateLimited {
        retry_after: Option<Duration>,
    },
    /// A retryable server-side failure: 5xx, overloaded, or an in-stream error.
    Unavailable {
        retry_after: Option<Duration>,
    },
}

impl fmt::Display for ProviderErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Authentication => "Authentication",
            Self::InvalidRequest => "InvalidRequest",
            Self::ContextWindowExceeded => "ContextWindowExceeded",
            Self::Protocol => "Protocol",
            Self::Timeout => "Timeout",
            Self::Transport => "Transport",
            Self::RateLimited { .. } => "RateLimited",
            Self::Unavailable { .. } => "Unavailable",
        })
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[error("{kind}: {message}")]
pub struct ProviderError {
    pub kind: ProviderErrorKind,
    pub message: String,
}

impl ProviderError {
    /// Whether the runtime may retry an *uncommitted* response after this error.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(
            self.kind,
            ProviderErrorKind::RateLimited { .. }
                | ProviderErrorKind::Timeout
                | ProviderErrorKind::Transport
                | ProviderErrorKind::Unavailable { .. }
        )
    }

    /// The server-directed delay for a runtime-owned retry, when one was sent.
    #[must_use]
    pub fn retry_after(&self) -> Option<Duration> {
        match self.kind {
            ProviderErrorKind::RateLimited { retry_after }
            | ProviderErrorKind::Unavailable { retry_after } => retry_after,
            _ => None,
        }
    }

    #[must_use]
    pub fn protocol(message: impl Into<String>) -> Self {
        Self {
            kind: ProviderErrorKind::Protocol,
            message: message.into(),
        }
    }
}
