//! Authenticated connections to one listener. Each has its own HTTP client holding at
//! most one connection, so requests that follow its handshake reuse the connection the
//! handshake authenticated.

use std::{net::Ipv6Addr, time::Duration};

use base64::Engine as _;
use reqwest::{
    StatusCode,
    header::{AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, HeaderValue, WWW_AUTHENTICATE},
};

use super::{
    Auth, Endpoint,
    context::{Carrier, Context, Package, Step},
    error::{DecodeError, WinrmError},
    framing::{ENCRYPTED, SOAP},
    kerberos,
    soap::{IDENTIFY, MAX_ENVELOPE_BYTES, OPERATION_TIMEOUT_SECS},
    tls::{Tls, Trust, channel_binding},
};

const HTTP_PORT: u16 = 5985;
const HTTPS_PORT: u16 = 5986;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// A Receive waits up to its operation timeout before the service answers.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(OPERATION_TIMEOUT_SECS + 40);
/// The most of a reply read: an envelope at its limit, sealed and framed.
const MAX_REPLY_BYTES: usize = MAX_ENVELOPE_BYTES + 64 * 1024;

/// What opens and authenticates a listener's connections.
pub(super) struct Connector {
    url: String,
    trust: Option<Trust>,
    method: Method,
    spn: String,
}

enum Method {
    /// The `Authorization` header every request carries.
    Basic(HeaderValue),
    /// A handshake per connection, after which requests carry no credentials.
    Handshake(Package),
}

/// One authenticated connection.
pub(super) struct Channel {
    http: reqwest::Client,
    session: Session,
}

enum Session {
    /// Requests go as they are: Basic authenticates each, or TLS protects a connection
    /// a handshake authenticated.
    Plain,
    /// Requests are sealed with the context the connection's handshake established.
    Sealed(Box<Context>),
}

/// What the listener answered, before any unsealing.
struct Reply {
    status: StatusCode,
    challenges: Vec<HeaderValue>,
    encrypted: bool,
    certificate: Option<Vec<u8>>,
    body: Vec<u8>,
}

impl Connector {
    pub(super) async fn new(endpoint: &Endpoint, auth: Auth, tls: Tls) -> Result<Self, WinrmError> {
        let trust = Trust::new(tls).await?;
        let (scheme, port) = match trust {
            Some(_) => ("https", HTTPS_PORT),
            None => ("http", HTTP_PORT),
        };
        let port = endpoint.port.map_or(port, |port| port.get());
        // The one host the URL and the service's name both name; text such as
        // `user@host` or `host/path` would otherwise name another in the URL.
        let host = match endpoint.host.parse::<Ipv6Addr>() {
            Ok(ip) => url::Host::Ipv6(ip),
            Err(_) => url::Host::parse(&endpoint.host)
                .map_err(|_| WinrmError::Host(endpoint.host.clone()))?,
        };
        let url = format!("{scheme}://{host}:{port}/wsman");
        let spn = match &host {
            url::Host::Ipv6(ip) => format!("HTTP/{ip}"),
            host => format!("HTTP/{host}"),
        };
        let identity = |username, password: crate::secret::Secret| sspi::AuthIdentity {
            username,
            password: String::from(password.as_str()).into(),
        };
        let method = match auth {
            Auth::Basic { user, password } => {
                let pair = format!("{user}:{}", password.as_str());
                let encoded = base64::engine::general_purpose::STANDARD.encode(pair);
                let mut header = HeaderValue::try_from(format!("Basic {encoded}"))
                    .expect("base64 is a valid header value");
                header.set_sensitive(true);
                Method::Basic(header)
            }
            Auth::Negotiate { user, password } => {
                let username = sspi::Username::parse(&user).map_err(|error| {
                    sspi::Error::new(sspi::ErrorKind::InvalidParameter, error.to_string())
                })?;
                Method::Handshake(Package::Ntlm(identity(username, password)))
            }
            Auth::Kerberos {
                principal,
                password,
                kdc,
            } => {
                let kdc = kerberos::locate(&principal, kdc).await?;
                let identity = identity(principal.username(), password);
                Method::Handshake(Package::Kerberos { identity, kdc })
            }
        };
        Ok(Self {
            url,
            trust,
            method,
            spn,
        })
    }

