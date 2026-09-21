//! Stable, sanitized failure details shared by local and targeted fetch execution.
//!
//! Error displays are deliberately never copied: HTTP, TLS, resolver and I/O errors
//! can contain URLs, credentials, response data or local paths. Only typed evidence
//! and fixed messages cross the tool boundary. In particular, no failure implies
//! that a firewall or other policy caused it.

use std::{error::Error, fmt, io};

use schemars::JsonSchema;
use serde::Serialize;

use crate::tool::diagnostic::{Cause, IoKind};

use super::LocalError;

/// The operation which was in progress, not a guess at a transport substage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub(super) struct FetchOsError {
    /// OS on the execution target; numeric codes are not portable across OSes.
    platform: String,
    code: Option<i32>,
    #[schemars(with = "String")]
    kind: IoKind,
}

/// Safe text is a closed vocabulary, never a caller-provided string or an error display.
#[derive(Debug, Clone, Copy)]
pub(super) enum DiagnosticMessage {
    Standard,
    ResponseExceedsMaxBytes,
    TextExtractionUnsupported,
    Extraction(super::fetch_text::ExtractionFailure),
    MaximumRedirectsExceeded,
    InvalidRedirectLocationHeader,
    InvalidRedirectUrl,
    RedirectUrlNotHttp,
    HttpsDowngradeBlocked,
    UnexpectedBodyEof,
    ProxyAuthRequired,
    ProxyHeadersTooLong,
    ProxyUnexpectedEof,
    ProxyRejectedTunnel,
    ProxyMissingHost,
}

impl DiagnosticMessage {
    fn render(self, kind: FetchErrorKind, phase: FetchPhase) -> &'static str {
        match self {
            Self::Standard => message(kind, phase),
            Self::ResponseExceedsMaxBytes => "response exceeds max_bytes",
            Self::TextExtractionUnsupported => {
                "text extraction is unsupported for this binary content type"
            }
            Self::Extraction(reason) => {
                use super::fetch_text::ExtractionFailure;
                match reason {
                    ExtractionFailure::RawSizeLimit => {
                        "HTML exceeds the 10 MiB extraction input limit; use text:false to retrieve the response without extraction."
                    }
                    ExtractionFailure::DecodedSizeLimit => {
                        "Decoded HTML exceeds the 10 MiB extraction limit; use text:false to retrieve the response without extraction."
                    }
                    ExtractionFailure::ElementLimit => {
                        "HTML exceeds the 50,000-element extraction limit; use text:false to retrieve the response without extraction."
                    }
                    ExtractionFailure::Parser => {
                        "The HTML parser could not extract an article; use text:false to retrieve the original response."
                    }
                    ExtractionFailure::Empty => {
                        "HTML extraction produced no readable text; use text:false to retrieve the original response."
                    }
                    ExtractionFailure::WorkerUnavailable => {
                        "The HTML extraction worker is unavailable; retry or use text:false to retrieve the response without extraction."
                    }
                    ExtractionFailure::WorkerFailed => {
                        "The HTML extraction worker did not complete; retry or use text:false to retrieve the response without extraction."
                    }
                }
            }
            Self::MaximumRedirectsExceeded => "maximum redirects exceeded",
            Self::InvalidRedirectLocationHeader => "invalid redirect location header",
            Self::InvalidRedirectUrl => "invalid redirect URL",
            Self::RedirectUrlNotHttp => "redirect URL must be HTTP(S) without embedded credentials",
            Self::HttpsDowngradeBlocked => "HTTPS to HTTP redirect blocked",
            Self::UnexpectedBodyEof => "The response body ended unexpectedly.",
            Self::ProxyAuthRequired => "HTTP proxy authentication required.",
            Self::ProxyHeadersTooLong => {
                "The HTTP proxy response headers exceeded the supported limit."
            }
            Self::ProxyUnexpectedEof => {
                "The HTTP proxy closed the connection before establishing the tunnel."
            }
            Self::ProxyRejectedTunnel => "The HTTP proxy rejected the CONNECT tunnel.",
            Self::ProxyMissingHost => "The HTTP proxy tunnel destination is missing a host.",
        }
    }
}

/// Unknown timeout evidence cannot accidentally acquire a configured limit.
#[derive(Debug, Clone, Copy, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum TimeoutAttribution {
    Unknown,
    Total { limit_ms: u64 },
    Connect { limit_ms: Option<u64> },
}

