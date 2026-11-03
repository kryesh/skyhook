//! The main configuration's admission and source-relative values.

use std::path::Path;

use super::{Config, ConfigError, LayerSources, LayeredConfig, ResolvedConfig, resolve_layers};
use crate::provider::dialect::ConfigHome;

impl Config {
    /// Resolve without constructing providers, reading credentials, or executing commands.
    /// An explicit file disables all user/workspace configuration discovery.
    pub async fn resolve(
        workspace: &Path,
        explicit: Option<&Path>,
    ) -> Result<ResolvedConfig, ConfigError> {
        Ok(resolve_layers::<Self>(workspace, explicit).await?)
    }
}

impl LayeredConfig for Config {
    type Error = ConfigError;

    const FILE_NAME: &'static str = "config.yaml";
    const NAMED_ENTRIES: &'static [&'static str] = &["providers", "modes", "targets", "mcp"];

    fn admit_layer(&mut self, sources: &LayerSources) -> Result<(), Self::Error> {
        self.admit()?;
        for (name, entry) in &mut self.providers {
            let file = sources
                .entry("providers", name.as_str())
                .expect("provider has a source");
            entry.home = ConfigHome::File(file.to_path_buf());
        }
        // Only MCP cwd follows its file. session_root remains process-relative;
        // target workspaces and SSH keys belong to their execution hosts.
        for (name, server) in &mut self.mcp {
            let file = sources.entry("mcp", name).expect("MCP server has a source");
            let directory = file.parent().expect("absolute config path has a parent");
            server
                .resolve_cwd(directory)
                .map_err(|error| ConfigError::Mcp {
                    name: name.clone(),
                    error,
                })?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::assert_matches;
    // Loader tests use injected candidate paths rather than mutating process environment.

    use super::*;
    use crate::config::LayerResolutionError;
    use crate::{
        config::loader::{self, tests::write},
        target::TargetAuth,
        tool::policy::CapabilitySet,
    };
    use serde_json::Value;
    use std::path::PathBuf;

    async fn resolve_paths(
        workspace: &Path,
        explicit: Option<&Path>,
        candidates: Vec<PathBuf>,
    ) -> Result<ResolvedConfig, LayerResolutionError> {
        loader::resolve_paths::<Config>(workspace, explicit, candidates).await
    }

    const PROVIDER: &str = "providers:\n  local:\n    dialect: compatible\n    base_url: https://example.com/v1\n    codec: chat_completions\n";
    const CONFIG: &str = "providers:\n  local:\n    dialect: compatible\n    base_url: https://example.com/v1\n    codec: chat_completions\n    models:\n      main:\n        model: test\n        max_context: 4096\n        max_output: 512\n";

    type Fixture = loader::tests::Fixture<Config>;

    fn normalized(config: &Config) -> Value {
        crate::yaml::from_str(&config.to_yaml().unwrap()).unwrap()
    }

    /// A candidate the generic loader reads but `Config` refuses falls back like any
    /// other refusal, and its diagnostic names the cause.
    #[tokio::test]
    async fn config_refusals_fall_back_without_leaking_fields() {
        let f = Fixture::new();
        write(&f.home, CONFIG);
        for (bytes, cause) in [
            ("session_root: leaked\nunrecognized: 1", "unknown field"),
            (
                "session_root: leaked\nmodes:\n  bad:\n    capabilities: [interactive]",
                "interactive",
            ),
            (
                "session_root: leaked\ntargets:\n  a:\n    type: ssh\n    host: a\n    via: b\n  b:\n    type: ssh\n    host: b\n    via: a",
                CYCLE,
            ),
        ] {
            write(&f.xdg, bytes);
            let resolved = f
                .resolve()
                .await
                .unwrap_or_else(|error| panic!("{bytes:?}: {error}"));
            assert_eq!(resolved.report.sources, std::slice::from_ref(&f.home));
            let diagnostics = &resolved.report.diagnostics;
            assert_eq!(diagnostics.len(), 1, "{bytes:?}");
            assert_eq!(diagnostics[0].path, f.xdg);
            let message = &diagnostics[0].message;
            assert!(message.contains(cause), "{bytes:?}: {message}");
            let config = &resolved.config;
            assert!(config.session_root.is_none(), "leaked fields: {bytes:?}");
            assert!(config.targets.entries.is_empty(), "{bytes:?}");
        }
    }

    #[tokio::test]
    async fn failed_candidates_are_reported_if_no_usable_config_exists() {
        let f = Fixture::new();
        write(&f.xdg, "max_child_depth: not-a-number");
        write(&f.home, "[broken");
        let error = f.resolve().await.unwrap_err();
        assert_matches!(error, LayerResolutionError::Resolution { .. });
        assert_eq!(error.report().diagnostics.len(), 2);
        let message = error.to_string();
        assert!(message.contains(f.xdg.to_str().unwrap()));
        assert!(message.contains(f.home.to_str().unwrap()));
        assert!(!message.contains("max_child_depth:") && !message.contains("[broken"));
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
            let error = f.resolve().await.unwrap_err();
            assert_eq!(error.report().diagnostics.len(), 1, "{bytes:?}");
            assert!(error.to_string().contains(f.local.to_str().unwrap()));
        }
        // An explicit file must be a nonempty mapping too.
        for document in ["", "null", "scalar", "{}"] {
            write(&f.local, document);
            assert!(Config::resolve(&f.workspace, Some(&f.local)).await.is_err());
        }
        // An empty mapping overlay is a no-op.
        assert!(f.resolve().await.is_ok());
        // An empty user candidate configures nothing, so discovery moves on.
        write(&f.xdg, "{}");
        let resolved = f.resolve().await.unwrap();
        assert_eq!(resolved.report.sources[0], f.home);
        std::fs::remove_file(&f.local).unwrap();
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
        let error = resolve_paths(&f.workspace, Some(&f.home), Vec::new())
            .await
            .unwrap_err();
        assert_eq!(error.report().diagnostics.len(), 1);
        assert!(!error.to_string().contains(f.xdg.to_str().unwrap()));
        std::fs::remove_file(&f.home).unwrap();
        assert!(Config::resolve(&f.workspace, Some(&f.home)).await.is_err());
    }

    #[tokio::test]
    async fn named_entries_replace_whole_and_keep_their_layer() {
        let f = Fixture::new();
        write(
            &f.xdg,
            format!(
                "session_root: sessions\n{CONFIG}    api_key: {{env: USER_KEY}}\n  retained:\n    dialect: codex\n    codec: responses\nmcp:\n  inherited:\n    transport: stdio\n    start_command: [old]\n    cwd: user-work\n  user-default:\n    transport: stdio\n    start_command: [old]\n  changed:\n    transport: stdio\n    start_command: [old, arg]\n    startup_timeout_secs: 77\n    env: {{A: a}}\ntargets:\n  changed:\n    type: ssh\n    host: old\n    workspace: /old\n    via: retained\n    ssh:\n      user: old-user\n      port: 2222\n      auth: {{kind: key, path: /old-key}}\n  retained:\n    type: ssh\n    host: other\nmodes:\n  changed:\n    capabilities: [read, exec]\n    instructions: old\n  retained:\n    capabilities: [read]\n"
            ),
        );
        write(
            &f.local,
            "providers:\n  local:\n    dialect: compatible\n    codec: chat_completions\n    base_url: https://other.example/v1\n  added:\n    dialect: codex\n    codec: responses\nmcp:\n  changed:\n    transport: stdio\n    start_command: [new]\n    cwd: workspace-work\n  workspace-default:\n    transport: stdio\n    start_command: [new]\ntargets:\n  remote:\n    type: ssh\n    host: host\n    workspace: remote-work\n    ssh:\n      auth: {kind: key, path: origin-key}\n  changed:\n    type: ssh\n    host: new\ndefault_mode: retained\nmodes:\n  changed:\n    capabilities: []\n",
        );
        // A legacy `config.toml` beside either layer is never read.
        for layer in [&f.xdg, &f.local] {
            write(&layer.with_file_name("config.toml"), "[broken");
        }
        let resolved = f.resolve().await.unwrap();
        assert_eq!(resolved.report.sources, [f.xdg.clone(), f.local.clone()]);
        assert!(resolved.report.diagnostics.is_empty());
        let config = &resolved.config;
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
        let raw = |name: &str| crate::mcp::RawMcpServerConfig::from(config.mcp[name].clone());
        assert_eq!(raw("inherited").cwd, Some(user.join("user-work")));
        let changed = raw("changed");
        assert_eq!(changed.cwd, Some(workspace.join("workspace-work")));
        assert_eq!(changed.start_command.unwrap(), ["new"]);
        assert!(changed.env.is_empty());
        // An omitted cwd stays omitted in either layer: it means the session workspace.
        assert_eq!(raw("user-default").cwd, None);
        assert_eq!(raw("workspace-default").cwd, None);
        // Only MCP cwd follows its file; these keep their own meaning.
        assert_eq!(config.session_root, Some(PathBuf::from("sessions")));
        let remote = &config.targets.entries["remote"];
        assert_eq!(remote.workspace, PathBuf::from("remote-work"));
        let origin_key = TargetAuth::Key {
            path: "origin-key".into(),
        };
        assert_eq!(remote.ssh.auth, origin_key);
        assert!(config.targets.entries.contains_key("retained"));
        let changed = &config.targets.entries["changed"];
        assert_eq!(
            (&*changed.host, &changed.workspace, &changed.via),
            ("new", &PathBuf::from("."), &None)
        );
        assert_eq!(
            (&changed.ssh.user, changed.ssh.port, &changed.ssh.auth),
            (&None, None, &TargetAuth::Default)
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
        let explicit = Config::resolve(&f.workspace, Some(&f.local)).await.unwrap();
        assert_eq!(
            explicit.config.providers["added"].home,
            ConfigHome::File(f.local.clone())
        );
        // A partial entry inherits no required field from the one it replaces.
        for partial in [
            "targets:\n  changed:\n    host: incomplete\n",
            "modes:\n  changed:\n    instructions: new\n",
        ] {
            write(&f.local, partial);
            assert!(f.resolve().await.is_err(), "{partial}");
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
        let mut resolved = f.resolve().await.unwrap();
        assert!(resolved.report.diagnostics.is_empty());
        let dump = resolved.config.to_yaml().unwrap();
        assert!(dump.contains("env: SKYHOOK_NONEXISTENT_TEST_RESOLUTION_KEY"));
        resolved.config.max_child_depth = 9;
        resolved.config.modes[0].capabilities = CapabilitySet::empty().try_into().unwrap();
        let config = Config::from_yaml(&resolved.config.to_yaml().unwrap()).unwrap();
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
        let resolved = f.resolve().await.unwrap();
        let config = Config::from_yaml(&resolved.config.to_yaml().unwrap()).unwrap();
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
        let resolved = f.resolve().await.unwrap();
        assert!(resolved.report.diagnostics.is_empty());
        let definitions = resolved.config.targets.definitions().unwrap();
        let registry = crate::target::TargetRegistry::from_definitions(definitions).unwrap();
        let route = registry.route(&"a".parse().unwrap()).unwrap();
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
            let error = resolve_paths(&f.workspace, Some(&f.xdg), Vec::new())
                .await
                .unwrap_err();
            assert!(error.to_string().contains(CYCLE));
            let report = error.report();
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
            let report = error.report();
            assert_eq!(report.sources, [f.xdg, f.local.clone()]);
            assert_eq!(report.diagnostics.len(), 1);
            assert_eq!(report.diagnostics[0].path, f.local);
        }
    }
}
