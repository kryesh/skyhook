//! Dialects. Each module owns its settings, the codecs it speaks and the
//! presets for each, its endpoint, credentials, and transport conventions.
//! `compatible` is the flexible one: standards-following servers need only an
//! endpoint, and its entry may still select each placement dimension.

use std::convert::Infallible;

use reqwest::header::{ACCEPT, HeaderName, HeaderValue};
use serde::Serialize;

use crate::provider::{
    codec::{Codec, StateRole},
    http::{
        Headers, HttpProvider, Transport, headers::Value, reasoning_scope, transport::EVENT_STREAM,
    },
};
use skyhook_base::enums::NamedEnum;
use skyhook_media::ImageLimits;

pub mod anthropic;
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

pub use crate::provider::codec::CodecName;
use crate::provider::http::credentials::{Pending, Scheme, Sources};
pub use crate::provider::http::{
    credentials::{BuildError, HeaderError, ValueError, ValueField, ValueProblem},
    headers::{HeaderText, InvalidHeaderText},
};
pub use crate::provider::settings::{MissingSetting, Setting};
pub(crate) use config::Connection;
pub use config::{AdmissionError, Common, EndpointError, ModelError, Unset};
pub(crate) use entry::AdmittedProvider;
pub use entry::{ConfigHome, ProviderModels, ProviderSettings, RawProviderConfig};
pub use login::{
    AccountUsage, AuthStatus, Login, LoginCommand, LoginError, LoginFlow, LoginPresenter,
    LoginReason, LoginRequired, UsageWindow,
};
pub use models::{
    AccessPrograms, AccessProgramsPatch, ImageLimitSettings, ImageLimitSettingsPatch, ModelSpec,
    Request, RequestPatch, RequestSettings, ResponseSetting, ServiceTier,
};
pub use placements::{PlacementError, PlacementKey, Placements, PlacementsPatch};

/// The preset a provider entry names.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq, NamedEnum)]
#[serde(rename_all = "snake_case")]
pub enum Dialect {
    Compatible,
    Openai,
    Anthropic,
    Codex,
    Litellm,
    Openrouter,
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
pub(crate) struct Wire {
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
    /// What the service accepts of each image; larger ones are sent fitted.
    pub images: ImageLimits,
}

impl Wire {
    pub(crate) fn new(
        codec: Codec,
        transport: Transport,
        dialect: Dialect,
        images: ImageLimits,
    ) -> Self {
        Self {
            headers: codec.headers(),
            codec,
            transport,
            scope: dialect.as_str().to_owned(),
            key: Scheme::Bearer,
            service: None,
            images,
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

    /// Send runtime state in the configured role, or the dialect's `default`. A role
    /// other than the default joins the replay scope: replay produced while state took
    /// another role followed a different conversation.
    pub(crate) fn state_role(
        &mut self,
        configured: Option<StateRole>,
        default: StateRole,
    ) -> StateRole {
        let role = configured.unwrap_or(default);
        *self.codec.state_role_mut() = role;
        if role != default {
            self.discriminate("state_role", &role);
        }
        role
    }

    /// Partition sealed replay for nonsecret routing and tenant selectors. Each part
    /// names its setting, so different settings with equal values never collide.
    pub(crate) fn discriminate(&mut self, setting: &'static str, identity: &impl Serialize) {
        let identity = serde_json::to_string(identity).expect("replay identity serializes");
        self.scope.push_str(&format!(":{setting}={identity}"));
    }

    /// Send `value` as `name` on every request.
    pub(crate) fn fixed(&mut self, name: &'static str, value: HeaderValue) {
        let name = HeaderName::from_static(name);
        self.headers.insert(name, Value::Fixed(value));
    }

    /// Add `values` to the list header `name` on every request.
    pub(crate) fn listed(
        &mut self,
        name: HeaderName,
        values: impl IntoIterator<Item = &'static str>,
    ) {
        let values = values
            .into_iter()
            .map(|value| Value::Fixed(HeaderValue::from_static(value)));
        self.headers.list(name, values);
    }
}

/// Why a dialect refused its settings or the codec.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DialectError {
    #[error("dialect {dialect} does not speak {codec}")]
    UnsupportedCodec { dialect: Dialect, codec: CodecName },
    #[error("api_key does not apply to {dialect}; run `skyhook auth login <provider>`")]
    ManagedLogin { dialect: Dialect },
}

/// What every dialect's settings provide.
pub(crate) trait DialectConfig {
    /// The API root an entry without `base_url` uses.
    fn base_url() -> BaseUrl
    where
        Self: Sized,
    {
        BaseUrl::Required
    }

