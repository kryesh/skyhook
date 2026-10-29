//! Credentials a service issues for each request, such as Codex's stored OAuth
//! tokens. The HTTP provider knows no vendor: a dialect enters its
//! authenticator into the request headers under each name it sets.

use std::fmt;

use futures_util::future::BoxFuture;
use reqwest::header::HeaderMap;

use crate::provider::ProviderError;

pub(crate) trait Authenticator: Send + Sync {
    /// The credential headers for one request.
    fn headers(&self) -> BoxFuture<'_, Result<HeaderMap, ProviderError>>;
}

impl fmt::Debug for dyn Authenticator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Authenticator")
    }
}
