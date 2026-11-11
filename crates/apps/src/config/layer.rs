//! `config.yaml` as the applications layer it: the main configuration, checked against
//! the host capabilities of the product it is loaded to run, if any.

use super::LayeredConfig;
use skyhook::config::Refused;
use skyhook::launch::config::HostConfig;
use skyhook::tool::policy::HostCapabilityDeclaration;

impl LayeredConfig for HostConfig {
    /// The host capabilities the product to run declares; none where the file is read
    /// for something no product's grants bear on.
    type Context<'a> = Option<&'a [HostCapabilityDeclaration]>;

    const FILE_NAME: &'static str = "config.yaml";
    const NAMED_ENTRIES: &'static [&'static str] = &["providers", "modes", "targets", "mcp"];

    fn check(&self, declared: Self::Context<'_>, complete: bool) -> Vec<Refused> {
        self.config.refusals(declared, complete)
    }
}

#[cfg(test)]
mod tests {
    use std::assert_matches;
    // Loader tests use injected candidate paths rather than mutating process environment.

    use super::*;
    use crate::config::loader::{self, tests::write};
    use crate::config::{LayerResolutionError, ResolvedConfig, resolve_layers};
    use serde_json::Value;
    use skyhook::config::Config;
    use skyhook::provider::dialect::{AuthStatus, ConfigHome, LoginError};
    use skyhook::target::SshAuth;
    use skyhook::tool::policy::CapabilitySet;
    use std::path::{Path, PathBuf};

    async fn resolve_paths(
        workspace: &Path,
        explicit: Option<&Path>,
        candidates: Vec<PathBuf>,
    ) -> Result<ResolvedConfig<HostConfig>, LayerResolutionError> {
        loader::resolve_paths::<HostConfig>(workspace, explicit, candidates, None).await
    }

    const PROVIDER: &str = "providers:\n  local:\n    dialect: compatible\n    base_url: https://example.com/v1\n    codec: chat_completions\n";
    const CONFIG: &str = "providers:\n  local:\n    dialect: compatible\n    base_url: https://example.com/v1\n    codec: chat_completions\n    models:\n      main:\n        model: test\n        max_context: 4096\n        max_output: 512\n";

    type Fixture = loader::tests::Fixture<HostConfig>;

