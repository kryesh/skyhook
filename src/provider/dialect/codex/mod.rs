//! The ChatGPT subscription service: Responses over Skyhook-owned OAuth
//! credentials, a sticky per-turn routing header, and cache affinity by header.
//! Authentication never imports the official client's credentials.
pub mod auth;
mod usage;

pub use usage::Account;

use std::{sync::Arc, time::Duration};

use futures_util::future::BoxFuture;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};
use zeroize::Zeroizing;

use super::{
    AdmissionError, BaseUrl, Common, ConfigHome, Connection, Dialect, DialectConfig, DialectError,
    Login, LoginCommand, Pending, Profile, UnsupportedCodec, entry::ProviderOptions,
};
use crate::provider::{
    ProviderError,
    codec::{
        Codec, CodecName, Identity, header,
        responses::{self, Instructions, TerminalOutput},
    },
    http::{Headers, Session, Transport, auth::Authenticator, headers::Value},
    profile::ProviderName,
};

const ACCOUNT_HEADER: HeaderName = HeaderName::from_static("chatgpt-account-id");
/// Names Skyhook as the client on every request to the service.
const ORIGINATOR: &str = "originator";
const SKYHOOK: HeaderValue = HeaderValue::from_static("skyhook");
/// The most of a JSON answer's body that is read.
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// The ChatGPT backend's API root, unless the entry names another. Codex
/// inference and account usage are services beneath it.
const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api";

/// The subscription service's Responses conventions: `instructions` is required, no
/// output limit is accepted, cache affinity travels as a header, completed items
/// are not restated in the terminal event, and quota notices ride the stream.
fn responses() -> responses::Dialect {
    responses::Dialect {
        instructions: Instructions::RequiredEvenWhenEmpty,
        output_limit: None,
        identity: Identity {
            cache_key: header("session-id"),
            user_id: None,
        },
        terminal_output: TerminalOutput::StreamedOnly,
        metadata_events: &[
            "codex.rate_limits",
            "codex.response.metadata",
            "responsesapi.websocket_timing",
        ],
        ..responses::Dialect::stateless()
    }
}

fn transport() -> Transport {
    Transport {
        session: Session::StickyTurn {
            header: HeaderName::from_static("x-codex-turn-state"),
        },
        ..Transport::plain()
    }
}

crate::provider::settings::settings! {
    /// Codex takes no key: `skyhook auth login` stores each entry's credentials. `base_url`
    /// and `auth_url` default to OpenAI's service and are named only for a mirror.
    #[derive(Default)]
    pub struct Options => OptionsPatch {
        /// The OAuth issuer `skyhook auth login` and token refresh talk to.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub auth_url: Option<auth::Issuer> => default,
    }
}

crate::provider::settings::settings! {
    #[derive(Default)]
    pub struct Config => Patch {}
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("api_key does not apply to codex; run `skyhook auth login <provider>`")]
    ApiKey,
}

impl ProviderOptions for Options {
    fn admit(&self, common: &Common) -> Result<Connection, AdmissionError> {
        let connection = common.admit(BaseUrl::Default(DEFAULT_BASE_URL))?;
        if common.api_key.is_some() {
            return Err(DialectError::Codex(Error::ApiKey).into());
        }
        Ok(connection)
    }

    fn login(
        &self,
        name: &ProviderName,
        home: &ConfigHome,
        connection: &Connection,
    ) -> Result<Option<Login>, ProviderError> {
        let command = LoginCommand {
            provider: name.clone(),
            home: home.clone(),
        };
        let manager = auth::AuthManager::new(command, self.issuer())?;
        Ok(Some(Login::Codex(Account::new(manager, connection))))
    }
}

impl Options {
    /// The issuer the entry names, or OpenAI's.
    fn issuer(&self) -> auth::Issuer {
        self.auth_url.clone().unwrap_or_default()
    }
}

