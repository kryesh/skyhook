//! Public-client OAuth, PKCE callbacks, device authorization, and token validation.
use super::store::{Stored, read_epoch, store_fingerprint};
use super::{AccountId, AuthManager, Issuer, Token, blocking, now, random_string};
use crate::provider::{
    ProviderError,
    ProviderErrorKind::{Authentication, Transport},
    codec::common::{lenient, lenient_count},
    http::{
        errors::{ErrorSignals, Reading},
        transport::retry_after,
    },
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zeroize::{Zeroize, Zeroizing};

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// Where the browser returns an authorization, on localhost.
const CALLBACK_PORT: u16 = 1455;
const CALLBACK_PATH: &str = "/auth/callback";
const LOGIN_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const CALLBACK_READ_TIMEOUT: Duration = Duration::from_secs(5);
const CALLBACK_REPLY_TIMEOUT: Duration = Duration::from_secs(2);
const BROWSER_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_CALLBACK_BYTES: usize = 16 * 1024;
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_CLAIMS_BYTES: usize = 1024 * 1024;
const MAX_USER_CODE_BYTES: usize = 128;
const DEFAULT_POLL_SECS: u64 = 5;
const MIN_POLL_SECS: u64 = 1;
const MAX_POLL_SECS: u64 = 60;

impl AuthManager {
    pub async fn login(&self, headless: bool) -> Result<(), ProviderError> {
        self.sign_in(async {
            if headless {
                self.device_login(tokio::time::sleep).await
            } else {
                self.browser_login().await
            }
        })
        .await
    }

    /// Persist the tokens `flow` obtains. The lock is not held while a human
    /// signs in: the previous generation is snapshotted, and a concurrent
    /// logout, login or refresh is never overwritten. Login never reads any
    /// official-client credential file.
    async fn sign_in(
        &self,
        flow: impl Future<Output = Result<TokenResponse, ProviderError>>,
    ) -> Result<(), ProviderError> {
        let (lock, previous) = self.load().await?;
        let previous = previous.current().map(store_fingerprint);
        let epoch = blocking(move || read_epoch(&lock)).await?;
        let token = tokio::time::timeout(LOGIN_TIMEOUT, flow)
            .await
            .map_err(|_| Authentication.error("Codex login timed out; start login again"))??;
        let stored = token.into_stored(&self.inner.issuer, None)?;
        let (lock, current) = self.load().await?;
        let (lock, current_epoch) = blocking(move || {
            let epoch = read_epoch(&lock)?;
            Ok((lock, epoch))
        })
        .await?;
        if current_epoch != epoch || current.current().map(store_fingerprint) != previous {
            return Err(
                Authentication.error("Codex credentials changed during login; start login again")
            );
        }
        self.save(lock, stored).await
    }

    /// Post `form` to the issuer's token endpoint as this client.
    pub(super) async fn token(
        &self,
        form: &[(&str, &str)],
    ) -> Result<TokenResponse, ProviderError> {
        let response = self
            .inner
            .client
            .post(self.inner.issuer.join("oauth/token"))
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(form_body(&[&[("client_id", CLIENT_ID)], form].concat()))
            .send()
            .await
            .map_err(|_| Transport.error("Codex could not reach the authentication server"))?;
        response_json(response).await
    }

    async fn exchange(
        &self,
        code: &str,
        verifier: &str,
        redirect: &str,
    ) -> Result<TokenResponse, ProviderError> {
        self.token(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", redirect),
        ])
        .await
    }

    /// `pace` waits out the issuer's interval between polls.
    async fn device_login<F: Future<Output = ()>>(
        &self,
        pace: impl Fn(Duration) -> F,
    ) -> Result<TokenResponse, ProviderError> {
        let response = self
            .inner
            .client
            .post(self.inner.issuer.join("api/accounts/deviceauth/usercode"))
            .json(&serde_json::json!({"client_id": CLIENT_ID}))
            .send()
            .await
            .map_err(|_| Transport.error("Cannot request a Codex device login code"))?;
        let device: DeviceCode = response_json(response).await?;
        if device.device_auth_id.is_empty()
            || device.user_code.is_empty()
            || device.user_code.len() > MAX_USER_CODE_BYTES
            || device.user_code.chars().any(char::is_control)
        {
            return Err(Authentication.error("Invalid Codex device login response"));
        }
        eprintln!(
            "Open {} and enter this one-time code: {}\nOnly continue if you initiated this Skyhook login. The code expires in 15 minutes.",
            self.inner.issuer.join("codex/device"),
            device.user_code
        );
        self.poll_device(&device, pace).await
    }

    async fn poll_device<F: Future<Output = ()>>(
        &self,
        device: &DeviceCode,
        pace: impl Fn(Duration) -> F,
    ) -> Result<TokenResponse, ProviderError> {
        let interval = device
            .interval
            .unwrap_or(DEFAULT_POLL_SECS)
            .clamp(MIN_POLL_SECS, MAX_POLL_SECS);
        loop {
            let response = self.inner.client.post(self.inner.issuer.join("api/accounts/deviceauth/token"))
                .json(&serde_json::json!({"device_auth_id":device.device_auth_id,"user_code":device.user_code}))
                .send().await.map_err(|_| Transport.error("Codex device authorization polling failed"))?;
            if matches!(response.status().as_u16(), 403 | 404) {
                pace(Duration::from_secs(interval)).await;
                continue;
            }
            let code: DeviceAuthorization = response_json(response).await?;
            if code.authorization_code.is_empty() || code.code_verifier.is_empty() {
                return Err(Authentication.error("Invalid Codex device authorization response"));
            }
            return self
                .exchange(
                    &code.authorization_code,
                    &code.code_verifier,
                    self.inner.issuer.join("deviceauth/callback").as_str(),
                )
                .await;
        }
    }

    async fn browser_login(&self) -> Result<TokenResponse, ProviderError> {
        let listener =
            tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, CALLBACK_PORT))
                .await
                .map_err(|_| {
                    Authentication.error(format!(
                "Cannot listen on localhost:{CALLBACK_PORT}; stop the other login or use --headless"
            ))
                })?;
        let redirect = format!("http://localhost:{CALLBACK_PORT}{CALLBACK_PATH}");
        let verifier = Zeroizing::new(random_string()?);
        let state = Zeroizing::new(random_string()?);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let mut url = self.inner.issuer.join("oauth/authorize");
        url.query_pairs_mut().extend_pairs([
            ("response_type", "code"),
            ("client_id", CLIENT_ID),
            ("redirect_uri", redirect.as_str()),
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
                .map_err(|_| Authentication.error("Codex login callback listener failed"))?;
            let request =
                tokio::time::timeout(CALLBACK_READ_TIMEOUT, read_callback(&mut stream)).await;
            let code = match request {
                Ok(Ok(request)) => parse_callback(&request, &state),
                _ => Err(Authentication.error("Invalid Codex login callback")),
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
                tokio::time::timeout(CALLBACK_REPLY_TIMEOUT, stream.write_all(reply.as_bytes()))
                    .await;
            if let Ok(code) = code {
                return self.exchange(&code, &verifier, &redirect).await;
            }
        }
    }
}

