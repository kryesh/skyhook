//! The common credential: one header carrying a configured key.

use reqwest::header::{AUTHORIZATION, HeaderName};

use super::{
    ValueField,
    config::{Pending, Source},
};
use crate::provider::{codec::CodecName, http::Headers};

/// How a key travels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Scheme {
    /// `authorization: Bearer <key>`.
    Bearer,
    /// `x-api-key: <key>`.
    XApiKey,
}

impl Scheme {
    /// The family's own convention: Messages takes `x-api-key`, the OpenAI
    /// families a bearer token.
    pub(crate) fn standard(codec: CodecName) -> Self {
        match codec {
            CodecName::Messages => Self::XApiKey,
            CodecName::ChatCompletions | CodecName::Responses => Self::Bearer,
        }
    }

    /// The configured `api_key` in the header this scheme sends it in.
    pub(crate) fn credential(self, key: &Source) -> Headers<Pending> {
        let (header, prefix) = match self {
            Self::Bearer => (AUTHORIZATION, Some("Bearer ")),
            Self::XApiKey => (HeaderName::from_static("x-api-key"), None),
        };
        let mut headers = Headers::default();
        headers.insert(header, key.header(ValueField::ApiKey, prefix));
        headers
    }
}
