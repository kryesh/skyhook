//! Flux-backed implementations of Skyhook's stable provider contract.

use std::sync::Arc;

use futures_util::StreamExt;

use crate::{
    provider::protocol::ModelRequest,
    provider::{Provider, ProviderFuture},
};

mod conversion;
mod credential;

pub use credential::{OpenAiApi, openai_compatible};

use conversion::{convert_chunk, convert_request, map_error};

#[derive(Clone)]
pub struct FluxProvider {
    inner: Arc<dyn flux_provider::Provider>,
}

impl FluxProvider {
    #[must_use]
    pub fn new(provider: impl flux_provider::Provider + 'static) -> Self {
        Self {
            inner: Arc::new(provider),
        }
    }
}

impl Provider for FluxProvider {
    fn invoke(&self, request: ModelRequest) -> ProviderFuture {
        let provider = self.inner.clone();
        Box::pin(async move {
            let request = convert_request(request)?;
            let stream = provider
                .stream(request)
                .await
                .map_err(|error| map_error(&error))?;
            let mapped = stream.filter_map(|item| {
                futures_util::future::ready(match item {
                    Ok(chunk) => convert_chunk(chunk).map(Ok),
                    Err(error) => Some(Err(map_error(&error))),
                })
            });
            Ok(Box::pin(mapped) as std::pin::Pin<Box<dyn crate::provider::ResponseHandle>>)
        })
    }
}
