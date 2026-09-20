//! Immutable runtime admission and configuration-bound model selection.
//!
//! Raw configuration remains available for inspection and caller overrides. Only
//! this boundary retains the provider/catalog proof used for runtime construction.

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use super::{Config, ConfigError, providers::ValidatedProvider};
use crate::{agent::HarnessBuilder, provider::profile::ModelProfile};

/// An admitted, immutable configuration generation. Admission validates settings
/// and model/provider membership without reading credentials or starting work.
/// Clone shares that generation; reload admits a new one.
#[derive(Clone)]
pub struct RuntimeConfig(Arc<AdmittedConfig>);

struct AdmittedConfig {
    config: Config,
    providers: BTreeMap<String, ValidatedProvider>,
}

/// A model selected from, and retaining, exactly one immutable configuration.
/// Names remain open-ended external identifiers; the index is process-local and
/// must not be persisted. Resume/reload selects the recorded name again.
///
/// There is deliberately no API taking both a selection and another config:
/// construction always uses the owner retained by this selection.
#[derive(Clone)]
pub struct ConfiguredModel {
    config: RuntimeConfig,
    index: usize,
}

impl Config {
    /// Seal caller overrides into a runtime configuration. Provider settings are
    /// admitted before targets and the model catalog; all admission and selection
    /// errors precede credential lookup, provider contexts, or workspace access.
    pub fn into_runtime(self) -> Result<RuntimeConfig, ConfigError> {
        let providers = self.validate_structure()?;
        if self.models.is_empty() {
            return Err(ConfigError::NoModels);
        }
        for (name, profile) in &self.models {
            if !providers.contains_key(&profile.provider) {
                return Err(ConfigError::UnknownModelProvider {
                    model: name.clone(),
                    provider: profile.provider.clone(),
                });
            }
        }
        if !self.modes.contains_key(&self.default_mode) {
            let message = "default_mode is not a declared mode";
            return Err(ConfigError::Mode(self.default_mode.clone(), message.into()));
        }
        Ok(RuntimeConfig(Arc::new(AdmittedConfig {
            config: self,
            providers,
        })))
    }
}

impl RuntimeConfig {
    pub fn config(&self) -> &Config {
        &self.0.config
    }

    /// Admit an external or persisted name against this configuration generation.
    pub fn select_model(&self, name: &str) -> Result<ConfiguredModel, ConfigError> {
        let index =
            self.config().models.get_index_of(name).ok_or_else(|| {
                ConfigError::Model(name.into(), "profile is not configured".into())
            })?;
        Ok(ConfiguredModel {
            config: self.clone(),
            index,
        })
    }

    /// Admit an external mode name, or the configured default when none is given.
    pub fn select_mode(&self, name: Option<&str>) -> Result<&str, ConfigError> {
        let config = self.config();
        let name = name.unwrap_or(&config.default_mode);
        let (_, name, _) = config
            .modes
            .get_full(name)
            .ok_or_else(|| ConfigError::Mode(name.into(), "mode is not configured".into()))?;
        Ok(name)
    }

    /// The first configured model in source/merge order. Runtime admission proves
    /// the catalog nonempty; this is not a lexicographic fallback.
    pub fn first_model(&self) -> ConfiguredModel {
        ConfiguredModel {
            config: self.clone(),
            index: 0,
        }
    }
}

impl ConfiguredModel {
    pub fn name(&self) -> &str {
        self.entry().0
    }

    pub fn profile(&self) -> &ModelProfile {
        self.entry().1
    }

    pub fn config(&self) -> &RuntimeConfig {
        &self.config
    }

    fn entry(&self) -> (&str, &ModelProfile) {
        let (name, profile) = self
            .config
            .config()
            .models
            .get_index(self.index)
            .expect("admitted model remains in immutable catalog");
        (name, profile)
    }

