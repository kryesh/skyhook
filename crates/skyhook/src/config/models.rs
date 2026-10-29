//! Typed access to configured models before runtime admission.

use super::{Config, ConfigError, SelectionError};
use crate::provider::profile::{ModelProfile, ModelRef};

impl Config {
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
