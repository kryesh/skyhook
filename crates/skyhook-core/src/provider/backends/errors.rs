//! Backend-private interpretation of native HTTP and streaming errors.
use crate::provider::{ProviderError, ProviderErrorKind};

/// Classify HTTP and in-stream errors identically without exposing server-controlled
/// messages, URLs, credentials, prompts, or unknown error identifiers.
pub(super) fn classify_error(status: Option<u16>, native: &serde_json::Value) -> ProviderError {
    let error = native
        .get("error")
        .filter(|value| value.is_object())
        .unwrap_or(native);
    let code = error
        .get("code")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let typ = error
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let status = status.or_else(|| {
        error
            .get("code")
            .and_then(serde_json::Value::as_u64)
            .and_then(|v| u16::try_from(v).ok())
    });
    let identifiers = [code, typ];
    let kind = if identifiers.iter().any(|id| {
        matches!(
            *id,
            "context_length_exceeded" | "context_window_exceeded" | "exceed_context_size_error"
        )
    }) {
        ProviderErrorKind::ContextWindowExceeded
    } else if identifiers.iter().any(|id| {
        matches!(
            *id,
            "authentication_error" | "invalid_api_key" | "permission_error"
        )
    }) {
        ProviderErrorKind::Authentication
    } else if identifiers
        .iter()
        .any(|id| matches!(*id, "rate_limit_error" | "rate_limit_exceeded"))
    {
        ProviderErrorKind::RateLimited
    } else if identifiers.iter().any(|id| {
        matches!(
            *id,
            "invalid_request_error" | "invalid_request" | "not_found_error"
        )
    }) {
        ProviderErrorKind::InvalidRequest
    } else {
        match status {
            Some(401 | 403) => ProviderErrorKind::Authentication,
            Some(408 | 504) => ProviderErrorKind::Timeout,
            Some(429) => ProviderErrorKind::RateLimited,
            Some(400 | 404 | 413 | 422) => ProviderErrorKind::InvalidRequest,
            _ => ProviderErrorKind::Response,
        }
    };
    ProviderError {
        kind,
        message: match status {
            Some(status) => format!("provider HTTP {status} error"),
            None => "provider stream error".into(),
        },
    }
}

#[cfg(test)]
mod error_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn http_and_stream_errors_share_typed_sanitized_classification() {
        for (native, expected) in [
            (
                json!({"error":{"code":400,"type":"exceed_context_size_error","message":"secret prompt"}}),
                ProviderErrorKind::ContextWindowExceeded,
            ),
            (
                json!({"error":{"code":"unknown","type":"context_length_exceeded"}}),
                ProviderErrorKind::ContextWindowExceeded,
            ),
            (
                json!({"type":"authentication_error"}),
                ProviderErrorKind::Authentication,
            ),
            (
                json!({"type":"rate_limit_error"}),
                ProviderErrorKind::RateLimited,
            ),
            (
                json!({"type":"invalid_request_error"}),
                ProviderErrorKind::InvalidRequest,
            ),
        ] {
            for status in [None, Some(400)] {
                let error = classify_error(status, &native);
                assert_eq!(error.kind, expected);
                assert!(!error.message.contains("secret"));
                assert!(!error.message.contains("unknown"));
            }
        }
        assert_eq!(
            classify_error(Some(401), &json!({})).kind,
            ProviderErrorKind::Authentication
        );
        assert_eq!(
            classify_error(None, &json!({"error":{"message":"secret"}})).message,
            "provider stream error"
        );
    }
}
