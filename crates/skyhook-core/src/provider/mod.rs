use std::{future::Future, pin::Pin};

use thiserror::Error;

use crate::provider::protocol::{ModelRequest, ResponseChunk};

pub mod backends;
pub mod profile;
pub mod protocol;

pub type ResponseStream =
    futures_util::stream::BoxStream<'static, Result<ResponseChunk, ProviderError>>;
pub type ProviderFuture =
    Pin<Box<dyn Future<Output = Result<ResponseStream, ProviderError>> + Send>>;

pub trait Provider: Send + Sync {
    /// Create independently owned conversation state. Factories may share credentials
    /// and immutable configuration, but must not share connection/continuation slots.
    /// Opening a context does not make a model request.
    fn open_context(&self, correlation: String) -> Result<Box<dyn ProviderContext>, ProviderError>;
}

/// A single conversation's provider state. The owner consumes or drops each response
/// before invoking again, and releases the context when the conversation ends.
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
}

impl ProviderError {
    #[must_use]
    pub fn protocol(message: impl Into<String>) -> Self {
        Self {
            kind: ProviderErrorKind::Protocol,
            message: message.into(),
        }
    }
}

/// HTTP startup is a per-attempt deadline; read-idle resets per body chunk.
/// Native HTTP requests have at most three attempts, so startup can consume three
/// times this duration plus bounded retry backoff. Codex HTTP remains single-attempt.
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
