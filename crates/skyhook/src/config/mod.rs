//! The configuration a host parses or builds, its admission, and the providers an admitted
//! configuration builds.

use std::collections::BTreeMap;

use crate::tool::policy::CapabilitySet;
use crate::{
    config::mode::{Mode, ModeName},
    mcp::config::McpServerConfig,
    provider::{
        dialect::{AdmissionError, AdmittedProvider, Login, LoginError, RawProviderConfig},
        model::{ModelProfile, ModelRef, ProviderName},
    },
    target::TargetsConfig,
    tool::policy::ModeCapabilities,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

mod builder;
pub mod mode;
mod paths;
mod runtime;

pub use builder::ConfigBuilder;
pub use runtime::{ConfiguredModel, RuntimeConfig, SelectionError};

pub(crate) use paths::user_config_directory;
pub use paths::{user_config_directories, workspace_directory, workspace_session_root};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// The mode a new session starts in: `general` or a declared mode.
    pub default_mode: ModeName,
    /// Named permission presets in declaration order, after the built-in `general`
    /// unless that name is declared.
    #[serde(deserialize_with = "deserialize_modes")]
    pub modes: indexmap::IndexMap<ModeName, Mode>,
    /// Providers in declaration order, each with the models served through it.
    pub providers: indexmap::IndexMap<ProviderName, RawProviderConfig>,
    /// The model a new session starts with, as `provider/model`; without it, the first
    /// model of the first provider.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_model: Option<ModelRef>,
    pub targets: TargetsConfig,
    /// Named, trusted MCP server connections. Empty by default.
    pub mcp: BTreeMap<String, McpServerConfig>,
    pub max_child_depth: usize,
}

pub(crate) const DEFAULT_MAX_CHILD_DEPTH: usize = 4;

/// What an empty document configures: the built-in `general` mode, and no model to run.
impl Default for Config {
    fn default() -> Self {
        Self {
            default_mode: default_mode(),
            modes: default_modes(),
            providers: indexmap::IndexMap::new(),
            default_model: None,
            targets: TargetsConfig::default(),
            mcp: BTreeMap::new(),
            max_child_depth: DEFAULT_MAX_CHILD_DEPTH,
        }
    }
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
    let general = Mode {
        capabilities: ModeCapabilities::general(),
        instructions: None,
        hint: None,
    };
    [(default_mode(), general)].into()
}

impl Config {
    /// Build a configuration in code, from [`Config::default`].
    pub fn builder() -> ConfigBuilder {
        ConfigBuilder::default()
    }

    /// Every capability some mode grants: the most a session can switch to.
    pub fn ceiling(&self) -> CapabilitySet {
        let modes = self.modes.values();
        modes
            .flat_map(|mode| mode.capabilities.iter().cloned())
            .collect()
    }

    /// Serialize effective configuration, including defaults and caller overrides.
    pub fn to_yaml(&self) -> Result<String, ConfigError> {
        crate::yaml::to_string(self).map_err(ConfigError::Serialize)
    }

    /// Parse and admit a single YAML document: typed extraction naming the offending
    /// field, then admission.
    pub fn from_yaml(text: &str) -> Result<Self, ConfigError> {
        let value = crate::yaml::from_str(text).map_err(ConfigError::Syntax)?;
        let config: Self =
            serde_path_to_error::deserialize(value).map_err(ConfigError::Structure)?;
        config.validate()?;
        Ok(config)
    }

