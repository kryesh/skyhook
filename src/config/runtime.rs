//! Runtime admission and configuration-bound model selection.
//!
//! A [`Config`] holds provider entries as written, for inspection and caller
//! edits. Sealing it admits every entry; only the sealed generation builds
//! providers, so an edit is always checked before it takes effect.

use std::{path::PathBuf, sync::Arc};

use indexmap::IndexMap;

use super::{Config, ConfigError, providers::ProviderConfig};
use crate::{
    agent::{Catalog, HarnessBuilder},
    provider::profile::{ModelProfile, ModelRef, ProviderName},
};

/// An admitted, immutable configuration generation. Admission validates settings
/// and catalog membership without reading credentials or starting work.
/// Clone shares that generation; reload admits a new one.
#[derive(Clone)]
pub struct RuntimeConfig(Arc<Admitted>);

struct Admitted {
    config: Config,
    providers: IndexMap<ProviderName, ProviderConfig>,
    /// Where the model a new session starts with sits in `providers`.
    default: (usize, usize),
}

/// Why a model name selects no configured model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SelectionError {
    #[error("provider is not configured")]
    UnknownProvider,
    #[error("model is not configured")]
    UnknownModel,
}

/// A model selected from, and retaining, exactly one immutable configuration.
/// Names remain open-ended external identifiers; the indices are process-local
/// and must not be persisted. Resume/reload selects the recorded name again.
///
/// There is deliberately no API taking both a selection and another config:
/// construction always uses the owner retained by this selection.
#[derive(Clone)]
pub struct ConfiguredModel {
    config: RuntimeConfig,
    provider: usize,
    model: usize,
}

impl Config {
    /// Seal the configuration, caller edits included. Targets, modes and every
    /// provider entry are admitted before the catalog; all admission and
    /// selection errors precede credential lookup, provider contexts, or
    /// workspace access.
    pub fn into_runtime(self) -> Result<RuntimeConfig, ConfigError> {
        let providers = self.admit()?;
        let first = providers
            .values()
            .position(|provider| !provider.models().is_empty())
            .ok_or(ConfigError::NoModels)?;
        if !self.modes.contains_key(&self.default_mode) {
            let message = "default_mode is not a declared mode";
            return Err(ConfigError::Mode(self.default_mode.clone(), message.into()));
        }
        let default = match &self.default_model {
            Some(model) => {
                locate(&providers, model).map_err(|error| ConfigError::DefaultModel {
                    model: model.clone(),
                    error,
                })?
            }
            None => (first, 0),
        };
        Ok(RuntimeConfig(Arc::new(Admitted {
            config: self,
            providers,
            default,
        })))
    }
}

fn locate(
    providers: &IndexMap<ProviderName, ProviderConfig>,
    model: &ModelRef,
) -> Result<(usize, usize), SelectionError> {
    let (provider, _, config) = providers
        .get_full(&model.provider)
        .ok_or(SelectionError::UnknownProvider)?;
    let model = (config.models().get_index_of(&model.model)).ok_or(SelectionError::UnknownModel)?;
    Ok((provider, model))
}

impl RuntimeConfig {
    /// The configuration as written; editing a clone and sealing it again
    /// admits a new generation.
    pub fn config(&self) -> &Config {
        &self.0.config
    }

    /// Admit an external or persisted `provider/model` name against this
    /// configuration generation.
    pub fn select_model(&self, name: &ModelRef) -> Result<ConfiguredModel, ConfigError> {
        let (provider, model) =
            locate(&self.0.providers, name).map_err(|error| ConfigError::Model {
                name: name.clone(),
                error,
            })?;
        Ok(ConfiguredModel {
            config: self.clone(),
            provider,
            model,
        })
    }

    /// The profile of a configured model, by its qualified name.
    pub fn model(&self, name: &ModelRef) -> Option<&ModelProfile> {
        let provider = self.0.providers.get(&name.provider)?;
        Some(provider.models().get(&name.model)?.profile())
    }

    /// Every configured model in declaration order, qualified by its provider.
    pub fn models(&self) -> impl Iterator<Item = (ModelRef, &ModelProfile)> {
        self.0.providers.iter().flat_map(|(provider, config)| {
            config.models().iter().map(move |(model, admitted)| {
                let name = ModelRef::new(provider.clone(), model.clone());
                (name, admitted.profile())
            })
        })
    }

    /// Admit an external mode name, or the configured default when none is given.
    /// Modes are looked up by name wherever they apply, so the admitted name is
    /// the selection.
    pub fn select_mode(&self, name: Option<&str>) -> Result<&str, ConfigError> {
        let Some(name) = name else {
            return Ok(self.default_mode());
        };
        self.config()
            .modes
            .get_key_value(name)
            .map(|(name, _)| name.as_str())
            .ok_or_else(|| ConfigError::Mode(name.into(), "mode is not configured".into()))
    }

