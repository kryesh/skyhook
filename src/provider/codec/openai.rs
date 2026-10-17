//! The error envelope OpenAI's APIs share with the servers that follow them:
//! `{"error": {"code", "type", "message"}}`, or the object unwrapped. Messages
//! fits it too, naming its codes only in `type`. Each codec reads it with its
//! own code vocabulary; Chat Completions and Responses share [`Code`].
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::{
    named_enum::{NamedEnum, named_enum},
    provider::{
        ProviderErrorKind,
        codec::common::lenient,
        http::errors::{Reading, error_object},
    },
};

/// A codec's error codes, each spelled once.
pub(crate) trait ErrorCode: NamedEnum + DeserializeOwned {
    /// How the API words an overflow without its code, as older OpenAI models
    /// and the servers following them do.
    const OVERFLOW_PREFIX: &'static str = "This model's maximum context length is";

    /// The kind the code names; `None` leaves the kind to the status.
    fn kind(self) -> Option<ProviderErrorKind>;
}

named_enum! {
    /// The error codes of OpenAI's Chat Completions and Responses APIs, over
    /// HTTP and in the stream.
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
    use crate::provider::http::errors::{ErrorSignals, classify};
    use serde_json::json;

    fn error(status: Option<u16>, native: &Value) -> crate::provider::ProviderError {
        classify(
            status,
            native,
            read::<Code>(native),
            ErrorSignals::NONE,
            None,
        )
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
            (json!({"type":"invalid_request_error"}), InvalidRequest),
            (json!({"code":"insufficient_quota"}), Billing),
            (
                json!({"error":{"message":"This model's maximum context length is 8192 tokens"}}),
                ContextWindowExceeded,
            ),
        ] {
            for status in [None, Some(400), Some(429)] {
                let error = error(status, &native);
                assert_eq!(error.kind(), kind, "{native}");
                assert!(!error.message.contains("[code=unknown"));
            }
        }
        // A status in the body stands in for the missing HTTP one.
        assert_eq!(
            error(None, &json!({"error": {"code":429, "message":"slow down"}})).message,
            "provider HTTP 429 error: slow down"
        );
    }
}
