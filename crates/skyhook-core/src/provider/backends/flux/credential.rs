use std::sync::Arc;

use async_trait::async_trait;
use flux_provider::{Credential, NativeProvider};
use flux_providers::openai::{OpenAiChat, OpenAiResponses};

use super::FluxProvider;

#[derive(Clone, Copy, Debug, Default, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OpenAiApi {
    #[default]
    ChatCompletions,
    Responses,
}

/// Builds an OpenAI-compatible provider with an explicit endpoint and optional bearer token.
#[must_use]
pub fn openai_compatible(
    name: impl Into<String>,
    base_url: impl Into<String>,
    api: OpenAiApi,
    api_key: Option<String>,
) -> FluxProvider {
    let endpoint = endpoint(base_url.into(), api);
    let codec: Arc<dyn flux_provider::WireCodec> = match api {
        OpenAiApi::ChatCompletions => Arc::new(OpenAiChat),
        OpenAiApi::Responses => Arc::new(OpenAiResponses { codex: false }),
    };
    FluxProvider::new(NativeProvider::new(
        name,
        codec,
        Arc::new(HttpCredential { endpoint, api_key }),
    ))
}

fn endpoint(base_url: String, api: OpenAiApi) -> String {
    let suffix = match api {
        OpenAiApi::ChatCompletions => "/v1/chat/completions",
        OpenAiApi::Responses => "/v1/responses",
    };
    if base_url.ends_with(suffix) {
        base_url
    } else {
        let base_url = base_url.trim_end_matches('/');
        if base_url.ends_with("/v1") {
            format!("{base_url}{}", suffix.trim_start_matches("/v1"))
        } else {
            format!("{base_url}{suffix}")
        }
    }
}

struct HttpCredential {
    endpoint: String,
    api_key: Option<String>,
}

#[async_trait]
impl Credential for HttpCredential {
    fn endpoint(&self) -> String {
        self.endpoint.clone()
    }

    async fn apply(
        &self,
        request: reqwest::RequestBuilder,
    ) -> flux_core::Result<reqwest::RequestBuilder> {
        match &self.api_key {
            Some(key) => Ok(request.header("authorization", format!("Bearer {key}"))),
            None => Ok(request),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compatible_base_urls_accept_host_version_or_full_endpoint() {
        let cases = [
            (
                "http://localhost:8000",
                "http://localhost:8000/v1/chat/completions",
            ),
            (
                "http://localhost:8000/v1",
                "http://localhost:8000/v1/chat/completions",
            ),
            (
                "http://localhost:8000/v1/",
                "http://localhost:8000/v1/chat/completions",
            ),
            (
                "http://localhost:8000/v1/chat/completions",
                "http://localhost:8000/v1/chat/completions",
            ),
        ];
        for (base, expected) in cases {
            assert_eq!(
                endpoint(base.to_owned(), OpenAiApi::ChatCompletions),
                expected
            );
        }
        assert_eq!(
            endpoint("http://localhost:8000/v1".to_owned(), OpenAiApi::Responses),
            "http://localhost:8000/v1/responses"
        );
    }
}
