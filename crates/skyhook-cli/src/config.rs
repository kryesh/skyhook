//! `config.yaml` as the host reads it: the library schema plus the host's own keys.

use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use serde_json::{Map, Value};

use skyhook::config::{Config, ConfigError, LayerSources, LayeredConfig};

#[derive(Clone, Debug, Serialize)]
pub struct HostConfig {
    /// Approve every tool call without consulting an interactive policy.
    pub approve_all: bool,
    #[serde(flatten)]
    pub config: Config,
}

impl HostConfig {
    pub fn to_yaml(&self) -> Result<String, ConfigError> {
        skyhook::yaml::to_string(self).map_err(ConfigError::Serialize)
    }
}

type Structure = serde_path_to_error::Error<serde_json::Error>;

/// The host's keys, taken out of the document before the library schema sees it:
/// that schema rejects unknown keys, so they cannot be flattened into it.
fn split(document: Value) -> Result<(bool, Map<String, Value>), Structure> {
    #[derive(Deserialize)]
    struct Keys {
        #[serde(default)]
        approve_all: bool,
        #[serde(flatten)]
        rest: Map<String, Value>,
    }
    let keys: Keys = serde_path_to_error::deserialize(document)?;
    Ok((keys.approve_all, keys.rest))
}

impl<'de> Deserialize<'de> for HostConfig {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let located = |error| D::Error::custom(skyhook::config::located(&error));
        let (approve_all, document) = split(Value::deserialize(deserializer)?).map_err(located)?;
        let config = serde_path_to_error::deserialize(Value::Object(document)).map_err(located)?;
        Ok(Self {
            approve_all,
            config,
        })
    }
}

impl LayeredConfig for HostConfig {
    type Error = ConfigError;

    const FILE_NAME: &'static str = Config::FILE_NAME;
    const NAMED_ENTRIES: &'static [&'static str] = Config::NAMED_ENTRIES;

    fn admit_layer(&mut self, sources: &LayerSources) -> Result<(), Self::Error> {
        self.config.admit_layer(sources)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_stays_valid() {
        let host = skyhook::yaml::parse::<HostConfig>(include_str!("../../../config.example.yaml"))
            .unwrap();
        assert!(!host.approve_all);
        assert!(host.config.providers.contains_key("codex"));
        host.config.into_runtime().unwrap();
    }
}