/// Internal evidence is not deserializable: only the typed constructors below
/// populate it (rendering the closed message once), and serialization is the
/// single wire projection.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub(super) struct FetchDiagnostic {
    #[schemars(with = "String")]
    phase: FetchPhase,
    #[schemars(with = "String")]
    error_kind: FetchErrorKind,
    message: &'static str,
    os_error: Option<FetchOsError>,
    timeout: Option<TimeoutAttribution>,
}

impl FetchDiagnostic {
    pub fn new(phase: FetchPhase, error_kind: FetchErrorKind) -> Self {
        Self::classified(phase, error_kind, DiagnosticMessage::Standard)
    }

    pub fn classified(phase: FetchPhase, kind: FetchErrorKind, message: DiagnosticMessage) -> Self {
        Self {
            phase,
            error_kind: kind,
            message: message.render(kind, phase),
            os_error: None,
            timeout: (kind == FetchErrorKind::Timeout).then_some(TimeoutAttribution::Unknown),
        }
    }

    pub fn total_timeout(phase: FetchPhase, limit_ms: u64) -> Self {
        Self {
            timeout: Some(TimeoutAttribution::Total { limit_ms }),
            ..Self::new(phase, FetchErrorKind::Timeout)
        }
    }

    pub fn phase(&self) -> FetchPhase {
        self.phase
    }

    pub fn message(&self) -> &'static str {
        self.message
    }

    pub fn os_code(&self) -> Option<(&str, i32)> {
        self.os_error
            .as_ref()
            .and_then(|os| os.code.map(|code| (os.platform.as_str(), code)))
    }

    pub fn with_connect_limit(mut self, limit_ms: u64) -> Self {
        if let Some(TimeoutAttribution::Connect {
            limit_ms: limit @ None,
        }) = &mut self.timeout
        {
            *limit = Some(limit_ms);
        }
        self
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
        } else if evidence.proxy.is_some() {
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
        let mut diagnostic = evidence.into_diagnostic(phase, kind);
        if kind == FetchErrorKind::Timeout && connecting {
            diagnostic.timeout = Some(TimeoutAttribution::Connect { limit_ms: None });
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
        } else if let Some(kind) = evidence.transport_kind {
            kind
        } else if evidence.proxy.is_some() {
            FetchErrorKind::ProxyFailure
        } else {
            fallback(phase)
        };
        evidence.into_diagnostic(phase, kind)
    }
}

/// Fetch-local errors preserve typed diagnostics until progress performs the single
/// tool-output projection. Generic errors are admitted only at named local-I/O,
/// validation and permission boundaries; arbitrary output and displays are not evidence.
#[derive(Debug)]
pub(super) enum FetchError {
    Diagnostic(FetchDiagnostic),
    /// Only control-flow/admission metadata crosses unchanged. A generic failed
    /// error, arbitrary ProducedOutput, or secret IO display is never retained.
    Passthrough(LocalError),
}

impl From<FetchDiagnostic> for FetchError {
    fn from(diagnostic: FetchDiagnostic) -> Self {
        Self::Diagnostic(diagnostic)
    }
}

impl FetchError {
    /// Admit only typed evidence at permission/validation boundaries. Existing
    /// generic payloads and error displays are never accepted as fetch evidence.
    pub fn from_tool_error(error: LocalError, phase: FetchPhase) -> Self {
        // Inspect native sources before normalization discards the wrapped evidence.
        if let LocalError::Io(error) = error {
            return FetchDiagnostic::from_io(&error, phase).into();
        }
        // General tool output is not authoritative HTTP response evidence.
        let (diagnostic, _) = error.into_parts();
        match diagnostic.cause {
            Cause::Io { kind, code, .. } => {
                let error_kind = if kind == IoKind::TimedOut {
                    FetchErrorKind::Timeout
                } else {
                    transport_kind(kind).unwrap_or_else(|| fallback(phase))
                };
                FetchDiagnostic {
                    os_error: Some(FetchOsError {
                        platform: std::env::consts::OS.into(),
                        code,
                        kind,
                    }),
                    ..FetchDiagnostic::new(phase, error_kind)
                }
                .into()
            }
            Cause::Message(_) | Cause::Json => {
                Self::Diagnostic(FetchDiagnostic::new(phase, fallback(phase)))
            }
            Cause::Cancelled
            | Cause::Interrupted
            | Cause::Denied(_)
            | Cause::InvalidArguments(_)
            | Cause::InputClosed => {
                Self::Passthrough(LocalError::from_diagnostic(diagnostic, None))
            }
        }
    }

