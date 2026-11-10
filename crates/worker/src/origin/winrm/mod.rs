//! A WinRM client: command lines run in a WinRS shell on a Windows host, their
//! standard streams carried by WS-Management requests over HTTP or HTTPS.
//!
//! NTLM and Kerberos authenticate a connection rather than a request, so a client
//! keeps a pool of authenticated connections and each exchange holds one of its own: a
//! Receive waits on one for output while input and signals go over another. Over plain
//! HTTP those schemes also seal every message with the keys their handshake derived.

mod channel;
mod context;
mod error;
mod framing;
mod kerberos;
mod process;
mod route;
mod soap;
#[cfg(unix)]
mod srv;
mod tls;

use std::{
    num::NonZeroU16,
    sync::{Arc, Mutex, PoisonError},
};

use serde::{Deserialize, Serialize};
use skyhook_base::enums::Kinds;

pub use error::{DecodeError, Fault, KdcError, WinrmError};
pub use kerberos::{Kdc, KdcSpellingError, Principal, PrincipalError};
pub(crate) use process::{DELETE_BOUND, Process};
pub use route::{WinrmRoute, open};
pub use tls::{CaError, Fingerprint, FingerprintError, Tls, TlsKind};

use crate::secret::{Credential, ResolveError, Secret};
use channel::{Channel, Connector};
use soap::{Request, Response};

/// Where a WinRM listener is.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Endpoint {
    pub host: String,
    /// 5985 without TLS and 5986 with it when not given.
    pub port: Option<NonZeroU16>,
}

/// How a client authenticates, with `P` its password: configured, or read.
#[derive(Clone, PartialEq, Eq, Deserialize, Serialize, Kinds)]
#[kinds(
    name = AuthKind,
    derive(schemars::JsonSchema),
    serde(rename_all = "snake_case"),
    doc = "How WinRM authenticates: NTLM, the user and password on every request, or a Kerberos ticket"
)]
pub enum Auth<P = Secret> {
    /// NTLM, in HTTP's Negotiate scheme.
    Negotiate { user: String, password: P },
    /// The user and password on every request.
    Basic { user: String, password: P },
    /// A ticket for `HTTP/<host>` from `principal`'s realm, its KDC found in DNS
    /// unless `kdc` names it.
    Kerberos {
        principal: Principal,
        password: P,
        kdc: Option<Kdc>,
    },
}

impl<P> Auth<P> {
    /// The user, as written.
    pub fn user(&self) -> String {
        match self {
            Self::Negotiate { user, .. } | Self::Basic { user, .. } => user.clone(),
            Self::Kerberos { principal, .. } => principal.to_string(),
        }
    }

    pub fn password(&self) -> &P {
        match self {
            Self::Negotiate { password, .. }
            | Self::Basic { password, .. }
            | Self::Kerberos { password, .. } => password,
        }
    }

    /// The KDC a Kerberos ticket comes from, when one is named.
    pub fn kdc(&self) -> Option<&Kdc> {
        match self {
            Self::Kerberos { kdc, .. } => kdc.as_ref(),
            Self::Negotiate { .. } | Self::Basic { .. } => None,
        }
    }

    /// This authentication with `password` in place of its own.
    pub fn with_password<Q>(self, password: Q) -> Auth<Q> {
        match self {
            Self::Negotiate { user, .. } => Auth::Negotiate { user, password },
            Self::Basic { user, .. } => Auth::Basic { user, password },
            Self::Kerberos { principal, kdc, .. } => Auth::Kerberos {
                principal,
                password,
                kdc,
            },
        }
    }
}

impl Auth<Credential> {
    /// This authentication with its password read on this machine, now.
    pub async fn resolve(self) -> Result<Auth, ResolveError> {
        let password = self.password().resolve().await?;
        Ok(self.with_password(password))
    }
}

/// The method and user only: a password, even as configured, may be a secret.
impl<P> std::fmt::Debug for Auth<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("Auth");
        debug
            .field("kind", &self.kind())
            .field("user", &self.user());
        if let Some(kdc) = self.kdc() {
            debug.field("kdc", kdc);
        }
        debug.finish_non_exhaustive()
    }
}

/// Connect to the listener at `endpoint`, authenticating one connection before
/// returning so that a refused certificate or credential fails here. The client's
/// shells, each until it is deleted, and its commands' loops are tasks of `work`.
pub async fn connect(
    endpoint: Endpoint,
    auth: Auth,
    tls: Tls,
    work: skyhook_base::Work,
) -> Result<Client, WinrmError> {
    let connector = Connector::new(&endpoint, auth, tls).await?;
    let channel = connector.open().await?;
    Ok(Client(Arc::new(Shared {
        connector,
        idle: Mutex::new(vec![channel]),
        work,
    })))
}

/// An authenticated client of one listener; clones share its connections.
#[derive(Clone)]
pub struct Client(Arc<Shared>);

