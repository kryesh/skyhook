//! The configuration a launch takes: the library schema plus the launch's own keys.

use serde::Serialize;
use skyhook_base::decode::Decode;

use crate::config::{Config, ConfigError};

#[derive(Clone, Debug, Serialize, Decode)]
pub struct HostConfig {
    /// Approve every tool call without consulting an interactive policy.
    #[serde(default)]
    pub approve_all: bool,
    #[serde(flatten)]
    pub config: Config,
}

impl HostConfig {
    pub fn to_yaml(&self) -> Result<String, ConfigError> {
        crate::yaml::to_string(self).map_err(ConfigError::Serialize)
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

    #[test]
    fn an_unknown_key_is_refused_with_the_launch_and_library_keys() {
        let error = crate::yaml::parse::<HostConfig>("approve_al: true").unwrap_err();
        let mut keys = vec!["approve_all"];
        let map = skyhook_base::decode::Map::default();
        <Config as skyhook_base::decode::Fields>::keys(&map, &mut keys);
        let keys = skyhook_base::decode::one_of(keys);
        assert_eq!(
            error.to_string(),
            format!("1:1: `approve_al`: unknown field `approve_al`, expected one of {keys}")
        );
        assert!(crate::yaml::parse::<HostConfig>("approve_all: 1").is_err());
    }
}