// reqwest's optional `form` feature is unnecessary: URL query serialization
// uses the same application/x-www-form-urlencoded encoding.
fn form_body(pairs: &[(&str, &str)]) -> String {
    let mut url = reqwest::Url::parse("http://localhost/").expect("static URL");
    url.query_pairs_mut().extend_pairs(pairs.iter().copied());
    url.query().unwrap_or_default().to_owned()
}

#[derive(Deserialize)]
pub(super) struct TokenResponse {
    access_token: Token,
    refresh_token: Option<Token>,
    /// Only a source of claims: an unusable one yields none.
    #[serde(default, deserialize_with = "claims_token")]
    id_token: Option<Token>,
    expires_in: Option<u64>,
}

fn claims_token<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Token>, D::Error> {
    let token = Option::<String>::deserialize(deserializer)?;
    Ok(token.and_then(|token| Token::try_from(token).ok()))
}
impl TokenResponse {
    pub(super) fn into_stored(
        self,
        issuer: &Issuer,
        previous: Option<&Stored>,
    ) -> Result<Stored, ProviderError> {
        let access_claims = Claims::of(&self.access_token);
        let id_claims = self.id_token.as_ref().and_then(Claims::of);
        // Claims are metadata from the TLS-authenticated token endpoint, not a
        // locally verified authentication assertion. Never accept user-supplied JWTs.
        let account_id = [&id_claims, &access_claims]
            .into_iter()
            .find_map(|claims| claims.as_ref()?.auth.as_ref()?.chatgpt_account_id.clone())
            .map(AccountId::try_from)
            .transpose()
            .map_err(|_| Authentication.error("Codex token names an invalid ChatGPT account ID"))?
            .or_else(|| previous.map(|p| p.account_id.clone()))
            .ok_or_else(|| {
                Authentication.error("Codex login did not return a ChatGPT account ID")
            })?;
        if previous.is_some_and(|p| p.account_id != account_id) {
            return Err(Authentication.error("Codex account changed during refresh; log in again"));
        }
        let refresh_token = self
            .refresh_token
            .as_ref()
            .or_else(|| previous.map(|p| &p.refresh_token))
            .ok_or_else(|| Authentication.error("Codex login did not return a refresh token"))?;
        let current = now()?;
        let expires_at = match (
            self.expires_in.and_then(|s| current.checked_add(s)),
            access_claims.and_then(|claims| claims.exp),
        ) {
            (Some(a), Some(b)) => a.min(b),
            (Some(a), None) | (None, Some(a)) => a,
            _ => {
                return Err(Authentication.error("Codex token response did not include an expiry"));
            }
        };
        if expires_at <= current {
            return Err(Authentication.error("Invalid Codex token response; log in again"));
        }
        Ok(Stored {
            issuer: issuer.clone(),
            access_token: self.access_token.clone(),
            refresh_token: refresh_token.clone(),
            account_id,
            expires_at,
        })
    }
}

