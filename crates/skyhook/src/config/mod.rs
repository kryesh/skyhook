//! The configuration a host parses or builds, its admission, and the providers an admitted
//! configuration builds.

use std::collections::BTreeMap;

use crate::tool::policy::CapabilitySet;
use crate::{
    config::mode::{Mode, ModeName},
    mcp::config::McpServerConfig,
    provider::{
        dialect::{AdmittedProvider, Login, LoginError, RawProviderConfig},
        model::{ModelProfile, ModelRef, ProviderName},
    },
    target::TargetsConfig,
    tool::policy::ModeCapabilities,
};
use serde::Serialize;
use skyhook_base::decode::{Context, Decode, Diagnostics, Expected, Node};
use thiserror::Error;

mod builder;
mod check;
pub mod mode;
mod paths;
mod runtime;

pub use builder::ConfigBuilder;
pub use check::{Refused, process, undeclared_capabilities};
pub use runtime::{ConfiguredModel, RuntimeConfig, SelectionError};

pub(crate) use paths::user_config_directory;
pub use paths::{user_config_directories, workspace_directory, workspace_session_root};

#[derive(Clone, Debug, Serialize, Decode)]
#[serde(default)]
pub struct Config {
    /// The mode a new session starts in: `general` or a declared mode.
    pub default_mode: ModeName,
    /// Named permission presets in declaration order, after the built-in `general`
    /// unless that name is declared.
    #[decode(with = "declared_modes")]
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

/// Declared modes follow the built-in `general`, unless they declare it.
fn declared_modes(node: &Node, cx: &mut Context) -> Option<indexmap::IndexMap<ModeName, Mode>> {
    let mut modes = default_modes();
    modes.extend(indexmap::IndexMap::decode(node, cx)?);
    Some(modes)
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

    /// Parse a single YAML document, refusing every bad value where it is written. The
    /// mode and model new sessions start with are checked by [`Config::into_runtime`].
    pub fn from_yaml(text: &str) -> Result<Self, ConfigError> {
        let node = crate::yaml::read(text, None).map_err(ConfigError::Syntax)?;
        process(&node, |config: &Self| config.refusals(None, true)).map_err(ConfigError::Invalid)
    }

    /// Check what sealing refuses: every provider entry, and what the configuration's
    /// parts say of each other, without external resources.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.admit()?;
        self.sealable().map(drop).map_err(ConfigError::Refused)
    }

