//! Private codecs for the JavaScript host boundary. Tool payloads remain arbitrary JSON.

use serde::Serialize;
use serde_json::Value;

use std::collections::BTreeSet;

/// Variant-specific fields are grouped here and serialized without patching JSON objects.
pub(super) enum HostResponse {
    Success {
        value: Value,
        annotations: Option<BTreeSet<String>>,
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
            annotations: &'a Option<BTreeSet<String>>,
        }
        #[derive(Serialize)]
        struct Failure<'a> {
            ok: bool,
            error: &'a str,
        }
        match self {
            Self::Success { value, annotations } => serde_json::to_string(&Success {
                ok: true,
                value,
                annotations,
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
        let annotations = ["/result/text".to_owned()].into_iter().collect();
        for (response, expected) in [
            (
                HostResponse::Success {
                    value: Value::Null,
                    annotations: None,
                },
                json!({"ok":true,"value":null}),
            ),
            (
                HostResponse::Success {
                    value: json!(12),
                    annotations: Some(annotations),
                },
                json!({"ok":true,"value":12,"annotations":["/result/text"]}),
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
