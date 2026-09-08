//! Skyhook-owned ChatGPT OAuth. Never consults CODEX_HOME, ~/.codex/auth.json,
//! Flux's credential store, or any other application's credentials.
//!
//! Public-client protocol checked against codewandler-flux-credentials 0.59.3
//! (src/lib.rs, codex_authorize_url) and upstream on 2026-09-08:
//! https://github.com/openai/codex/blob/main/codex-rs/login/src/server.rs
//! https://github.com/openai/codex/blob/main/codex-rs/login/src/device_code_auth.rs
//! Device auth is OpenAI's public /api/accounts/deviceauth flow, NOT the
//! unrelated RFC 8628 grant. No client secret or privileged/private API is used.

use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zeroize::{Zeroize, Zeroizing};

use crate::provider::{ProviderError, ProviderErrorKind};

pub type AuthError = ProviderError;
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const ISSUER: &str = "https://auth.openai.com";
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const MAX_BODY: usize = 1024 * 1024;
const LOGIN_TIMEOUT: Duration = Duration::from_secs(15 * 60);
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

#[derive(Serialize, Deserialize)]
struct Stored {
    version: u32,
    access_token: String,
    refresh_token: String,
    account_id: String,
    expires_at: u64,
}
impl Drop for Stored {
    fn drop(&mut self) {
        self.access_token.zeroize();
        self.refresh_token.zeroize();
    }
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

    pub async fn status(&self) -> Result<AuthStatus, AuthError> {
        let (_lock, stored) = self.load().await?;
        Ok(match stored {
            Some(stored) => AuthStatus::LoggedIn {
                account_id: stored.account_id.clone(),
                expires_at: stored.expires_at,
            },
            None => AuthStatus::LoggedOut,
        })
    }

    pub async fn logout(&self) -> Result<(), AuthError> {
        let directory = self.inner.directory.clone();
        blocking(move || {
            let mut lock = lock_store(&directory)?;
            // Persist an epoch even when already logged out: an in-progress
            // login must not resurrect credentials after explicit logout.
            let epoch = random_string()?;
            lock.seek(SeekFrom::Start(0))
                .and_then(|_| lock.write_all(epoch.as_bytes()))
                .and_then(|_| lock.set_len(epoch.len() as u64))
                .and_then(|_| lock.sync_all())
                .map_err(|_| error("Cannot invalidate in-progress Codex login"))?;
            match fs::remove_file(directory.join("codex-oauth.json")) {
                Ok(()) => sync_directory(&directory),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(_) => Err(error("Cannot remove Skyhook Codex credentials")),
            }
            // Do not remove the lock file: other processes may have its inode open.
        })
        .await
    }

    async fn load(&self) -> Result<(File, Option<Stored>), AuthError> {
        let directory = self.inner.directory.clone();
        blocking(move || {
            let lock = lock_store(&directory)?;
            let stored = read_store(&directory)?;
            Ok((lock, stored))
        })
        .await
    }

    async fn save(&self, lock: File, stored: Stored) -> Result<(), AuthError> {
        let directory = self.inner.directory.clone();
        blocking(move || {
            let _lock = lock;
            write_store(&directory, &stored)
        })
        .await
    }

    pub async fn login(&self, headless: bool) -> Result<(), AuthError> {
        // Avoid holding the refresh lock while a human signs in. Snapshot the
        // previous generation and refuse to overwrite a concurrent logout/login
        // or refresh. Login never reads any official-client credential file.
        let (lock, previous) = self.load().await?;
        let previous = previous.as_ref().map(store_fingerprint);
        let epoch = blocking(move || read_epoch(&lock)).await?;
        let token = tokio::time::timeout(LOGIN_TIMEOUT, async {
            if headless {
                self.device_login().await
            } else {
                self.browser_login().await
            }
        })
        .await
        .map_err(|_| error("Codex login timed out; start login again"))??;
        let stored = token.into_stored(None)?;
        let (lock, current) = self.load().await?;
        let (lock, current_epoch) = blocking(move || {
            let epoch = read_epoch(&lock)?;
            Ok((lock, epoch))
        })
        .await?;
        if current_epoch != epoch || current.as_ref().map(store_fingerprint) != previous {
            return Err(error(
                "Codex credentials changed during login; start login again",
            ));
        }
        self.save(lock, stored).await
    }

