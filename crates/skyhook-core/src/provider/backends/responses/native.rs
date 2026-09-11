//! Native item shapes and strict field/argument validation.
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Text,
    Reasoning,
    Function,
}

pub(super) fn kind(item: &Value) -> Result<Kind, ProviderError> {
    match string(item, "type")? {
        "message" => Ok(Kind::Text),
        "reasoning" => Ok(Kind::Reasoning),
        "function_call" => Ok(Kind::Function),
        _ => Err(protocol("unsupported output item type")),
    }
}

pub(super) fn final_parts(item: &Value) -> Result<Vec<BlockContent>, ProviderError> {
    match kind(item)? {
        Kind::Text => {
            if string(item, "role")? != "assistant" {
                return Err(protocol("output message role is not assistant"));
            }
            array(item, "content")?
                .iter()
                .map(|part| {
                    let text = match string(part, "type")? {
                        "output_text" => string(part, "text")?,
                        "refusal" => string(part, "refusal")?,
                        _ => return Err(protocol("unsupported message content")),
                    };
                    Ok(BlockContent::Text { text: text.into() })
                })
                .collect()
        }
        Kind::Reasoning => Ok(reasoning_parts(item)?.into_values().collect()),
        Kind::Function => {
            let arguments = arguments(string(item, "arguments")?)?;
            let id = string(item, "call_id")?;
            let name = string(item, "name")?;
            if id.is_empty() || name.is_empty() {
                return Err(protocol("empty function call ID or name"));
            }
            Ok(vec![BlockContent::ToolCall(ToolCall {
                id: id.into(),
                name: name.into(),
                arguments,
            })])
        }
    }
}

/// Executable function arguments must decode to an object, not an arbitrary JSON value.
pub(super) fn arguments(text: &str) -> Result<Value, ProviderError> {
    let value: Value =
        serde_json::from_str(text).map_err(|_| protocol("invalid function arguments JSON"))?;
    if !value.is_object() {
        return Err(protocol("function arguments must be a JSON object"));
    }
    Ok(value)
}

impl Kind {
    pub(super) fn item_kind(self) -> ItemKind {
        match self {
            Self::Text => ItemKind::Text,
            Self::Reasoning => ItemKind::Reasoning,
            Self::Function => ItemKind::ToolCall,
        }
    }
    pub(super) fn block_kind(self) -> BlockKind {
        match self {
            Self::Text => BlockKind::Text,
            Self::Reasoning => BlockKind::Reasoning,
            Self::Function => BlockKind::ToolCallArguments,
        }
    }
    pub(super) fn part_id(self, position: usize) -> String {
        match self {
            Self::Text => format!("content_{position}"),
            Self::Reasoning if position.is_multiple_of(2) => format!("summary_{}", position / 2),
            Self::Reasoning => format!("content_{}", position / 2),
            Self::Function => format!("arguments_{position}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn function_arguments_require_a_json_object() {
        let mut item = json!({"type":"function_call", "call_id":"call", "name":"lookup"});
        for args in ["", "not json", "[]", "null"] {
            item["arguments"] = json!(args);
            assert_eq!(
                final_parts(&item).unwrap_err().kind,
                ProviderErrorKind::Protocol
            );
        }
        item["arguments"] = json!(r#"{"query":"rust"}"#);
        assert_eq!(
            final_parts(&item).unwrap(),
            vec![BlockContent::ToolCall(ToolCall {
                id: "call".into(),
                name: "lookup".into(),
                arguments: json!({"query":"rust"}),
            })]
        );
    }
}
