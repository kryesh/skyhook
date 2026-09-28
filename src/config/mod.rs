//! Side-effect-free configuration resolution and provider construction.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use crate::{
    mcp::config::McpServerConfig,
    provider::{
        dialect::{self, AdmissionError, AdmittedProvider, RawProviderConfig},
        profile::{ModelRef, ProviderName},
    },
    target::TargetsConfig,
    tool::policy::{Capability, CapabilitySet, Mode, ModeName},
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

mod loader;
mod paths;
mod runtime;

pub use runtime::{ConfiguredModel, RuntimeConfig, SelectionError};

pub use loader::{ConfigDiagnostic, ConfigReport, ResolutionStage, ResolvedConfig};
pub(crate) use paths::{user_config_directories, user_config_directory};
pub use paths::{workspace_directory, workspace_session_root};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Session storage override. Relative paths retain their historical meaning:
    /// relative to the process working directory, not the config or target directory.
    /// When absent, the harness uses `<resolved workspace>/.skyhook/sessions`.
    /// The CLI always uses that workspace-local directory, ignoring this override.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_root: Option<PathBuf>,
    /// Approve all tool calls without consulting an interactive policy.
    #[serde(default)]
    pub approve_all: bool,
    /// The mode a new session starts in: `general` or a declared mode.
    #[serde(default = "default_mode")]
    pub default_mode: ModeName,
    /// Named permission presets in declaration order, after the built-in `general`
    /// unless that name is declared.
    #[serde(default = "default_modes", deserialize_with = "deserialize_modes")]
    pub modes: indexmap::IndexMap<ModeName, Mode>,
    /// Providers in declaration order, each with the models served through it.
    #[serde(default)]
    pub providers: indexmap::IndexMap<ProviderName, RawProviderConfig>,
    /// The model a new session starts with, as `provider/model`; without it, the first
    /// model of the first provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<ModelRef>,
    #[serde(default, serialize_with = "serialize_connection_options")]
    pub targets: TargetsConfig,
    /// Named, trusted MCP server connections. Empty by default.
    #[serde(default, serialize_with = "serialize_connection_options")]
    pub mcp: BTreeMap<String, McpServerConfig>,
    #[serde(default = "default_child_depth")]
    pub max_child_depth: usize,
}

// Target and MCP protocol serializers retain their own public shapes. In these
// config-only branches, null means an absent ordinary option, never a request
// patch clear; omit it here rather than changing those serializers or YAML values.
fn serialize_connection_options<T: Serialize, S: serde::Serializer>(
    value: &T,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    fn omit_absent(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Object(fields) => {
                fields.retain(|_, value| !value.is_null());
                fields.values_mut().for_each(omit_absent);
            }
            serde_json::Value::Array(items) => items.iter_mut().for_each(omit_absent),
            _ => {}
        }
    }
    let mut value = serde_json::to_value(value).map_err(serde::ser::Error::custom)?;
    omit_absent(&mut value);
    value.serialize(serializer)
}

pub(crate) const DEFAULT_MAX_CHILD_DEPTH: usize = 4;

const fn default_child_depth() -> usize {
    DEFAULT_MAX_CHILD_DEPTH
}

fn default_mode() -> ModeName {
    "general"
        .parse()
        .expect("the built-in mode name is nonblank")
}

fn deserialize_modes<'de, D>(
    deserializer: D,
) -> Result<indexmap::IndexMap<ModeName, Mode>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let mut modes = default_modes();
    modes.extend(indexmap::IndexMap::<ModeName, Mode>::deserialize(
        deserializer,
    )?);
    Ok(modes)
}

fn default_modes() -> indexmap::IndexMap<ModeName, Mode> {
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
        Self::from_value(&crate::yaml::from_str(text).map_err(ConfigError::Syntax)?)
    }

    /// Typed extraction naming the offending field, then admission.
    fn from_value(value: &serde_json::Value) -> Result<Self, ConfigError> {
        let config: Self =
            serde_path_to_error::deserialize(value).map_err(ConfigError::Structure)?;
        config.admit()?;
        Ok(config)
    }

    /// Admit targets and every provider entry, without model selection or
    /// external resources. Loading checks each layer and the merged result
    /// with this; sealing keeps the admitted entries.
    fn admit(&self) -> Result<indexmap::IndexMap<ProviderName, AdmittedProvider>, ConfigError> {
        self.targets.validate_structure()?;
        let providers = self
            .providers
            .iter()
            .map(|(name, entry)| {
                let provider = entry.admit().map_err(|error| EntryError {
                    provider: name.clone(),
                    error,
                })?;
                Ok((name.clone(), provider))
            })
            .collect::<Result<_, EntryError>>()?;
        dialect::validate_entries(&self.providers)?;
        Ok(providers)
    }
}

