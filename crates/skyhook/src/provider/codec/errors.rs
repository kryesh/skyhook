//! Boundary interpretation of native HTTP and streaming errors: a dialect's
//! reading of its vendors' evidence first, then the codec's reading of the body
//! with its API's own codes, then the HTTP status. The codecs share one error
//! envelope, `{"error": {"code", "type", "message"}}` or the object unwrapped,
//! which Messages fits too, naming its codes only in `type`. Each codec reads it
//! with its own code vocabulary; Chat Completions and Responses share [`Code`].
use std::time::Duration;

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::provider::{ProviderError, ProviderErrorKind, codec::common::lenient};
use skyhook_base::enums::NamedEnum;

/// A vendor's reading of a native error from its status, body and retry
/// hint, for kinds its servers name outside the codec's envelope; `None`
/// leaves the kind to the codec.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ErrorSignals(
    pub fn(Option<u16>, &Value, Option<Duration>) -> Option<ProviderErrorKind>,
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
        native: &Value,
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
    pub(crate) fn describe(&self, mut summary: String, native: &Value) -> String {
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
pub(crate) fn error_object(native: &Value) -> &Value {
    native
        .get("error")
        .filter(|value| value.is_object())
        .unwrap_or(native)
}

/// Classify HTTP and in-stream errors identically; diagnostics carry the
/// status, the known code, and an excerpt of the server's message.
pub(crate) fn classify(
    status: Option<u16>,
    native: &Value,
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
fn server_message(native: &Value) -> Option<String> {
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
    .find_map(Value::as_str)
    .map_or_else(|| members(error_object(native)), excerpt)
}

/// An error object or list as compact JSON, unless it holds nothing.
fn members(value: &Value) -> Option<String> {
    let held = match value {
        Value::Object(members) => !members.is_empty(),
        Value::Array(items) => !items.is_empty(),
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

/// A codec's error codes, each spelled once.
pub(crate) trait ErrorCode: NamedEnum + DeserializeOwned {
    /// How the API words an overflow without its code, as older OpenAI models
    /// and the servers following them do.
    const OVERFLOW_PREFIX: &'static str = "This model's maximum context length is";

    /// The kind the code names; `None` leaves the kind to the status.
    fn kind(self) -> Option<ProviderErrorKind>;
}

/// The error codes of the Chat Completions and Responses APIs, over HTTP and
/// in the stream.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq, NamedEnum)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Code {
    ContextLengthExceeded,
    InvalidApiKey,
    InsufficientQuota,
    RateLimitExceeded,
    #[serde(rename = "invalid_request_error")]
    InvalidRequest,
    ModelNotFound,
    ServerError,
}

impl ErrorCode for Code {
    fn kind(self) -> Option<ProviderErrorKind> {
        Some(match self {
            Self::ContextLengthExceeded => ProviderErrorKind::ContextWindowExceeded,
            Self::InvalidApiKey => ProviderErrorKind::Authentication,
            Self::InsufficientQuota => ProviderErrorKind::Billing,
            Self::RateLimitExceeded => ProviderErrorKind::RateLimited,
            Self::InvalidRequest => ProviderErrorKind::InvalidRequest,
            Self::ModelNotFound | Self::ServerError => return None,
        })
    }
}

/// `code`: a known code, or the HTTP status some servers repeat there.
#[derive(Deserialize)]
#[serde(untagged, bound = "C: DeserializeOwned")]
enum CodeField<C> {
    Known(C),
    Status(u16),
}

#[derive(Deserialize)]
#[serde(bound = "C: DeserializeOwned")]
struct Envelope<C> {
    #[serde(default, deserialize_with = "lenient")]
    code: Option<CodeField<C>>,
    #[serde(rename = "type", default, deserialize_with = "lenient")]
    kind: Option<C>,
    #[serde(default, deserialize_with = "lenient")]
    message: Option<String>,
}

/// Read `native` with the vocabulary `C`. The specific `code` names the kind
/// before the `type` category.
pub(crate) fn read<C: ErrorCode>(native: &Value) -> Reading {
    let Ok(envelope) = Envelope::<C>::deserialize(error_object(native)) else {
        return Reading::default();
    };
    let (code, status) = match envelope.code {
        Some(CodeField::Known(code)) => (Some(code), None),
        Some(CodeField::Status(status)) => (None, Some(status)),
        None => (None, None),
    };
    let overflow = envelope
        .message
        .is_some_and(|message| message.starts_with(C::OVERFLOW_PREFIX));
    Reading {
        kind: if overflow {
            Some(ProviderErrorKind::ContextWindowExceeded)
        } else {
            code.and_then(C::kind).or(envelope.kind.and_then(C::kind))
        },
        code: code.or(envelope.kind).map(NamedEnum::as_str),
        status,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::assert_matches;

    #[test]
    fn statuses_set_recovery_and_diagnostics_carry_the_code_and_server_text() {
        let classify = |status, native: &Value, reading| {
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
        assert_matches!(error.kind(), ProviderErrorKind::RateLimited);
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

    fn read_error(status: Option<u16>, native: &Value) -> crate::provider::ProviderError {
        classify(
            status,
            native,
            read::<Code>(native),
            ErrorSignals::NONE,
            None,
        )
    }

    /// The specific code names the kind before the type category, over HTTP or
    /// in the stream, wrapped or not.
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
            (json!({"type":"invalid_request_error"}), InvalidRequest),
            (json!({"code":"insufficient_quota"}), Billing),
            (
                json!({"error":{"message":"This model's maximum context length is 8192 tokens"}}),
                ContextWindowExceeded,
            ),
        ] {
            for status in [None, Some(400), Some(429)] {
                let error = read_error(status, &native);
                assert_eq!(error.kind(), kind, "{native}");
            }
        }
        // A status in the body stands in for the missing HTTP one.
        let error = read_error(None, &json!({"error": {"code":429, "message":"slow down"}}));
        assert_eq!(error.kind(), RateLimited);
    }
}
