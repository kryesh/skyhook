//! Dialect-owned provider declarations, admission, and shared-resource construction.

use std::{fmt::Debug, sync::Arc};

use indexmap::IndexMap;
use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{self, DeserializeOwned},
};
use serde_json::{Map, Value};

use super::{
    self as dialect, AdmissionError, BaseUrl, BuildError, Common, Connection, Dialect,
    DialectConfig, DialectError, ModelError, ModelSpec, Profile, Request, RequestPatch,
    RequestSettings, anthropic, codex, compatible,
    config::Pending,
    litellm,
    models::{Fields, object},
    openai, openrouter,
};
use crate::provider::{
    Provider,
    http::Headers,
    profile::{ModelName, ModelProfile, ProviderName},
    settings::{Empty, Field, Patch, Settings, find_field},
};

/// Provider-only options own connection rules, authentication, and constraints
/// shared by entries of the same backend. Request settings never carry them.
pub(crate) trait ProviderOptions:
    Settings + Clone + Debug + Serialize + DeserializeOwned
{
    fn admit(&self, common: &Common) -> Result<Connection, AdmissionError> {
        common.admit(BaseUrl::Required)
    }

    fn authentication(&self) -> Result<Option<Headers<Pending>>, BuildError> {
        Ok(None)
    }

    fn validate<'a>(
        _entries: impl Iterator<Item = (&'a ProviderName, &'a Self)>,
    ) -> Result<(), DialectError>
    where
        Self: 'a,
    {
        Ok(())
    }
}

impl ProviderOptions for Empty {}

#[derive(Clone, Debug)]
pub struct ProviderModels<P, O> {
    pub options: O,
    pub defaults: RequestSettings<P>,
    pub models: IndexMap<ModelName, ModelSpec<P>>,
}

impl<P: Patch, O> ProviderModels<P, O> {
    fn parse(mut fields: Map<String, Value>) -> Result<Self, String>
    where
        O: ProviderOptions,
    {
        let models = fields
            .remove("models")
            .unwrap_or_else(|| Value::Object(Map::new()));
        let (options, defaults) = fields
            .into_iter()
            .partition(|(key, _)| find_field(<O::Patch as Patch>::FIELDS, key).is_some());
        let options = serde_path_to_error::deserialize(Value::Object(options))
            .map_err(|error| error.to_string())?;
        let models =
            serde_path_to_error::deserialize(models).map_err(|error| format!("models.{error}"))?;
        let defaults = serde_path_to_error::deserialize(Value::Object(defaults))
            .map_err(|error| error.to_string())?;
        Ok(Self {
            options,
            defaults,
            models,
        })
    }

    fn fields(&self) -> Map<String, Value>
    where
        O: Serialize,
    {
        let mut fields = object(&self.options);
        fields.extend(self.defaults.fields());
        fields.insert("models".into(), serde_json::to_value(&self.models).unwrap());
        fields
    }

    fn admit<C>(&self, common: &Common) -> Result<AdmittedEntry<O>, AdmissionError>
    where
        C: Settings<Patch = P> + DialectConfig,
        O: ProviderOptions,
    {
        let connection = self.options.admit(common)?;
        let models = self
            .models
            .iter()
            .map(|(name, model)| {
                let admit = || -> Result<AdmittedModel, ModelError> {
                    let settings = self.defaults.overlay(&model.settings);
                    let request = Request::resolve(&settings.request)?;
                    let mut wire = C::resolve(&settings.dialect)?.admit(request.codec)?;
                    wire.codec = request.placements.apply(wire.codec)?;
                    model.validate(&wire.codec)?;
                    Ok(AdmittedModel {
                        profile: model.profile.clone(),
                        wire,
                    })
                };
                let admitted = admit().map_err(|error| AdmissionError::Model {
                    model: name.clone(),
                    error,
                })?;
                Ok((name.clone(), admitted))
            })
            .collect::<Result<_, AdmissionError>>()?;
        Ok(AdmittedEntry {
            connection,
            models,
            options: self.options.clone(),
        })
    }
}

