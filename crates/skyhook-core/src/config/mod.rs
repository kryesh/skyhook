//! Side-effect-free configuration resolution and provider construction.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use crate::{
    agent::HarnessBuilder,
    mcp::config::McpServerConfig,
    provider::backends::OpenAiApi,
    provider::profile::ModelProfile,
    target::TargetsConfig,
    tool::policy::{Capability, CapabilitySet},
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

mod loader;
mod paths;
mod providers;

pub use loader::{ConfigDiagnostic, ConfigReport, ResolvedConfig};
pub(crate) use paths::user_config_directory;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Session storage override. Relative paths retain their historical meaning:
    /// relative to the process working directory, not the config or target directory.
    /// When absent, the harness uses `<resolved workspace>/.skyhook/sessions`.
    pub session_root: Option<PathBuf>,
    /// Approve all tool calls without consulting an interactive policy.
    #[serde(default)]
    pub approve_all: bool,
    /// Exact policy capabilities. Interaction is supplied separately by the runtime host.
    #[serde(
        default = "default_capabilities",
        deserialize_with = "deserialize_capabilities"
    )]
    pub capabilities: Vec<Capability>,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    #[serde(default)]
    pub models: indexmap::IndexMap<String, ModelProfile>,
    #[serde(default)]
    pub targets: TargetsConfig,
    /// Named, trusted MCP server connections. Empty by default.
    #[serde(default)]
    pub mcp: BTreeMap<String, McpServerConfig>,
    #[serde(default = "default_child_depth")]
    pub max_child_depth: usize,
}

const fn default_child_depth() -> usize {
    4
}

fn default_capabilities() -> Vec<Capability> {
    CapabilitySet::default()
        .iter()
        .filter(|capability| *capability != Capability::Interactive)
        .collect()
}

fn deserialize_capabilities<'de, D>(deserializer: D) -> Result<Vec<Capability>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let capabilities = Vec::<Capability>::deserialize(deserializer)?;
    if capabilities.contains(&Capability::Interactive) {
        return Err(serde::de::Error::custom(
            "interactive is controlled by the runtime host, not the capability allowlist",
        ));
    }
    Ok(capabilities)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderConfig {
    /// Standard OpenAI wire protocols, without endpoint or model presets.
    Openai {
        base_url: String,
        api: OpenAiApi,
        /// Chat-only request field; absence uses the shared reasoning_content default.
        chat_reasoning_replay: Option<crate::provider::backends::ChatReasoningReplay>,
        api_key_env: Option<String>,
        /// Shell command run on first provider request; trimmed stdout is cached as the key.
        /// Mutually exclusive with api_key_env.
        api_key_command: Option<String>,
        /// Time to receive HTTP response headers per attempt (default: 600 seconds).
        startup_timeout_secs: Option<u64>,
        /// Maximum interval between HTTP response body reads (default: 600 seconds).
        read_idle_timeout_secs: Option<u64>,
    },
    Anthropic {
        base_url: String,
        api_key_env: Option<String>,
        /// Shell command run on first provider request; trimmed stdout is cached as the key.
        /// Mutually exclusive with api_key_env.
        api_key_command: Option<String>,
        /// Time to receive HTTP response headers per attempt (default: 600 seconds).
        startup_timeout_secs: Option<u64>,
        /// Maximum interval between HTTP response body reads (default: 600 seconds).
        read_idle_timeout_secs: Option<u64>,
    },
    /// ChatGPT subscription using Skyhook-owned OAuth credentials.
    Codex {},
}

impl Config {
    /// Loads an explicit config, or resolves user and current-workspace layers.
    pub async fn load(explicit: Option<&Path>) -> Result<Self, ConfigError> {
        Self::load_for_workspace(Path::new("."), explicit).await
    }

    /// Resolve without constructing providers, reading credentials, or executing commands.
    /// An explicit file disables all user/workspace configuration discovery.
    pub async fn resolve(
        workspace: &Path,
        explicit: Option<&Path>,
    ) -> Result<ResolvedConfig, ConfigError> {
        loader::resolve(workspace, explicit).await
    }

    pub async fn load_for_workspace(
        workspace: &Path,
        explicit: Option<&Path>,
    ) -> Result<Self, ConfigError> {
        Ok(Self::resolve(workspace, explicit).await?.config)
    }