    async fn exchange(
        &self,
        code: &str,
        verifier: &str,
        redirect: &str,
    ) -> Result<TokenResponse, AuthError> {
        let response = self
            .inner
            .client
            .post(format!("{}/oauth/token", self.inner.issuer))
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(form_body(&[
                ("grant_type", "authorization_code"),
                ("client_id", CLIENT_ID),
                ("code", code),
                ("code_verifier", verifier),
                ("redirect_uri", redirect),
            ]))
            .send()
            .await
            .map_err(|_| {
                error("Codex authorization exchange could not reach the authentication server")
            })?;
        response_json(response).await
    }

    async fn device_login(&self) -> Result<TokenResponse, AuthError> {
        let response = self
            .inner
            .client
            .post(format!(
                "{}/api/accounts/deviceauth/usercode",
                self.inner.issuer
            ))
            .json(&serde_json::json!({"client_id": CLIENT_ID}))
            .send()
            .await
            .map_err(|_| error("Cannot request a Codex device login code"))?;
        let device: DeviceCode = response_json(response).await?;
        if device.device_auth_id.is_empty()
            || device.user_code.is_empty()
            || device.user_code.len() > 128
            || device.user_code.chars().any(char::is_control)
        {
            return Err(error("Invalid Codex device login response"));
        }
        eprintln!(
            "Open {}/codex/device and enter this one-time code: {}\nOnly continue if you initiated this Skyhook login. The code expires in 15 minutes.",
            self.inner.issuer, device.user_code
        );
        self.poll_device(&device).await
    }

    async fn poll_device(&self, device: &DeviceCode) -> Result<TokenResponse, AuthError> {
        let interval = device
            .interval
            .as_ref()
            .and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()))
            .unwrap_or(5)
            .clamp(1, 60);
        loop {
            let response = self.inner.client.post(format!("{}/api/accounts/deviceauth/token", self.inner.issuer))
                .json(&serde_json::json!({"device_auth_id":device.device_auth_id,"user_code":device.user_code}))
                .send().await.map_err(|_| error("Codex device authorization polling failed"))?;
            if matches!(response.status().as_u16(), 403 | 404) {
                tokio::time::sleep(Duration::from_secs(interval)).await;
                continue;
            }
            let code: DeviceAuthorization = response_json(response).await?;
            if code.authorization_code.is_empty() || code.code_verifier.is_empty() {
                return Err(error("Invalid Codex device authorization response"));
            }
            return self
                .exchange(
                    &code.authorization_code,
                    &code.code_verifier,
                    &format!("{}/deviceauth/callback", self.inner.issuer),
                )
                .await;
        }
    }

    async fn browser_login(&self) -> Result<TokenResponse, AuthError> {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 1455))
            .await
            .map_err(|_| {
                error("Cannot listen on localhost:1455; stop the other login or use --headless")
            })?;
        let verifier = Zeroizing::new(random_string()?);
        let state = Zeroizing::new(random_string()?);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let mut url = reqwest::Url::parse(&format!("{}/oauth/authorize", self.inner.issuer))
            .map_err(|_| error("Invalid Codex authorization URL"))?;
        url.query_pairs_mut().extend_pairs([
            ("response_type", "code"),
            ("client_id", CLIENT_ID),
            ("redirect_uri", REDIRECT_URI),
            ("scope", "openid profile email offline_access"),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
            ("state", state.as_str()),
            ("id_token_add_organizations", "true"),
            ("codex_cli_simplified_flow", "true"),
            ("originator", "skyhook"),
        ]);
        eprintln!("Open this URL to sign in to Codex for Skyhook:\n{url}");
        launch_browser(url.as_str()).await;
        loop {
            let (mut stream, _) = listener
                .accept()
                .await
                .map_err(|_| error("Codex login callback listener failed"))?;
            let request =
                tokio::time::timeout(Duration::from_secs(5), read_callback(&mut stream)).await;
            let code = match request {
                Ok(Ok(request)) => parse_callback(&request, &state),
                _ => Err(error("Invalid Codex login callback")),
            };
            let (status, body) = if code.is_ok() {
                (
                    "200 OK",
                    "Authorization received. Return to Skyhook to complete login.",
                )
            } else {
                (
                    "400 Bad Request",
                    "Invalid callback. Return to Skyhook and try again.",
                )
            };
            let reply = format!(
                "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nCache-Control: no-store\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let _ =
                tokio::time::timeout(Duration::from_secs(2), stream.write_all(reply.as_bytes()))
                    .await;
            if let Ok(code) = code {
                return self.exchange(&code, &verifier, REDIRECT_URI).await;
            }
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

// reqwest's optional `form` feature is unnecessary: URL query serialization
// uses the same application/x-www-form-urlencoded encoding.
fn form_body(pairs: &[(&str, &str)]) -> String {
    let mut url = reqwest::Url::parse("http://localhost/").expect("static URL");
    url.query_pairs_mut().extend_pairs(pairs.iter().copied());
    url.query().unwrap_or_default().to_owned()
}

fn random_string() -> Result<String, AuthError> {
    let mut bytes = Zeroizing::new([0u8; 32]);
    getrandom::fill(&mut *bytes)
        .map_err(|_| error("Cannot obtain secure randomness for Codex login"))?;
    Ok(URL_SAFE_NO_PAD.encode(*bytes))
}

// OS locking is performed in spawn_blocking, never on a Tokio worker. Bounded
// try-lock polling also bounds the lifetime of a cancelled blocking operation.
fn lock_store(directory: &Path) -> Result<File, AuthError> {
    secure_directory(directory)?;
    let file = private_open(&directory.join("codex-oauth.lock"), true)
        .map_err(|_| error("Cannot open Skyhook Codex credential lock"))?;
    let start = std::time::Instant::now();
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(file),
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    && start.elapsed() < Duration::from_secs(60) =>
            {
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => {
                return Err(error(
                    "Cannot acquire Skyhook Codex credential lock; try again",
                ));
            }
        }
    }
}

#[cfg(unix)]
fn secure_directory(directory: &Path) -> Result<(), AuthError> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder
        .create(directory)
        .map_err(|_| error("Cannot create Skyhook credential directory"))?;
    let metadata = fs::symlink_metadata(directory)
        .map_err(|_| error("Cannot inspect Skyhook credential directory"))?;
    if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(error(
            "Skyhook credential directory must be an owned, non-symlink directory",
        ));
    }
    // Existing config directories may intentionally contain readable config.
    // Do not chmod unrelated configuration; private files protect token contents.
    if metadata.permissions().mode() & 0o022 != 0 {
        return Err(error(
            "Skyhook credential directory must not be writable by other users",
        ));
    }
    Ok(())
}
#[cfg(not(unix))]
fn secure_directory(_directory: &Path) -> Result<(), AuthError> {
    // Do not pretend Unix mode bits provide a private ACL on another OS.
    Err(error(
        "Private Skyhook Codex credential storage is currently supported on Unix only",
    ))
}

