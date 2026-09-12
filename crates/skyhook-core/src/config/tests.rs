//! Loader tests use injected candidate paths rather than mutating process environment.

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

#[tokio::test]
async fn xdg_wins_without_even_reading_home_and_workspace_merges() {
    let f = Fixture::new();
    std::fs::write(&f.xdg, format!("approve_all = true\n{MODEL}{PROVIDER}")).unwrap();
    std::fs::create_dir(&f.home).unwrap(); // Would be a read error if probed.
    std::fs::write(&f.local, "[models.main]\nmodel = 'workspace-model'\n").unwrap();
    let resolved = f.resolve().await.unwrap();
    assert_eq!(resolved.report.sources, [f.xdg.clone(), f.local.clone()]);
    assert!(resolved.report.diagnostics.is_empty());
    assert!(resolved.config.approve_all);
    assert_eq!(resolved.config.models["main"].model, "workspace-model");
    assert_eq!(resolved.config.models["main"].max_output, 512);
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
        Some(b""),
    ];
    for bytes in invalid {
        let f = Fixture::new();
        if let Some(bytes) = bytes {
            if *bytes == b"DIRECTORY" {
                std::fs::create_dir(&f.xdg).unwrap();
            } else {
                std::fs::write(&f.xdg, bytes).unwrap();
            }
        }
        std::fs::write(&f.home, MODEL).unwrap();
        let resolved = f
            .resolve()
            .await
            .unwrap_or_else(|error| panic!("{bytes:?}: {error}"));
        assert_eq!(
            resolved.report.sources.as_slice(),
            std::slice::from_ref(&f.home)
        );
        assert_eq!(resolved.report.diagnostics.len(), 1, "{bytes:?}");
        assert_eq!(resolved.report.diagnostics[0].path, f.xdg);
        assert!(
            !resolved.config.approve_all,
            "rejected candidate leaked fields: {bytes:?}"
        );
        assert!(resolved.config.providers.is_empty());
    }
}

#[tokio::test]
async fn failed_candidates_are_reported_if_no_usable_config_exists() {
    let f = Fixture::new();
    std::fs::write(&f.xdg, "approve_all = 'not-a-bool'").unwrap();
    std::fs::write(&f.home, "[broken").unwrap();
    let error = f.resolve().await.unwrap_err();
    assert_eq!(error.report().unwrap().diagnostics.len(), 2);
    let message = error.to_string();
    assert!(message.contains(f.xdg.to_str().unwrap()));
    assert!(message.contains(f.home.to_str().unwrap()));
    assert!(!message.contains("approve_all ="));
    assert!(!message.contains("[broken"));
}

#[tokio::test]
async fn workspace_only_is_allowed_after_missing_or_invalid_users() {
    for invalid_user in [false, true] {
        let f = Fixture::new();
        if invalid_user {
            std::fs::write(&f.xdg, "[bad").unwrap();
        }
        std::fs::write(&f.local, format!("{MODEL}{PROVIDER}")).unwrap();
        let resolved = f.resolve().await.unwrap();
        assert_eq!(
            resolved.report.sources.as_slice(),
            std::slice::from_ref(&f.local)
        );
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
        std::fs::write(&f.home, MODEL).unwrap();
        std::fs::write(&f.local, bytes).unwrap();
        let error = f.resolve().await.unwrap_err();
        assert_eq!(error.report().unwrap().diagnostics.len(), 2);
        assert!(error.to_string().contains(f.local.to_str().unwrap()));
        assert!(error.to_string().contains(f.xdg.to_str().unwrap()));
    }
    let f = Fixture::new();
    std::fs::write(&f.home, MODEL).unwrap();
    std::fs::create_dir(&f.local).unwrap();
    assert!(f.resolve().await.is_err());
}

