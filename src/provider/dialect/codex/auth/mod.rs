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

use store::Saved;

use crate::provider::{ProviderError, ProviderErrorKind};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use oauth::{CLIENT_ID, TokenResponse, form_body, response_json};
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use zeroize::{Zeroize, Zeroizing};

pub type AuthError = ProviderError;
/// OpenAI's OAuth issuer, unless a codex entry names another.
const DEFAULT_ISSUER: &str = "https://auth.openai.com/";
const MAX_BODY: usize = 1024 * 1024;
const REFRESH_SKEW: u64 = 60;

fn error(message: &'static str) -> AuthError {
    ProviderError {
        kind: ProviderErrorKind::Authentication,
        message: message.into(),
    }
}
fn now() -> Result<u64, AuthError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|_| error("System clock is invalid"))
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

/// Why stored credentials cannot be used until the user logs in again.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LoginRequired {
    #[error("Codex is not logged in; run `skyhook auth login codex`")]
    LoggedOut,
    #[error("Skyhook's Codex credentials predate issuer binding; run `skyhook auth login codex`")]
    Unbound,
    #[error(
        "Skyhook's Codex credentials were issued by another auth_url; run `skyhook auth login codex`"
    )]
    OtherIssuer,
}

impl From<LoginRequired> for ProviderError {
    fn from(required: LoginRequired) -> Self {
        Self {
            kind: ProviderErrorKind::Authentication,
            message: required.to_string(),
        }
    }
}

/// Tokens are deliberately absent from Debug and are wiped on drop.
#[derive(Clone)]
pub struct Credentials {
    pub access_token: String,
    pub account_id: String,
}
impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Credentials([redacted])")
    }
}
impl Drop for Credentials {
    fn drop(&mut self) {
        self.access_token.zeroize();
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthStatus {
    LoginRequired(LoginRequired),
    /// Local state only; status does not refresh or validate against the server.
    LoggedIn {
        account_id: String,
        expires_at: u64,
    },
}

#[derive(Clone)]
pub struct AuthManager {
    inner: Arc<Inner>,
}
struct Inner {
    directory: PathBuf,
    client: reqwest::Client,
    issuer: Issuer,
}

impl AuthManager {
    /// Does not read credentials or perform network I/O. Login is deferred until
    /// explicitly requested; credentials() never launches an interactive flow.
    pub fn new(issuer: Issuer) -> Result<Self, AuthError> {
        let directory = crate::config::user_config_directory()
            .ok_or_else(|| error("Cannot locate the Skyhook configuration directory"))?;
        Self::at(directory, issuer)
    }

    fn at(directory: PathBuf, issuer: Issuer) -> Result<Self, AuthError> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(45))
            .build()
            .map_err(|_| error("Cannot initialize Codex OAuth HTTP client"))?;
        Ok(Self {
            inner: Arc::new(Inner {
                directory,
                client,
                issuer,
            }),
        })
    }

    /// Reloads under an OS lock on every call, so refresh-token rotation is
    /// coordinated between clones AND separate Skyhook processes. The File lock
    /// remains owned across the network await and is released on cancellation.
    /// Credentials another issuer granted are never used or sent.
    pub async fn credentials(&self) -> Result<Credentials, AuthError> {
        let (lock, saved) = self.load().await?;
        let mut stored = match saved {
            Saved::Absent => return Err(LoginRequired::LoggedOut.into()),
            Saved::Unbound => return Err(LoginRequired::Unbound.into()),
            Saved::Current(stored) if stored.issuer != self.inner.issuer => {
                return Err(LoginRequired::OtherIssuer.into());
            }
            Saved::Current(stored) => stored,
        };
        if stored.expires_at <= now()?.saturating_add(REFRESH_SKEW) {
            let response = self
                .inner
                .client
                .post(self.inner.issuer.join("oauth/token"))
                .header(
                    reqwest::header::CONTENT_TYPE,
                    "application/x-www-form-urlencoded",
                )
                .body(form_body(&[
                    ("grant_type", "refresh_token"),
                    ("client_id", CLIENT_ID),
                    ("refresh_token", stored.refresh_token.as_str()),
                ]))
                .send()
                .await
                .map_err(|_| {
                    error("Codex token refresh could not reach the authentication server")
                })?;
            let token: TokenResponse = response_json(response).await?;
            stored = token.into_stored(&self.inner.issuer, Some(&stored))?;
            let credentials = Credentials {
                access_token: stored.access_token.clone(),
                account_id: stored.account_id.clone(),
            };
            self.save(lock, stored).await?;
            Ok(credentials)
        } else {
            Ok(Credentials {
                access_token: stored.access_token.clone(),
                account_id: stored.account_id.clone(),
            })
        }
    }
}

pub async fn login(issuer: Issuer, headless: bool) -> Result<(), AuthError> {
    AuthManager::new(issuer)?.login(headless).await
}
/// Local state only: whether stored credentials exist and serve `issuer`.
pub async fn status(issuer: Issuer) -> Result<AuthStatus, AuthError> {
    AuthManager::new(issuer)?.status().await
}
/// Removes Skyhook's local tokens only; does not revoke other sessions.
pub async fn logout() -> Result<(), AuthError> {
    AuthManager::new(Issuer::default())?.logout().await
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, AuthError> + Send + 'static,
) -> Result<T, AuthError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| error("Codex credential storage task failed"))?
}

