//! Dialects. Each module owns its settings, the codecs it speaks and the
//! presets for each, its endpoint, credentials, and transport conventions.
//! `compatible` is the flexible one: standards-following servers need only an
//! endpoint, and its entry may still select each placement dimension.

use std::convert::Infallible;

use reqwest::header::{ACCEPT, HeaderValue};
use serde::{Deserialize, Serialize};

use crate::{
    named_enum::named_enum,
    provider::{
        ProviderError,
        codec::{Codec, CodecName, chat_completions, messages, responses},
        http::{Build, Headers, HttpProvider, Transport, headers::Value, transport::EVENT_STREAM},
    },
};

pub mod anthropic;
mod api_key;
pub mod codex;
pub mod compatible;
pub(crate) mod config;
pub mod litellm;
pub mod openai;
pub mod openrouter;
mod overrides;

pub(crate) use api_key::{Scheme, key};
pub(crate) use config::Connection;
use config::Pending;
pub use config::{
    AdmissionError, Common, EndpointError, ModelError, ModelSpec, Sourced, ValueError, ValueField,
    ValueProblem,
};
pub use overrides::{OverrideError, OverrideKey, Overrides, Placement, PlacementError};

named_enum! {
    /// The preset a provider entry names.
    #[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
    pub enum Dialect {
        Compatible = "compatible",
        Openai = "openai",
        Anthropic = "anthropic",
        Codex = "codex",
        Litellm = "litellm",
        Openrouter = "openrouter",
    }
}

/// What the entry's `base_url` means to the dialect; the codec's path is
/// appended either way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BaseUrl {
    Required,
    /// The dialect's own service, unless the entry names another root.
    Default(&'static str),
}

/// The wire conventions an admitted entry fixes for one codec.
#[derive(Clone, Debug)]
pub(crate) struct Profile {
    pub codec: Codec,
    pub transport: Transport,
    pub base_url: BaseUrl,
    /// Names the dialect in replay scope, with whatever else changes the
    /// meaning of replayed state.
    pub scope: String,
    /// Fixed headers: the codec's own, then the dialect's constants and those
    /// the entry's settings derive (workspace, tags, identity, betas).
    pub headers: Headers,
}

