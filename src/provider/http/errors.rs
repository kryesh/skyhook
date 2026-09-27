//! Boundary interpretation of native HTTP and streaming errors: a dialect's
//! rules and reader for its vendors' evidence first, then the codec's reading
//! of the body with its API's own codes, then the HTTP status.
use std::time::Duration;

use crate::provider::{ProviderError, ProviderErrorKind};

/// One vendor error shape. Every named condition must hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ErrorRule {
    pub status: Option<u16>,
    pub field: Option<Field>,
    /// Text that `error.message` contains, for servers that name the condition
    /// only there.
    pub message: Option<&'static str>,
    pub retry_after: RetryAfter,
    pub kind: ProviderErrorKind,
}

/// A condition on a dot path under `error`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Field {
    Equals(&'static str, &'static str),
    /// The path holds any value; for lists such as moderation reasons.
    Present(&'static str),
}

impl Field {
    fn holds(self, error: &serde_json::Value) -> bool {
        let lookup = |path: &str| path.split('.').try_fold(error, |value, key| value.get(key));
        match self {
            Self::Equals(path, expected) => {
                lookup(path).and_then(serde_json::Value::as_str) == Some(expected)
            }
            Self::Present(path) => lookup(path).is_some_and(|value| !value.is_null()),
        }
    }
}

/// Whether the rule needs the response's `Retry-After` hint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RetryAfter {
    Any,
    /// The same status without a hint means something else (an OpenRouter 402
    /// mid-request is transient only with one).
    Required,
}

impl ErrorRule {
    /// A rule with no conditions yet; presets add the ones that hold.
    pub(crate) const fn kind(kind: ProviderErrorKind) -> Self {
        Self {
            status: None,
            field: None,
            message: None,
            retry_after: RetryAfter::Any,
            kind,
        }
    }
}

/// A vendor's reading of the whole native body, for kinds its servers name
/// outside the codec's envelope; `None` leaves the kind to the codec.
pub(crate) type Reader = fn(&serde_json::Value) -> Option<ProviderErrorKind>;

/// A vendor's error evidence, consulted before the codec reads the body: its
/// rules, then its reader.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ErrorSignals {
    pub rules: &'static [ErrorRule],
    pub read: Reader,
}

impl ErrorSignals {
    pub(crate) const NONE: Self = Self {
        rules: &[],
        read: |_| None,
    };

    fn classify(
        &self,
        status: Option<u16>,
        native: &serde_json::Value,
        retry_after: Option<Duration>,
    ) -> Option<ProviderErrorKind> {
        let error = error_object(native);
        let holds = |rule: &ErrorRule| {
            rule.status.is_none_or(|expected| status == Some(expected))
                && rule.field.is_none_or(|field| field.holds(error))
                && (rule.retry_after == RetryAfter::Any || retry_after.is_some())
                && rule.message.is_none_or(|text| {
                    let message = error.get("message").and_then(serde_json::Value::as_str);
                    message.is_some_and(|message| message.contains(text))
                })
        };
        self.rules
            .iter()
            .find(|rule| holds(rule))
            .map(|rule| rule.kind)
            .or_else(|| (self.read)(native))
    }
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
        signals
            .classify(status, native, retry_after)
            .or(self.kind)
            .unwrap_or(match status {
                Some(401 | 403) => ProviderErrorKind::Authentication,
                Some(402) => ProviderErrorKind::Billing,
                Some(408 | 504) => ProviderErrorKind::Timeout,
                Some(429) => ProviderErrorKind::RateLimited,
                Some(409 | 425) => ProviderErrorKind::Unavailable,
                Some(300..=499) => ProviderErrorKind::InvalidRequest,
                _ => ProviderErrorKind::Unavailable,
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A rule matches only when every condition it names holds, and a match
    /// wins over the codec's reading.
    #[test]
    fn rules_match_every_condition_before_the_codec_reading() {
        const RULES: ErrorSignals = ErrorSignals {
            rules: &[
                ErrorRule {
                    status: Some(429),
                    field: Some(Field::Equals("detail.code", "cap")),
                    ..ErrorRule::kind(ProviderErrorKind::Billing)
                },
                ErrorRule {
                    message: Some("Marker"),
                    ..ErrorRule::kind(ProviderErrorKind::ContextWindowExceeded)
                },
                ErrorRule {
                    field: Some(Field::Present("reasons")),
                    ..ErrorRule::kind(ProviderErrorKind::InvalidRequest)
                },
                ErrorRule {
                    status: Some(402),
                    retry_after: RetryAfter::Required,
                    ..ErrorRule::kind(ProviderErrorKind::Unavailable)
                },
            ],
            ..ErrorSignals::NONE
        };
        let limited = Reading {
            kind: Some(ProviderErrorKind::RateLimited),
            ..Reading::default()
        };
        let kind = |status, native: serde_json::Value, reading: Reading, hint| {
            classify(status, &native, reading, RULES, hint).kind()
        };
        let capped = json!({"error":{"detail":{"code":"cap"}}});
        assert_eq!(
            kind(Some(429), capped.clone(), limited, None),
            ProviderErrorKind::Billing
        );
        assert!(matches!(
            kind(Some(503), capped, limited, None),
            ProviderErrorKind::RateLimited
        ));
        let none = Reading::default();
        assert_eq!(
            kind(None, json!({"error":{"message":"x.Marker: y"}}), none, None),
            ProviderErrorKind::ContextWindowExceeded
        );
        assert!(matches!(
            kind(None, json!({"error":{"message":"No marker"}}), none, None),
            ProviderErrorKind::Unavailable
        ));
        assert_eq!(
            kind(Some(401), json!({"error":{"reasons":["x"]}}), none, None),
            ProviderErrorKind::InvalidRequest
        );
        assert_eq!(
            kind(Some(401), json!({"error":{"reasons":null}}), none, None),
            ProviderErrorKind::Authentication
        );
        let hint = Some(Duration::from_secs(3));
        let hinted = |status| {
            let error = classify(Some(status), &json!({}), none, RULES, hint);
            (error.kind(), error.retry_after())
        };
        assert_eq!(hinted(402), (ProviderErrorKind::Unavailable, hint));
        assert_eq!(
            kind(Some(402), json!({}), none, None),
            ProviderErrorKind::Billing
        );
        assert_eq!(hinted(429), (ProviderErrorKind::RateLimited, hint));
    }

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
