//! Native item shapes and field/argument validation.
use super::*;

pub(super) fn protocol(message: impl Into<String>) -> ProviderError {
    ProviderError::protocol(format!("Responses: {}", message.into()))
}

pub(super) fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str, ProviderError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| protocol(format!("missing or invalid {key}")))
}

pub(super) fn index(value: &Value, key: &str) -> Result<usize, ProviderError> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(|| protocol(format!("missing or invalid {key}")))
}

pub(super) fn array<'a>(value: &'a Value, key: &str) -> Result<&'a Vec<Value>, ProviderError> {
    value
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| protocol(format!("missing or invalid {key}")))
}

/// Items of a type this protocol does not represent (hosted tools, future
/// types) are ignored; items without a type are malformed.
pub(super) fn is_foreign(item: &Value) -> bool {
    item.get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| !matches!(kind, "message" | "reasoning" | "function_call"))
}

pub(super) fn kind(item: &Value) -> Result<ItemKind, ProviderError> {
    match string(item, "type")? {
        "message" => Ok(ItemKind::Text),
        "reasoning" => Ok(ItemKind::Reasoning),
        "function_call" => Ok(ItemKind::ToolCall),
        _ => Err(protocol("unsupported output item type")),
    }
}

/// Validated view of consumed item identity; all vendor fields stay in `raw`.
#[derive(Clone, Copy, Debug)]
pub(super) struct NativeItem<'a> {
    pub(super) raw: &'a Value,
    pub(super) id: &'a str,
    pub(super) kind: ItemKind,
}
impl<'a> NativeItem<'a> {
    pub(super) fn parse(raw: &'a Value) -> Result<Self, ProviderError> {
        let id = string(raw, "id")?;
        if id.is_empty() {
            return Err(protocol("empty item ID"));
        }
        Ok(Self {
            raw,
            id,
            kind: kind(raw)?,
        })
    }

    pub(super) fn final_parts(self) -> Result<Vec<BlockContent>, ProviderError> {
        parts_for_kind(self.raw, self.kind)
    }
}

pub(super) fn final_parts(item: &Value) -> Result<Vec<BlockContent>, ProviderError> {
    parts_for_kind(item, kind(item)?)
}

fn parts_for_kind(item: &Value, kind: ItemKind) -> Result<Vec<BlockContent>, ProviderError> {
    match kind {
        ItemKind::Text if item.get("role").is_some_and(|role| role != "assistant") => {
            Err(protocol("output message role is not assistant"))
        }
        // Content parts other than text and refusals carry nothing representable.
        ItemKind::Text => array(item, "content")?
            .iter()
            .filter_map(|part| {
                let text = match part.get("type").and_then(Value::as_str)? {
                    "output_text" => string(part, "text"),
                    "refusal" => string(part, "refusal"),
                    _ => return None,
                };
                Some(text.map(|text| BlockContent::Text { text: text.into() }))
            })
            .collect(),
        ItemKind::Reasoning => Ok(reasoning_parts(item)?.into_values().collect()),
        ItemKind::ToolCall => Ok(vec![BlockContent::ToolCall(function_call(item)?)]),
    }
}

pub(super) fn function_call(item: &Value) -> Result<ToolCall, ProviderError> {
    let arguments = item_arguments(item)?;
    let id = string(item, "call_id")?;
    let name = string(item, "name")?;
    ToolCall::new(id, name, Value::Object(arguments)).map_err(|error| protocol(error.to_string()))
}

/// A function item's arguments.
pub(super) fn item_arguments(
    item: &Value,
) -> Result<serde_json::Map<String, Value>, ProviderError> {
    crate::provider::backends::common::arguments_field(item.get("arguments"))
        .ok_or_else(|| protocol("function arguments must be a JSON object"))
}

/// Executable function arguments must decode to an object.
pub(super) fn arguments(text: &str) -> Result<serde_json::Map<String, Value>, ProviderError> {
    crate::provider::backends::common::parse_tool_arguments(text)
        .ok_or_else(|| protocol("function arguments must be a JSON object"))
}

impl ItemKind {
    pub(super) fn block_kind(self) -> BlockKind {
        match self {
            Self::Text => BlockKind::Text,
            Self::Reasoning => BlockKind::Reasoning,
            Self::ToolCall => BlockKind::ToolCallArguments,
        }
    }
    pub(super) fn part_id(self, position: usize) -> String {
        match self {
            Self::Text => format!("content_{position}"),
            Self::Reasoning if position.is_multiple_of(2) => format!("summary_{}", position / 2),
            Self::Reasoning => format!("content_{}", position / 2),
            Self::ToolCall => format!("arguments_{position}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completed_function_identity_is_nonempty_without_chat_name_rules() {
        let mut item = json!({"type":"function_call", "call_id":"call", "name":"vendor.tool/雪", "arguments":"{}"});
        let parts = final_parts(&item).unwrap();
        assert_eq!(parts[0].tool_call_ref().unwrap().name(), "vendor.tool/雪");
        for (id, name) in [("", "tool"), ("call", "")] {
            item["call_id"] = json!(id);
            item["name"] = json!(name);
            assert_eq!(
                final_parts(&item).unwrap_err().kind,
                ProviderErrorKind::Protocol
            );
        }
    }

    #[test]
    fn function_arguments_require_a_json_object() {
        let mut item = json!({"type":"function_call", "call_id":"call", "name":"lookup"});
        for args in ["not json", "[]", "1", "\"[]\""] {
            item["arguments"] = json!(args);
            assert_eq!(
                final_parts(&item).unwrap_err().kind,
                ProviderErrorKind::Protocol
            );
        }
        for args in [json!(""), json!("null"), Value::Null] {
            item["arguments"] = args;
            assert_eq!(
                final_parts(&item).unwrap(),
                vec![BlockContent::ToolCall(
                    ToolCall::new("call", "lookup", json!({})).unwrap()
                )]
            );
        }
        item["arguments"] = json!(r#"{"query":"rust"}"#);
        assert_eq!(
            final_parts(&item).unwrap(),
            vec![BlockContent::ToolCall(
                ToolCall::new("call", "lookup", json!({"query":"rust"})).unwrap()
            )]
        );
    }
}
