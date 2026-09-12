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

    /// Retire connection/continuation state before a runtime-owned retry. The
    /// previous invocation and stream must have been dropped. Stateless providers
    /// need no reset; stateful providers must replay the next request in full.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_eligibility_is_category_based_and_provider_independent() {
        for (kind, eligible) in [
            (ProviderErrorKind::Response, true),
            (ProviderErrorKind::Transport, true),
            (ProviderErrorKind::Timeout, true),
            (ProviderErrorKind::RateLimited, true),
            (
                ProviderErrorKind::CodexWebSocket(CodexWebSocketError::Read),
                true,
            ),
            (ProviderErrorKind::Authentication, false),
            (ProviderErrorKind::InvalidRequest, false),
            (ProviderErrorKind::Protocol, false),
            (ProviderErrorKind::ContextWindowExceeded, false),
        ] {
            for message in [
                "provider error",
                "retry reconnect timeout previous_response_not_found",
            ] {
                assert_eq!(
                    ProviderError {
                        retry_after: None,
                        kind,
                        message: message.into()
                    }
                    .recovery()
                    .is_some(),
                    eligible,
                    "{kind:?}"
                );
            }
        }
    }
}