    pub(super) fn url(&self) -> &str {
        &self.url
    }

    /// A new connection, authenticated.
    pub(super) async fn open(&self) -> Result<Channel, WinrmError> {
        let http = self.http()?;
        let session = self.authenticate(&http).await?;
        Ok(Channel { http, session })
    }

    /// The HTTP client of one connection.
    fn http(&self) -> Result<reqwest::Client, WinrmError> {
        // A forward proxy may relay requests over connections other than the one a
        // handshake authenticated, and a redirect would send them to another listener.
        let builder = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .http1_only()
            .pool_max_idle_per_host(1)
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .tcp_keepalive(Some(Duration::from_secs(30)));
        let builder = match &self.trust {
            Some(trust) => builder
                .use_preconfigured_tls(trust.config.clone())
                .tls_info(true),
            // Plain HTTP trusts nothing. Left its own TLS, the client would load this
            // machine's certificate store and fail to build where there is none.
            None => builder.tls_certs_only([]),
        };
        builder.build().map_err(|error| WinrmError::connect(&error))
    }

    async fn authenticate(&self, http: &reqwest::Client) -> Result<Session, WinrmError> {
        let package = match &self.method {
            Method::Basic(header) => {
                let reply = self.send(http, IDENTIFY.into(), SOAP, Some(header)).await?;
                return match reply.status {
                    StatusCode::UNAUTHORIZED => Err(reply.refused()),
                    status if status.is_success() => Ok(Session::Plain),
                    status => Err(WinrmError::Status(status.as_u16())),
                };
            }
            Method::Handshake(package) => package,
        };
        // Kerberos binds its first token to the TLS channel, so learn the certificate
        // before it.
        let carrier = match self.trust {
            Some(_) => Carrier::Tls(
                (self.send(http, Vec::new(), SOAP, None).await?.certificate)
                    .map(|certificate| channel_binding(&certificate))
                    .transpose()?,
            ),
            None => Carrier::Http,
        };
        let mut context = Context::new(package, self.spn.clone(), carrier)?;
        let mut step = context.step(None).await?;
        loop {
            let (Step::Continue(token) | Step::Established(token)) = &step;
            let header = HeaderValue::try_from(format!(
                "Negotiate {}",
                base64::engine::general_purpose::STANDARD.encode(token)
            ))
            .expect("base64 is a valid header value");
            // An empty body: a sealed connection's first message needs the keys this
            // leg's answer derives.
            let reply = self.send(http, Vec::new(), SOAP, Some(&header)).await?;
            step = match (reply.status, reply.token()?, step) {
                (StatusCode::UNAUTHORIZED, Some(answer), Step::Continue(_)) => {
                    context.step(Some(answer)).await?
                }
                (StatusCode::UNAUTHORIZED, _, _) => return Err(reply.refused()),
                (status, _, _) if !status.is_success() => {
                    return Err(WinrmError::Status(status.as_u16()));
                }
                (_, _, Step::Established(_)) => break,
                // Accepting the connection, the listener may answer with its last
                // token, which must establish the context with nothing left to send.
                (_, Some(answer), Step::Continue(_)) => match context.step(Some(answer)).await? {
                    Step::Established(last) if last.is_empty() => break,
                    Step::Established(_) | Step::Continue(_) => {
                        return Err(WinrmError::Incomplete);
                    }
                },
                (_, None, Step::Continue(_)) => return Err(WinrmError::Incomplete),
            };
        }
        Ok(match self.trust {
            Some(_) => Session::Plain,
            None => Session::Sealed(Box::new(context)),
        })
    }

