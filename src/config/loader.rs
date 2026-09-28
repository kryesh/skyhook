//! Layer selection, provenance-aware merging, and side-effect-free validation.

use std::{
    fmt,
    path::{Path, PathBuf},
};

use tokio::fs;

use super::{Config, ConfigError};
use crate::provider::{
    dialect::Dialect,
    settings::{FieldKind, find_field},
};

#[derive(Debug, thiserror::Error)]
enum LayerError {
    #[error("file not found")]
    Missing,
    #[error("could not read configuration: {0}")]
    Read(std::io::Error),
    #[error(transparent)]
    Yaml(crate::yaml::YamlError),
    #[error("configuration must be a mapping")]
    NotMapping,
    #[error("MCP cwd cannot be represented as UTF-8")]
    NonUtf8Cwd,
}

/// Where resolution stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ResolutionStage {
    #[error("explicit configuration failed")]
    Explicit,
    #[error("could not resolve workspace")]
    Workspace,
    #[error("workspace configuration failed")]
    WorkspaceLayer,
    #[error("no usable Skyhook config found")]
    NoUsableLayer,
    #[error("effective configuration failed")]
    Effective,
}

/// A rejected candidate or fatal layer error. Never includes YAML source excerpts.
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

#[derive(Clone)]
pub struct ResolvedConfig {
    pub config: Config,
    /// Snapshot at resolution time. Use `config.to_yaml()` after caller overrides.
    pub normalized_yaml: String,
    pub report: ConfigReport,
}

/// The YAML may hold literal secrets; only its length is shown.
impl fmt::Debug for ResolvedConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedConfig")
            .field("config", &self.config)
            .field(
                "normalized_yaml",
                &format_args!("<{} bytes>", self.normalized_yaml.len()),
            )
            .field("report", &self.report)
            .finish()
    }
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
        let explicit = |report: &mut ConfigReport, message: String| {
            failure(report, ResolutionStage::Explicit, diagnostic(path, message))
        };
        let value = read_layer(path)
            .await
            .map_err(|error| explicit(&mut report, error.to_string()))?;
        let config =
            deserialize(&value).map_err(|error| explicit(&mut report, error.to_string()))?;
        report.sources.push(path.to_path_buf());
        return finish(config, report);
    }

    let mut merged = serde_json::Value::Object(serde_json::Map::new());
    let mut seen = Vec::new();
    // Whether a candidate exists but was refused, as opposed to none existing.
    let mut rejected = false;
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
                .map_err(|error| diagnostic(&path, error.to_string())),
            Err(LayerError::Missing) => {
                let missing = LayerError::Missing.to_string();
                report.diagnostics.push(diagnostic(&path, missing));
                continue;
            }
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
            Err(diagnostic) => {
                rejected = true;
                report.diagnostics.push(diagnostic);
            }
        }
    }

    let workspace = fs::canonicalize(workspace).await.map_err(|error| {
        let diagnostic = diagnostic(workspace, error.to_string());
        failure(&mut report, ResolutionStage::Workspace, diagnostic)
    })?;
    let workspace_path = super::paths::workspace_config_path(&workspace);
    match read_layer(&workspace_path).await {
        Ok(value) => {
            merge(&mut merged, value, &mut Vec::new(), None);
            report.sources.push(workspace_path.clone());
        }
        Err(LayerError::Missing) => {}
        Err(error) => {
            let diagnostic = diagnostic(&workspace_path, error.to_string());
            return Err(failure(
                &mut report,
                ResolutionStage::WorkspaceLayer,
                diagnostic,
            ));
        }
    }
    if report.sources.is_empty() {
        return Err(if rejected {
            ConfigError::Resolution {
                stage: ResolutionStage::NoUsableLayer,
                report,
            }
        } else {
            ConfigError::Missing(report)
        });
    }
    let config = deserialize(&merged).map_err(|error| {
        let path = report.sources.last().cloned().unwrap();
        let diagnostic = diagnostic(&path, error.to_string());
        failure(&mut report, ResolutionStage::Effective, diagnostic)
    })?;
    finish(config, report)
}

fn finish(config: Config, report: ConfigReport) -> Result<ResolvedConfig, ConfigError> {
    Ok(ResolvedConfig {
        normalized_yaml: config.to_yaml()?,
        config,
        report,
    })
}

fn failure(
    report: &mut ConfigReport,
    stage: ResolutionStage,
    diagnostic: ConfigDiagnostic,
) -> ConfigError {
    report.diagnostics.push(diagnostic);
    ConfigError::Resolution {
        stage,
        report: report.clone(),
    }
}

fn diagnostic(path: &Path, message: impl Into<String>) -> ConfigDiagnostic {
    ConfigDiagnostic {
        path: path.to_path_buf(),
        message: message.into(),
    }
}

fn deserialize(value: &serde_json::Value) -> Result<Config, ConfigError> {
    if value.as_object().is_none_or(serde_json::Map::is_empty) {
        return Err(ConfigError::Empty);
    }
    Config::from_value(value)
}

