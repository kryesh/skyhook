//! Flux-backed implementations of Skyhook's stable provider contract.

use std::sync::Arc;

use futures_util::StreamExt;

use crate::{
    provider::protocol::ModelRequest,
    provider::{Provider, ProviderFuture},
};

mod conversion;
mod credential;
mod schema;

pub use credential::{OpenAiApi, anthropic_api, claude_oauth, codex_oauth, openai_compatible};

use conversion::{convert_chunk, convert_request, map_error};

#[derive(Clone)]
pub struct FluxProvider {
    inner: Arc<dyn flux_provider::Provider>,
    schema_inner: Option<Arc<dyn flux_provider::Provider>>,
}

impl FluxProvider {
    /// Wraps an opaque Flux provider. Use the backend constructors in this module
    /// for schema support; opaque providers reject requests with response schemas.
    #[must_use]
    pub fn new(provider: impl flux_provider::Provider + 'static) -> Self {
        Self {
            inner: Arc::new(provider),
            schema_inner: None,
        }
    }

    fn with_schema_provider(provider: impl flux_provider::Provider + 'static) -> Self {
        let inner: Arc<dyn flux_provider::Provider> = Arc::new(provider);
        Self {
            schema_inner: Some(inner.clone()),
            inner,
        }
    }
}

impl Provider for FluxProvider {
    fn invoke(&self, request: ModelRequest) -> ProviderFuture {
        let provider = if request.response_schema.is_some() {
            self.schema_inner.clone()
        } else {
            Some(self.inner.clone())
        };
        Box::pin(async move {
            let provider = provider.ok_or_else(|| crate::provider::ProviderError {
                kind: crate::provider::ProviderErrorKind::InvalidRequest,
                message: "this Flux provider does not support response schemas".to_owned(),
            })?;
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
            Ok(Box::pin(mapped) as crate::provider::ResponseStream)
        })
    }
}
