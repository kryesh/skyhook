//! Provider admission at the YAML boundary and live construction. Model names
//! are passed through unchanged.

use std::{env, sync::Arc, time::Duration};

use serde::{Deserialize, Serialize};

use super::ConfigError;
use crate::provider::{
    Provider,
    backends::{
        ChatReasoningReplay, NativeSettings, OpenAiApi, Protocol, ProviderTimeouts,
        codex::CodexProvider,
    },
};

/// An admitted provider: settings whose syntax and applicability are proven, with
/// exactly one credential source. Deserializing admits the editable
/// [`RawProviderConfig`]; serializing writes it back with resolved defaults.
/// Environment lookup and command execution happen at construction, never here.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(try_from = "RawProviderConfig", into = "RawProviderConfig")]
pub struct ProviderConfig(Admitted);

#[derive(Clone, Debug)]
enum Admitted {
    Native {
        /// As written, so a dump reproduces the spelling the URL parser normalizes.
        base_url: String,
        settings: NativeSettings,
        auth: AuthSource,
    },
    /// ChatGPT subscription using Skyhook-owned OAuth credentials.
    Codex,
}

#[derive(Clone, Debug)]
enum AuthSource {
    None,
    Environment(String),
    Command(String),
}

/// The YAML shape of a provider entry.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RawProviderConfig {
    /// Standard OpenAI wire protocols, without endpoint or model presets.
    Openai {
        base_url: String,
        api: OpenAiApi,
        /// Chat-only request field; absence uses the shared reasoning_content default.
        chat_reasoning_replay: Option<ChatReasoningReplay>,
        api_key_env: Option<String>,
        /// Shell command run on first provider request; trimmed stdout is cached as the key.
        /// Mutually exclusive with api_key_env.
        api_key_command: Option<String>,
        /// Time to receive HTTP response headers per attempt; absent or null
        /// takes the default.
        startup_timeout_secs: Option<u64>,
        /// Maximum interval between HTTP response body reads; absent or null
        /// takes the default.
        read_idle_timeout_secs: Option<u64>,
    },
    Anthropic {
        base_url: String,
        api_key_env: Option<String>,
        /// Shell command run on first provider request; trimmed stdout is cached as the key.
        /// Mutually exclusive with api_key_env.
        api_key_command: Option<String>,
        /// Time to receive HTTP response headers per attempt; absent or null
        /// takes the default.
        startup_timeout_secs: Option<u64>,
        /// Maximum interval between HTTP response body reads; absent or null
        /// takes the default.
        read_idle_timeout_secs: Option<u64>,
    },
    /// ChatGPT subscription using Skyhook-owned OAuth credentials.
    Codex {},
}

impl TryFrom<RawProviderConfig> for ProviderConfig {
    type Error = String;

