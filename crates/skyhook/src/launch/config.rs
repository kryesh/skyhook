//! The configuration a launch takes: the library schema plus the launch's own keys.

use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use serde_json::{Map, Value};

use crate::config::{Config, ConfigError};

#[derive(Clone, Debug, Serialize)]
pub struct HostConfig {
    /// Approve every tool call without consulting an interactive policy.
    pub approve_all: bool,
    #[serde(flatten)]
    pub config: Config,
}

impl HostConfig {
    pub fn to_yaml(&self) -> Result<String, ConfigError> {
        crate::yaml::to_string(self).map_err(ConfigError::Serialize)
    }
}

impl<'de> Deserialize<'de> for HostConfig {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Keys {
            #[serde(default)]
            approve_all: bool,
        }
        let mut document = Map::new();
        let Keys { approve_all } = crate::yaml::split(deserializer, &mut document)?;
        let config = serde_path_to_error::deserialize(Value::Object(document))
            .map_err(|error| D::Error::custom(crate::config::located(&error)))?;
        Ok(Self {
            approve_all,
            config,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_stays_valid() {
        let host =
            crate::yaml::parse::<HostConfig>(include_str!("../../../../config.example.yaml"))
                .unwrap();
        assert!(!host.approve_all);
        assert!(host.config.providers.contains_key("codex"));
        host.config.into_runtime().unwrap();
    }
}
