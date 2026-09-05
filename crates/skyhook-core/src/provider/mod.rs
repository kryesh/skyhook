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
    fn invoke(&self, request: ModelRequest) -> ProviderFuture;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderErrorKind {
    Authentication,
    RateLimited,
    Timeout,
    Transport,
    Protocol,
    InvalidRequest,
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