    fn try_from(raw: RawProviderConfig) -> Result<Self, String> {
        let (base_url, protocol, environment, command, startup, idle) = match raw {
            RawProviderConfig::Openai {
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
                            return Err("chat_reasoning_replay applies only to Chat Completions; Responses replays native reasoning automatically".into());
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
            RawProviderConfig::Anthropic {
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
            RawProviderConfig::Codex {} => return Ok(Self(Admitted::Codex)),
        };
        let auth = match (environment, command) {
            (Some(_), Some(_)) => {
                return Err("api_key_env and api_key_command are mutually exclusive".into());
            }
            (_, Some(command)) if command.trim().is_empty() => {
                return Err("api_key_command must not be blank".into());
            }
            (Some(environment), _)
                if environment.trim().is_empty() || environment.contains(['=', '\0']) =>
            {
                return Err("api_key_env must name a nonempty environment variable".into());
            }
            (Some(environment), None) => AuthSource::Environment(environment),
            (None, Some(command)) => AuthSource::Command(command),
            (None, None) => AuthSource::None,
        };
        let defaults = ProviderTimeouts::default();
        let timeouts = ProviderTimeouts {
            startup: startup.map_or(defaults.startup, Duration::from_secs),
            read_idle: idle.map_or(defaults.read_idle, Duration::from_secs),
        };
        let settings =
            NativeSettings::new(&base_url, protocol, timeouts).map_err(|error| error.message)?;
        Ok(Self(Admitted::Native {
            base_url,
            settings,
            auth,
        }))
    }
}

impl From<ProviderConfig> for RawProviderConfig {
    fn from(config: ProviderConfig) -> Self {
        let Admitted::Native {
            base_url,
            settings,
            auth,
        } = config.0
        else {
            return Self::Codex {};
        };
        let (api_key_env, api_key_command) = match auth {
            AuthSource::None => (None, None),
            AuthSource::Environment(variable) => (Some(variable), None),
            AuthSource::Command(command) => (None, Some(command)),
        };
        let timeouts = settings.timeouts();
        let startup_timeout_secs = Some(timeouts.startup.as_secs());
        let read_idle_timeout_secs = Some(timeouts.read_idle.as_secs());
        match settings.protocol() {
            Protocol::Anthropic => Self::Anthropic {
                base_url,
                api_key_env,
                api_key_command,
                startup_timeout_secs,
                read_idle_timeout_secs,
            },
            protocol => {
                let (api, chat_reasoning_replay) = match protocol {
                    Protocol::Chat { reasoning_replay } => {
                        (OpenAiApi::ChatCompletions, Some(reasoning_replay))
                    }
                    _ => (OpenAiApi::Responses, None),
                };
                Self::Openai {
                    base_url,
                    api,
                    chat_reasoning_replay,
                    api_key_env,
                    api_key_command,
                    startup_timeout_secs,
                    read_idle_timeout_secs,
                }
            }
        }
    }
}

impl ProviderConfig {
    /// Environment credentials are read here; commands stay lazy until the first
    /// request. Codex keeps its existing auth/context lifecycle.
    pub(super) fn build(self, name: &str) -> Result<Arc<dyn Provider>, ConfigError> {
        let error = |error: crate::provider::ProviderError| {
            ConfigError::Provider(name.into(), error.to_string())
        };
        match self.0 {
            Admitted::Native { settings, auth, .. } => {
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
            Admitted::Codex => Ok(Arc::new(
                CodexProvider::new().map_err(error)?.with_name(name),
            )),
        }
    }

    #[cfg(test)]
    pub(super) fn timeouts(&self) -> Option<ProviderTimeouts> {
        match &self.0 {
            Admitted::Native { settings, .. } => Some(settings.timeouts()),
            Admitted::Codex => None,
        }
    }
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
    use crate::config::Config;

    const LOCAL: &str = r#"
providers:
  local:
    kind: openai
    base_url: http://127.0.0.1:11434/v1
    api: chat_completions
models:
  local:
    provider: local
    model: local-model
    max_context: 4096
    max_output: 512
"#;

    fn with_replay(value: &str) -> String {
        let replay = format!("    chat_reasoning_replay: {value}\nmodels:");
        LOCAL.replace("models:", &replay)
    }

    fn builds(text: &str) -> bool {
        let config = Config::from_yaml(text).unwrap();
        config
            .into_runtime()
            .and_then(|runtime| runtime.select_model("local")?.harness_builder("."))
            .is_ok()
    }

    fn chat_replay(text: &str) -> Option<ChatReasoningReplay> {
        assert!(builds(text));
        let config = Config::from_yaml(text).unwrap();
        let RawProviderConfig::Openai {
            chat_reasoning_replay,
            ..
        } = RawProviderConfig::from(config.providers["local"].clone())
        else {
            panic!("expected OpenAI provider");
        };
        chat_reasoning_replay
    }

    #[test]
    fn provider_replay_defaults_to_reasoning_content_and_can_override_or_disable() {
        // The admitted default is written back on dump.
        assert_eq!(
            chat_replay(LOCAL),
            Some(ChatReasoningReplay::ReasoningContent)
        );
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
        assert!(Config::from_yaml(&with_replay("guess")).is_err());
        let on_model = format!("{LOCAL}    chat_reasoning_replay: reasoning\n");
        assert!(
            Config::from_yaml(&on_model).is_err(),
            "model-level policy must not be silently ignored"
        );
        let responses = |text: &str| text.replace("api: chat_completions", "api: responses");
        let error = Config::from_yaml(&responses(&with_replay("reasoning"))).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("`providers.local`: chat_reasoning_replay applies only"),
            "{error}"
        );
        assert!(builds(&responses(LOCAL)));
        for provider in [
            "    kind: anthropic\n    base_url: https://api.anthropic.com/v1",
            "    kind: codex",
        ] {
            let text = format!(
                "providers:\n  native:\n{provider}\n    chat_reasoning_replay: reasoning\n"
            );
            assert!(
                Config::from_yaml(&text).is_err(),
                "unrelated provider accepted Chat-specific config"
            );
        }
    }

    fn native_configs(
        authentication: &str,
    ) -> impl Iterator<Item = Result<ProviderConfig, String>> + '_ {
        [
            "kind: openai\napi: chat_completions",
            "kind: openai\napi: responses",
            "kind: anthropic",
        ]
        .into_iter()
        .map(move |kind| {
            crate::yaml::parse(&format!(
                "{kind}\nbase_url: https://example.com/v1\n{authentication}"
            ))
        })
    }

    #[test]
    fn command_credentials_are_not_executed_by_build_or_open_context() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("executed");
        let command = format!("printf key > '{}'", marker.display());
        let authentication = format!(
            "api_key_command: {}",
            serde_json::to_string(&command).unwrap()
        );
        for config in native_configs(&authentication) {
            let provider = config.unwrap().build("test").unwrap();
            let _first = provider.open_context("first".parse().unwrap()).unwrap();
            let _second = provider.open_context("second".parse().unwrap()).unwrap();
            assert!(
                !marker.exists(),
                "credentials must be resolved only on first request"
            );
        }
    }

