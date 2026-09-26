//! Provider entries: the YAML shape a configuration holds and callers edit, and
//! the admitted form a runtime configuration builds providers from. An entry
//! names its dialect and codec; the dialect owns the fields beside the common ones.

use std::sync::Arc;

use indexmap::IndexMap;
use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{self, MapAccess},
};
use serde_json::{Map, Value};

use super::ConfigError;
use crate::{
    agent::ModelEntry,
    provider::{
        codec::{Codec, CodecName},
        dialect::{
            AdmissionError, Common, Connection, Dialect, DialectSettings, Profile,
            codex::auth::Issuer,
        },
        profile::{ModelName, ModelProfile, ProviderName},
    },
};

/// A provider entry as configured. Admission happens when a runtime
/// configuration is sealed, so edits here are checked there.
#[derive(Clone, Debug)]
pub struct RawProviderConfig {
    pub settings: DialectSettings,
    pub codec: CodecName,
    pub common: Common,
}

/// The common fields are read in place, so their errors carry their path; the
/// rest belong to the dialect, which is parsed once it is known.
impl<'de> Deserialize<'de> for RawProviderConfig {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Default)]
        struct Rest {
            dialect: Option<Dialect>,
            codec: Option<CodecName>,
            fields: Map<String, Value>,
        }

        impl<'de> crate::yaml::Rest<'de> for Rest {
            fn read<A: MapAccess<'de>>(
                &mut self,
                key: String,
                map: &mut A,
            ) -> Result<(), A::Error> {
                match key.as_str() {
                    "dialect" => self.dialect = Some(map.next_value()?),
                    "codec" => self.codec = Some(map.next_value()?),
                    _ => {
                        self.fields.insert(key, map.next_value()?);
                    }
                }
                Ok(())
            }
        }

        let mut rest = Rest::default();
        let common = crate::yaml::split(deserializer, &mut rest)?;
        let dialect = rest
            .dialect
            .ok_or_else(|| de::Error::missing_field("dialect"))?;
        let settings = DialectSettings::parse(dialect, Value::Object(rest.fields))
            .map_err(|error| de::Error::custom(format_args!("{dialect}: {error}")))?;
        Ok(Self {
            settings,
            codec: rest
                .codec
                .ok_or_else(|| de::Error::missing_field("codec"))?,
            common,
        })
    }
}

/// `dump config` writes the entry with its defaults resolved.
impl Serialize for RawProviderConfig {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Written {
            dialect: Dialect,
            codec: CodecName,
            #[serde(flatten)]
            common: Common,
            #[serde(flatten)]
            settings: Map<String, Value>,
        }
        Written {
            dialect: self.settings.dialect(),
            codec: self.codec,
            common: self.common.clone().with_resolved_defaults(),
            settings: self.settings.fields(),
        }
        .serialize(serializer)
    }
}

impl RawProviderConfig {
    /// The dialect proves it speaks the codec and fixes its conventions, the
    /// common fields are typed, and each model's conventions are derived from
    /// the entry's. Nothing is read from the environment and no command runs.
    pub(crate) fn admit(&self) -> Result<ProviderConfig, AdmissionError> {
        let profile = self.settings.admit(&self.common, self.codec)?;
        let connection = self.common.admit(profile.base_url, self.codec)?;
        let models = self
            .common
            .models
            .iter()
            .map(|(name, spec)| {
                let codec = spec
                    .admit(&profile.codec)
                    .map_err(|error| AdmissionError::Model {
                        model: name.clone(),
                        error,
                    })?;
                let model = AdmittedModel {
                    profile: spec.profile.clone(),
                    codec,
                };
                Ok((name.clone(), model))
            })
            .collect::<Result<_, AdmissionError>>()?;
        Ok(ProviderConfig {
            settings: self.settings.clone(),
            profile,
            connection,
            models,
        })
    }
}

/// A provider entry refused on admission, located by its key.
#[derive(Debug, thiserror::Error)]
#[error("`providers.{provider}`: {error}")]
pub struct EntryError {
    pub provider: ProviderName,
    pub error: AdmissionError,
}

/// Admit every entry, in declaration order. Every codex entry names one
/// issuer, since one credential store serves one.
pub(super) fn admit(
    providers: &IndexMap<ProviderName, RawProviderConfig>,
) -> Result<IndexMap<ProviderName, ProviderConfig>, ConfigError> {
    let admitted = providers
        .iter()
        .map(|(name, entry)| {
            let admitted = entry.admit().map_err(|error| EntryError {
                provider: name.clone(),
                error,
            })?;
            Ok((name.clone(), admitted))
        })
        .collect::<Result<IndexMap<_, _>, EntryError>>()?;
    let mut issuers = admitted
        .iter()
        .filter_map(|(name, entry)| Some((name, entry.codex_issuer()?)));
    if let Some((first, issuer)) = issuers.next()
        && let Some((second, _)) = issuers.find(|(_, other)| *other != issuer)
    {
        return Err(ConfigError::CodexIssuers {
            first: first.clone(),
            second: second.clone(),
        });
    }
    Ok(admitted)
}