/// A provider admission failure located in the configuration document.
#[derive(Debug, Error)]
#[error("`providers.{provider}`: {error}")]
pub struct EntryError {
    pub provider: ProviderName,
    pub error: AdmissionError,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("{stage}{report}")]
    Resolution {
        stage: ResolutionStage,
        report: ConfigReport,
    },
    #[error("could not serialize configuration: {0}")]
    Serialize(crate::yaml::YamlError),
    #[error("invalid configuration: invalid YAML: {0}")]
    Syntax(serde_saphyr::Error),
    #[error("invalid configuration: {}", located(.0))]
    Structure(serde_path_to_error::Error<serde_json::Error>),
    #[error("invalid configuration: configuration is empty")]
    Empty,
    #[error("invalid configuration: {0}")]
    Admission(#[from] EntryError),
    #[error("invalid configuration: {0}")]
    Targets(#[from] crate::target::TargetError),
    #[error("no Skyhook config found; pass --config or create ~/.config/skyhook/config.yaml{0}")]
    Missing(ConfigReport),
    #[error("invalid configuration: {0}")]
    Dialect(#[from] dialect::DialectError),
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
    #[error("invalid mode `{name}`: {error}")]
    Mode { name: ModeName, error: ModeError },
}

/// A rejected value reads `` `providers.local.models.main.max_context`: ... ``.
fn located(error: &serde_path_to_error::Error<serde_json::Error>) -> String {
    match error.path().to_string().as_str() {
        "." => error.inner().to_string(),
        path => format!("`{path}`: {}", error.inner()),
    }
}

/// Why a mode name selects no configured mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ModeError {
    #[error("default_mode is not a declared mode")]
    Undeclared,
    #[error("mode is not configured")]
    Unknown,
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
            serde_json::to_value(&config.providers["local"]).unwrap()["models"]
                .get("local")
                .is_some()
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
    fn config_omits_absent_connection_options_without_changing_shared_serializers() {
        let config = parse(
            "targets:\n  remote:\n    type: ssh\n    host: host\nmcp:\n  local:\n    transport: stdio\n    start_command: [server]\n",
        )
        .unwrap();
        let value = crate::yaml::from_str(&config.to_yaml().unwrap()).unwrap();
        assert!(value.get("session_root").is_none());
        let target = &value["targets"]["remote"];
        assert!(target.get("via").is_none());
        assert!(target["ssh"].get("user").is_none());
        assert!(target["ssh"].get("port").is_none());
        let server = &value["mcp"]["local"];
        assert!(server.get("url").is_none());
        assert!(server.get("cwd").is_none());
        assert_eq!(server["start_command"], serde_json::json!(["server"]));

        let target = serde_json::to_value(&config.targets.entries["remote"]).unwrap();
        assert_eq!(target.get("via"), Some(&serde_json::Value::Null));
        assert_eq!(target["ssh"].get("user"), Some(&serde_json::Value::Null));
        let server = serde_json::to_value(&config.mcp["local"]).unwrap();
        assert_eq!(server.get("url"), Some(&serde_json::Value::Null));
        assert_eq!(server.get("cwd"), Some(&serde_json::Value::Null));
        Config::from_yaml(&config.to_yaml().unwrap()).unwrap();
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
        let names = |config: &Config| Vec::from_iter(config.modes.keys().map(ModeName::to_string));
        let general = parse("{}").unwrap();
        assert_eq!(names(&general), ["general"]);
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
        assert_eq!(names(&declared), ["general", "z", "a"]);
        assert_eq!(declared.modes["general"], general.modes["general"]);
        // A declaration of that name replaces it.
        let replaced =
            parse("modes:\n  z:\n    capabilities: []\n  general:\n    capabilities: [targets]")
                .unwrap();
        assert_eq!(names(&replaced), ["general", "z"]);
        assert_eq!(
            replaced.modes["general"].capabilities,
            [Capability::Targets]
        );
        assert_eq!(
            declared.modes["a"].capabilities,
            [Capability::Read, Capability::Targets]
        );
        assert_eq!(
            declared.modes["a"].instructions,
            Some("look".parse().unwrap())
        );
        let hinted = mode("capabilities: []\nhint: Thinks").unwrap();
        assert_eq!(hinted.modes["m"].hint, Some("Thinks".parse().unwrap()));
        for blank in ["instructions", "hint"] {
            let error = mode(&format!("capabilities: []\n{blank}: ' '"))
                .unwrap_err()
                .to_string();
            assert!(
                error.contains(&format!("`modes.m.{blank}`: text must not be blank")),
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
        assert_eq!(config.select_mode(None).unwrap().as_str(), "second");
        assert_eq!(config.default_mode().as_str(), "second");
        let name = |name: &str| name.parse::<ModeName>().unwrap();
        assert_eq!(
            config.select_mode(Some(&name("first"))).unwrap(),
            &name("first")
        );
        assert!(matches!(
            config.select_mode(Some(&name("missing"))),
            Err(ConfigError::Mode {
                error: ModeError::Unknown,
                ..
            })
        ));
        // `general` stays the default, and declared, until the config says otherwise.
        for (top, modes) in [("", ""), ("", two), ("modes: {}", "")] {
            let config = runtime(top, modes).unwrap();
            assert_eq!(config.select_mode(None).unwrap().as_str(), "general");
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
