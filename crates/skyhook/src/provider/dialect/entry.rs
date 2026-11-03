//! Dialect-owned provider declarations, admission, and shared-resource construction.

use std::{fmt::Debug, path::PathBuf, sync::Arc};

use indexmap::IndexMap;
use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{self, DeserializeOwned},
};
use serde_json::{Map, Value};

use super::{
    self as dialect, AdmissionError, BaseUrl, BuildError, Common, Connection, Dialect,
    DialectConfig, ModelError, ModelSpec, Profile, Request, RequestSettings, anthropic, codex,
    compatible, litellm, models::object, openai, openrouter,
};
use crate::identity::HostName;
use crate::provider::{
    ProviderError,
    ProviderErrorKind::Authentication,
    dialect::LoginCommand,
    profile::{ModelName, ModelProfile, ProviderName},
    settings::{Empty, Patch, Settings},
};

/// Provider-only options own connection rules and authentication. Request
/// settings never carry them.
pub(crate) trait ProviderOptions:
    Settings + Clone + Debug + Serialize + DeserializeOwned
{
    /// The entry's connection, at `base_url` when the entry names none.
    fn admit(&self, common: &Common, base_url: BaseUrl) -> Result<Connection, AdmissionError> {
        common.admit(base_url)
    }

    /// The credentials the host's `auth` commands manage for the entry `command`
    /// names, which replace its `api_key`.
    fn login(
        &self,
        _command: LoginCommand,
        _connection: &Connection,
    ) -> Result<Option<codex::Account>, ProviderError> {
        Ok(None)
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
            .partition(|(key, _)| <O::Patch as Patch>::FIELDS.contains(&key.as_str()));
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

    fn admit<C>(
        &self,
        common: &Common,
        home: &ConfigHome,
    ) -> Result<AdmittedEntry<O>, AdmissionError>
    where
        C: Settings<Patch = P> + DialectConfig,
        O: ProviderOptions,
    {
        let connection = self.options.admit(common, C::base_url())?;
        let models = self
            .models
            .iter()
            .map(|(name, model)| {
                let admit = || -> Result<AdmittedModel, ModelError> {
                    let settings = self.defaults.overlay(&model.settings);
                    let request = Request::resolve(&settings.request)?;
                    let wire = C::resolve(&settings.dialect)?.admit(request.codec)?;
                    let wire = request.apply(wire)?;
                    model.validate(&wire)?;
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
            home: home.clone(),
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

            pub(crate) fn model_profile(&self, name: &ModelName) -> Option<&ModelProfile> {
                match self {
                    $(Self::$variant(entry) => entry.models.get(name).map(|model| &model.profile)),+
                }
            }

            pub(crate) fn model_profile_mut(&mut self, name: &ModelName) -> Option<&mut ModelProfile> {
                match self {
                    $(Self::$variant(entry) => entry.models.get_mut(name).map(|model| &mut model.profile)),+
                }
            }

            pub(crate) fn model_profiles(&self) -> Box<dyn Iterator<Item = (&ModelName, &ModelProfile)> + '_> {
                match self {
                    $(Self::$variant(entry) => Box::new(entry.models.iter().map(|(name, model)| (name, &model.profile)))),+
                }
            }

            fn parse(dialect: Dialect, fields: Map<String, Value>) -> Result<Self, String> {
                Ok(match dialect {
                    $(Dialect::$variant => Self::$variant(ProviderModels::parse(fields)?)),+
                })
            }

            fn fields(&self) -> Map<String, Value> {
                match self { $(Self::$variant(entry) => entry.fields()),+ }
            }

            fn admit(&self, common: &Common, home: &ConfigHome) -> Result<AdmittedProvider, AdmissionError> {
                Ok(match self {
                    $(Self::$variant(entry) => {
                        AdmittedProvider::$variant(entry.admit::<$module::Config>(common, home)?)
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

            /// `program` is the host command that manages managed logins.
            pub(crate) fn build(&self, name: &ProviderName, program: &HostName) -> Result<Vec<BuiltModel>, BuildError> {
                match self { $(Self::$variant(entry) => entry.build(name, program)),+ }
            }

            pub(crate) fn login(&self, name: &ProviderName, program: &HostName) -> Result<Option<codex::Account>, ProviderError> {
                match self { $(Self::$variant(entry) => entry.options.login(entry.login_command(name, program), &entry.connection)),+ }
            }
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

/// The configuration file that defined an entry, and so where the state it
/// owns is kept.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ConfigHome {
    /// A document read from no file, homed in the user configuration directory.
    #[default]
    Unfiled,
    File(PathBuf),
}

impl ConfigHome {
    /// Where the credentials `skyhook auth` stores are kept.
    pub fn credentials(&self) -> Result<PathBuf, ProviderError> {
        let directory = match self {
            Self::Unfiled => crate::config::user_config_directory().ok_or_else(|| {
                Authentication.error("Cannot locate the Skyhook configuration directory")
            })?,
            Self::File(file) => file.parent().unwrap_or(file).to_path_buf(),
        };
        Ok(directory.join("credentials"))
    }
}

/// Mutable declarations; model inheritance is resolved only on admission.
#[derive(Clone, Debug)]
pub struct RawProviderConfig {
    pub common: Common,
    pub settings: ProviderSettings,
    pub home: ConfigHome,
}

impl<'de> Deserialize<'de> for RawProviderConfig {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut fields = Map::new();
        let common = crate::yaml::split(deserializer, &mut fields)?;
        let dialect = fields
            .remove("dialect")
            .ok_or_else(|| de::Error::missing_field("dialect"))?;
        let dialect = Dialect::deserialize(dialect).map_err(de::Error::custom)?;
        let settings = ProviderSettings::parse(dialect, fields).map_err(de::Error::custom)?;
        Ok(Self {
            common,
            settings,
            home: ConfigHome::default(),
        })
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
        self.settings.admit(&self.common, &self.home)
    }
}

pub(crate) struct AdmittedEntry<O> {
    connection: Connection,
    models: IndexMap<ModelName, AdmittedModel>,
    options: O,
    home: ConfigHome,
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
    pub provider: Arc<crate::provider::http::HttpProvider>,
}

impl<O: ProviderOptions> AdmittedEntry<O> {
    fn login_command(&self, name: &ProviderName, program: &HostName) -> LoginCommand {
        LoginCommand {
            program: program.clone(),
            provider: name.clone(),
            home: self.home.clone(),
        }
    }

    fn build(
        &self,
        name: &ProviderName,
        program: &HostName,
    ) -> Result<Vec<BuiltModel>, BuildError> {
        if self.models.is_empty() {
            return Ok(Vec::new());
        }
        let mut resources = dialect::Resources::default();
        let authentication = self
            .options
            .login(self.login_command(name, program), &self.connection)?
            .map(codex::Account::into_headers);
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
        media::ImageLimits,
        provider::{
            codec::CodecName,
            dialect::{Sourced, compatible},
            settings::Setting,
        },
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
                let model = runtime.select_model(&"local/local".parse().unwrap())?;
                crate::agent::HarnessBuilder::configured(&model, ".", "test".parse().unwrap())
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
        (admitted.build(&"local".parse().unwrap(), &"skyhook".parse().unwrap())).unwrap()
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
                "    headers: {x-title: \"a\\nb\"}\n",
                "`providers.local`: headers.x-title: must be a valid header value",
            ),
            (
                "    headers: {x-count: 1}\n",
                "`providers.local.headers.x-count`",
            ),
            (
                "    headers: {X-Title: a, x-title: b}\n",
                "`providers.local`: headers: `x-title` is named twice",
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
        // Admission checks a model's limits.
        let limits = "max_context: 4096\n        max_output: 512";
        let refused = error(&LOCAL.replace(limits, "max_context: 4096\n        max_output: 4096"));
        assert!(
            refused.contains("providers.local") && refused.contains("max_output must be smaller"),
            "{refused}"
        );
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
    }

    /// A value missing when the provider is built is reported with its provider
    /// and field.
    #[test]
    fn build_errors_name_the_provider_and_field() {
        let field = "    headers: {x-token: {env: SKYHOOK_TEST_UNSET_HEADER}}";
        let error = Config::from_yaml(&with(LOCAL, field))
            .unwrap()
            .into_runtime()
            .and_then(|runtime| {
                let model = runtime.select_model(&"local/local".parse().unwrap())?;
                crate::agent::HarnessBuilder::configured(&model, ".", "test".parse().unwrap())
            })
            .err()
            .unwrap()
            .to_string();
        let expected = "provider `local` could not be initialized: headers.x-token: environment variable `SKYHOOK_TEST_UNSET_HEADER` is required";
        assert!(error.contains(expected), "{error}");
    }

    #[test]
    fn entries_name_a_known_dialect_and_codec_and_an_endpoint_it_needs() {
        let root = "base_url: https://example.com/v1";
        let model =
            "models:\n  test:\n    model: fixture\n    max_context: 4096\n    max_output: 512";
        for text in [
            format!("codec: responses\n{root}"),
            format!("dialect: openai\n{root}"),
            format!("dialect: unknown\ncodec: messages\n{root}"),
            format!("dialect: compatible\ncodec: completions\n{root}"),
            // Only a dialect with its own service has a default root.
            "dialect: compatible\ncodec: responses".to_owned(),
        ] {
            assert!(!admits(&format!("{text}\n{model}")), "{text}");
        }
        for text in [
            format!("dialect: openai\ncodec: responses\n{root}"),
            "dialect: openai\ncodec: responses".to_owned(),
            "dialect: anthropic\ncodec: messages".to_owned(),
            "dialect: openrouter\ncodec: chat_completions".to_owned(),
        ] {
            assert!(admits(&format!("{text}\n{model}")), "{text}");
        }
    }

    /// Each model is served with its own settings: one caps output under
    /// `max_tokens` and replays no reasoning, the other keeps the entry's.
    #[tokio::test]
    async fn model_settings_specialise_the_provider_per_model() {
        use crate::provider::{
            codec::common::{
                self,
                tests::{tool_result, user},
            },
            http::tests::chat_request,
            protocol::{AssistantItem, Binding, Message, ReplayFormat, ToolCall},
        };
        use serde_json::json;
        let text = LOCAL.replace(
            "        max_output: 512\n",
            "        max_output: 512\n      strict:\n        model: strict-model\n        max_context: 4096\n        max_output: 512\n        reasoning_replay: null\n        output_limit: max_tokens\n",
        );
        let mut requests = Vec::new();
        for entry in &build(&text) {
            let model = entry.profile.model.as_str();
            let format = ReplayFormat::ChatText;
            let plan = json!({"text":"plan"});
            let replay = common::replay(format, model, &entry.provider.scope, plan, Binding::Free);
            let call = ToolCall::new("call_a", "lookup", json!({})).unwrap();
            let assistant = Message::Assistant(vec![
                AssistantItem::reasoning("r", 0, "plan", Some(replay)),
                AssistantItem::tool_call("c", 1, call),
            ]);
            let result = tool_result("call_a", "lookup", json!({}), vec![], false);
            let request = chat_request(model, vec![user("find"), assistant, result]);
            requests.push(entry.provider.sent(request).await.unwrap().body);
        }
        let sent = |request: &Value, field: &str| request.get(field).is_some();
        let replayed = |request: &Value| sent(&request["messages"][1], "reasoning_content");
        let [local, strict] = &requests[..] else {
            panic!("{requests:?}")
        };
        assert_eq!(
            (&local["model"], &strict["model"]),
            (&json!("local-model"), &json!("strict-model"))
        );
        assert!(sent(local, "max_completion_tokens") && !sent(local, "max_tokens"));
        assert!(sent(strict, "max_tokens") && !sent(strict, "max_completion_tokens"));
        assert!(replayed(local) && !replayed(strict));
    }

    const PROXY: &str = r#"
providers:
  local:
    dialect: litellm
    base_url: http://127.0.0.1:11434/v1
    api_key: virtual-key
    tags: [shared]
    image_limits: {max_bytes: 1000000}
    models:
      openai:
        model: openai-alias
        codec: responses
        upstream: openai
        tags: []
        image_limits: {max_edge: 2000}
        max_context: 4096
        max_output: 512
      anthropic:
        model: anthropic-alias
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
            admitted.models()["anthropic"].wire.codec.name(),
            CodecName::Messages
        );
        // Image limits merge per limit over the dialect's.
        let images = |model: &str| admitted.models()[model].wire.images;
        assert_eq!(images("openai"), ImageLimits::new(2000, 1_000_000));
        let edge = compatible::IMAGES.max_edge.get();
        assert_eq!(images("anthropic"), ImageLimits::new(edge, 1_000_000));
        // Limits other than the dialect's partition sealed replay; its own do not.
        let scope = |text: &str| {
            let admitted = Config::from_yaml(text).unwrap().providers["local"].admit();
            admitted.unwrap().models()["anthropic"].wire.scope.clone()
        };
        let unset = PROXY.replace("    image_limits: {max_bytes: 1000000}\n", "");
        let max_bytes = compatible::IMAGES.max_bytes.to_string();
        assert_ne!(scope(PROXY), scope(&unset));
        assert_eq!(scope(&unset), scope(&PROXY.replace("1000000", &max_bytes)));
        // Limits read as the ones in effect, so only an effective change partitions.
        let over = |max_bytes: &str| PROXY.replace("1000000", max_bytes);
        assert_eq!(scope(&over("9000000")), scope(&over("90000000")));
        let over = Config::from_yaml(&over("9000000").replace("2000", "70000")).unwrap();
        let admitted = over.providers["local"].admit().unwrap();
        let effective = ImageLimits::new(65_535, crate::media::MAX_IMAGE_BYTES);
        assert_eq!(admitted.models()["openai"].wire.images, effective);
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
    fn models_refuse_connection_fields_and_foreign_settings() {
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

    #[tokio::test]
    async fn one_credential_command_is_shared_across_model_codecs_and_contexts() {
        use crate::provider::dialect::tests::request;
        const MESSAGES: &str = "
      messages:
        model: messages-model
        codec: messages
        max_context: 4096
        max_output: 512
";
        let directory = tempfile::tempdir().unwrap();
        let count = directory.path().join("count");
        let command = format!("printf x >> '{}'; printf shared-key", count.display());
        let command = serde_json::to_string(&command).unwrap();
        let text = format!(
            "{}{}",
            with(LOCAL, &format!("    api_key: {{command: {command}}}")),
            &MESSAGES[1..]
        );
        let models = build(&text);
        assert!(!count.exists());
        // Each request opens a context of its own.
        for _ in 0..2 {
            let mut sent = Vec::new();
            for model in &models {
                let mut request = request();
                request.model = model.profile.model.clone();
                sent.push(model.provider.sent(request).await.unwrap());
            }
            let [chat, messages] = &sent[..] else {
                unreachable!()
            };
            assert_eq!(chat.endpoint.path(), "/v1/chat/completions");
            assert_eq!(chat.header("authorization"), ["Bearer shared-key"]);
            assert!(chat.header("x-api-key").is_empty());
            assert_eq!(messages.endpoint.path(), "/v1/messages");
            assert_eq!(messages.header("x-api-key"), ["shared-key"]);
            assert_eq!(messages.header("anthropic-version"), ["2023-06-01"]);
            assert!(messages.header("authorization").is_empty());
        }
        assert_eq!(std::fs::read(&count).unwrap(), b"x");
    }

    /// A model's own upstream and tags scope signed history apart from the
    /// entry's, and its tags replace the entry's on the wire.
    #[tokio::test]
    async fn litellm_model_upstream_and_tags_scope_signed_chat_history() {
        use crate::provider::dialect::tests::request;
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
        let proxy = LOCAL.replace("dialect: compatible", "dialect: litellm");
        let proxy = with(&proxy, "    upstream: anthropic\n    tags: [tenant-a]");
        let text = format!("{proxy}{}", &SIBLINGS[1..]);
        let config = Config::from_yaml(&text).unwrap();
        let admitted = config.providers["local"].admit().unwrap();
        let scopes: Vec<_> = (admitted.models().values())
            .map(|model| &model.wire.scope)
            .collect();
        for other in &scopes[1..] {
            assert_ne!(scopes[0], *other);
        }
        let models = build(&text);
        let tags = async |model: &BuiltModel| {
            let sent = model.provider.sent(request()).await.unwrap();
            sent.header("x-litellm-tags").join(",")
        };
        assert_eq!(tags(&models[0]).await, "tenant-a");
        assert_eq!(tags(&models[1]).await, "tenant-b");
    }

    /// Request settings reach the body on every codec a dialect speaks: the service
    /// tier leads it; a Responses model asks for its dialect's summary unless it
    /// chooses another or null; a cache lifetime rides wherever the vendor marks
    /// breakpoints, and OpenRouter's Responses asks for automatic caching unless
    /// explicit caching is selected.
    #[tokio::test]
    async fn request_settings_reach_every_dialect_and_codec() {
        use crate::provider::dialect::tests::{ROOT, request};
        use serde_json::json;
        /// Every prompt-cache field in the body, as `{field: value}`.
        fn caching(value: &Value, found: &mut Vec<Value>) {
            match value {
                Value::Object(fields) => {
                    for (key, value) in fields {
                        match key.as_str() {
                            "cache_control"
                            | "prompt_cache_breakpoint"
                            | "prompt_cache_options" => {
                                found.push(json!({key: value}));
                            }
                            _ => caching(value, found),
                        }
                    }
                }
                Value::Array(items) => items.iter().for_each(|item| caching(item, found)),
                _ => {}
            }
        }
        let hour = json!({"cache_control": {"type": "ephemeral", "ttl": "1h"}});
        let automatic = json!({"cache_control": {"type": "ephemeral"}});
        let explicit = json!({"prompt_cache_options": {"mode": "explicit"}});
        let breakpoint = json!({"prompt_cache_breakpoint": {"mode": "explicit"}});
        // Per entry: its dialect and fields, then each model's codec and fields, the
        // summary it asks for, and the prompt-cache fields it sends, if checked.
        let entries = [
            (
                "compatible",
                "",
                vec![
                    ("chat_completions", "", None, Some(vec![])),
                    ("responses", "", Some("auto"), Some(vec![])),
                    (
                        "responses",
                        "reasoning_summary: concise",
                        Some("concise"),
                        None,
                    ),
                    ("responses", "reasoning_summary: null", None, None),
                    ("messages", "", None, None),
                ],
            ),
            (
                "openai",
                "",
                vec![
                    ("chat_completions", "", None, Some(vec![])),
                    (
                        "chat_completions",
                        "explicit_caching: true",
                        None,
                        Some(vec![breakpoint.clone(), explicit.clone()]),
                    ),
                    ("responses", "", Some("auto"), Some(vec![])),
                    (
                        "responses",
                        "explicit_caching: true",
                        Some("auto"),
                        Some(vec![breakpoint.clone(), explicit.clone()]),
                    ),
                    ("responses", "reasoning_summary: null", None, None),
                ],
            ),
            (
                "anthropic",
                "cache_ttl: 1h",
                vec![("messages", "", None, Some(vec![hour.clone()]))],
            ),
            (
                "litellm",
                "upstream: openai",
                vec![
                    ("chat_completions", "", None, None),
                    ("responses", "", Some("auto"), None),
                    (
                        "responses",
                        "reasoning_summary: detailed",
                        Some("detailed"),
                        None,
                    ),
                    ("messages", "", None, None),
                ],
            ),
            (
                "openrouter",
                "",
                vec![
                    (
                        "chat_completions",
                        "cache_ttl: 1h",
                        None,
                        Some(vec![hour.clone()]),
                    ),
                    (
                        "chat_completions",
                        "explicit_caching: true",
                        None,
                        Some(vec![automatic.clone(), explicit.clone()]),
                    ),
                    (
                        "responses",
                        "cache_ttl: 1h",
                        Some("auto"),
                        Some(vec![hour.clone()]),
                    ),
                    (
                        "responses",
                        "reasoning_summary: null",
                        None,
                        Some(vec![automatic]),
                    ),
                    (
                        "responses",
                        "explicit_caching: true",
                        Some("auto"),
                        Some(vec![breakpoint, explicit]),
                    ),
                    ("messages", "cache_ttl: 1h", None, Some(vec![hour])),
                ],
            ),
        ];
        for (dialect, fields, models) in &entries {
            let text: String = models
                .iter()
                .enumerate()
                .map(|(index, (codec, fields, ..))| {
                    format!(
                        "      m{index}:\n        model: m{index}\n        codec: {codec}\n        max_context: 4096\n        max_output: 512\n        {fields}\n"
                    )
                })
                .collect();
            let text = format!(
                "providers:\n  local:\n    dialect: {dialect}\n    base_url: {ROOT}\n    api_key: k\n    service_tier: priority\n    {fields}\n    models:\n{text}"
            );
            let built = build(&text);
            assert_eq!(built.len(), models.len());
            for (model, (codec, fields, summary, expected_caching)) in built.iter().zip(models) {
                let body = model.provider.sent(request()).await.unwrap().body;
                let case = format!("{dialect} {codec} {fields}");
                let object = body.as_object().unwrap();
                assert_eq!(object.keys().next().unwrap(), "service_tier", "{case}");
                assert_eq!(body["service_tier"], "priority", "{case}");
                let sent_summary = body.pointer("/reasoning/summary");
                assert_eq!(sent_summary, summary.map(Value::from).as_ref(), "{case}");
                if let Some(expected) = expected_caching {
                    let mut found = Vec::new();
                    caching(&body, &mut found);
                    found.sort_by_key(Value::to_string);
                    found.dedup();
                    assert_eq!(&found, expected, "{case}");
                }
            }
        }
    }
}
