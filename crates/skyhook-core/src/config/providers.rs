//! Native provider construction. Model names are passed through unchanged.
//! Backend-specific settings are resolved here, never by the agent runtime.

use std::{env, sync::Arc, time::Duration};

use crate::provider::{
    Provider, ProviderTimeouts,
    backends::{OpenAiApi, anthropic_api, codex::CodexProvider, openai_compatible},
};

use super::{ConfigError, ProviderConfig};

pub(super) fn build(name: &str, config: &ProviderConfig) -> Result<Arc<dyn Provider>, ConfigError> {
    let error = |error: crate::provider::ProviderError| {
        ConfigError::Provider(name.to_owned(), error.to_string())
    };
    match config {
        ProviderConfig::Openai {
            base_url,
            api,
            api_key_env,
            api_key_command,
            chat_reasoning_replay,
            startup_timeout_secs,
            read_idle_timeout_secs,
        } => {
            if *api != OpenAiApi::ChatCompletions && chat_reasoning_replay.is_some() {
                return Err(ConfigError::Provider(name.into(),
                    "chat_reasoning_replay applies only to Chat Completions; Responses replays native reasoning automatically".into()));
            }
            let key = api_key(name, api_key_env.as_deref(), api_key_command.as_deref())?;
            let timeouts = timeouts(name, *startup_timeout_secs, *read_idle_timeout_secs)?;
            let mut provider = openai_compatible(name, base_url, *api, key)
                .map_err(error)?
                .with_timeouts(timeouts)
                .with_chat_reasoning_replay(chat_reasoning_replay.unwrap_or_default());
            if let Some(command) = api_key_command {
                provider = provider.with_api_key_command(command.clone());
            }
            Ok(Arc::new(provider))
        }
        ProviderConfig::Anthropic {
            base_url,
            api_key_env,
            api_key_command,
            startup_timeout_secs,
            read_idle_timeout_secs,
        } => {
            let key = api_key(name, api_key_env.as_deref(), api_key_command.as_deref())?;
            let timeouts = timeouts(name, *startup_timeout_secs, *read_idle_timeout_secs)?;
            let mut provider = anthropic_api(name, base_url, key)
                .map_err(error)?
                .with_timeouts(timeouts);
            if let Some(command) = api_key_command {
                provider = provider.with_api_key_command(command.clone());
            }
            Ok(Arc::new(provider))
        }
        ProviderConfig::Codex {} => Ok(Arc::new(
            CodexProvider::new().map_err(error)?.with_name(name),
        )),
    }
}

fn timeouts(
    name: &str,
    startup_secs: Option<u64>,
    read_idle_secs: Option<u64>,
) -> Result<ProviderTimeouts, ConfigError> {
    let defaults = ProviderTimeouts::default();
    let timeouts = ProviderTimeouts {
        startup: startup_secs
            .map(Duration::from_secs)
            .unwrap_or(defaults.startup),
        read_idle: read_idle_secs
            .map(Duration::from_secs)
            .unwrap_or(defaults.read_idle),
    };
    if [timeouts.startup, timeouts.read_idle]
        .into_iter()
        .any(|value| value.is_zero() || std::time::Instant::now().checked_add(value).is_none())
    {
        return Err(ConfigError::Provider(
            name.into(),
            "startup_timeout_secs and read_idle_timeout_secs must be positive, representable durations".into(),
        ));
    }
    Ok(timeouts)
}