    /// Construct providers only after pure admission and selection. Environment
    /// keys retain build-time lookup; command auth remains lazy until invocation.
    pub fn harness_builder(
        &self,
        workspace: impl Into<PathBuf>,
    ) -> Result<HarnessBuilder, ConfigError> {
        let config = self.config.config();
        let mut builder = HarnessBuilder::new(workspace)
            .default_model_profile(self.name())
            .max_child_depth(config.max_child_depth)
            .capabilities(config.ceiling())
            .modes(config.modes.clone())
            .mode(self.config.select_mode(None)?)
            .mcp(config.mcp.clone())
            .targets_config(config.targets.clone());
        for (name, settings) in &self.config.0.providers {
            builder = builder.provider(name.clone(), settings.clone().build(name)?);
        }
        for (name, profile) in &config.models {
            builder = builder.model_profile(name.clone(), profile.clone());
        }
        if let Some(root) = &config.session_root {
            builder = builder.session_root(root.clone());
        }
        Ok(builder)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProviderConfig;

    const VENDOR: &str = "vendor / 任意";

    fn config() -> Config {
        toml::from_str(
            r#"
[providers."vendor / 任意"]
kind = 'openai'
api = 'chat_completions'
base_url = 'http://127.0.0.1:1/v1'
[models."z first / 任意"]
provider = 'vendor / 任意'
model = 'external:model/version'
max_context = 8192
max_output = 512
[models."a second"]
provider = 'vendor / 任意'
model = 'another-external-model'
max_context = 4096
max_output = 256
"#,
        )
        .unwrap()
    }

    fn vendor(config: &mut Config) -> (&mut String, &mut Option<String>) {
        let Some(ProviderConfig::Openai {
            base_url,
            api_key_env,
            ..
        }) = config.providers.get_mut(VENDOR)
        else {
            unreachable!()
        };
        (base_url, api_key_env)
    }

    #[test]
    fn selection_retains_its_generation_and_open_names_in_insertion_order() {
        let original = config();
        let runtime = original.clone().into_runtime().unwrap();
        let selected = runtime.first_model();
        assert_eq!(selected.name(), "z first / 任意");
        let profile = selected.profile();
        assert_eq!(
            (&*profile.provider, &*profile.model),
            (VENDOR, "external:model/version")
        );
        assert_eq!(profile.max_context, 8192);
        let second = runtime.select_model("a second").unwrap();
        assert_eq!(second.name(), "a second");
        assert!(
            matches!(runtime.select_model("unknown / 任意"), Err(ConfigError::Model(name, _)) if name == "unknown / 任意")
        );
        assert!(Arc::ptr_eq(&selected.config.0, &second.config.0));

        let mut reload = original;
        reload.models["z first / 任意"].model = "replacement-wire-model".into();
        reload.models.swap_remove("a second");
        let reloaded = reload.into_runtime().unwrap();
        let rebound = reloaded.select_model(selected.name()).unwrap();
        assert!(!Arc::ptr_eq(&selected.config.0, &rebound.config.0));
        assert_eq!(rebound.profile().model, "replacement-wire-model");
        assert!(reloaded.select_model(second.name()).is_err());
        drop(runtime);
        assert_eq!(selected.profile().model, "external:model/version");
        assert_eq!(second.profile().model, "another-external-model");
    }

    #[test]
    fn admission_checks_all_catalog_members_before_selection_or_credentials() {
        let root = tempfile::tempdir().unwrap();
        let file_name = root.path().file_name().unwrap().to_string_lossy();
        let variable = format!("SKYHOOK_ABSENT_{file_name}");
        assert!(std::env::var_os(&variable).is_none());
        let mut raw = config();
        *vendor(&mut raw).1 = Some(variable.clone());
        raw.models["a second"].provider = "missing provider".into();
        assert!(
            matches!(raw.clone().into_runtime(), Err(ConfigError::UnknownModelProvider { model, provider })
        if model == "a second" && provider == "missing provider")
        );
        raw.models["a second"].provider = VENDOR.into();
        let runtime = raw.into_runtime().unwrap();
        assert!(matches!(
            runtime.select_model("missing model"),
            Err(ConfigError::Model(..))
        ));
        let selected = runtime.first_model();
        assert_eq!(selected.profile().model, "external:model/version");
        assert!(
            matches!(selected.harness_builder(root.path()), Err(ConfigError::MissingEnvironment(name)) if name == variable)
        );
        assert!(!root.path().join(".skyhook").exists());

        // Provider structure precedes catalog checks, and an empty catalog is
        // rejected before any secret lookup.
        let mut raw = config();
        raw.models.clear();
        *vendor(&mut raw).0 = "not an endpoint".into();
        assert!(
            matches!(raw.clone().into_runtime(), Err(ConfigError::Provider(name, _)) if name == VENDOR)
        );
        let (base_url, api_key_env) = vendor(&mut raw);
        *base_url = "http://127.0.0.1:1/v1".into();
        *api_key_env = Some("SKYHOOK_EMPTY_CATALOG_MUST_NOT_LOOK_UP_SECRET".into());
        raw.targets = toml::from_str("[root]\ntype = 'ssh'\nhost = 'unused'").unwrap();
        assert!(matches!(
            raw.clone().into_runtime(),
            Err(ConfigError::Structure(_))
        ));
        raw.targets.entries.clear();
        assert!(matches!(raw.into_runtime(), Err(ConfigError::NoModels)));
    }
}
