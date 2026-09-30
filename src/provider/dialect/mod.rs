//! Dialects. Each module owns its settings, the codecs it speaks and the
//! presets for each, its endpoint, credentials, and transport conventions.
//! `compatible` is the flexible one: standards-following servers need only an
//! endpoint, and its entry may still select each placement dimension.

use std::convert::Infallible;

use reqwest::header::{ACCEPT, HeaderName, HeaderValue};
use serde::Serialize;

use crate::{
    named_enum::named_enum,
    provider::{
        ProviderError,
        codec::{Codec, CodecName},
        http::{Build, Headers, HttpProvider, Transport, headers::Value, transport::EVENT_STREAM},
    },
};

pub mod anthropic;
mod api_key;
pub mod codex;
pub mod compatible;
pub(crate) mod config;
pub(crate) mod entry;
pub mod litellm;
mod login;
pub(crate) mod models;
pub mod openai;
pub mod openrouter;
mod placements;

pub use crate::provider::http::headers::{HeaderText, InvalidHeaderText, ValueField};
pub use crate::provider::settings::{MissingSetting, Setting};
pub(crate) use api_key::Scheme;
pub(crate) use config::Connection;
pub use config::{
    AdmissionError, Common, EndpointError, ModelError, Sourced, ValueError, ValueProblem,
};
use config::{Pending, Sources};
pub(crate) use entry::AdmittedProvider;
pub use entry::{ConfigHome, ProviderModels, ProviderSettings, RawProviderConfig};
pub use login::{
    AuthStatus, Login, LoginCommand, LoginError, LoginReason, LoginRequired, Usage, UsageWindow,
};
pub use models::{ModelSpec, Request, RequestPatch, RequestSettings};
pub use placements::{Placement, PlacementError, PlacementKey, Placements, PlacementsPatch};

