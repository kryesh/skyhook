//! Canonical public script result, retaining console completion evidence until projection.

use std::borrow::Cow;

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde_json::{Map, Value};

use crate::{job::output::CompletedCapture, tool::ToolOutput};

/// Project a script result. A missing capture means genuinely empty console output,
/// not a capture placeholder; images remain owned separately by `ToolOutput`.
/// Completion evidence is preserved until the terminal output owner publishes it:
/// only that owner inserts a captured-field placeholder into the wire result.
pub(crate) fn script_output(
    value: Value,
    failure: Option<Value>,
    console: Option<CompletedCapture>,
) -> ToolOutput {
    let mut result = Map::new();
    result.insert("value".into(), value);
    result.insert("failure".into(), failure.unwrap_or(Value::Null));
    let captures = match console {
        Some(capture) => vec![capture],
        None => {
            result.insert("console".into(), Value::String(String::new()));
            Vec::new()
        }
    };
    ToolOutput::new(Value::Object(result)).with_captures(captures)
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
                "console": {"type": "string", "x-skyhook-truncatable": true},
                "failure": {}
            },
            "required": ["value", "console", "failure"],
            "additionalProperties": false
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{
        job::output::{CaptureKind, CaptureWriter, TextCaptureField},
        tests::TestRuntime,
    };

    async fn console(runtime: &TestRuntime, job: crate::identity::JobId) -> CaptureWriter {
        let field = TextCaptureField::Console.pointer();
        let pending = runtime
            .jobs
            .pending_capture(job, field, CaptureKind::Text, true);
        pending.await.unwrap().open()
    }

    fn assert_codec(output: ToolOutput, expected: Value) {
        assert!(output.captures.is_empty());
        assert_eq!(output.value, expected);
        let round_trip: Value =
            serde_json::from_str(&serde_json::to_string(&output.value).unwrap()).unwrap();
        assert_eq!(round_trip, expected);
    }

    #[test]
    fn schema_requires_all_canonical_fields() {
        let schema = serde_json::to_value(schemars::schema_for!(ScriptResult)).unwrap();
        assert_eq!(schema["required"], json!(["value", "console", "failure"]));
    }

    #[tokio::test]
    async fn empty_console_success_preserves_arbitrary_values_and_real_empty_finalization() {
        for value in [
            Value::Null,
            json!(false),
            json!(42),
            json!("text"),
            json!([null, {"nested": true}]),
            json!({"failure": "ordinary returned data", "console": [1, 2]}),
        ] {
            assert_codec(
                script_output(value.clone(), None, None),
                json!({"value": value, "console": "", "failure": null}),
            );
        }
        let runtime = TestRuntime::new().await;
        let spec = crate::job::JobSpec::test(runtime.agent.clone(), "script");
        let job = runtime.jobs.test_create(spec).await;
        let proof = console(&runtime, job).await.finish_nonempty().unwrap();
        assert!(proof.is_none());
        assert_codec(
            script_output(Value::Null, None, proof),
            json!({"value": null, "console": "", "failure": null}),
        );
    }

    #[tokio::test]
    async fn finalized_console_proof_projects_on_success_and_failure() {
        let runtime = TestRuntime::new().await;
        for failure in [false, true] {
            let spec = crate::job::JobSpec::test(runtime.agent.clone(), "script");
            let lease = runtime.jobs.create(spec).await.unwrap();
            let job = lease.id();
            let mut pending = console(&runtime, job).await;
            pending.write_text("captured console\n").unwrap();
            let proof = pending.finish_nonempty().unwrap();
            assert!(proof.is_some());
            let output = if failure {
                script_output(Value::Null, Some(json!({"message": "failed"})), proof)
            } else {
                script_output(json!(42), None, proof)
            };
            assert!(
                output.value.get("console").is_none(),
                "only terminal publication inserts a captured placeholder"
            );
            assert_eq!(output.captures.len(), 1);
            assert_eq!(output.captures[0].field(), "/result/console");
            // snapshot() hydrates finalized captures; only the stored terminal
            // document uses a captured-field placeholder.
            let (expected, outcome) = if failure {
                let expected = json!({"value": null, "console": "captured console\n", "failure": {"message": "failed"}});
                let outcome = crate::job::JobOutcome::Failed {
                    message: "failed".into(),
                    output: Some(output),
                    denial: None,
                };
                (expected, outcome)
            } else {
                (
                    json!({"value": 42, "console": "captured console\n", "failure": null}),
                    crate::job::JobOutcome::Completed(output),
                )
            };
            runtime.jobs.finish(job, outcome).await.unwrap();
            assert_eq!(
                runtime.jobs.snapshot(job).await.unwrap().output,
                Some(expected)
            );
        }
    }
}
