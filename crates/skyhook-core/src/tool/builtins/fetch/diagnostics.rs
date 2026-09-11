//! Stable, sanitized failure details shared by local and targeted fetch execution.
//!
//! Error displays are deliberately never copied: HTTP, TLS, resolver and I/O errors
//! can contain URLs, credentials, response data or local paths. Only typed evidence
//! and fixed messages cross the tool boundary. In particular, no failure implies
//! that a firewall or other policy caused it.

use std::{error::Error, fmt, io};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The operation which was in progress, not a guess at a transport substage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum FetchPhase {
    ClientPreparation,
    Request,
    Connect,
    Resolve,
    Tls,
    Proxy,
    Redirect,
    Authorization,
    ResponseBody,
    Decode,
    Extraction,
    LocalIo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum FetchErrorKind {
    ConnectionRefused,
    HostUnreachable,
    NetworkUnreachable,
    ConnectionReset,
    DnsFailure,
    Timeout,
    TlsFailure,
    ProxyFailure,
    RedirectFailure,
    ClientConfiguration,
    ResponseBodyFailure,
    DecodeFailure,
    ExtractionFailure,
    SizeLimit,
    LocalIo,
    Transport,
    Unknown,
}

/// A bounded, version-independent vocabulary rather than `ErrorKind`'s Debug text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum FetchIoKind {
    NotFound,
    PermissionDenied,
    ConnectionRefused,
    ConnectionReset,
    ConnectionAborted,
    NotConnected,
    HostUnreachable,
    NetworkUnreachable,
    NetworkDown,
    AddressInUse,
    AddressNotAvailable,
    BrokenPipe,
    TimedOut,
    UnexpectedEof,
    Interrupted,
    WouldBlock,
    InvalidInput,
    InvalidData,
    WriteZero,
    Unsupported,
    OutOfMemory,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub(super) struct FetchOsError {
    /// OS on the execution target; numeric codes are not portable across OSes.
    pub platform: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<i32>,
    #[schemars(with = "String")]
    pub kind: FetchIoKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(super) enum FetchTimeoutKind {
    Total,
    Connect,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub(super) struct FetchTimeout {
    pub kind: FetchTimeoutKind,
    /// Present only when the caller knows which configured limit expired.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub(super) struct FetchDiagnostic {
    // These are extensible diagnostic labels, not caller-supplied choices.
    // Keep the advertised result schema compact without listing every label.
    #[schemars(with = "String")]
    pub phase: FetchPhase,
    #[schemars(with = "String")]
    pub error_kind: FetchErrorKind,
    /// Safe, fixed text; the human summary may separately add a sanitized origin.
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os_error: Option<FetchOsError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<FetchTimeout>,
}

impl FetchDiagnostic {
    pub fn new(phase: FetchPhase, error_kind: FetchErrorKind) -> Self {
        Self {
            phase,
            error_kind,
            message: message(error_kind, phase).into(),
            os_error: None,
            timeout: (error_kind == FetchErrorKind::Timeout).then_some(FetchTimeout {
                kind: FetchTimeoutKind::Unknown,
                limit_ms: None,
            }),
        }
    }

    pub fn timeout(phase: FetchPhase, kind: FetchTimeoutKind, limit_ms: u64) -> Self {
        Self {
            timeout: Some(FetchTimeout {
                kind,
                limit_ms: Some(limit_ms),
            }),
            ..Self::new(phase, FetchErrorKind::Timeout)
        }
    }

    pub fn from_reqwest(error: &reqwest::Error, phase: FetchPhase) -> Self {
        let evidence = Evidence::collect(error);
        // Reqwest's timeout/connect predicates walk sources without a depth
        // limit. Only call them after verifying that the ordinary source chain
        // terminates within our budget; otherwise retain typed evidence.
        let (timed_out, connecting) = if source_chain_is_bounded(error) {
            (error.is_timeout(), error.is_connect())
        } else {
            (false, false)
        };
        let phase = evidence.refine_phase(phase, connecting);
        // Reqwest's top-level display usually hides the useful connection cause.
        // Typed timeout/decode predicates and source downcasts are sufficient;
        // parsing display strings would both leak data and invent certainty.
        let kind = if timed_out || evidence.timed_out {
            FetchErrorKind::Timeout
        } else if evidence.dns {
            FetchErrorKind::DnsFailure
        } else if evidence.tls {
            FetchErrorKind::TlsFailure
        } else if let Some(kind) = evidence.transport_kind {
            kind
        } else if evidence.proxy {
            FetchErrorKind::ProxyFailure
        } else if error.is_builder() {
            FetchErrorKind::ClientConfiguration
        } else if error.is_redirect() {
            FetchErrorKind::RedirectFailure
        } else if phase == FetchPhase::ResponseBody && evidence.unexpected_eof {
            FetchErrorKind::ResponseBodyFailure
        } else if error.is_decode() {
            FetchErrorKind::DecodeFailure
        } else if error.is_body() || phase == FetchPhase::ResponseBody {
            FetchErrorKind::ResponseBodyFailure
        } else if connecting || error.is_request() {
            FetchErrorKind::Transport
        } else {
            fallback(phase)
        };
        let mut diagnostic = Self::new(phase, kind);
        diagnostic.message = evidence.message(kind, phase).into();
        diagnostic.os_error = evidence.os_error;
        if kind == FetchErrorKind::Timeout && connecting {
            diagnostic.timeout.as_mut().expect("timeout details").kind = FetchTimeoutKind::Connect;
        }
        diagnostic
    }

    pub fn from_io(error: &io::Error, phase: FetchPhase) -> Self {
        let evidence = Evidence::collect(error);
        let phase = evidence.refine_phase(phase, false);
        let kind = if evidence.timed_out {
            FetchErrorKind::Timeout
        } else if evidence.dns {
            FetchErrorKind::DnsFailure
        } else if evidence.tls {
            FetchErrorKind::TlsFailure
        } else {
            evidence.transport_kind.unwrap_or_else(|| {
                if evidence.proxy {
                    FetchErrorKind::ProxyFailure
                } else {
                    fallback(phase)
                }
            })
        };
        Self {
            message: evidence.message(kind, phase).into(),
            os_error: evidence.os_error,
            ..Self::new(phase, kind)
        }
    }
}

pub(super) fn message(kind: FetchErrorKind, phase: FetchPhase) -> &'static str {
    match kind {
        FetchErrorKind::ConnectionRefused => "The connection was refused.",
        FetchErrorKind::HostUnreachable => "The host was unreachable.",
        FetchErrorKind::NetworkUnreachable => "The network was unreachable.",
        FetchErrorKind::ConnectionReset => "The connection was reset.",
        FetchErrorKind::DnsFailure => "DNS resolution failed.",
        FetchErrorKind::Timeout => match phase {
            FetchPhase::ClientPreparation => "HTTP client preparation timed out.",
            FetchPhase::Request => "The HTTP request timed out.",
            FetchPhase::Connect => "Establishing the connection timed out.",
            FetchPhase::Resolve => "DNS resolution timed out.",
            FetchPhase::Tls => "Establishing the TLS connection timed out.",
            FetchPhase::Proxy => "Establishing the proxy connection timed out.",
            FetchPhase::Redirect => "Following the HTTP redirect timed out.",
            FetchPhase::Authorization => "HTTP request authorization timed out.",
            FetchPhase::ResponseBody => "Reading the response body timed out.",
            FetchPhase::Decode => "Decoding the response timed out.",
            FetchPhase::Extraction => "Extracting response text timed out.",
            FetchPhase::LocalIo => "Local file I/O timed out.",
        },
        FetchErrorKind::TlsFailure => "The TLS connection failed.",
        FetchErrorKind::ProxyFailure => "The proxy connection failed.",
        FetchErrorKind::RedirectFailure => "Following the HTTP redirect failed.",
        FetchErrorKind::ClientConfiguration => "Preparing the HTTP client failed.",
        FetchErrorKind::ResponseBodyFailure => "Reading the response body failed.",
        FetchErrorKind::DecodeFailure => "Decoding the response failed.",
        FetchErrorKind::ExtractionFailure => "Extracting response text failed.",
        FetchErrorKind::SizeLimit => "The configured size limit was exceeded.",
        FetchErrorKind::LocalIo => "Local file I/O failed.",
        FetchErrorKind::Transport => "The HTTP transport failed.",
        FetchErrorKind::Unknown => match phase {
            FetchPhase::ClientPreparation => "Preparing the HTTP client failed.",
            _ => "The fetch operation failed.",
        },
    }
}

fn fallback(phase: FetchPhase) -> FetchErrorKind {
    match phase {
        FetchPhase::ClientPreparation => FetchErrorKind::ClientConfiguration,
        FetchPhase::Request | FetchPhase::Connect => FetchErrorKind::Transport,
        FetchPhase::Resolve => FetchErrorKind::DnsFailure,
        FetchPhase::Tls => FetchErrorKind::TlsFailure,
        FetchPhase::Proxy => FetchErrorKind::ProxyFailure,
        FetchPhase::Redirect => FetchErrorKind::RedirectFailure,
        FetchPhase::Authorization => FetchErrorKind::Unknown,
        FetchPhase::ResponseBody => FetchErrorKind::ResponseBodyFailure,
        FetchPhase::Decode => FetchErrorKind::DecodeFailure,
        FetchPhase::Extraction => FetchErrorKind::ExtractionFailure,
        FetchPhase::LocalIo => FetchErrorKind::LocalIo,
    }
}

/// Explicit resolver evidence. Wrap errors only at the actual resolution boundary;
/// reqwest's generic `is_connect()` is not evidence of a DNS failure.
#[derive(Debug)]
pub(super) struct DnsFailure {
    source: Box<dyn Error + Send + Sync>,
}

impl DnsFailure {
    pub fn new(source: impl Error + Send + Sync + 'static) -> Self {
        Self {
            source: Box::new(source),
        }
    }
}

impl fmt::Display for DnsFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DNS resolution failed")
    }
}

