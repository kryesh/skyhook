//! Security-only settings, which a host reads beside the shared configuration and hands in.

use skyhook::product::Attendance;
use std::{
    collections::BTreeMap,
    fmt::Display,
    num::NonZeroU16,
    path::{Path, PathBuf},
};

use indexmap::{IndexMap, IndexSet};
use serde::Serialize;
use skyhook::{
    config::mode::{Mode, ModeName},
    config::{Config, ConfigError},
    mcp::config::McpServerConfig,
    provider::model::ModelRef,
};
use skyhook_base::{
    decode::{Decode, Diagnostic, Path as Steps, Segment, Source},
    enums::NamedEnum,
    newtype::Prose,
};

use crate::{
    cvss::Severity,
    modes,
    store::{LivePolicy, RunOptions},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, NamedEnum)]
#[serde(rename_all = "snake_case")]
pub enum ModelRole {
    Orchestrator,
    Explorer,
    Validator,
    Environment,
    Poc,
    Reporter,
}

#[derive(Clone, Debug, Serialize, Decode)]
pub struct ModelAssignment {
    pub roles: IndexSet<ModelRole>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<Prose>,
    /// Where the assignment is written, named when it is refused.
    #[serde(skip)]
    #[decode(source)]
    pub at: Source,
}

/// The product's own settings as a host read them; a document that sets none is the
/// default.
#[derive(Clone, Debug, Default, Serialize, Decode)]
#[serde(default)]
pub struct SecurityConfig {
    pub models: IndexMap<ModelRef, ModelAssignment>,
    pub mcp: BTreeMap<String, McpServerConfig>,
    /// Named entries replace the embedded mode of the same name whole.
    pub modes: IndexMap<ModeName, Mode>,
    /// How many agents a new run's scheduler runs at once; the run persists what it starts
    /// with.
    pub workers: Option<NonZeroU16>,
    pub shortlist: Option<Severity>,
    /// Absence is distinct from an explicit ask: a batch job defaults to no.
    pub live: Option<LivePolicy>,
    /// Where the host read the settings, named in what the product warns of them.
    #[serde(skip)]
    pub file: Option<PathBuf>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    /// An assignment naming no configured model, where it is written.
    #[error("{0}")]
    ModelAssignment(Diagnostic),
    #[error(transparent)]
    Shared(#[from] ConfigError),
}

/// `message` as said of `file`, when the host named one.
fn named(file: Option<&Path>, message: impl Display) -> String {
    match file {
        Some(file) => format!("{}: {message}", file.display()),
        None => message.to_string(),
    }
}

impl SecurityConfig {
    /// A new run's options: the flags', then the configured, then the defaults; an
    /// attended launch's live policy defaults to ask, a batch job's to no.
    pub(crate) fn run_options(
        &self,
        shortlist: Option<Severity>,
        workers: Option<NonZeroU16>,
        live: Option<LivePolicy>,
        attendance: Attendance,
    ) -> RunOptions {
        let defaults = RunOptions::default();
        RunOptions {
            threshold: shortlist.or(self.shortlist).unwrap_or(defaults.threshold),
            workers: workers.or(self.workers).unwrap_or(defaults.workers),
            live: live.or(self.live).unwrap_or(match attendance {
                Attendance::Batch => LivePolicy::No,
                Attendance::Attended => LivePolicy::Ask,
            }),
        }
    }

    /// What the settings leave for the host to tell the user.
    pub(crate) fn warnings(&self) -> Vec<String> {
        let unassigned = (self.models.values()).all(|assignment| assignment.roles.is_empty());
        let warning = "no model role hints configured; every agent runs on the default model (see security.example.yaml)";
        Vec::from_iter(unassigned.then(|| named(self.file.as_deref(), warning)))
    }

