//! Config discovery and TOML parsing.

use std::{
    env,
    path::{Path, PathBuf},
};

use tokio::fs;

use super::{Config, ConfigError};

pub(super) async fn load(explicit: Option<&Path>) -> Result<Config, ConfigError> {
    let value = match explicit {
        Some(path) => read_required_toml(path).await?,
        None => match user_config_path() {
            Some(path) => read_optional_toml(&path).await?,
            None => toml::Value::Table(toml::Table::new()),
        },
    };
    if value.as_table().is_none_or(toml::Table::is_empty) {
        return Err(ConfigError::Missing);
    }
    Ok(value.try_into()?)
}

fn user_config_path() -> Option<PathBuf> {
    if let Some(root) = env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(root).join("skyhook/config.toml"));
    }
    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".config/skyhook/config.toml"))
}

async fn read_optional_toml(path: &Path) -> Result<toml::Value, ConfigError> {
    match fs::read_to_string(path).await {
        Ok(text) => Ok(toml::from_str(&text)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(toml::Value::Table(toml::Table::new()))
        }
        Err(error) => Err(error.into()),
    }
}

async fn read_required_toml(path: &Path) -> Result<toml::Value, ConfigError> {
    Ok(toml::from_str(&fs::read_to_string(path).await?)?)
}