    /// Check targets and every provider entry as sealing admits them, without model
    /// selection or external resources.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.admit().map(drop)
    }

    /// Admit targets and every provider entry; sealing keeps the admitted entries.
    fn admit(&self) -> Result<indexmap::IndexMap<ProviderName, AdmittedProvider>, ConfigError> {
        self.targets.validate_structure()?;
        self.providers
            .iter()
            .map(|(name, entry)| {
                let provider = entry.admit().map_err(|error| ConfigError::Admission {
                    provider: name.clone(),
                    error,
                })?;
                Ok((name.clone(), provider))
            })
            .collect()
    }

    /// The Skyhook-managed login of the provider `name`.
    /// `program` is the host command whose `auth login` signs the entry in.
    pub fn login(
        &self,
        name: &ProviderName,
        program: &crate::identity::HostName,
    ) -> Result<Login, LoginError> {
        let entry = (self.providers.get(name)).ok_or_else(|| LoginError::Unknown(name.clone()))?;
        entry
            .admit()?
            .login(name, program)?
            .ok_or_else(|| LoginError::Unmanaged {
                provider: name.clone(),
                dialect: entry.settings.dialect(),
            })
    }

    /// Every configured model with its profile, in configuration order.
    pub fn models(&self) -> impl Iterator<Item = (ModelRef, &ModelProfile)> {
        self.providers.iter().flat_map(|(provider, entry)| {
            entry
                .settings
                .model_profiles()
                .map(move |(model, profile)| {
                    (ModelRef::new(provider.clone(), model.clone()), profile)
                })
        })
    }

    /// The profile of a configured model; an unknown provider or model is
    /// [`ConfigError::Model`].
    pub fn model_profile(&self, name: &ModelRef) -> Result<&ModelProfile, ConfigError> {
        let provider = self.providers.get(&name.provider);
        provider
            .ok_or(SelectionError::UnknownProvider)
            .and_then(|provider| {
                provider
                    .settings
                    .model_profile(&name.model)
                    .ok_or(SelectionError::UnknownModel)
            })
            .map_err(|error| ConfigError::Model {
                name: name.clone(),
                error,
            })
    }

    /// Edit a configured model's profile without changing its settings or provider
    /// provenance. Seal with [`Config::into_runtime`] to use the edit.
    pub fn model_profile_mut(&mut self, name: &ModelRef) -> Result<&mut ModelProfile, ConfigError> {
        let provider = self.providers.get_mut(&name.provider);
        provider
            .ok_or(SelectionError::UnknownProvider)
            .and_then(|provider| {
                provider
                    .settings
                    .model_profile_mut(&name.model)
                    .ok_or(SelectionError::UnknownModel)
            })
            .map_err(|error| ConfigError::Model {
                name: name.clone(),
                error,
            })
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not serialize configuration: {0}")]
    Serialize(crate::yaml::YamlError),
    #[error("invalid configuration: invalid YAML: {0}")]
    Syntax(serde_saphyr::Error),
    #[error("invalid configuration: {}", located(.0))]
    Structure(serde_path_to_error::Error<serde_json::Error>),
    #[error("invalid configuration: `mcp.{name}`: {error}")]
    Mcp {
        name: String,
        error: crate::mcp::config::McpConfigError,
    },
    #[error("invalid configuration: `providers.{provider}`: {error}")]
    Admission {
        provider: ProviderName,
        error: AdmissionError,
    },
    #[error("invalid configuration: {0}")]
    Targets(#[from] crate::target::TargetError),
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
/// A deserialization error prefixed with the field path it names.
pub fn located(error: &serde_path_to_error::Error<serde_json::Error>) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::policy::Capability;
    use std::assert_matches;

    fn parse(text: &str) -> Result<Config, ConfigError> {
        Config::from_yaml(text)
    }

    #[test]
    fn config_omits_absent_connection_options() {
        let config = parse(
            "targets:\n  remote:\n    type: ssh\n    host: host\nmcp:\n  local:\n    transport: stdio\n    start_command: [server]\n",
        )
        .unwrap();
        let value = crate::yaml::from_str(&config.to_yaml().unwrap()).unwrap();
        let target = &value["targets"]["remote"];
        assert!(target.get("via").is_none());
        assert!(target["ssh"].get("user").is_none());
        assert!(target["ssh"].get("port").is_none());
        let server = &value["mcp"]["local"];
        assert!(server.get("url").is_none());
        assert!(server.get("cwd").is_none());
        assert_eq!(server["start_command"], serde_json::json!(["server"]));
        Config::from_yaml(&config.to_yaml().unwrap()).unwrap();
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
        assert!(general.modes["general"].capabilities.iter().eq(&expected));
        assert!(
            general.ceiling().iter().eq(CapabilitySet::default()
                .iter()
                .filter(|c| **c != Capability::Interactive))
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
        let listed = |mode: &Mode| Vec::from_iter(mode.capabilities.iter().cloned());
        assert_eq!(listed(&replaced.modes["general"]), [Capability::Targets]);
        assert_eq!(
            listed(&declared.modes["a"]),
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
        assert!(replaced.ceiling().iter().eq(&[Capability::Targets]));
        let error = mode("capabilities: [interactive]").unwrap_err();
        assert!(error.to_string().contains("controlled by the runtime host"));
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
        assert_matches!(
            config.select_mode(Some(&name("missing"))),
            Err(ConfigError::Mode {
                error: ModeError::Unknown,
                ..
            })
        );
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