    /// The mode a new session starts in; admission proved it declared.
    pub fn default_mode(&self) -> &str {
        &self.config().default_mode
    }

    /// The configured `default_model`, else the first model of the first provider
    /// in source/merge order.
    pub fn default_model(&self) -> ConfiguredModel {
        let (provider, model) = self.0.default;
        ConfiguredModel {
            config: self.clone(),
            provider,
            model,
        }
    }
}

impl ConfiguredModel {
    pub fn name(&self) -> ModelRef {
        let (provider, config) = self.provider();
        let (model, _) = config
            .models()
            .get_index(self.model)
            .expect("admitted model remains in immutable catalog");
        ModelRef::new(provider.clone(), model.clone())
    }

    pub fn profile(&self) -> &ModelProfile {
        let (_, config) = self.provider();
        config
            .models()
            .get_index(self.model)
            .expect("admitted model remains in immutable catalog")
            .1
            .profile()
    }

    pub fn config(&self) -> &RuntimeConfig {
        &self.config
    }

    fn provider(&self) -> (&ProviderName, &ProviderConfig) {
        self.config
            .0
            .providers
            .get_index(self.provider)
            .expect("admitted provider remains in immutable catalog")
    }

    /// Construct providers only after pure admission and selection. Environment
    /// keys retain build-time lookup; command auth remains lazy until invocation.
    /// The builder starts from this admitted catalog without checking it again.
    pub fn harness_builder(
        &self,
        workspace: impl Into<PathBuf>,
    ) -> Result<HarnessBuilder, ConfigError> {
        let config = self.config.config();
        let mut models = IndexMap::new();
        for (name, settings) in &self.config.0.providers {
            for (model, entry) in settings.build(name)? {
                models.insert(ModelRef::new(name.clone(), model), entry);
            }
        }
        let catalog = Catalog {
            models,
            default_model: self.name(),
            modes: config.modes.clone(),
            mode: Some(self.config.default_mode().to_owned()),
        };
        let mut builder = HarnessBuilder::admitted(workspace, catalog)
            .max_child_depth(config.max_child_depth)
            .capabilities(config.ceiling())
            .mcp(config.mcp.clone())
            .targets_config(config.targets.clone());
        if let Some(root) = &config.session_root {
            builder = builder.session_root(root.clone());
        }
        Ok(builder)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VENDOR: &str = "vendor 任意";

    fn name(name: &str) -> ModelRef {
        name.parse().unwrap()
    }

    fn text(base_url: &str, authentication: &str, models: &str) -> String {
        let models = if models.is_empty() {
            "    models: {}\n".to_owned()
        } else {
            format!("    models:\n{models}")
        };
        format!(
            "providers:\n  vendor 任意:\n    dialect: compatible\n    codec: chat_completions\n    base_url: {base_url}\n{authentication}{models}"
        )
    }

    const MODELS: &str = "      z first 任意:\n        model: external:model/version\n        max_context: 8192\n        max_output: 512\n      a second:\n        model: another-external-model\n        max_context: 4096\n        max_output: 256\n";

    fn config() -> Config {
        Config::from_yaml(&text("http://127.0.0.1:1/v1", "", MODELS)).unwrap()
    }

    fn models_mut(
        config: &mut Config,
    ) -> &mut indexmap::IndexMap<
        crate::provider::profile::ModelName,
        crate::provider::dialect::ModelSpec,
    > {
        &mut config.providers.get_index_mut(0).unwrap().1.common.models
    }

    #[test]
    fn selection_retains_its_generation_and_open_names_in_insertion_order() {
        let original = config();
        let runtime = original.clone().into_runtime().unwrap();
        let selected = runtime.default_model();
        assert_eq!(selected.name().to_string(), "vendor 任意/z first 任意");
        let profile = selected.profile();
        assert_eq!(profile.model, "external:model/version");
        assert_eq!(profile.max_context, 8192);
        let second = runtime.select_model(&name("vendor 任意/a second")).unwrap();
        assert_eq!(second.name().to_string(), "vendor 任意/a second");
        for (unknown, reason) in [
            ("unknown/a second", SelectionError::UnknownProvider),
            ("vendor 任意/unknown", SelectionError::UnknownModel),
        ] {
            let unknown = name(unknown);
            assert!(
                matches!(runtime.select_model(&unknown), Err(ConfigError::Model { name, error }) if name == unknown && error == reason),
                "{unknown}"
            );
        }
        assert!(Arc::ptr_eq(&selected.config.0, &second.config.0));

        let mut reload = original;
        models_mut(&mut reload)["z first 任意"].profile.model = "replacement-wire-model".into();
        models_mut(&mut reload).swap_remove("a second");
        let reloaded = reload.into_runtime().unwrap();
        let rebound = reloaded.select_model(&selected.name()).unwrap();
        assert!(!Arc::ptr_eq(&selected.config.0, &rebound.config.0));
        assert_eq!(rebound.profile().model, "replacement-wire-model");
        assert!(reloaded.select_model(&second.name()).is_err());
        drop(runtime);
        assert_eq!(selected.profile().model, "external:model/version");
        assert_eq!(second.profile().model, "another-external-model");
    }

    #[test]
    fn default_model_is_declared_or_first_and_must_be_a_member() {
        let explicit = format!(
            "default_model: {VENDOR}/a second\n{}",
            text("http://127.0.0.1:1/v1", "", MODELS)
        );
        let runtime = Config::from_yaml(&explicit)
            .unwrap()
            .into_runtime()
            .unwrap();
        assert_eq!(
            runtime.default_model().name().to_string(),
            "vendor 任意/a second"
        );
        let missing = format!(
            "default_model: {VENDOR}/missing\n{}",
            text("http://127.0.0.1:1/v1", "", MODELS)
        );
        let error = Config::from_yaml(&missing).unwrap().into_runtime().err();
        assert!(matches!(
            &error,
            Some(ConfigError::DefaultModel { model, error: SelectionError::UnknownModel })
                if model.to_string() == "vendor 任意/missing"
        ));
        assert!(error.unwrap().to_string().contains("default_model"));
        assert!(Config::from_yaml("default_model: unqualified").is_err());
        // An earlier provider without models does not supply the default.
        let two = format!(
            "providers:\n  empty:\n    dialect: codex\n    codec: responses\n  vendor 任意:\n    dialect: compatible\n    codec: chat_completions\n    base_url: http://127.0.0.1:1/v1\n    models:\n{MODELS}"
        );
        let runtime = Config::from_yaml(&two).unwrap().into_runtime().unwrap();
        assert_eq!(
            runtime.default_model().name().to_string(),
            "vendor 任意/z first 任意"
        );
    }

    #[test]
    fn admission_checks_all_catalog_members_before_selection_or_credentials() {
        let root = tempfile::tempdir().unwrap();
        let file_name = root.path().file_name().unwrap().to_string_lossy();
        let variable = format!("SKYHOOK_ABSENT_{file_name}");
        assert!(std::env::var_os(&variable).is_none());
        let keyed = format!("    api_key: {{env: {variable}}}\n");
        let raw = Config::from_yaml(&text("http://127.0.0.1:1/v1", &keyed, MODELS)).unwrap();
        let runtime = raw.into_runtime().unwrap();
        assert!(matches!(
            runtime.select_model(&name("vendor 任意/missing model")),
            Err(ConfigError::Model { .. })
        ));
        let selected = runtime.default_model();
        assert_eq!(selected.profile().model, "external:model/version");
        use crate::provider::dialect::{BuildError, ValueError, ValueField, ValueProblem};
        assert!(matches!(
            selected.harness_builder(root.path()),
            Err(ConfigError::Provider {
                error: BuildError::Value(ValueError {
                    field: ValueField::ApiKey,
                    problem: ValueProblem::MissingEnvironment(name),
                }),
                ..
            }) if name == variable
        ));
        assert!(!root.path().join(".skyhook").exists());

        // Provider settings are rejected at parse time, naming the entry; targets
        // precede catalog checks, and an empty catalog is rejected before any
        // secret lookup.
        let error = Config::from_yaml(&text("not an endpoint", "", "")).unwrap_err();
        let error = error.to_string();
        assert!(
            error.contains("`providers.vendor 任意`: base_url"),
            "{error}"
        );
        let secret = "    api_key: {env: SKYHOOK_EMPTY_CATALOG_MUST_NOT_LOOK_UP_SECRET}\n";
        let mut raw = Config::from_yaml(&text("http://127.0.0.1:1/v1", secret, "")).unwrap();
        raw.targets = crate::yaml::parse("root:\n  type: ssh\n  host: unused").unwrap();
        assert!(matches!(
            raw.clone().into_runtime(),
            Err(ConfigError::Targets(_))
        ));
        raw.targets.entries.clear();
        assert!(matches!(raw.into_runtime(), Err(ConfigError::NoModels)));
        // Keys are the qualified name's parts, so they cannot contain its separator.
        let slashed = Config::from_yaml(&text(
            "http://127.0.0.1:1/v1",
            "",
            "      a/b:\n        model: m\n        max_context: 2\n        max_output: 1\n",
        ));
        assert!(
            slashed
                .unwrap_err()
                .to_string()
                .contains("must not contain '/'")
        );
        assert!(
            Config::from_yaml("providers:\n  a/b:\n    dialect: codex\n    codec: responses\n")
                .is_err()
        );
    }
}
