//! Flux-backed implementations of Skyhook's stable provider contract.

use std::sync::Arc;

use futures_util::StreamExt;

use crate::{
    provider::protocol::ModelRequest,
    provider::{Provider, ProviderContext, ProviderError, ProviderErrorKind, ProviderFuture},
};

mod conversion;
mod credential;
mod schema;

pub use credential::{OpenAiApi, anthropic_api, claude_oauth, codex_oauth, openai_compatible};

use conversion::{convert_chunk, convert_request, map_error};

#[derive(Clone)]
pub struct FluxProvider {
    factory: Arc<dyn Fn() -> FluxBackends + Send + Sync>,
}

struct FluxBackends {
    inner: Arc<dyn flux_provider::Provider>,
    schema_inner: Option<Arc<dyn flux_provider::Provider>>,
}

impl FluxProvider {
    /// Creates a fresh opaque Flux backend for each context. The factory must not
    /// reuse connection state. Use the backend constructors for schema support.
    #[must_use]
    pub fn new<P: flux_provider::Provider + 'static>(
        factory: impl Fn() -> P + Send + Sync + 'static,
    ) -> Self {
        Self {
            factory: Arc::new(move || FluxBackends {
                inner: Arc::new(factory()),
                schema_inner: None,
            }),
        }
    }

    fn with_schema_provider<P: flux_provider::Provider + 'static>(
        factory: impl Fn() -> P + Send + Sync + 'static,
    ) -> Self {
        Self {
            factory: Arc::new(move || {
                let inner: Arc<dyn flux_provider::Provider> = Arc::new(factory());
                FluxBackends {
                    schema_inner: Some(inner.clone()),
                    inner,
                }
            }),
        }
    }
}

impl Provider for FluxProvider {
    fn open_context(&self, correlation: String) -> Result<Box<dyn ProviderContext>, ProviderError> {
        Ok(Box::new(FluxContext {
            correlation,
            backends: (self.factory)(),
        }))
    }
}

struct FluxContext {
    correlation: String,
    backends: FluxBackends,
}

impl ProviderContext for FluxContext {
    fn invoke(&mut self, request: ModelRequest) -> ProviderFuture {
        if request.correlation.as_deref() != Some(self.correlation.as_str()) {
            return Box::pin(async {
                Err(ProviderError {
                    kind: ProviderErrorKind::InvalidRequest,
                    message: "request correlation does not match its provider context".into(),
                })
            });
        }
        let provider = if request.response_schema.is_some() {
            self.backends.schema_inner.clone()
        } else {
            Some(self.backends.inner.clone())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::protocol::{Message, UserContent};
    use flux_provider::WireCodec;
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    struct Recorder {
        id: usize,
        calls: Arc<Mutex<Vec<(usize, String)>>>,
    }

    #[async_trait::async_trait]
    impl flux_provider::Provider for Recorder {
        fn name(&self) -> &str {
            "recorder"
        }
        async fn stream(
            &self,
            request: flux_provider::Request,
        ) -> flux_core::Result<flux_provider::ChunkStream> {
            let body =
                flux_providers::openai::OpenAiResponses { codex: true }.build_body(&request)?;
            self.calls.lock().unwrap().push((
                self.id,
                body["prompt_cache_key"].as_str().unwrap().to_owned(),
            ));
            Ok(Box::pin(futures_util::stream::empty()))
        }
    }

    fn request(correlation: Option<&str>) -> ModelRequest {
        ModelRequest {
            model: "test".into(),
            system: vec![],
            messages: vec![Message::User(vec![UserContent::Text {
                text: "hello".into(),
            }])],
            tools: vec![],
            response_schema: None,
            reasoning: None,
            max_output_tokens: None,
            correlation: correlation.map(str::to_owned),
        }
    }

    #[tokio::test]
    async fn contexts_isolate_backends_and_preserve_distinct_wire_cache_keys() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let next = AtomicUsize::new(0);
        let recorded = calls.clone();
        let factory = FluxProvider::new(move || Recorder {
            id: next.fetch_add(1, Ordering::SeqCst),
            calls: recorded.clone(),
        });
        let mut root = factory.open_context("session:root".into()).unwrap();
        let mut child = factory.clone().open_context("session:1".into()).unwrap();
        for key in ["session:root", "session:1", "session:root"] {
            let context = if key == "session:root" {
                &mut root
            } else {
                &mut child
            };
            context
                .invoke(request(Some(key)))
                .await
                .unwrap()
                .collect::<Vec<_>>()
                .await;
        }
        let calls = calls.lock().unwrap();
        assert_eq!(
            calls.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            [0, 1, 0]
        );
        assert_eq!(calls[0].1, calls[2].1);
        assert_ne!(calls[0].1, calls[1].1);
    }

    #[tokio::test]
    async fn mismatched_or_missing_identity_is_rejected_before_backend_dispatch() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let recorded = calls.clone();
        let factory = FluxProvider::new(move || Recorder {
            id: 0,
            calls: recorded.clone(),
        });
        let mut context = factory.open_context("session:root".into()).unwrap();
        for identity in [None, Some("session:1"), Some("other-session:root")] {
            let error = context.invoke(request(identity)).await.err().unwrap();
            assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
        }
        assert!(calls.lock().unwrap().is_empty());
    }
}