macro_rules! providers {
    ($($variant:ident => $module:ident),+ $(,)?) => {
        /// Each variant binds defaults, models, and provider options to one dialect.
        #[derive(Clone, Debug)]
        pub enum ProviderSettings {
            $($variant(ProviderModels<$module::Patch, $module::Options>)),+
        }

        impl ProviderSettings {
            pub fn dialect(&self) -> Dialect {
                match self { $(Self::$variant(_) => Dialect::$variant),+ }
            }

            fn parse(dialect: Dialect, fields: Map<String, Value>) -> Result<Self, String> {
                Ok(match dialect {
                    $(Dialect::$variant => Self::$variant(ProviderModels::parse(fields)?)),+
                })
            }

            fn fields(&self) -> Map<String, Value> {
                match self { $(Self::$variant(entry) => entry.fields()),+ }
            }

            fn admit(&self, common: &Common) -> Result<AdmittedProvider, AdmissionError> {
                Ok(match self {
                    $(Self::$variant(entry) => {
                        AdmittedProvider::$variant(entry.admit::<$module::Config>(common)?)
                    }),+
                })
            }
        }

        pub(crate) enum AdmittedProvider {
            $($variant(AdmittedEntry<$module::Options>)),+
        }

        impl AdmittedProvider {
            pub(crate) fn models(&self) -> &IndexMap<ModelName, AdmittedModel> {
                match self { $(Self::$variant(entry) => &entry.models),+ }
            }

            pub(crate) fn build(&self, name: &ProviderName) -> Result<Vec<BuiltModel>, BuildError> {
                match self { $(Self::$variant(entry) => entry.build(name)),+ }
            }
        }

        impl Dialect {
            pub(crate) fn request_field(self, name: &str) -> Option<&'static Field> {
                find_field(RequestPatch::FIELDS, name)
                    .or_else(|| find_field(self.request_fields(), name))
            }

            pub(crate) fn provider_field(self, name: &str) -> Option<&'static Field> {
                let fields = match self {
                    $(Self::$variant => <<$module::Options as Settings>::Patch as Patch>::FIELDS),+
                };
                find_field(fields, name).or_else(|| self.request_field(name))
            }

            pub(crate) fn owns_request_field(self, name: &str) -> bool {
                find_field(self.request_fields(), name).is_some()
            }

            fn request_fields(self) -> &'static [Field] {
                match self { $(Self::$variant => $module::Patch::FIELDS),+ }
            }
        }

        /// Cross-entry constraints belong to each backend, not the config loader.
        pub(crate) fn validate_entries(
            entries: &IndexMap<ProviderName, RawProviderConfig>,
        ) -> Result<(), DialectError> {
            $(
                let options = entries.iter().filter_map(|(name, entry)| match &entry.settings {
                    ProviderSettings::$variant(entry) => Some((name, &entry.options)),
                    _ => None,
                });
                <$module::Options as ProviderOptions>::validate(options)?;
            )+
            Ok(())
        }
    };
}

providers! {
    Compatible => compatible,
    Openai => openai,
    Anthropic => anthropic,
    Codex => codex,
    Litellm => litellm,
    Openrouter => openrouter,
}

/// Mutable declarations; model inheritance is resolved only on admission.
#[derive(Clone, Debug)]
pub struct RawProviderConfig {
    pub common: Common,
    pub settings: ProviderSettings,
}

impl<'de> Deserialize<'de> for RawProviderConfig {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut fields = Map::new();
        let common = crate::yaml::split(deserializer, &mut Fields(&mut fields))?;
        let dialect = fields
            .remove("dialect")
            .ok_or_else(|| de::Error::missing_field("dialect"))?;
        let dialect = Dialect::deserialize(dialect).map_err(de::Error::custom)?;
        let settings = ProviderSettings::parse(dialect, fields).map_err(de::Error::custom)?;
        Ok(Self { common, settings })
    }
}

impl Serialize for RawProviderConfig {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut fields = object(&self.common.clone().with_resolved_defaults());
        fields.insert(
            "dialect".into(),
            serde_json::to_value(self.settings.dialect()).unwrap(),
        );
        fields.extend(self.settings.fields());
        fields.serialize(serializer)
    }
}