impl Error for DnsFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.as_ref())
    }
}

// The tunnel error's defining module is private, but its concrete type is part
// of Tunnel's public Service contract. Name it through that associated type so
// real CONNECT failures remain typed evidence rather than display-string guesses.
type ProxyTunnelError = <hyper_util::client::legacy::connect::proxy::Tunnel<
    hyper_util::client::legacy::connect::HttpConnector,
> as tower_service::Service<http::Uri>>::Error;

#[derive(Default)]
struct Evidence {
    dns: bool,
    proxy: bool,
    proxy_message: Option<&'static str>,
    tls: bool,
    timed_out: bool,
    unexpected_eof: bool,
    transport_kind: Option<FetchErrorKind>,
    os_error: Option<FetchOsError>,
}

const MAX_SOURCE_DEPTH: usize = 32;

fn source_chain_is_bounded(error: &(dyn Error + 'static)) -> bool {
    let mut current = Some(error);
    for _ in 0..MAX_SOURCE_DEPTH {
        let Some(error) = current else { return true };
        current = error.source();
    }
    current.is_none()
}

impl Evidence {
    fn message(&self, kind: FetchErrorKind, phase: FetchPhase) -> &'static str {
        if kind == FetchErrorKind::ResponseBodyFailure && self.unexpected_eof {
            "The response body ended unexpectedly."
        } else if kind == FetchErrorKind::ProxyFailure {
            self.proxy_message.unwrap_or_else(|| message(kind, phase))
        } else {
            message(kind, phase)
        }
    }

    fn refine_phase(&self, phase: FetchPhase, connecting: bool) -> FetchPhase {
        // Never relabel a body/file failure as a connect failure merely because
        // its cause is a reset. Only refine the broad request phase.
        if phase != FetchPhase::Request {
            return phase;
        }
        if self.dns {
            FetchPhase::Resolve
        } else if self.proxy {
            FetchPhase::Proxy
        } else if self.tls {
            FetchPhase::Tls
        } else if connecting {
            FetchPhase::Connect
        } else {
            phase
        }
    }

    fn collect(error: &(dyn Error + 'static)) -> Self {
        // A malicious or buggy source implementation can cycle. Limit the walk
        // independently of chain contents and never recursively format an error.
        let mut evidence = Self::default();
        let mut current = Some(error);
        for _ in 0..MAX_SOURCE_DEPTH {
            let Some(error) = current else { break };
            evidence.dns |= error.is::<DnsFailure>();
            if let Some(error) = error.downcast_ref::<ProxyTunnelError>() {
                evidence.proxy = true;
                evidence.proxy_message = Some(match error {
                    ProxyTunnelError::ProxyAuthRequired => "HTTP proxy authentication required.",
                    ProxyTunnelError::ProxyHeadersTooLong => {
                        "The HTTP proxy response headers exceeded the supported limit."
                    }
                    ProxyTunnelError::TunnelUnexpectedEof => {
                        "The HTTP proxy closed the connection before establishing the tunnel."
                    }
                    ProxyTunnelError::TunnelUnsuccessful => {
                        "The HTTP proxy rejected the CONNECT tunnel."
                    }
                    ProxyTunnelError::MissingHost => {
                        "The HTTP proxy tunnel destination is missing a host."
                    }
                    ProxyTunnelError::ConnectFailed(_) | ProxyTunnelError::Io(_) => {
                        "The proxy connection failed."
                    }
                });
            }
            evidence.tls |= error.is::<rustls::Error>();
            if let Some(error) = error.downcast_ref::<io::Error>() {
                let kind = io_kind(error.kind());
                evidence.timed_out |= kind == FetchIoKind::TimedOut;
                evidence.unexpected_eof |= kind == FetchIoKind::UnexpectedEof;
                if let Some(kind) = transport_kind(kind) {
                    evidence.transport_kind = Some(kind);
                }
                // Prefer an actual OS code over an outer custom I/O wrapper.
                // When several OS errors exist, retain the deepest typed cause.
                if error.raw_os_error().is_some()
                    || evidence
                        .os_error
                        .as_ref()
                        .is_none_or(|error| error.code.is_none())
                {
                    evidence.os_error = Some(FetchOsError {
                        platform: std::env::consts::OS.into(),
                        code: error.raw_os_error(),
                        kind,
                    });
                }
                // io::Error::source can skip the wrapped error itself. Inspect
                // get_ref first so a wrapped rustls::Error/marker is not lost.
                current = error
                    .get_ref()
                    .map(|inner| inner as &(dyn Error + 'static))
                    .or_else(|| error.source());
            } else {
                current = error.source();
            }
        }
        evidence
    }
}

fn transport_kind(kind: FetchIoKind) -> Option<FetchErrorKind> {
    match kind {
        FetchIoKind::ConnectionRefused => Some(FetchErrorKind::ConnectionRefused),
        FetchIoKind::HostUnreachable => Some(FetchErrorKind::HostUnreachable),
        FetchIoKind::NetworkUnreachable => Some(FetchErrorKind::NetworkUnreachable),
        FetchIoKind::ConnectionReset => Some(FetchErrorKind::ConnectionReset),
        _ => None,
    }
}

fn io_kind(kind: io::ErrorKind) -> FetchIoKind {
    match kind {
        io::ErrorKind::NotFound => FetchIoKind::NotFound,
        io::ErrorKind::PermissionDenied => FetchIoKind::PermissionDenied,
        io::ErrorKind::ConnectionRefused => FetchIoKind::ConnectionRefused,
        io::ErrorKind::ConnectionReset => FetchIoKind::ConnectionReset,
        io::ErrorKind::ConnectionAborted => FetchIoKind::ConnectionAborted,
        io::ErrorKind::NotConnected => FetchIoKind::NotConnected,
        io::ErrorKind::HostUnreachable => FetchIoKind::HostUnreachable,
        io::ErrorKind::NetworkUnreachable => FetchIoKind::NetworkUnreachable,
        io::ErrorKind::NetworkDown => FetchIoKind::NetworkDown,
        io::ErrorKind::AddrInUse => FetchIoKind::AddressInUse,
        io::ErrorKind::AddrNotAvailable => FetchIoKind::AddressNotAvailable,
        io::ErrorKind::BrokenPipe => FetchIoKind::BrokenPipe,
        io::ErrorKind::TimedOut => FetchIoKind::TimedOut,
        io::ErrorKind::UnexpectedEof => FetchIoKind::UnexpectedEof,
        io::ErrorKind::Interrupted => FetchIoKind::Interrupted,
        io::ErrorKind::WouldBlock => FetchIoKind::WouldBlock,
        io::ErrorKind::InvalidInput => FetchIoKind::InvalidInput,
        io::ErrorKind::InvalidData => FetchIoKind::InvalidData,
        io::ErrorKind::WriteZero => FetchIoKind::WriteZero,
        io::ErrorKind::Unsupported => FetchIoKind::Unsupported,
        io::ErrorKind::OutOfMemory => FetchIoKind::OutOfMemory,
        _ => FetchIoKind::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_use_stable_wire_values() {
        let diagnostic = serde_json::to_value(FetchDiagnostic::new(
            FetchPhase::Connect,
            FetchErrorKind::ConnectionRefused,
        ))
        .unwrap();
        assert_eq!(diagnostic["phase"], "connect");
        assert_eq!(diagnostic["error_kind"], "connection_refused");
    }
}
