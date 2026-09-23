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

use crate::provider::{ProviderError, ProviderErrorKind};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use oauth::{CLIENT_ID, ISSUER, TokenResponse, form_body, response_json};
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use zeroize::{Zeroize, Zeroizing};

pub type AuthError = ProviderError;
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
    LoggedOut,
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
    issuer: String,
}

impl AuthManager {
    /// Does not read credentials or perform network I/O. Login is deferred until
    /// explicitly requested; credentials() never launches an interactive flow.
    pub fn new() -> Result<Self, AuthError> {
        let directory = crate::config::user_config_directory()
            .ok_or_else(|| error("Cannot locate the Skyhook configuration directory"))?;
        Self::at(directory, ISSUER.into())
    }

    fn at(directory: PathBuf, issuer: String) -> Result<Self, AuthError> {
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
    pub async fn credentials(&self) -> Result<Credentials, AuthError> {
        let (lock, stored) = self.load().await?;
        let mut stored = stored
            .ok_or_else(|| error("Codex is not logged in; run `skyhook auth login codex`"))?;
        if stored.expires_at <= now()?.saturating_add(REFRESH_SKEW) {
            let response = self
                .inner
                .client
                .post(format!("{}/oauth/token", self.inner.issuer))
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
            stored = token.into_stored(Some(&stored))?;
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

pub async fn login(headless: bool) -> Result<(), AuthError> {
    AuthManager::new()?.login(headless).await
}
pub async fn status() -> Result<AuthStatus, AuthError> {
    AuthManager::new()?.status().await
}
/// Removes Skyhook's local tokens only; does not revoke other sessions.
pub async fn logout() -> Result<(), AuthError> {
    AuthManager::new()?.logout().await
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

/// Shared by the transport's loopback-only integration tests.
#[cfg(test)]
pub(super) fn test_manager(directory: PathBuf) -> AuthManager {
    let manager = AuthManager::at(directory, "http://127.0.0.1:1".into()).unwrap();
    let _lock = store::lock_store(&manager.inner.directory).unwrap();
    store::write_store(
        &manager.inner.directory,
        &store::Stored {
            version: 1,
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

    #[tokio::test]
    async fn independent_managers_coordinate_rotating_refresh() {
        let temp = tempfile::tempdir().unwrap();
        let (url, count, server) = mock(vec![(200, token_body())]).await;
        let one = AuthManager::at(temp.path().to_path_buf(), url.clone()).unwrap();
        let two = AuthManager::at(temp.path().to_path_buf(), url).unwrap();
        let (lock, _) = one.load().await.unwrap();
        one.save(lock, stored(1)).await.unwrap();
        let (a, b) = tokio::join!(one.credentials(), two.credentials());
        assert_eq!(a.unwrap().access_token, b.unwrap().access_token);
        assert_eq!(count.load(Ordering::SeqCst), 1);
        let requests = server.await.unwrap();
        assert!(requests[0].starts_with("POST /oauth/token "));
        assert!(requests[0].contains("grant_type=refresh_token"));
        assert!(requests[0].contains("refresh_token=old-refresh"));
        let (_, stored) = one.load().await.unwrap();
        assert_eq!(stored.unwrap().refresh_token, "rotated-refresh");
    }

    #[tokio::test]
    async fn refresh_rejection_preserves_credentials_and_redacts_response() {
        let temp = tempfile::tempdir().unwrap();
        let (url, _, server) = mock(vec![(
            401,
            serde_json::json!({"error":"old-refresh TOP-SECRET"}),
        )])
        .await;
        let manager = AuthManager::at(temp.path().to_path_buf(), url).unwrap();
        let (lock, _) = manager.load().await.unwrap();
        manager.save(lock, stored(1)).await.unwrap();
        let err = manager.credentials().await.unwrap_err();
        assert_eq!(err.kind, ProviderErrorKind::Authentication);
        assert!(!format!("{err:?}").contains("TOP-SECRET"));
        assert!(!format!("{err:?}").contains("old-refresh"));
        server.await.unwrap();
        let (_, stored) = manager.load().await.unwrap();
        assert_eq!(stored.unwrap().refresh_token, "old-refresh");
    }
}
