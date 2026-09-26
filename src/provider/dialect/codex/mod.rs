//! The ChatGPT subscription service: Responses over Skyhook-owned OAuth
//! credentials, a sticky per-turn routing header, and cache affinity by header.
//! Authentication never imports the official client's credentials.
pub mod auth;

use std::sync::Arc;

use futures_util::future::BoxFuture;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};

use super::{
    BaseUrl, BuildError, Common, Connection, Dialect, DialectConfig, DialectError, Pending,
    Profile, UnsupportedCodec,
};
use crate::provider::{
    ProviderError, ProviderErrorKind,
    codec::{
        Codec, CodecName, Identity, header,
        responses::{self, Instructions, TerminalOutput},
    },
    http::{Headers, Session, Transport, auth::Authenticator, headers::Value},
};

const ACCOUNT_HEADER: HeaderName = HeaderName::from_static("chatgpt-account-id");

/// The subscription service's API root, unless the entry names another.
const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

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

/// Codex takes no key: `skyhook auth login` stores its credentials. `base_url`
/// and `auth_url` default to OpenAI's service and are named only for a mirror.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The OAuth issuer `skyhook auth login` and token refresh talk to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_url: Option<auth::Issuer>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("api_key does not apply to codex; run `skyhook auth login`")]
    ApiKey,
}

impl Config {
    /// The issuer the entry names, or OpenAI's.
    pub fn issuer(&self) -> auth::Issuer {
        self.auth_url.clone().unwrap_or_default()
    }
}

impl DialectConfig for Config {
    fn admit(&self, common: &Common, codec: CodecName) -> Result<Profile, DialectError> {
        if codec != CodecName::Responses {
            return Err(UnsupportedCodec {
                dialect: Dialect::Codex,
                codec,
            }
            .into());
        }
        if common.api_key.is_some() {
            return Err(Error::ApiKey.into());
        }
        let mut profile = Profile {
            base_url: BaseUrl::Default(DEFAULT_BASE_URL),
            ..Profile::new(Codec::Responses(responses()), transport(), Dialect::Codex)
        };
        profile.headers.insert(
            HeaderName::from_static("originator"),
            Value::Fixed(HeaderValue::from_static("skyhook")),
        );
        Ok(profile)
    }

    /// No credentials are read and no login is required until invocation.
    fn credentials(&self, _: &Profile, _: &Connection) -> Result<Headers<Pending>, BuildError> {
        Ok(subscription(auth::AuthManager::new(self.issuer())?))
    }
}

/// The stored subscription credentials, under each header they set.
fn subscription(manager: auth::AuthManager) -> Headers<Pending> {
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
            let sensitive = |value: &str, message| {
                let mut value = HeaderValue::from_str(value).map_err(|_| ProviderError {
                    kind: ProviderErrorKind::Authentication,
                    message: String::from(message),
                })?;
                value.set_sensitive(true);
                Ok::<_, ProviderError>(value)
            };
            let token = sensitive(
                &format!("Bearer {}", credentials.access_token),
                "invalid Codex access token",
            )?;
            let account = sensitive(&credentials.account_id, "invalid Codex account identifier")?;
            Ok(HeaderMap::from_iter([
                (AUTHORIZATION, token),
                (ACCOUNT_HEADER, account),
            ]))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{
        Provider,
        codec::common::tests::{reasoning_tool_request, reduce},
        http::transport::tests::{Plan, Server, header_values, reply},
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
        let profile = Config::default()
            .admit(&common, CodecName::Responses)
            .unwrap();
        let connection = common
            .admit(profile.base_url, CodecName::Responses)
            .unwrap();
        let credentials = subscription(auth::test_manager(directory));
        super::super::build(name, profile, &connection, credentials).unwrap()
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
        assert!(head.starts_with("POST /responses HTTP/1.1"));
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
    fn speaks_responses_without_a_key_at_the_service_or_a_mirror() {
        let config = Config::default();
        let chat = CodecName::ChatCompletions;
        assert_eq!(
            config.admit(&Common::default(), chat).unwrap_err(),
            UnsupportedCodec {
                dialect: Dialect::Codex,
                codec: chat
            }
            .into()
        );
        let keyed = Common {
            api_key: Some(crate::provider::dialect::Sourced::Literal("k".into())),
            ..Common::default()
        };
        assert_eq!(
            config.admit(&keyed, CodecName::Responses).unwrap_err(),
            Error::ApiKey.into()
        );
        assert_eq!(config.issuer().as_str(), "https://auth.openai.com/");
        let profile = config
            .admit(&Common::default(), CodecName::Responses)
            .unwrap();
        assert_eq!(
            Common::default()
                .admit(profile.base_url, CodecName::Responses)
                .unwrap()
                .endpoint
                .as_str(),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        let mirror = |url: &str| crate::yaml::parse::<Config>(&format!("auth_url: '{url}'"));
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
            let error = mirror(invalid).unwrap_err().to_string();
            assert!(error.contains(&auth::InvalidIssuer.to_string()), "{error}");
        }
    }
}