fn private_open(path: &Path, create: bool) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(create).create(create);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(std::io::ErrorKind::PermissionDenied.into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
            || metadata.nlink() != 1
        {
            return Err(std::io::ErrorKind::PermissionDenied.into());
        }
    }
    Ok(file)
}

fn read_store(directory: &Path) -> Result<Option<Stored>, AuthError> {
    let file = match private_open(&directory.join("codex-oauth.json"), false) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => {
            return Err(error(
                "Cannot read Skyhook Codex credentials; require an owned private regular file",
            ));
        }
    };
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(MAX_BODY as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| error("Cannot read Skyhook Codex credentials"))?;
    if bytes.len() > MAX_BODY {
        return Err(error("Skyhook Codex credential file is too large"));
    }
    let stored: Stored = serde_json::from_slice(&bytes)
        .map_err(|_| error("Skyhook Codex credentials are malformed; log in again"))?;
    if stored.version != 1
        || !valid_token(&stored.access_token)
        || !valid_token(&stored.refresh_token)
        || !valid_account(&stored.account_id)
    {
        return Err(error("Skyhook Codex credentials are invalid; log in again"));
    }
    Ok(Some(stored))
}

fn write_store(directory: &Path, stored: &Stored) -> Result<(), AuthError> {
    let bytes = Zeroizing::new(
        serde_json::to_vec(stored)
            .map_err(|_| error("Cannot serialize Skyhook Codex credentials"))?,
    );
    let mut temp = tempfile::NamedTempFile::new_in(directory)
        .map_err(|_| error("Cannot create private Skyhook credential file"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temp.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|_| error("Cannot make Skyhook credentials private"))?;
    }
    temp.write_all(&bytes)
        .and_then(|_| temp.as_file().sync_all())
        .map_err(|_| error("Cannot write Skyhook Codex credentials"))?;
    temp.persist(directory.join("codex-oauth.json"))
        .map_err(|_| error("Cannot atomically save Skyhook Codex credentials"))?;
    sync_directory(directory)
}
fn sync_directory(directory: &Path) -> Result<(), AuthError> {
    File::open(directory)
        .and_then(|f| f.sync_all())
        .map_err(|_| error("Cannot sync Skyhook credential directory"))
}
fn read_epoch(mut lock: &File) -> Result<Vec<u8>, AuthError> {
    lock.seek(SeekFrom::Start(0))
        .map_err(|_| error("Cannot inspect Codex login generation"))?;
    let mut epoch = Vec::new();
    lock.take(128)
        .read_to_end(&mut epoch)
        .map_err(|_| error("Cannot read Codex login generation"))?;
    Ok(epoch)
}

