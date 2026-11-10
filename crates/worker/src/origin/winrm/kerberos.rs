//! Kerberos principals, where their realm's KDC is, and the exchanges with it.

use std::{fmt, str::FromStr, time::Duration};

use serde::{Deserialize, Serialize};
use sspi::{NetworkProtocol, NetworkRequest};
use thiserror::Error;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use url::Url;

use super::error::{KdcError, WinrmError};

const KDC_PORT: u16 = 88;
/// How long one KDC exchange may take, as Windows waits for one.
const KDC_TIMEOUT: Duration = Duration::from_secs(10);
/// The most of a KDC reply read; far above any ticket.
const MAX_KDC_REPLY: usize = 1024 * 1024;

/// An account in a Kerberos realm, written `user@REALM` or `DOMAIN\user`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct Principal {
    /// As written, which is how it is shown and stored.
    written: String,
    /// `account@REALM`.
    username: sspi::Username,
    realm: String,
}

/// Why a user name names no Kerberos principal.
#[derive(Clone, Copy, Debug, Error)]
#[error("a Kerberos user is written user@REALM or DOMAIN\\user")]
pub struct PrincipalError;

impl FromStr for Principal {
    type Err = PrincipalError;

    fn from_str(user: &str) -> Result<Self, Self::Err> {
        let (account, realm) = match (user.rsplit_once('@'), user.split_once('\\')) {
            (Some((account, realm)), None) => (account, realm),
            (None, Some((realm, account))) => (account, realm),
            _ => return Err(PrincipalError),
        };
        if account.is_empty() || realm.is_empty() || realm.contains(['@', '\\']) {
            return Err(PrincipalError);
        }
        let realm = realm.to_uppercase();
        let username = sspi::Username::new_upn(account, &realm).map_err(|_| PrincipalError)?;
        let written = user.to_owned();
        Ok(Self {
            written,
            username,
            realm,
        })
    }
}

impl Principal {
    pub(super) fn username(&self) -> sspi::Username {
        self.username.clone()
    }

    /// The realm, uppercased.
    pub fn realm(&self) -> &str {
        &self.realm
    }
}

impl fmt::Display for Principal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.written)
    }
}

impl TryFrom<String> for Principal {
    type Error = PrincipalError;

    fn try_from(user: String) -> Result<Self, Self::Error> {
        user.parse()
    }
}

impl From<Principal> for String {
    fn from(principal: Principal) -> Self {
        principal.written
    }
}

/// A KDC, written `host` or `host:port`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct Kdc {
    written: String,
    url: Url,
}

/// Why text names no KDC.
#[derive(Clone, Copy, Debug, Error)]
#[error("a KDC is written host or host:port")]
pub struct KdcSpellingError;

impl FromStr for Kdc {
    type Err = KdcSpellingError;

    fn from_str(kdc: &str) -> Result<Self, Self::Err> {
        let mut url = Url::parse(&format!("tcp://{kdc}")).map_err(|_| KdcSpellingError)?;
        let bare = url.username().is_empty() && url.query().is_none() && url.fragment().is_none();
        if url.host().is_none() || !bare || !matches!(url.path(), "" | "/") {
            return Err(KdcSpellingError);
        }
        if url.port().is_none() {
            url.set_port(Some(KDC_PORT))
                .map_err(|()| KdcSpellingError)?;
        }
        let written = kdc.to_owned();
        Ok(Self { written, url })
    }
}

impl fmt::Display for Kdc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.written)
    }
}

impl TryFrom<String> for Kdc {
    type Error = KdcSpellingError;

    fn try_from(kdc: String) -> Result<Self, Self::Error> {
        kdc.parse()
    }
}

impl From<Kdc> for String {
    fn from(kdc: Kdc) -> Self {
        kdc.written
    }
}

impl schemars::JsonSchema for Kdc {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Kdc".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({"type": "string", "minLength": 1})
    }
}

/// The KDC of `principal`'s realm: `kdc` when given, else the one this machine's
/// Kerberos configuration or its DNS SRV records name.
pub(super) async fn locate(principal: &Principal, kdc: Option<Kdc>) -> Result<Url, WinrmError> {
    if let Some(kdc) = kdc {
        return Ok(kdc.url);
    }
    let realm = principal.realm.clone();
    // Windows looks the records up itself; elsewhere sspi reads only the configuration.
    let found = skyhook_base::blocking(move || {
        let configured = sspi::detect_kdc_url(&realm);
        #[cfg(unix)]
        let configured = configured.or_else(|| super::srv::kdc(&realm));
        configured
    });
    found.await.ok_or_else(|| WinrmError::NoKdc {
        realm: principal.realm.clone(),
    })
}