impl RawProviderConfig {
    pub(crate) fn admit(&self) -> Result<AdmittedProvider, AdmissionError> {
        self.settings.admit(&self.common)
    }
}

pub(crate) struct AdmittedEntry<O> {
    connection: Connection,
    models: IndexMap<ModelName, AdmittedModel>,
    options: O,
}

pub(crate) struct AdmittedModel {
    profile: ModelProfile,
    wire: Profile,
}

impl AdmittedModel {
    pub(crate) fn profile(&self) -> &ModelProfile {
        &self.profile
    }
}

/// Provider-owned construction output, adapted into the host's model catalog.
pub(crate) struct BuiltModel {
    pub name: ModelName,
    pub profile: ModelProfile,
    pub provider: Arc<dyn Provider>,
}

impl<O: ProviderOptions> AdmittedEntry<O> {
    fn build(&self, name: &ProviderName) -> Result<Vec<BuiltModel>, BuildError> {
        if self.models.is_empty() {
            return Ok(Vec::new());
        }
        let mut resources = dialect::Resources::new()?;
        let authentication = self.options.authentication()?;
        self.models
            .iter()
            .map(|(model, admitted)| {
                let wire = admitted.wire.clone();
                let credentials = authentication
                    .clone()
                    .unwrap_or_else(|| self.connection.credentials(wire.key));
                let provider = dialect::build(
                    name.as_str(),
                    wire,
                    &self.connection,
                    credentials,
                    &mut resources,
                )?;
                Ok(BuiltModel {
                    name: model.clone(),
                    profile: admitted.profile.clone(),
                    provider: Arc::new(provider),
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::Config,
        provider::{codec::CodecName, dialect::Sourced, settings::Setting},
    };

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

    /// Every model of the `local` entry, built as a harness builds them.
    fn build(text: &str) -> Vec<BuiltModel> {
        let config = Config::from_yaml(text).unwrap();
        let admitted = config.providers["local"].admit().unwrap();
        admitted.build(&"local".parse().unwrap()).unwrap()
    }

    #[test]
    fn entries_are_dialect_tagged_and_dump_with_resolved_defaults() {
        assert!(builds(LOCAL));
        let config = Config::from_yaml(LOCAL).unwrap();
        let entry = &config.providers["local"];
        let ProviderSettings::Compatible(models) = &entry.settings else {
            panic!("compatible fixture");
        };
        assert_eq!(
            (entry.settings.dialect(), &models.defaults.request.codec),
            (
                Dialect::Compatible,
                &Setting::Set(CodecName::ChatCompletions)
            )
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
        // An omitted setting is not materialized by the dump.
        let openai = LOCAL.replace("dialect: compatible", "dialect: openai");
        let dumped = Config::from_yaml(&openai).unwrap().to_yaml().unwrap();
        assert!(!dumped.contains("reasoning_summary"), "{dumped}");
    }

    #[test]
    fn edits_are_admitted_when_the_runtime_is_sealed() {
        let mut config = Config::from_yaml(LOCAL).unwrap();
        let entry = config.providers.get_index_mut(0).unwrap().1;
        let ProviderSettings::Compatible(models) = &mut entry.settings else {
            panic!("compatible fixture");
        };
        models.defaults.request.codec = Setting::Set(CodecName::Messages);
        assert_eq!(
            entry.admit().unwrap().models()["local"].wire.codec.name(),
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
                "`providers.local`: reasoning_summary: unknown field",
            ),
        ] {
            let refused = error(&with(LOCAL, field.trim_end()));
            assert!(refused.contains(expected), "{field}: {refused}");
        }
        let located = error(&LOCAL.replace("max_context: 4096", "max_context: large"));
        assert!(
            located.contains("providers.local") && located.contains("models.local.max_context"),
            "{located}"
        );
        // Both limits are required, and admission checks them.
        let limits = "max_context: 4096\n        max_output: 512";
        for (replacement, expected) in [
            ("max_output: 512", "max_context"),
            ("max_context: 4096", "max_output"),
            (
                "max_context: 4096\n        max_output: 4096",
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
            "max_output: 512\n        reasoning_effort: messages",
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
        let config = Config::from_yaml(&text).unwrap();
        let issuer = codex::issuer(&config.providers).unwrap();
        assert_eq!(issuer.as_str(), "https://auth.example/tenant/");
    }

    #[test]
    fn entries_name_a_known_dialect_and_codec_and_an_endpoint_it_needs() {
        let root = "base_url: https://example.com/v1";
        let model =
            "models:\n  test:\n    model: fixture\n    max_context: 4096\n    max_output: 512";
        for text in [
            format!("codec: responses\n{root}"),
            format!("dialect: openai\n{root}"),
            format!("dialect: claude\ncodec: messages\n{root}"),
            format!("dialect: compatible\ncodec: completions\n{root}"),
            "dialect: openai\ncodec: responses".to_owned(),
        ] {
            assert!(!admits(&format!("{text}\n{model}")), "{text}");
        }
        assert!(admits(&format!(
            "dialect: openai\ncodec: responses\n{root}\n{model}"
        )));
    }

    /// Each model is served with its own settings: one caps output under
    /// `max_tokens` and replays no reasoning, the other keeps the entry's.
    #[tokio::test]
    async fn model_settings_specialise_the_provider_per_model() {
        use crate::provider::{
            http::tests::{chat_request, complete, serve},
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
                "        max_output: 512\n      strict:\n        model: strict-model\n        max_context: 4096\n        max_output: 512\n        reasoning_replay: omitted\n        output_limit: {field: max_tokens}\n",
            );
        for entry in &build(&text) {
            let mut context = entry.provider.open_context("c".parse().unwrap()).unwrap();
            let user = Message::User(vec![UserContent::Text {
                text: "find".into(),
            }]);
            let model = entry.profile.model.as_str();
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
            .replace("        output_limit: {field: max_tokens}\n", "");
        let refused = error(&foreign);
        assert!(
            refused.contains("reasoning_replay is not a responses setting"),
            "{refused}"
        );
    }

    const PROXY: &str = r#"
providers:
  local:
    dialect: litellm
    base_url: http://127.0.0.1:11434/v1
    api_key: virtual-key
    tags: [shared]
    models:
      openai:
        model: openai-alias
        codec: responses
        upstream: openai
        tags: []
        max_context: 4096
        max_output: 512
      claude:
        model: claude-alias
        codec: messages
        upstream: anthropic
        max_context: 4096
        max_output: 512
"#;

    fn roundtrip(text: &str) -> Config {
        let config = Config::from_yaml(text).unwrap();
        let reparsed = Config::from_yaml(&config.to_yaml().unwrap()).unwrap();
        assert_eq!(
            serde_json::to_value(&reparsed.providers["local"]).unwrap(),
            serde_json::to_value(&config.providers["local"]).unwrap()
        );
        reparsed
    }

    #[test]
    fn required_request_fields_may_be_supplied_only_by_models() {
        let config = roundtrip(PROXY);
        let entry = &config.providers["local"];
        let ProviderSettings::Litellm(models) = &entry.settings else {
            panic!("LiteLLM fixture");
        };
        assert_eq!(models.defaults.request.codec, Setting::Inherit);
        assert_eq!(models.defaults.dialect.upstream, Setting::Inherit);
        let admitted = entry.admit().unwrap();
        assert_eq!(
            admitted.models()["openai"].wire.codec.name(),
            CodecName::Responses
        );
        assert_eq!(
            admitted.models()["claude"].wire.codec.name(),
            CodecName::Messages
        );
        for (field, value) in [("codec", "responses"), ("upstream", "openai")] {
            let missing = PROXY.replace(&format!("        {field}: {value}\n"), "");
            let refused = error(&missing);
            assert!(
                refused.contains("providers.local")
                    && refused.contains("models.openai")
                    && refused.contains(field)
                    && refused.contains("missing"),
                "{refused}"
            );
            let supplied = with(&missing, &format!("    {field}: {value}"));
            assert!(roundtrip(&supplied).into_runtime().is_ok());
        }
    }

    #[test]
    fn request_roundtrip_preserves_explicit_defaults_and_nullable_fields() {
        use crate::provider::codec::{Codec, responses::ReasoningSummary};
        let openai = with(
            &LOCAL.replace("dialect: compatible", "dialect: openai")
                .replace("codec: chat_completions", "codec: responses")
                .replace("max_output: 512", "max_output: 512\n        reasoning_summary: requested\n        organization: null"),
            "    reasoning_summary: unsupported\n    organization: inherited",
        );
        let config = roundtrip(&openai);
        let admitted = config.providers["local"].admit().unwrap();
        let Codec::Responses(profile) = &admitted.models()["local"].wire.codec else {
            panic!("Responses fixture");
        };
        assert_eq!(profile.reasoning_summary, ReasoningSummary::Requested);
        let ProviderSettings::Openai(models) = &config.providers["local"].settings else {
            panic!("OpenAI fixture");
        };
        let effective = models.defaults.overlay(&models.models["local"].settings);
        assert!(
            openai::Config::resolve(&effective.dialect)
                .unwrap()
                .organization
                .is_none()
        );
    }

    #[test]
    fn routing_roundtrip_merges_only_declared_members_and_can_clear_the_object() {
        use crate::provider::codec::{Codec, chat_completions::DataCollection};
        let router = with(
            &LOCAL.replace("dialect: compatible", "dialect: openrouter")
                .replace("max_output: 512", "max_output: 512\n        routing:\n          order: [chosen]\n          allow_fallbacks: false\n          require_parameters: null\n          quantizations: []\n          fallback_models: []\n          data_collection: allow"),
            "    routing:\n      order: [first, second]\n      allow_fallbacks: true\n      require_parameters: true\n      quantizations: [int8]\n      fallback_models: [other-model]\n      data_collection: deny\n      zdr: true",
        );
        let config = roundtrip(&router);
        let admitted = config.providers["local"].admit().unwrap();
        let Codec::ChatCompletions(profile) = &admitted.models()["local"].wire.codec else {
            panic!("Chat fixture");
        };
        let routing = profile.routing.as_ref().unwrap();
        assert_eq!(routing.provider.order, ["chosen"]);
        assert_eq!(routing.provider.allow_fallbacks, Some(false));
        assert_eq!(routing.provider.require_parameters, None);
        assert_eq!(routing.provider.zdr, Some(true));
        assert_eq!(
            routing.provider.data_collection,
            Some(DataCollection::Allow)
        );
        assert!(routing.provider.quantizations.is_empty());
        assert!(routing.fallback_models.is_empty());

        let cleared = router.replace(
            "        routing:\n          order: [chosen]\n          allow_fallbacks: false\n          require_parameters: null\n          quantizations: []\n          fallback_models: []\n          data_collection: allow",
            "        codec: responses\n        routing: null",
        );
        assert!(roundtrip(&cleared).into_runtime().is_ok());
    }

    #[test]
    fn models_refuse_connection_fields_foreign_settings_and_removed_wrappers() {
        for field in [
            "dialect: compatible",
            "base_url: https://example.com/v1",
            "api_key: key",
            "headers: {x-title: other}",
            "startup_timeout_secs: 30",
            "read_idle_timeout_secs: 30",
            "auth_url: https://auth.example.com",
            "upstream: openai",
            "unknown_setting: true",
            "overrides: {output_limit: omitted}",
        ] {
            let refused = error(&LOCAL.replace(
                "max_output: 512",
                &format!("max_output: 512\n        {field}"),
            ));
            assert!(
                refused.contains("providers.local")
                    && refused.contains("models.local")
                    && refused.contains(field.split_once(':').unwrap().0),
                "{field}: {refused}"
            );
        }
    }

    #[test]
    fn placement_null_restores_the_effective_codec_preset_while_omitted_disables() {
        use crate::provider::codec::{Codec, path};
        let provider = with(
            LOCAL,
            "    output_limit: {field: max_tokens}\n    reasoning_replay: omitted",
        );
        for (selection, expected) in [
            ("null", Some(path("max_completion_tokens"))),
            ("omitted", None),
        ] {
            let text = provider.replace(
                "max_output: 512",
                &format!("max_output: 512\n        output_limit: {selection}"),
            );
            let config = roundtrip(&text);
            let admitted = config.providers["local"].admit().unwrap();
            let Codec::ChatCompletions(profile) = &admitted.models()["local"].wire.codec else {
                panic!("Chat fixture");
            };
            assert_eq!(profile.output_limit, expected);
        }
        let switched = provider.replace("max_output: 512", "max_output: 512\n        codec: responses\n        output_limit: null\n        reasoning_replay: null");
        let config = roundtrip(&switched);
        let admitted = config.providers["local"].admit().unwrap();
        let Codec::Responses(profile) = &admitted.models()["local"].wire.codec else {
            panic!("Responses fixture");
        };
        assert_eq!(profile.output_limit, Some(path("max_output_tokens")));
        let inherited = switched.replace("        reasoning_replay: null\n", "");
        assert!(error(&inherited).contains("reasoning_replay is not a responses setting"));
    }

    #[tokio::test]
    async fn one_litellm_entry_builds_different_wire_profiles_over_one_virtual_key() {
        use crate::provider::{
            dialect::tests::{heads, request},
            http::transport::tests::header_values,
        };
        use futures_util::StreamExt;
        crate::tests::bounded(async {
            let requests = heads(2, async |root| {
                for model in build(&PROXY.replace("http://127.0.0.1:11434/v1", root)) {
                    let mut context = model
                        .provider
                        .open_context("shared-context".parse().unwrap())
                        .unwrap();
                    let mut request = request();
                    request.model = model.profile.model;
                    drop(context.invoke(request).next().await);
                }
            })
            .await;
            let [responses, messages] = &requests[..] else {
                panic!("{requests:?}")
            };
            assert!(responses.starts_with("POST /v1/responses HTTP/1.1\r\n"));
            assert!(messages.starts_with("POST /v1/messages HTTP/1.1\r\n"));
            for request in &requests {
                assert_eq!(
                    header_values(request, "authorization"),
                    ["Bearer virtual-key"]
                );
                assert!(header_values(request, "x-api-key").is_empty());
                assert_eq!(
                    header_values(request, "x-litellm-session-id"),
                    ["shared-context"]
                );
            }
            assert!(header_values(responses, "anthropic-version").is_empty());
            assert_eq!(header_values(messages, "anthropic-version"), ["2023-06-01"]);
            assert!(header_values(responses, "x-litellm-tags").is_empty());
            assert_eq!(header_values(messages, "x-litellm-tags"), ["shared"]);
        })
        .await;
    }

    #[tokio::test]
    async fn one_credential_command_is_shared_across_model_codecs_and_contexts() {
        use crate::provider::{
            dialect::tests::{heads, request},
            http::transport::tests::header_values,
        };
        use futures_util::StreamExt;
        const MESSAGES: &str = "
      messages:
        model: messages-model
        codec: messages
        max_context: 4096
        max_output: 512
";
        crate::tests::bounded(async {
            let directory = tempfile::tempdir().unwrap();
            let count = directory.path().join("count");
            let command = format!("printf x >> '{}'; printf shared-key", count.display());
            let command = serde_json::to_string(&command).unwrap();
            let text = format!(
                "{}{}",
                with(LOCAL, &format!("    api_key: {{command: {command}}}")),
                &MESSAGES[1..]
            );
            let requests = heads(4, async |root| {
                let models = build(&text.replace("http://127.0.0.1:11434/v1", root));
                assert!(!count.exists());
                for context_id in ["parent", "child"] {
                    for model in &models {
                        let context_id = context_id.parse().unwrap();
                        let mut context = model.provider.open_context(context_id).unwrap();
                        let mut request = request();
                        request.model = model.profile.model.clone();
                        drop(context.invoke(request).next().await);
                    }
                }
            })
            .await;
            assert_eq!(std::fs::read(&count).unwrap(), b"x");
            for [chat, messages] in requests.as_chunks::<2>().0 {
                assert!(chat.starts_with("POST /v1/chat/completions HTTP/1.1\r\n"));
                assert_eq!(header_values(chat, "authorization"), ["Bearer shared-key"]);
                assert!(header_values(chat, "x-api-key").is_empty());
                assert!(messages.starts_with("POST /v1/messages HTTP/1.1\r\n"));
                assert_eq!(header_values(messages, "x-api-key"), ["shared-key"]);
                assert!(header_values(messages, "authorization").is_empty());
            }
        })
        .await;
    }

    #[tokio::test]
    async fn litellm_model_upstream_and_tags_scope_signed_chat_history() {
        use crate::provider::{
            http::{
                tests::{chat_request, complete, serve},
                transport::tests::header_values,
            },
            protocol::{Message, ToolResult, UserContent},
        };
        use serde_json::json;
        const SIBLINGS: &str = "
      other_tags:
        model: local-model
        max_context: 4096
        max_output: 512
        tags: [tenant-b]
      other_upstream:
        model: local-model
        max_context: 4096
        max_output: 512
        upstream: bedrock
      openai:
        model: local-model
        max_context: 4096
        max_output: 512
        upstream: openai
";
        crate::tests::bounded(async {
            let block = json!({"type":"thinking","thinking":"plan","signature":"signed"});
            let call = json!({"index":0,"id":"call_a","type":"function",
                "function":{"name":"lookup","arguments":"{}"}});
            let first = vec![
                json!({"choices":[{"index":0,
                    "delta":{"thinking_blocks":[block.clone()],"tool_calls":[call]}}]}),
                json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
            ];
            let answer = vec![
                json!({"choices":[{"index":0,"delta":{"content":"done"},"finish_reason":"stop"}]}),
            ];
            let mut replies = vec![first];
            replies.resize(5, answer);
            let (root, server) = serve(replies).await;
            let proxy = LOCAL
                .replace("dialect: compatible", "dialect: litellm")
                .replace("http://127.0.0.1:11434/v1", &root);
            let proxy = with(&proxy, "    upstream: anthropic\n    tags: [tenant-a]");
            let models = build(&format!("{proxy}{}", &SIBLINGS[1..]));
            let user = Message::User(vec![UserContent::Text {
                text: "find".into(),
            }]);
            let initial = models[0].provider.open_context("initial".parse().unwrap());
            let request = chat_request("local-model", vec![user.clone()]);
            let completion = complete(&mut *initial.unwrap(), request).await;
            let assistant = Message::Assistant(completion.items().to_vec());
            let result = Message::Tool(vec![ToolResult {
                call_id: "call_a".into(),
                name: "lookup".into(),
                result: json!({}),
                is_error: false,
                images: Vec::new(),
            }]);
            let history = vec![user, assistant, result];
            for model in models {
                let context = model.provider.open_context("replay".parse().unwrap());
                let request = chat_request("local-model", history.clone());
                complete(&mut *context.unwrap(), request).await;
            }
            let requests = server.finish().await;
            let bodies: Vec<Value> = requests
                .iter()
                .map(|request| serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1))
                .collect::<Result<_, _>>()
                .unwrap();
            assert_eq!(bodies[1]["messages"][1]["thinking_blocks"], json!([block]));
            for body in &bodies[2..] {
                assert!(
                    body["messages"][1].get("thinking_blocks").is_none(),
                    "{body}"
                );
            }
            assert_eq!(header_values(&requests[1], "x-litellm-tags"), ["tenant-a"]);
            assert_eq!(header_values(&requests[2], "x-litellm-tags"), ["tenant-b"]);
            let cached = |body: &Value| {
                let parts = body["messages"].as_array().unwrap().iter();
                parts
                    .filter_map(|message| message["content"].as_array())
                    .flatten()
                    .any(|part| part.get("cache_control").is_some())
            };
            assert!(cached(&bodies[1]) && cached(&bodies[2]) && cached(&bodies[3]));
            assert!(!cached(&bodies[4]));
        })
        .await;
    }
}