    pub fn into_diagnostic(self) -> Result<FetchDiagnostic, LocalError> {
        match self {
            Self::Diagnostic(diagnostic) => Ok(diagnostic),
            Self::Passthrough(error) => Err(error),
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

/// Proxy evidence carries its closed message. This is independent of TLS/DNS/IO
/// evidence: a single source chain can legitimately contain several.
#[derive(Default)]
struct Evidence {
    dns: bool,
    proxy: Option<DiagnosticMessage>,
    tls: bool,
    timed_out: bool,
    unexpected_eof: bool,
    /// Only observed IO connection causes refine transport classification.
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
    fn into_diagnostic(self, phase: FetchPhase, kind: FetchErrorKind) -> FetchDiagnostic {
        let message = if kind == FetchErrorKind::ResponseBodyFailure && self.unexpected_eof {
            DiagnosticMessage::UnexpectedBodyEof
        } else if kind == FetchErrorKind::ProxyFailure {
            self.proxy.unwrap_or(DiagnosticMessage::Standard)
        } else {
            DiagnosticMessage::Standard
        };
        FetchDiagnostic {
            os_error: self.os_error,
            ..FetchDiagnostic::classified(phase, kind, message)
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
        } else if self.proxy.is_some() {
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
                evidence.proxy = Some(match error {
                    ProxyTunnelError::ProxyAuthRequired => DiagnosticMessage::ProxyAuthRequired,
                    ProxyTunnelError::ProxyHeadersTooLong => DiagnosticMessage::ProxyHeadersTooLong,
                    ProxyTunnelError::TunnelUnexpectedEof => DiagnosticMessage::ProxyUnexpectedEof,
                    ProxyTunnelError::TunnelUnsuccessful => DiagnosticMessage::ProxyRejectedTunnel,
                    ProxyTunnelError::MissingHost => DiagnosticMessage::ProxyMissingHost,
                    ProxyTunnelError::ConnectFailed(_) | ProxyTunnelError::Io(_) => {
                        DiagnosticMessage::Standard
                    }
                });
            }
            evidence.tls |= error.is::<rustls::Error>();
            if let Some(error) = error.downcast_ref::<io::Error>() {
                let kind = IoKind::from(error.kind());
                evidence.timed_out |= kind == IoKind::TimedOut;
                evidence.unexpected_eof |= kind == IoKind::UnexpectedEof;
                if let Some(cause) = transport_kind(kind) {
                    evidence.transport_kind = Some(cause);
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

fn transport_kind(kind: IoKind) -> Option<FetchErrorKind> {
    match kind {
        IoKind::ConnectionRefused => Some(FetchErrorKind::ConnectionRefused),
        IoKind::HostUnreachable => Some(FetchErrorKind::HostUnreachable),
        IoKind::NetworkUnreachable => Some(FetchErrorKind::NetworkUnreachable),
        IoKind::ConnectionReset => Some(FetchErrorKind::ConnectionReset),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalized_io_classifies_by_typed_kind_and_keeps_the_os_code() {
        use crate::tool::diagnostic::{Diagnostic, DiagnosticContext};

        for (kind, expected) in [
            (IoKind::TimedOut, FetchErrorKind::Timeout),
            (IoKind::ConnectionReset, FetchErrorKind::ConnectionReset),
            (IoKind::InvalidInput, FetchErrorKind::ResponseBodyFailure),
        ] {
            let cause = Cause::Io {
                kind,
                code: Some(12345),
                detail: None,
            };
            let error = LocalError::from_diagnostic(
                Diagnostic::new(DiagnosticContext::default(), cause),
                None,
            );
            let admitted = FetchError::from_tool_error(error, FetchPhase::ResponseBody)
                .into_diagnostic()
                .unwrap();
            assert_eq!(admitted.error_kind, expected);
            assert_eq!(admitted.os_error.as_ref().unwrap().kind, kind);
            assert_eq!(admitted.os_code(), Some((std::env::consts::OS, 12345)));
        }
    }

    #[test]
    fn timeout_attribution_is_coherent_and_enrichment_is_evidence_limited() {
        let unknown = FetchDiagnostic::new(FetchPhase::ResponseBody, FetchErrorKind::Timeout);
        let total = FetchDiagnostic::total_timeout(FetchPhase::ResponseBody, 5000);
        let connect = FetchDiagnostic {
            timeout: Some(TimeoutAttribution::Connect { limit_ms: None }),
            ..FetchDiagnostic::new(FetchPhase::Connect, FetchErrorKind::Timeout)
        };
        for (diagnostic, expected) in [
            (unknown, json!({"kind":"unknown"})),
            (total, json!({"kind":"total", "limit_ms":5000})),
            (connect, json!({"kind":"connect", "limit_ms":1234})),
        ] {
            let value = serde_json::to_value(diagnostic.with_connect_limit(1234)).unwrap();
            assert_eq!(
                (&value["error_kind"], &value["timeout"]),
                (&json!("timeout"), &expected)
            );
        }
        // Stable wire values; non-timeouts carry no timeout attribution.
        for (phase, kind, wire) in [
            (
                FetchPhase::Connect,
                FetchErrorKind::ConnectionRefused,
                ["connect", "connection_refused"],
            ),
            (
                FetchPhase::ResponseBody,
                FetchErrorKind::ConnectionReset,
                ["response_body", "connection_reset"],
            ),
        ] {
            let value =
                serde_json::to_value(FetchDiagnostic::new(phase, kind).with_connect_limit(1234))
                    .unwrap();
            assert_eq!(value["timeout"], serde_json::Value::Null);
            assert_eq!(value["os_error"], serde_json::Value::Null);
            assert_eq!(
                [&value["phase"], &value["error_kind"]],
                wire.map(|w| json!(w)).each_ref()
            );
        }
    }

    #[test]
    fn nullable_diagnostic_details_are_emitted() {
        let os_error = FetchOsError {
            platform: "test".into(),
            code: None,
            kind: IoKind::Other,
        };
        assert_eq!(serde_json::to_value(os_error).unwrap()["code"], json!(null));
        let connect = TimeoutAttribution::Connect { limit_ms: None };
        assert_eq!(
            serde_json::to_value(connect).unwrap(),
            json!({"kind":"connect", "limit_ms":null})
        );
    }

    #[test]
    fn wrapped_tls_dns_and_os_evidence_keep_precedence_and_deep_code() {
        use FetchErrorKind as Kind;
        use FetchPhase as Phase;
        let tls = io::Error::other(rustls::Error::General("secret TLS detail".into()));
        let reset = io::Error::new(
            io::ErrorKind::ConnectionReset,
            "https://secret:password@host/path?token=secret",
        );
        let proxy = io::Error::new(io::ErrorKind::TimedOut, ProxyTunnelError::ProxyAuthRequired);
        for (error, phase, expected_phase, kind, message) in [
            (
                tls,
                Phase::Request,
                Phase::Tls,
                Kind::TlsFailure,
                "The TLS connection failed.",
            ),
            (
                reset,
                Phase::ResponseBody,
                Phase::ResponseBody,
                Kind::ConnectionReset,
                "The connection was reset.",
            ),
            (
                proxy,
                Phase::Request,
                Phase::Proxy,
                Kind::Timeout,
                "Establishing the proxy connection timed out.",
            ),
        ] {
            for diagnostic in [
                FetchDiagnostic::from_io(&error, phase),
                FetchError::from_tool_error(LocalError::Io(error), phase)
                    .into_diagnostic()
                    .unwrap(),
            ] {
                assert_eq!(
                    (diagnostic.phase, diagnostic.error_kind),
                    (expected_phase, kind)
                );
                assert_eq!(diagnostic.message(), message);
                let value = serde_json::to_value(diagnostic).unwrap();
                assert!(!value.to_string().contains("secret"));
                let timeout = if kind == Kind::Timeout {
                    json!({"kind":"unknown"})
                } else {
                    serde_json::Value::Null
                };
                assert_eq!(value["timeout"], timeout);
            }
        }
        for (code, kind) in [
            (12345, Kind::LocalIo),
            #[cfg(unix)]
            (libc::ECONNRESET, Kind::ConnectionReset),
        ] {
            let os = io::Error::other(io::Error::from_raw_os_error(code));
            for diagnostic in [
                FetchDiagnostic::from_io(&os, Phase::LocalIo),
                FetchError::from_tool_error(LocalError::Io(os), Phase::LocalIo)
                    .into_diagnostic()
                    .unwrap(),
            ] {
                assert_eq!(
                    (diagnostic.phase, diagnostic.error_kind),
                    (Phase::LocalIo, kind)
                );
                assert_eq!(diagnostic.os_code(), Some((std::env::consts::OS, code)));
                if kind == Kind::ConnectionReset {
                    assert_eq!(
                        diagnostic.os_error.as_ref().unwrap().kind,
                        IoKind::ConnectionReset
                    );
                }
            }
        }
    }
}