/// Send `request` to its KDC and return the reply as sspi expects it: TCP's
/// length-prefixed record, which UDP's datagram is given too.
pub(super) async fn exchange(request: &NetworkRequest) -> Result<Vec<u8>, WinrmError> {
    let failed = |error: KdcError| WinrmError::Kdc {
        url: request.url.to_string(),
        error,
    };
    let exchange = async {
        // By the URL's typed host: an address is used as it is, and only a name resolved.
        let url = request.url.clone();
        let peers = skyhook_base::blocking(move || url.socket_addrs(|| Some(KDC_PORT))).await?;
        match request.protocol {
            NetworkProtocol::Tcp => {
                let mut stream = tokio::net::TcpStream::connect(&*peers).await?;
                stream.write_all(&request.data).await?;
                let length = stream.read_u32().await?;
                let size = usize::try_from(length).unwrap_or(usize::MAX);
                if size > MAX_KDC_REPLY {
                    return Err(KdcError::TooLarge(MAX_KDC_REPLY));
                }
                let mut reply = length.to_be_bytes().to_vec();
                reply.resize(4 + size, 0);
                stream.read_exact(&mut reply[4..]).await?;
                Ok(reply)
            }
            NetworkProtocol::Udp => {
                let peer = *(peers.first())
                    .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))?;
                let local: std::net::IpAddr = match peer {
                    std::net::SocketAddr::V4(_) => std::net::Ipv4Addr::UNSPECIFIED.into(),
                    std::net::SocketAddr::V6(_) => std::net::Ipv6Addr::UNSPECIFIED.into(),
                };
                let socket = tokio::net::UdpSocket::bind((local, 0)).await?;
                socket.connect(peer).await?;
                socket.send(&request.data).await?;
                let mut datagram = vec![0; 65_536];
                let size = socket.recv(&mut datagram).await?;
                let length = u32::try_from(size).expect("a datagram is under 64 KiB");
                Ok([&length.to_be_bytes(), &datagram[..size]].concat())
            }
            protocol => Err(KdcError::Protocol(format!("{protocol:?}"))),
        }
    };
    match tokio::time::timeout(KDC_TIMEOUT, exchange).await {
        Ok(reply) => reply.map_err(failed),
        Err(_) => Err(failed(KdcError::Timeout)),
    }
}

/// This machine's name, the workstation a Kerberos request comes from.
pub(super) fn computer_name() -> String {
    #[cfg(unix)]
    {
        let mut name = [0u8; 256];
        // SAFETY: `name` is writable for its whole length, which is passed with it.
        let result = unsafe { libc::gethostname(name.as_mut_ptr().cast(), name.len()) };
        let end = name
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(name.len());
        match result {
            0 => String::from_utf8_lossy(&name[..end]).into_owned(),
            _ => String::new(),
        }
    }
    #[cfg(windows)]
    {
        std::env::var("COMPUTERNAME").unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Principals take their realm from either spelling, uppercased; KDCs default
    /// their port.
    #[test]
    fn principals_and_kdcs_parse() {
        let principal = |user: &str| {
            let principal = user.parse::<Principal>().ok()?;
            let username = principal.username.account_name().to_owned();
            Some(format!("{username}@{}", principal.realm()))
        };
        assert_eq!(
            principal("alice@corp.example").as_deref(),
            Some("alice@CORP.EXAMPLE")
        );
        assert_eq!(principal("CORP\\alice").as_deref(), Some("alice@CORP"));
        for invalid in ["alice", "@CORP", "alice@", "CORP\\", "a\\b@c"] {
            assert_eq!(principal(invalid), None, "{invalid}");
        }
        let kdc = |kdc: &str| kdc.parse::<Kdc>().map(|kdc| kdc.url.to_string()).ok();
        assert_eq!(kdc("dc.corp").as_deref(), Some("tcp://dc.corp:88"));
        assert_eq!(kdc("10.0.0.1:750").as_deref(), Some("tcp://10.0.0.1:750"));
        assert_eq!(kdc("[::1]").as_deref(), Some("tcp://[::1]:88"));
        assert_eq!(kdc("dc.corp/path"), None);
    }

    /// A KDC named by an IPv6 address is reached over TCP and UDP.
    #[tokio::test]
    async fn kdcs_are_reached_at_ipv6_addresses() {
        let tcp = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
        let udp = tokio::net::UdpSocket::bind("[::1]:0").await.unwrap();
        let (tcp_port, udp_port) = (
            tcp.local_addr().unwrap().port(),
            udp.local_addr().unwrap().port(),
        );
        let serve = tokio::spawn(async move {
            let (mut stream, _) = tcp.accept().await.unwrap();
            let mut request = [0; 4];
            stream.read_exact(&mut request).await.unwrap();
            stream.write_all(&[0, 0, 0, 2, b'o', b'k']).await.unwrap();
            let mut datagram = [0; 4];
            let (_, peer) = udp.recv_from(&mut datagram).await.unwrap();
            udp.send_to(b"ok", peer).await.unwrap();
        });
        for (protocol, port) in [
            (NetworkProtocol::Tcp, tcp_port),
            (NetworkProtocol::Udp, udp_port),
        ] {
            let request = NetworkRequest {
                protocol,
                url: Url::parse(&format!("tcp://[::1]:{port}")).unwrap(),
                data: b"asks".to_vec(),
            };
            let exchanged = crate::tests::bounded(exchange(&request)).await;
            assert_eq!(exchanged.unwrap(), [0, 0, 0, 2, b'o', b'k']);
        }
        crate::tests::bounded(serve).await.unwrap();
    }
}
