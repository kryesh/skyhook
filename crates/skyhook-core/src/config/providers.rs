//! Native provider construction. Model names are passed through unchanged.

use std::{env, sync::Arc};

use crate::provider::{
    Provider,
    backends::{anthropic_api, codex::CodexProvider, openai_compatible},
};

use super::{ConfigError, ProviderConfig};

pub(super) fn build(name: &str, config: &ProviderConfig) -> Result<Arc<dyn Provider>, ConfigError> {
    let error = |error: crate::provider::ProviderError| {
        ConfigError::Provider(name.to_owned(), error.to_string())
    };
    match config {
        ProviderConfig::Openai {
            base_url,
            api,
            api_key_env,
        } => {
            let key = api_key_env.as_deref().map(required_env).transpose()?;
            Ok(Arc::new(
                openai_compatible(name, base_url, *api, key).map_err(error)?,
            ))
        }
        ProviderConfig::Anthropic {
            base_url,
            api_key_env,
        } => {
            let key = api_key_env.as_deref().map(required_env).transpose()?;
            Ok(Arc::new(anthropic_api(name, base_url, key).map_err(error)?))
        }
        ProviderConfig::Codex => Ok(Arc::new(CodexProvider::new().map_err(error)?)),
    }
}

fn required_env(name: &str) -> Result<String, ConfigError> {
    env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| ConfigError::MissingEnvironment(name.to_owned()))
}