    /// Resolve wire conventions after inheritance. Connection and model-profile
    /// validation are shared.
    fn admit(&self, codec: CodecName) -> Result<Wire, DialectError>;
}

/// Build the provider at the admitted endpoint. Header sources compose in
/// order, each replacing a header or joining a list header: the wire conventions', the
/// entry's `headers`, `credentials`, then the stream the transport reads. Only
/// the environment values that remain are read.
pub(crate) fn build(
    name: &str,
    wire: Wire,
    connection: &Connection,
    credentials: Headers<Pending>,
    resources: &mut Resources,
) -> Result<HttpProvider, BuildError> {
    let endpoint = connection.url(&wire.path());
    let ready = |value| Ok::<_, Infallible>(Pending::Ready(value));
    let Ok(mut headers) = wire.headers.try_map(ready);
    headers.extend(connection.headers.pending());
    headers.extend(credentials);
    let stream = Value::Fixed(HeaderValue::from_static(EVENT_STREAM));
    headers.insert(ACCEPT, Pending::Ready(stream));
    let headers = headers.try_map(|pending| resources.sources.read(pending))?;
    Ok(HttpProvider {
        scope: reasoning_scope(name, endpoint.as_str(), wire.codec.name(), &wire.scope),
        client: resources.client.clone(),
        endpoint,
        codec: wire.codec,
        transport: wire.transport,
        headers,
        timeouts: connection.timeouts,
        images: wire.images,
    })
}

#[derive(Default)]
pub(crate) struct Resources {
    client: crate::provider::http::transport::LazyClient,
    sources: Sources,
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    pub(crate) use crate::provider::http::tests::Sent;
    use skyhook_worker::process::CommandLine;
    use skyhook_worker::secret::Credential;

    /// An entry at `root`, keyed or anonymous.
    pub(crate) fn common(root: &str, api_key: Option<Credential>) -> Common {
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
        let mut wire = settings.admit(codec).unwrap();
        wire.codec = placements.apply(wire.codec).unwrap();
        let connection = common.admit(BaseUrl::Required).unwrap();
        let credentials = connection.credentials(wire.key);
        let mut resources = Resources::default();
        build(name, wire, &connection, credentials, &mut resources).unwrap()
    }

    /// An API root nothing listens on: requests are prepared, never posted.
    pub(crate) const ROOT: &str = "http://127.0.0.1:9/v1";

    /// One request a keyed entry sends.
    pub(crate) async fn head(
        settings: &impl DialectConfig,
        codec: CodecName,
        api_key: &str,
    ) -> Sent {
        sent(settings, codec, api_key, request()).await
    }

    /// What a keyed entry sends for `request`.
    pub(crate) async fn sent(
        settings: &impl DialectConfig,
        codec: CodecName,
        api_key: &str,
        request: crate::provider::protocol::ModelRequest,
    ) -> Sent {
        let common = common(ROOT, Some(Credential::Literal(api_key.into())));
        let provider = provider("test", settings, codec, &common);
        provider.sent(request).await.unwrap()
    }

    /// The replay scope `yaml` settings admit under `codec`.
    pub(crate) fn replay_scope<C>(yaml: &str, codec: CodecName) -> String
    where
        C: DialectConfig + skyhook_base::decode::Decode,
    {
        let settings = crate::yaml::parse::<C>(yaml).unwrap();
        settings.admit(codec).unwrap().scope
    }

