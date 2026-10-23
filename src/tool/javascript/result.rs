//! Canonical public script result, retaining console completion evidence until projection.

use std::borrow::Cow;

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde_json::{Map, Value};

use crate::{job::output::CompletedCapture, tool::ToolOutput};

/// Project a script result. A missing capture means genuinely empty console output;
/// images remain owned separately by `ToolOutput`. Completion evidence is preserved
/// until the terminal output owner publishes it: only that owner inserts a
/// captured-field placeholder into the wire result.
pub(crate) fn script_output(
    value: Value,
    failure: Option<Map<String, Value>>,
    console: Option<CompletedCapture>,
) -> ToolOutput {
    let mut result = Map::new();
    result.insert("value".into(), value);
    if let Some(failure) = failure {
        result.insert("failure".into(), Value::Object(failure));
    }
    ToolOutput::new(Value::Object(result)).with_captures(console.into_iter().collect())
}

/// Schema-only marker for the script tool's native result.
pub(crate) struct ScriptResult;

impl JsonSchema for ScriptResult {
    fn schema_name() -> Cow<'static, str> {
        "ScriptResult".into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        // Keep arbitrary JSON as {}, and retain the existing untitled wire schema.
        schemars::json_schema!({
            "type": "object",
            "properties": {
                "value": {},
                "console": {"type": "string"},
                "failure": {"type": "object"}
            },
            "required": ["value"],
            "additionalProperties": false
        })
    }
}
