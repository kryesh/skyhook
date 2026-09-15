//! Layer selection, provenance-aware merging, and side-effect-free validation.

use std::{
    fmt,
    path::{Path, PathBuf},
};

use tokio::fs;

use super::{Config, ConfigError};

#[derive(Debug, thiserror::Error)]
enum LayerError {
    #[error("file not found")]
    Missing,
    #[error("could not read configuration: {0}")]
    Read(std::io::Error),
    #[error("{0}")]
    Invalid(String),
}

/// A rejected candidate or fatal layer error. Never includes TOML source excerpts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigDiagnostic {
    pub path: PathBuf,
    pub message: String,
}

impl fmt::Display for ConfigDiagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.message)
    }
}

/// Selected layers in merge order, plus rejected candidate diagnostics.
#[derive(Clone, Debug, Default)]
pub struct ConfigReport {
    pub sources: Vec<PathBuf>,
    pub diagnostics: Vec<ConfigDiagnostic>,
}

impl fmt::Display for ConfigReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for diagnostic in &self.diagnostics {
            write!(f, "\n  {diagnostic}")?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct ResolvedConfig {
    pub config: Config,
    /// Snapshot at resolution time. Use `config.to_toml()` after caller overrides.
    pub normalized_toml: String,
    pub report: ConfigReport,
}

pub(super) async fn resolve(
    workspace: &Path,
    explicit: Option<&Path>,
) -> Result<ResolvedConfig, ConfigError> {
    // Do not even inspect environment roots or workspace for explicit selection.
    if let Some(path) = explicit {
        return resolve_paths(workspace, Some(path), Vec::new()).await;
    }
    resolve_paths(workspace, None, super::paths::user_config_paths()).await
}

async fn resolve_paths(
    workspace: &Path,
    explicit: Option<&Path>,
    candidates: Vec<PathBuf>,
) -> Result<ResolvedConfig, ConfigError> {
    let mut report = ConfigReport::default();
    if let Some(path) = explicit {
        let value = read_layer(path).await.map_err(|error| {
            failure(
                &mut report,
                "explicit configuration failed",
                diagnostic(path, error.to_string()),
            )
        })?;
        let config = deserialize(&value).map_err(|message| {
            failure(
                &mut report,
                "explicit configuration failed",
                diagnostic(path, message),
            )
        })?;
        report.sources.push(path.to_path_buf());
        return finish(config, report);
    }

    let mut merged = toml::Value::Table(toml::Table::new());
    let mut seen = Vec::new();
    for path in candidates {
        // Compare absolute paths as well, so relative/absolute identical candidates
        // cannot result in duplicate attempts or duplicate diagnostics.
        let identity = std::path::absolute(&path).unwrap_or_else(|_| path.clone());
        if seen.contains(&identity) {
            continue;
        }
        seen.push(identity);
        let loaded = match read_layer(&path).await {
            Ok(value) => deserialize(&value)
                .map(|_| value)
                .map_err(|message| diagnostic(&path, message)),
            Err(error) => Err(diagnostic(&path, error.to_string())),
        };
        match loaded {
            Ok(value) => {
                // Validation above intentionally discards deserialization defaults:
                // only actual source values participate in the layer merge.
                merged = value;
                report.sources.push(path);
                break;
            }
            Err(diagnostic) => report.diagnostics.push(diagnostic),
        }
    }

    let workspace = fs::canonicalize(workspace).await.map_err(|error| {
        failure(
            &mut report,
            "could not resolve workspace",
            diagnostic(workspace, error.to_string()),
        )
    })?;
    let workspace_path = workspace.join(".skyhook/config.toml");
    match read_layer(&workspace_path).await {
        Ok(value) => {
            merge(&mut merged, value, &mut Vec::new());
            report.sources.push(workspace_path.clone());
        }
        Err(LayerError::Missing) => {}
        Err(error) => {
            return Err(failure(
                &mut report,
                "workspace configuration failed",
                diagnostic(&workspace_path, error.to_string()),
            ));
        }
    }
    if report.sources.is_empty() {
        return Err(ConfigError::Resolution {
            message: ConfigError::Missing.to_string(),
            report,
        });
    }
    let config = deserialize(&merged).map_err(|message| {
        let path = report.sources.last().cloned().unwrap();
        failure(
            &mut report,
            "effective configuration failed",
            diagnostic(&path, message),
        )
    })?;
    finish(config, report)
}

fn finish(config: Config, report: ConfigReport) -> Result<ResolvedConfig, ConfigError> {
    let normalized_toml = config.to_toml().map_err(|error| ConfigError::Resolution {
        message: error.to_string(),
        report: report.clone(),
    })?;
    Ok(ResolvedConfig {
        config,
        normalized_toml,
        report,
    })
}

fn failure(report: &mut ConfigReport, message: &str, diagnostic: ConfigDiagnostic) -> ConfigError {
    report.diagnostics.push(diagnostic);
    ConfigError::Resolution {
        message: message.to_owned(),
        report: report.clone(),
    }
}

fn diagnostic(path: &Path, message: impl Into<String>) -> ConfigDiagnostic {
    ConfigDiagnostic {
        path: path.to_path_buf(),
        message: message.into(),
    }
}

fn deserialize(value: &toml::Value) -> Result<Config, String> {
    if value.as_table().is_none_or(toml::Table::is_empty) {
        return Err("configuration is empty".to_owned());
    }
    let config: Config = value.clone().try_into().map_err(|error: toml::de::Error| {
        // Display includes source text for parser errors; message() omits it.
        format!("invalid configuration: {}", error.message())
    })?;
    config
        .validate_structure()
        .map_err(|error| error.to_string())?;
    Ok(config)
}

async fn read_layer(path: &Path) -> Result<toml::Value, LayerError> {
    let text = fs::read_to_string(path)
        .await
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => LayerError::Missing,
            _ => LayerError::Read(error),
        })?;
    let mut value: toml::Value = toml::from_str(&text).map_err(|error: toml::de::Error| {
        let location = error
            .span()
            .map(|span| {
                let line = text[..span.start.min(text.len())]
                    .bytes()
                    .filter(|b| *b == b'\n')
                    .count()
                    + 1;
                format!(" at line {line}")
            })
            .unwrap_or_default();
        LayerError::Invalid(format!("invalid TOML{location}: {}", error.message()))
    })?;
    // Convert only source-file-relative values before merging. Thus an inherited
    // cwd retains its own layer's directory even if sibling fields are overridden.
    // session_root intentionally keeps its historical process-CWD-relative meaning;
    // target workspace and SSH key paths belong to another host, not this file.
    if let Some(servers) = value.get_mut("mcp").and_then(toml::Value::as_table_mut) {
        let absolute =
            std::path::absolute(path).map_err(|error| LayerError::Invalid(error.to_string()))?;
        let directory = absolute
            .parent()
            .expect("absolute config path has a parent");
        for (_, server) in servers.iter_mut() {
            if let Some(cwd) = server.get_mut("cwd")
                && let Some(relative) = cwd.as_str()
                && Path::new(relative).is_relative()
            {
                *cwd = toml::Value::String(
                    directory
                        .join(relative)
                        .to_str()
                        .ok_or_else(|| {
                            LayerError::Invalid("MCP cwd cannot be represented as UTF-8".into())
                        })?
                        .to_owned(),
                );
            }
        }
    }
    Ok(value)
}

fn merge(base: &mut toml::Value, overlay: toml::Value, path: &mut Vec<String>) {
    match (base, overlay) {
        (toml::Value::Table(base), toml::Value::Table(overlay)) => {
            for (key, value) in overlay {
                // A target's omitted fields must never leak in from another layer.
                if path.as_slice() == ["targets"] && key != "import_ssh_config" {
                    base.insert(key, value);
                } else if let Some(previous) = base.get_mut(&key) {
                    path.push(key);
                    merge(previous, value, path);
                    path.pop();
                } else {
                    base.insert(key, value);
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