    async fn send(
        &self,
        http: &reqwest::Client,
        body: Vec<u8>,
        content_type: &'static str,
        authorization: Option<&HeaderValue>,
    ) -> Result<Reply, WinrmError> {
        // HTTP.sys refuses a request without a length, even an empty handshake leg's.
        let mut request = (http.post(&self.url))
            .header(CONTENT_TYPE, content_type)
            .header(CONTENT_LENGTH, body.len())
            .body(body);
        if let Some(authorization) = authorization {
            request = request.header(AUTHORIZATION, authorization);
        }
        let mut response = request.send().await.map_err(|error| self.failed(error))?;
        let headers = response.headers();
        let challenges = headers.get_all(WWW_AUTHENTICATE).iter().cloned().collect();
        let encrypted = (headers.get(CONTENT_TYPE))
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("multipart/encrypted"));
        let certificate = (response.extensions().get::<reqwest::tls::TlsInfo>())
            .and_then(|info| info.peer_certificate())
            .map(<[u8]>::to_vec);
        let status = response.status();
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|error| self.failed(error))? {
            if body.len() + chunk.len() > MAX_REPLY_BYTES {
                return Err(DecodeError::TooLarge(MAX_REPLY_BYTES).into());
            }
            body.extend_from_slice(&chunk);
        }
        Ok(Reply {
            status,
            challenges,
            encrypted,
            certificate,
            body,
        })
    }

    /// A request that failed, as its certificate's refusal when that is why.
    fn failed(&self, error: reqwest::Error) -> WinrmError {
        let refused = (self.trust.as_ref()).and_then(|trust| trust.take_refused());
        match refused {
            Some(fingerprint) if error.is_connect() => WinrmError::Untrusted(fingerprint),
            _ => WinrmError::connect(&error),
        }
    }

    fn basic(&self) -> Option<&HeaderValue> {
        match &self.method {
            Method::Basic(header) => Some(header),
            Method::Handshake(_) => None,
        }
    }
}

impl Channel {
    /// Post `envelope` and return the envelope answering it with its status: success,
    /// or the server error a fault comes with. A connection the listener no longer
    /// counts as authenticated, as when it closed the one the handshake ran on,
    /// authenticates again once.
    pub(super) async fn post(
        &mut self,
        connector: &Connector,
        envelope: &str,
    ) -> Result<(StatusCode, String), WinrmError> {
        let mut reauthenticate = matches!(connector.method, Method::Handshake(_));
        loop {
            let reply = match &mut self.session {
                Session::Plain => {
                    let body = envelope.as_bytes().to_vec();
                    connector
                        .send(&self.http, body, SOAP, connector.basic())
                        .await?
                }
                Session::Sealed(context) => {
                    let body = context.seal(envelope.as_bytes())?;
                    connector.send(&self.http, body, ENCRYPTED, None).await?
                }
            };
            if reply.status == StatusCode::UNAUTHORIZED {
                if !std::mem::take(&mut reauthenticate) {
                    return Err(reply.refused());
                }
                self.session = connector.authenticate(&self.http).await?;
                continue;
            }
            if !matches!(
                reply.status,
                StatusCode::OK | StatusCode::INTERNAL_SERVER_ERROR
            ) {
                return Err(WinrmError::Status(reply.status.as_u16()));
            }
            let body = match (&mut self.session, reply.encrypted) {
                (Session::Sealed(context), true) => context.unseal(&reply.body)?,
                (Session::Plain, false) => reply.body,
                _ => return Err(WinrmError::Status(reply.status.as_u16())),
            };
            return Ok((reply.status, String::from_utf8_lossy(&body).into_owned()));
        }
    }
}

impl Reply {
    /// The Negotiate token the listener answered with, if any.
    fn token(&self) -> Result<Option<Vec<u8>>, DecodeError> {
        let token = (self.challenges.iter())
            .filter_map(|value| value.to_str().ok())
            .find_map(|value| value.strip_prefix("Negotiate "));
        token
            .map(|token| base64::engine::general_purpose::STANDARD.decode(token.trim()))
            .transpose()
            .map_err(|_| DecodeError::Token)
    }

