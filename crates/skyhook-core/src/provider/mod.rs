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
