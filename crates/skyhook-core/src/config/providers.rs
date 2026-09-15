//! Native provider construction. Model names are passed through unchanged.
//! Raw TOML DTOs are admitted without credential or process effects.

use std::{env, sync::Arc, time::Duration};

use super::{ConfigError, ProviderConfig};
use crate::provider::{
    Provider, ProviderTimeouts,
    backends::{NativeSettings, OpenAiApi, Protocol, codex::CodexProvider},
};

/// No public fields or constructors: each value owns exactly one validated auth
/// source. Environment lookup and shell execution are deliberately not admission.
#[derive(Clone)]
pub(super) enum AuthSource {
    None,
    Environment(String),
    Command(String),
}

impl AuthSource {
    fn new(
        name: &str,
        environment: Option<&str>,
        command: Option<&str>,
    ) -> Result<Self, ConfigError> {
        let invalid = |message: &str| ConfigError::Provider(name.into(), message.into());
        match (environment, command) {
            (Some(_), Some(_)) => Err(invalid(
                "api_key_env and api_key_command are mutually exclusive",
            )),
            (_, Some(command)) if command.trim().is_empty() => {
                Err(invalid("api_key_command must not be blank"))
            }
            (Some(environment), _)
                if environment.trim().is_empty() || environment.contains(['=', '\0']) =>
            {
                Err(invalid(
                    "api_key_env must name a nonempty environment variable",
                ))
            }
            (Some(environment), None) => Ok(Self::Environment(environment.into())),
            (None, Some(command)) => Ok(Self::Command(command.into())),
            (None, None) => Ok(Self::None),
        }
    }
}

/// Live construction consumes settings whose syntax and applicability have
/// already been proven. Codex keeps its existing auth/context lifecycle.
#[derive(Clone)]
pub(super) enum ValidatedProvider {
    Native {
        settings: NativeSettings,
        auth: AuthSource,
    },
    Codex,
}

impl ValidatedProvider {
    pub(super) fn new(name: &str, config: &ProviderConfig) -> Result<Self, ConfigError> {
        let (base, protocol, environment, command, startup, idle) = match config {
            ProviderConfig::Openai {
                base_url,
                api,
                chat_reasoning_replay,
                api_key_env,
                api_key_command,
                startup_timeout_secs,
                read_idle_timeout_secs,
            } => {
                let protocol = match api {
                    OpenAiApi::ChatCompletions => Protocol::Chat {
                        reasoning_replay: chat_reasoning_replay.unwrap_or_default(),
                    },
                    OpenAiApi::Responses => {
                        if chat_reasoning_replay.is_some() {
                            return Err(ConfigError::Provider(name.into(),
                                "chat_reasoning_replay applies only to Chat Completions; Responses replays native reasoning automatically".into()));
                        }
                        Protocol::Responses
                    }
                };
                (
                    base_url,
                    protocol,
                    api_key_env,
                    api_key_command,
                    startup_timeout_secs,
                    read_idle_timeout_secs,
                )
            }
            ProviderConfig::Anthropic {
                base_url,
                api_key_env,
                api_key_command,
                startup_timeout_secs,
                read_idle_timeout_secs,
            } => (
                base_url,
                Protocol::Anthropic,
                api_key_env,
                api_key_command,
                startup_timeout_secs,
                read_idle_timeout_secs,
            ),
            ProviderConfig::Codex {} => return Ok(Self::Codex),
        };
        let auth = AuthSource::new(name, environment.as_deref(), command.as_deref())?;
        let defaults = ProviderTimeouts::default();
        let timeouts = ProviderTimeouts {
            startup: startup.map(Duration::from_secs).unwrap_or(defaults.startup),
            read_idle: idle.map(Duration::from_secs).unwrap_or(defaults.read_idle),
        };
        let settings = NativeSettings::new(base, protocol, timeouts)
            .map_err(|error| ConfigError::Provider(name.into(), error.message))?;
        Ok(Self::Native { settings, auth })
    }