    fn normalized(config: &Config) -> Value {
        skyhook::yaml::from_str(&config.to_yaml().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn cwd_and_credentials_are_relative_to_selected_config_directory() {
        let current = std::env::current_dir().unwrap();
        let root = tempfile::tempdir_in(&current).unwrap();
        let path = root.path().join("mcp.yaml");
        let absolute = root.path().join("absolute");
        let server = "    transport: stdio\n    start_command: [server]";
        let text = format!(
            "providers:\n  sub:\n    dialect: codex\n    codec: responses\n  keyed:\n    dialect: openai\n    codec: responses\n    base_url: https://example.com/v1\nmcp:\n  relative:\n{server}\n    cwd: work\n  absolute:\n{server}\n    cwd: {}\n  default:\n{server}",
            serde_json::to_string(&absolute).unwrap()
        );
        tokio::fs::write(&path, text).await.unwrap();
        // A relative --config path must still produce absolute process directories.
        let relative = path.strip_prefix(&current).unwrap();
        let resolved = resolve_layers::<HostConfig>(Path::new("."), Some(relative), None);
        let config = resolved.await.unwrap().config.config;
        let work = root.path().join("work");
        let cwd = |name: &str| serde_json::to_value(&config.mcp[name]).unwrap()["cwd"].clone();
        assert_eq!(cwd("relative"), serde_json::to_value(work).unwrap());
        assert_eq!(cwd("absolute"), serde_json::to_value(absolute).unwrap());
        assert_eq!(cwd("default"), serde_json::Value::Null);
        let host = "skyhook".parse().unwrap();
        let login = |name: &str| config.login(&name.parse().unwrap(), &host);
        assert!(matches!(login("missing"), Err(LoginError::Unknown(_))));
        assert!(matches!(login("keyed"), Err(LoginError::Unmanaged { .. })));
        // A selected file is named in the login a request asks for.
        let AuthStatus::LoginRequired(required) = login("sub").unwrap().status().await.unwrap()
        else {
            panic!("nothing was saved");
        };
        assert_eq!(required.command.home, ConfigHome::File(path.clone()));
    }

    /// A candidate the generic loader reads but `Config` refuses falls back like any
    /// other refusal, and its diagnostic names the cause.
    #[tokio::test]
    async fn config_refusals_fall_back() {
        let f = Fixture::new();
        write(&f.home, CONFIG);
        for (bytes, cause) in [
            ("unrecognized: 1", "unknown field"),
            (
                "modes:\n  bad:\n    capabilities: [interactive]",
                "interactive",
            ),
            (
                "targets:\n  a:\n    type: ssh\n    host: a\n    via: b\n  b:\n    type: ssh\n    host: b\n    via: a",
                CYCLE,
            ),
        ] {
            write(&f.xdg, bytes);
            let resolved = f
                .resolve(None)
                .await
                .unwrap_or_else(|error| panic!("{bytes:?}: {error}"));
            assert_eq!(resolved.report.sources, std::slice::from_ref(&f.home));
            let diagnostics = &resolved.report.diagnostics;
            assert!(!diagnostics.is_empty(), "{bytes:?}");
            assert!(
                diagnostics
                    .iter()
                    .all(|diagnostic| diagnostic.path == f.xdg)
            );
            let message = &diagnostics[0].message;
            assert!(message.contains(cause), "{bytes:?}: {message}");
        }
    }

    /// A candidate is refused for what its own target says, a password among it, though
    /// the route it names waits on the workspace to define it.
    #[tokio::test]
    async fn a_candidate_with_an_unresolved_route_is_still_checked() {
        let f = Fixture::new();
        let target = "targets:\n  box:\n    type: ssh\n    host: box\n    via: jump\n";
        write(
            &f.xdg,
            format!("{CONFIG}{target}    ssh:\n      password: ''\n"),
        );
        write(&f.home, format!("{CONFIG}{target}"));
        write(&f.local, targets(&[("jump", None)]));
        let resolved = f.resolve(None).await.unwrap();
        assert_eq!(resolved.report.sources, [f.home.clone(), f.local.clone()]);
        let diagnostics = &resolved.report.diagnostics;
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].path, f.xdg);
        assert!(
            diagnostics[0]
                .message
                .contains("`targets.box.ssh.password`"),
            "{}",
            diagnostics[0].message
        );
    }

    /// A host capability is checked against the product a file is loaded to run: with no
    /// product, a grant of one is not refused.
    #[tokio::test]
    async fn host_capabilities_are_checked_only_against_a_product() {
        let f = Fixture::new();
        write(
            &f.xdg,
            format!("{CONFIG}modes:\n  research:\n    capabilities: [read, host.research]\n"),
        );
        let resolve = async |declared: Option<Vec<HostCapabilityDeclaration>>| {
            let candidates = vec![f.xdg.clone()];
            loader::resolve_paths::<HostConfig>(&f.workspace, None, candidates, declared.as_deref())
                .await
        };
        resolve(None).await.unwrap();
        let refused = resolve(Some(Vec::new())).await.unwrap_err().to_string();
        assert!(
            refused.contains("unknown capability `host.research`"),
            "{refused}"
        );
    }

    #[tokio::test]
    async fn failed_candidates_are_reported_if_no_usable_config_exists() {
        let f = Fixture::new();
        write(&f.xdg, "max_child_depth: not-a-number");
        write(&f.home, "[broken");
        let error = f.resolve(None).await.unwrap_err();
        assert_matches!(error, LayerResolutionError::Resolution { .. });
        assert_eq!(error.report().diagnostics.len(), 2);
        let message = error.to_string();
        assert!(message.contains(f.xdg.to_str().unwrap()));
        assert!(message.contains(f.home.to_str().unwrap()));
    }

