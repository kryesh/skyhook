//! Skyhook-owned ChatGPT OAuth. Never consults CODEX_HOME, ~/.codex/auth.json,
//! Flux's credential store, or any other application's credentials.
//!
//! Public-client protocol checked against codewandler-flux-credentials 0.59.3
//! (src/lib.rs, codex_authorize_url) and upstream on 2026-09-08:
//! https://github.com/openai/codex/blob/main/codex-rs/login/src/server.rs
//! https://github.com/openai/codex/blob/main/codex-rs/login/src/device_code_auth.rs
//! Device auth is OpenAI's public /api/accounts/deviceauth flow, NOT the
//! unrelated RFC 8628 grant. No client secret or privileged/private API is used.

mod oauth;
mod store;

pub use store::{AccountId, InvalidAccount, InvalidToken, Token};

use crate::provider::{ProviderError, ProviderErrorKind::Authentication, dialect::LoginCommand};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use zeroize::Zeroizing;

/// OpenAI's OAuth issuer, unless a codex entry names another.
const DEFAULT_ISSUER: &str = "https://auth.openai.com/";
/// Credentials this close to expiry are refreshed before use.
const REFRESH_SKEW: u64 = 60;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(45);

fn now() -> Result<u64, ProviderError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|_| Authentication.error("System clock is invalid"))
}

/// An OAuth issuer: HTTPS, or HTTP on a loopback host, without credentials,
/// query or fragment. Its path ends in `/`, so OAuth paths join beneath any
/// prefix it has.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Issuer(reqwest::Url);

/// An `auth_url` that is not a valid issuer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "auth_url must be an HTTPS URL (HTTP only on a loopback host), without credentials, query, or fragment"
)]
pub struct InvalidIssuer;

impl Issuer {
    pub fn parse(text: &str) -> Result<Self, InvalidIssuer> {
        let mut url = super::super::config::service_url(text).map_err(|_| InvalidIssuer)?;
        let loopback = url.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host
                    .trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
        if url.scheme() != "https" && !loopback {
            return Err(InvalidIssuer);
        }
        if !url.path().ends_with('/') {
            url.set_path(&format!("{}/", url.path()));
        }
        Ok(Self(url))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// An OAuth path beneath the issuer.
    pub(super) fn join(&self, path: &'static str) -> reqwest::Url {
        self.0
            .join(path)
            .expect("a relative path joins any base URL")
    }
}

impl Default for Issuer {
    fn default() -> Self {
        Self::parse(DEFAULT_ISSUER).expect("the default issuer is valid")
    }
}

impl TryFrom<String> for Issuer {
    type Error = InvalidIssuer;
    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse(&text)
    }
}

impl From<Issuer> for String {
    fn from(issuer: Issuer) -> Self {
        issuer.0.into()
    }
}

impl std::fmt::Display for Issuer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The token is deliberately absent from Debug.
#[derive(Clone)]
pub struct Credentials {
    pub access_token: Token,
    pub account_id: AccountId,
}
impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Credentials([redacted])")
    }
}

#[derive(Clone)]
pub struct AuthManager {
    inner: Arc<Inner>,
}
struct Inner {
    files: store::Files,
    /// Named when the credentials need a login.
    command: LoginCommand,
    client: reqwest::Client,
    issuer: Issuer,
}

impl AuthManager {
    /// Does not read credentials or perform network I/O. Login is deferred until
    /// explicitly requested; credentials() never launches an interactive flow.
    pub(in crate::provider::dialect) fn new(
        command: LoginCommand,
        issuer: Issuer,
    ) -> Result<Self, ProviderError> {
        let files = store::Files::new(command.home.credentials()?, &command.provider)?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|_| Authentication.error("Cannot initialize Codex OAuth HTTP client"))?;
        Ok(Self {
            inner: Arc::new(Inner {
                files,
                command,
                client,
                issuer,
            }),
        })
    }

    /// Reloads under an OS lock on every call, so refresh-token rotation is
    /// coordinated between clones AND separate Skyhook processes. The File lock
    /// remains owned across the network await and is released on cancellation.
    /// Credentials another issuer granted are never used or sent.
    pub async fn credentials(&self) -> Result<Credentials, ProviderError> {
        let (lock, stored) = self.usable(self.read().await?)?;
        if stored.expires_at > now()?.saturating_add(REFRESH_SKEW) {
            return Ok(Credentials {
                access_token: stored.access_token,
                account_id: stored.account_id,
            });
        }
        let refresh = [
            ("grant_type", "refresh_token"),
            ("refresh_token", stored.refresh_token.as_str()),
        ];
        let token = self.token(&refresh).await?;
        let stored = token.into_stored(&self.inner.issuer, Some(&stored))?;
        let credentials = Credentials {
            access_token: stored.access_token.clone(),
            account_id: stored.account_id.clone(),
        };
        self.save(lock, stored).await?;
        Ok(credentials)
    }
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, ProviderError> + Send + 'static,
) -> Result<T, ProviderError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| Authentication.error("Codex credential storage task failed"))?
}

fn random_string() -> Result<String, ProviderError> {
    let mut bytes = Zeroizing::new([0u8; 32]);
    getrandom::fill(&mut *bytes)
        .map_err(|_| Authentication.error("Cannot obtain secure randomness for Codex login"))?;
    Ok(URL_SAFE_NO_PAD.encode(*bytes))
}

/// How the test provider `name` defined in `root` is signed in; its
/// credentials are kept in `root`'s `credentials` directory.
#[cfg(test)]
pub(super) fn login_command(root: &std::path::Path, name: &str) -> LoginCommand {
    LoginCommand {
        provider: name.parse().unwrap(),
        home: super::super::ConfigHome::File(root.join("config.yaml")),
    }
}

