use crate::provider::protocol::{ContextId, ModelRequest, ResponseEvent};

pub mod codec;
pub mod dialect;
mod error;
pub mod http;
pub mod profile;
pub mod protocol;
pub(crate) mod settings;

pub use error::{ProviderError, ProviderErrorKind};

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