    #[tokio::test]
    async fn invalid_workspace_is_always_fatal_and_retains_fallback_diagnostics() {
        let f = Fixture::new();
        write(&f.home, CONFIG);
        let invalid: [&[u8]; 15] = [
            b"[invalid",
            b"\xff",
            b"max_child_depth: wrong",
            b"providers:\n  local:\n    dialect: compatible\n    base_url: https://example.com/v1\n    codec: chat_completions\n    models:\n      main:\n        model: test\n        max_context: 4096\n        max_output: 0",
            b"targets:\n  bad:\n    type: ssh", // Atomic/incomplete target.
            // Null neither deletes entries nor applies defaults.
            b"max_child_depth: null",
            b"providers: {local: {dialect: compatible, codec: chat_completions, base_url: 'https://example.com/v1', models: null}}",
            b"modes: null",
            b"providers: null",
            b"providers: {local: null}",
            // Each layer must be a mapping.
            b"",
            b"# comment only",
            b"null",
            b"scalar",
            b"[one, two]",
        ];
        for bytes in invalid {
            write(&f.local, bytes);
            let error = f.resolve(None).await.unwrap_err();
            assert_eq!(error.report().diagnostics.len(), 1, "{bytes:?}");
            assert!(error.to_string().contains(f.local.to_str().unwrap()));
        }
        // An explicit file must be a nonempty mapping too.
        for document in ["", "null", "scalar", "{}"] {
            write(&f.local, document);
            assert!(
                resolve_layers::<HostConfig>(&f.workspace, Some(&f.local), None)
                    .await
                    .is_err()
            );
        }
        // An empty mapping overlay is a no-op.
        assert!(f.resolve(None).await.is_ok());
        // An empty user candidate configures nothing, so discovery moves on.
        write(&f.xdg, "{}");
        let resolved = f.resolve(None).await.unwrap();
        assert_eq!(resolved.report.sources[0], f.home);
        std::fs::remove_file(&f.local).unwrap();
        std::fs::create_dir(&f.local).unwrap();
        assert!(f.resolve(None).await.is_err());
    }

