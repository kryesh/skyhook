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
    store::{PocPolicy, RunOptions},
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

impl clap::ValueEnum for PocPolicy {
    fn value_variants<'a>() -> &'a [Self] {
        &Self::ALL
    }
    fn to_possible_value(&self) -> Option<clap::builder::PossibleValue> {
        Some(clap::builder::PossibleValue::new(self.as_str()))
    }
}

// Defaults are per field, so the embedded modes are built once: as the default, or as
// the base the configured modes replace entries of.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SecurityConfig {
    #[serde(default)]
    pub models: IndexMap<ModelRef, ModelAssignment>,
    #[serde(default)]
    pub mcp: BTreeMap<String, McpServerConfig>,
    /// Named entries replace the embedded mode of the same name whole.
    #[serde(default = "presets::modes", deserialize_with = "over_presets")]
    pub modes: IndexMap<ModeName, Mode>,
    /// How many agents a new run's scheduler runs at once; the run persists what it starts
    /// with.
    #[serde(default)]
    pub workers: Option<NonZeroU16>,
    #[serde(default)]
    pub shortlist: Option<Severity>,
    /// Absence is distinct from an explicit ask: headless defaults to no.
    #[serde(default)]
    pub poc: Option<PocPolicy>,
    #[serde(skip)]
    sources: LayerSources,
}

fn over_presets<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<IndexMap<ModeName, Mode>, D::Error> {
    let mut modes = presets::modes();
    modes.extend(IndexMap::<ModeName, Mode>::deserialize(deserializer)?);
    Ok(modes)
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            models: IndexMap::new(),
            mcp: BTreeMap::new(),
            modes: presets::modes(),
            workers: None,
            shortlist: None,
            poc: None,
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
    #[error("poc: ask requires an interactive session; use --poc yes or --poc no in headless mode")]
    HeadlessAsk,
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

    pub fn poc_policy(&self, cli: Option<PocPolicy>, headless: bool) -> Result<PocPolicy, Error> {
        let policy = cli.or(self.poc).unwrap_or(if headless {
            PocPolicy::No
        } else {
            PocPolicy::Ask
        });
        if headless && policy == PocPolicy::Ask {
            return Err(Error::HeadlessAsk);
        }
        Ok(policy)
    }

    /// Replace ordinary modes with the security modes, merge MCP servers, and rewrite
    /// model offers. Nothing changes when an assigned model is not configured.
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
        config.modes = self.modes.clone();
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
    use super::*;
    use serde_json::json;
    use skyhook::mcp::config::RawMcpServerConfig;

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
    async fn explicit_file_records_model_source_normalizes_mcp_and_replaces_named_modes() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("selected.yaml");
        std::fs::write(
            &path,
            r#"
models:
  test/missing:
    roles: [validator]
mcp:
  research:
    transport: stdio
    start_command: [research-server]
    cwd: ../server
modes:
  report:
    capabilities: [read, targets]
"#,
        )
        .unwrap();
        let security = SecurityConfig::resolve(Path::new("/workspace-is-not-read"), Some(&path))
            .await
            .unwrap()
            .config;
        let raw = RawMcpServerConfig::from(security.mcp["research"].clone());
        assert_eq!(raw.cwd, Some(root.path().join("../server")));
        let report = &security.modes["report"];
        assert_eq!(
            report.capabilities.iter().cloned().collect::<Vec<_>>(),
            [
                skyhook::tool::policy::Capability::Read,
                skyhook::tool::policy::Capability::Targets
            ]
        );
        assert!(security.modes.contains_key("orchestrate"));
        let mut config = shared();
        let before = serde_json::to_value(&config).unwrap();
        let error = security.apply(&mut config).unwrap_err();
        assert!(matches!(&error, Error::ModelAssignment { source, .. } if source == &path));
        assert_eq!(serde_json::to_value(config).unwrap(), before);
    }

    #[test]
    fn policy_defaults_cli_precedence_and_headless_ask_rejection() {
        let mut config = SecurityConfig::default();
        assert_eq!(config.poc_policy(None, false).unwrap(), PocPolicy::Ask);
        assert_eq!(config.poc_policy(None, true).unwrap(), PocPolicy::No);
        config.poc = Some(PocPolicy::Ask);
        assert!(matches!(
            config.poc_policy(None, true),
            Err(Error::HeadlessAsk)
        ));
        assert_eq!(
            config.poc_policy(Some(PocPolicy::Yes), true).unwrap(),
            PocPolicy::Yes
        );
        config.poc = Some(PocPolicy::Yes);
        assert_eq!(
            config.poc_policy(Some(PocPolicy::No), false).unwrap(),
            PocPolicy::No
        );
        assert!(matches!(
            config.poc_policy(Some(PocPolicy::Ask), true),
            Err(Error::HeadlessAsk)
        ));
    }
}
