//! Validation and conversion of native Anthropic content fields.
use crate::provider::{
    ProviderError,
    backends::common::{arguments_field, lenient_u64, parse_tool_arguments},
    protocol::ToolCall,
};
use serde_json::Value;

pub(super) fn protocol(message: impl Into<String>) -> ProviderError {
    ProviderError::protocol(format!("Anthropic: {}", message.into()))
}

pub(super) fn string<'a>(value: &'a Value, field: &str) -> Result<&'a str, ProviderError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| protocol(format!("missing or invalid string field {field}")))
}

pub(super) fn validate_thinking(value: &Value) -> Result<(), ProviderError> {
    match string(value, "type")? {
        "thinking" => {
            string(value, "thinking")?;
            if string(value, "signature")?.is_empty() {
                return Err(protocol("thinking block has an empty signature"));
            }
        }
        "redacted_thinking" => {
            if string(value, "data")?.is_empty() {
                return Err(protocol("redacted thinking block has empty data"));
            }
        }
        _ => return Err(protocol("opaque reasoning is not a native thinking block")),
    }
    Ok(())
}

/// The call's input from the start block and any streamed JSON. A streamed
/// placeholder (`""`, `{}`, `null`) never erases start input, and two
/// different non-empty inputs are a conflict rather than a choice.
pub(super) fn tool_content(
    native: &Value,
    partial_json: Option<&str>,
) -> Result<ToolCall, ProviderError> {
    let initial = initial_input(native)?;
    let streamed = partial_json
        .map(|json| parse_tool_arguments(json).ok_or_else(|| protocol("invalid tool input JSON")))
        .transpose()?;
    let arguments = match streamed {
        Some(streamed) if initial.is_empty() => streamed,
        Some(streamed) if streamed.is_empty() || streamed == initial => initial,
        Some(_) => return Err(protocol("streamed tool input conflicts with start input")),
        None => initial,
    };
    ToolCall::new(
        string(native, "id")?,
        string(native, "name")?,
        Value::Object(arguments),
    )
    .map_err(|error| protocol(error.to_string()))
}

/// A `tool_use` start block's input.
pub(super) fn initial_input(
    native: &Value,
) -> Result<serde_json::Map<String, Value>, ProviderError> {
    arguments_field(native.get("input")).ok_or_else(|| protocol("invalid tool input"))
}

pub(super) fn index(value: &Value) -> Result<usize, ProviderError> {
    value
        .get("index")
        .and_then(Value::as_u64)
        .and_then(|index| usize::try_from(index).ok())
        .ok_or_else(|| protocol("missing or invalid content block index"))
}

pub(super) fn append(value: &mut Value, field: &str, suffix: &str) -> Result<(), ProviderError> {
    match value.get_mut(field) {
        Some(Value::String(text)) => {
            text.push_str(suffix);
            Ok(())
        }
        _ => Err(protocol(format!("missing or invalid string field {field}"))),
    }
}

/// Cumulative counters: missing or smaller late values keep the previous total.
pub(super) fn counter(value: &Value, key: &str, previous: u64) -> Result<u64, ProviderError> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(previous),
        Some(number) => lenient_u64(number)
            .map(|next| next.max(previous))
            .ok_or_else(|| protocol(format!("invalid usage counter {key}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool_block(id: &str, name: &str, input: Value) -> Value {
        json!({"type":"tool_use", "id":id, "name":name, "input":input})
    }

    #[test]
    fn completed_tool_input_rejects_empty_identity_and_nonobjects() {
        for (id, name, input) in [
            ("", "tool", json!({})),
            ("call", "", json!({})),
            ("call", "tool", json!([])),
            ("call", "tool", json!(1)),
            ("call", "tool", json!("[1]")),
        ] {
            let error = tool_content(&tool_block(id, name, input), None).unwrap_err();
            assert_eq!(error.kind, crate::provider::ProviderErrorKind::Protocol);
        }
    }

    #[test]
    fn empty_missing_and_encoded_inputs_are_objects() {
        let block = tool_block("call", "list_jobs", json!({}));
        for partial in [None, Some(""), Some("  ")] {
            let call = tool_content(&block, partial).unwrap();
            assert!(call.arguments().is_empty());
        }
        for input in [Value::Null, json!("{}"), json!("")] {
            let call = tool_content(&tool_block("call", "tool", input), None).unwrap();
            assert!(call.arguments().is_empty());
        }
        // Streamed placeholders or an identical restatement keep start input.
        let started = tool_block("call", "tool", json!({"x":1}));
        for partial in ["", "{}", "null", "\"\"", "{\"x\":1}"] {
            let call = tool_content(&started, Some(partial)).unwrap();
            assert_eq!(
                Value::Object(call.arguments().clone()),
                json!({"x":1}),
                "{partial}"
            );
        }
        // Two different inputs conflict.
        assert!(tool_content(&started, Some("{\"x\":2}")).is_err());
    }

    #[test]
    fn counters_are_monotone_and_lenient() {
        let usage = json!({"a":5, "b":"9", "c":2});
        assert_eq!(counter(&usage, "a", 3).unwrap(), 5);
        assert_eq!(counter(&usage, "b", 3).unwrap(), 9);
        assert_eq!(counter(&usage, "c", 3).unwrap(), 3);
        assert_eq!(counter(&usage, "missing", 4).unwrap(), 4);
        assert!(counter(&json!({"a":"x"}), "a", 0).is_err());
    }

    #[test]
    fn complete_json_fragments_preserve_arbitrary_arguments_and_native_names() {
        let input = json!({"x-vendor":{"anyOf":[null, [1, false], "雪"]}});
        let block = tool_block("call", "vendor.tool/雪", json!({}));
        assert!(tool_content(&block, Some("{\"x-vendor\":")).is_err());
        let call = tool_content(&block, Some(&input.to_string())).unwrap();
        assert_eq!(call.name(), "vendor.tool/雪");
        assert_eq!(Value::Object(call.arguments().clone()), input);
    }
}
