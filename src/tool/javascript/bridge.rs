//! Private codecs for the JavaScript host boundary. Tool payloads remain arbitrary JSON.

use serde::Serialize;
use serde_json::Value;

use crate::{identity::JobId, tool::Denial};

#[derive(Serialize)]
pub(super) struct SourceProvenance {
    pub(super) source_job: JobId,
    pub(super) annotations: std::collections::BTreeSet<String>,
}

/// Variant-specific fields are grouped here and serialized without patching JSON objects.
pub(super) enum HostResponse {
    Success {
        value: Value,
        provenance: Option<SourceProvenance>,
    },
    Failure {
        message: String,
        denial: Option<Denial>,
        output: Option<Value>,
    },
}

impl HostResponse {
    pub(super) fn failure(message: String) -> Self {
        Self::Failure {
            message,
            denial: None,
            output: None,
        }
    }

    pub(super) fn encode(&self) -> Result<String, serde_json::Error> {
        #[derive(Serialize)]
        struct Success<'a> {
            ok: bool,
            value: &'a Value,
            #[serde(flatten, skip_serializing_if = "Option::is_none")]
            provenance: &'a Option<SourceProvenance>,
        }
        #[derive(Serialize)]
        struct Failure<'a> {
            ok: bool,
            error: &'a str,
            #[serde(flatten, skip_serializing_if = "Option::is_none")]
            denial: &'a Option<Denial>,
            #[serde(skip_serializing_if = "Option::is_none")]
            output: &'a Option<Value>,
        }
        match self {
            Self::Success { value, provenance } => serde_json::to_string(&Success {
                ok: true,
                value,
                provenance,
            }),
            Self::Failure {
                message,
                denial,
                output,
            } => serde_json::to_string(&Failure {
                ok: false,
                error: message,
                denial,
                output,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn response_codecs_keep_omissions_and_null_payloads() {
        let provenance = SourceProvenance {
            source_job: serde_json::from_value(json!(1)).unwrap(),
            annotations: Default::default(),
        };
        for (response, expected) in [
            (
                HostResponse::Success {
                    value: Value::Null,
                    provenance: None,
                },
                json!({"ok":true,"value":null}),
            ),
            (
                HostResponse::Success {
                    value: json!(12),
                    provenance: Some(provenance),
                },
                json!({"ok":true,"value":12,"source_job":1,"annotations":[]}),
            ),
            (
                HostResponse::Failure {
                    message: "failure".into(),
                    denial: None,
                    output: Some(Value::Null),
                },
                json!({"ok":false,"error":"failure","output":null}),
            ),
            (
                HostResponse::Failure {
                    message: "denied".into(),
                    denial: Some(Denial::permission_denied()),
                    output: Some(json!({"nested":null})),
                },
                json!({"ok":false,"error":"denied","code":"permission_denied","executed":false,"output":{"nested":null}}),
            ),
        ] {
            assert_eq!(
                serde_json::from_str::<Value>(&response.encode().unwrap()).unwrap(),
                expected
            );
        }
    }
}