impl Profile {
    pub(crate) fn new(codec: Codec, transport: Transport, dialect: Dialect) -> Self {
        Self {
            headers: codec.headers(),
            codec,
            transport,
            base_url: BaseUrl::Required,
            scope: dialect.as_str().to_owned(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("dialect {dialect} does not speak {codec}")]
pub struct UnsupportedCodec {
    pub dialect: Dialect,
    pub codec: CodecName,
}

/// Why an admitted entry's provider could not be constructed.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error(transparent)]
    Value(#[from] ValueError),
    #[error(transparent)]
    Provider(#[from] ProviderError),
}

/// The base conventions of a family, which `compatible` serves as they are.
pub(crate) fn base(codec: CodecName) -> Codec {
    match codec {
        CodecName::ChatCompletions => {
            Codec::ChatCompletions(chat_completions::Dialect::compatible())
        }
        CodecName::Responses => Codec::Responses(responses::Dialect::stateless()),
        CodecName::Messages => Codec::Messages(messages::Dialect::anthropic()),
    }
}

/// What every dialect's settings provide.
pub(crate) trait DialectConfig: Serialize + serde::de::DeserializeOwned {
    /// The dialect's own checks and the conventions it fixes for `codec`. The
    /// common fields, the endpoint and the models are checked once for every
    /// dialect, so nothing here is proved again at build.
    fn admit(&self, common: &Common, codec: CodecName) -> Result<Profile, DialectError>;

    /// The credential headers: by default the entry's `api_key` as a bearer
    /// token, absent for a keyless endpoint.
    fn credentials(
        &self,
        _: &Profile,
        connection: &Connection,
    ) -> Result<Headers<Pending>, BuildError> {
        Ok(key(connection, Scheme::Bearer))
    }

    /// The provider for an admitted profile. Environment values are read here;
    /// a command runs on the first request that sends its value.
    fn provider(
        &self,
        name: &str,
        profile: Profile,
        connection: &Connection,
    ) -> Result<HttpProvider, BuildError> {
        let credentials = self.credentials(&profile, connection)?;
        build(name, profile, connection, credentials)
    }
}

/// Build the provider at the admitted endpoint. Header sources compose in
/// order, each replacing a header or joining a list header: the profile's, the
/// entry's `headers`, `credentials`, then the stream the transport reads. Only
/// the environment values that remain are read.
fn build(
    name: &str,
    profile: Profile,
    connection: &Connection,
    credentials: Headers<Pending>,
) -> Result<HttpProvider, BuildError> {
    let ready = |value| Ok::<_, Infallible>(Pending::Ready(value));
    let Ok(mut headers) = profile.headers.try_map(ready);
    for (header, value) in &connection.headers {
        let field = ValueField::Header(header.clone());
        headers.insert(header.clone(), value.header(field, None));
    }
    headers.extend(credentials);
    let stream = Value::Fixed(HeaderValue::from_static(EVENT_STREAM));
    headers.insert(ACCEPT, Pending::Ready(stream));
    let headers = headers.try_map(Pending::read)?;
    Ok(HttpProvider::new(Build {
        name,
        scope_tag: &profile.scope,
        endpoint: connection.endpoint.clone(),
        codec: profile.codec,
        session: profile.transport.session,
        errors: profile.transport.errors,
        headers,
        timeouts: connection.timeouts,
    })?)
}

macro_rules! dialects {
    ($($variant:ident => $module:ident),+ $(,)?) => {
        /// A dialect's own settings.
        #[derive(Clone, Debug, PartialEq)]
        pub enum DialectSettings {
            $($variant($module::Config)),+
        }

        /// Why a dialect refused its settings or the codec.
        #[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
        pub enum DialectError {
            #[error(transparent)]
            Codec(#[from] UnsupportedCodec),
            $(#[error(transparent)] $variant(#[from] $module::Error)),+
        }

        impl DialectSettings {
            /// The dialect's fields, from the entry's remaining YAML.
            pub(crate) fn parse(
                dialect: Dialect,
                fields: serde_json::Value,
            ) -> Result<Self, serde_path_to_error::Error<serde_json::Error>> {
                Ok(match dialect {
                    $(Dialect::$variant => Self::$variant(serde_path_to_error::deserialize(fields)?)),+
                })
            }

            pub fn dialect(&self) -> Dialect {
                match self {
                    $(Self::$variant(_) => Dialect::$variant),+
                }
            }

            pub(crate) fn admit(&self, common: &Common, codec: CodecName) -> Result<Profile, DialectError> {
                match self {
                    $(Self::$variant(config) => config.admit(common, codec)),+
                }
            }

            pub(crate) fn provider(
                &self,
                name: &str,
                profile: Profile,
                connection: &Connection,
            ) -> Result<HttpProvider, BuildError> {
                match self {
                    $(Self::$variant(config) => config.provider(name, profile, connection)),+
                }
            }

            /// The dialect's fields, as YAML at the entry's level.
            pub(crate) fn fields(&self) -> serde_json::Map<String, serde_json::Value> {
                let value = match self {
                    $(Self::$variant(config) => serde_json::to_value(config)),+
                };
                match value {
                    Ok(serde_json::Value::Object(fields)) => fields,
                    _ => unreachable!("dialect settings serialize as an object"),
                }
            }
        }
    };
}

dialects! {
    Compatible => compatible,
    Openai => openai,
    Anthropic => anthropic,
    Codex => codex,
    Litellm => litellm,
    Openrouter => openrouter,
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::provider::{
        Provider,
        http::transport::tests::{Plan, Server, header_values},
    };

    /// An entry at `root`, keyed or anonymous.
    pub(crate) fn common(root: &str, api_key: Option<Sourced>) -> Common {
        Common {
            base_url: Some(root.to_owned()),
            api_key,
            ..Common::default()
        }
    }

    /// Admit and build, as configuration does.
    pub(crate) fn provider(
        name: &str,
        settings: &impl DialectConfig,
        codec: CodecName,
        common: &Common,
    ) -> HttpProvider {
        let profile = settings.admit(common, codec).unwrap();
        let connection = common.admit(profile.base_url, codec).unwrap();
        settings.provider(name, profile, &connection).unwrap()
    }

    /// The head of one request a keyed entry sends.
    pub(crate) async fn head(
        settings: &impl DialectConfig,
        codec: CodecName,
        api_key: &str,
    ) -> String {
        let mut heads = heads(1, async |root| {
            let common = common(root, Some(Sourced::Literal(api_key.into())));
            let provider = provider("test", settings, codec, &common);
            let mut context = provider.open_context("ctx".parse().unwrap()).unwrap();
            drop(futures_util::StreamExt::next(&mut context.invoke(request())).await);
        })
        .await;
        heads.remove(0)
    }

    /// The raw heads of `count` requests to a server that answers each with an
    /// empty stream, after `invoke` ran them.
    pub(crate) async fn heads(count: usize, invoke: impl AsyncFnOnce(&str)) -> Vec<String> {
        let plans = (0..count).map(|_| Plan::empty_stream()).collect();
        let server = Server::start(plans).await;
        let root = format!("{}/v1", server.url.trim_end_matches("/responses"));
        invoke(&root).await;
        server
            .finish()
            .await
            .into_iter()
            .map(|request| request.split_once("\r\n\r\n").unwrap().0.to_owned())
            .collect()
    }

    /// The kind `settings` read a rejection of `codec` as.
    pub(crate) fn rejected(
        settings: &impl DialectConfig,
        codec: CodecName,
        status: u16,
        body: serde_json::Value,
        retry_after: Option<std::time::Duration>,
    ) -> crate::provider::ProviderErrorKind {
        let profile = settings.admit(&Common::default(), codec).unwrap();
        let rejection = crate::provider::http::transport::Rejection {
            status,
            body,
            retry_after,
        };
        let signals = profile.transport.errors;
        profile.codec.error(&rejection, signals).kind
    }

    /// The kind `settings` read a mid-stream error event of `codec` as.
    pub(crate) fn streamed(
        settings: &impl DialectConfig,
        codec: CodecName,
        event: serde_json::Value,
    ) -> crate::provider::ProviderErrorKind {
        let profile = settings.admit(&Common::default(), codec).unwrap();
        let scope = crate::provider::codec::common::tests::scope();
        let signals = profile.transport.errors;
        let body = serde_json::Value::Null;
        let mut decoder = profile.codec.decoder("model".into(), scope, &body, signals);
        let event = crate::provider::http::transport::SseEvent {
            event: None,
            data: event.to_string(),
        };
        decoder.decode(&event).unwrap_err().kind
    }

    pub(crate) fn request() -> crate::provider::protocol::ModelRequest {
        crate::provider::protocol::ModelRequest {
            max_output_tokens: Some(16),
            ..crate::provider::codec::common::tests::request("test-model")
        }
    }

    #[tokio::test]
    async fn fixed_headers_merge_in_order() {
        let heads = heads(2, async |root| {
            let mut common = common(root, None);
            common.headers = [
                ("x-literal".to_owned(), Sourced::Literal("plain".into())),
                (
                    "x-lazy".to_owned(),
                    Sourced::Command {
                        command: "printf lazy".into(),
                    },
                ),
                // The entry's headers win over the transport's constants.
                (
                    "anthropic-version".to_owned(),
                    Sourced::Literal("2099-01-01".into()),
                ),
            ]
            .into_iter()
            .collect();
            let settings = anthropic::Config::default();
            let provider = provider("test", &settings, CodecName::Messages, &common);
            let mut context = provider.open_context("c".parse().unwrap()).unwrap();
            for _ in 0..2 {
                drop(futures_util::StreamExt::next(&mut context.invoke(request())).await);
            }
        })
        .await;
        for head in &heads {
            assert_eq!(header_values(head, "x-literal"), ["plain"]);
            assert_eq!(header_values(head, "x-lazy"), ["lazy"]);
            assert_eq!(header_values(head, "anthropic-version"), ["2099-01-01"]);
            assert!(header_values(head, "x-api-key").is_empty());
        }
    }

    /// The key replaces an entry header of its name, and a codec placement
    /// replaces the key; a replaced command never runs, and a replaced
    /// environment value is never read.
    #[tokio::test]
    async fn placements_replace_credentials_which_replace_entry_headers() {
        use crate::provider::codec::header;
        let dir = tempfile::tempdir().unwrap();
        let count = dir.path().join("count");
        let command = format!("printf x >> '{}'; printf entry", count.to_str().unwrap());
        let placed = compatible::Config(Overrides {
            cache_key: Some(header("x-api-key")),
            ..Overrides::default()
        });
        let heads = heads(2, async |root| {
            let mut common = common(root, Some(Sourced::Literal("k".into())));
            let entry = Sourced::Command { command };
            common.headers.insert("x-api-key".into(), entry);
            let unset = Sourced::Env {
                env: "SKYHOOK_TEST_UNSET_X".into(),
            };
            common.headers.insert("accept".into(), unset);
            for settings in [compatible::Config::default(), placed] {
                let provider = provider("test", &settings, CodecName::Messages, &common);
                let mut context = provider.open_context("ctx".parse().unwrap()).unwrap();
                drop(futures_util::StreamExt::next(&mut context.invoke(request())).await);
            }
        })
        .await;
        assert_eq!(header_values(&heads[0], "x-api-key"), ["k"]);
        assert_eq!(header_values(&heads[1], "x-api-key"), ["ctx"]);
        assert!(!count.exists());
    }
}
