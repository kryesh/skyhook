//! Security-only settings layered by the shared loader, independently of config.yaml.

use std::{collections::BTreeMap, num::NonZeroU16, path::Path};

use indexmap::{IndexMap, IndexSet};
use serde::{Deserialize, Serialize};
use skyhook::{
    Prose,
    config::{
        Config, ConfigError, LayerResolutionError, LayerSources, LayeredConfig, ResolvedConfig,
        resolve_layers,
    },
    mcp::config::{McpConfigError, McpServerConfig},
    provider::profile::ModelRef,
    tool::policy::{Mode, ModeName},
};

use crate::{
    cvss::Severity,
    presets,
    store::{LivePolicy, RunOptions},
};

skyhook::named_enum! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
    pub enum ModelRole {
        Orchestrator = "orchestrator",
        Explorer = "explorer",
        Validator = "validator",
        Environment = "environment",
        Poc = "poc",
        Reporter = "reporter",
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelAssignment {
    pub roles: IndexSet<ModelRole>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<Prose>,
}

impl clap::ValueEnum for LivePolicy {
    fn value_variants<'a>() -> &'a [Self] {
        &Self::ALL
    }
    fn to_possible_value(&self) -> Option<clap::builder::PossibleValue> {
        Some(clap::builder::PossibleValue::new(self.as_str()))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SecurityConfig {
    #[serde(default)]
    pub models: IndexMap<ModelRef, ModelAssignment>,
    #[serde(default)]
    pub mcp: BTreeMap<String, McpServerConfig>,
    /// Named entries replace the embedded mode of the same name whole.
    #[serde(default)]
    pub modes: IndexMap<ModeName, Mode>,
    /// How many agents a new run's scheduler runs at once; the run persists what it starts
    /// with.
    #[serde(default)]
    pub workers: Option<NonZeroU16>,
    #[serde(default)]
    pub shortlist: Option<Severity>,
    /// Absence is distinct from an explicit ask: headless defaults to no.
    #[serde(default)]
    pub live: Option<LivePolicy>,
    #[serde(skip)]
    sources: LayerSources,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            models: IndexMap::new(),
            mcp: BTreeMap::new(),
            modes: IndexMap::new(),
            workers: None,
            shortlist: None,
            live: None,
            sources: LayerSources::default(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{}: models.{model}: {error}", source.display())]
    ModelAssignment {
        source: std::path::PathBuf,
        model: ModelRef,
        #[source]
        error: Box<ConfigError>,
    },
    #[error("mcp.{name}: {error}")]
    Mcp {
        name: String,
        #[source]
        error: McpConfigError,
    },
    #[error(
        "live: ask requires an interactive session; use --live yes or --live no in headless mode"
    )]
    HeadlessAsk,
    #[error("--live sets a new session's policy; a resumed run keeps the one it started with")]
    LiveOnResume,
    #[error(transparent)]
    Shared(#[from] ConfigError),
}

impl LayeredConfig for SecurityConfig {
    type Error = Error;

    const FILE_NAME: &'static str = "security.yaml";
    const NAMED_ENTRIES: &'static [&'static str] = &["models", "modes", "mcp"];

    fn missing() -> Option<Self> {
        Some(Self::default())
    }

    fn admit_layer(&mut self, sources: &LayerSources) -> Result<(), Error> {
        for (name, server) in &mut self.mcp {
            let file = sources.entry("mcp", name).expect("MCP server has a source");
            let directory = file.parent().expect("absolute config path has a parent");
            server.resolve_cwd(directory).map_err(|error| Error::Mcp {
                name: name.clone(),
                error,
            })?;
        }
        self.sources = sources.clone();
        Ok(())
    }
}

impl SecurityConfig {
    pub async fn resolve(
        workspace: &Path,
        explicit: Option<&Path>,
    ) -> Result<ResolvedConfig<Self>, LayerResolutionError> {
        resolve_layers(workspace, explicit).await
    }

    /// A new run's options: the flags', then the configured, then the defaults.
    pub fn run_options(
        &self,
        shortlist: Option<Severity>,
        workers: Option<NonZeroU16>,
    ) -> RunOptions {
        let defaults = RunOptions::default();
        RunOptions {
            threshold: shortlist.or(self.shortlist).unwrap_or(defaults.threshold),
            workers: workers.or(self.workers).unwrap_or(defaults.workers),
        }
    }

    /// A new run's policy: the flag's, the configuration's, or the default (interactive: ask;
    /// headless: no).
    pub fn live_policy(&self, cli: Option<LivePolicy>, headless: bool) -> LivePolicy {
        cli.or(self.live).unwrap_or(if headless {
            LivePolicy::No
        } else {
            LivePolicy::Ask
        })
    }

    /// Replace ordinary modes with the security modes, merge MCP servers, and rewrite model
    /// offers. Nothing changes when an assigned model is not configured.
    pub fn apply(&self, config: &mut Config) -> Result<(), Error> {
        for model in self.models.keys() {
            config
                .model_profile(model)
                .map_err(|error| Error::ModelAssignment {
                    source: self
                        .sources
                        .entry("models", &model.to_string())
                        .unwrap_or_else(|| Path::new(Self::FILE_NAME))
                        .to_path_buf(),
                    model: model.clone(),
                    error: Box::new(error),
                })?;
        }
        if let Some((model, _)) = self
            .models
            .iter()
            .find(|(_, assignment)| assignment.roles.contains(&ModelRole::Orchestrator))
        {
            config.default_model = Some(model.clone());
        }
        config.modes = presets::configured(&self.modes);
        config.default_mode = presets::root();
        config.mcp.extend(self.mcp.clone());
        let hints: Vec<(ModelRef, Option<Prose>)> = config
            .models()
            .map(|(model, profile)| {
                let hint = self
                    .models
                    .get(&model)
                    .filter(|assignment| !assignment.roles.is_empty())
                    .map(|assignment| {
                        let roles = assignment
                            .roles
                            .iter()
                            .map(|role| role.as_str())
                            .collect::<Vec<_>>()
                            .join(", ");
                        let mut text = format!("Roles: {roles}.");
                        if let Some(hint) = assignment.hint.as_ref().or(profile.hint.as_ref()) {
                            text.push(' ');
                            text.push_str(hint.as_str());
                        }
                        text.parse().expect("role prefix is nonblank")
                    });
                (model, hint)
            })
            .collect();
        for (model, hint) in hints {
            config.model_profile_mut(&model)?.hint = hint;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use super::*;
    use serde_json::json;

    fn shared() -> Config {
        Config::from_yaml(
            r#"
providers:
  test:
    dialect: compatible
    codec: chat_completions
    base_url: https://example.test/v1
    models:
      first:
        model: first
        max_context: 4096
        max_output: 512
        hint: Existing hint.
      second:
        model: second
        max_context: 4096
        max_output: 512
        hint: Another hint.
      unassigned:
        model: third
        max_context: 4096
        max_output: 512
        hint: Not offered.
"#,
        )
        .unwrap()
    }

    fn parse(value: serde_json::Value) -> SecurityConfig {
        let mut config: SecurityConfig = serde_json::from_value(value).unwrap();
        config.admit_layer(&LayerSources::default()).unwrap();
        config
    }

    #[test]
    fn assignments_rewrite_hints_select_the_orchestrator_and_root_the_modes() {
        let security = parse(json!({"models": {
            "test/second": {"roles": ["orchestrator", "validator"], "hint": "Prefer deep review."},
            "test/first": {"roles": ["explorer", "orchestrator"]}
        }}));
        let mut config = shared();
        config.default_model = Some("test/first".parse().unwrap());
        security.apply(&mut config).unwrap();
        assert_eq!(config.default_model, Some("test/second".parse().unwrap()));
        assert_eq!(config.default_mode, presets::root());
        for (model, hint) in [
            (
                "test/second",
                Some("Roles: orchestrator, validator. Prefer deep review."),
            ),
            (
                "test/first",
                Some("Roles: explorer, orchestrator. Existing hint."),
            ),
            ("test/unassigned", None),
        ] {
            assert_eq!(
                config
                    .model_profile(&model.parse().unwrap())
                    .unwrap()
                    .hint
                    .as_ref()
                    .map(Prose::as_str),
                hint
            );
        }
        assert!(!config.modes.contains_key("general"));
        assert!(config.modes.contains_key(&presets::root()));
        config.into_runtime().unwrap();

        let mut config = shared();
        config.default_model = Some("test/first".parse().unwrap());
        parse(json!({"models": {"test/first": {"roles": [], "hint": "Ignored."}}}))
            .apply(&mut config)
            .unwrap();
        assert_eq!(config.default_model, Some("test/first".parse().unwrap()));
        assert!(config.models().all(|(_, profile)| profile.hint.is_none()));
    }

    #[tokio::test]
    async fn explicit_file_records_model_source_and_replaces_named_modes() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("selected.yaml");
        std::fs::write(
            &path,
            r#"
models:
  test/missing:
    roles: [validator]
modes:
  report:
    capabilities: [read, targets]
mcp:
  security:
    transport: stdio
    start_command: [/bin/true]
"#,
        )
        .unwrap();
        let mut security =
            SecurityConfig::resolve(Path::new("/workspace-is-not-read"), Some(&path))
                .await
                .unwrap()
                .config;
        let modes = presets::configured(&security.modes);
        assert_eq!(
            modes["report"]
                .capabilities
                .iter()
                .cloned()
                .collect::<Vec<_>>(),
            [
                skyhook::tool::policy::Capability::Read,
                skyhook::tool::policy::Capability::Targets
            ]
        );
        assert!(modes.contains_key("orchestrate"));
        let mut config = shared();
        let before = serde_json::to_value(&config).unwrap();
        let error = security.apply(&mut config).unwrap_err();
        assert_matches!(&error, Error::ModelAssignment { source, .. } if source == &path);
        assert_eq!(serde_json::to_value(&config).unwrap(), before);
        security.models.clear();
        security.apply(&mut config).unwrap();
        assert!(config.mcp.contains_key("security"));
    }

    #[test]
    fn policy_defaults_and_cli_precedence() {
        let mut config = SecurityConfig::default();
        assert_eq!(config.run_options(None, None), RunOptions::default());
        config.shortlist = Some(Severity::High);
        config.workers = NonZeroU16::new(3);
        let options = |shortlist, workers| {
            let options = config.run_options(shortlist, workers);
            (options.threshold, options.workers.get())
        };
        assert_eq!(options(None, None), (Severity::High, 3));
        let flags = options(Some(Severity::Medium), NonZeroU16::new(2));
        assert_eq!(flags, (Severity::Medium, 2));
        assert_eq!(config.live_policy(None, false), LivePolicy::Ask);
        assert_eq!(config.live_policy(None, true), LivePolicy::No);
        config.live = Some(LivePolicy::Ask);
        assert_eq!(config.live_policy(None, true), LivePolicy::Ask);
        assert_eq!(
            config.live_policy(Some(LivePolicy::Yes), true),
            LivePolicy::Yes
        );
        config.live = Some(LivePolicy::Yes);
        assert_eq!(
            config.live_policy(Some(LivePolicy::No), false),
            LivePolicy::No
        );
    }
}