struct Shared {
    connector: Connector,
    idle: Mutex<Vec<Channel>>,
    work: skyhook_base::Work,
}

impl Client {
    /// Run `command` through `cmd /c` in a new shell, its standard streams piped.
    pub async fn run(&self, command: &str) -> Result<Process, WinrmError> {
        process::start(self.clone(), command).await
    }

    /// Exchange `request` over an idle connection, or a new one when none is idle. A
    /// connection that failed is dropped rather than returned.
    async fn exchange(&self, request: &Request<'_>) -> Result<Response, WinrmError> {
        let idle = self.lock().pop();
        let mut channel = match idle {
            Some(channel) => channel,
            None => self.0.connector.open().await?,
        };
        let envelope = request.envelope(self.0.connector.url());
        let (status, reply) = channel.post(&self.0.connector, &envelope).await?;
        self.lock().push(channel);
        // The service sends faults, and only faults, with a server error.
        match Response::parse(&reply, request)? {
            Response::Reply(_) if status != reqwest::StatusCode::OK => {
                Err(WinrmError::Status(status.as_u16()))
            }
            response => Ok(response),
        }
    }

    /// Exchange `request`, its fault an error.
    async fn request(&self, request: &Request<'_>) -> Result<soap::Reply, WinrmError> {
        match self.exchange(request).await? {
            Response::Reply(reply) => Ok(reply),
            Response::TimedOut(fault) | Response::Fault(fault) => Err(WinrmError::Fault(fault)),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Channel>> {
        self.0.idle.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
pub(super) mod tests {
    use std::assert_matches;

    use super::*;
    use tokio::io::AsyncWriteExt as _;

    /// A status, headers and a body.
    pub(super) type Reply = (u16, Vec<(&'static str, String)>, String);

    /// A reply, or a request held until its reply is sent: never, once its sender drops.
    pub(super) enum Answer {
        Now(Reply),
        Held(tokio::sync::oneshot::Receiver<Reply>),
    }

    /// A plain HTTP listener on this machine answering each request, given whole,
    /// with `answer`.
    pub(super) async fn listener(
        answer: impl Fn(&str) -> Answer + Send + Sync + 'static,
    ) -> Endpoint {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = NonZeroU16::new(listener.local_addr().unwrap().port());
        let answer = Arc::new(answer);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let answer = answer.clone();
                tokio::spawn(async move {
                    loop {
                        let request = crate::tests::read_request(&mut socket).await;
                        if request.is_empty() {
                            return;
                        }
                        let (status, headers, body) = match answer(&request) {
                            Answer::Now(reply) => reply,
                            Answer::Held(reply) => match reply.await {
                                Ok(reply) => reply,
                                Err(_) => return std::future::pending().await,
                            },
                        };
                        let mut reply = format!("HTTP/1.1 {status} Answer\r\n");
                        for (name, value) in headers {
                            reply += &format!("{name}: {value}\r\n");
                        }
                        reply += &format!("Content-Length: {}\r\n\r\n{body}", body.len());
                        if socket.write_all(reply.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        let host = "127.0.0.1".into();
        Endpoint { host, port }
    }

    pub(super) fn basic() -> Auth {
        let (user, password) = ("user".into(), "password".to_owned().into());
        Auth::Basic { user, password }
    }

    /// A listener that answers a client's first request, Identify, and everything else
    /// with `answer`.
    pub(super) async fn identified(
        answer: impl Fn(&str) -> Answer + Send + Sync + 'static,
    ) -> Endpoint {
        listener(move |request| match request.contains("Identify") {
            true => Answer::Now((200, Vec::new(), String::new())),
            false => answer(request),
        })
        .await
    }

    /// A client of [`identified`]'s listener.
    pub(super) async fn client(answer: impl Fn(&str) -> Answer + Send + Sync + 'static) -> Client {
        let endpoint = identified(answer).await;
        crate::tests::bounded(connect(endpoint, basic(), Tls::Off, Default::default()))
            .await
            .unwrap()
    }

    /// Only success answers a request and only a fault comes with a server error, so
    /// any other status, or a reply that answers no request, fails the exchange.
    #[tokio::test]
    async fn requests_are_answered_only_by_their_status_and_action() {
        let created = soap::tests::answer(&Request::Create.action(), "");
        for (status, body) in [(503, &created), (500, &created), (200, &String::new())] {
            let body = body.clone();
            let client = client(move |_| Answer::Now((status, Vec::new(), body.clone()))).await;
            let requested = crate::tests::bounded(client.request(&Request::Create)).await;
            let error = requested.unwrap_err();
            match status {
                200 => assert_matches!(error, WinrmError::Decode(DecodeError::Unanswered)),
                status => assert_matches!(error, WinrmError::Status(code) if code == status),
            }
        }
    }
}
