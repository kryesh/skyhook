//! Side-effect-free configuration resolution and provider construction.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use crate::{
    mcp::config::McpServerConfig,
    provider::profile::{ModelRef, ProviderName},
    target::TargetsConfig,
    tool::policy::{Capability, CapabilitySet, Mode},
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

mod loader;
mod paths;
mod providers;
mod runtime;

pub use providers::{EntryError, RawProviderConfig};
pub use runtime::{ConfiguredModel, RuntimeConfig, SelectionError};

pub use loader::{ConfigDiagnostic, ConfigReport, ResolvedConfig};
pub use paths::workspace_session_root;
pub(crate) use paths::{user_config_directories, user_config_directory};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Session storage override. Relative paths retain their historical meaning:
    /// relative to the process working directory, not the config or target directory.
    /// When absent, the harness uses `<resolved workspace>/.skyhook/sessions`.
    /// The CLI always uses that workspace-local directory, ignoring this override.
    pub session_root: Option<PathBuf>,
    /// Approve all tool calls without consulting an interactive policy.
    #[serde(default)]
    pub approve_all: bool,
    /// The mode a new session starts in: `general` or a declared mode.
    #[serde(default = "default_mode")]
    pub default_mode: String,
    /// Named permission presets in declaration order, after the built-in `general`
    /// unless that name is declared.
    #[serde(default = "default_modes", deserialize_with = "deserialize_modes")]
    pub modes: indexmap::IndexMap<String, Mode>,
    /// Providers in declaration order, each with the models served through it.
    #[serde(default)]
    pub providers: indexmap::IndexMap<ProviderName, RawProviderConfig>,
    /// The model a new session starts with, as `provider/model`; without it, the first
    /// model of the first provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<ModelRef>,
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

fn default_mode() -> String {
    "general".to_owned()
}

fn deserialize_modes<'de, D>(deserializer: D) -> Result<indexmap::IndexMap<String, Mode>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let mut modes = default_modes();
    modes.extend(indexmap::IndexMap::<String, Mode>::deserialize(
        deserializer,
    )?);
    Ok(modes)
}

