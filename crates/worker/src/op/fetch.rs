//! One HTTP exchange from a target, on its network: its resolver, proxy environment
//! and trusted roots. Redirects, credential isolation and limits run inside it.
use std::{collections::BTreeMap, time::Duration};

use reqwest::{
    Method, Url,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{Input, TargetPath};
use crate::diagnostic::Fault;
use crate::fs::spill::SpillName;

/// The exchange, its response body going where `O` says: a [`FetchOutput`] once it
/// runs, and before then whatever its tool admitted.
#[derive(Debug, Deserialize, Serialize)]
pub struct Fetch<O = FetchOutput> {
    pub url: HttpRequestUrl,
    pub method: HttpMethod,
    pub headers: Headers,
    pub body: Option<FetchBody>,
    pub client: ClientIdentity,
    /// The whole exchange's deadline, redirects and body included, when it has one.
    pub timeout: Option<Duration>,
    /// Decoded response bytes past which the fetch fails, when it has a limit.
    pub max_bytes: Option<u64>,
    pub max_redirects: u64,
    pub redirects: RedirectPolicy,
    pub include_headers: bool,
    pub output: O,
}

/// What a fetch reports as it runs, so its consumer knows where it stands
/// whenever it stops.
#[derive(Debug, Deserialize, Serialize)]
pub enum FetchEvent {
    /// The fetch reached `step`; once one sends, the request may have executed.
    Step(FetchStep),
    /// A download created missing directories to hold its destination, which
    /// remain whatever follows.
    CreatedParents,
    /// Bytes of a response body that [`FetchOutput::Return`] returns, sent once the
    /// body ended within its `inline` bytes.
    Body(#[serde(with = "serde_bytes")] Vec<u8>),
}

#[derive(Debug, Deserialize, Serialize)]
pub enum FetchBody {
    /// `length` bytes the host streams, sent with `content_type` unless the
    /// request sets its own.
    Bytes {
        content_type: Option<ContentType>,
        length: u64,
        #[serde(with = "super::input")]
        input: Input,
    },
    /// A regular file on the target, streamed from its start by each hop that sends
    /// it, as it is then.
    File { path: TargetPath },
}

/// The media type an encoded body declares.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum ContentType {
    Text,
    Json,
    Form,
}

impl ContentType {
    pub(crate) const fn as_header(self) -> &'static str {
        match self {
            Self::Text => "text/plain; charset=utf-8",
            Self::Json => "application/json",
            Self::Form => "application/x-www-form-urlencoded",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum FetchOutput {
    /// Send back a body of at most `inline` bytes; a longer one is kept whole as
    /// `spill` in the target's spill directory.
    Return { inline: u64, spill: SpillName },
    /// Commit the body to `path` atomically, replacing a regular file there only
    /// with `overwrite`.
    Save { path: TargetPath, overwrite: bool },
}

/// The settings an HTTP client is built from, which the calls sharing it have in common.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ClientIdentity {
    pub connect_timeout: Duration,
    pub proxy: Option<String>,
    pub insecure: bool,
}

#[derive(Debug, Default, Clone, Copy, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RedirectPolicy {
    #[default]
    Safe,
    Follow,
    Manual,
}

impl RedirectPolicy {
    /// Whether a request with `method` follows a redirect: safe redirects follow
    /// only GET and HEAD.
    pub(crate) fn follows(self, method: &Method) -> bool {
        match self {
            Self::Safe => method == Method::GET || method == Method::HEAD,
            Self::Follow => true,
            Self::Manual => false,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Fetched {
    pub status: u16,
    pub headers: Option<BTreeMap<String, Vec<String>>>,
    pub redirects: Vec<Redirect>,
    /// The final response's content type.
    pub content_type: Option<String>,
    /// Where the final response came from.
    pub url: HttpRequestUrl,
    /// Decoded entity bytes received.
    pub size: u64,
    /// The file a returned body longer than its `inline` bytes was kept in.
    pub spilled: Option<crate::fs::spill::SpillFile>,
    /// Milliseconds.
    pub duration: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct Redirect {
    pub status: u16,
    pub location: String,
    pub method: String,
}

/// What a fetch was doing.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum FetchStep {
    Client,
    Body,
    ReadUpload(TargetPath),
    /// Sending `method` to `origin`.
    Send {
        method: String,
        origin: String,
    },
    Redirect,
    Receive,
    /// A download's staging, which never touches its destination.
    Download {
        step: DownloadStep,
        path: TargetPath,
    },
    /// Committing a download onto its destination.
    Commit(TargetPath),
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub enum DownloadStep {
    Inspect,
    Stage,
    Receive,
    Write,
    Sync,
}

/// A request endpoint, not a proxy endpoint: HTTP(S) with a host and no credentials.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct HttpRequestUrl(Url);

/// Why a URL is no request endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum UrlError {
    #[error("invalid absolute URL")]
    Invalid,
    #[error("URL must use HTTP or HTTPS and have a host")]
    Scheme,
    #[error("embedded URL credentials are not supported; use auth")]
    Credentials,
}

impl HttpRequestUrl {
    pub fn parse(value: &str) -> Result<Self, UrlError> {
        Self::admit(Url::parse(value).map_err(|_| UrlError::Invalid)?)
    }

    fn admit(mut url: Url) -> Result<Self, UrlError> {
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            return Err(UrlError::Scheme);
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(UrlError::Credentials);
        }
        url.set_fragment(None);
        Ok(Self(url))
    }

    /// `location` resolved against this URL, as a redirect names it.
    pub fn join(&self, location: &str) -> Result<Self, Fault> {
        let url = (self.0.join(location)).map_err(|_| Fault::InvalidRedirectUrl)?;
        Self::admit(url).map_err(|_| Fault::UnsupportedRedirectUrl)
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub fn url(&self) -> &Url {
        &self.0
    }

    /// The origin, as permissions name it.
    pub fn origin(&self) -> String {
        self.0.origin().ascii_serialization()
    }

    pub fn append_query(&mut self, query: &[(String, String)]) {
        if !query.is_empty() {
            (self.0.query_pairs_mut()).extend_pairs(query.iter().map(|(k, v)| (k, v)));
        }
    }
}

impl TryFrom<String> for HttpRequestUrl {
    type Error = UrlError;

    fn try_from(value: String) -> Result<Self, UrlError> {
        Self::parse(&value)
    }
}

impl From<HttpRequestUrl> for String {
    fn from(url: HttpRequestUrl) -> Self {
        url.0.into()
    }
}

/// A request method, a token by construction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct HttpMethod(pub Method);

impl TryFrom<String> for HttpMethod {
    type Error = http::method::InvalidMethod;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Method::from_bytes(value.as_bytes()).map(Self)
    }
}

impl From<HttpMethod> for String {
    fn from(method: HttpMethod) -> Self {
        method.0.to_string()
    }
}

/// Request headers, each name and value valid by construction; a sensitive value
/// stays marked as one.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<WireHeader>", into = "Vec<WireHeader>")]
pub struct Headers(pub HeaderMap);

#[derive(Serialize, Deserialize)]
struct WireHeader {
    name: String,
    #[serde(with = "serde_bytes")]
    value: Vec<u8>,
    sensitive: bool,
}

/// A header name or value HTTP cannot carry.
#[derive(Debug, thiserror::Error)]
#[error("invalid HTTP header")]
pub struct InvalidHeader;

impl TryFrom<Vec<WireHeader>> for Headers {
    type Error = InvalidHeader;

    fn try_from(headers: Vec<WireHeader>) -> Result<Self, InvalidHeader> {
        let mut map = HeaderMap::new();
        for header in headers {
            let name = HeaderName::from_bytes(header.name.as_bytes()).map_err(|_| InvalidHeader)?;
            let mut value = HeaderValue::from_bytes(&header.value).map_err(|_| InvalidHeader)?;
            value.set_sensitive(header.sensitive);
            map.append(name, value);
        }
        Ok(Self(map))
    }
}

impl From<Headers> for Vec<WireHeader> {
    fn from(headers: Headers) -> Self {
        (headers.0.iter())
            .map(|(name, value)| WireHeader {
                name: name.as_str().to_owned(),
                value: value.as_bytes().to_vec(),
                sensitive: value.is_sensitive(),
            })
            .collect()
    }
}