#[tokio::test]
async fn explicit_file_is_isolated_even_from_nonexistent_workspace() {
    let f = Fixture::new();
    std::fs::write(&f.xdg, "[broken").unwrap();
    std::fs::write(&f.local, "[broken").unwrap();
    std::fs::write(&f.home, MODEL).unwrap();
    let resolved = Config::resolve(&f.workspace.join("does-not-exist"), Some(&f.home))
        .await
        .unwrap();
    assert_eq!(
        resolved.report.sources.as_slice(),
        std::slice::from_ref(&f.home)
    );
    assert!(resolved.report.diagnostics.is_empty());
    assert_eq!(resolved.config.models.len(), 1);
    std::fs::write(&f.home, "[broken").unwrap();
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
    std::fs::write(&f.xdg, format!("{MODEL}\n[targets]\nimport_ssh_config = true\n[targets.changed]\ntype = 'ssh'\nhost = 'old'\nworkspace = '/old'\nvia = 'retained'\nssh.user = 'old-user'\nssh.port = 2222\nssh.auth = {{ kind = 'key', path = '/old-key' }}\n[targets.retained]\ntype = 'ssh'\nhost = 'other'\n")).unwrap();
    std::fs::write(
        &f.local,
        "[targets]\nimport_ssh_config = false\n[targets.changed]\ntype = 'ssh'\nhost = 'new'\n",
    )
    .unwrap();
    let resolved = f.resolve().await.unwrap();
    let targets = resolved.config.targets;
    assert!(!targets.import_ssh_config);
    assert!(targets.entries.contains_key("retained"));
    let changed = &targets.entries["changed"];
    assert_eq!(changed.host, "new");
    assert_eq!(changed.workspace, PathBuf::from("."));
    assert_eq!(changed.via, None);
    assert_eq!(changed.ssh.user, None);
    assert_eq!(changed.ssh.port, None);
    assert_eq!(changed.ssh.auth, TargetAuth::Openssh);
    std::fs::write(&f.local, "[targets.changed]\nhost = 'incomplete'\n").unwrap();
    assert!(
        f.resolve().await.is_err(),
        "target must not inherit required type"
    );
}

#[tokio::test]
async fn arrays_replace_and_defaults_are_applied_only_after_merge() {
    let f = Fixture::new();
    std::fs::write(&f.xdg, format!("capabilities = ['exec']\nmax_child_depth = 8\n{MODEL}\n[mcp.test]\ntransport = 'stdio'\nstart_command = ['old', 'arg']\ncapabilities = ['read']\nstartup_timeout_secs = 77\nenv = {{ A = 'a', B = 'b' }}\n")).unwrap();
    std::fs::write(&f.local, "capabilities = []\n[mcp.test]\nstart_command = ['new']\ncapabilities = []\nenv = { B = 'changed', C = 'c' }\n").unwrap();
    let resolved = f.resolve().await.unwrap();
    assert!(resolved.config.capabilities.is_empty());
    assert_eq!(resolved.config.max_child_depth, 8);
    let server = &resolved.config.mcp["test"];
    assert_eq!(server.start_command.as_ref().unwrap(), &["new"]);
    assert!(server.capabilities.is_empty());
    assert_eq!(server.startup_timeout_secs, 77);
    assert_eq!(server.call_timeout_secs, 120);
    assert_eq!(server.env["A"], "a");
    assert_eq!(server.env["B"], "changed");
    assert_eq!(server.env["C"], "c");
    let round_trip: Config = toml::from_str(&resolved.normalized_toml).unwrap();
    assert_eq!(round_trip.mcp["test"].startup_timeout_secs, 77);
    assert!(round_trip.capabilities.is_empty());
    assert!(resolved.normalized_toml.contains("call_timeout_secs = 120"));
}