    /// Serialize effective configuration, including defaults and caller overrides.
    pub fn to_toml(&self) -> Result<String, ConfigError> {
        Ok(toml::to_string_pretty(self)?)
    }

    /// Validate entries without runtime model selection or external resources.
    fn validate_structure(&self) -> Result<(), ConfigError> {
        for (name, profile) in &self.models {
            profile
                .validate_limits()
                .map_err(|message| ConfigError::Model(name.clone(), message.to_owned()))?;
        }
        for (name, server) in &self.mcp {
            server
                .validate()
                .map_err(|message| ConfigError::Mcp(name.clone(), message))?;
        }
        for (name, provider) in &self.providers {
            providers::validate(name, provider)?;
        }
        self.targets
            .validate_structure()
            .map_err(|error| ConfigError::Structure(error.to_string()))?;
        Ok(())
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
        for (name, server) in &self.mcp {
            server
                .validate()
                .map_err(|message| ConfigError::Mcp(name.clone(), message))?;
        }
        let mut builder = HarnessBuilder::new(workspace)
            .default_model_profile(model)
            .max_child_depth(self.max_child_depth)
            .capabilities(self.capabilities.iter().copied().collect())
            .mcp(self.mcp.clone())
            .targets_config(self.targets.clone());
        if let Some(root) = &self.session_root {
            builder = builder.session_root(root.clone());
        }
        for (name, config) in &self.providers {
            builder = builder.provider(name.clone(), providers::build(name, config)?);
        }
        for (name, profile) in &self.models {
            builder = builder.model_profile(name.clone(), profile.clone());
        }
        Ok(builder)
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("{message}{report}")]
    Resolution {
        message: String,
        report: ConfigReport,
    },
    #[error("could not serialize configuration: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("invalid configuration: {0}")]
    Structure(String),
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
    #[error("invalid MCP server `{0}`: {1}")]
    Mcp(String, String),
    #[error(transparent)]
    Harness(#[from] crate::agent::HarnessError),
}

impl ConfigError {
    /// Candidate diagnostics are retained when no effective config is available.
    pub fn report(&self) -> Option<&ConfigReport> {
        match self {
            Self::Resolution { report, .. } => Some(report),
            _ => None,
        }
    }
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
            "[models.local]\nprovider = 'local'\nmodel = 'test'\nmax_context = 128000\nmax_output = 16384\nsupports_images = false\n",
        )
        .await
        .unwrap();
        let config = Config::load(Some(&path)).await.unwrap();
        assert_eq!(config.models.first().unwrap().0, "local");
        assert!(!config.approve_all);
        assert_eq!(config.capabilities, default_capabilities());
        assert!(config.mcp.is_empty());
    }

    #[tokio::test]
    async fn mcp_cwd_is_relative_to_selected_config_directory() {
        let current = std::env::current_dir().unwrap();
        let root = tempfile::tempdir_in(&current).unwrap();
        let path = root.path().join("mcp.toml");
        let absolute = root.path().join("absolute");
        let mut config_value: toml::Value = toml::from_str(
            "[mcp.relative]\ntransport = 'stdio'\nstart_command = ['server']\ncwd = 'work'\n[mcp.absolute]\ntransport = 'stdio'\nstart_command = ['server']\n[mcp.default]\ntransport = 'stdio'\nstart_command = ['server']",
        ).unwrap();
        config_value["mcp"]["absolute"]
            .as_table_mut()
            .unwrap()
            .insert(
                "cwd".to_owned(),
                toml::Value::String(absolute.to_str().unwrap().to_owned()),
            );
        tokio::fs::write(&path, toml::to_string(&config_value).unwrap())
            .await
            .unwrap();
        // A relative --config path must still produce absolute process directories.
        let config = Config::load(Some(path.strip_prefix(&current).unwrap()))
            .await
            .unwrap();
        assert_eq!(config.mcp["relative"].cwd, Some(root.path().join("work")));
        assert_eq!(config.mcp["absolute"].cwd, Some(absolute));
        assert_eq!(config.mcp["default"].cwd, None);
    }

    #[test]
    fn mcp_map_has_exact_name_and_validates_before_provider_credentials() {
        assert!(toml::from_str::<Config>("[mcp]").unwrap().mcp.is_empty());
        assert!(toml::from_str::<Config>("[mcp_servers]").is_err());
        assert!(toml::from_str::<Config>("[mcp.invalid]\ntransport = 'stdio'").is_err());
        let mut config: Config = toml::from_str(
            "[mcp.test]\ntransport = 'stdio'\nstart_command = ['server']\n[providers.test]\nkind = 'anthropic'\nbase_url = 'https://api.anthropic.com/v1'\napi_key_env = 'SKYHOOK_TEST_MISSING_API_KEY'",
        ).unwrap();
        config.mcp.get_mut("test").unwrap().startup_timeout_secs = 0;
        let Err(ConfigError::Mcp(name, message)) = config.harness_builder(".", "test") else {
            panic!("expected MCP validation before credential loading");
        };
        assert_eq!(name, "test");
        assert!(message.contains("startup_timeout_secs"));
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
                "[providers.test]\nkind = 'anthropic'\nbase_url = 'https://api.anthropic.com/v1'\napi_key_env = 'SKYHOOK_TEST_MISSING_API_KEY'\n[models.test]\nprovider = 'test'\nmodel = 'test'\nmax_context = {max_context}\nmax_output = {max_output}\n"
            )).unwrap();
            let Err(ConfigError::Model(name, message)) = config.harness_builder(".", "test") else {
                panic!("expected limit validation before credential loading");
            };
            assert_eq!(name, "test");
            assert!(message.contains(expected), "{message}");
        }
    }

    #[test]
    fn example_config_stays_valid() {
        let config: Config =
            toml::from_str(include_str!("../../../../config.example.toml")).unwrap();
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
    fn capabilities_are_exact_and_validate_names() {
        let defaults: Config = toml::from_str("").unwrap();
        let expected = vec![
            Capability::Read,
            Capability::Write,
            Capability::Exec,
            Capability::Network,
            Capability::Agents,
            Capability::Mcp,
        ];
        assert_eq!(defaults.capabilities, expected);

        let empty: Config = toml::from_str("capabilities = []").unwrap();
        assert!(empty.capabilities.is_empty());
        let exact: Config = toml::from_str("capabilities = ['read', 'targets']").unwrap();
        assert_eq!(exact.capabilities, [Capability::Read, Capability::Targets]);
        for name in ["unknown", "Mcp", "Interactive", "READ"] {
            let error = toml::from_str::<Config>(&format!("capabilities = ['{name}']"))
                .unwrap_err()
                .to_string();
            assert!(error.contains(name), "{error}");
            assert!(error.contains("unknown variant"), "{error}");
        }
        let all: Config = toml::from_str(
            "capabilities = ['read', 'write', 'exec', 'network', 'targets', 'agents', 'mcp']",
        )
        .unwrap();
        assert_eq!(
            all.capabilities,
            Capability::ALL
                .into_iter()
                .filter(|capability| *capability != Capability::Interactive)
                .collect::<Vec<_>>()
        );
        let error = toml::from_str::<Config>("capabilities = ['interactive']").unwrap_err();
        assert!(error.to_string().contains("controlled by the runtime host"));
        let approved: Config = toml::from_str("approve_all = true\ncapabilities = []").unwrap();
        assert!(approved.approve_all);
        assert!(approved.capabilities.is_empty());
        assert!(toml::from_str::<Config>("capabilities = 'read'").is_err());
        assert!(toml::from_str::<Config>("targets_enabled = true").is_err());
    }

    #[test]
    fn approve_all_is_opt_in() {
        let disabled: Config = toml::from_str(
            "[models.test]\nprovider='test'\nmodel='test'\nmax_context=128000\nmax_output=16384\nsupports_images=false\n",
        )
        .unwrap();
        assert!(!disabled.approve_all);
        assert!(!disabled.capabilities.contains(&Capability::Targets));

        let enabled: Config = toml::from_str(
            "approve_all=true\n[models.test]\nprovider='test'\nmodel='test'\nmax_context=128000\nmax_output=16384\nsupports_images=false\n",
        )
        .unwrap();
        assert!(enabled.approve_all);
    }
}
