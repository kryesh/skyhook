//! The error codes of the Responses API, over HTTP and in `error`,
//! `response.error` and `response.failed` events. Dialects map their servers' own codes onto kinds
//! with their error rules.
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    named_enum::named_enum,
    provider::{
        ProviderError, ProviderErrorKind,
        codec::openai::{self, ErrorCode},
        http::errors::{ErrorSignals, Reading},
    },
};

named_enum! {
    #[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
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
            Self::RateLimitExceeded => ProviderErrorKind::RateLimited { retry_after: None },
            Self::InvalidRequest => ProviderErrorKind::InvalidRequest,
            Self::ModelNotFound | Self::PreviousResponseNotFound | Self::ServerError => {
                return None;
            }
        })
    }
}

/// A Responses error body.
pub(crate) fn read(native: &Value) -> Reading {
    openai::read::<Code>(native)
}

/// The error an `error` event or a failed response carries, read from the
/// envelope around it.
pub(super) fn api_error(native: &Value, signals: ErrorSignals) -> ProviderError {
    let reading = read(native);
    let kind = reading.kind(None, native, signals, None);
    let summary = format!("Responses request failed ({kind})");
    ProviderError {
        kind,
        message: reading.describe(summary, native),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn error_adapter_preserves_classification_and_sanitized_diagnostics() {
        for (field, identifier, kind) in [
            (
                "type",
                "invalid_request_error",
                ProviderErrorKind::InvalidRequest,
            ),
            (
                "code",
                "server_error",
                ProviderErrorKind::Unavailable { retry_after: None },
            ),
            (
                "code",
                "unknown_error_SECRET",
                ProviderErrorKind::Unavailable { retry_after: None },
            ),
        ] {
            let error = api_error(
                &json!({field: identifier, "message": "rejected"}),
                ErrorSignals::NONE,
            );
            assert_eq!(error.kind, kind);
            let prefix = format!("Responses request failed ({kind})");
            assert!(error.message.starts_with(&prefix));
            assert_eq!(
                error.message.contains("[code="),
                !identifier.contains("SECRET")
            );
            assert!(error.message.ends_with(": rejected"));
            assert!(!error.message.contains("SECRET"));
        }
    }
}
