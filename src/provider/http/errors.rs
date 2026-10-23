//! Boundary interpretation of native HTTP and streaming errors: a dialect's
//! reading of its vendors' evidence first, then the codec's reading
//! of the body with its API's own codes, then the HTTP status.
use std::time::Duration;

use crate::provider::{ProviderError, ProviderErrorKind};

/// A vendor's reading of a native error from its status, body and retry
/// hint, for kinds its servers name outside the codec's envelope; `None`
/// leaves the kind to the codec.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ErrorSignals(
    pub fn(Option<u16>, &serde_json::Value, Option<Duration>) -> Option<ProviderErrorKind>,
);

impl ErrorSignals {
    pub(crate) const NONE: Self = Self(|_, _, _| None);
}

/// What a codec read from a native error body, with its own vocabulary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Reading {
    /// The kind the body names: an overflow message, or a known code.
    pub kind: Option<ProviderErrorKind>,
    /// The known code's wire name, for the diagnostic.
    pub code: Option<&'static str>,
    /// The HTTP status an in-stream error repeats in its body.
    pub status: Option<u16>,
}

impl Reading {
    /// The vendor's evidence first, then what the body names, then the status.
    pub(crate) fn kind(
        &self,
        status: Option<u16>,
        native: &serde_json::Value,
        signals: ErrorSignals,
        retry_after: Option<Duration>,
    ) -> ProviderErrorKind {
        let status = status.or(self.status);
        (signals.0)(status, native, retry_after)
            .or(self.kind)
            .unwrap_or_else(|| {
                status.map_or(ProviderErrorKind::Unavailable, ProviderErrorKind::of_status)
            })
    }

    /// `summary`, then the known code and an excerpt of the server's message.
    pub(crate) fn describe(&self, mut summary: String, native: &serde_json::Value) -> String {
        if let Some(code) = self.code {
            summary.push_str(&format!(" [code={code}]"));
        }
        if let Some(excerpt) = server_message(native) {
            summary.push_str(": ");
            summary.push_str(&excerpt);
        }
        summary
    }
}

/// The error object of a native body: its `error` member, or the body itself
/// for servers that send it unwrapped.
pub(crate) fn error_object(native: &serde_json::Value) -> &serde_json::Value {
    native
        .get("error")
        .filter(|value| value.is_object())
        .unwrap_or(native)
}

/// Classify HTTP and in-stream errors identically; diagnostics carry the
/// status, the known code, and an excerpt of the server's message.
pub(crate) fn classify(
    status: Option<u16>,
    native: &serde_json::Value,
    reading: Reading,
    signals: ErrorSignals,
    retry_after: Option<Duration>,
) -> ProviderError {
    let kind = reading.kind(status, native, signals, retry_after);
    let summary = match status.or(reading.status) {
        Some(status) => format!("provider HTTP {status} error"),
        None => "provider stream error".into(),
    };
    kind.error(reading.describe(summary, native))
        .with_retry_after(retry_after)
}

/// The server's own explanation, from the common error envelope shapes:
/// `{"error":{"message"}}`, `{"error":"text"}`, `{"message"}`, or `{"detail"}`,
/// or a bare string for bodies that were not JSON. An error object without one
/// is itself the explanation, as compact JSON, unless it is empty.
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
    .map_or_else(|| members(error_object(native)), excerpt)
}

/// An error object or list as compact JSON, unless it holds nothing.
fn members(value: &serde_json::Value) -> Option<String> {
    let held = match value {
        serde_json::Value::Object(members) => !members.is_empty(),
        serde_json::Value::Array(items) => !items.is_empty(),
        _ => false,
    };
    held.then(|| value.to_string())
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn statuses_set_recovery_and_diagnostics_carry_the_code_and_server_text() {
        let classify = |status, native: &serde_json::Value, reading| {
            classify(status, native, reading, ErrorSignals::NONE, None)
        };
        for (statuses, retryable) in [
            (
                &[301, 400, 401, 402, 403, 404, 405, 413, 422, 426][..],
                false,
            ),
            (&[408, 409, 425, 429, 500, 503, 504][..], true),
        ] {
            for &status in statuses {
                let error = classify(Some(status), &json!({}), Reading::default());
                assert_eq!(error.is_retryable(), retryable, "HTTP {status}");
            }
        }
        assert_eq!(
            classify(Some(401), &json!({}), Reading::default()).kind(),
            ProviderErrorKind::Authentication
        );
        // A status the body repeats stands in for a missing HTTP one.
        let repeated = Reading {
            status: Some(429),
            code: Some("server_error"),
            ..Reading::default()
        };
        let error = classify(
            None,
            &json!({"error":{"message":"upstream\n\tbusy"}}),
            repeated,
        );
        assert!(matches!(error.kind(), ProviderErrorKind::RateLimited));
        assert_eq!(
            error.message,
            "provider HTTP 429 error [code=server_error]: upstream busy"
        );
        for (status, native, expected) in [
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
            // An error object with no string message is shown whole.
            (
                Some(422),
                json!({"detail":[{"loc":["body","tools"],"msg":"field required"}]}),
                r#"provider HTTP 422 error: {"detail":[{"loc":["body","tools"],"msg":"field required"}]}"#,
            ),
            (Some(400), json!({"error":{}}), "provider HTTP 400 error"),
            // A proxy nesting an upstream error keeps the upstream reason.
            (
                Some(400),
                json!({"error":{"message":"proxy.BadRequestError: {\"message\":\"tools.0.strict: Extra inputs\"}\nFallbacks=None","type":null,"code":"400"}}),
                "provider HTTP 400 error: proxy.BadRequestError: {\"message\":\"tools.0.strict: Extra inputs\"} Fallbacks=None",
            ),
        ] {
            assert_eq!(
                classify(status, &native, Reading::default()).message,
                expected
            );
        }
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
