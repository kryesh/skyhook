//! User configuration with explicit-file selection.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use crate::{
    agent::{AgentProfile, HarnessBuilder},
    provider::backends::flux::OpenAiApi,
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
        assert_eq!(config.default_model_profile, "local");
        assert!(!config.approve_all);
        assert!(!config.targets_enabled);
    }

    #[test]
    fn every_provider_requires_both_model_limits() {
        for kind in ["openai", "anthropic", "claude", "codex"] {
            for limits in ["", "max_context = 128000\n", "max_output = 16384\n"] {
                let text = format!(
                    "default_model_profile = 'test'\n[providers.test]\nkind = '{kind}'\n[models.test]\nprovider = 'test'\nmodel = 'test'\n{limits}"
                );
                assert!(toml::from_str::<Config>(&text).is_err(), "{text}");
            }
        }
    }

    #[test]
    fn invalid_limits_are_rejected_before_credentials_are_loaded() {
        for (max_context, max_output, expected) in [
            (0, 1, "max_context must be positive"),
            (128000, 0, "max_output must be positive"),
            (128000, 128000, "max_output must be smaller"),
            (128000, 128001, "max_output must be smaller"),
            (
                u64::from(u32::MAX) + 2,
                u64::from(u32::MAX) + 1,
                "provider u32",
            ),
        ] {
            let config: Config = toml::from_str(&format!(
                "default_model_profile = 'test'\n[providers.test]\nkind = 'anthropic'\n[models.test]\nprovider = 'test'\nmodel = 'test'\nmax_context = {max_context}\nmax_output = {max_output}\n"
            )).unwrap();
            let Err(ConfigError::Model(name, message)) = config.harness_builder(".") else {
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
max_context = 128000
max_output = 16384
supports_images = false

[targets]
import_ssh_config = true

[targets.bastion]
type = "ssh"
host = "bastion.example.com"

[targets.build]
type = "ssh"
host = "build.internal"
via = "bastion"
workspace = "/srv/project"

[targets.build.ssh.auth]
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
        assert_eq!(config.targets.entries["build"].ssh.auth.kind(), "key");
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
