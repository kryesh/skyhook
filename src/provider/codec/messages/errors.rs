//! Anthropic's error envelope: `{"type": "error", "error": {"type", "message"}}`.
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    named_enum::named_enum,
    provider::{
        ProviderErrorKind,
        codec::common::lenient,
        http::errors::{Reading, error_object},
    },
};

named_enum! {
    /// The `error.type` values.
    #[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
    pub(crate) enum Code {
        InvalidRequest = "invalid_request_error",
        Authentication = "authentication_error",
        Permission = "permission_error",
        NotFound = "not_found_error",
        RequestTooLarge = "request_too_large",
        RateLimit = "rate_limit_error",
        Billing = "billing_error",
        Api = "api_error",
        Overloaded = "overloaded_error",
    }
}

impl Code {
    /// The kind the code names; `None` leaves the kind to the status.
    fn kind(self) -> Option<ProviderErrorKind> {
        Some(match self {
            Self::InvalidRequest | Self::NotFound | Self::RequestTooLarge => {
                ProviderErrorKind::InvalidRequest
            }
            Self::Authentication | Self::Permission => ProviderErrorKind::Authentication,
            Self::RateLimit => ProviderErrorKind::RateLimited { retry_after: None },
            Self::Billing => ProviderErrorKind::Billing,
            Self::Api | Self::Overloaded => return None,
        })
    }
}

/// How the service words an overflow; it has no code of its own.
const OVERFLOW_PREFIX: &str = "prompt is too long";

#[derive(Default, Deserialize)]
struct Envelope {
    #[serde(rename = "type", default, deserialize_with = "lenient")]
    code: Option<Code>,
    #[serde(default, deserialize_with = "lenient")]
    message: Option<String>,
}

/// A Messages error body, HTTP or in-stream.
pub(crate) fn read(native: &Value) -> Reading {
    let envelope = Envelope::deserialize(error_object(native)).unwrap_or_default();
    let overflow = envelope
        .message
        .is_some_and(|message| message.starts_with(OVERFLOW_PREFIX));
    Reading {
        kind: if overflow {
            Some(ProviderErrorKind::ContextWindowExceeded)
        } else {
            envelope.code.and_then(Code::kind)
        },
        code: envelope.code.map(Code::as_str),
        status: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::http::errors::{ErrorSignals, classify};
    use serde_json::json;

    #[test]
    fn types_name_the_kind_over_http_and_in_the_stream() {
        use ProviderErrorKind::*;
        for (kind_name, kind) in [
            ("authentication_error", Authentication),
            ("billing_error", Billing),
            ("rate_limit_error", RateLimited { retry_after: None }),
            ("invalid_request_error", InvalidRequest),
        ] {
            let native = json!({"type":"error", "error":{"type":kind_name}});
            for status in [None, Some(400)] {
                let reading = read(&native);
                let error = classify(status, &native, reading, ErrorSignals::NONE, None);
                assert_eq!(error.kind, kind);
                assert!(error.message.contains(&format!("[code={kind_name}]")));
            }
        }
    }
}