impl DialectConfig for Config {
    fn admit(&self, codec: CodecName) -> Result<Profile, DialectError> {
        if codec != CodecName::Responses {
            return Err(UnsupportedCodec {
                dialect: Dialect::Codex,
                codec,
            }
            .into());
        }
        let mut profile = Profile::new(Codec::Responses(responses()), transport(), Dialect::Codex);
        profile.service = Some("codex");
        profile.fixed(ORIGINATOR, SKYHOOK);
        Ok(profile)
    }
}

/// Why a JSON answer's body could not be read.
enum BodyError {
    Unreadable,
    /// No read completed within the idle limit.
    Stalled,
    TooLarge,
}

/// A JSON answer's body, bounded, each read within `read_idle`, and wiped on drop.
async fn body(
    mut response: reqwest::Response,
    read_idle: Duration,
) -> Result<Zeroizing<Vec<u8>>, BodyError> {
    let mut bytes = Zeroizing::new(Vec::new());
    loop {
        let chunk = tokio::time::timeout(read_idle, response.chunk())
            .await
            .map_err(|_| BodyError::Stalled)?
            .map_err(|_| BodyError::Unreadable)?;
        let Some(chunk) = chunk else {
            return Ok(bytes);
        };
        if bytes.len().saturating_add(chunk.len()) > MAX_BODY_BYTES {
            return Err(BodyError::TooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
}

/// The stored subscription credentials, under each header they set.
pub(super) fn subscription(manager: auth::AuthManager) -> Headers<Pending> {
    let source: Arc<dyn Authenticator> = Arc::new(Subscription(manager));
    let mut headers = Headers::default();
    for name in [AUTHORIZATION, ACCOUNT_HEADER] {
        headers.insert(name, Pending::Ready(Value::Issued(Arc::clone(&source))));
    }
    headers
}

struct Subscription(auth::AuthManager);

impl Authenticator for Subscription {
    fn headers(&self) -> BoxFuture<'_, Result<HeaderMap, ProviderError>> {
        Box::pin(async move {
            let credentials = self.0.credentials().await?;
            let sensitive = |text: &str| {
                let mut value = HeaderValue::from_str(text)
                    .expect("tokens and account IDs hold no control characters");
                value.set_sensitive(true);
                value
            };
            let token = format!("Bearer {}", credentials.access_token.as_str());
            Ok(HeaderMap::from_iter([
                (AUTHORIZATION, sensitive(&token)),
                (ACCOUNT_HEADER, sensitive(credentials.account_id.as_str())),
            ]))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{
        Provider,
        codec::common::tests::reduce,
        dialect::Sourced,
        http::{
            tests::reasoning_tool_request,
            transport::tests::{Plan, Server, header_values, reply},
        },
        protocol::Outcome,
    };
    use futures_util::StreamExt;
    use serde_json::{Value, json};

    /// A provider on stored test credentials, at a server's root.
    fn provider(
        name: &str,
        server: &Server,
        directory: std::path::PathBuf,
    ) -> crate::provider::http::HttpProvider {
        let common = Common {
            base_url: Some(server.url.trim_end_matches("/responses").to_owned()),
            ..Common::default()
        };
        let profile = Config::default().admit(CodecName::Responses).unwrap();
        let connection = common.admit(BaseUrl::Required).unwrap();
        let credentials = subscription(auth::test_manager(directory));
        super::super::build(
            name,
            profile,
            &connection,
            credentials,
            &mut super::super::Resources::new().unwrap(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn posts_its_dialect_with_subscription_headers_and_ignores_metadata() {
        let message = json!({"type":"message", "id":"msg_1", "status":"completed", "role":"assistant",
            "content":[{"type":"output_text","text":"OK","annotations":[]}]});
        let body: String = [
            json!({"type":"codex.rate_limits","rate_limits":{}}),
            json!({"type":"codex.response.metadata","metadata":{}}),
            json!({"type":"responsesapi.websocket_timing","timing":{}}),
            json!({"type":"response.output_item.added","output_index":0,"item":message}),
            json!({"type":"response.output_item.done","output_index":0,"item":message}),
            // Codex omits terminal output; streamed items remain authoritative.
            json!({"type":"response.completed","response":{"id":"r1","status":"completed","output":[]}}),
        ]
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect();
        let sse = reply("200 OK", "Content-Type: text/event-stream\r\n", &body);
        let server = Server::start(vec![Plan::reply(sse)]).await;
        let directory = tempfile::tempdir().unwrap();
        let provider = provider("codex", &server, directory.path().to_owned());
        let mut context = provider.open_context("context".parse().unwrap()).unwrap();
        let request = reasoning_tool_request(provider.scope());
        let events: Vec<_> = context.invoke(request).collect().await;
        assert!(events.iter().all(Result::is_ok), "{events:?}");
        let reduced = reduce(events.into_iter().map(Result::unwrap));
        assert_eq!(reduced.completion.outcome(), Outcome::Answer);
        assert_eq!(reduced.items()[0].text_content().as_deref(), Some("OK"));
        let requests = server.finish().await;
        let (head, body) = requests[0].split_once("\r\n\r\n").unwrap();
        assert!(head.starts_with("POST /codex/responses HTTP/1.1"));
        assert_eq!(
            header_values(head, "authorization"),
            ["Bearer test-access-token"]
        );
        assert_eq!(
            header_values(head, "chatgpt-account-id"),
            ["test-account-id"]
        );
        assert_eq!(header_values(head, "session-id"), ["context"]);
        assert_eq!(header_values(head, "originator"), ["skyhook"]);
        let body: Value = serde_json::from_str(body).unwrap();
        assert!(body.get("max_output_tokens").is_none());
        assert_eq!(body["instructions"], "");
    }

    #[test]
    fn takes_no_key_at_the_service_or_a_mirror() {
        // Refused whether or not the entry has models yet.
        let keyed = "providers:\n  codex:\n    dialect: codex\n    api_key: k\n";
        let refused = crate::config::Config::from_yaml(keyed)
            .unwrap_err()
            .to_string();
        assert!(refused.contains(&Error::ApiKey.to_string()), "{refused}");
        let options = Options::default();
        assert_eq!(options.issuer().as_str(), "https://auth.openai.com/");
        Config::default().admit(CodecName::Responses).unwrap();
        assert_eq!(
            options.admit(&Common::default()).unwrap().root.as_str(),
            "https://chatgpt.com/backend-api"
        );
        for base_url in [None, Some("https://api.example/codex".to_owned())] {
            let common = Common {
                base_url,
                api_key: Some(Sourced::Literal("k".to_owned())),
                ..Common::default()
            };
            assert!(matches!(
                options.admit(&common),
                Err(AdmissionError::Dialect(DialectError::Codex(Error::ApiKey)))
            ));
        }
        let mirror = |url: &str| crate::yaml::parse::<Options>(&format!("auth_url: '{url}'"));
        // OAuth paths join beneath a mirror's path prefix.
        let tenant = mirror("https://auth.example/tenant").unwrap().issuer();
        assert_eq!(
            tenant.join("oauth/token").as_str(),
            "https://auth.example/tenant/oauth/token"
        );
        for loopback in [
            "http://127.0.0.1:1455",
            "http://localhost/",
            "http://[::1]/",
        ] {
            assert!(mirror(loopback).is_ok(), "{loopback}");
        }
        for invalid in [
            "auth.example",
            "ftp://auth.example",
            "http://auth.example",
            "https://user:secret@auth.example",
            "https://auth.example/?secret",
            "https://auth.example/#secret",
        ] {
            assert_eq!(
                auth::Issuer::parse(invalid),
                Err(auth::InvalidIssuer),
                "{invalid}"
            );
        }
    }
}