async fn read_layer(path: &Path) -> Result<serde_json::Value, LayerError> {
    let text = fs::read_to_string(path)
        .await
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => LayerError::Missing,
            _ => LayerError::Read(error),
        })?;
    let mut value: serde_json::Value = crate::yaml::parse(&text).map_err(LayerError::Yaml)?;
    if !value.is_object() {
        return Err(LayerError::NotMapping);
    }
    // Convert only source-file-relative values before merging. Thus an inherited
    // cwd retains its own layer's directory even if sibling fields are overridden.
    // session_root intentionally keeps its historical process-CWD-relative meaning;
    // target workspace and SSH key paths belong to another host, not this file.
    if let Some(servers) = value
        .get_mut("mcp")
        .and_then(serde_json::Value::as_object_mut)
    {
        let absolute = std::path::absolute(path).map_err(LayerError::Read)?;
        let directory = absolute
            .parent()
            .expect("absolute config path has a parent");
        for (_, server) in servers.iter_mut() {
            if let Some(cwd) = server.get_mut("cwd")
                && let Some(relative) = cwd.as_str()
                && Path::new(relative).is_relative()
            {
                let joined = directory.join(relative);
                let joined = joined.to_str().ok_or(LayerError::NonUtf8Cwd)?;
                *cwd = serde_json::Value::String(joined.to_owned());
            }
        }
    }
    Ok(value)
}

/// How a later layer's value at a path joins an earlier one's, following the
/// shape of [`Config`] and its provider entries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Merge {
    /// Mapping keys merge one by one.
    Deep,
    /// The later value replaces the earlier whole: a target's or mode's
    /// omitted fields never leak in from another layer, and a source
    /// (`{env: …}`, `{command: …}`) or a placement (`{header: …}`,
    /// `{field: …}`) is one of several shapes, chosen rather than merged into.
    Whole,
    /// Header names ignore case: a later layer's header replaces an earlier
    /// one's however either spells it.
    Headers,
    /// A dialect change replaces provider settings, preserving only model
    /// profiles and common request settings before merging the new models.
    Provider,
}

fn merge_rule(path: &[String], dialect: Option<Dialect>) -> Merge {
    let path: Vec<&str> = path.iter().map(String::as_str).collect();
    let request = match path.as_slice() {
        ["targets" | "modes", _] => return Merge::Whole,
        ["providers", _] => return Merge::Provider,
        ["providers", _, "headers"] => return Merge::Headers,
        ["providers", _, "api_key"] => return Merge::Whole,
        ["providers", _, "models", _, request @ ..] => request,
        ["providers", _, request @ ..] => request,
        _ => return Merge::Deep,
    };
    let Some(dialect) = dialect else {
        return Merge::Deep;
    };
    let Some((name, nested)) = request.split_first() else {
        return Merge::Deep;
    };
    let mut field = if matches!(path.as_slice(), ["providers", _, "models", _, ..]) {
        dialect.request_field(name)
    } else {
        dialect.provider_field(name)
    };
    for name in nested {
        field = field.and_then(|field| match field.kind {
            FieldKind::Object(fields) => find_field(fields, name),
            FieldKind::Atomic => None,
        });
    }
    match field.map(|field| field.kind) {
        Some(FieldKind::Atomic) => Merge::Whole,
        _ => Merge::Deep,
    }
}

fn merge(
    base: &mut serde_json::Value,
    overlay: serde_json::Value,
    path: &mut Vec<String>,
    dialect: Option<Dialect>,
) {
    match (base, overlay) {
        (serde_json::Value::Object(base), serde_json::Value::Object(overlay)) => {
            if merge_rule(path, dialect) == Merge::Headers {
                base.retain(|name, _| !overlay.keys().any(|key| key.eq_ignore_ascii_case(name)));
            }
            for (key, value) in overlay {
                let Some(previous) = base.get_mut(&key) else {
                    base.insert(key, value);
                    continue;
                };
                path.push(key);
                match merge_rule(path, dialect) {
                    Merge::Whole => *previous = value,
                    Merge::Provider => {
                        let dialect =
                            provider_dialect(&value).or_else(|| provider_dialect(previous));
                        if changes_kind(previous, &value) {
                            replace_provider(previous, value, path, dialect);
                        } else {
                            merge(previous, value, path, dialect);
                        }
                    }
                    _ => merge(previous, value, path, dialect),
                }
                path.pop();
            }
        }
        (base, overlay) => *base = overlay,
    }
}

fn provider_dialect(value: &serde_json::Value) -> Option<Dialect> {
    value.get("dialect")?.as_str()?.parse().ok()
}

/// Whether an overlay names a dialect other than the base entry's.
fn changes_kind(base: &serde_json::Value, overlay: &serde_json::Value) -> bool {
    overlay
        .get("dialect")
        .is_some_and(|dialect| base.get("dialect") != Some(dialect))
}

/// Only profiles and common request choices survive a dialect transition.
/// Remove old dialect-owned model fields before merging the new layer, so its
/// explicitly supplied fields are still validated against the new dialect.
fn replace_provider(
    base: &mut serde_json::Value,
    overlay: serde_json::Value,
    path: &mut Vec<String>,
    dialect: Option<Dialect>,
) {
    let previous_dialect = provider_dialect(base);
    let models = base.get_mut("models").map(std::mem::take);
    *base = overlay;
    if let (Some(mut models), Some(base)) = (models, base.as_object_mut()) {
        if let Some(previous_dialect) = previous_dialect
            && let Some(models) = models.as_object_mut()
        {
            for model in models
                .values_mut()
                .filter_map(serde_json::Value::as_object_mut)
            {
                model.retain(|name, _| !previous_dialect.owns_request_field(name));
            }
        }
        if let Some(overlay) = base.remove("models") {
            path.push("models".into());
            merge(&mut models, overlay, path, dialect);
            path.pop();
        }
        base.insert("models".into(), models);
    }
}