    /// Admit every provider entry; sealing keeps the admitted entries.
    fn admit(&self) -> Result<indexmap::IndexMap<ProviderName, AdmittedProvider>, ConfigError> {
        self.providers
            .iter()
            .map(|(name, entry)| {
                let provider = entry.admit().map_err(|error| {
                    let (within, message) = error.refusals().swap_remove(0);
                    let mut path = vec![
                        skyhook_base::decode::Segment::Key("providers".into()),
                        skyhook_base::decode::Segment::Key(name.to_string()),
                    ];
                    path.extend(within);
                    ConfigError::Refused(Refused::new(path, message))
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

    /// The model `name` selects, and where its provider and it are declared.
    pub(crate) fn selected(
        &self,
        name: &ModelRef,
    ) -> Result<((usize, usize), &ModelProfile), SelectionError> {
        let provider = self.providers.get_full(&name.provider);
        let (index, _, provider) = provider.ok_or(SelectionError::UnknownProvider)?;
        let model = provider.settings.model_profile(&name.model);
        let (model, profile) = model.ok_or(SelectionError::UnknownModel)?;
        Ok(((index, model), profile))
    }

    /// The profile of a configured model; an unknown provider or model is
    /// [`ConfigError::Model`].
    pub fn model_profile(&self, name: &ModelRef) -> Result<&ModelProfile, ConfigError> {
        let selected = self.selected(name).map(|(_, profile)| profile);
        selected.map_err(|error| ConfigError::Model {
            name: name.clone(),
            error,
            configured: self.configured_models(),
        })
    }

    /// Edit a configured model's profile without changing its settings or provider
    /// provenance. Seal with [`Config::into_runtime`] to use the edit.
    pub fn model_profile_mut(&mut self, name: &ModelRef) -> Result<&mut ModelProfile, ConfigError> {
        let configured = self.configured_models();
        let provider = self.providers.get_mut(&name.provider);
        let provider = provider.ok_or(SelectionError::UnknownProvider);
        let profile = provider.and_then(|provider| {
            (provider.settings.model_profile_mut(&name.model)).ok_or(SelectionError::UnknownModel)
        });
        profile.map_err(|error| ConfigError::Model {
            name: name.clone(),
            error,
            configured,
        })
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not serialize configuration: {0}")]
    Serialize(crate::yaml::YamlError),
    #[error("invalid configuration: invalid YAML: {0}")]
    Syntax(serde_saphyr::Error),
    /// Every value a document refuses, where it is written.
    #[error("invalid configuration:\n{0}")]
    Invalid(Diagnostics),
    /// A setting of a configuration built in code, by its path.
    #[error("invalid configuration: {0}")]
    Refused(Refused),
    #[error("provider `{provider}` could not be initialized: {error}")]
    Provider {
        provider: ProviderName,
        error: crate::provider::dialect::BuildError,
    },
    #[error("No models configured. Add a named entry under a provider's models in your config.")]
    NoModels,
    #[error("invalid model `{name}`: {error}, {configured}")]
    Model {
        name: ModelRef,
        error: SelectionError,
        configured: Expected<ModelRef>,
    },
    #[error("invalid mode `{name}`: mode is not configured, {declared}")]
    Mode {
        name: ModeName,
        declared: Expected<ModeName>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::policy::{Capability, HostCapabilityDeclaration};
    use skyhook_base::enums::NamedEnum;
    use std::assert_matches;

    fn parse(text: &str) -> Result<Config, ConfigError> {
        Config::from_yaml(text)
    }

    #[test]
    fn config_omits_absent_connection_options() {
        let config = parse(
            "targets:\n  remote:\n    host: host\n    transport: {ssh: {}}\nmcp:\n  local:\n    transport: stdio\n    start_command: [server]\n",
        )
        .unwrap();
        let value = crate::yaml::from_str(&config.to_yaml().unwrap()).unwrap();
        let target = &value["targets"]["remote"];
        assert!(target.get("via").is_none());
        let ssh = &target["transport"]["ssh"];
        assert!(ssh.get("user").is_none());
        assert!(ssh.get("port").is_none());
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
            Err(ConfigError::Mode { .. })
        );
        let refused = runtime("default_mode: missing", two).err().unwrap();
        assert_eq!(
            refused.to_string(),
            "invalid configuration: `default_mode`: default_mode is not a declared mode, \
             expected one of `general`, `first`, `second`"
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

    /// Refused against the product it configures, a host capability no product
    /// declares is refused where it is listed, with every capability it could be.
    #[test]
    fn host_capabilities_must_be_declared_by_the_product() {
        let declared = [HostCapabilityDeclaration {
            name: "host.review".parse().unwrap(),
            title: "Review",
        }];
        let valid = skyhook_base::decode::one_of(
            (Capability::ALL.iter().map(Capability::as_str)).chain(["host.review"]),
        );
        let refused = |text: &str, declared: &[HostCapabilityDeclaration]| {
            let node = crate::yaml::read(text, None).unwrap();
            let checked = process(&node, |config: &Config| {
                config.refusals(Some(declared), true)
            });
            checked.map(drop).map_err(|refused| refused.to_string())
        };
        let mcp = "mcp:\n  s:\n    transport: stdio\n    start_command: [server]\n";
        for (text, at) in [
            (
                "modes:\n  m:\n    capabilities: [read, host.revew]",
                "3:26: `modes.m.capabilities[1]`",
            ),
            (
                &format!("{mcp}    capabilities: [host.revew]") as &str,
                "5:20: `mcp.s.capabilities[0]`",
            ),
        ] {
            assert_eq!(
                refused(text, &declared),
                Err(format!(
                    "{at}: unknown capability `host.revew`, expected one of {valid}"
                ))
            );
            assert!(refused(&text.replace("revew", "review"), &[]).is_err());
            refused(&text.replace("revew", "review"), &declared).unwrap();
        }
    }

    /// One run refuses every bad value of a document and every reference to something
    /// that decoded, each where it is written and in the order written.
    #[test]
    fn a_document_is_refused_once_with_every_refusal_where_it_is_written() {
        let text = "providers:\n  p:\n    dialect: compatible\n    codec: chat\n    \
                    base_url: http://h/v1\n    models:\n      m: {model: x}\n      n: {model: x, \
                    max_context: 2, max_output: 2}\ntargets:\n  t:\n    host: h\n    transport: {ssh: {}}\n    \
                    via: nope\nmodes:\n  m:\n    capabilities: [wirte]\n";
        let refused = Config::from_yaml(text).unwrap_err().to_string();
        let lines = Vec::from_iter(refused.lines().skip(1));
        let codecs = skyhook_base::decode::one_of(crate::provider::codec::CodecName::NAMES);
        let capabilities =
            skyhook_base::decode::one_of(Capability::ALL.iter().map(Capability::as_str));
        assert_eq!(
            lines,
            [
                format!(
                    "4:12: `providers.p.codec`: unknown variant `chat`, expected one of {codecs}"
                ),
                "7:10: `providers.p.models.m`: missing required fields `max_context`, `max_output`"
                    .into(),
                "13:10: `targets.t.via`: target `t` references unknown via target `nope`, and \
                 none is configured"
                    .into(),
                format!(
                    "16:20: `modes.m.capabilities[0]`: unknown capability `wirte`, expected one \
                     of {capabilities}"
                ),
            ],
            "{refused}"
        );
    }
}