fn random_string() -> Result<String, AuthError> {
    let mut bytes = Zeroizing::new([0u8; 32]);
    getrandom::fill(&mut *bytes)
        .map_err(|_| error("Cannot obtain secure randomness for Codex login"))?;
    Ok(URL_SAFE_NO_PAD.encode(*bytes))
}

/// A manager holding unexpired credentials from a loopback issuer, for the
/// codex provider's tests.
#[cfg(test)]
pub(super) fn test_manager(directory: PathBuf) -> AuthManager {
    let issuer = Issuer::parse("http://127.0.0.1:1").unwrap();
    let manager = AuthManager::at(directory, issuer.clone()).unwrap();
    let _lock = store::lock_store(&manager.inner.directory).unwrap();
    store::write_store(
        &manager.inner.directory,
        &store::Stored {
            issuer,
            access_token: "test-access-token".into(),
            refresh_token: "test-refresh-token".into(),
            account_id: "test-account-id".into(),
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
    use std::sync::atomic::Ordering;

    /// Refresh joins its path beneath the issuer's prefix.
    #[tokio::test]
    async fn independent_managers_coordinate_rotating_refresh() {
        let temp = tempfile::tempdir().unwrap();
        let (url, count, server) = mock(vec![(200, token_body())]).await;
        let issuer = Issuer::parse(&format!("{url}/tenant")).unwrap();
        let one = AuthManager::at(temp.path().to_path_buf(), issuer.clone()).unwrap();
        let two = AuthManager::at(temp.path().to_path_buf(), issuer.clone()).unwrap();
        let (lock, _) = one.load().await.unwrap();
        one.save(lock, stored(&issuer, 1)).await.unwrap();
        let (a, b) = tokio::join!(one.credentials(), two.credentials());
        assert_eq!(a.unwrap().access_token, b.unwrap().access_token);
        assert_eq!(count.load(Ordering::SeqCst), 1);
        let requests = server.await.unwrap();
        assert!(requests[0].starts_with("POST /tenant/oauth/token "));
        assert!(requests[0].contains("grant_type=refresh_token"));
        assert!(requests[0].contains("refresh_token=old-refresh"));
        let (_, saved) = one.load().await.unwrap();
        assert_eq!(saved.current().unwrap().refresh_token, "rotated-refresh");
    }

    /// Credentials another issuer granted, fresh or due for refresh, and a
    /// record from before issuer binding ask for a login, in use and in
    /// status, and send nothing.
    #[tokio::test]
    async fn credentials_of_another_or_no_issuer_require_login_and_send_nothing() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let (url, count, server) = mock(vec![(200, token_body())]).await;
        let manager =
            AuthManager::at(temp.path().to_path_buf(), Issuer::parse(&url).unwrap()).unwrap();
        let other = Issuer::parse("https://auth.example/").unwrap();
        for expiry in [1, now().unwrap() + 3600] {
            let (lock, _) = manager.load().await.unwrap();
            manager.save(lock, stored(&other, expiry)).await.unwrap();
            let error = manager.credentials().await.unwrap_err();
            assert_eq!(error, LoginRequired::OtherIssuer.into());
            assert_eq!(
                manager.status().await.unwrap(),
                AuthStatus::LoginRequired(LoginRequired::OtherIssuer)
            );
        }
        let path = temp.path().join("codex-oauth.json");
        let unbound = serde_json::json!({"version": 1, "access_token": "old-access",
            "refresh_token": "old-refresh", "account_id": "account-123", "expires_at": 1});
        std::fs::write(&path, unbound.to_string()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let error = manager.credentials().await.unwrap_err();
        assert_eq!(error, LoginRequired::Unbound.into());
        assert!(error.message.contains("skyhook auth login codex"));
        assert_eq!(
            manager.status().await.unwrap(),
            AuthStatus::LoginRequired(LoginRequired::Unbound)
        );
        assert_eq!(count.load(Ordering::SeqCst), 0);
        server.abort();
    }

    #[tokio::test]
    async fn refresh_rejection_preserves_credentials_and_redacts_response() {
        let temp = tempfile::tempdir().unwrap();
        let (url, _, server) = mock(vec![(
            401,
            serde_json::json!({"error":"old-refresh TOP-SECRET"}),
        )])
        .await;
        let issuer = Issuer::parse(&url).unwrap();
        let manager = AuthManager::at(temp.path().to_path_buf(), issuer.clone()).unwrap();
        let (lock, _) = manager.load().await.unwrap();
        manager.save(lock, stored(&issuer, 1)).await.unwrap();
        let err = manager.credentials().await.unwrap_err();
        assert_eq!(err.kind, ProviderErrorKind::Authentication);
        assert!(!format!("{err:?}").contains("TOP-SECRET"));
        assert!(!format!("{err:?}").contains("old-refresh"));
        server.await.unwrap();
        let (_, saved) = manager.load().await.unwrap();
        assert_eq!(saved.current().unwrap().refresh_token, "old-refresh");
    }
}
