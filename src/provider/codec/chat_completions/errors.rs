//! The error codes of the Chat Completions API. Dialects map their servers'
//! own codes onto kinds with their error rules.
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    named_enum::named_enum,
    provider::{
        ProviderErrorKind,
        codec::openai::{self, ErrorCode},
        http::errors::Reading,
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
            Self::ModelNotFound | Self::ServerError => return None,
        })
    }
}

/// A Chat error body, HTTP or in-stream.
pub(crate) fn read(native: &Value) -> Reading {
    openai::read::<Code>(native)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::http::errors::{ErrorSignals, classify};
    use serde_json::json;

    fn error(status: Option<u16>, native: &Value) -> crate::provider::ProviderError {
        classify(status, native, read(native), ErrorSignals::NONE, None)
    }

    /// The specific code names the kind before the type category, over HTTP or
    /// in the stream, wrapped or not; an unknown code is neither read nor echoed.
    #[test]
    fn codes_and_overflow_wording_classify_http_and_stream_errors() {
        use ProviderErrorKind::*;
        for (native, kind) in [
            (
                json!({"error":{"code":"invalid_api_key","type":"invalid_request_error"}}),
                Authentication,
            ),
            (
                json!({"error":{"code":"unknown","type":"context_length_exceeded"}}),
                ContextWindowExceeded,
            ),
            (json!({"code":"insufficient_quota"}), Billing),
            (json!({"type":"invalid_request_error"}), InvalidRequest),
            (
                json!({"error":{"message":"This model's maximum context length is 8192 tokens"}}),
                ContextWindowExceeded,
            ),
        ] {
            for status in [None, Some(400)] {
                let error = error(status, &native);
                assert_eq!(error.kind, kind, "{native}");
                assert!(!error.message.contains("[code=unknown"));
            }
        }
        for (status, native, expected) in [
            (
                Some(503),
                json!({"error": {"code":"server_error", "message":"upstream\n\tunavailable"}}),
                "provider HTTP 503 error [code=server_error]: upstream unavailable",
            ),
            (
                Some(429),
                json!({"error": {"code":"sk-private-credential"}}),
                "provider HTTP 429 error",
            ),
            // A status in the body stands in for the missing HTTP one.
            (
                None,
                json!({"error": {"code":429, "message":"slow down"}}),
                "provider HTTP 429 error: slow down",
            ),
        ] {
            assert_eq!(error(status, &native).message, expected);
        }
    }
}