    /// The refusal of the credentials, naming the schemes the listener offers.
    fn refused(&self) -> WinrmError {
        let offered = (self.challenges.iter())
            .filter_map(|value| value.to_str().ok())
            .filter_map(|value| value.split_whitespace().next())
            .map(str::to_owned)
            .collect();
        WinrmError::Refused { offered }
    }
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use super::super::{
        Tls, connect,
        tests::{Answer, basic, identified, listener},
    };
    use super::*;
    use crate::tests::bounded;

    /// A listener that accepts a connection while its security context still waits
    /// for an answer has authenticated nothing, so no connection is made.
    #[tokio::test]
    async fn a_handshake_still_in_continuation_makes_no_connection() {
        let endpoint = listener(|_| Answer::Now((200, Vec::new(), String::new()))).await;
        let (user, password) = ("DOMAIN\\user".into(), "password".to_owned().into());
        let auth = Auth::Negotiate { user, password };
        let connected = bounded(connect(endpoint, auth, Tls::Off, Default::default())).await;
        assert_matches!(connected.err(), Some(WinrmError::Incomplete));
    }

    /// Plain HTTP connects from a machine without a certificate store.
    #[tokio::test]
    async fn plain_http_needs_no_certificate_store() {
        let store = tempfile::tempdir().unwrap();
        // SAFETY: nextest runs each test in a process of its own, so no other test
        // reads or changes the environment meanwhile.
        unsafe {
            std::env::set_var("SSL_CERT_DIR", store.path());
            std::env::remove_var("SSL_CERT_FILE");
        }
        let endpoint = identified(|_| Answer::Now((200, Vec::new(), String::new()))).await;
        bounded(connect(endpoint, basic(), Tls::Off, Default::default()))
            .await
            .unwrap();
    }

    /// The URL and the service's name derive from one host; text that is no host, or
    /// that a URL would read as naming another, is refused.
    #[tokio::test]
    async fn a_listener_is_one_host_named_once() {
        let connector = async |host: &str| {
            let endpoint = Endpoint {
                host: host.into(),
                port: None,
            };
            let connector = Connector::new(&endpoint, basic(), Tls::Off).await?;
            Ok::<_, WinrmError>((connector.url, connector.spn))
        };
        for (host, url, spn) in [
            (
                "Win.Example",
                "http://win.example:5985/wsman",
                "HTTP/win.example",
            ),
            ("10.0.0.1", "http://10.0.0.1:5985/wsman", "HTTP/10.0.0.1"),
            ("::1", "http://[::1]:5985/wsman", "HTTP/::1"),
            ("[::1]", "http://[::1]:5985/wsman", "HTTP/::1"),
        ] {
            let named = connector(host).await.unwrap();
            assert_eq!(named, (url.to_owned(), spn.to_owned()), "{host}");
        }
        for host in ["user@evil.example", "evil.example:80", "good/x", "a#b", ""] {
            let refused = connector(host).await.unwrap_err();
            assert_matches!(&refused, WinrmError::Host(text) if text == host);
        }
    }

    /// A redirect fails the request rather than sending it to another listener.
    #[tokio::test]
    async fn redirects_are_not_followed() {
        let elsewhere = listener(|_| Answer::Now((200, Vec::new(), String::new()))).await;
        let location = format!("http://127.0.0.1:{}/wsman", elsewhere.port.unwrap());
        let endpoint = listener(move |_| {
            Answer::Now((307, vec![("Location", location.clone())], String::new()))
        })
        .await;
        let connected = bounded(connect(endpoint, basic(), Tls::Off, Default::default())).await;
        assert_matches!(connected.err(), Some(WinrmError::Status(307)));
    }
}
