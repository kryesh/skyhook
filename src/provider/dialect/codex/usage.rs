//! The account's usage as the ChatGPT backend reports it. The request runs no
//! model, so it proves the credentials work without spending quota. The
//! endpoint is undocumented: fields it omits are left out rather than refused.
use std::time::Duration;

use reqwest::header::HeaderName;
use serde::Deserialize;

use super::{BodyError, ORIGINATOR, SKYHOOK, auth::AuthManager, body, subscription};
use crate::provider::{
    ProviderError,
    ProviderErrorKind::{Protocol, Timeout, Transport},
    dialect::{
        AuthStatus, BuildError, Connection, LoginReason, Usage, UsageWindow,
        config::{Pending, Sources},
    },
    http::{
        Headers, Timeouts,
        errors::{ErrorSignals, Reading},
        headers::Value,
        transport::{client, retry_after},
    },
};

/// A codex entry's login: its credentials, and the usage endpoint and entry
/// headers a check presents them with, as a request would.
#[derive(Clone)]
pub struct Account {
    manager: AuthManager,
    usage: reqwest::Url,
    headers: Headers<Pending>,
    timeouts: Timeouts,
}

impl Account {
    pub(super) fn new(manager: AuthManager, connection: &Connection) -> Self {
        let mut headers = Headers::default();
        let originator = HeaderName::from_static(ORIGINATOR);
        headers.insert(originator, Pending::Ready(Value::Fixed(SKYHOOK)));
        headers.extend(connection.configured_headers());
        Self {
            manager,
            usage: connection.url("wham/usage"),
            headers,
            timeouts: connection.timeouts,
        }
    }

    pub(in crate::provider::dialect) async fn login(
        &self,
        headless: bool,
    ) -> Result<(), ProviderError> {
        self.manager.login(headless).await
    }

    pub(in crate::provider::dialect) async fn status(&self) -> Result<AuthStatus, ProviderError> {
        self.manager.status().await
    }

    pub(in crate::provider::dialect) fn into_headers(self) -> Headers<Pending> {
        subscription(self.manager)
    }

    /// Present the credentials, refreshed if due, to the account's usage.
    pub(in crate::provider::dialect) async fn check(&self) -> Result<Usage, BuildError> {
        let mut headers = self.headers.clone();
        headers.extend(subscription(self.manager.clone()));
        let mut sources = Sources::default();
        let headers = headers.try_map(|pending| sources.read(pending))?;
        let sent = headers.resolve().await?;
        // As a request does: startup bounds the response headers, read-idle
        // each read of the body.
        let request = client()?.get(self.usage.clone()).headers(sent.map());
        let response = tokio::time::timeout(self.timeouts.startup, request.send())
            .await
            .map_err(|_| {
                Timeout.error("Codex usage service did not answer within the startup timeout")
            })?
            .map_err(|_| Transport.error("Cannot reach the Codex usage service"))?;
        let status = response.status().as_u16();
        if matches!(status, 401 | 403) {
            return Err(ProviderError::from(self.manager.required(LoginReason::Rejected)).into());
        }
        if !response.status().is_success() {
            // Bodies and URLs are never echoed: either may carry credentials.
            let retry_after = retry_after(response.headers(), std::time::SystemTime::now());
            let kind = Reading::default().kind(
                Some(status),
                &serde_json::Value::Null,
                ErrorSignals::NONE,
                retry_after,
            );
            let message = format!("Codex usage service returned HTTP {status}");
            return Err(kind.error(message).with_retry_after(retry_after).into());
        }
        let bytes = body(response, self.timeouts.read_idle)
            .await
            .map_err(|error| match error {
                BodyError::Unreadable => Transport.error("Cannot read the Codex usage response"),
                BodyError::Stalled => {
                    Timeout.error("Codex usage response stalled past the read-idle timeout")
                }
                BodyError::TooLarge => Transport.error("Codex usage response is too large"),
            })?;
        let payload: Payload = serde_json::from_slice(&bytes)
            .map_err(|_| Protocol.error("Codex usage service returned a malformed response"))?;
        Ok(payload.into())
    }
}

#[derive(Deserialize)]
struct Payload {
    plan_type: Option<String>,
    rate_limit: Option<Limits>,
}

#[derive(Deserialize)]
struct Limits {
    primary_window: Option<Window>,
    secondary_window: Option<Window>,
}

#[derive(Deserialize)]
struct Window {
    used_percent: f64,
    limit_window_seconds: Option<u64>,
}