    pub(super) fn build(self, name: &str) -> Result<Arc<dyn Provider>, ConfigError> {
        let error = |error: crate::provider::ProviderError| {
            ConfigError::Provider(name.into(), error.to_string())
        };
        match self {
            Self::Native { settings, auth } => {
                // Environment credentials retain build-time lookup. Commands
                // retain lazy invocation-time resolution/cache/cancellation.
                let key = match &auth {
                    AuthSource::Environment(variable) => Some(required_env(variable)?),
                    AuthSource::None | AuthSource::Command(_) => None,
                };
                let mut provider = settings.build(name, key).map_err(error)?;
                if let AuthSource::Command(command) = auth {
                    provider = provider.with_api_key_command(command).map_err(error)?;
                }
                Ok(Arc::new(provider))
            }
            Self::Codex => Ok(Arc::new(
                CodexProvider::new().map_err(error)?.with_name(name),
            )),
        }
    }
}

#[cfg(test)]
fn build(name: &str, config: &ProviderConfig) -> Result<Arc<dyn Provider>, ConfigError> {
    ValidatedProvider::new(name, config)?.build(name)
}

fn required_env(name: &str) -> Result<String, ConfigError> {
    env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| ConfigError::MissingEnvironment(name.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, provider::backends::ChatReasoningReplay};

    const LOCAL: &str = r#"
[providers.local]
kind = "openai"
base_url = "http://127.0.0.1:11434/v1"
api = "chat_completions"
[models.local]
provider = "local"
model = "local-model"
max_context = 4096
max_output = 512
"#;

    fn with_replay(value: &str) -> String {
        let replay = format!("chat_reasoning_replay = \"{value}\"\n[models.local]");
        LOCAL.replace("[models.local]", &replay)
    }

    fn builds(text: &str) -> bool {
        let config: Config = toml::from_str(text).unwrap();
        config
            .into_runtime()
            .and_then(|runtime| runtime.select_model("local")?.harness_builder("."))
            .is_ok()
    }

    fn chat_replay(text: &str) -> Option<ChatReasoningReplay> {
        assert!(builds(text));
        let config: Config = toml::from_str(text).unwrap();
        let ProviderConfig::Openai {
            chat_reasoning_replay,
            ..
        } = config.providers["local"]
        else {
            panic!("expected OpenAI provider");
        };
        chat_reasoning_replay
    }

    #[test]
    fn provider_replay_defaults_to_reasoning_content_and_can_override_or_disable() {
        let default = chat_replay(LOCAL).unwrap_or_default();
        assert_eq!(default, ChatReasoningReplay::ReasoningContent);
        for (value, expected) in [
            ("reasoning_content", ChatReasoningReplay::ReasoningContent),
            ("reasoning", ChatReasoningReplay::Reasoning),
            ("unsupported", ChatReasoningReplay::Unsupported),
        ] {
            assert_eq!(chat_replay(&with_replay(value)), Some(expected));
        }
    }

    #[test]
    fn replay_is_provider_scoped_and_only_configurable_for_chat() {
        assert!(toml::from_str::<Config>(&with_replay("guess")).is_err());
        let on_model = format!("{LOCAL}\nchat_reasoning_replay = \"reasoning\"\n");
        assert!(
            toml::from_str::<Config>(&on_model).is_err(),
            "model-level policy must not be silently ignored"
        );
        let responses =
            |text: &str| text.replace("api = \"chat_completions\"", "api = \"responses\"");
        assert!(!builds(&responses(&with_replay("reasoning"))));
        assert!(builds(&responses(LOCAL)));
        for provider in [
            "kind = \"anthropic\"\nbase_url = \"https://api.anthropic.com/v1\"",
            "kind = \"codex\"",
        ] {
            let text =
                format!("[providers.native]\n{provider}\nchat_reasoning_replay = \"reasoning\"\n");
            assert!(
                toml::from_str::<Config>(&text).is_err(),
                "unrelated provider accepted Chat-specific config"
            );
        }
    }

    fn native_configs(authentication: &str) -> impl Iterator<Item = ProviderConfig> + '_ {
        [
            "kind = 'openai'\napi = 'chat_completions'",
            "kind = 'openai'\napi = 'responses'",
            "kind = 'anthropic'",
        ]
        .into_iter()
        .map(move |kind| {
            toml::from_str(&format!(
                "{kind}\nbase_url = 'https://example.com/v1'\n{authentication}"
            ))
            .unwrap()
        })
    }

    #[test]
    fn command_credentials_are_not_executed_by_build_or_open_context() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("executed");
        let command = format!("printf key > '{}'", marker.display());
        let authentication = format!(
            "api_key_command = {}",
            serde_json::to_string(&command).unwrap()
        );
        for config in native_configs(&authentication) {
            let provider = build("test", &config).unwrap();
            let _first = provider.open_context("first".to_owned()).unwrap();
            let _second = provider.open_context("second".to_owned()).unwrap();
            assert!(
                !marker.exists(),
                "credentials must be resolved only on first request"
            );
        }
    }

    #[test]
    fn credential_sources_are_exclusive_blank_commands_rejected_and_kept_out_of_errors() {
        let exclusive =
            "api_key_env = 'SKYHOOK_TEST_MISSING_API_KEY'\napi_key_command = 'secret-marker'";
        for config in native_configs(exclusive) {
            let admitted = ValidatedProvider::new("vendor", &config).map(|_| ());
            let built = build("test", &config).map(|_| ());
            for error in [
                admitted.unwrap_err().to_string(),
                built.unwrap_err().to_string(),
            ] {
                assert!(error.contains("mutually exclusive"), "{error}");
                assert!(!error.contains("secret-marker"));
            }
        }
        for config in native_configs("api_key_command = '   '") {
            let error = build("test", &config).err().unwrap().to_string();
            assert!(error.contains("must not be blank"), "{error}");
        }
    }

    #[test]
    fn commands_do_not_change_keyless_or_required_environment_behavior() {
        for config in native_configs("") {
            assert!(build("test", &config).is_ok());
        }
        // No mutation of process-wide environment in concurrent tests.
        for config in native_configs("api_key_env = 'SKYHOOK_TEST_MISSING_API_KEY'") {
            assert!(matches!(
                build("test", &config),
                Err(ConfigError::MissingEnvironment(_))
            ));
        }
        let codex = "kind = 'codex'\napi_key_command = 'echo key'";
        assert!(toml::from_str::<ProviderConfig>(codex).is_err());
    }

    #[test]
    fn timeout_defaults_overrides_and_validation() {
        // Exercise sealed admission without creating a client or resolving credentials.
        let timeouts = |startup_timeout_secs, read_idle_timeout_secs| {
            let config = ProviderConfig::Anthropic {
                base_url: "https://example.com/v1".into(),
                api_key_env: None,
                api_key_command: None,
                startup_timeout_secs,
                read_idle_timeout_secs,
            };
            match ValidatedProvider::new("local", &config)? {
                ValidatedProvider::Native { settings, .. } => {
                    let timeouts = settings.timeouts();
                    Ok::<_, ConfigError>((timeouts.startup.as_secs(), timeouts.read_idle.as_secs()))
                }
                ValidatedProvider::Codex => unreachable!("native fixture"),
            }
        };
        assert_eq!(timeouts(None, None).unwrap(), (600, 600));
        assert_eq!(timeouts(Some(180), Some(300)).unwrap(), (180, 300));
        for (startup, read_idle) in [(Some(0), None), (None, Some(0)), (Some(u64::MAX), None)] {
            assert!(timeouts(startup, read_idle).is_err());
        }
    }
}