#[cfg(test)]
mod tests {
    // Loader tests use injected candidate paths rather than mutating process environment.

    use super::*;
    use crate::target::TargetAuth;
    use serde_json::{Value, json};

    const PROVIDER: &str = "providers:\n  local:\n    dialect: compatible\n    base_url: https://example.com/v1\n    codec: chat_completions\n";
    const CONFIG: &str = "providers:\n  local:\n    dialect: compatible\n    base_url: https://example.com/v1\n    codec: chat_completions\n    models:\n      main:\n        model: test\n        max_context: 4096\n        max_output: 512\n";

    const ROUTED: &str = r#"providers:
  local:
    dialect: openrouter
    codec: chat_completions
    base_url: https://example.com/v1
    routing:
      order: [provider]
      allow_fallbacks: true
      quantizations: [fp16]
    models:
      main:
        model: test
        max_context: 4096
        max_output: 512
        routing:
          order: [old, removed]
          zdr: true
          data_collection: deny
"#;

    struct Fixture {
        _root: tempfile::TempDir,
        workspace: PathBuf,
        xdg: PathBuf,
        home: PathBuf,
        local: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let workspace = root.path().join("project");
            let xdg = root.path().join("xdg/skyhook/config.yaml");
            let home = root.path().join("home/.config/skyhook/config.yaml");
            let local = workspace.join(".skyhook/config.yaml");
            for path in [&xdg, &home, &local] {
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            }
            Self {
                _root: root,
                workspace,
                xdg,
                home,
                local,
            }
        }

