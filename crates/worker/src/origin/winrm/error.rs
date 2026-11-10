//! Why a WinRM exchange failed. Errors cross the protocol from an origin, so a foreign
//! error is kept as the message it and its sources give.

use std::{fmt, io};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::tls::{BindingError, CaError, Fingerprint};

#[derive(Clone, Debug, Error, Deserialize, Serialize)]
pub enum WinrmError {
    #[error("`{0}` is not a host name or IP address")]
    Host(String),
    #[error("WinRM request failed: {0}")]
    Connect(String),
    #[error("the WinRM listener's certificate is not trusted; its SHA-256 fingerprint is {0}")]
    Untrusted(Fingerprint),
    #[error("the WinRM listener's certificate {0}")]
    Binding(#[from] BindingError),
    #[error("CA certificates {0}")]
    Ca(#[from] CaError),
    #[error("the WinRM listener refused the credentials; it offers {}", offered.join(", "))]
    Refused { offered: Vec<String> },
    #[error("authentication failed: {0}")]
    Security(String),
    #[error("the WinRM listener accepted the connection before authentication completed")]
    Incomplete,
    #[error("no KDC found for realm {realm}")]
    NoKdc { realm: String },
    #[error("KDC {url}: {error}")]
    Kdc { url: String, error: KdcError },
    #[error("WinRM answered HTTP {0}")]
    Status(u16),
    #[error("WS-Management fault: {0}")]
    Fault(Fault),
    #[error("malformed WinRM reply: {0}")]
    Decode(#[from] DecodeError),
}

impl From<sspi::Error> for WinrmError {
    fn from(error: sspi::Error) -> Self {
        Self::Security(error.to_string())
    }
}

impl WinrmError {
    pub(super) fn connect(error: &reqwest::Error) -> Self {
        Self::Connect(Chain(error).to_string())
    }
}

/// A WS-Management fault, as the service described it.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct Fault {
    /// The subcode's local name, such as `InvalidSelectors`.
    pub subcode: String,
    /// The Windows error code the service reported, if any.
    pub code: Option<u32>,
    pub reason: String,
}

impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({}", self.reason, self.subcode)?;
        if let Some(code) = self.code {
            write!(f, ", code {code}")?;
        }
        f.write_str(")")
    }
}

/// Why a reply could not be read.
#[derive(Clone, Debug, Error, Deserialize, Serialize)]
pub enum DecodeError {
    #[error("{0}")]
    Xml(String),
    #[error("an attribute is malformed")]
    Attribute,
    #[error("a stream is not base64")]
    Base64,
    #[error("an exit code is not a number")]
    ExitCode,
    #[error("a stream is neither stdout nor stderr")]
    Stream,
    #[error("Create returned no shell")]
    NoShell,
    #[error("Command returned no command")]
    NoCommand,
    #[error("it does not answer the request")]
    Unanswered,
    #[error("an authentication token is not base64")]
    Token,
    #[error("an encrypted message is malformed")]
    Framing,
    #[error("an encrypted message has the wrong length")]
    Length,
    #[error("a reply exceeds {0} bytes")]
    TooLarge(usize),
}

impl From<quick_xml::Error> for DecodeError {
    fn from(error: quick_xml::Error) -> Self {
        Self::Xml(error.to_string())
    }
}

/// Why a KDC exchange failed.
#[derive(Clone, Debug, Error, Deserialize, Serialize)]
pub enum KdcError {
    #[error("{0}")]
    Io(String),
    #[error("did not answer in time")]
    Timeout,
    #[error("is reached over {0}, which is not supported")]
    Protocol(String),
    #[error("answered more than {0} bytes")]
    TooLarge(usize),
}

impl From<io::Error> for KdcError {
    fn from(error: io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

/// An error and its sources, which reqwest keeps out of its own message.
struct Chain<'a>(&'a reqwest::Error);

impl fmt::Display for Chain<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)?;
        let mut source = std::error::Error::source(self.0);
        while let Some(error) = source {
            write!(f, ": {error}")?;
            source = error.source();
        }
        Ok(())
    }
}