    #[tokio::test]
    async fn explicit_file_is_isolated_even_from_nonexistent_workspace() {
        let f = Fixture::new();
        write(&f.xdg, "[broken");
        write(&f.local, "[broken");
        write(&f.home, CONFIG);
        let resolved =
            resolve_layers::<HostConfig>(&f.workspace.join("does-not-exist"), Some(&f.home), None)
                .await
                .unwrap();
        assert_eq!(resolved.report.sources, std::slice::from_ref(&f.home));
        assert!(resolved.report.diagnostics.is_empty());
        assert_eq!(
            normalized(&resolved.config.config)["providers"]["local"]["models"]
                .as_object()
                .unwrap()
                .len(),
            1
        );
        write(&f.home, "[broken");
        let error = f.resolve(None).await.unwrap_err();
        assert!(error.to_string().contains(f.local.to_str().unwrap()));
        let error = resolve_paths(&f.workspace, Some(&f.home), Vec::new())
            .await
            .unwrap_err();
        assert_eq!(error.report().diagnostics.len(), 1);
        std::fs::remove_file(&f.home).unwrap();
        assert!(
            resolve_layers::<HostConfig>(&f.workspace, Some(&f.home), None)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn named_entries_replace_whole_and_keep_their_layer() {
        let f = Fixture::new();
        write(
            &f.xdg,
            format!(
                "{CONFIG}    api_key: {{env: USER_KEY}}\n  retained:\n    dialect: codex\n    codec: responses\nmcp:\n  inherited:\n    transport: stdio\n    start_command: [old]\n    cwd: user-work\n  user-default:\n    transport: stdio\n    start_command: [old]\n  changed:\n    transport: stdio\n    start_command: [old, arg]\n    startup_timeout_secs: 77\n    env: {{A: a}}\ntargets:\n  changed:\n    type: ssh\n    host: old\n    workspace: /old\n    via: retained\n    ssh:\n      user: old-user\n      port: 2222\n      key: /old-key\n  retained:\n    type: ssh\n    host: other\nmodes:\n  changed:\n    capabilities: [read, exec]\n    instructions: old\n  retained:\n    capabilities: [read]\n"
            ),
        );
        write(
            &f.local,
            "providers:\n  local:\n    dialect: compatible\n    codec: chat_completions\n    base_url: https://other.example/v1\n  added:\n    dialect: codex\n    codec: responses\nmcp:\n  changed:\n    transport: stdio\n    start_command: [new]\n    cwd: workspace-work\n  workspace-default:\n    transport: stdio\n    start_command: [new]\ntargets:\n  remote:\n    type: ssh\n    host: host\n    workspace: remote-work\n    ssh:\n      key: origin-key\n  changed:\n    type: ssh\n    host: new\ndefault_mode: retained\nmodes:\n  changed:\n    capabilities: []\n",
        );
        let resolved = f.resolve(None).await.unwrap();
        assert_eq!(resolved.report.sources, [f.xdg.clone(), f.local.clone()]);
        assert!(resolved.report.diagnostics.is_empty());
        let config = &resolved.config.config;
        // A replaced entry keeps its place; one only the workspace names follows.
        let providers: Vec<_> = config.providers.keys().map(|name| name.as_str()).collect();
        assert_eq!(providers, ["local", "retained", "added"]);
        assert!(config.providers["local"].common.api_key.is_none());
        let models = &normalized(config)["providers"]["local"]["models"];
        assert!(models.as_object().unwrap().is_empty());
        let user = f.xdg.parent().unwrap();
        let workspace = f.local.parent().unwrap();
        let home = |name: &str| config.providers[name].home.clone();
        assert_eq!(home("local"), ConfigHome::File(f.local.clone()));
        assert_eq!(home("retained"), ConfigHome::File(f.xdg.clone()));
        assert_eq!(home("added"), ConfigHome::File(f.local.clone()));
        let raw = |name: &str| serde_json::to_value(&config.mcp[name]).unwrap();
        let path = |path: PathBuf| serde_json::to_value(path).unwrap();
        assert_eq!(raw("inherited")["cwd"], path(user.join("user-work")));
        let changed = raw("changed");
        assert_eq!(changed["cwd"], path(workspace.join("workspace-work")));
        assert_eq!(changed["start_command"], serde_json::json!(["new"]));
        assert_eq!(changed["env"], serde_json::json!({}));
        // An omitted cwd stays omitted in either layer: it means the session workspace.
        assert!(raw("user-default").get("cwd").is_none());
        assert!(raw("workspace-default").get("cwd").is_none());
        // Only MCP cwd follows its file; these keep their own meaning.
        let remote = &config.targets.entries["remote"];
        assert_eq!(remote.workspace, "remote-work".into());
        let origin_key = SshAuth::Key {
            path: "origin-key".into(),
            password: None,
        };
        assert_eq!(remote.ssh.auth, origin_key);
        assert!(config.targets.entries.contains_key("retained"));
        let changed = &config.targets.entries["changed"];
        assert_eq!(
            (&*changed.host, &changed.workspace, &changed.via),
            ("new", &".".into(), &None)
        );
        assert_eq!(
            (&changed.ssh.user, changed.ssh.port, &changed.ssh.auth),
            (&None, None, &SshAuth::Agent)
        );
        assert_eq!(
            Vec::from_iter(config.modes.keys().map(|mode| mode.as_str())),
            ["general", "changed", "retained"]
        );
        assert_eq!(config.default_mode.as_str(), "retained");
        let changed = &config.modes["changed"];
        assert!(changed.capabilities.set().is_empty() && changed.instructions.is_none());
        // Defaults apply after merging, to the entry that won.
        let startup = |name: &str| config.mcp[name].startup_timeout();
        assert_eq!(startup("changed"), startup("inherited"));
        // Alone, an explicit file names a mode nothing declares, which sealing refuses.
        let alone = resolve_layers::<HostConfig>(&f.workspace, Some(&f.local), None);
        let alone = alone.await.unwrap().config.config;
        assert_eq!(
            alone.providers["added"].home,
            ConfigHome::File(f.local.clone())
        );
        let refused = alone.into_runtime().err().unwrap().to_string();
        assert!(
            refused.contains("`default_mode`: default_mode is not a declared mode"),
            "{refused}"
        );
        // A partial entry inherits no required field from the one it replaces.
        for partial in [
            "targets:\n  changed:\n    host: incomplete\n",
            "modes:\n  changed:\n    instructions: new\n",
        ] {
            write(&f.local, partial);
            assert!(f.resolve(None).await.is_err(), "{partial}");
        }
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
        let mut resolved = f.resolve(None).await.unwrap();
        assert!(resolved.report.diagnostics.is_empty());
        let dump = resolved.config.config.to_yaml().unwrap();
        assert!(dump.contains("env: SKYHOOK_NONEXISTENT_TEST_RESOLUTION_KEY"));
        resolved.config.config.max_child_depth = 9;
        resolved.config.config.modes[0].capabilities = CapabilitySet::empty().try_into().unwrap();
        let config = Config::from_yaml(&resolved.config.config.to_yaml().unwrap()).unwrap();
        assert_eq!(config.max_child_depth, 9);
        assert!(config.modes[0].capabilities.set().is_empty());
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn model_order_survives_merging_dumping_and_runtime_admission() {
        let f = Fixture::new();
        let model = |name: &str, id: &str| {
            format!(
                "      '{name}':\n        model: '{id}'\n        max_context: 4096\n        max_output: 512\n"
            )
        };
        write(
            &f.xdg,
            format!("{PROVIDER}    models:\n{}", model("user", "user")),
        );
        let models = [
            model("true", "42"),
            model("42", "42"),
            model("null", "true"),
        ]
        .concat();
        write(&f.local, format!("{PROVIDER}    models:\n{models}"));
        let resolved = f.resolve(None).await.unwrap();
        let config = Config::from_yaml(&resolved.config.config.to_yaml().unwrap()).unwrap();
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
        let resolved = f.resolve(None).await.unwrap();
        assert!(resolved.report.diagnostics.is_empty());
        let targets = &resolved.config.config.targets.entries;
        let hops = targets
            .iter()
            .map(|(name, target)| (&**name, target.via.as_deref()));
        let expected = [("a", Some("b")), ("b", Some("c")), ("c", None)];
        assert_eq!(hops.collect::<Vec<_>>(), expected);
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
            let error = resolve_paths(&f.workspace, Some(&f.xdg), Vec::new())
                .await
                .unwrap_err();
            assert!(error.to_string().contains(CYCLE));
            let report = error.report();
            assert!(report.sources.is_empty() && !report.diagnostics.is_empty());
            assert!(
                report
                    .diagnostics
                    .iter()
                    .all(|diagnostic| diagnostic.path == f.xdg)
            );
        }
        // Merged cycles are fatal, including when a workspace replaces a definition.
        for user in [
            targets(&[("a", Some("b"))]),
            targets(&[("a", Some("b")), ("b", None)]),
        ] {
            let f = Fixture::new();
            write(&f.xdg, user);
            write(&f.local, targets(&[("b", Some("a"))]));
            let error = f.resolve(None).await.unwrap_err();
            let headline = format!(
                "the config.yaml merged from {}, {} is invalid",
                f.xdg.display(),
                f.local.display()
            );
            assert_eq!(error.to_string().lines().next(), Some(headline.as_str()));
            assert!(error.to_string().contains(CYCLE));
            let report = error.report();
            assert_eq!(report.sources, [f.xdg.clone(), f.local.clone()]);
            // Each target on the cycle is refused in the file that defines it.
            let files = Vec::from_iter(report.diagnostics.iter().map(|d| &d.path));
            assert_eq!(files, [&f.local, &f.xdg]);
        }
    }
}
