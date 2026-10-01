//! Private codecs for the JavaScript host boundary. Tool payloads remain arbitrary JSON.

use serde::Serialize;
use serde_json::Value;

use std::collections::BTreeMap;

use crate::{job::FieldPointer, tool::output::FieldPresentation};

/// Variant-specific fields are grouped here and serialized without patching JSON objects.
pub(super) enum HostResponse {
    Success {
        value: Value,
        presented: Option<BTreeMap<FieldPointer, FieldPresentation>>,
    },
    // Tool failures travel in Success.value as public JobViews; only receive
    // failures use this private bridge error.
    Failure(String),
}

impl HostResponse {
    pub(super) fn encode(&self) -> Result<String, serde_json::Error> {
        #[derive(Serialize)]
        struct Success<'a> {
            ok: bool,
            value: &'a Value,
            #[serde(skip_serializing_if = "Option::is_none")]
            presented: &'a Option<BTreeMap<FieldPointer, FieldPresentation>>,
        }
        #[derive(Serialize)]
        struct Failure<'a> {
            ok: bool,
            error: &'a str,
        }
        match self {
            Self::Success { value, presented } => serde_json::to_string(&Success {
                ok: true,
                value,
                presented,
            }),
            Self::Failure(error) => serde_json::to_string(&Failure { ok: false, error }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn response_codecs_keep_omissions_and_null_payloads() {
        let presented = [
            ("/result/text".parse().unwrap(), FieldPresentation::Complete),
            ("/result/stdout".parse().unwrap(), FieldPresentation::Ends),
        ]
        .into_iter()
        .collect();
        for (response, expected) in [
            (
                HostResponse::Success {
                    value: Value::Null,
                    presented: None,
                },
                json!({"ok":true,"value":null}),
            ),
            (
                HostResponse::Success {
                    value: json!(12),
                    presented: Some(presented),
                },
                json!({"ok":true,"value":12,"presented":{"/result/text":"complete","/result/stdout":"ends"}}),
            ),
            (
                HostResponse::Failure("failure".into()),
                json!({"ok":false,"error":"failure"}),
            ),
        ] {
            assert_eq!(
                serde_json::from_str::<Value>(&response.encode().unwrap()).unwrap(),
                expected
            );
        }
    }
}