#[tokio::test]
async fn cwd_origins_follow_values_not_overridden_siblings() {
    let f = Fixture::new();
    std::fs::write(&f.xdg, format!("session_root = 'sessions'\n{MODEL}\n[mcp.inherited]\ntransport = 'stdio'\nstart_command = ['old']\ncwd = 'user-work'\n[mcp.changed]\ntransport = 'stdio'\nstart_command = ['old']\ncwd = 'old-work'\n[targets.remote]\ntype = 'ssh'\nhost = 'host'\nworkspace = 'remote-work'\nssh.auth = {{ kind = 'key', path = 'origin-key' }}\n")).unwrap();
    std::fs::write(
        &f.local,
        "[mcp.inherited]\nstart_command = ['new']\n[mcp.changed]\ncwd = 'workspace-work'\n",
    )
    .unwrap();
    let resolved = f.resolve().await.unwrap();
    assert_eq!(
        resolved.config.mcp["inherited"].cwd,
        Some(f.xdg.parent().unwrap().join("user-work"))
    );
    assert_eq!(
        resolved.config.mcp["changed"].cwd,
        Some(f.local.parent().unwrap().join("workspace-work"))
    );
    assert_eq!(
        resolved.config.session_root,
        Some(PathBuf::from("sessions"))
    );
    assert_eq!(
        resolved.config.targets.entries["remote"].workspace,
        PathBuf::from("remote-work")
    );
    assert_eq!(
        resolved.config.targets.entries["remote"].ssh.auth,
        TargetAuth::Key {
            path: "origin-key".into()
        }
    );
}

#[tokio::test]
async fn resolution_neither_requires_secrets_nor_runs_commands_and_dump_tracks_overrides() {
    let f = Fixture::new();
    let marker = f.workspace.join("command-ran");
    std::fs::write(&f.xdg, format!("{MODEL}{PROVIDER}\napi_key_env = 'SKYHOOK_NONEXISTENT_TEST_RESOLUTION_KEY'\n[providers.command]\nkind = 'anthropic'\nbase_url = 'https://example.com'\napi_key_command = 'touch {}'\n[providers.subscription]\nkind = 'codex'\n", marker.display())).unwrap();
    let mut resolved = f.resolve().await.unwrap();
    assert!(!marker.exists());
    assert!(resolved.report.diagnostics.is_empty());
    assert!(resolved.normalized_toml.contains("api_key_env"));
    resolved.config.approve_all = true;
    resolved.config.capabilities.clear();
    let updated = resolved.config.to_toml().unwrap();
    let config: Config = toml::from_str(&updated).unwrap();
    assert!(config.approve_all);
    assert!(config.capabilities.is_empty());
    assert!(!marker.exists());
}

#[tokio::test]
async fn duplicate_candidates_attempted_once() {
    let f = Fixture::new();
    std::fs::write(&f.local, MODEL).unwrap();
    let resolved = resolve_paths(&f.workspace, None, vec![f.xdg.clone(), f.xdg.clone()])
        .await
        .unwrap();
    assert_eq!(resolved.report.diagnostics.len(), 1);
}

#[tokio::test]
async fn no_roots_or_files_returns_missing_but_workspace_needs_no_roots() {
    let f = Fixture::new();
    assert!(resolve_paths(&f.workspace, None, vec![]).await.is_err());
    std::fs::write(&f.local, MODEL).unwrap();
    let config = resolve_paths(&f.workspace, None, vec![]).await.unwrap();
    assert!(config.report.diagnostics.is_empty());
}

fn target(name: &str, via: Option<&str>) -> String {
    let via = via
        .map(|name| format!("via = '{name}'\n"))
        .unwrap_or_default();
    format!("[targets.{name}]\ntype = 'ssh'\nhost = '{name}'\n{via}")
}

#[tokio::test]
async fn cyclic_xdg_targets_fall_back_without_leaking_fields() {
    let f = Fixture::new();
    std::fs::write(
        &f.xdg,
        format!(
            "approve_all = true\n{}{}",
            target("a", Some("b")),
            target("b", Some("a"))
        ),
    )
    .unwrap();
    std::fs::write(&f.home, target("home", None)).unwrap();
    let resolved = f.resolve().await.unwrap();
    assert_eq!(resolved.report.sources, [f.home]);
    assert_eq!(resolved.report.diagnostics.len(), 1);
    assert_eq!(resolved.report.diagnostics[0].path, f.xdg);
    assert!(
        resolved.report.diagnostics[0]
            .message
            .contains("target route contains a cycle")
    );
    assert!(!resolved.config.approve_all);
    assert_eq!(
        resolved.config.targets.entries.keys().collect::<Vec<_>>(),
        ["home"]
    );
}

