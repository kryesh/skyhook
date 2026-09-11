//! Validation and conversion of native Anthropic content fields.
use super::Block;
use crate::provider::{
    ProviderError,
    protocol::{BlockContent, ToolCall},
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

pub(super) fn tool_content(block: &Block) -> Result<BlockContent, ProviderError> {
    let arguments = if block.has_json_delta {
        serde_json::from_str::<Value>(&block.partial_json)
            .map_err(|_| protocol("invalid tool input JSON"))?
    } else {
        block.native["input"].clone()
    };
    if !arguments.is_object() {
        return Err(protocol("tool input must be a JSON object"));
    }
    Ok(BlockContent::ToolCall(ToolCall {
        id: string(&block.native, "id")?.into(),
        name: string(&block.native, "name")?.into(),
        arguments,
    }))
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