fn default_modes() -> indexmap::IndexMap<String, Mode> {
    let capabilities = CapabilitySet::default()
        .iter()
        .filter(|capability| *capability != Capability::Interactive)
        .collect();
    let general = Mode {
        capabilities,
        instructions: None,
        hint: None,
    };
    [(default_mode(), general)].into()
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

    /// Every capability some mode grants: the most a session can switch to.
    pub fn ceiling(&self) -> CapabilitySet {
        let modes = self.modes.values();
        modes
            .flat_map(|mode| mode.capabilities.iter().copied())
            .collect()
    }

    /// Serialize effective configuration, including defaults and caller overrides.
    pub fn to_yaml(&self) -> Result<String, ConfigError> {
        crate::yaml::to_string(self).map_err(ConfigError::Serialize)
    }

    /// Parse and admit a single YAML document, as a loaded layer is, without
    /// discovery or merging.
    pub fn from_yaml(text: &str) -> Result<Self, ConfigError> {
        let value = crate::yaml::from_str(text)
            .map_err(|error| ConfigError::Structure(format!("invalid YAML: {error}")))?;
        Self::from_value(&value)
    }

    /// Typed extraction naming the offending field, so a rejected value reads
    /// `` `providers.local.models.main.max_context`: ... ``, then admission.
    fn from_value(value: &serde_json::Value) -> Result<Self, ConfigError> {
        let config: Self = serde_path_to_error::deserialize(value).map_err(|error| {
            let path = error.path().to_string();
            let inner = error.into_inner();
            ConfigError::Structure(if path == "." {
                inner.to_string()
            } else {
                format!("`{path}`: {inner}")
            })
        })?;
        config.admit()?;
        Ok(config)
    }

    /// Admit targets, mode text and every provider entry, without model
    /// selection or external resources. Loading checks each layer and the
    /// merged result with this; sealing keeps the admitted entries.
    fn admit(
        &self,
    ) -> Result<indexmap::IndexMap<ProviderName, providers::ProviderConfig>, ConfigError> {
        self.targets.validate_structure()?;
        for (name, mode) in &self.modes {
            for (field, text) in [("instructions", &mode.instructions), ("hint", &mode.hint)] {
                if text.as_deref().is_some_and(|text| text.trim().is_empty()) {
                    return Err(ConfigError::Mode(
                        name.clone(),
                        format!("{field} must not be empty"),
                    ));
                }
            }
        }
        providers::admit(&self.providers)
    }

    /// The OAuth issuer every codex entry shares, as admission proves; OpenAI's
    /// when no entry names one.
    pub fn codex_issuer(
        &self,
    ) -> Result<crate::provider::dialect::codex::auth::Issuer, ConfigError> {
        let providers = self.admit()?;
        Ok(providers
            .values()
            .find_map(providers::ProviderConfig::codex_issuer)
            .unwrap_or_default())
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
    Serialize(String),
    #[error("invalid configuration: {0}")]
    Structure(String),
    #[error("invalid configuration: {0}")]
    Admission(#[from] EntryError),
    #[error("invalid configuration: {0}")]
    Targets(#[from] crate::target::TargetError),
    #[error("no Skyhook config found; pass --config or create ~/.config/skyhook/config.yaml{0}")]
    Missing(ConfigReport),
    #[error(
        "invalid configuration: codex providers `{first}` and `{second}` name different auth_url \
         issuers, but Skyhook keeps one Codex login; give them the same auth_url"
    )]
    CodexIssuers {
        first: ProviderName,
        second: ProviderName,
    },
    #[error("provider `{provider}` could not be initialized: {error}")]
    Provider {
        provider: ProviderName,
        error: crate::provider::dialect::BuildError,
    },
    #[error("No models configured. Add a named entry under a provider's models in your config.")]
    NoModels,
    #[error("invalid model `{name}`: {error}")]
    Model {
        name: ModelRef,
        error: SelectionError,
    },
    #[error("invalid default_model `{model}`: {error}")]
    DefaultModel {
        model: ModelRef,
        error: SelectionError,
    },
    #[error("invalid mode `{0}`: {1}")]
    Mode(String, String),
}

impl ConfigError {
    /// Candidate diagnostics are retained when no effective config is available.
    pub fn report(&self) -> Option<&ConfigReport> {
        match self {
            Self::Resolution { report, .. } | Self::Missing(report) => Some(report),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANTHROPIC_MISSING_KEY: &str = r#"
providers:
  test:
    dialect: anthropic
    codec: messages
    base_url: https://api.anthropic.com/v1
    api_key: {env: SKYHOOK_TEST_MISSING_API_KEY}
"#;

    fn parse(text: &str) -> Result<Config, ConfigError> {
        Config::from_yaml(text)
    }

    #[tokio::test]
    async fn explicit_config_is_authoritative() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("explicit.yaml");
        let text = r#"
providers:
  local:
    dialect: codex
    codec: responses
    models:
      local:
        model: test
        max_context: 128000
        max_output: 16384
        supports_images: false
"#;
        tokio::fs::write(&path, text).await.unwrap();
        let config = Config::load(Some(&path)).await.unwrap();
        assert!(
            config.providers["local"]
                .common
                .models
                .contains_key("local")
        );
        assert!(!config.approve_all);
        assert_eq!(config.modes, default_modes());
        assert!(config.mcp.is_empty());
    }

    #[tokio::test]
    async fn mcp_cwd_is_relative_to_selected_config_directory() {
        let current = std::env::current_dir().unwrap();
        let root = tempfile::tempdir_in(&current).unwrap();
        let path = root.path().join("mcp.yaml");
        let absolute = root.path().join("absolute");
        let server = "    transport: stdio\n    start_command: [server]";
        let text = format!(
            "mcp:\n  relative:\n{server}\n    cwd: work\n  absolute:\n{server}\n    cwd: {}\n  default:\n{server}",
            serde_json::to_string(&absolute).unwrap()
        );
        tokio::fs::write(&path, text).await.unwrap();
        // A relative --config path must still produce absolute process directories.
        let config = Config::load(Some(path.strip_prefix(&current).unwrap()))
            .await
            .unwrap();
        let work = root.path().join("work");
        let cwd = |name: &str| crate::mcp::RawMcpServerConfig::from(config.mcp[name].clone()).cwd;
        assert_eq!(cwd("relative"), Some(work));
        assert_eq!(cwd("absolute"), Some(absolute));
        assert_eq!(cwd("default"), None);
    }

    #[test]
    fn mcp_map_has_exact_name_and_validates_before_provider_credentials() {
        assert!(parse("mcp: {}").unwrap().mcp.is_empty());
        assert!(parse("mcp_servers: {}").is_err());
        assert!(parse("mcp:\n  invalid:\n    transport: stdio").is_err());
        let text = format!(
            "mcp:\n  test:\n    transport: stdio\n    start_command: [server]\n    startup_timeout_secs: 0\n{ANTHROPIC_MISSING_KEY}"
        );
        let error =
            parse(&text).expect_err("MCP is rejected at ingress before provider credentials");
        assert!(error.to_string().contains("startup_timeout_secs"));
    }

    #[test]
    fn example_config_stays_valid() {
        let config = parse(include_str!("../../config.example.yaml")).unwrap();
        assert!(config.providers.contains_key("codex"));
        config.into_runtime().unwrap();
    }

    #[test]
    fn modes_are_exact_ordered_and_validate_names() {
        let mode = |text: &str| {
            parse(&format!(
                "modes:\n  m:\n    {}",
                text.replace('\n', "\n    ")
            ))
        };
        let general = parse("{}").unwrap();
        assert_eq!(general.modes.keys().collect::<Vec<_>>(), ["general"]);
        let expected = [
            Capability::Read,
            Capability::Write,
            Capability::Exec,
            Capability::Network,
            Capability::Agents,
            Capability::Mcp,
        ];
        assert_eq!(general.modes["general"].capabilities, expected);
        assert!(
            general.ceiling().iter().eq(CapabilitySet::default()
                .iter()
                .filter(|c| *c != Capability::Interactive))
        );
        // Declared modes follow the built-in one, in declaration order.
        let declared = parse("modes:\n  z:\n    capabilities: []\n  a:\n    capabilities: [read, targets]\n    instructions: look").unwrap();
        assert_eq!(
            declared.modes.keys().collect::<Vec<_>>(),
            ["general", "z", "a"]
        );
        assert_eq!(declared.modes["general"], general.modes["general"]);
        // A declaration of that name replaces it.
        let replaced =
            parse("modes:\n  z:\n    capabilities: []\n  general:\n    capabilities: [targets]")
                .unwrap();
        assert_eq!(replaced.modes.keys().collect::<Vec<_>>(), ["general", "z"]);
        assert_eq!(
            replaced.modes["general"].capabilities,
            [Capability::Targets]
        );
        assert_eq!(
            declared.modes["a"].capabilities,
            [Capability::Read, Capability::Targets]
        );
        assert_eq!(declared.modes["a"].instructions.as_deref(), Some("look"));
        let hinted = mode("capabilities: []\nhint: Thinks").unwrap();
        assert_eq!(hinted.modes["m"].hint.as_deref(), Some("Thinks"));
        for blank in ["instructions", "hint"] {
            let error = mode(&format!("capabilities: []\n{blank}: ' '"))
                .unwrap_err()
                .to_string();
            assert!(
                error.contains(&format!("{blank} must not be empty")),
                "{error}"
            );
        }
        assert!(replaced.ceiling().iter().eq([Capability::Targets]));
        assert!(
            mode("{}")
                .unwrap_err()
                .to_string()
                .contains("missing field `capabilities`")
        );
        for name in ["unknown", "Mcp", "Interactive", "READ"] {
            let error = mode(&format!("capabilities: ['{name}']"))
                .unwrap_err()
                .to_string();
            assert!(
                error.contains(name) && error.contains("unknown variant"),
                "{error}"
            );
        }
        let scalar = mode("capabilities: read").unwrap_err().to_string();
        assert!(scalar.contains("expected a sequence"), "{scalar}");
        let error = mode("capabilities: [interactive]").unwrap_err();
        assert!(error.to_string().contains("controlled by the runtime host"));
        assert!(mode("capabilities: []\nextra: 1").is_err());
        assert!(parse("capabilities: [read]").is_err());
    }

    #[test]
    fn default_mode_and_instructions_are_admitted_with_the_catalog() {
        const BASE: &str = r#"
providers:
  p:
    dialect: compatible
    codec: chat_completions
    base_url: http://127.0.0.1:1/v1
    models:
      m:
        model: x
        max_context: 128000
        max_output: 4096
"#;
        let runtime = |top: &str, modes: &str| {
            parse(&format!("{top}\n{BASE}{modes}")).and_then(Config::into_runtime)
        };
        let two = "modes:\n  first:\n    capabilities: []\n  second:\n    capabilities: [read]\n";
        let config = runtime("default_mode: second", two).unwrap();
        assert_eq!(config.select_mode(None).unwrap(), "second");
        assert_eq!(config.default_mode(), "second");
        assert_eq!(config.select_mode(Some("first")).unwrap(), "first");
        assert!(config.select_mode(Some("missing")).is_err());
        // `general` stays the default, and declared, until the config says otherwise.
        for (top, modes) in [("", ""), ("", two), ("modes: {}", "")] {
            let config = runtime(top, modes).unwrap();
            assert_eq!(config.select_mode(None).unwrap(), "general");
        }
        for (top, modes) in [
            ("default_mode: missing", two),
            (
                "",
                "modes:\n  blank:\n    capabilities: []\n    instructions: '  '",
            ),
        ] {
            assert!(runtime(top, modes).is_err(), "{top} {modes}");
        }
    }
}
