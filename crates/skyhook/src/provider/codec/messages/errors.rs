//! Anthropic's error codes, which its envelope names only in `error.type`:
//! `{"type": "error", "error": {"type", "message"}}`.
use serde::Serialize;

use crate::{
    named_enum::named_enum,
    provider::{ProviderErrorKind, codec::openai::ErrorCode},
};

named_enum! {
    #[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
    pub(crate) enum Code {
        InvalidRequest = "invalid_request_error",
        Authentication = "authentication_error",
        Permission = "permission_error",
        NotFound = "not_found_error",
        RequestTooLarge = "request_too_large",
        RateLimit = "rate_limit_error",
        Billing = "billing_error",
        Api = "api_error",
        Overloaded = "overloaded_error",
    }
}

impl ErrorCode for Code {
    /// The service has no code for an overflow.
    const OVERFLOW_PREFIX: &'static str = "prompt is too long";

    fn kind(self) -> Option<ProviderErrorKind> {
        Some(match self {
            Self::InvalidRequest | Self::NotFound | Self::RequestTooLarge => {
                ProviderErrorKind::InvalidRequest
            }
            Self::Authentication | Self::Permission => ProviderErrorKind::Authentication,
            Self::RateLimit => ProviderErrorKind::RateLimited,
            Self::Billing => ProviderErrorKind::Billing,
            Self::Api | Self::Overloaded => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{
        codec::openai::read,
        http::errors::{ErrorSignals, classify},
    };
    use serde_json::json;

    #[test]
    fn types_name_the_kind_over_http_and_in_the_stream() {
        use ProviderErrorKind::*;
        for (kind_name, kind) in [
            ("authentication_error", Authentication),
            ("billing_error", Billing),
            ("rate_limit_error", RateLimited),
            ("invalid_request_error", InvalidRequest),
        ] {
            let native = json!({"type":"error", "error":{"type":kind_name}});
            for status in [None, Some(400)] {
                let reading = read::<Code>(&native);
                let error = classify(status, &native, reading, ErrorSignals::NONE, None);
                assert_eq!(error.kind(), kind);
                assert!(error.message.contains(&format!("[code={kind_name}]")));
            }
        }
    }
}