impl From<Payload> for Usage {
    fn from(payload: Payload) -> Self {
        let windows = (payload.rate_limit.into_iter())
            .flat_map(|limits| [limits.primary_window, limits.secondary_window])
            .flatten()
            .map(|window| UsageWindow {
                used_percent: window.used_percent,
                length: window.limit_window_seconds.map(Duration::from_secs),
            })
            .collect();
        Self {
            plan: payload.plan_type,
            windows,
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::super::auth::{login_command, oauth_mock, test_manager};
    use super::*;
    use crate::provider::dialect::{BaseUrl, Common, LoginRequired, Sourced};

    /// The check presents the saved credentials with the entry's headers in a
    /// GET that runs no model, reads what the service reports, and names a
    /// rejection as a login.
    #[tokio::test]
    async fn check_presents_credentials_and_entry_headers_and_reads_usage_or_rejection() {
        let temp = tempfile::tempdir().unwrap();
        let usage = serde_json::json!({"plan_type": "plus", "rate_limit": {
            "primary_window": {"used_percent": 41, "limit_window_seconds": 18000},
            "secondary_window": {"used_percent": 12.5}}, "credits": {}});
        let (url, _, server) = oauth_mock(vec![(200, usage), (401, serde_json::json!({}))]).await;
        let common = Common {
            base_url: Some(url),
            headers: [("x-proxy-key".into(), Sourced::Literal("proxy".into()))].into(),
            ..Common::default()
        };
        let connection = common.admit(BaseUrl::Required).unwrap();
        let account = Account::new(test_manager(temp.path().to_owned()), &connection);
        let report = account.check().await.unwrap();
        assert_eq!(report.plan.as_deref(), Some("plus"));
        let windows: Vec<_> = (report.windows.iter())
            .map(|window| (window.used_percent, window.length))
            .collect();
        assert_eq!(
            windows,
            [(41.0, Some(Duration::from_secs(18000))), (12.5, None)]
        );
        let required = LoginRequired {
            command: login_command(temp.path(), "test"),
            reason: LoginReason::Rejected,
        };
        let Err(BuildError::Provider(rejected)) = account.check().await else {
            panic!("a 401 is a rejection");
        };
        assert_eq!(rejected, required.into());
        let requests = server.await.unwrap();
        let head = requests[0].to_ascii_lowercase();
        assert!(head.starts_with("get /wham/usage "), "{head}");
        for header in [
            "authorization: bearer test-access-token",
            "chatgpt-account-id: test-account-id",
            "originator: skyhook",
            "x-proxy-key: proxy",
        ] {
            assert!(head.contains(header), "{header}: {head}");
        }
    }

    /// Startup bounds only the wait for the answer's headers, and read-idle
    /// each body read, whichever of the two budgets is longer.
    #[tokio::test]
    async fn check_bounds_the_answer_by_startup_and_each_body_read_by_read_idle() {
        use tokio::io::AsyncWriteExt;
        let stall = Duration::from_secs(2);
        for (startup, read_idle, completes) in [(1, 10, true), (10, 1, false)] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                crate::provider::http::transport::tests::read_request(&mut stream).await;
                let body = r#"{"plan_type":"plus"}"#;
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                    body.len()
                );
                stream.write_all(head.as_bytes()).await.unwrap();
                // Virtual time only for the stall: the exchange before it is
                // real socket I/O, which a paused clock would skip past.
                tokio::time::pause();
                tokio::time::sleep(stall).await;
                let _ = stream.write_all(body.as_bytes()).await;
            });
            let common = Common {
                base_url: Some(url),
                startup_timeout_secs: Some(startup),
                read_idle_timeout_secs: Some(read_idle),
                ..Common::default()
            };
            let connection = common.admit(BaseUrl::Required).unwrap();
            let temp = tempfile::tempdir().unwrap();
            let account = Account::new(test_manager(temp.path().to_owned()), &connection);
            match account.check().await {
                Ok(usage) => assert!(completes, "{startup}s/{read_idle}s: {usage:?}"),
                Err(BuildError::Provider(error)) => {
                    assert!(!completes, "{startup}s/{read_idle}s: {error:?}");
                    assert_eq!(error.kind(), crate::provider::ProviderErrorKind::Timeout);
                }
                Err(error) => panic!("{error}"),
            }
            server.await.unwrap();
            tokio::time::resume();
        }
    }
}
