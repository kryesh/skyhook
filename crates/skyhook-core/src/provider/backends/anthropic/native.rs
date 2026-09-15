//! Validation and conversion of native Anthropic content fields.
use crate::provider::{ProviderError, protocol::ToolCall};
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

pub(super) fn tool_content(
    native: &Value,
    partial_json: Option<&str>,
) -> Result<ToolCall, ProviderError> {
    let arguments = if let Some(partial_json) = partial_json {
        serde_json::from_str::<Value>(partial_json)
            .map_err(|_| protocol("invalid tool input JSON"))?
    } else {
        native["input"].clone()
    };
    ToolCall::new(string(native, "id")?, string(native, "name")?, arguments)
        .map_err(|error| protocol(error.to_string()))
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

pub(super) fn reject_citations(value: &Value) -> Result<(), ProviderError> {
    if let Some(citations) = value.get("citations").filter(|value| !value.is_null())
        && !citations.as_array().is_some_and(Vec::is_empty)
    {
        return Err(protocol(
            "citations cannot be represented by the response protocol",
        ));
    }
    Ok(())
}

pub(super) fn counter(
    value: &Value,
    key: &str,
    previous: u64,
    required: bool,
) -> Result<u64, ProviderError> {
    match value.get(key) {
        Some(number) => {
            let next = number
                .as_u64()
                .ok_or_else(|| protocol(format!("invalid usage counter {key}")))?;
            if next < previous {
                return Err(protocol(format!("usage counter {key} decreased")));
            }
            Ok(next)
        }
        None if required => Err(protocol(format!("missing usage counter {key}"))),
        None => Ok(previous),
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
            ("call", "tool", Value::Null),
            ("call", "tool", json!([])),
            ("call", "tool", json!(1)),
            ("call", "tool", json!("{}")),
        ] {
            let error = tool_content(&tool_block(id, name, input), None).unwrap_err();
            assert_eq!(error.kind, crate::provider::ProviderErrorKind::Protocol);
        }
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
