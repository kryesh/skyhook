//! Backend-private interpretation of native HTTP and streaming errors.
use crate::provider::{ProviderError, ProviderErrorKind};

/// Classify HTTP and in-stream errors identically. Diagnostics carry the
/// status, known codes, and an excerpt of the server's message.
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
    } else if identifiers
        .iter()
        .any(|id| matches!(*id, "timeout" | "request_timeout"))
    {
        ProviderErrorKind::Timeout
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
            Some(409 | 425) => ProviderErrorKind::Response,
            Some(300..=499) => ProviderErrorKind::InvalidRequest,
            _ => ProviderErrorKind::Response,
        }
    };
    let mut message = match status {
        Some(status) => format!("provider HTTP {status} error"),
        None => "provider stream error".into(),
    };
    if let Some(code) = safe_error_code(native) {
        message.push_str(&format!(" [code={code}]"));
    }
    append_server_message(&mut message, native);
    ProviderError {
        kind,
        message,
        retry_after: None,
    }
}

pub(super) fn append_server_message(message: &mut String, native: &serde_json::Value) {
    if let Some(excerpt) = server_message(native) {
        message.push_str(": ");
        message.push_str(&excerpt);
    }
}

/// The server's own explanation, from the common error envelope shapes:
/// `{"error":{"message"}}`, `{"error":"text"}`, `{"message"}`, or `{"detail"}`,
/// or a bare string for bodies that were not JSON.
fn server_message(native: &serde_json::Value) -> Option<String> {
    let error = native.get("error");
    [
        error.and_then(|error| error.get("message")),
        error,
        native.get("message"),
        native.get("detail"),
        Some(native),
    ]
    .into_iter()
    .flatten()
    .find_map(serde_json::Value::as_str)
    .and_then(excerpt)
}

/// The server's message as one line.
fn excerpt(text: &str) -> Option<String> {
    let out = text
        .split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    (!out.is_empty()).then_some(out)
}

/// Known machine identifiers, tagged as `[code=…]`; the server's own text is
/// carried separately by `append_server_message`.
pub(super) fn safe_error_code(native: &serde_json::Value) -> Option<&str> {
    let error = native
        .get("error")
        .filter(|value| value.is_object())
        .unwrap_or(native);
    ["code", "type"]
        .into_iter()
        .filter_map(|key| error.get(key)?.as_str())
        .find(|code| {
            matches!(
                *code,
                "context_length_exceeded"
                    | "context_window_exceeded"
                    | "exceed_context_size_error"
                    | "authentication_error"
                    | "invalid_api_key"
                    | "permission_error"
                    | "rate_limit_error"
                    | "rate_limit_exceeded"
                    | "insufficient_quota"
                    | "invalid_request_error"
                    | "invalid_request"
                    | "not_found_error"
                    | "model_not_found"
                    | "timeout"
                    | "request_timeout"
                    | "server_error"
                    | "internal_error"
                    | "internal_server_error"
                    | "overloaded_error"
                    | "service_unavailable"
                    | "request_aborted"
                    | "incomplete_response"
                    | "previous_response_not_found"
                    | "content_filter"
                    | "content_policy_violation"
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn status_recovery_and_diagnostics_never_expose_server_text() {
        for (statuses, retryable) in [
            (&[301, 400, 401, 403, 404, 405, 413, 422, 426][..], false),
            (&[408, 409, 425, 429, 500, 503, 504][..], true),
        ] {
            for &status in statuses {
                let error = classify_error(Some(status), &json!({}));
                assert_eq!(error.is_retryable(), retryable, "HTTP {status}");
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
            (
                None,
                json!({"error":{"message":"slow down"}}),
                "provider stream error: slow down",
            ),
            (
                Some(502),
                json!("<html>Bad gateway</html>"),
                "provider HTTP 502 error: <html>Bad gateway</html>",
            ),
            (
                Some(400),
                json!({"detail":"model not loaded"}),
                "provider HTTP 400 error: model not loaded",
            ),
            (
                Some(400),
                json!({"error":"bad field"}),
                "provider HTTP 400 error: bad field",
            ),
        ] {
            assert_eq!(classify_error(status, &native).message, expected);
        }
    }

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
                assert!(!error.message.contains("[code=unknown"));
            }
        }
        let unauthorized = classify_error(Some(401), &json!({}));
        assert_eq!(unauthorized.kind, ProviderErrorKind::Authentication);
    }

    /// A proxy rejection nesting an upstream error: the reason must survive.
    #[test]
    fn nested_proxy_rejection_is_diagnosable() {
        let native = json!({"error":{"message":"proxy.BadRequestError: UpstreamException - {\"message\":\"The model returned the following errors: tools.0.custom.strict: Extra inputs are not permitted\"}. Received Model Group=vendor.model-family-5\nAvailable Model Group Fallbacks=None","type":null,"param":null,"code":"400"}});
        let error = classify_error(Some(400), &native);
        assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
        assert!(
            error
                .message
                .starts_with("provider HTTP 400 error: proxy.BadRequestError")
        );
        assert!(
            error
                .message
                .contains("tools.0.custom.strict: Extra inputs are not permitted")
        );
        assert!(
            error
                .message
                .contains("Model Group=vendor.model-family-5 Available")
        );
    }

    #[test]
    fn excerpts_are_single_line_and_complete() {
        assert_eq!(
            excerpt("\u{1b}[31mred\u{0}\r\n  text").as_deref(),
            Some("[31mred text")
        );
        assert_eq!(excerpt(" \n\t"), None);
        let long = "é".repeat(1000);
        assert_eq!(excerpt(&long).as_deref(), Some(long.as_str()));
    }
}
