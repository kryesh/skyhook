use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use thiserror::Error;

use crate::provider::protocol::{ModelRequest, ResponseChunk};

pub mod backends;
pub mod profile;
pub mod protocol;

pub type ProviderFuture = Pin<
    Box<dyn Future<Output = Result<Pin<Box<dyn ResponseHandle>>, ProviderError>> + Send + 'static>,
>;

pub trait Provider: Send + Sync {
    fn invoke(&self, request: ModelRequest) -> ProviderFuture;
}

pub trait ResponseHandle: Send {
    fn poll_chunk(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<ResponseChunk, ProviderError>>>;
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

impl<T> ResponseHandle for T
where
    T: futures_util::Stream<Item = Result<ResponseChunk, ProviderError>> + Send + Unpin,
{
    fn poll_chunk(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<ResponseChunk, ProviderError>>> {
        futures_util::Stream::poll_next(self, context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_object_safe(_: &dyn Provider, _: Pin<Box<dyn ResponseHandle>>) {}

    #[allow(dead_code)]
    fn object_safety(provider: &dyn Provider, response: Pin<Box<dyn ResponseHandle>>) {
        assert_object_safe(provider, response);
    }
}
