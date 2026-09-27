//! Native Anthropic content blocks: their types, and validation and
//! conversion of tool blocks.
use super::NATIVE;
use crate::{
    named_enum::named_enum,
    provider::{
        ProviderError,
        codec::common::{arguments_field, parse_tool_arguments},
        protocol::ToolCall,
    },
};
use serde_json::Value;

named_enum! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) parsed enum BlockType {
        Text = "text",
        Thinking = "thinking",
        RedactedThinking = "redacted_thinking",
        ToolUse = "tool_use",
    }
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
        .map(|json| {
            parse_tool_arguments(json).ok_or_else(|| NATIVE.error("invalid tool input JSON"))
        })
        .transpose()?;
    let arguments = match streamed {
        Some(streamed) if initial.is_empty() => streamed,
        Some(streamed) if streamed.is_empty() || streamed == initial => initial,
        Some(_) => return Err(NATIVE.error("streamed tool input conflicts with start input")),
        None => initial,
    };
    ToolCall::new(
        NATIVE.string(native, "id")?,
        NATIVE.string(native, "name")?,
        Value::Object(arguments),
    )
    .map_err(|error| NATIVE.error(error))
}

/// A `tool_use` start block's input.
pub(super) fn initial_input(
    native: &Value,
) -> Result<serde_json::Map<String, Value>, ProviderError> {
    arguments_field(native.get("input")).ok_or_else(|| NATIVE.error("invalid tool input"))
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
            assert_eq!(error.kind(), crate::provider::ProviderErrorKind::Protocol);
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
    fn complete_json_fragments_preserve_arbitrary_arguments_and_native_names() {
        let input = json!({"x-vendor":{"anyOf":[null, [1, false], "雪"]}});
        let block = tool_block("call", "vendor.tool/雪", json!({}));
        assert!(tool_content(&block, Some("{\"x-vendor\":")).is_err());
        let call = tool_content(&block, Some(&input.to_string())).unwrap();
        assert_eq!(call.name(), "vendor.tool/雪");
        assert_eq!(Value::Object(call.arguments().clone()), input);
    }
}