        async fn resolve(&self) -> Result<ResolvedConfig, ConfigError> {
            resolve_paths(
                &self.workspace,
                None,
                vec![self.xdg.clone(), self.home.clone()],
            )
            .await
        }
    }

    fn write(path: &Path, contents: impl AsRef<[u8]>) {
        std::fs::write(path, contents).unwrap();
    }

    fn normalized(config: &Config) -> Value {
        crate::yaml::from_str(&config.to_yaml().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn xdg_wins_without_even_reading_home_and_workspace_merges() {
        let f = Fixture::new();
        write(&f.xdg, format!("approve_all: true\n{CONFIG}"));
        std::fs::create_dir(&f.home).unwrap(); // Would be a read error if probed.
        write(
            &f.local,
            "providers:\n  local:\n    models:\n      main:\n        model: workspace-model\n",
        );
        let resolved = f.resolve().await.unwrap();
        assert_eq!(resolved.report.sources, [f.xdg.clone(), f.local.clone()]);
        assert!(resolved.report.diagnostics.is_empty());
        assert!(resolved.config.approve_all);
        let value = normalized(&resolved.config);
        let main = &value["providers"]["local"]["models"]["main"];
        assert_eq!(main["model"], "workspace-model");
        assert_eq!(main["max_output"], 512);
    }

    #[tokio::test]
    async fn every_failed_candidate_class_falls_back_without_leaking_fields() {
        let invalid: &[Option<&[u8]>] = &[
        None, // missing
        Some(b"DIRECTORY"), // read failure, works even when tests run as root
        Some(b"\xff\xfe"), // invalid UTF-8
        Some(b"approve_all: true\nmalformed: ["), // YAML
        Some(b"approve_all: true\nmax_child_depth: bad"), // serde type
        Some(b"approve_all: true\nunrecognized: 1"), // serde unknown field
        Some(b"approve_all: false\n!!binary YXBwcm92ZV9hbGw=: true"), // coerced key collision
        Some(b"approve_all: true\nmodes:\n  bad:\n    capabilities: [interactive]"),
        Some(b"approve_all: true\nproviders:\n  bad:\n    dialect: codex\n    codec: responses\n    models:\n      bad:\n        model: bad\n        max_context: 10\n        max_output: 10"),
        Some(b"approve_all: true\ntargets:\n  bad:\n    type: ssh\n    host: bad host"),
        Some(b"approve_all: true\nmcp:\n  bad:\n    transport: stdio\n    start_command: []"),
        Some(b"approve_all: true\nproviders:\n  bad:\n    dialect: anthropic\n    codec: messages\n    base_url: https://example.com\n    api_key: {env: ''}"),
        Some(b"approve_all: true\nproviders:\n  bad:\n    dialect: anthropic\n    codec: messages\n    base_url: https://example.com\n    startup_timeout_secs: 0"),
        Some(b"approve_all: true\nproviders:\n  bad:\n    dialect: anthropic\n    codec: messages\n    base_url: relative"),
        Some(b"approve_all: true\nproviders:\n  bad:\n    dialect: anthropic\n    codec: messages\n    base_url: https://example.com\n    api_key: {command: '  '}"),
        Some(b"approve_all: true\nproviders:\n  bad:\n    dialect: openai\n    codec: responses\n    base_url: https://example.com\n    reasoning_summary: maybe"),
        Some(b"approve_all: true\nproviders:\n  bad:\n    dialect: anthropic\n    codec: responses\n    base_url: https://example.com\n    models:\n      bad:\n        model: bad\n        max_context: 4096\n        max_output: 512"),
        Some(b"approve_all: true\ntargets:\n  a:\n    type: ssh\n    host: a\n    via: b\n  b:\n    type: ssh\n    host: b\n    via: a"), // route cycle
        Some(b""),
    ];
        for bytes in invalid {
            let f = Fixture::new();
            match bytes {
                Some(b"DIRECTORY") => std::fs::create_dir(&f.xdg).unwrap(),
                Some(bytes) => write(&f.xdg, bytes),
                None => {}
            }
            write(&f.home, CONFIG);
            let resolved = f
                .resolve()
                .await
                .unwrap_or_else(|error| panic!("{bytes:?}: {error}"));
            assert_eq!(resolved.report.sources, std::slice::from_ref(&f.home));
            assert_eq!(resolved.report.diagnostics.len(), 1, "{bytes:?}");
            assert_eq!(resolved.report.diagnostics[0].path, f.xdg);
            assert!(
                !resolved.config.approve_all,
                "rejected candidate leaked fields: {bytes:?}"
            );
            let providers: Vec<_> = resolved
                .config
                .providers
                .keys()
                .map(ToString::to_string)
                .collect();
            assert_eq!(providers, ["local"], "{bytes:?}");
            assert!(resolved.config.targets.entries.is_empty(), "{bytes:?}");
            // The diagnostic names the actual cause, shown here for the route cycle.
            let cyclic = bytes.is_some_and(|bytes| bytes.ends_with(b"via: a"));
            let message = &resolved.report.diagnostics[0].message;
            assert_eq!(message.contains(CYCLE), cyclic, "{bytes:?}: {message}");
        }
    }

    #[tokio::test]
    async fn failed_candidates_are_reported_if_no_usable_config_exists() {
        let f = Fixture::new();
        write(&f.xdg, "approve_all: not-a-bool");
        write(&f.home, "[broken");
        let error = f.resolve().await.unwrap_err();
        assert!(matches!(error, ConfigError::Resolution { .. }));
        assert_eq!(error.report().unwrap().diagnostics.len(), 2);
        let message = error.to_string();
        assert!(message.contains(f.xdg.to_str().unwrap()));
        assert!(message.contains(f.home.to_str().unwrap()));
        assert!(!message.contains("approve_all:") && !message.contains("[broken"));
    }

    #[tokio::test]
    async fn workspace_only_is_allowed_after_missing_or_invalid_users() {
        for invalid_user in [false, true] {
            let f = Fixture::new();
            if invalid_user {
                write(&f.xdg, "[bad");
            }
            write(&f.local, CONFIG);
            let resolved = f.resolve().await.unwrap();
            assert_eq!(resolved.report.sources, std::slice::from_ref(&f.local));
            assert_eq!(resolved.report.diagnostics.len(), 2);
            assert_eq!(
                normalized(&resolved.config)["providers"]["local"]["models"]
                    .as_object()
                    .unwrap()
                    .len(),
                1
            );
        }
    }

    #[tokio::test]
    async fn invalid_workspace_is_always_fatal_and_retains_fallback_diagnostics() {
        for bytes in [
            b"[invalid".as_slice(),
            b"\xff",
            b"max_child_depth: wrong",
            b"providers:\n  local:\n    models:\n      main:\n        max_output: 0",
            b"targets:\n  bad:\n    type: ssh", // Atomic/incomplete target.
        ] {
            let f = Fixture::new();
            write(&f.home, CONFIG);
            write(&f.local, bytes);
            let error = f.resolve().await.unwrap_err();
            assert_eq!(error.report().unwrap().diagnostics.len(), 2);
            assert!(error.to_string().contains(f.local.to_str().unwrap()));
            assert!(error.to_string().contains(f.xdg.to_str().unwrap()));
        }
        let f = Fixture::new();
        write(&f.home, CONFIG);
        std::fs::create_dir(&f.local).unwrap();
        assert!(f.resolve().await.is_err());
    }

    #[tokio::test]
    async fn explicit_file_is_isolated_even_from_nonexistent_workspace() {
        let f = Fixture::new();
        write(&f.xdg, "[broken");
        write(&f.local, "[broken");
        write(&f.home, CONFIG);
        let resolved = Config::resolve(&f.workspace.join("does-not-exist"), Some(&f.home))
            .await
            .unwrap();
        assert_eq!(resolved.report.sources, std::slice::from_ref(&f.home));
        assert!(resolved.report.diagnostics.is_empty());
        assert_eq!(
            normalized(&resolved.config)["providers"]["local"]["models"]
                .as_object()
                .unwrap()
                .len(),
            1
        );
        write(&f.home, "[broken");
        let error = f.resolve().await.unwrap_err();
        assert!(error.to_string().contains(f.local.to_str().unwrap()));
        let error = Config::resolve(&f.workspace, Some(&f.home))
            .await
            .unwrap_err();
        assert_eq!(error.report().unwrap().diagnostics.len(), 1);
        assert!(!error.to_string().contains(f.xdg.to_str().unwrap()));
        std::fs::remove_file(&f.home).unwrap();
        assert!(Config::resolve(&f.workspace, Some(&f.home)).await.is_err());
    }

    #[tokio::test]
    async fn named_targets_replace_whole_definition_but_other_names_survive() {
        let f = Fixture::new();
        write(
            &f.xdg,
            format!(
                "{CONFIG}\ntargets:\n  changed:\n    type: ssh\n    host: old\n    workspace: /old\n    via: retained\n    ssh:\n      user: old-user\n      port: 2222\n      auth: {{kind: key, path: /old-key}}\n  retained:\n    type: ssh\n    host: other\n"
            ),
        );
        write(
            &f.local,
            "targets:\n  changed:\n    type: ssh\n    host: new\n",
        );
        let targets = f.resolve().await.unwrap().config.targets;
        assert!(targets.entries.contains_key("retained"));
        let changed = &targets.entries["changed"];
        assert_eq!(
            (&*changed.host, &changed.workspace, &changed.via),
            ("new", &PathBuf::from("."), &None)
        );
        assert_eq!(
            (&changed.ssh.user, changed.ssh.port, &changed.ssh.auth),
            (&None, None, &TargetAuth::Default)
        );
        write(&f.local, "targets:\n  changed:\n    host: incomplete\n");
        assert!(
            f.resolve().await.is_err(),
            "target must not inherit required type"
        );
    }

    #[tokio::test]
    async fn named_modes_replace_whole_definition_but_other_names_survive() {
        let f = Fixture::new();
        write(
            &f.xdg,
            format!(
                "{CONFIG}\nmodes:\n  changed:\n    capabilities: [read, exec]\n    instructions: old\n  retained:\n    capabilities: [read]\n"
            ),
        );
        write(
            &f.local,
            "default_mode: retained\nmodes:\n  changed:\n    capabilities: []\n",
        );
        let config = f.resolve().await.unwrap().config;
        assert_eq!(
            Vec::from_iter(config.modes.keys().map(|mode| mode.as_str())),
            ["general", "changed", "retained"]
        );
        assert_eq!(config.default_mode.as_str(), "retained");
        let changed = &config.modes["changed"];
        assert!(changed.capabilities.is_empty() && changed.instructions.is_none());
        write(&f.local, "modes:\n  changed:\n    instructions: new\n");
        assert!(
            f.resolve().await.is_err(),
            "mode must not inherit capabilities"
        );
    }

    #[tokio::test]
    async fn arrays_replace_and_defaults_are_applied_only_after_merge() {
        let f = Fixture::new();
        write(
            &f.xdg,
            format!(
                "max_child_depth: 8\n{CONFIG}\nmcp:\n  test:\n    transport: stdio\n    start_command: [old, arg]\n    capabilities: [read]\n    startup_timeout_secs: 77\n    env: {{A: a, B: b}}\n"
            ),
        );
        write(
            &f.local,
            "mcp:\n  test:\n    start_command: [new]\n    capabilities: []\n    env: {B: changed, C: c}\n",
        );
        let resolved = f.resolve().await.unwrap();
        assert_eq!(resolved.config.max_child_depth, 8);
        let server = &resolved.config.mcp["test"];
        let raw = crate::mcp::RawMcpServerConfig::from(server.clone());
        assert_eq!(raw.start_command.unwrap(), ["new"]);
        assert!(server.capabilities().is_empty());
        assert_eq!(
            (
                server.startup_timeout().as_secs(),
                server.call_timeout().as_secs()
            ),
            (77, 120)
        );
        let env = raw.env;
        assert_eq!((&*env["A"], &*env["B"], &*env["C"]), ("a", "changed", "c"));
        let round_trip = Config::from_yaml(&resolved.normalized_yaml).unwrap();
        assert_eq!(round_trip.mcp["test"].startup_timeout().as_secs(), 77);
        assert!(resolved.normalized_yaml.contains("call_timeout_secs: 120"));
        assert!(!resolved.normalized_yaml.contains("null"));
    }

    #[tokio::test]
    async fn cwd_origins_follow_values_not_overridden_siblings() {
        let f = Fixture::new();
        write(
            &f.xdg,
            format!(
                "session_root: sessions\n{CONFIG}\nmcp:\n  inherited:\n    transport: stdio\n    start_command: [old]\n    cwd: user-work\n  changed:\n    transport: stdio\n    start_command: [old]\n    cwd: old-work\ntargets:\n  remote:\n    type: ssh\n    host: host\n    workspace: remote-work\n    ssh:\n      auth: {{kind: key, path: origin-key}}\n"
            ),
        );
        write(
            &f.local,
            "mcp:\n  inherited:\n    start_command: [new]\n  changed:\n    cwd: workspace-work\n",
        );
        let config = f.resolve().await.unwrap().config;
        let user_work = f.xdg.parent().unwrap().join("user-work");
        let workspace_work = f.local.parent().unwrap().join("workspace-work");
        let cwd = |name: &str| crate::mcp::RawMcpServerConfig::from(config.mcp[name].clone()).cwd;
        assert_eq!(cwd("inherited"), Some(user_work));
        assert_eq!(cwd("changed"), Some(workspace_work));
        assert_eq!(config.session_root, Some(PathBuf::from("sessions")));
        let remote = &config.targets.entries["remote"];
        assert_eq!(remote.workspace, PathBuf::from("remote-work"));
        let origin_key = TargetAuth::Key {
            path: "origin-key".into(),
        };
        assert_eq!(remote.ssh.auth, origin_key);
    }

    #[tokio::test]
    async fn resolution_neither_requires_secrets_nor_runs_commands_and_dump_tracks_overrides() {
        let f = Fixture::new();
        let marker = f.workspace.join("command-ran");
        write(
            &f.xdg,
            format!(
                "{CONFIG}    api_key: {{env: SKYHOOK_NONEXISTENT_TEST_RESOLUTION_KEY}}\n  command:\n    dialect: anthropic\n    codec: messages\n    base_url: https://example.com\n    api_key: {{command: 'touch {}'}}\n  subscription:\n    dialect: codex\n    codec: responses\n",
                marker.display()
            ),
        );
        let mut resolved = f.resolve().await.unwrap();
        assert!(resolved.report.diagnostics.is_empty());
        assert!(
            resolved
                .normalized_yaml
                .contains("env: SKYHOOK_NONEXISTENT_TEST_RESOLUTION_KEY")
        );
        // Resolved provider defaults are written back, like MCP timeouts.
        for resolved_default in ["startup_timeout_secs: 600", "read_idle_timeout_secs: 600"] {
            assert!(resolved.normalized_yaml.contains(resolved_default));
        }
        resolved.config.approve_all = true;
        resolved.config.modes[0].capabilities.clear();
        let config = Config::from_yaml(&resolved.config.to_yaml().unwrap()).unwrap();
        assert!(config.approve_all);
        assert!(config.modes[0].capabilities.is_empty());
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn debug_omits_literal_secrets() {
        let f = Fixture::new();
        write(&f.xdg, format!("{CONFIG}    api_key: sk-literal-secret\n"));
        let resolved = f.resolve().await.unwrap();
        assert!(resolved.normalized_yaml.contains("sk-literal-secret"));
        assert!(!format!("{resolved:?}").contains("sk-literal-secret"));
    }

    #[tokio::test]
    async fn model_order_survives_merging_dumping_and_runtime_admission() {
        let f = Fixture::new();
        let models = "    models:\n      'true': &profile\n        model: '42'\n        max_context: 4096\n        max_output: 512\n      '42': *profile\n";
        write(&f.xdg, format!("{PROVIDER}{models}"));
        write(
            &f.local,
            "providers:\n  local:\n    models:\n      'true':\n        max_output: 256\n      'null':\n        model: 'true'\n        max_context: 4096\n        max_output: 512\n",
        );
        let resolved = f.resolve().await.unwrap();
        let config = Config::from_yaml(&resolved.normalized_yaml).unwrap();
        let value = normalized(&config);
        let models = &value["providers"]["local"]["models"];
        assert_eq!(models["true"]["max_output"], 256);
        assert_eq!(models["true"]["model"], "42");
        assert_eq!(models["null"]["model"], "true");
        let runtime = config.into_runtime().unwrap();
        assert_eq!(
            runtime
                .models()
                .map(|(name, _)| name.to_string())
                .collect::<Vec<_>>(),
            ["local/true", "local/42", "local/null"]
        );
        assert_eq!(runtime.default_model().name().to_string(), "local/true");
    }

    #[tokio::test]
    async fn a_provider_of_another_dialect_replaces_settings_but_merges_models() {
        let f = Fixture::new();
        write(&f.xdg, format!("{CONFIG}    api_key: {{env: USER_KEY}}\n"));
        // Same dialect: settings and models merge field by field.
        write(
            &f.local,
            "providers:\n  local:\n    dialect: compatible\n    base_url: https://other.example/v1\n    models:\n      extra:\n        model: extra\n        max_context: 4096\n        max_output: 512\n",
        );
        let config = f.resolve().await.unwrap().config;
        assert!(config.to_yaml().unwrap().contains("USER_KEY"));
        assert_eq!(
            normalized(&config)["providers"]["local"]["models"]
                .as_object()
                .unwrap()
                .len(),
            2
        );
        // Another dialect: the earlier settings belong to it, so only models carry over.
        write(
            &f.local,
            "providers:\n  local:\n    dialect: codex\n    codec: responses\n    models:\n      extra:\n        model: extra\n        max_context: 4096\n        max_output: 512\n      main:\n        max_output: 256\n",
        );
        let config = f.resolve().await.unwrap().config;
        let value = normalized(&config);
        let models = value["providers"]["local"]["models"].as_object().unwrap();
        assert!(!config.to_yaml().unwrap().contains("USER_KEY"));
        assert_eq!(
            models.keys().map(String::as_str).collect::<Vec<_>>(),
            ["main", "extra"]
        );
        assert_eq!(models["main"]["model"], "test");
        assert_eq!(models["main"]["max_output"], 256);
    }

    #[tokio::test]
    async fn dialect_changes_drop_inherited_request_flags_before_overlaying_models() {
        let f = Fixture::new();
        write(
            &f.xdg,
            format!(
                "{ROUTED}        codec: chat_completions\n        cache_key: {{header: x-model}}\n        cache_ttl: 1h\n      untouched:\n        model: untouched\n        max_context: 4096\n        max_output: 512\n        cache_key: null\n        routing: {{order: [old]}}\n"
            ),
        );
        let overlay = "providers:\n  local:\n    dialect: openai\n    codec: responses\n    base_url: https://other.example/v1\n    models:\n      main:\n        max_output: 256\n        organization: workspace\n";
        write(&f.local, overlay);
        let value = normalized(&f.resolve().await.unwrap().config);
        let entry = &value["providers"]["local"];
        assert!(entry.get("routing").is_none());
        assert_eq!(entry["codec"], "responses");
        let models = entry["models"].as_object().unwrap();
        for model in models.values() {
            assert!(model.get("routing").is_none());
            assert!(model.get("cache_ttl").is_none());
        }
        let main = &models["main"];
        assert_eq!(main["model"], "test");
        assert_eq!(main["max_context"], 4096);
        assert_eq!(main["max_output"], 256);
        assert_eq!(main["codec"], "chat_completions");
        assert_eq!(main["cache_key"], json!({"header": "x-model"}));
        assert_eq!(main["organization"], "workspace");
        assert_eq!(models["untouched"]["model"], "untouched");
        assert_eq!(models["untouched"].get("cache_key"), Some(&Value::Null));

        // Filtering inherited flags must not hide an invalid flag in the new layer.
        write(
            &f.local,
            format!("{overlay}        routing: {{order: [new]}}\n"),
        );
        assert!(f.resolve().await.is_err());
    }

    #[tokio::test]
    async fn structured_request_settings_merge_fields_and_replace_lists_at_both_levels() {
        let f = Fixture::new();
        write(&f.xdg, ROUTED);
        write(
            &f.local,
            "providers:\n  local:\n    routing: {order: [new-provider], allow_fallbacks: false}\n    models:\n      main:\n        routing: {order: [], data_collection: null}\n",
        );
        let resolved = f.resolve().await.unwrap();
        let value = normalized(&resolved.config);
        let entry = &value["providers"]["local"];
        assert_eq!(
            entry["routing"],
            json!({"order": ["new-provider"], "allow_fallbacks": false, "quantizations": ["fp16"]})
        );
        assert_eq!(
            entry["models"]["main"]["routing"],
            json!({"order": [], "zdr": true, "data_collection": null})
        );
        let round_trip = Config::from_yaml(&resolved.normalized_yaml).unwrap();
        assert_eq!(normalized(&round_trip), value);
    }

    #[tokio::test]
    async fn request_null_clears_survive_layering_and_normalized_yaml_round_trips() {
        let f = Fixture::new();
        write(
            &f.xdg,
            format!("{ROUTED}        cache_key: {{header: x-model}}\n"),
        );
        write(
            &f.local,
            "providers:\n  local:\n    routing: null\n    models:\n      main:\n        routing: null\n        cache_key: null\n",
        );
        let resolved = f.resolve().await.unwrap();
        let value = normalized(&resolved.config);
        let entry = &value["providers"]["local"];
        assert_eq!(entry.get("routing"), Some(&Value::Null));
        let model = &entry["models"]["main"];
        assert_eq!(model.get("routing"), Some(&Value::Null));
        assert_eq!(model.get("cache_key"), Some(&Value::Null));
        assert!(
            model.get("user_id").is_none(),
            "undeclared settings still inherit"
        );
        let round_trip = Config::from_yaml(&resolved.normalized_yaml).unwrap();
        assert_eq!(normalized(&round_trip), value);
    }

    #[tokio::test]
    async fn null_clears_optional_values_but_does_not_delete_entries_or_apply_defaults() {
        let f = Fixture::new();
        write(
            &f.xdg,
            format!("{CONFIG}    api_key: {{env: UNUSED_KEY}}\n    startup_timeout_secs: 5\n"),
        );
        write(
            &f.local,
            "providers:\n  local:\n    api_key: {command: echo unused}\n    startup_timeout_secs: null\n",
        );
        let resolved = f.resolve().await.unwrap();
        assert_eq!(
            resolved.config.providers["local"].common.api_key,
            Some(crate::provider::dialect::Sourced::Command {
                command: "echo unused".into()
            })
        );
        let default_startup = crate::provider::http::Timeouts::default().startup;
        let startup = format!("startup_timeout_secs: {}", default_startup.as_secs());
        assert!(resolved.normalized_yaml.contains(&startup));

        for overlay in [
            "approve_all: null",
            "providers: {local: {models: null}}",
            "providers: {local: {models: {main: null}}}",
            "modes: null",
            "providers: {local: null}",
        ] {
            write(&f.local, overlay);
            assert!(f.resolve().await.is_err(), "accepted {overlay}");
        }
    }

    #[tokio::test]
    async fn choices_are_replaced_whole_only_where_a_choice_is_accepted() {
        let f = Fixture::new();
        let env = "      env:\n        model: env-model\n        max_context: 4096\n        max_output: 512\n        cache_key: {header: x-model}\n";
        write(
            &f.xdg,
            format!(
                "{CONFIG}{env}    api_key: {{env: USER_KEY}}\n    headers: {{X-Token: {{command: user}}}}\n    cache_key: {{header: x-entry}}\n"
            ),
        );
        // A model named `env` is a model, not a source.
        write(
            &f.local,
            "providers:\n  local:\n    api_key: {command: workspace}\n    headers: {x-token: {env: WORKSPACE}}\n    cache_key: {body: entry_key}\n    models:\n      env:\n        max_output: 256\n        cache_key: {body: model_key}\n",
        );
        let value = normalized(&f.resolve().await.unwrap().config);
        let entry = &value["providers"]["local"];
        assert_eq!(entry["api_key"], json!({"command": "workspace"}));
        // Header names ignore case across layers too.
        assert_eq!(entry["headers"], json!({"x-token": {"env": "WORKSPACE"}}));
        assert_eq!(entry["cache_key"], json!({"body": "entry_key"}));
        let models = &entry["models"];
        assert_eq!(models.as_object().unwrap().len(), 2);
        let env = &models["env"];
        assert_eq!(env["model"], "env-model");
        assert_eq!(env["max_output"], 256);
        assert_eq!(env["cache_key"], json!({"body": "model_key"}));

        // A layer that changes the dialect still replaces its models' choices whole.
        write(
            &f.local,
            "providers:\n  local:\n    dialect: openai\n    codec: responses\n    base_url: https://example.com/v1\n    models:\n      env:\n        cache_key: {body: model_key}\n",
        );
        let value = normalized(&f.resolve().await.unwrap().config);
        assert_eq!(
            value["providers"]["local"]["models"]["env"]["cache_key"],
            json!({"body": "model_key"})
        );
    }

    #[tokio::test]
    async fn each_layer_requires_a_mapping_and_the_effective_config_must_not_be_empty() {
        let f = Fixture::new();
        write(&f.xdg, CONFIG);
        for document in ["", "# comment only", "null", "scalar", "[one, two]"] {
            write(&f.local, document);
            assert!(
                f.resolve().await.is_err(),
                "accepted workspace {document:?}"
            );
            assert!(Config::resolve(&f.workspace, Some(&f.local)).await.is_err());
        }
        write(&f.local, "{}");
        assert!(
            f.resolve().await.is_ok(),
            "an empty mapping overlay is a no-op"
        );
        assert!(Config::resolve(&f.workspace, Some(&f.local)).await.is_err());
    }

    #[tokio::test]
    async fn candidate_list_edge_cases() {
        let f = Fixture::new();
        // No roots or files is missing config, but a workspace needs no roots.
        let missing = resolve_paths(&f.workspace, None, vec![f.xdg.clone()]).await;
        assert!(
            matches!(missing, Err(ConfigError::Missing(report)) if report.diagnostics.len() == 1)
        );
        write(&f.local, CONFIG);
        let config = resolve_paths(&f.workspace, None, vec![]).await.unwrap();
        assert!(config.report.diagnostics.is_empty());
        // Duplicate candidates are attempted once.
        let resolved = resolve_paths(&f.workspace, None, vec![f.xdg.clone(), f.xdg.clone()])
            .await
            .unwrap();
        assert_eq!(resolved.report.diagnostics.len(), 1);
    }

    fn targets(entries: &[(&str, Option<&str>)]) -> String {
        let mut text = String::from("targets:\n");
        for (name, via) in entries {
            text.push_str(&format!("  {name}:\n    type: ssh\n    host: {name}\n"));
            if let Some(via) = via {
                text.push_str(&format!("    via: {via}\n"));
            }
        }
        text
    }

    const CYCLE: &str = "target route contains a cycle";

    /// A route may span layers: the workspace supplies hops a user target names.
    #[tokio::test]
    async fn target_routes_resolve_across_layers() {
        let f = Fixture::new();
        write(&f.xdg, targets(&[("a", Some("b"))]));
        write(&f.local, targets(&[("b", Some("c")), ("c", None)]));
        let resolved = f.resolve().await.unwrap();
        assert!(resolved.report.diagnostics.is_empty());
        let definitions = resolved.config.targets.definitions().unwrap();
        let registry = crate::target::TargetRegistry::from_definitions(definitions).unwrap();
        let route = registry.route(&"a".parse().unwrap()).await.unwrap();
        let names: Vec<_> = route.iter().map(|target| target.name.as_str()).collect();
        assert_eq!(names, ["c", "b", "a"]);
    }

    #[tokio::test]
    async fn explicit_and_merged_target_cycles_are_rejected() {
        // Explicit files, including self cycles, report only the explicit file.
        for config in [
            targets(&[("a", Some("a"))]),
            targets(&[("a", Some("b")), ("b", Some("a"))]),
        ] {
            let f = Fixture::new();
            write(&f.xdg, config);
            write(&f.home, CONFIG);
            let error = Config::resolve(&f.workspace, Some(&f.xdg))
                .await
                .unwrap_err();
            assert!(error.to_string().contains(CYCLE));
            let report = error.report().unwrap();
            assert!(report.sources.is_empty());
            assert_eq!(report.diagnostics.len(), 1);
            assert_eq!(report.diagnostics[0].path, f.xdg);
        }
        // Merged cycles are fatal, including when a workspace replaces a definition.
        for user in [
            targets(&[("a", Some("b"))]),
            targets(&[("a", Some("b")), ("b", None)]),
        ] {
            let f = Fixture::new();
            write(&f.xdg, user);
            write(&f.local, targets(&[("b", Some("a"))]));
            let error = f.resolve().await.unwrap_err();
            assert!(error.to_string().contains("effective configuration failed"));
            assert!(error.to_string().contains(CYCLE));
            let report = error.report().unwrap();
            assert_eq!(report.sources, [f.xdg, f.local.clone()]);
            assert_eq!(report.diagnostics.len(), 1);
            assert_eq!(report.diagnostics[0].path, f.local);
        }
    }
}
