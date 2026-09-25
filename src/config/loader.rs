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

#[derive(Clone, Debug)]
pub struct ResolvedConfig {
    pub config: Config,
    /// Snapshot at resolution time. Use `config.to_yaml()` after caller overrides.
    pub normalized_yaml: String,
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

    let mut merged = serde_json::Value::Object(serde_json::Map::new());
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
    let workspace_path = super::paths::workspace_config_path(&workspace);
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
    let normalized_yaml = config.to_yaml().map_err(|error| ConfigError::Resolution {
        message: error.to_string(),
        report: report.clone(),
    })?;
    Ok(ResolvedConfig {
        config,
        normalized_yaml,
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

fn deserialize(value: &serde_json::Value) -> Result<Config, String> {
    if value.as_object().is_none_or(serde_json::Map::is_empty) {
        return Err("configuration is empty".to_owned());
    }
    let config =
        Config::from_value(value).map_err(|error| format!("invalid configuration: {error}"))?;
    config
        .validate_structure()
        .map_err(|error| error.to_string())?;
    Ok(config)
}

async fn read_layer(path: &Path) -> Result<serde_json::Value, LayerError> {
    let text = fs::read_to_string(path)
        .await
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => LayerError::Missing,
            _ => LayerError::Read(error),
        })?;
    let mut value: serde_json::Value = crate::yaml::parse(&text).map_err(LayerError::Invalid)?;
    if !value.is_object() {
        return Err(LayerError::Invalid(
            "configuration must be a mapping".into(),
        ));
    }
    // Convert only source-file-relative values before merging. Thus an inherited
    // cwd retains its own layer's directory even if sibling fields are overridden.
    // session_root intentionally keeps its historical process-CWD-relative meaning;
    // target workspace and SSH key paths belong to another host, not this file.
    if let Some(servers) = value
        .get_mut("mcp")
        .and_then(serde_json::Value::as_object_mut)
    {
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
                *cwd = serde_json::Value::String(
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

fn merge(base: &mut serde_json::Value, overlay: serde_json::Value, path: &mut Vec<String>) {
    match (base, overlay) {
        (serde_json::Value::Object(base), serde_json::Value::Object(overlay)) => {
            for (key, value) in overlay {
                // A target's or mode's omitted fields must never leak in from another layer.
                if matches!(path.as_slice(), [table] if table == "targets" || table == "modes") {
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
mod tests {
    // Loader tests use injected candidate paths rather than mutating process environment.

    use super::*;
    use crate::target::TargetAuth;

    const MODEL: &str = "models:\n  main:\n    provider: local\n    model: test\n    max_context: 4096\n    max_output: 512\n";
    const PROVIDER: &str = "providers:\n  local:\n    kind: openai\n    base_url: https://example.com/v1\n    api: chat_completions\n";

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

    #[tokio::test]
    async fn xdg_wins_without_even_reading_home_and_workspace_merges() {
        let f = Fixture::new();
        write(&f.xdg, format!("approve_all: true\n{MODEL}{PROVIDER}"));
        std::fs::create_dir(&f.home).unwrap(); // Would be a read error if probed.
        write(&f.local, "models:\n  main:\n    model: workspace-model\n");
        let resolved = f.resolve().await.unwrap();
        assert_eq!(resolved.report.sources, [f.xdg.clone(), f.local.clone()]);
        assert!(resolved.report.diagnostics.is_empty());
        assert!(resolved.config.approve_all);
        let main = &resolved.config.models["main"];
        assert_eq!(
            (main.model.as_str(), main.max_output),
            ("workspace-model", 512)
        );
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
        Some(b"approve_all: true\nmodels:\n  bad:\n    provider: local\n    model: bad\n    max_context: 10\n    max_output: 10"),
        Some(b"approve_all: true\ntargets:\n  bad:\n    type: ssh\n    host: bad host"),
        Some(b"approve_all: true\nmcp:\n  bad:\n    transport: stdio\n    start_command: []"),
        Some(b"approve_all: true\nproviders:\n  bad:\n    kind: anthropic\n    base_url: https://example.com\n    api_key_env: 'KEY'\n    api_key_command: 'echo key'"),
        Some(b"approve_all: true\nproviders:\n  bad:\n    kind: anthropic\n    base_url: https://example.com\n    startup_timeout_secs: 0"),
        Some(b"approve_all: true\nproviders:\n  bad:\n    kind: anthropic\n    base_url: relative"),
        Some(b"approve_all: true\nproviders:\n  bad:\n    kind: anthropic\n    base_url: https://example.com\n    api_key_command: '  '"),
        Some(b"approve_all: true\nproviders:\n  bad:\n    kind: openai\n    base_url: https://example.com\n    api: responses\n    chat_reasoning_replay: reasoning"),
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
            write(&f.home, MODEL);
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
            assert!(resolved.config.providers.is_empty());
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
            write(&f.local, format!("{MODEL}{PROVIDER}"));
            let resolved = f.resolve().await.unwrap();
            assert_eq!(resolved.report.sources, std::slice::from_ref(&f.local));
            assert_eq!(resolved.report.diagnostics.len(), 2);
            assert_eq!(resolved.config.models.len(), 1);
        }
    }

    #[tokio::test]
    async fn invalid_workspace_is_always_fatal_and_retains_fallback_diagnostics() {
        for bytes in [
            b"[invalid".as_slice(),
            b"\xff",
            b"max_child_depth: wrong",
            b"models:\n  main:\n    max_output: 0",
            b"targets:\n  bad:\n    type: ssh", // Atomic/incomplete target.
        ] {
            let f = Fixture::new();
            write(&f.home, MODEL);
            write(&f.local, bytes);
            let error = f.resolve().await.unwrap_err();
            assert_eq!(error.report().unwrap().diagnostics.len(), 2);
            assert!(error.to_string().contains(f.local.to_str().unwrap()));
            assert!(error.to_string().contains(f.xdg.to_str().unwrap()));
        }
        let f = Fixture::new();
        write(&f.home, MODEL);
        std::fs::create_dir(&f.local).unwrap();
        assert!(f.resolve().await.is_err());
    }

    #[tokio::test]
    async fn explicit_file_is_isolated_even_from_nonexistent_workspace() {
        let f = Fixture::new();
        write(&f.xdg, "[broken");
        write(&f.local, "[broken");
        write(&f.home, MODEL);
        let resolved = Config::resolve(&f.workspace.join("does-not-exist"), Some(&f.home))
            .await
            .unwrap();
        assert_eq!(resolved.report.sources, std::slice::from_ref(&f.home));
        assert!(resolved.report.diagnostics.is_empty());
        assert_eq!(resolved.config.models.len(), 1);
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
                "{MODEL}\ntargets:\n  changed:\n    type: ssh\n    host: old\n    workspace: /old\n    via: retained\n    ssh:\n      user: old-user\n      port: 2222\n      auth: {{kind: key, path: /old-key}}\n  retained:\n    type: ssh\n    host: other\n"
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
                "{MODEL}\nmodes:\n  changed:\n    capabilities: [read, exec]\n    instructions: old\n  retained:\n    capabilities: [read]\n"
            ),
        );
        write(
            &f.local,
            "default_mode: retained\nmodes:\n  changed:\n    capabilities: []\n",
        );
        let config = f.resolve().await.unwrap().config;
        assert_eq!(
            config.modes.keys().collect::<Vec<_>>(),
            ["general", "changed", "retained"]
        );
        assert_eq!(config.default_mode, "retained");
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
                "max_child_depth: 8\n{MODEL}\nmcp:\n  test:\n    transport: stdio\n    start_command: [old, arg]\n    capabilities: [read]\n    startup_timeout_secs: 77\n    env: {{A: a, B: b}}\n"
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
                "session_root: sessions\n{MODEL}\nmcp:\n  inherited:\n    transport: stdio\n    start_command: [old]\n    cwd: user-work\n  changed:\n    transport: stdio\n    start_command: [old]\n    cwd: old-work\ntargets:\n  remote:\n    type: ssh\n    host: host\n    workspace: remote-work\n    ssh:\n      auth: {{kind: key, path: origin-key}}\n"
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
                "{MODEL}{PROVIDER}    api_key_env: SKYHOOK_NONEXISTENT_TEST_RESOLUTION_KEY\n  command:\n    kind: anthropic\n    base_url: https://example.com\n    api_key_command: 'touch {}'\n  subscription:\n    kind: codex\n",
                marker.display()
            ),
        );
        let mut resolved = f.resolve().await.unwrap();
        assert!(resolved.report.diagnostics.is_empty());
        assert!(resolved.normalized_yaml.contains("api_key_env"));
        // Resolved provider defaults are written back, like MCP timeouts.
        for resolved_default in [
            "startup_timeout_secs: 600",
            "read_idle_timeout_secs: 600",
            "chat_reasoning_replay: reasoning_content",
        ] {
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
    async fn model_order_survives_merging_dumping_and_runtime_admission() {
        let f = Fixture::new();
        let models = "models:\n  'true': &profile\n    provider: local\n    model: '42'\n    max_context: 4096\n    max_output: 512\n  '42': *profile\n";
        write(&f.xdg, format!("{PROVIDER}{models}"));
        write(
            &f.local,
            "models:\n  'true':\n    max_output: 256\n  'null':\n    provider: local\n    model: 'true'\n    max_context: 4096\n    max_output: 512\n",
        );
        let resolved = f.resolve().await.unwrap();
        let config = Config::from_yaml(&resolved.normalized_yaml).unwrap();
        assert_eq!(
            config.models.keys().map(String::as_str).collect::<Vec<_>>(),
            ["true", "42", "null"]
        );
        assert_eq!(config.models["true"].max_output, 256);
        assert_eq!(config.models["true"].model, "42");
        assert_eq!(config.models["null"].model, "true");
        assert_eq!(config.into_runtime().unwrap().first_model().name(), "true");
    }

    #[tokio::test]
    async fn null_clears_optional_values_but_does_not_delete_entries_or_apply_defaults() {
        let f = Fixture::new();
        write(
            &f.xdg,
            format!("{MODEL}{PROVIDER}    api_key_env: UNUSED_KEY\n    startup_timeout_secs: 5\n"),
        );
        write(
            &f.local,
            "providers:\n  local:\n    api_key_env: null\n    api_key_command: echo unused\n    startup_timeout_secs: null\n",
        );
        let config = f.resolve().await.unwrap().config;
        let crate::config::RawProviderConfig::Openai {
            api_key_env,
            api_key_command,
            startup_timeout_secs,
            ..
        } = crate::config::RawProviderConfig::from(config.providers["local"].clone())
        else {
            panic!("expected OpenAI provider");
        };
        assert!(api_key_env.is_none());
        assert_eq!(api_key_command.as_deref(), Some("echo unused"));
        let default_startup = crate::provider::backends::ProviderTimeouts::default().startup;
        assert_eq!(startup_timeout_secs, Some(default_startup.as_secs()));

        for overlay in [
            "approve_all: null",
            "models: null",
            "models: {main: null}",
            "modes: null",
            "providers: {local: null}",
        ] {
            write(&f.local, overlay);
            assert!(f.resolve().await.is_err(), "accepted {overlay}");
        }
    }

    #[tokio::test]
    async fn each_layer_requires_a_mapping_and_the_effective_config_must_not_be_empty() {
        let f = Fixture::new();
        write(&f.xdg, MODEL);
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
        assert!(resolve_paths(&f.workspace, None, vec![]).await.is_err());
        write(&f.local, MODEL);
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

    #[tokio::test]
    async fn explicit_and_merged_target_cycles_are_rejected() {
        // Explicit files, including self cycles, report only the explicit file.
        for config in [
            targets(&[("a", Some("a"))]),
            targets(&[("a", Some("b")), ("b", Some("a"))]),
        ] {
            let f = Fixture::new();
            write(&f.xdg, config);
            write(&f.home, MODEL);
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

    #[tokio::test]
    async fn acyclic_targets_and_unresolved_references_are_valid_config() {
        use crate::target::{TargetError, TargetRegistry};

        let f = Fixture::new();
        write(&f.xdg, targets(&[("a", Some("b"))]));
        // Names may be supplied later by workspace config or at runtime.
        let partial = f.resolve().await.unwrap();
        assert_eq!(partial.report.sources, std::slice::from_ref(&f.xdg));
        assert!(partial.report.diagnostics.is_empty());
        assert!(matches!(
            TargetRegistry::from_definitions(partial.config.targets.definitions().unwrap()),
            Err(TargetError::UnknownReference {
                edge: crate::target::TargetEdge::Via,
                reference: name,
                ..
            }) if name == "b"
        ));
        write(&f.local, targets(&[("b", Some("c")), ("c", None)]));
        let resolved = f.resolve().await.unwrap();
        assert!(resolved.report.diagnostics.is_empty());
        let registry =
            TargetRegistry::from_definitions(resolved.config.targets.definitions().unwrap())
                .unwrap();
        let route = registry.route(&"a".parse().unwrap()).await.unwrap();
        let names: Vec<_> = route.iter().map(|target| target.name.as_str()).collect();
        assert_eq!(names, ["c", "b", "a"]);
    }

    #[tokio::test]
    async fn root_is_implicit_and_local_named_targets_remain_invalid() {
        let f = Fixture::new();
        for config in [
            targets(&[("root", None)]),
            targets(&[("a", Some("root"))]),
            "targets:\n  a:\n    type: local\n    host: localhost\n".to_owned(),
        ] {
            write(&f.xdg, config);
            assert!(Config::resolve(&f.workspace, Some(&f.xdg)).await.is_err());
        }
        write(&f.xdg, targets(&[("a", None)]));
        assert!(Config::resolve(&f.workspace, Some(&f.xdg)).await.is_ok());
    }
}
