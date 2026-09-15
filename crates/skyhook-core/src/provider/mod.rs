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
    /// Create independently owned conversation state. Factories may share credentials
    /// and immutable configuration, but must not share connection/continuation slots.
    /// Opening a context does not make a model request.
    fn open_context(&self, correlation: String) -> Result<Box<dyn ProviderContext>, ProviderError>;
}

/// A single conversation's provider state. Startup futures and response streams are
/// owned and movable, not exclusive borrows of this context, and may outlive reset.
/// This API does not statically prohibit overlapping invocations or guarantee FIFO
/// execution of separately polled futures. Callers requiring conversation order
/// consume or drop each response before awaiting the next startup. Stateful Codex
/// contexts serialize the same session through the response stream's owned guard;
/// awaiting a later startup while retaining an unconsumed earlier stream can wait
/// indefinitely. Stateless contexts need not serialize independent invocations.
pub trait ProviderContext: Send {
    /// Streams reasoning independently of the final answer. When `response_schema`
    /// is supplied, transmit it as a structured-output constraint or return
    /// `InvalidRequest`; do not silently ignore it or replace it with a prompt.
    fn invoke(&mut self, request: ModelRequest) -> ProviderFuture;

    /// Retire connection/continuation state for subsequently created invocations.
    /// Outstanding owned futures/streams may still exist: reset must not wait for
    /// their locks or let their completion repopulate replacement state. Reset is
    /// not cancellation; those invocations retain their detached prior state.
    /// Runtime retries normally drop failed work first, and stateful providers
    /// replay the next request in full. Stateless providers need no reset.
    fn reset(&mut self) {}
}

/// Recovery is permission to retry an *uncommitted* local-tool response, not a
/// claim of general request idempotency. The runtime owns the retry policy and
/// must not replay committed responses or externally executed tool effects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderRecovery {
    ResetContext,
}

/// Sanitized categories for transient Codex WebSocket failures. Native errors,
/// close reasons, URLs, credentials, and response content are never retained.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodexWebSocketError {
    EndOfStream,
    Closed,
    Read,
    ReadTimeout,
    Ping,
    Write,
    WriteTimeout,
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
    CodexWebSocket(CodexWebSocketError),
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
    /// Classify retry eligibility independently of the provider adapter. Adapters
    /// choose a sanitized error category and may reset transport state, while the
    /// agent runtime owns the retry policy and cancellation.
    #[must_use]
    pub fn recovery(&self) -> Option<ProviderRecovery> {
        match self.kind {
            ProviderErrorKind::RateLimited
            | ProviderErrorKind::Timeout
            | ProviderErrorKind::Transport
            | ProviderErrorKind::Response
            | ProviderErrorKind::CodexWebSocket(_) => Some(ProviderRecovery::ResetContext),
            ProviderErrorKind::Authentication
            | ProviderErrorKind::Protocol
            | ProviderErrorKind::InvalidRequest
            | ProviderErrorKind::ContextWindowExceeded => None,
        }
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

/// HTTP startup is a per-attempt deadline; read-idle resets per body chunk. HTTP
/// transports make one attempt. The agent runtime applies the provider-independent
/// cancellable retry policy to transient failures.
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