/// What a token's JWT payload says about it. Each claim is read on its own, so
/// a malformed one cannot hide the others.
#[derive(Deserialize)]
struct Claims {
    #[serde(default, deserialize_with = "lenient")]
    exp: Option<u64>,
    #[serde(
        default,
        rename = "https://api.openai.com/auth",
        deserialize_with = "lenient"
    )]
    auth: Option<AuthClaims>,
}

#[derive(Deserialize)]
struct AuthClaims {
    chatgpt_account_id: Option<String>,
}

impl Claims {
    fn of(token: &Token) -> Option<Self> {
        let mut parts = token.as_str().split('.');
        parts.next()?;
        let payload = parts.next()?;
        parts.next()?;
        if parts.next().is_some() || payload.len() > MAX_CLAIMS_BYTES {
            return None;
        }
        let bytes = Zeroizing::new(URL_SAFE_NO_PAD.decode(payload).ok()?);
        serde_json::from_slice(&bytes).ok()
    }
}

async fn response_json<T: serde::de::DeserializeOwned>(
    mut response: reqwest::Response,
) -> Result<T, ProviderError> {
    if !response.status().is_success() {
        // Never include response bodies, URLs, reqwest errors, or OAuth error
        // descriptions: these can echo credentials and authorization codes.
        let status = response.status().as_u16();
        let retry_after = retry_after(response.headers(), std::time::SystemTime::now());
        let read =
            Reading::default().kind(Some(status), &Value::Null, ErrorSignals::NONE, retry_after);
        // OAuth refuses a grant or client with 400.
        let kind = if status == 400 { Authentication } else { read };
        let message = match (kind, status) {
            (Authentication, _) => {
                "Codex authorization was rejected; log in again (device auth may need enabling in ChatGPT settings)"
            }
            (_, 404) => "Codex device authorization is unavailable; use browser login",
            _ => "Codex authentication server returned an error; try again later",
        };
        return Err(kind.error(message).with_retry_after(retry_after));
    }
    let mut bytes = Zeroizing::new(Vec::new());
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| Transport.error("Cannot read Codex authentication response"))?
    {
        if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(Authentication.error("Codex authentication response is too large"));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| {
        Authentication.error("Codex authentication server returned a malformed response")
    })
}

