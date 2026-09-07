use std::sync::Arc;

use async_trait::async_trait;
use flux_provider::{Credential, NativeProvider, TokenSource};
use flux_providers::anthropic::{AnthropicMessages, ApiKeyAnthropic, OAuthAnthropic};
use flux_providers::openai::{OpenAiChat, OpenAiResponses};

use super::schema::{SchemaCodec, SchemaFormat};
use super::{FluxBackends, FluxProvider};

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
    let name = name.into();
    let endpoint = endpoint(base_url.into(), api);
    let codec: Arc<dyn flux_provider::WireCodec> = match api {
        OpenAiApi::ChatCompletions => Arc::new(SchemaCodec {
            inner: OpenAiChat,
            format: SchemaFormat::Chat,
        }),
        OpenAiApi::Responses => Arc::new(SchemaCodec {
            inner: OpenAiResponses { codex: false },
            format: SchemaFormat::Responses,
        }),
    };
    FluxProvider::with_schema_provider(move || {
        NativeProvider::new(
            name.clone(),
            codec.clone(),
            Arc::new(HttpCredential {
                endpoint: endpoint.clone(),
                api_key: api_key.clone(),
            }),
        )
    })
}

#[must_use]
pub fn anthropic_api(api_key: String) -> FluxProvider {
    anthropic(
        "anthropic",
        ApiKeyAnthropic {
            api_key,
            base_url: "https://api.anthropic.com".to_owned(),
        },
    )
}

#[must_use]
pub fn claude_oauth(tokens: Arc<dyn TokenSource>) -> FluxProvider {
    anthropic(
        "claude",
        OAuthAnthropic {
            tokens,
            base_url: "https://api.anthropic.com".to_owned(),
        },
    )
}

fn anthropic(name: &str, credential: impl Credential + 'static) -> FluxProvider {
    let name = name.to_owned();
    let credential = Arc::new(credential);
    FluxProvider::with_schema_provider(move || {
        NativeProvider::new(
            name.clone(),
            Arc::new(SchemaCodec {
                inner: AnthropicMessages::direct(),
                format: SchemaFormat::Anthropic,
            }),
            credential.clone(),
        )
    })
}

/// Ordinary requests retain Flux's session-scoped WebSocket transport. Schema
/// requests use HTTP because Flux's Codex constructor does not expose its codec.
#[must_use]
pub fn codex_oauth(tokens: Arc<dyn TokenSource>) -> FluxProvider {
    FluxProvider {
        factory: Arc::new(move || FluxBackends {
            inner: Arc::new(flux_providers::codex::oauth(tokens.clone())),
            schema_inner: Some(Arc::new(NativeProvider::new(
                "codex",
                Arc::new(SchemaCodec {
                    inner: OpenAiResponses { codex: true },
                    format: SchemaFormat::Responses,
                }),
                Arc::new(CodexSchemaCredential {
                    tokens: tokens.clone(),
                    turn_state: std::sync::Mutex::new(None),
                }),
            ))),
        }),
    }
}

struct CodexSchemaCredential {
    tokens: Arc<dyn TokenSource>,
    turn_state: std::sync::Mutex<Option<String>>,
}

#[async_trait]
impl Credential for CodexSchemaCredential {
    fn endpoint(&self) -> String {
        "https://chatgpt.com/backend-api/codex/responses".to_owned()
    }

    async fn apply(
        &self,
        request: reqwest::RequestBuilder,
    ) -> flux_core::Result<reqwest::RequestBuilder> {
        let token = self.tokens.access_token().await?;
        let account = self.tokens.account_id().ok_or_else(|| {
            flux_core::Error::Auth(
                "codex: no ChatGPT account id; log in with the Codex CLI".to_owned(),
            )
        })?;
        let mut request = request
            .header("authorization", format!("Bearer {token}"))
            .header("chatgpt-account-id", account)
            .header("OpenAI-Beta", "responses=experimental")
            .header("originator", "codex_cli_rs");
        if let Some(state) = self.turn_state.lock().expect("turn-state lock").as_ref() {
            request = request.header("x-codex-turn-state", state);
        }
        Ok(request)
    }

    fn token_source(&self) -> Option<Arc<dyn TokenSource>> {
        Some(self.tokens.clone())
    }

    fn observe_response_headers(&self, headers: &reqwest::header::HeaderMap) {
        if let Some(state) = headers
            .get("x-codex-turn-state")
            .and_then(|value| value.to_str().ok())
        {
            *self.turn_state.lock().expect("turn-state lock") = Some(state.to_owned());
        }
    }

    fn is_terminal_http_error(&self, status: u16, body: &str) -> bool {
        if status != 429 {
            return false;
        }
        let lower = body.to_ascii_lowercase();
        let compact: String = lower.chars().filter(char::is_ascii_alphanumeric).collect();
        compact.contains("usagelimitexceeded")
            || compact.contains("usagelimitreached")
            || lower.contains("usage limit reached")
            || lower.contains("usage limit exceeded")
            || lower.contains("purchase more credits")
            || (["\"reset_at\"", "\"resets_at\"", "\"reset_time\""]
                .iter()
                .any(|marker| lower.contains(marker))
                && (lower.contains("usage") || lower.contains("quota")))
    }
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

    struct TestTokens;

    #[async_trait]
    impl TokenSource for TestTokens {
        async fn access_token(&self) -> flux_core::Result<String> {
            Ok("test-token".to_owned())
        }
        fn account_id(&self) -> Option<String> {
            Some("test-account".to_owned())
        }
    }

    #[tokio::test]
    async fn codex_schema_http_keeps_auth_refresh_and_session_affinity() {
        let tokens: Arc<dyn TokenSource> = Arc::new(TestTokens);
        let credential = CodexSchemaCredential {
            tokens: tokens.clone(),
            turn_state: std::sync::Mutex::new(None),
        };
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-codex-turn-state", "test-state".parse().unwrap());
        credential.observe_response_headers(&headers);
        let request = credential
            .apply(reqwest::Client::new().post(credential.endpoint()))
            .await
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(request.headers()["authorization"], "Bearer test-token");
        assert_eq!(request.headers()["chatgpt-account-id"], "test-account");
        assert_eq!(request.headers()["OpenAI-Beta"], "responses=experimental");
        assert_eq!(request.headers()["originator"], "codex_cli_rs");
        assert_eq!(request.headers()["x-codex-turn-state"], "test-state");
        assert!(Arc::ptr_eq(&credential.token_source().unwrap(), &tokens));
        assert!(credential.is_terminal_http_error(429, r#"{"type":"usageLimitExceeded"}"#));
        assert!(!credential.is_terminal_http_error(429, "rate limited"));
    }

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