named_enum! {
    /// The preset a provider entry names.
    #[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
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

/// The complete wire conventions of an admitted model.
#[derive(Clone, Debug)]
pub(crate) struct Profile {
    pub codec: Codec,
    pub transport: Transport,
    /// Names the dialect in replay scope, with whatever else changes the
    /// meaning of replayed state.
    pub scope: String,
    /// Fixed headers: the codec's own, then the dialect's constants and those
    /// the entry's settings derive (workspace, tags, identity, betas).
    pub headers: Headers,
    /// How the entry's `api_key` travels.
    pub key: Scheme,
    /// The service beneath an API root that serves several, which the codec's
    /// path joins (Codex's `codex`).
    pub service: Option<&'static str>,
}

impl Profile {
    pub(crate) fn new(codec: Codec, transport: Transport, dialect: Dialect) -> Self {
        Self {
            headers: codec.headers(),
            codec,
            transport,
            scope: dialect.as_str().to_owned(),
            key: Scheme::Bearer,
            service: None,
        }
    }

    /// The codec's path beneath the API root.
    fn path(&self) -> String {
        let codec = self.codec.name().path_suffix();
        match self.service {
            Some(service) => format!("{service}/{codec}"),
            None => codec.to_owned(),
        }
    }

    /// Partition private replay for nonsecret routing and tenant selectors.
    pub(crate) fn discriminate(&mut self, identity: &impl Serialize) {
        self.scope.push(':');
        self.scope
            .push_str(&serde_json::to_string(identity).expect("replay identity serializes"));
    }

    /// Send `value` as `name` on every request.
    pub(crate) fn fixed(&mut self, name: &'static str, value: HeaderValue) {
        let name = HeaderName::from_static(name);
        self.headers.insert(name, Value::Fixed(value));
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("dialect {dialect} does not speak {codec}")]
pub struct UnsupportedCodec {
    pub dialect: Dialect,
    pub codec: CodecName,
}

/// Why a dialect refused its settings or the codec.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DialectError {
    #[error(transparent)]
    Codec(#[from] UnsupportedCodec),
    #[error(transparent)]
    Openai(#[from] openai::Error),
    #[error(transparent)]
    Codex(#[from] codex::Error),
    #[error(transparent)]
    Openrouter(#[from] openrouter::Error),
}

/// Why an admitted entry's provider could not be constructed.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error(transparent)]
    Value(#[from] ValueError),
    #[error(transparent)]
    Provider(#[from] ProviderError),
}

/// What every dialect's settings provide.
pub(crate) trait DialectConfig {
    /// Resolve wire conventions after inheritance. Connection and model-profile
    /// validation are shared, and no admission is repeated at build.
    fn admit(&self, codec: CodecName) -> Result<Profile, DialectError>;
}

/// Build the provider at the admitted endpoint. Header sources compose in
/// order, each replacing a header or joining a list header: the profile's, the
/// entry's `headers`, `credentials`, then the stream the transport reads. Only
/// the environment values that remain are read.
pub(crate) fn build(
    name: &str,
    profile: Profile,
    connection: &Connection,
    credentials: Headers<Pending>,
    resources: &mut Resources,
) -> Result<HttpProvider, BuildError> {
    let endpoint = connection.url(&profile.path());
    let ready = |value| Ok::<_, Infallible>(Pending::Ready(value));
    let Ok(mut headers) = profile.headers.try_map(ready);
    headers.extend(connection.configured_headers());
    headers.extend(credentials);
    let stream = Value::Fixed(HeaderValue::from_static(EVENT_STREAM));
    headers.insert(ACCEPT, Pending::Ready(stream));
    let headers = headers.try_map(|pending| resources.sources.read(pending))?;
    Ok(HttpProvider::new(Build {
        name,
        scope_tag: &profile.scope,
        client: resources.client.clone(),
        endpoint,
        codec: profile.codec,
        transport: profile.transport,
        headers,
        timeouts: connection.timeouts,
    }))
}

pub(crate) struct Resources {
    client: reqwest::Client,
    sources: Sources,
}

impl Resources {
    pub(crate) fn new() -> Result<Self, BuildError> {
        Ok(Self {
            client: crate::provider::http::transport::client()?,
            sources: Sources::default(),
        })
    }
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
        placed(name, settings, &Placements::default(), codec, common)
    }

    /// Admit, apply `placements`, and build, as configuration does.
    pub(crate) fn placed(
        name: &str,
        settings: &impl DialectConfig,
        placements: &Placements,
        codec: CodecName,
        common: &Common,
    ) -> HttpProvider {
        let mut profile = settings.admit(codec).unwrap();
        profile.codec = placements.apply(profile.codec).unwrap();
        let connection = common.admit(BaseUrl::Required).unwrap();
        let credentials = connection.credentials(profile.key);
        let mut resources = Resources::new().unwrap();
        build(name, profile, &connection, credentials, &mut resources).unwrap()
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
        let profile = settings.admit(codec).unwrap();
        let rejection = crate::provider::http::transport::Rejection {
            status,
            body,
            retry_after,
        };
        let signals = profile.transport.errors;
        profile.codec.error(&rejection, signals).kind()
    }

    /// The kind `settings` read a mid-stream error event of `codec` as.
    pub(crate) fn streamed(
        settings: &impl DialectConfig,
        codec: CodecName,
        event: serde_json::Value,
    ) -> crate::provider::ProviderErrorKind {
        let profile = settings.admit(codec).unwrap();
        let scope = crate::provider::codec::common::tests::scope();
        let signals = profile.transport.errors;
        let mut decoder = profile.codec.decoder("model".into(), scope, b"", signals);
        let event = crate::provider::http::transport::SseEvent {
            event: None,
            data: event.to_string(),
        };
        decoder.decode(&event).unwrap_err().kind()
    }

    pub(crate) fn request() -> crate::provider::protocol::ModelRequest {
        crate::provider::protocol::ModelRequest {
            max_output_tokens: std::num::NonZeroU64::new(16),
            ..crate::provider::codec::common::tests::request("test-model")
        }
    }

    #[test]
    fn dialects_refuse_codecs_they_do_not_speak() {
        use CodecName::*;
        let anthropic = anthropic::Config::default();
        let codex = codex::Config::default();
        for (settings, dialect, codec) in [
            (
                &anthropic as &dyn DialectConfig,
                Dialect::Anthropic,
                ChatCompletions,
            ),
            (&anthropic, Dialect::Anthropic, Responses),
            (&openai::Config::default(), Dialect::Openai, Messages),
            (&codex, Dialect::Codex, ChatCompletions),
            (&codex, Dialect::Codex, Messages),
        ] {
            assert_eq!(
                settings.admit(codec).unwrap_err(),
                UnsupportedCodec { dialect, codec }.into()
            );
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
        let selected = Placements {
            cache_key: Some(header("x-api-key")),
            ..Placements::default()
        };
        let heads = heads(2, async |root| {
            let mut common = common(root, Some(Sourced::Literal("k".into())));
            let entry = Sourced::Command { command };
            common.headers.insert("x-api-key".into(), entry);
            let unset = Sourced::Env {
                env: "SKYHOOK_TEST_UNSET_X".into(),
            };
            common.headers.insert("accept".into(), unset);
            for placements in [Placements::default(), selected] {
                let settings = compatible::Config::default();
                let provider = placed("test", &settings, &placements, CodecName::Messages, &common);
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