#[tokio::test]
async fn explicit_target_cycles_including_self_cycles_are_rejected() {
    for config in [
        target("a", Some("a")),
        format!("{}{}", target("a", Some("b")), target("b", Some("a"))),
    ] {
        let f = Fixture::new();
        std::fs::write(&f.xdg, config).unwrap();
        std::fs::write(&f.home, MODEL).unwrap();
        let error = Config::resolve(&f.workspace, Some(&f.xdg))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("target route contains a cycle"));
        let report = error.report().unwrap();
        assert!(report.sources.is_empty());
        assert_eq!(report.diagnostics.len(), 1);
        assert_eq!(report.diagnostics[0].path, f.xdg);
    }
}

#[tokio::test]
async fn merged_target_cycles_are_fatal_including_replaced_definitions() {
    for user in [
        target("a", Some("b")), // Workspace completes an unresolved route.
        format!("{}{}", target("a", Some("b")), target("b", None)),
    ] {
        let f = Fixture::new();
        std::fs::write(&f.xdg, user).unwrap();
        std::fs::write(&f.local, target("b", Some("a"))).unwrap();
        let error = f.resolve().await.unwrap_err();
        assert!(error.to_string().contains("effective configuration failed"));
        assert!(error.to_string().contains("target route contains a cycle"));
        let report = error.report().unwrap();
        assert_eq!(report.sources, [f.xdg, f.local.clone()]);
        assert_eq!(report.diagnostics.len(), 1);
        assert_eq!(report.diagnostics[0].path, f.local);
    }
}

#[tokio::test]
async fn acyclic_targets_and_unresolved_references_are_valid_config() {
    use crate::target::{TargetError, TargetRegistry};

    for import in [false, true] {
        let f = Fixture::new();
        std::fs::write(
            &f.xdg,
            format!(
                "[targets]\nimport_ssh_config = {import}\n{}",
                target("a", Some("b"))
            ),
        )
        .unwrap();
        // The candidate and final config must allow a name supplied later by
        // workspace configuration, imported SSH aliases, or runtime resolution.
        let partial = f.resolve().await.unwrap();
        assert_eq!(
            partial.report.sources.as_slice(),
            std::slice::from_ref(&f.xdg)
        );
        assert!(partial.report.diagnostics.is_empty());
        assert!(matches!(
            TargetRegistry::from_definitions(partial.config.targets.definitions().unwrap()),
            Err(TargetError::UnknownJump(name)) if name == "b"
        ));
        std::fs::write(
            &f.local,
            format!("{}{}", target("b", Some("c")), target("c", None)),
        )
        .unwrap();
        let resolved = f.resolve().await.unwrap();
        assert!(resolved.report.diagnostics.is_empty());
        let registry =
            TargetRegistry::from_definitions(resolved.config.targets.definitions().unwrap())
                .unwrap();
        assert_eq!(
            registry
                .route("a")
                .await
                .unwrap()
                .iter()
                .map(|target| target.name.as_str())
                .collect::<Vec<_>>(),
            ["c", "b", "a"]
        );
    }
}

#[tokio::test]
async fn root_is_implicit_and_local_named_targets_remain_invalid() {
    let f = Fixture::new();
    for config in [
        target("root", None),
        target("a", Some("root")),
        "[targets.a]\ntype = 'local'\nhost = 'localhost'\n".to_owned(),
    ] {
        std::fs::write(&f.xdg, config).unwrap();
        assert!(Config::resolve(&f.workspace, Some(&f.xdg)).await.is_err());
    }
    std::fs::write(&f.xdg, target("a", None)).unwrap();
    assert!(Config::resolve(&f.workspace, Some(&f.xdg)).await.is_ok());
}
