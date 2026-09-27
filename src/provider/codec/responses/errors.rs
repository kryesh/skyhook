//! The error codes of the Responses API, over HTTP and in `error`,
//! `response.error` and `response.failed` events. Dialects map their servers' own codes onto kinds
//! with their error rules.
use serde::Serialize;

use crate::{
    named_enum::named_enum,
    provider::{ProviderErrorKind, codec::openai::ErrorCode},
};

named_enum! {
    #[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
    pub(crate) enum Code {
        ContextLengthExceeded = "context_length_exceeded",
        InvalidApiKey = "invalid_api_key",
        InsufficientQuota = "insufficient_quota",
        RateLimitExceeded = "rate_limit_exceeded",
        InvalidRequest = "invalid_request_error",
        ModelNotFound = "model_not_found",
        PreviousResponseNotFound = "previous_response_not_found",
        ServerError = "server_error",
    }
}

impl ErrorCode for Code {
    fn kind(self) -> Option<ProviderErrorKind> {
        Some(match self {
            Self::ContextLengthExceeded => ProviderErrorKind::ContextWindowExceeded,
            Self::InvalidApiKey => ProviderErrorKind::Authentication,
            Self::InsufficientQuota => ProviderErrorKind::Billing,
            Self::RateLimitExceeded => ProviderErrorKind::RateLimited,
            Self::InvalidRequest => ProviderErrorKind::InvalidRequest,
            Self::ModelNotFound | Self::PreviousResponseNotFound | Self::ServerError => {
                return None;
            }
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
    fn error_adapter_preserves_classification_and_sanitized_diagnostics() {
        for (field, identifier, kind) in [
            (
                "type",
                "invalid_request_error",
                ProviderErrorKind::InvalidRequest,
            ),
            ("code", "server_error", ProviderErrorKind::Unavailable),
            (
                "code",
                "unknown_error_SECRET",
                ProviderErrorKind::Unavailable,
            ),
        ] {
            let native = json!({field: identifier, "message": "rejected"});
            let error = classify(
                None,
                &native,
                read::<Code>(&native),
                ErrorSignals::NONE,
                None,
            );
            assert_eq!(error.kind(), kind);
            assert_eq!(
                error.message.contains("[code="),
                !identifier.contains("SECRET")
            );
            assert!(error.message.ends_with(": rejected"));
            assert!(!error.message.contains("SECRET"));
        }
    }
}