fn store_fingerprint(stored: &Stored) -> Vec<u8> {
    let mut hash = Sha256::new();
    hash.update(stored.access_token.as_bytes());
    hash.update([0]);
    hash.update(stored.refresh_token.as_bytes());
    hash.finalize().to_vec()
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}
impl Drop for TokenResponse {
    fn drop(&mut self) {
        self.access_token.zeroize();
        if let Some(v) = &mut self.refresh_token {
            v.zeroize();
        }
        if let Some(v) = &mut self.id_token {
            v.zeroize();
        }
    }
}
impl TokenResponse {
    fn into_stored(self, previous: Option<&Stored>) -> Result<Stored, AuthError> {
        let access_claims = jwt_claims(&self.access_token);
        let id_claims = self.id_token.as_deref().and_then(jwt_claims);
        // Claims are metadata from the TLS-authenticated token endpoint, not a
        // locally verified authentication assertion. Never accept user-supplied JWTs.
        let account_id = id_claims
            .as_ref()
            .and_then(account_claim)
            .or_else(|| access_claims.as_ref().and_then(account_claim))
            .or_else(|| previous.map(|p| p.account_id.clone()))
            .ok_or_else(|| error("Codex login did not return a ChatGPT account ID"))?;
        if previous.is_some_and(|p| p.account_id != account_id) {
            return Err(error("Codex account changed during refresh; log in again"));
        }
        let refresh_token = self
            .refresh_token
            .as_deref()
            .or_else(|| previous.map(|p| p.refresh_token.as_str()))
            .ok_or_else(|| error("Codex login did not return a refresh token"))?;
        let current = now()?;
        let expires_at = match (
            self.expires_in.and_then(|s| current.checked_add(s)),
            access_claims
                .as_ref()
                .and_then(|v| v.get("exp"))
                .and_then(|v| v.as_u64()),
        ) {
            (Some(a), Some(b)) => a.min(b),
            (Some(a), None) | (None, Some(a)) => a,
            _ => return Err(error("Codex token response did not include an expiry")),
        };
        if !valid_token(&self.access_token)
            || !valid_token(refresh_token)
            || !valid_account(&account_id)
            || expires_at <= current
        {
            return Err(error("Invalid Codex token response; log in again"));
        }
        Ok(Stored {
            version: 1,
            access_token: self.access_token.clone(),
            refresh_token: refresh_token.into(),
            account_id,
            expires_at,
        })
    }
}
fn valid_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= 128 * 1024
        && !token.chars().any(char::is_whitespace)
        && !token.chars().any(char::is_control)
}
fn valid_account(account: &str) -> bool {
    !account.is_empty()
        && account.len() <= 256
        && account
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
fn jwt_claims(token: &str) -> Option<serde_json::Value> {
    let mut parts = token.split('.');
    parts.next()?;
    let payload = parts.next()?;
    parts.next()?;
    if parts.next().is_some() || payload.len() > MAX_BODY {
        return None;
    }
    let bytes = Zeroizing::new(URL_SAFE_NO_PAD.decode(payload).ok()?);
    serde_json::from_slice(&bytes).ok()
}
fn account_claim(claims: &serde_json::Value) -> Option<String> {
    claims
        .get("https://api.openai.com/auth")?
        .get("chatgpt_account_id")?
        .as_str()
        .map(str::to_owned)
}

async fn response_json<T: serde::de::DeserializeOwned>(
    mut response: reqwest::Response,
) -> Result<T, AuthError> {
    if !response.status().is_success() {
        // Never include response bodies, URLs, reqwest errors, or OAuth error
        // descriptions: these can echo credentials and authorization codes.
        return Err(match response.status().as_u16() {
            400 | 401 | 403 => error(
                "Codex authorization was rejected; log in again (device auth may need enabling in ChatGPT settings)",
            ),
            404 => error("Codex device authorization is unavailable; use browser login"),
            429 => error("Codex authentication rate limited; try again later"),
            _ => error("Codex authentication server returned an error; try again later"),
        });
    }
    let mut bytes = Zeroizing::new(Vec::new());
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| error("Cannot read Codex authentication response"))?
    {
        if bytes.len().saturating_add(chunk.len()) > MAX_BODY {
            return Err(error("Codex authentication response is too large"));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| error("Codex authentication server returned a malformed response"))
}

#[derive(Deserialize)]
struct DeviceCode {
    device_auth_id: String,
    #[serde(alias = "usercode")]
    user_code: String,
    #[serde(default)]
    interval: Option<serde_json::Value>,
}
#[derive(Deserialize)]
struct DeviceAuthorization {
    authorization_code: String,
    code_verifier: String,
}
impl Drop for DeviceAuthorization {
    fn drop(&mut self) {
        self.authorization_code.zeroize();
        self.code_verifier.zeroize();
    }
}

async fn read_callback(stream: &mut tokio::net::TcpStream) -> Result<Zeroizing<String>, AuthError> {
    let mut bytes = Zeroizing::new(Vec::new());
    let mut chunk = [0u8; 1024];
    while !bytes.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|_| error("Cannot read Codex login callback"))?;
        if n == 0 || bytes.len() + n > 16 * 1024 {
            return Err(error("Invalid Codex login callback"));
        }
        bytes.extend_from_slice(&chunk[..n]);
        chunk.zeroize();
    }
    String::from_utf8(bytes.to_vec())
        .map(Zeroizing::new)
        .map_err(|_| error("Invalid Codex login callback"))
}
fn parse_callback(request: &str, state: &str) -> Result<Zeroizing<String>, AuthError> {
    let invalid = || error("Invalid Codex login callback");
    let mut line = request
        .lines()
        .next()
        .ok_or_else(invalid)?
        .split_whitespace();
    if line.next() != Some("GET") {
        return Err(invalid());
    }
    let target = line.next().ok_or_else(invalid)?;
    if !target.starts_with("/auth/callback?") {
        return Err(invalid());
    }
    let url = reqwest::Url::parse(&format!("http://localhost{target}")).map_err(|_| invalid())?;
    if url.path() != "/auth/callback" {
        return Err(invalid());
    }
    let mut received_state = None;
    let mut code = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "state" if received_state.is_none() => received_state = Some(value.into_owned()),
            "code" if code.is_none() => code = Some(Zeroizing::new(value.into_owned())),
            "state" | "code" | "error" => return Err(invalid()),
            _ => {}
        }
    }
    if received_state.as_deref() != Some(state) {
        return Err(invalid());
    }
    code.filter(|c| !c.is_empty()).ok_or_else(invalid)
}

