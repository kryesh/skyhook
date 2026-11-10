//! Native item shapes and field/argument validation.

use super::*;
use crate::provider::codec::common::{arguments_field, parse_tool_arguments, tagged};
use skyhook_base::enums::NamedEnum;

/// The output item types this protocol represents.
#[derive(Clone, Copy, Debug, PartialEq, Eq, NamedEnum)]
#[serde(rename_all = "snake_case")]
pub(super) enum ItemType {
    Message,
    Reasoning,
    FunctionCall,
}

/// An output item's own account of whether it finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq, NamedEnum)]
#[serde(rename_all = "snake_case")]
pub(super) enum ItemStatus {
    Completed,
    InProgress,
    Incomplete,
}

/// The readable content part types.
#[derive(Clone, Copy, Debug, PartialEq, Eq, NamedEnum)]
#[serde(rename_all = "snake_case")]
pub(super) enum PartType {
    OutputText,
    Refusal,
    ReasoningText,
    SummaryText,
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
    /// The item says it stopped short. Leaving the status out claims nothing.
    pub(super) unfinished: bool,
}
impl<'a> NativeItem<'a> {
    pub(super) fn parse(raw: &'a Value) -> Result<Self, ProviderError> {
        let id = NATIVE.string(raw, "id")?;
        if id.is_empty() {
            return Err(NATIVE.error("empty item ID"));
        }
        let unfinished = match raw.get("status") {
            None | Some(Value::Null) => false,
            Some(status) => match status.as_str().and_then(ItemStatus::parse) {
                Some(ItemStatus::Completed) => false,
                Some(ItemStatus::InProgress | ItemStatus::Incomplete) => true,
                None => return Err(NATIVE.error("unsupported output item status")),
            },
        };
        Ok(Self {
            raw,
            id,
            kind: kind(raw)?,
            unfinished,
        })
    }
}

/// The text parts of a message or reasoning item.
pub(super) fn final_parts(item: &Value, kind: ItemKind) -> Result<Vec<String>, ProviderError> {
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
                Some(text.map(str::to_owned))
            })
            .collect(),
        ItemKind::Reasoning => Ok(reasoning_parts(item)?.into_values().collect()),
        ItemKind::ToolCall => Err(NATIVE.error("a function call has no text parts")),
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

    /// A completed call needs an identity and object arguments, but its name
    /// follows no Chat rules.
    #[test]
    fn completed_function_calls_need_an_identity_and_object_arguments() {
        let item = json!({"type":"function_call", "call_id":"call", "name":"vendor.tool/雪", "arguments":"{}"});
        assert_eq!(function_call(&item).unwrap().name(), "vendor.tool/雪");
        for (field, value) in [
            ("call_id", json!("")),
            ("name", json!("")),
            ("arguments", json!("[]")),
        ] {
            let mut item = item.clone();
            item[field] = value;
            assert_eq!(
                function_call(&item).unwrap_err().kind(),
                ProviderErrorKind::Protocol,
                "{field}"
            );
        }
        let mut item = json!({"type":"function_call", "call_id":"call", "name":"lookup"});
        for (arguments, expected) in [
            (Value::Null, json!({})),
            (json!(r#"{"query":"rust"}"#), json!({"query":"rust"})),
        ] {
            item["arguments"] = arguments;
            assert_eq!(
                function_call(&item).unwrap(),
                ToolCall::new("call", "lookup", expected).unwrap()
            );
        }
    }
}