#[derive(Deserialize)]
struct DeviceCode {
    device_auth_id: String,
    #[serde(alias = "usercode")]
    user_code: String,
    /// Seconds between polls; an unreadable value leaves the default.
    #[serde(default, deserialize_with = "lenient_count")]
    interval: Option<u64>,
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

async fn read_callback(
    stream: &mut tokio::net::TcpStream,
) -> Result<Zeroizing<String>, ProviderError> {
    let mut bytes = Zeroizing::new(Vec::new());
    let mut chunk = [0u8; 1024];
    while !bytes.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|_| Authentication.error("Cannot read Codex login callback"))?;
        if n == 0 || bytes.len() + n > MAX_CALLBACK_BYTES {
            return Err(Authentication.error("Invalid Codex login callback"));
        }
        bytes.extend_from_slice(&chunk[..n]);
        chunk.zeroize();
    }
    String::from_utf8(bytes.to_vec())
        .map(Zeroizing::new)
        .map_err(|_| Authentication.error("Invalid Codex login callback"))
}
fn parse_callback(request: &str, state: &str) -> Result<Zeroizing<String>, ProviderError> {
    let invalid = || Authentication.error("Invalid Codex login callback");
    let mut line = request
        .lines()
        .next()
        .ok_or_else(invalid)?
        .split_whitespace();
    if line.next() != Some("GET") {
        return Err(invalid());
    }
    let target = line.next().ok_or_else(invalid)?;
    if !target.starts_with(CALLBACK_PATH) {
        return Err(invalid());
    }
    let url = reqwest::Url::parse(&format!("http://localhost{target}")).map_err(|_| invalid())?;
    if url.path() != CALLBACK_PATH {
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
        let _ = tokio::time::timeout(BROWSER_TIMEOUT, command.status()).await;
    }
}

#[cfg(all(test, unix))]
pub(super) mod tests {
    use super::super::store::tests::stored;
    use super::*;
    use crate::provider::http::transport::tests::read_request as read_http_request;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    fn jwt(expiry: u64) -> String {
        format!("e30.{}.signature", URL_SAFE_NO_PAD.encode(serde_json::to_vec(&serde_json::json!({
            "exp": expiry, "https://api.openai.com/auth": {"chatgpt_account_id":"account-123"}
        })).unwrap()))
    }

    pub(in super::super) fn token_body() -> serde_json::Value {
        serde_json::json!({"access_token":jwt(now().unwrap()+3600),"refresh_token":"rotated-refresh","expires_in":3600})
    }