/// An admitted provider entry. Environment values are read when it is built.
pub(crate) struct ProviderConfig {
    settings: DialectSettings,
    profile: Profile,
    connection: Connection,
    models: IndexMap<ModelName, AdmittedModel>,
}

/// A model and the conventions it is served with.
pub(crate) struct AdmittedModel {
    profile: ModelProfile,
    codec: Codec,
}

impl AdmittedModel {
    pub(crate) fn profile(&self) -> &ModelProfile {
        &self.profile
    }
}

impl ProviderConfig {
    /// The models this entry serves, in declaration order.
    pub(crate) fn models(&self) -> &IndexMap<ModelName, AdmittedModel> {
        &self.models
    }

    /// The issuer of a codex entry.
    pub(super) fn codex_issuer(&self) -> Option<Issuer> {
        match &self.settings {
            DialectSettings::Codex(codex) => Some(codex.issuer()),
            _ => None,
        }
    }

    /// Construct the provider and specialise it for each model's conventions.
    /// Environment values are read here; a command runs on the first request
    /// that sends its value.
    pub(super) fn build(
        &self,
        name: &ProviderName,
    ) -> Result<Vec<(ModelName, ModelEntry)>, ConfigError> {
        let provider = self
            .settings
            .provider(name.as_str(), self.profile.clone(), &self.connection)
            .map_err(|error| ConfigError::Provider {
                provider: name.clone(),
                error,
            })?;
        Ok(self
            .models
            .iter()
            .map(|(model, admitted)| {
                let entry = ModelEntry {
                    profile: admitted.profile.clone(),
                    provider: Arc::new(provider.with_codec(admitted.codec.clone())),
                };
                (model.clone(), entry)
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, provider::dialect::Sourced};

    const LOCAL: &str = r#"
providers:
  local:
    dialect: compatible
    codec: chat_completions
    base_url: http://127.0.0.1:11434/v1
    models:
      local:
        model: local-model
        max_context: 4096
        max_output: 512
"#;

    fn builds(text: &str) -> bool {
        let config = Config::from_yaml(text).unwrap();
        config
            .into_runtime()
            .and_then(|runtime| {
                runtime
                    .select_model(&"local/local".parse().unwrap())?
                    .harness_builder(".")
            })
            .is_ok()
    }

    fn with(text: &str, extra: &str) -> String {
        text.replace("    models:", &format!("{extra}\n    models:"))
    }

    fn admits(text: &str) -> bool {
        crate::yaml::parse::<RawProviderConfig>(text)
            .ok()
            .and_then(|entry| entry.admit().ok())
            .is_some()
    }

    fn error(text: &str) -> String {
        Config::from_yaml(text).unwrap_err().to_string()
    }

    #[test]
    fn entries_are_dialect_tagged_and_dump_with_resolved_defaults() {
        assert!(builds(LOCAL));
        let config = Config::from_yaml(LOCAL).unwrap();
        let entry = &config.providers["local"];
        assert_eq!(
            (entry.settings.dialect(), entry.codec),
            (Dialect::Compatible, CodecName::ChatCompletions)
        );
        let dumped = config.to_yaml().unwrap();
        for resolved in [
            "dialect: compatible",
            "codec: chat_completions",
            "startup_timeout_secs: 600",
            "read_idle_timeout_secs: 600",
        ] {
            assert!(dumped.contains(resolved), "{dumped}");
        }
        assert!(!dumped.contains("api_key"));
        let reparsed = Config::from_yaml(&dumped).unwrap();
        assert_eq!(
            serde_json::to_value(&reparsed.providers["local"]).unwrap(),
            serde_json::to_value(&config.providers["local"]).unwrap()
        );
        // A setting at its default is not written.
        let openai = LOCAL.replace("dialect: compatible", "dialect: openai");
        let dumped = Config::from_yaml(&openai).unwrap().to_yaml().unwrap();
        assert!(!dumped.contains("reasoning_summary"), "{dumped}");
    }

    #[test]
    fn edits_are_admitted_when_the_runtime_is_sealed() {
        let mut config = Config::from_yaml(LOCAL).unwrap();
        let entry = config.providers.get_index_mut(0).unwrap().1;
        entry.codec = CodecName::Messages;
        assert_eq!(
            entry.admit().unwrap().profile.codec.name(),
            CodecName::Messages
        );
        let base_url = entry.common.base_url.replace("relative".into());
        assert!(matches!(entry.admit(), Err(AdmissionError::Endpoint(_))));
        entry.common.base_url = base_url;
        entry
            .common
            .headers
            .insert("bad header".into(), Sourced::Literal("x".into()));
        let error = config.into_runtime().err().unwrap().to_string();
        assert!(
            error.contains("`providers.local`: headers: `bad header`"),
            "{error}"
        );
    }

    #[test]
    fn refusals_name_the_entry_and_field() {
        for (field, expected) in [
            (
                "    api_key: |\n      sk-key\n",
                "`providers.local`: api_key: must be a valid header value",
            ),
            (
                "    headers: {x-title: \"a\\nb\"}\n",
                "`providers.local`: headers.x-title: must be a valid header value",
            ),
            (
                "    headers: {x-count: 1}\n",
                "`providers.local.headers.x-count`",
            ),
            ("    api_key: 1\n", "`providers.local.api_key`"),
            (
                "    headers: {X-Title: a, x-title: b}\n",
                "`providers.local`: headers: `x-title` is named twice",
            ),
            (
                "    startup_timeout_secs: soon\n",
                "`providers.local.startup_timeout_secs`",
            ),
            (
                "    reasoning_summary: false",
                "`providers.local`: compatible: reasoning_summary: unknown field",
            ),
        ] {
            let refused = error(&with(LOCAL, field.trim_end()));
            assert!(refused.contains(expected), "{field}: {refused}");
        }
        let located = error(&LOCAL.replace("max_context: 4096", "max_context: large"));
        assert!(
            located.contains("`providers.local.models.local.max_context`"),
            "{located}"
        );
        // Both limits are required, and the output fits inside the context.
        let limits = "max_context: 4096\n        max_output: 512";
        for (replacement, expected) in [
            ("max_output: 512", "max_context"),
            ("max_context: 4096", "max_output"),
            (
                "max_context: 0\n        max_output: 1",
                "max_context must be positive",
            ),
            (
                "max_context: 4096\n        max_output: 0",
                "max_output must be positive",
            ),
            (
                "max_context: 4096\n        max_output: 4096",
                "max_output must be smaller",
            ),
            (
                "max_context: 4096\n        max_output: 4097",
                "max_output must be smaller",
            ),
        ] {
            let refused = error(&LOCAL.replace(limits, replacement));
            assert!(
                refused.contains("providers.local") && refused.contains(expected),
                "{replacement}: {refused}"
            );
        }
        // A model's reasoning level is one its conventions accept.
        let adaptive = LOCAL
            .replace("dialect: compatible", "dialect: openai")
            .replace("codec: chat_completions", "codec: responses")
            .replace(
                "max_output: 512",
                "max_output: 512\n        reasoning: adaptive",
            );
        let refused = error(&adaptive);
        assert!(
            refused.contains("`providers.local`: models.local: reasoning `adaptive` is not one of"),
            "{refused}"
        );
        assert!(builds(&adaptive.replace("adaptive", "high")));
        // A model's placements stay clear of the codec's own fields.
        let placed = LOCAL.replace(
            "max_output: 512",
            "max_output: 512\n        overrides:\n          reasoning_effort: messages",
        );
        let refused = error(&placed);
        assert!(
            refused.contains(
                "`providers.local`: models.local: reasoning_effort `messages` is a field the chat_completions codec writes"
            ),
            "{refused}"
        );
    }

    /// A value missing when the provider is built is reported with its provider
    /// and field.
    #[test]
    fn build_errors_name_the_provider_and_field() {
        for (field, expected) in [
            (
                "    api_key: {env: SKYHOOK_TEST_UNSET_API_KEY}",
                "provider `local` could not be initialized: api_key: environment variable `SKYHOOK_TEST_UNSET_API_KEY` is required",
            ),
            (
                "    headers: {x-token: {env: SKYHOOK_TEST_UNSET_HEADER}}",
                "provider `local` could not be initialized: headers.x-token: environment variable `SKYHOOK_TEST_UNSET_HEADER` is required",
            ),
        ] {
            let error = Config::from_yaml(&with(LOCAL, field))
                .unwrap()
                .into_runtime()
                .and_then(|runtime| {
                    runtime
                        .select_model(&"local/local".parse().unwrap())?
                        .harness_builder(".")
                })
                .err()
                .unwrap()
                .to_string();
            assert!(error.contains(expected), "{error}");
        }
    }

    /// One credential store serves one issuer, so every codex entry names the
    /// same one; the default counts as OpenAI's.
    #[test]
    fn codex_entries_share_one_issuer() {
        let entry = |name: &str, auth_url: &str| {
            format!("  {name}:\n    dialect: codex\n    codec: responses\n{auth_url}")
        };
        let mirror = "    auth_url: https://auth.example/tenant\n";
        for second in ["    auth_url: https://auth.example/other\n", ""] {
            let text = format!("providers:\n{}{}", entry("a", mirror), entry("b", second));
            let refused = error(&text);
            assert!(
                refused.contains("codex providers `a` and `b` name different auth_url issuers"),
                "{refused}"
            );
        }
        let same = "    auth_url: https://auth.example/tenant/\n";
        let text = format!("providers:\n{}{}", entry("a", mirror), entry("b", same));
        let issuer = Config::from_yaml(&text).unwrap().codex_issuer().unwrap();
        assert_eq!(issuer.as_str(), "https://auth.example/tenant/");
    }

    #[test]
    fn entries_name_a_known_dialect_and_codec_and_an_endpoint_it_needs() {
        let root = "base_url: https://example.com/v1";
        for text in [
            format!("codec: responses\n{root}"),
            format!("dialect: openai\n{root}"),
            format!("dialect: claude\ncodec: messages\n{root}"),
            format!("dialect: compatible\ncodec: completions\n{root}"),
            "dialect: openai\ncodec: responses".to_owned(),
        ] {
            assert!(!admits(&text), "{text}");
        }
        assert!(admits(&format!(
            "dialect: openai\ncodec: responses\n{root}"
        )));
    }

    /// Each model is served with its own overrides: one caps output under
    /// `max_tokens` and replays no reasoning, the other keeps the entry's.
    #[tokio::test]
    async fn model_overrides_specialise_the_provider_per_model() {
        use crate::provider::{
            codec::common::tests::chat_request,
            http::tests::{complete, serve},
            protocol::{Message, ToolResult, UserContent},
        };
        use serde_json::json;
        let first = vec![
            json!({"choices":[{"index":0,"delta":{"reasoning_content":"plan","tool_calls":[
                {"index":0,"id":"call_a","type":"function","function":{"name":"lookup","arguments":"{}"}}
            ]}}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
        ];
        let answer = vec![
            json!({"choices":[{"index":0,"delta":{"content":"done"},"finish_reason":"stop"}]}),
        ];
        let (root, server) = serve(vec![first.clone(), answer.clone(), first, answer]).await;
        let text = LOCAL
            .replace("http://127.0.0.1:11434/v1", &root)
            .replace(
                "        max_output: 512\n",
                "        max_output: 512\n      strict:\n        model: strict-model\n        max_context: 4096\n        max_output: 512\n        overrides:\n          reasoning_replay: omitted\n          output_limit: {field: max_tokens}\n",
            );
        let config = Config::from_yaml(&text).unwrap();
        let models = config.providers["local"]
            .admit()
            .unwrap()
            .build(&"local".parse().unwrap())
            .unwrap();
        for (_, entry) in &models {
            let mut context = entry.provider.open_context("c".parse().unwrap()).unwrap();
            let user = Message::User(vec![UserContent::Text {
                text: "find".into(),
            }]);
            let model = &entry.profile.model;
            let reduced = complete(&mut *context, chat_request(model, vec![user.clone()])).await;
            let result = Message::Tool(vec![ToolResult {
                call_id: "call_a".into(),
                name: "lookup".into(),
                result: json!({}),
                is_error: false,
                images: Vec::new(),
            }]);
            let assistant = Message::Assistant(reduced.items().to_vec());
            complete(
                &mut *context,
                chat_request(model, vec![user, assistant, result]),
            )
            .await;
        }
        let requests: Vec<Value> = server
            .finish()
            .await
            .iter()
            .map(|request| serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap())
            .collect();
        let sent = |request: &Value, field: &str| request.get(field).is_some();
        let replayed = |request: &Value| sent(&request["messages"][1], "reasoning_content");
        let [local, local_replay, strict, strict_replay] = &requests[..] else {
            panic!("{requests:?}")
        };
        assert_eq!(
            (&local["model"], &strict["model"]),
            (&json!("local-model"), &json!("strict-model"))
        );
        assert!(sent(local, "max_completion_tokens") && !sent(local, "max_tokens"));
        assert!(sent(strict, "max_tokens") && !sent(strict, "max_completion_tokens"));
        assert!(replayed(local_replay) && !replayed(strict_replay));
        let foreign = text
            .replace("codec: chat_completions", "codec: responses")
            .replace("          output_limit: {field: max_tokens}\n", "");
        let refused = error(&foreign);
        assert!(
            refused.contains("reasoning_replay is not a responses setting"),
            "{refused}"
        );
    }
}
