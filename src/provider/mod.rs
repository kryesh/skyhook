use std::{future::Future, pin::Pin};

use thiserror::Error;

use crate::provider::protocol::{ModelRequest, ResponseChunk};

pub mod backends;
pub mod profile;
pub mod protocol;

/// An owned, movable response; consuming or dropping it releases invocation state.
pub type ResponseStream =
    futures_util::stream::BoxStream<'static, Result<ResponseChunk, ProviderError>>;
/// Owned startup work. Its successful response stream owns the remaining invocation.
pub type ProviderFuture =
    Pin<Box<dyn Future<Output = Result<ResponseStream, ProviderError>> + Send>>;

pub trait Provider: Send + Sync {
    /// Create independently owned conversation state without making a model request.
    fn open_context(&self, correlation: String) -> Result<Box<dyn ProviderContext>, ProviderError>;
}

/// A single conversation's provider state. Startup futures and response streams are
/// owned and movable, not borrows of this context.
pub trait ProviderContext: Send {
    /// Streams reasoning independently of the final answer. When `response_schema`
    /// is supplied, transmit it as a structured-output constraint or return
    /// `InvalidRequest`; do not silently ignore it or replace it with a prompt.
    fn invoke(&mut self, request: ModelRequest) -> ProviderFuture;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderErrorKind {
    Authentication,
    RateLimited,
    Timeout,
    Transport,
    Protocol,
    InvalidRequest,
    ContextWindowExceeded,
    Response,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[error("{kind:?}: {message}")]
pub struct ProviderError {
    pub kind: ProviderErrorKind,
    pub message: String,
    /// Server-directed delay for runtime-owned retries. Never included in Display.
    pub retry_after: Option<std::time::Duration>,
}

impl ProviderError {
    /// Whether the runtime may retry an *uncommitted* response after this error.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(
            self.kind,
            ProviderErrorKind::RateLimited
                | ProviderErrorKind::Timeout
                | ProviderErrorKind::Transport
                | ProviderErrorKind::Response
        )
    }

    #[must_use]
    pub fn protocol(message: impl Into<String>) -> Self {
        Self {
            kind: ProviderErrorKind::Protocol,
            message: message.into(),
            retry_after: None,
        }
    }
}

/// Startup is a per-attempt deadline; read-idle resets per body chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProviderTimeouts {
    pub startup: std::time::Duration,
    pub read_idle: std::time::Duration,
}

impl Default for ProviderTimeouts {
    fn default() -> Self {
        Self {
            startup: std::time::Duration::from_secs(600),
            read_idle: std::time::Duration::from_secs(600),
        }
    }
}