    /// A loopback-only HTTP mock; joins are always awaited by callers.
    pub(in super::super) async fn mock(
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
                let (mut stream, _) = crate::tests::bounded(listener.accept()).await.unwrap();
                requests.push(read_http_request(&mut stream).await);
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
    async fn headless_login_polls_pending_codes_exchanges_pkce_and_persists_credentials() {
        let temp = tempfile::tempdir().unwrap();
        let (url, count, server) = mock(vec![
            (200, serde_json::json!({"device_auth_id":"device-123","usercode":"ABCD-123","interval":"0"})),
            (403, serde_json::json!({})),
            (404, serde_json::json!({})),
            (200, serde_json::json!({"authorization_code":"issued-code","code_verifier":"issued-verifier","code_challenge":"unused"})),
            (200, token_body()),
        ]).await;
        let manager =
            AuthManager::at(temp.path().to_path_buf(), Issuer::parse(&url).unwrap()).unwrap();
        let paced = std::sync::Mutex::new(Vec::new());
        let pace = |interval| {
            paced.lock().unwrap().push(interval);
            std::future::ready(())
        };
        manager.sign_in(manager.device_login(pace)).await.unwrap();
        // Each pending code waits the issuer's interval, raised to the minimum.
        let minimum = Duration::from_secs(MIN_POLL_SECS);
        assert_eq!(*paced.lock().unwrap(), [minimum, minimum]);
        assert_eq!(count.load(Ordering::SeqCst), 5);
        let requests = server.await.unwrap();
        assert!(requests[0].starts_with("POST /api/accounts/deviceauth/usercode "));
        assert!(requests[0].contains(CLIENT_ID));
        for request in &requests[1..4] {
            assert!(request.starts_with("POST /api/accounts/deviceauth/token "));
            assert!(request.contains("device-123"));
            assert!(request.contains("ABCD-123"));
        }
        assert!(requests[4].starts_with("POST /oauth/token "));
        assert!(requests[4].contains("code=issued-code"));
        assert!(requests[4].contains("code_verifier=issued-verifier"));
        assert!(requests[4].contains("deviceauth%2Fcallback"));
        assert!(requests[4].contains(CLIENT_ID));
        assert_eq!(
            manager.credentials().await.unwrap().account_id.as_str(),
            "account-123"
        );
        assert!(temp.path().join(super::super::store::STORE_FILE).exists());
    }

    #[test]
    fn unreadable_device_poll_interval_falls_back_to_the_default() {
        for (interval, seconds) in [
            (serde_json::json!(7), Some(7)),
            (serde_json::json!("7"), Some(7)),
            (serde_json::json!(7.0), Some(7)),
            (serde_json::json!(-1), None),
            (serde_json::json!(true), None),
            (serde_json::json!({}), None),
        ] {
            let device =
                serde_json::json!({"device_auth_id":"d", "user_code":"c", "interval":interval});
            let device: DeviceCode = serde_json::from_value(device).unwrap();
            assert_eq!(device.interval, seconds, "{interval}");
        }
    }

    #[test]
    fn token_validation_requires_account_and_expiry() {
        let parse = |value| serde_json::from_value::<TokenResponse>(value).unwrap();
        let issuer = Issuer::default();
        assert!(parse(serde_json::json!({"access_token":"opaque","refresh_token":"refresh","expires_in":3600})).into_stored(&issuer, None).is_err());
        assert!(
            parse(serde_json::json!({"access_token":jwt(1),"refresh_token":"refresh"}))
                .into_stored(&issuer, None)
                .is_err()
        );
        let previous = stored(&issuer, 1);
        let rotated = parse(serde_json::json!({"access_token":"new-opaque","expires_in":3600}))
            .into_stored(&issuer, Some(&previous))
            .unwrap();
        assert_eq!(rotated.refresh_token.as_str(), "old-refresh");
        assert_eq!(rotated.account_id.as_str(), "account-123");
        // An unusable id_token only loses its claims.
        for id_token in ["", "not a token"] {
            let response = parse(
                serde_json::json!({"access_token":jwt(now().unwrap() + 3600),
                "id_token":id_token, "refresh_token":"refresh"}),
            );
            let stored = response.into_stored(&issuer, None).unwrap();
            assert_eq!(stored.account_id.as_str(), "account-123");
        }
        // A malformed claim hides no other claim, and an invalid account does not
        // fall back to the previous one.
        for claims in [
            r#"{"exp":1.5,"https://api.openai.com/auth":{"chatgpt_account_id":"other"}}"#,
            r#"{"https://api.openai.com/auth":{"chatgpt_account_id":"bad id"}}"#,
            r#"{"exp":1,"https://api.openai.com/auth":{"chatgpt_account_id":5}}"#,
        ] {
            let token = format!("e30.{}.signature", URL_SAFE_NO_PAD.encode(claims));
            let response = parse(serde_json::json!({"access_token":token,"expires_in":3600}));
            assert!(response.into_stored(&issuer, Some(&previous)).is_err());
        }
    }
}
