//! User configuration with explicit-file selection.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use crate::{
    agent::{AgentProfile, Harness, HarnessBuilder},
    provider::backends::flux::OpenAiApi,
    provider::profile::ModelProfile,
    target::TargetsConfig,
};
use serde::Deserialize;
use thiserror::Error;

mod loader;
mod providers;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub default_model_profile: String,
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
    pub models: BTreeMap<String, ModelProfile>,
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
    Openai {
        #[serde(default)]
        api: OpenAiApi,
        #[serde(default = "openai_key_env")]
        api_key_env: String,
    },
    Anthropic {
        #[serde(default = "anthropic_key_env")]
        api_key_env: String,
    },
    Codex,
    Claude,
    OpenaiCompatible {
        base_url: String,
        #[serde(default)]
        api: OpenAiApi,
        api_key_env: Option<String>,
    },
}

fn openai_key_env() -> String {
    "OPENAI_API_KEY".to_owned()
}
fn anthropic_key_env() -> String {
    "ANTHROPIC_API_KEY".to_owned()
}

impl Config {
    /// Loads an explicit config when supplied, otherwise the user-level config.
    pub async fn load(explicit: Option<&Path>) -> Result<Self, ConfigError> {
        loader::load(explicit).await
    }

    pub fn harness_builder(
        &self,
        workspace: impl Into<PathBuf>,
    ) -> Result<HarnessBuilder, ConfigError> {
        let mut capabilities = crate::tool::policy::CapabilitySet::default();
        if self.targets_enabled {
            capabilities.insert(crate::tool::policy::Capability::Targets);
        }
        let mut builder = HarnessBuilder::new(workspace)
            .default_model_profile(self.default_model_profile.clone())
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
            let mut profile = profile.clone();
            let provider = self.providers.get(&profile.provider);
            providers::resolve_model(&mut profile, provider);
            builder = builder.model_profile(name.clone(), profile);
        }
        for (name, profile) in &self.agents {
            builder = builder.agent_profile(name.clone(), profile.clone());
        }
        Ok(builder)
    }

    pub async fn build_harness(
        &self,
        workspace: impl Into<PathBuf>,
    ) -> Result<Harness, ConfigError> {
        self.harness_builder(workspace)?
            .build()
            .await
            .map_err(ConfigError::Harness)
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
            "default_model_profile = 'local'\n\n[models.local]\nprovider = 'local'\nmodel = 'test'\nsupports_images = false\n",
        )
        .await
        .unwrap();
        let config = Config::load(Some(&path)).await.unwrap();
        assert_eq!(config.default_model_profile, "local");
        assert!(!config.approve_all);
        assert!(!config.targets_enabled);
    }

    #[test]
    fn example_config_stays_valid() {
        let config: Config =
            toml::from_str(include_str!("../../../../skyhook.example.toml")).unwrap();
        assert!(config.providers.contains_key("codex"));
        assert!(config.models.contains_key(&config.default_model_profile));
    }

    #[test]
    fn named_targets_live_directly_below_targets() {
        let config: Config = toml::from_str(
            r#"
default_model_profile = "test"

[models.test]
provider = "test"
model = "test"
supports_images = false

[targets]
import_ssh_config = true

[targets.bastion]
host = "bastion.example.com"

[targets.build]
host = "build.internal"
via = "bastion"
workspace = "/srv/project"

[targets.build.auth]
kind = "key"
path = "~/.ssh/build"
"#,
        )
        .unwrap();
        assert!(config.targets.import_ssh_config);
        assert_eq!(
            config.targets.entries["build"].via.as_deref(),
            Some("bastion")
        );
        assert_eq!(config.targets.entries["build"].auth.kind(), "key");
    }

    #[test]
    fn approve_all_is_opt_in() {
        let disabled: Config = toml::from_str(
            "default_model_profile='test'\n[models.test]\nprovider='test'\nmodel='test'\nsupports_images=false\n",
        )
        .unwrap();
        assert!(!disabled.approve_all);
        assert!(!disabled.targets_enabled);

        let enabled: Config = toml::from_str(
            "default_model_profile='test'\napprove_all=true\n[models.test]\nprovider='test'\nmodel='test'\nsupports_images=false\n",
        )
        .unwrap();
        assert!(enabled.approve_all);
    }
}