async fn launch_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let command = Some(("open", vec![url]));
    #[cfg(all(unix, not(target_os = "macos")))]
    let command = Some(("xdg-open", vec![url]));
    #[cfg(not(unix))]
    let command: Option<(&str, Vec<&str>)> = None;
    if let Some((program, args)) = command {
        // No shell interpolation, no inherited output, no un-reaped background
        // process. Failure is harmless: the URL was printed for manual opening.
        let mut command = tokio::process::Command::new(program);
        command
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let _ = tokio::time::timeout(Duration::from_secs(3), command.status()).await;
    }
}

/// Shared by the transport's loopback-only integration tests.
#[cfg(test)]
pub(super) fn test_manager(directory: PathBuf) -> AuthManager {
    let manager = AuthManager::at(directory, "http://127.0.0.1:1".into()).unwrap();
    let _lock = lock_store(&manager.inner.directory).unwrap();
    write_store(
        &manager.inner.directory,
        &Stored {
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
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn stored(expiry: u64) -> Stored {
        Stored {
            version: 1,
            access_token: "old-access".into(),
            refresh_token: "old-refresh".into(),
            account_id: "account-123".into(),
            expires_at: expiry,
        }
    }
    fn jwt(expiry: u64) -> String {
        format!("e30.{}.signature", URL_SAFE_NO_PAD.encode(serde_json::to_vec(&serde_json::json!({
            "exp": expiry, "https://api.openai.com/auth": {"chatgpt_account_id":"account-123"}
        })).unwrap()))
    }
    fn token_body() -> serde_json::Value {
        serde_json::json!({"access_token":jwt(now().unwrap()+3600),"refresh_token":"rotated-refresh","expires_in":3600})
    }

    /// A loopback-only HTTP mock; joins are always awaited by callers.
    async fn mock(
        replies: Vec<(u16, serde_json::Value)>,
    ) -> (
        String,
        Arc<AtomicUsize>,
        tokio::task::JoinHandle<Vec<String>>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let count = Arc::new(AtomicUsize::new(0));
        let calls = count.clone();
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (status, body) in replies {
                let (mut stream, _) =
                    tokio::time::timeout(Duration::from_secs(10), listener.accept())
                        .await
                        .unwrap()
                        .unwrap();
                let mut bytes = Vec::new();
                let mut buf = [0; 4096];
                loop {
                    let n = stream.read(&mut buf).await.unwrap();
                    assert_ne!(n, 0);
                    bytes.extend_from_slice(&buf[..n]);
                    if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        let header = String::from_utf8_lossy(&bytes[..end]);
                        let length: usize = header
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|s| s.trim().parse().unwrap())
                            })
                            .unwrap_or(0);
                        if bytes.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                requests.push(String::from_utf8(bytes).unwrap());
                calls.fetch_add(1, Ordering::SeqCst);
                let body = body.to_string();
                stream.write_all(format!("HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
            requests
        });
        (url, count, task)
    }

    #[test]
    fn callback_requires_exact_state_path_and_single_code() {
        assert_eq!(
            parse_callback(
                "GET /auth/callback?state=expected&code=abc%2B123 HTTP/1.1\r\n\r\n",
                "expected"
            )
            .unwrap()
            .as_str(),
            "abc+123"
        );
        for target in [
            "/auth/callback?state=wrong&code=secret",
            "/auth/callback?code=secret",
            "/auth/callback?state=expected&code=secret&code=other",
            "/auth/callback?state=expected&state=expected&code=secret",
            "/other?state=expected&code=secret",
            "/auth/callback?state=expected&error=access_denied",
        ] {
            let err =
                parse_callback(&format!("GET {target} HTTP/1.1\r\n\r\n"), "expected").unwrap_err();
            assert!(!err.to_string().contains("secret"));
        }
    }

    #[test]
    fn pkce_entropy_and_form_encoding() {
        let first = random_string().unwrap();
        assert_eq!(first.len(), 43);
        assert_ne!(first, random_string().unwrap());
        assert_eq!(form_body(&[("code", "a+b &c")]), "code=a%2Bb+%26c");
        let expected = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert_eq!(
            URL_SAFE_NO_PAD.encode(Sha256::digest(
                b"dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"
            )),
            expected
        );
    }

    #[tokio::test]
    async fn persisted_private_atomic_credentials_and_logout() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let manager =
            AuthManager::at(temp.path().join("skyhook"), "http://127.0.0.1:1".into()).unwrap();
        assert_eq!(manager.status().await.unwrap(), AuthStatus::LoggedOut);
        assert!(
            manager
                .credentials()
                .await
                .unwrap_err()
                .message
                .contains("skyhook auth login codex")
        );
        let (lock, _) = manager.load().await.unwrap();
        manager
            .save(lock, stored(now().unwrap() + 3600))
            .await
            .unwrap();
        let path = manager.inner.directory.join("codex-oauth.json");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&manager.inner.directory)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            manager.credentials().await.unwrap().access_token,
            "old-access"
        );
        assert!(
            matches!(manager.status().await.unwrap(), AuthStatus::LoggedIn{account_id, ..} if account_id == "account-123")
        );
        assert!(!format!("{:?}", manager.credentials().await.unwrap()).contains("old-access"));
        manager.logout().await.unwrap();
        manager.logout().await.unwrap();
        assert!(!path.exists());
        assert!(manager.inner.directory.join("codex-oauth.lock").exists());
        assert_eq!(manager.status().await.unwrap(), AuthStatus::LoggedOut);
    }

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

    #[tokio::test]
    async fn public_device_poll_and_pkce_exchange() {
        let temp = tempfile::tempdir().unwrap();
        let (url, count, server) = mock(vec![
            (403, serde_json::json!({})),
            (404, serde_json::json!({})),
            (200, serde_json::json!({"authorization_code":"issued-code","code_verifier":"issued-verifier","code_challenge":"unused"})),
            (200, token_body()),
        ]).await;
        let manager = AuthManager::at(temp.path().to_path_buf(), url.clone()).unwrap();
        let device = DeviceCode {
            device_auth_id: "device-123".into(),
            user_code: "ABCD-123".into(),
            interval: Some(serde_json::json!("1")),
        };
        let token = manager
            .poll_device(&device)
            .await
            .unwrap()
            .into_stored(None)
            .unwrap();
        assert_eq!(token.account_id, "account-123");
        assert_eq!(count.load(Ordering::SeqCst), 4);
        let requests = server.await.unwrap();
        assert!(requests[0].starts_with("POST /api/accounts/deviceauth/token "));
        assert!(requests[0].contains("device-123"));
        assert!(requests[3].contains("code=issued-code"));
        assert!(requests[3].contains("code_verifier=issued-verifier"));
        assert!(requests[3].contains("deviceauth%2Fcallback"));
        assert!(requests[3].contains(CLIENT_ID));
    }

    #[test]
    fn private_store_rejects_symlinks_permissive_files_and_invalid_json() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("codex-oauth.json");
        let unrelated = temp.path().join("unrelated");
        fs::write(&unrelated, b"DO-NOT-READ").unwrap();
        symlink(&unrelated, &path).unwrap();
        assert!(read_store(temp.path()).is_err());
        fs::remove_file(&path).unwrap();
        fs::write(&path, b"TOP-SECRET invalid JSON").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_store(temp.path()).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let err = read_store(temp.path()).err().unwrap();
        assert!(!format!("{err:?}").contains("TOP-SECRET"));
    }

    #[tokio::test]
    async fn headless_login_uses_public_usercode_endpoint_and_own_store() {
        let temp = tempfile::tempdir().unwrap();
        let (url, _, server) = mock(vec![
            (200, serde_json::json!({"device_auth_id":"device-123","usercode":"ABCD-123","interval":"5"})),
            (200, serde_json::json!({"authorization_code":"issued-code","code_verifier":"issued-verifier"})),
            (200, token_body()),
        ]).await;
        let manager = AuthManager::at(temp.path().to_path_buf(), url).unwrap();
        manager.login(true).await.unwrap();
        let requests = server.await.unwrap();
        assert!(requests[0].starts_with("POST /api/accounts/deviceauth/usercode "));
        assert!(requests[0].contains(CLIENT_ID));
        assert_eq!(
            manager.credentials().await.unwrap().account_id,
            "account-123"
        );
        assert!(temp.path().join("codex-oauth.json").exists());
    }

    #[tokio::test]
    async fn logout_even_when_empty_changes_login_epoch() {
        let temp = tempfile::tempdir().unwrap();
        let manager =
            AuthManager::at(temp.path().to_path_buf(), "http://127.0.0.1:1".into()).unwrap();
        let (lock, _) = manager.load().await.unwrap();
        let before = read_epoch(&lock).unwrap();
        drop(lock);
        manager.logout().await.unwrap();
        let (lock, stored) = manager.load().await.unwrap();
        assert!(stored.is_none());
        assert_ne!(before, read_epoch(&lock).unwrap());
    }

    #[test]
    fn existing_configuration_permissions_are_not_changed() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o755)).unwrap();
        secure_directory(temp.path()).unwrap();
        assert_eq!(
            fs::metadata(temp.path()).unwrap().permissions().mode() & 0o777,
            0o755
        );
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o777)).unwrap();
        assert!(secure_directory(temp.path()).is_err());
    }

    #[test]
    fn token_validation_requires_account_and_expiry() {
        let parse = |value| serde_json::from_value::<TokenResponse>(value).unwrap();
        assert!(parse(serde_json::json!({"access_token":"opaque","refresh_token":"refresh","expires_in":3600})).into_stored(None).is_err());
        assert!(
            parse(serde_json::json!({"access_token":jwt(1),"refresh_token":"refresh"}))
                .into_stored(None)
                .is_err()
        );
        let previous = stored(1);
        let rotated = parse(serde_json::json!({"access_token":"new-opaque","expires_in":3600}))
            .into_stored(Some(&previous))
            .unwrap();
        assert_eq!(rotated.refresh_token, "old-refresh");
        assert_eq!(rotated.account_id, "account-123");
    }
}
