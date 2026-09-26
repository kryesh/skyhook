//! The error envelope OpenAI's APIs share with the servers that follow them:
//! `{"error": {"code", "type", "message"}}`, or the object unwrapped. Chat and
//! Responses each read it with their own code vocabulary.
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::Value;

use crate::{
    named_enum::NamedEnum,
    provider::{
        ProviderErrorKind,
        codec::common::lenient,
        http::errors::{Reading, error_object},
    },
};

/// A codec's error codes, each spelled once.
pub(crate) trait ErrorCode: NamedEnum + DeserializeOwned {
    /// The kind the code names; `None` leaves the kind to the status.
    fn kind(self) -> Option<ProviderErrorKind>;
}

/// How the API words an overflow without its code, as older models and the
/// servers following them do.
const OVERFLOW_PREFIX: &str = "This model's maximum context length is";

/// `code`: a known code, or the HTTP status some servers repeat there.
#[derive(Deserialize)]
#[serde(untagged, bound = "C: DeserializeOwned")]
enum Code<C> {
    Known(C),
    Status(u16),
}

#[derive(Deserialize)]
#[serde(bound = "C: DeserializeOwned")]
struct Envelope<C> {
    #[serde(default, deserialize_with = "lenient")]
    code: Option<Code<C>>,
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
        Some(Code::Known(code)) => (Some(code), None),
        Some(Code::Status(status)) => (None, Some(status)),
        None => (None, None),
    };
    let overflow = envelope
        .message
        .is_some_and(|message| message.starts_with(OVERFLOW_PREFIX));
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
