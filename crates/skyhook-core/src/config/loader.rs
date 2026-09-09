//! Config discovery and TOML parsing.

use std::path::{Path, PathBuf};

use tokio::fs;

use super::{Config, ConfigError};

pub(super) async fn load(explicit: Option<&Path>) -> Result<Config, ConfigError> {
    let path = explicit.map(Path::to_path_buf).or_else(user_config_path);
    let value = match path.as_deref() {
        Some(path) if explicit.is_some() => read_required_toml(path).await?,
        Some(path) => read_optional_toml(path).await?,
        None => toml::Value::Table(toml::Table::new()),
    };
    if value.as_table().is_none_or(toml::Table::is_empty) {
        return Err(ConfigError::Missing);
    }
    let mut config: Config = value.try_into()?;
    if let Some(path) = path {
        // Resolve against the selected file, never the agent workspace or target.
        let path = std::path::absolute(path)?;
        let directory = path.parent().expect("absolute config path has a parent");
        for server in config.mcp.values_mut() {
            if let Some(cwd) = &mut server.cwd
                && cwd.is_relative()
            {
                *cwd = directory.join(&*cwd);
            }
        }
    }
    Ok(config)
}

fn user_config_path() -> Option<PathBuf> {
    super::user_config_directory().map(|root| root.join("config.toml"))
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
