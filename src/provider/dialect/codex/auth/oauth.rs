//! Public-client OAuth, PKCE callbacks, device authorization, and token validation.
use super::store::{Stored, read_epoch, store_fingerprint, valid_account, valid_token};
use super::{AuthError, AuthManager, Issuer, MAX_BODY, blocking, error, now, random_string};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zeroize::{Zeroize, Zeroizing};

pub(super) const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const LOGIN_TIMEOUT: Duration = Duration::from_secs(15 * 60);

impl AuthManager {
    pub async fn login(&self, headless: bool) -> Result<(), AuthError> {
        // Avoid holding the refresh lock while a human signs in. Snapshot the
        // previous generation and refuse to overwrite a concurrent logout/login
        // or refresh. Login never reads any official-client credential file.
        let (lock, previous) = self.load().await?;
        let previous = previous.current().map(store_fingerprint);
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
        let stored = token.into_stored(&self.inner.issuer, None)?;
        let (lock, current) = self.load().await?;
        let (lock, current_epoch) = blocking(move || {
            let epoch = read_epoch(&lock)?;
            Ok((lock, epoch))
        })
        .await?;
        if current_epoch != epoch || current.current().map(store_fingerprint) != previous {
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
            .post(self.inner.issuer.join("oauth/token"))
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
            .post(self.inner.issuer.join("api/accounts/deviceauth/usercode"))
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
            "Open {} and enter this one-time code: {}\nOnly continue if you initiated this Skyhook login. The code expires in 15 minutes.",
            self.inner.issuer.join("codex/device"),
            device.user_code
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
            let response = self.inner.client.post(self.inner.issuer.join("api/accounts/deviceauth/token"))
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
                    self.inner.issuer.join("deviceauth/callback").as_str(),
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
        let mut url = self.inner.issuer.join("oauth/authorize");
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

// reqwest's optional `form` feature is unnecessary: URL query serialization
// uses the same application/x-www-form-urlencoded encoding.
pub(super) fn form_body(pairs: &[(&str, &str)]) -> String {
    let mut url = reqwest::Url::parse("http://localhost/").expect("static URL");
    url.query_pairs_mut().extend_pairs(pairs.iter().copied());
    url.query().unwrap_or_default().to_owned()
}

#[derive(Deserialize)]
pub(super) struct TokenResponse {
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
    pub(super) fn into_stored(
        self,
        issuer: &Issuer,
        previous: Option<&Stored>,
    ) -> Result<Stored, AuthError> {
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
            issuer: issuer.clone(),
            access_token: self.access_token.clone(),
            refresh_token: refresh_token.into(),
            account_id,
            expires_at,
        })
    }
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

pub(super) async fn response_json<T: serde::de::DeserializeOwned>(
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
                let (mut stream, _) =
                    tokio::time::timeout(Duration::from_secs(10), listener.accept())
                        .await
                        .unwrap()
                        .unwrap();
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
            (200, serde_json::json!({"device_auth_id":"device-123","usercode":"ABCD-123","interval":"1"})),
            (403, serde_json::json!({})),
            (404, serde_json::json!({})),
            (200, serde_json::json!({"authorization_code":"issued-code","code_verifier":"issued-verifier","code_challenge":"unused"})),
            (200, token_body()),
        ]).await;
        let manager =
            AuthManager::at(temp.path().to_path_buf(), Issuer::parse(&url).unwrap()).unwrap();
        manager.login(true).await.unwrap();
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
            manager.credentials().await.unwrap().account_id,
            "account-123"
        );
        assert!(temp.path().join("codex-oauth.json").exists());
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
        assert_eq!(rotated.refresh_token, "old-refresh");
        assert_eq!(rotated.account_id, "account-123");
    }
}
