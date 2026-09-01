//! Provider construction and provider-specific model alias resolution.

use std::{env, sync::Arc};

use crate::{
    provider::profile::ModelProfile,
    provider::{
        Provider,
        backends::flux::{FluxProvider, openai_compatible},
    },
};

use super::{ConfigError, ProviderConfig};

pub(super) fn build(name: &str, config: &ProviderConfig) -> Result<Arc<dyn Provider>, ConfigError> {
    let provider = match config {
        ProviderConfig::Openai { api, api_key_env } => {
            let key = required_env(api_key_env)?;
            openai_compatible(name, "https://api.openai.com", *api, Some(key))
        }
        ProviderConfig::Anthropic { api_key_env } => FluxProvider::new(
            flux_providers::anthropic::anthropic_api(required_env(api_key_env)?),
        ),
        ProviderConfig::Codex => {
            let tokens = flux_credentials::codex_token_source()
                .map_err(|error| ConfigError::Provider(name.to_owned(), error.to_string()))?;
            FluxProvider::new(flux_providers::codex::oauth(tokens))
        }
        ProviderConfig::Claude => {
            let tokens = flux_credentials::claude_token_source()
                .map_err(|error| ConfigError::Provider(name.to_owned(), error.to_string()))?;
            FluxProvider::new(flux_providers::anthropic::claude_oauth(tokens))
        }
        ProviderConfig::OpenaiCompatible {
            base_url,
            api,
            api_key_env,
        } => {
            let key = api_key_env.as_deref().map(required_env).transpose()?;
            openai_compatible(name, base_url, *api, key)
        }
    };
    Ok(Arc::new(provider))
}

pub(super) fn resolve_model(profile: &mut ModelProfile, provider: Option<&ProviderConfig>) {
    match provider {
        Some(ProviderConfig::Anthropic { .. } | ProviderConfig::Claude) => {
            profile.model = flux_providers::anthropic::resolve_model(&profile.model);
        }
        Some(ProviderConfig::Codex) => {
            profile.model = flux_providers::codex::resolve_model(&profile.model);
        }
        _ => {}
    }
}

fn required_env(name: &str) -> Result<String, ConfigError> {
    env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| ConfigError::MissingEnvironment(name.to_owned()))
}