    /// The kind `settings` read a rejection of `codec` as.
    pub(crate) fn rejected(
        settings: &impl DialectConfig,
        codec: CodecName,
        status: u16,
        body: serde_json::Value,
        retry_after: Option<std::time::Duration>,
    ) -> crate::provider::ProviderErrorKind {
        let wire = settings.admit(codec).unwrap();
        let rejection = crate::provider::http::transport::Rejection {
            status,
            body,
            retry_after,
        };
        let signals = wire.transport.errors;
        wire.codec.error(&rejection, signals).kind()
    }

    /// The kind `settings` read a mid-stream error event of `codec` as.
    pub(crate) fn streamed(
        settings: &impl DialectConfig,
        codec: CodecName,
        event: serde_json::Value,
    ) -> crate::provider::ProviderErrorKind {
        let wire = settings.admit(codec).unwrap();
        let scope = crate::provider::codec::common::tests::scope();
        let signals = wire.transport.errors;
        let mut decoder = wire.codec.decoder("model".into(), scope, b"", signals);
        let event = crate::provider::http::transport::tests::sse(None, event.to_string());
        decoder.decode(&event).unwrap_err().kind()
    }

    pub(crate) fn request() -> crate::provider::protocol::ModelRequest {
        crate::provider::protocol::ModelRequest {
            max_output_tokens: std::num::NonZeroU64::new(16).unwrap(),
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
                DialectError::UnsupportedCodec { dialect, codec }
            );
        }
    }

    #[tokio::test]
    async fn fixed_headers_merge_in_order() {
        let mut common = common(ROOT, None);
        common.headers = [
            ("x-literal".to_owned(), Credential::Literal("plain".into())),
            (
                "x-lazy".to_owned(),
                Credential::Command(CommandLine::Shell("printf lazy".into())),
            ),
            // The entry's headers win over the transport's constants.
            (
                "anthropic-version".to_owned(),
                Credential::Literal("2099-01-01".into()),
            ),
        ]
        .into_iter()
        .collect();
        let settings = anthropic::Config::default();
        let provider = provider("test", &settings, CodecName::Messages, &common);
        for _ in 0..2 {
            let sent = provider.sent(request()).await.unwrap();
            assert_eq!(sent.header("x-literal"), ["plain"]);
            assert_eq!(sent.header("x-lazy"), ["lazy"]);
            assert_eq!(sent.header("anthropic-version"), ["2099-01-01"]);
            assert!(sent.header("x-api-key").is_empty());
        }
    }

    /// The key replaces an entry header of its name, a codec placement replaces
    /// the key, and the transport's `Accept` replaces the entry's; a replaced
    /// environment value is never read.
    #[tokio::test]
    async fn placements_replace_credentials_which_replace_entry_headers() {
        use crate::provider::codec::{NonEmpty, header};
        let selected = Placements {
            cache_key: crate::provider::settings::Nullable::Value(NonEmpty::one(header(
                "x-api-key",
            ))),
            ..Placements::default()
        };
        let mut common = common(ROOT, Some(Credential::Literal("k".into())));
        let entry = Credential::Literal("entry".into());
        common.headers.insert("x-api-key".into(), entry);
        let unset = Credential::Env("SKYHOOK_TEST_UNSET_X".into());
        common.headers.insert("accept".into(), unset);
        let mut keys = Vec::new();
        for placements in [Placements::default(), selected] {
            let settings = compatible::Config::default();
            let provider = placed("test", &settings, &placements, CodecName::Messages, &common);
            let sent = provider.sent(request()).await.unwrap();
            // The transport's own `Accept` displaces the entry's.
            assert_eq!(sent.header("accept"), ["text/event-stream"]);
            keys.push(sent.header("x-api-key").join(""));
        }
        assert_eq!(keys, ["k", "ctx"]);
    }
}
