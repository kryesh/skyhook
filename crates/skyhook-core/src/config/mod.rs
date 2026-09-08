//! User configuration with explicit-file selection.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use crate::{
    agent::{AgentProfile, HarnessBuilder},
    provider::backends::OpenAiApi,
    provider::profile::ModelProfile,
    target::TargetsConfig,
};
use serde::Deserialize;
use thiserror::Error;

mod loader;
mod paths;
mod providers;

pub(crate) use paths::user_config_directory;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    // Accepted only for compatibility; model selection belongs to the host.
    #[serde(default, rename = "default_model_profile")]
    _legacy_model: Option<serde::de::IgnoredAny>,
    pub default_agent_profile: Option<String>,
    pub session_root: Option<PathBuf>,
    /// Approve all tool calls without consulting an interactive policy.
    #[serde(default)]
    pub approve_all: bool,
    #[serde(default)]
    pub targets_enabled: bool,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    #[serde(default)]
    pub models: indexmap::IndexMap<String, ModelProfile>,
    #[serde(default)]
    pub agents: BTreeMap<String, AgentProfile>,
    #[serde(default)]
    pub targets: TargetsConfig,
    #[serde(default = "default_child_depth")]
    pub max_child_depth: usize,
}

const fn default_child_depth() -> usize {
    4
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderConfig {
    /// Standard OpenAI wire protocols, without endpoint or model presets.
    Openai {
        base_url: String,
        api: OpenAiApi,
        api_key_env: Option<String>,
    },
    Anthropic {
        base_url: String,
        api_key_env: Option<String>,
    },
    /// ChatGPT subscription using Skyhook-owned OAuth credentials.
    Codex,
}

impl Config {
    /// Loads an explicit config when supplied, otherwise the user-level config.
    pub async fn load(explicit: Option<&Path>) -> Result<Self, ConfigError> {
        loader::load(explicit).await
    }

    pub fn harness_builder(
        &self,
        workspace: impl Into<PathBuf>,
        model: &str,
    ) -> Result<HarnessBuilder, ConfigError> {
        for (name, profile) in &self.models {
            profile
                .validate_limits()
                .map_err(|message| ConfigError::Model(name.clone(), message.to_owned()))?;
        }
        let mut capabilities = crate::tool::policy::CapabilitySet::default();
        if self.targets_enabled {
            capabilities.insert(crate::tool::policy::Capability::Targets);
        }
        let mut builder = HarnessBuilder::new(workspace)
            .default_model_profile(model)
            .max_child_depth(self.max_child_depth)
            .capabilities(capabilities)
            .targets_config(self.targets.clone());
        if let Some(root) = &self.session_root {
            builder = builder.session_root(root.clone());
        }
        if let Some(profile) = &self.default_agent_profile {
            builder = builder.default_agent_profile(profile.clone());
        }
        for (name, config) in &self.providers {
            builder = builder.provider(name.clone(), providers::build(name, config)?);
        }
        for (name, profile) in &self.models {
            builder = builder.model_profile(name.clone(), profile.clone());
        }
        for (name, profile) in &self.agents {
            builder = builder.agent_profile(name.clone(), profile.clone());
        }
        Ok(builder)
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("no Skyhook config found; pass --config or create ~/.config/skyhook/config.toml")]
    Missing,
    #[error("configuration I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid TOML configuration: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("environment variable `{0}` is required and must not be empty")]
    MissingEnvironment(String),
    #[error("provider `{0}` could not be initialized: {1}")]
    Provider(String, String),
    #[error("invalid model profile `{0}`: {1}")]
    Model(String, String),
    #[error(transparent)]
    Harness(#[from] crate::agent::HarnessError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn explicit_config_is_authoritative() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("explicit.toml");
        tokio::fs::write(
            &path,
            "default_model_profile = 'local'\n\n[models.local]\nprovider = 'local'\nmodel = 'test'\nmax_context = 128000\nmax_output = 16384\nsupports_images = false\n",
        )
        .await
        .unwrap();
        let config = Config::load(Some(&path)).await.unwrap();
        assert_eq!(config.models.first().unwrap().0, "local");
        assert!(!config.approve_all);
        assert!(!config.targets_enabled);
    }

    #[test]
    fn model_profiles_require_both_limits() {
        for limits in ["", "max_context = 128000\n", "max_output = 16384\n"] {
            let text = format!("[models.test]\nprovider = 'test'\nmodel = 'test'\n{limits}");
            assert!(toml::from_str::<Config>(&text).is_err(), "{text}");
        }
    }

    #[test]
    fn invalid_limits_are_rejected_before_credentials_are_loaded() {
        for (max_context, max_output, expected) in [
            (0, 1, "max_context must be positive"),
            (128000, 0, "max_output must be positive"),
            (128000, 128000, "max_output must be smaller"),
            (128000, 128001, "max_output must be smaller"),
        ] {
            let config: Config = toml::from_str(&format!(
                "default_model_profile = 'test'\n[providers.test]\nkind = 'anthropic'\nbase_url = 'https://api.anthropic.com/v1'\napi_key_env = 'SKYHOOK_TEST_MISSING_API_KEY'\n[models.test]\nprovider = 'test'\nmodel = 'test'\nmax_context = {max_context}\nmax_output = {max_output}\n"
            )).unwrap();
            let Err(ConfigError::Model(name, message)) = config.harness_builder(".", "test") else {
                panic!("expected limit validation before credential loading");
            };
            assert_eq!(name, "test");
            assert!(message.contains(expected), "{message}");
        }
    }

    #[test]
    fn removed_model_limit_name_is_rejected() {
        let text = "default_model_profile = 'test'\n[models.test]\nprovider = 'test'\nmodel = 'test'\nmax_context = 128000\nmax_output = 16384\nmax_output_tokens = 16384\n";
        assert!(
            toml::from_str::<Config>(text)
                .unwrap_err()
                .to_string()
                .contains("max_output_tokens")
        );
    }

    #[test]
    fn example_config_stays_valid() {
        let config: Config =
            toml::from_str(include_str!("../../../../skyhook.example.toml")).unwrap();
        assert!(config.providers.contains_key("codex"));
        assert!(!config.models.is_empty());
    }

    #[test]
    fn native_protocols_require_explicit_configuration_without_presets() {
        for text in [
            "kind = 'openai'\napi = 'responses'",
            "kind = 'openai'\nbase_url = 'https://example.com/v1'",
            "kind = 'anthropic'",
            "kind = 'claude'",
            "kind = 'openai_compatible'\nbase_url = 'https://example.com/v1'\napi = 'responses'",
        ] {
            assert!(toml::from_str::<ProviderConfig>(text).is_err(), "{text}");
        }
        for text in [
            "kind = 'openai'\nbase_url = 'https://example.com/custom/v1'\napi = 'responses'",
            "kind = 'openai'\nbase_url = 'http://localhost:8080/v1'\napi = 'chat_completions'",
            "kind = 'anthropic'\nbase_url = 'https://example.com/v1'",
            "kind = 'codex'",
        ] {
            assert!(toml::from_str::<ProviderConfig>(text).is_ok(), "{text}");
        }
    }

    #[test]
    fn native_configuration_passes_model_identifiers_through() {
        let config: Config = toml::from_str(
            "[providers.local]\nkind = 'openai'\nbase_url = 'http://localhost:8080/v1'\napi = 'responses'\n\
             [models.local]\nprovider = 'local'\nmodel = 'exact-model-id'\nmax_context = 128000\nmax_output = 16384\n",
        ).unwrap();
        assert_eq!(config.models["local"].model, "exact-model-id");
        // Provider construction must not connect to an endpoint or require a key.
        assert!(config.harness_builder(".", "local").is_ok());
    }

    #[test]
    fn approve_all_is_opt_in() {
        let disabled: Config = toml::from_str(
            "default_model_profile='test'\n[models.test]\nprovider='test'\nmodel='test'\nmax_context=128000\nmax_output=16384\nsupports_images=false\n",
        )
        .unwrap();
        assert!(!disabled.approve_all);
        assert!(!disabled.targets_enabled);

        let enabled: Config = toml::from_str(
            "default_model_profile='test'\napprove_all=true\n[models.test]\nprovider='test'\nmodel='test'\nmax_context=128000\nmax_output=16384\nsupports_images=false\n",
        )
        .unwrap();
        assert!(enabled.approve_all);
    }
}
