//! Anthropic's error codes, which its envelope names only in `error.type`:
//! `{"type": "error", "error": {"type", "message"}}`.
use serde::Serialize;

use crate::provider::{ProviderErrorKind, codec::errors::ErrorCode};
use skyhook_base::enums::NamedEnum;

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq, NamedEnum)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Code {
    #[serde(rename = "invalid_request_error")]
    InvalidRequest,
    #[serde(rename = "authentication_error")]
    Authentication,
    #[serde(rename = "permission_error")]
    Permission,
    #[serde(rename = "not_found_error")]
    NotFound,
    RequestTooLarge,
    #[serde(rename = "rate_limit_error")]
    RateLimit,
    #[serde(rename = "billing_error")]
    Billing,
    #[serde(rename = "api_error")]
    Api,
    #[serde(rename = "overloaded_error")]
    Overloaded,
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
    use crate::provider::codec::errors::{ErrorSignals, classify, read};
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
            }
        }
    }
}
