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
            .capabilities(config.capabilities.iter().copied().collect())
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
mod tests;