    #[test]
    fn credential_sources_are_exclusive_blank_commands_rejected_and_kept_out_of_errors() {
        let exclusive = "api_key_env: SKYHOOK_TEST_MISSING_API_KEY\napi_key_command: secret-marker";
        for config in native_configs(exclusive) {
            let error = config.err().unwrap();
            assert!(error.contains("mutually exclusive"), "{error}");
            assert!(!error.contains("secret-marker"));
        }
        for config in native_configs("api_key_command: '   '") {
            let error = config.err().unwrap();
            assert!(error.contains("must not be blank"), "{error}");
        }
    }

    #[test]
    fn commands_do_not_change_keyless_or_required_environment_behavior() {
        for config in native_configs("") {
            assert!(config.unwrap().build("test").is_ok());
        }
        // No mutation of process-wide environment in concurrent tests.
        for config in native_configs("api_key_env: SKYHOOK_TEST_MISSING_API_KEY") {
            assert!(matches!(
                config.unwrap().build("test"),
                Err(ConfigError::MissingEnvironment(_))
            ));
        }
        let codex = "kind: codex\napi_key_command: echo key";
        assert!(crate::yaml::parse::<ProviderConfig>(codex).is_err());
    }

    #[test]
    fn timeout_defaults_overrides_and_validation() {
        // Admission alone: no client is created and no credential resolved.
        let timeouts = |startup: Option<u64>, read_idle: Option<u64>| {
            let mut text = "kind: anthropic\nbase_url: https://example.com/v1\n".to_owned();
            if let Some(startup) = startup {
                text.push_str(&format!("startup_timeout_secs: {startup}\n"));
            }
            if let Some(read_idle) = read_idle {
                text.push_str(&format!("read_idle_timeout_secs: {read_idle}\n"));
            }
            let config: ProviderConfig = crate::yaml::parse(&text)?;
            let timeouts = config.timeouts().unwrap();
            Ok::<_, String>((timeouts.startup.as_secs(), timeouts.read_idle.as_secs()))
        };
        assert_eq!(timeouts(None, None).unwrap(), (600, 600));
        assert_eq!(timeouts(Some(180), Some(300)).unwrap(), (180, 300));
        for (startup, read_idle) in [(Some(0), None), (None, Some(0)), (Some(u64::MAX), None)] {
            assert!(timeouts(startup, read_idle).is_err());
        }
    }
}
