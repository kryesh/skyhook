//! Native item shapes and field/argument validation.

use super::*;
use crate::named_enum::named_enum;
use crate::provider::codec::common::{arguments_field, parse_tool_arguments, tagged};

named_enum! {
    /// The output item types this protocol represents.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) parsed enum ItemType {
        Message = "message",
        Reasoning = "reasoning",
        FunctionCall = "function_call",
    }
}

named_enum! {
    /// The readable content part types.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) parsed enum PartType {
        OutputText = "output_text",
        Refusal = "refusal",
        ReasoningText = "reasoning_text",
        SummaryText = "summary_text",
    }
}

pub(super) fn array<'a>(value: &'a Value, key: &str) -> Result<&'a Vec<Value>, ProviderError> {
    value
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| NATIVE.error(format_args!("missing or invalid {key}")))
}

/// Items of a type this protocol does not represent (hosted tools, future
/// types) are ignored; items without a type are malformed.
pub(super) fn is_foreign(item: &Value) -> bool {
    item.get("type").is_some_and(Value::is_string) && tagged::<ItemType>(item).is_none()
}

fn kind(item: &Value) -> Result<ItemKind, ProviderError> {
    match tagged(item) {
        Some(ItemType::Message) => Ok(ItemKind::Text),
        Some(ItemType::Reasoning) => Ok(ItemKind::Reasoning),
        Some(ItemType::FunctionCall) => Ok(ItemKind::ToolCall),
        None => Err(NATIVE.error("missing or unsupported output item type")),
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
        let id = NATIVE.string(raw, "id")?;
        if id.is_empty() {
            return Err(NATIVE.error("empty item ID"));
        }
        Ok(Self {
            raw,
            id,
            kind: kind(raw)?,
        })
    }
}

pub(super) fn final_parts(item: &Value, kind: ItemKind) -> Result<Vec<Content>, ProviderError> {
    match kind {
        ItemKind::Text if item.get("role").is_some_and(|role| role != "assistant") => {
            Err(NATIVE.error("output message role is not assistant"))
        }
        // Content parts other than text and refusals carry nothing representable.
        ItemKind::Text => array(item, "content")?
            .iter()
            .filter_map(|part| {
                let text = match tagged(part)? {
                    PartType::OutputText => NATIVE.string(part, "text"),
                    PartType::Refusal => NATIVE.string(part, "refusal"),
                    PartType::ReasoningText | PartType::SummaryText => return None,
                };
                Some(text.map(|text| Content::Text { text: text.into() }))
            })
            .collect(),
        ItemKind::Reasoning => Ok(reasoning_parts(item)?.into_values().collect()),
        ItemKind::ToolCall => Ok(vec![Content::ToolCall(function_call(item)?)]),
    }
}

pub(super) fn function_call(item: &Value) -> Result<ToolCall, ProviderError> {
    let arguments = item_arguments(item)?;
    let id = NATIVE.string(item, "call_id")?;
    let name = NATIVE.string(item, "name")?;
    ToolCall::new(id, name, Value::Object(arguments)).map_err(|error| NATIVE.error(error))
}

/// A function item's arguments.
pub(super) fn item_arguments(
    item: &Value,
) -> Result<serde_json::Map<String, Value>, ProviderError> {
    arguments_field(item.get("arguments"))
        .ok_or_else(|| NATIVE.error("function arguments must be a JSON object"))
}

/// Executable function arguments must decode to an object.
pub(super) fn arguments(text: &str) -> Result<serde_json::Map<String, Value>, ProviderError> {
    parse_tool_arguments(text)
        .ok_or_else(|| NATIVE.error("function arguments must be a JSON object"))
}

impl ItemKind {
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
    use crate::provider::ProviderErrorKind;

    #[test]
    fn completed_function_identity_is_nonempty_without_chat_name_rules() {
        let mut item = json!({"type":"function_call", "call_id":"call", "name":"vendor.tool/雪", "arguments":"{}"});
        let parts = final_parts(&item, ItemKind::ToolCall).unwrap();
        let Content::ToolCall(call) = &parts[0] else {
            panic!("a tool call")
        };
        assert_eq!(call.name(), "vendor.tool/雪");
        for (id, name) in [("", "tool"), ("call", "")] {
            item["call_id"] = json!(id);
            item["name"] = json!(name);
            assert_eq!(
                final_parts(&item, ItemKind::ToolCall).unwrap_err().kind(),
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
                final_parts(&item, ItemKind::ToolCall).unwrap_err().kind(),
                ProviderErrorKind::Protocol
            );
        }
        for args in [json!(""), json!("null"), Value::Null] {
            item["arguments"] = args;
            assert_eq!(
                final_parts(&item, ItemKind::ToolCall).unwrap(),
                vec![Content::ToolCall(
                    ToolCall::new("call", "lookup", json!({})).unwrap()
                )]
            );
        }
        item["arguments"] = json!(r#"{"query":"rust"}"#);
        assert_eq!(
            final_parts(&item, ItemKind::ToolCall).unwrap(),
            vec![Content::ToolCall(
                ToolCall::new("call", "lookup", json!({"query":"rust"})).unwrap()
            )]
        );
    }
}