    /// Replace ordinary modes with the security modes, merge MCP servers, and rewrite model
    /// offers. Nothing changes when an assigned model is not configured.
    pub(crate) fn apply(&self, config: &mut Config) -> Result<(), Error> {
        for (model, assignment) in &self.models {
            if let Err(ConfigError::Model {
                error, configured, ..
            }) = config.model_profile(model)
            {
                let path = vec![
                    Segment::Key("models".into()),
                    Segment::Key(model.to_string()),
                ];
                return Err(Error::ModelAssignment(Diagnostic {
                    at: assignment.at.clone(),
                    path: Steps(path),
                    message: format!("{error}, {configured}"),
                }));
            }
        }
        if let Some((model, _)) = self
            .models
            .iter()
            .find(|(_, assignment)| assignment.roles.contains(&ModelRole::Orchestrator))
        {
            config.default_model = Some(model.clone());
        }
        config.modes = modes::configured(&self.modes);
        config.default_mode = modes::root();
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
                        let mut text = format!("Roles: {roles}");
                        if let Some(hint) = assignment.hint.as_ref().or(profile.hint.as_ref()) {
                            text.push_str(". ");
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

    const SHARED: &str = r#"
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
"#;

    fn shared() -> Config {
        Config::from_yaml(SHARED).unwrap()
    }

    /// The shared configuration may start sessions in a mode the product installs.
    #[test]
    fn a_default_mode_the_product_installs_loads_and_seals() {
        let text = format!("default_mode: {}\n{SHARED}", modes::root());
        let mut config = Config::from_yaml(&text).unwrap();
        SecurityConfig::default().apply(&mut config).unwrap();
        config.into_runtime().unwrap();
    }

    fn parse(value: serde_json::Value) -> SecurityConfig {
        let text = serde_json::to_string(&value).unwrap();
        skyhook::yaml::parse(&text).unwrap()
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
        assert_eq!(config.default_mode, modes::root());
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
        assert!(config.modes.contains_key(&modes::root()));
        config.into_runtime().unwrap();

        let mut config = shared();
        config.default_model = Some("test/first".parse().unwrap());
        parse(json!({"models": {"test/first": {"roles": [], "hint": "Ignored."}}}))
            .apply(&mut config)
            .unwrap();
        assert_eq!(config.default_model, Some("test/first".parse().unwrap()));
        assert!(config.models().all(|(_, profile)| profile.hint.is_none()));
    }

    #[test]
    fn a_refused_assignment_names_its_file_and_named_modes_replace() {
        let mut security = parse(json!({
            "models": {"test/missing": {"roles": ["validator"]}},
            "modes": {"report": {"capabilities": ["read", "targets"]}},
            "mcp": {"security": {"transport": "stdio", "start_command": ["/bin/true"]}},
        }));
        let modes = modes::configured(&security.modes);
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
        let configured = skyhook_base::decode::one_of(config.models().map(|(model, _)| model));
        let refusal = format!(
            "1:27: `models.test/missing`: model is not configured, expected one of {configured}"
        );
        assert_eq!(
            security.apply(&mut config).unwrap_err().to_string(),
            refusal
        );
        security.models[0].at.file = Some(std::path::Path::new("/user/security.yaml").into());
        let error = security.apply(&mut config).unwrap_err();
        assert_matches!(&error, Error::ModelAssignment(_));
        assert_eq!(error.to_string(), format!("/user/security.yaml:{refusal}"));
        assert_eq!(serde_json::to_value(&config).unwrap(), before);
        security.models.clear();
        security.apply(&mut config).unwrap();
        assert!(config.mcp.contains_key("security"));
    }

    #[test]
    fn policy_defaults_and_cli_precedence() {
        let mut config = SecurityConfig::default();
        let batch = config.run_options(None, None, None, Attendance::Batch);
        assert_eq!(batch, RunOptions::default());
        let live = |config: &SecurityConfig, flag, attendance| {
            config.run_options(None, None, flag, attendance).live
        };
        assert_eq!(live(&config, None, Attendance::Attended), LivePolicy::Ask);
        config.shortlist = Some(Severity::High);
        config.workers = NonZeroU16::new(3);
        let options = |shortlist, workers| {
            let options = config.run_options(shortlist, workers, None, Attendance::Batch);
            (options.threshold, options.workers.get())
        };
        assert_eq!(options(None, None), (Severity::High, 3));
        let flags = options(Some(Severity::Medium), NonZeroU16::new(2));
        assert_eq!(flags, (Severity::Medium, 2));
        config.live = Some(LivePolicy::Ask);
        assert_eq!(live(&config, None, Attendance::Batch), LivePolicy::Ask);
        assert_eq!(
            live(&config, Some(LivePolicy::Yes), Attendance::Batch),
            LivePolicy::Yes
        );
        config.live = Some(LivePolicy::Yes);
        assert_eq!(
            live(&config, Some(LivePolicy::No), Attendance::Attended),
            LivePolicy::No
        );
    }
}
