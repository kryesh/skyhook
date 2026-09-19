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
                if path.as_slice() == ["targets"] {
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

    const MODEL: &str =
        "[models.main]\nprovider = 'local'\nmodel = 'test'\nmax_context = 4096\nmax_output = 512\n";
    const PROVIDER: &str = "[providers.local]\nkind = 'openai'\nbase_url = 'https://example.com/v1'\napi = 'chat_completions'\n";

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
            let xdg = root.path().join("xdg/skyhook/config.toml");
            let home = root.path().join("home/.config/skyhook/config.toml");
            let local = workspace.join(".skyhook/config.toml");
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
        write(&f.xdg, format!("approve_all = true\n{MODEL}{PROVIDER}"));
        std::fs::create_dir(&f.home).unwrap(); // Would be a read error if probed.
        write(&f.local, "[models.main]\nmodel = 'workspace-model'\n");
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
        Some(b"approve_all = true\n[malformed"), // TOML
        Some(b"approve_all = true\nmax_child_depth = 'bad'"), // serde type
        Some(b"approve_all = true\nunrecognized = 1"), // serde unknown field
        Some(b"approve_all = true\ncapabilities = ['interactive']"),
        Some(b"approve_all = true\n[models.bad]\nprovider = 'local'\nmodel = 'bad'\nmax_context = 10\nmax_output = 10"),
        Some(b"approve_all = true\n[targets.bad]\ntype = 'ssh'\nhost = 'bad host'"),
        Some(b"approve_all = true\n[mcp.bad]\ntransport = 'stdio'\nstart_command = []"),
        Some(b"approve_all = true\n[providers.bad]\nkind = 'anthropic'\nbase_url = 'https://example.com'\napi_key_env = 'KEY'\napi_key_command = 'echo key'"),
        Some(b"approve_all = true\n[providers.bad]\nkind = 'anthropic'\nbase_url = 'https://example.com'\nstartup_timeout_secs = 0"),
        Some(b"approve_all = true\n[providers.bad]\nkind = 'anthropic'\nbase_url = 'relative'"),
        Some(b"approve_all = true\n[providers.bad]\nkind = 'anthropic'\nbase_url = 'https://example.com'\napi_key_command = '  '"),
        Some(b"approve_all = true\n[providers.bad]\nkind = 'openai'\nbase_url = 'https://example.com'\napi = 'responses'\nchat_reasoning_replay = 'reasoning'"),
        Some(b"approve_all = true\n[targets.a]\ntype = 'ssh'\nhost = 'a'\nvia = 'b'\n[targets.b]\ntype = 'ssh'\nhost = 'b'\nvia = 'a'"), // route cycle
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
            let cyclic = bytes.is_some_and(|bytes| bytes.ends_with(b"via = 'a'"));
            let message = &resolved.report.diagnostics[0].message;
            assert_eq!(message.contains(CYCLE), cyclic, "{bytes:?}: {message}");
        }
    }

    #[tokio::test]
    async fn failed_candidates_are_reported_if_no_usable_config_exists() {
        let f = Fixture::new();
        write(&f.xdg, "approve_all = 'not-a-bool'");
        write(&f.home, "[broken");
        let error = f.resolve().await.unwrap_err();
        assert_eq!(error.report().unwrap().diagnostics.len(), 2);
        let message = error.to_string();
        assert!(message.contains(f.xdg.to_str().unwrap()));
        assert!(message.contains(f.home.to_str().unwrap()));
        assert!(!message.contains("approve_all =") && !message.contains("[broken"));
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
            b"max_child_depth = 'wrong'",
            b"[models.main]\nmax_output = 0",
            b"[targets.bad]\ntype = 'ssh'", // Atomic/incomplete target.
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
                "{MODEL}\n[targets.changed]\ntype = 'ssh'\nhost = 'old'\nworkspace = '/old'\nvia = 'retained'\nssh.user = 'old-user'\nssh.port = 2222\nssh.auth = {{ kind = 'key', path = '/old-key' }}\n[targets.retained]\ntype = 'ssh'\nhost = 'other'\n"
            ),
        );
        write(&f.local, "[targets.changed]\ntype = 'ssh'\nhost = 'new'\n");
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
        write(&f.local, "[targets.changed]\nhost = 'incomplete'\n");
        assert!(
            f.resolve().await.is_err(),
            "target must not inherit required type"
        );
    }

    #[tokio::test]
    async fn arrays_replace_and_defaults_are_applied_only_after_merge() {
        let f = Fixture::new();
        write(
            &f.xdg,
            format!(
                "capabilities = ['exec']\nmax_child_depth = 8\n{MODEL}\n[mcp.test]\ntransport = 'stdio'\nstart_command = ['old', 'arg']\ncapabilities = ['read']\nstartup_timeout_secs = 77\nenv = {{ A = 'a', B = 'b' }}\n"
            ),
        );
        write(
            &f.local,
            "capabilities = []\n[mcp.test]\nstart_command = ['new']\ncapabilities = []\nenv = { B = 'changed', C = 'c' }\n",
        );
        let resolved = f.resolve().await.unwrap();
        assert!(resolved.config.capabilities.is_empty());
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
        let round_trip: Config = toml::from_str(&resolved.normalized_toml).unwrap();
        assert_eq!(round_trip.mcp["test"].startup_timeout().as_secs(), 77);
        assert!(round_trip.capabilities.is_empty());
        assert!(resolved.normalized_toml.contains("call_timeout_secs = 120"));
    }

    #[tokio::test]
    async fn cwd_origins_follow_values_not_overridden_siblings() {
        let f = Fixture::new();
        write(
            &f.xdg,
            format!(
                "session_root = 'sessions'\n{MODEL}\n[mcp.inherited]\ntransport = 'stdio'\nstart_command = ['old']\ncwd = 'user-work'\n[mcp.changed]\ntransport = 'stdio'\nstart_command = ['old']\ncwd = 'old-work'\n[targets.remote]\ntype = 'ssh'\nhost = 'host'\nworkspace = 'remote-work'\nssh.auth = {{ kind = 'key', path = 'origin-key' }}\n"
            ),
        );
        write(
            &f.local,
            "[mcp.inherited]\nstart_command = ['new']\n[mcp.changed]\ncwd = 'workspace-work'\n",
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
                "{MODEL}{PROVIDER}\napi_key_env = 'SKYHOOK_NONEXISTENT_TEST_RESOLUTION_KEY'\n[providers.command]\nkind = 'anthropic'\nbase_url = 'https://example.com'\napi_key_command = 'touch {}'\n[providers.subscription]\nkind = 'codex'\n",
                marker.display()
            ),
        );
        let mut resolved = f.resolve().await.unwrap();
        assert!(resolved.report.diagnostics.is_empty());
        assert!(resolved.normalized_toml.contains("api_key_env"));
        resolved.config.approve_all = true;
        resolved.config.capabilities.clear();
        let config: Config = toml::from_str(&resolved.config.to_toml().unwrap()).unwrap();
        assert!(config.approve_all);
        assert!(config.capabilities.is_empty());
        assert!(!marker.exists());
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

    fn target(name: &str, via: Option<&str>) -> String {
        let via = via
            .map(|name| format!("via = '{name}'\n"))
            .unwrap_or_default();
        format!("[targets.{name}]\ntype = 'ssh'\nhost = '{name}'\n{via}")
    }

    const CYCLE: &str = "target route contains a cycle";

    #[tokio::test]
    async fn explicit_and_merged_target_cycles_are_rejected() {
        // Explicit files, including self cycles, report only the explicit file.
        for config in [
            target("a", Some("a")),
            format!("{}{}", target("a", Some("b")), target("b", Some("a"))),
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
            target("a", Some("b")),
            format!("{}{}", target("a", Some("b")), target("b", None)),
        ] {
            let f = Fixture::new();
            write(&f.xdg, user);
            write(&f.local, target("b", Some("a")));
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
        write(&f.xdg, target("a", Some("b")));
        // Names may be supplied later by workspace config or at runtime.
        let partial = f.resolve().await.unwrap();
        assert_eq!(partial.report.sources, std::slice::from_ref(&f.xdg));
        assert!(partial.report.diagnostics.is_empty());
        assert!(matches!(
            TargetRegistry::from_definitions(partial.config.targets.definitions().unwrap()),
            Err(TargetError::UnknownJump(name)) if name == "b"
        ));
        write(
            &f.local,
            format!("{}{}", target("b", Some("c")), target("c", None)),
        );
        let resolved = f.resolve().await.unwrap();
        assert!(resolved.report.diagnostics.is_empty());
        let registry =
            TargetRegistry::from_definitions(resolved.config.targets.definitions().unwrap())
                .unwrap();
        let route = registry.route("a").await.unwrap();
        let names: Vec<_> = route.iter().map(|target| target.name.as_str()).collect();
        assert_eq!(names, ["c", "b", "a"]);
    }

    #[tokio::test]
    async fn root_is_implicit_and_local_named_targets_remain_invalid() {
        let f = Fixture::new();
        for config in [
            target("root", None),
            target("a", Some("root")),
            "[targets.a]\ntype = 'local'\nhost = 'localhost'\n".to_owned(),
        ] {
            write(&f.xdg, config);
            assert!(Config::resolve(&f.workspace, Some(&f.xdg)).await.is_err());
        }
        write(&f.xdg, target("a", None));
        assert!(Config::resolve(&f.workspace, Some(&f.xdg)).await.is_ok());
    }
}