/// Validate without executing commands, including for currently unused providers.
fn api_key(
    provider: &str,
    environment: Option<&str>,
    command: Option<&str>,
) -> Result<Option<String>, ConfigError> {
    if environment.is_some() && command.is_some() {
        return Err(ConfigError::Provider(
            provider.to_owned(),
            "api_key_env and api_key_command are mutually exclusive".to_owned(),
        ));
    }
    if command.is_some_and(|command| command.trim().is_empty()) {
        return Err(ConfigError::Provider(
            provider.to_owned(),
            "api_key_command must not be blank".to_owned(),
        ));
    }
    environment.map(required_env).transpose()
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

    #[test]
    fn provider_replay_defaults_to_reasoning_content_and_can_override_or_disable() {
        let config: Config = toml::from_str(LOCAL).unwrap();
        let ProviderConfig::Openai {
            chat_reasoning_replay,
            ..
        } = config.providers["local"]
        else {
            panic!("expected OpenAI provider");
        };
        assert_eq!(
            chat_reasoning_replay.unwrap_or_default(),
            ChatReasoningReplay::ReasoningContent
        );
        assert!(config.harness_builder(".", "local").is_ok());
        for (value, expected) in [
            ("reasoning_content", ChatReasoningReplay::ReasoningContent),
            ("reasoning", ChatReasoningReplay::Reasoning),
            ("unsupported", ChatReasoningReplay::Unsupported),
        ] {
            let configured = LOCAL.replace(
                "[models.local]",
                &format!("chat_reasoning_replay = \"{value}\"\n[models.local]"),
            );
            let config: Config = toml::from_str(&configured).unwrap();
            let ProviderConfig::Openai {
                chat_reasoning_replay,
                ..
            } = config.providers["local"]
            else {
                panic!("expected OpenAI provider");
            };
            assert_eq!(chat_reasoning_replay, Some(expected));
            assert!(config.harness_builder(".", "local").is_ok());
        }
    }

    #[test]
    fn replay_is_provider_scoped_and_only_configurable_for_chat() {
        let bad_value = LOCAL.replace(
            "[models.local]",
            "chat_reasoning_replay = \"guess\"\n[models.local]",
        );
        assert!(toml::from_str::<Config>(&bad_value).is_err());
        let on_model = format!("{LOCAL}\nchat_reasoning_replay = \"reasoning\"\n");
        assert!(
            toml::from_str::<Config>(&on_model).is_err(),
            "model-level policy must not be silently ignored"
        );
        let wrong_api = LOCAL
            .replace(
                "[models.local]",
                "chat_reasoning_replay = \"reasoning\"\n[models.local]",
            )
            .replace("api = \"chat_completions\"", "api = \"responses\"");
        let config: Config = toml::from_str(&wrong_api).unwrap();
        assert!(config.harness_builder(".", "local").is_err());
        let native = LOCAL.replace("api = \"chat_completions\"", "api = \"responses\"");
        let config: Config = toml::from_str(&native).unwrap();
        assert!(config.harness_builder(".", "local").is_ok());
    }

    #[test]
    fn unrelated_provider_kinds_reject_the_chat_selector() {
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
    fn credential_sources_are_exclusive_and_blank_commands_are_rejected() {
        for config in native_configs(
            "api_key_env = 'SKYHOOK_TEST_MISSING_API_KEY'\napi_key_command = 'echo secret'",
        ) {
            let error = build("test", &config).err().unwrap().to_string();
            assert!(error.contains("mutually exclusive"), "{error}");
            assert!(!error.contains("echo secret"));
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
        assert!(
            toml::from_str::<ProviderConfig>("kind = 'codex'\napi_key_command = 'echo key'")
                .is_err()
        );
    }

    #[test]
    fn timeout_defaults_overrides_and_validation() {
        let defaults = timeouts("local", None, None).unwrap();
        assert_eq!(defaults.startup, Duration::from_secs(600));
        assert_eq!(defaults.read_idle, Duration::from_secs(600));
        let custom = timeouts("local", Some(180), Some(300)).unwrap();
        assert_eq!(custom.startup, Duration::from_secs(180));
        assert_eq!(custom.read_idle, Duration::from_secs(300));
        assert!(timeouts("local", Some(0), None).is_err());
        assert!(timeouts("local", None, Some(0)).is_err());
        assert!(timeouts("local", Some(u64::MAX), None).is_err());
    }
}