/// A manager for the provider `test` defined in `root`, at `issuer`.
#[cfg(test)]
pub(super) fn manager_at(root: &std::path::Path, issuer: &Issuer) -> AuthManager {
    AuthManager::new(login_command(root, "test"), issuer.clone()).unwrap()
}

#[cfg(all(test, unix))]
pub(super) use oauth::tests::mock as oauth_mock;

/// A manager holding unexpired credentials from a loopback issuer, for the
/// codex provider's tests.
#[cfg(test)]
pub(super) fn test_manager(directory: std::path::PathBuf) -> AuthManager {
    let issuer = Issuer::parse("http://127.0.0.1:1").unwrap();
    let manager = manager_at(&directory, &issuer);
    let _lock = store::lock_store(&manager.inner.files).unwrap();
    store::write_store(
        &manager.inner.files,
        &store::Stored {
            issuer,
            access_token: Token::try_from("test-access-token".to_owned()).unwrap(),
            refresh_token: Token::try_from("test-refresh-token".to_owned()).unwrap(),
            account_id: "test-account-id".parse().unwrap(),
            expires_at: now().unwrap() + 3600,
        },
    )
    .unwrap();
    manager
}

#[cfg(all(test, unix))]
mod tests {
    use super::oauth::tests::{mock, token_body};
    use super::store::tests::stored;
    use super::*;
    use crate::provider::dialect::{AuthStatus, LoginReason, LoginRequired};
    use std::sync::atomic::Ordering;

    /// Refresh joins its path beneath the issuer's prefix.
    #[tokio::test]
    async fn independent_managers_coordinate_rotating_refresh() {
        let temp = tempfile::tempdir().unwrap();
        let (url, count, server) = mock(vec![(200, token_body())]).await;
        let issuer = Issuer::parse(&format!("{url}/tenant")).unwrap();
        let one = manager_at(temp.path(), &issuer);
        let two = manager_at(temp.path(), &issuer);
        let (lock, _) = one.load().await.unwrap();
        one.save(lock, stored(&issuer, 1)).await.unwrap();
        let (a, b) = tokio::join!(one.credentials(), two.credentials());
        assert!(a.unwrap().access_token == b.unwrap().access_token);
        assert_eq!(count.load(Ordering::SeqCst), 1);
        let requests = server.await.unwrap();
        assert!(requests[0].starts_with("POST /tenant/oauth/token "));
        assert!(requests[0].contains("grant_type=refresh_token"));
        assert!(requests[0].contains("refresh_token=old-refresh"));
        let (_, saved) = one.load().await.unwrap();
        assert_eq!(saved.unwrap().refresh_token.as_str(), "rotated-refresh");
    }

    /// Credentials another issuer granted, fresh or due for refresh, ask for a
    /// login, in use and in status, and send nothing.
    #[tokio::test]
    async fn credentials_of_another_issuer_require_login_and_send_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let (url, count, server) = mock(vec![(200, token_body())]).await;
        let manager = manager_at(temp.path(), &Issuer::parse(&url).unwrap());
        let other = Issuer::parse("https://auth.example/").unwrap();
        let required = LoginRequired {
            command: login_command(temp.path(), "test"),
            reason: LoginReason::OtherIssuer,
        };
        for expiry in [1, now().unwrap() + 3600] {
            let (lock, _) = manager.load().await.unwrap();
            manager.save(lock, stored(&other, expiry)).await.unwrap();
            let error = manager.credentials().await.unwrap_err();
            assert_eq!(error, required.clone().into());
            assert_eq!(
                manager.status().await.unwrap(),
                AuthStatus::LoginRequired(required.clone())
            );
        }
        assert_eq!(count.load(Ordering::SeqCst), 0);
        server.abort();
    }

    /// A failed refresh keeps the stored credentials and never echoes the
    /// response. Its kind tells the runtime whether a retry can succeed.
    #[tokio::test]
    async fn refresh_failures_keep_credentials_and_carry_their_kind() {
        use crate::provider::ProviderErrorKind::*;
        let temp = tempfile::tempdir().unwrap();
        let secret = serde_json::json!({"error":"old-refresh TOP-SECRET"});
        let replies = [400, 429, 503].map(|status| (status, secret.clone()));
        let (url, _, server) = mock(replies.to_vec()).await;
        let issuer = Issuer::parse(&url).unwrap();
        let manager = manager_at(temp.path(), &issuer);
        let (lock, _) = manager.load().await.unwrap();
        manager.save(lock, stored(&issuer, 1)).await.unwrap();
        let mut kinds = Vec::new();
        for _ in replies {
            let err = manager.credentials().await.unwrap_err();
            assert!(!format!("{err:?}").contains("TOP-SECRET"));
            assert!(!format!("{err:?}").contains("old-refresh"));
            kinds.push(err.kind());
        }
        server.await.unwrap();
        let (_, saved) = manager.load().await.unwrap();
        assert_eq!(saved.unwrap().refresh_token.as_str(), "old-refresh");
        let refused = crate::tests::RefusedPort::new();
        let issuer = Issuer::parse(&format!("http://{}", refused.address())).unwrap();
        let manager = manager_at(temp.path(), &issuer);
        let (lock, _) = manager.load().await.unwrap();
        manager.save(lock, stored(&issuer, 1)).await.unwrap();
        kinds.push(manager.credentials().await.unwrap_err().kind());
        let unavailable = Unavailable;
        let limited = RateLimited;
        assert_eq!(kinds, [Authentication, limited, unavailable, Transport]);
    }
}
