use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::media::{AttachmentRef, ImageRef};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "role", content = "content", rename_all = "snake_case")]
pub enum Message {
    User(Vec<UserContent>),
    Assistant(Vec<AssistantItem>),
    Tool(Vec<ToolResult>),
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UserContent {
    Text { text: String },
    Attachment { attachment: AttachmentRef },
    Runtime { text: String },
    ParentInput { text: String },
    Compaction { text: String },
}

impl UserContent {
    #[must_use]
    pub fn is_image(&self) -> bool {
        matches!(
            self,
            Self::Attachment {
                attachment: AttachmentRef::Image(_)
            }
        )
    }
}

/// A provider-native item. Its blocks and replay state must remain grouped.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct AssistantItem {
    pub id: String,
    pub position: usize,
    pub kind: ItemKind,
    pub blocks: Vec<AssistantBlock>,
    pub replay: Option<ReplayEnvelope>,
}

/// Migration alias; assistant content is now an item, never a flat block.
pub type AssistantContent = AssistantItem;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct AssistantBlock {
    pub id: String,
    pub position: usize,
    pub content: BlockContent,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    Text,
    Reasoning,
    ToolCall,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BlockKind {
    Text,
    Reasoning,
    ToolCallArguments,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BlockContent {
    Text { text: String },
    Reasoning { text: String },
    ToolCall(ToolCall),
}

/// Opaque native replay state, attached exactly once to its owning item.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ReplayEnvelope {
    pub version: u32,
    pub protocol: String,
    pub model: String,
    pub scope: String,
    pub payload: Value,
}

impl AssistantItem {
    pub fn text(id: impl Into<String>, position: usize, text: impl Into<String>) -> Self {
        Self::single(
            id.into(),
            position,
            ItemKind::Text,
            BlockContent::Text { text: text.into() },
            None,
        )
    }

    pub fn reasoning(
        id: impl Into<String>,
        position: usize,
        text: impl Into<String>,
        replay: Option<ReplayEnvelope>,
    ) -> Self {
        Self::single(
            id.into(),
            position,
            ItemKind::Reasoning,
            BlockContent::Reasoning { text: text.into() },
            replay,
        )
    }

    pub fn tool_call(id: impl Into<String>, position: usize, call: ToolCall) -> Self {
        Self::single(
            id.into(),
            position,
            ItemKind::ToolCall,
            BlockContent::ToolCall(call),
            None,
        )
    }

    fn single(
        id: String,
        position: usize,
        kind: ItemKind,
        content: BlockContent,
        replay: Option<ReplayEnvelope>,
    ) -> Self {
        let block = AssistantBlock {
            id: format!("{id}:0"),
            position: 0,
            content,
        };
        Self {
            id,
            position,
            kind,
            blocks: vec![block],
            replay,
        }
    }

    /// Concatenates visible text without flattening stored item boundaries.
    /// Kind-mismatched blocks yield `None` rather than a partial projection.
    pub fn text_content(&self) -> Option<String> {
        (self.kind == ItemKind::Text)
            .then(|| {
                self.blocks
                    .iter()
                    .map(|b| b.content.text_content())
                    .collect()
            })
            .flatten()
    }

    pub fn reasoning_content(&self) -> Option<String> {
        (self.kind == ItemKind::Reasoning)
            .then(|| {
                self.blocks
                    .iter()
                    .map(|b| b.content.reasoning_content())
                    .collect()
            })
            .flatten()
    }

    pub fn tool_call_ref(&self) -> Option<&ToolCall> {
        match (self.kind, self.blocks.as_slice()) {
            (
                ItemKind::ToolCall,
                [
                    AssistantBlock {
                        content: BlockContent::ToolCall(call),
                        ..
                    },
                ],
            ) => Some(call),
            _ => None,
        }
    }
}

impl BlockContent {
    pub fn kind(&self) -> BlockKind {
        match self {
            Self::Text { .. } => BlockKind::Text,
            Self::Reasoning { .. } => BlockKind::Reasoning,
            Self::ToolCall(_) => BlockKind::ToolCallArguments,
        }
    }

    pub fn text_content(&self) -> Option<&str> {
        match self {
            Self::Text { text } => Some(text),
            _ => None,
        }
    }

    pub fn reasoning_content(&self) -> Option<&str> {
        match self {
            Self::Reasoning { text } => Some(text),
            _ => None,
        }
    }

    pub fn tool_call_ref(&self) -> Option<&ToolCall> {
        match self {
            Self::ToolCall(call) => Some(call),
            _ => None,
        }
    }
}

/// A completed call: identity is nonblank and arguments are a JSON object.
/// Fields are private (also through serde); provider name rules stay at their boundaries.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(try_from = "RawToolCall")]
pub struct ToolCall {
    id: String,
    name: String,
    arguments: Map<String, Value>,
}

/// Wire/journal DTO; its shape intentionally matches the original completed call.
#[derive(Deserialize)]
struct RawToolCall {
    id: String,
    name: String,
    arguments: Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ToolCallError {
    #[error("tool call ID must be nonblank")]
    EmptyId,
    #[error("tool call name must be nonblank")]
    EmptyName,
    #[error("tool call arguments must be a JSON object")]
    ArgumentsNotObject,
}

impl ToolCall {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: Value,
    ) -> Result<Self, ToolCallError> {
        let id = id.into();
        let name = name.into();
        if id.trim().is_empty() {
            return Err(ToolCallError::EmptyId);
        }
        if name.trim().is_empty() {
            return Err(ToolCallError::EmptyName);
        }
        let Value::Object(arguments) = arguments else {
            return Err(ToolCallError::ArgumentsNotObject);
        };
        Ok(Self {
            id,
            name,
            arguments,
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn arguments(&self) -> &Map<String, Value> {
        &self.arguments
    }
}

impl TryFrom<RawToolCall> for ToolCall {
    type Error = ToolCallError;

    fn try_from(raw: RawToolCall) -> Result<Self, Self::Error> {
        Self::new(raw.id, raw.name, raw.arguments)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ToolResult {
    pub call_id: String,
    pub name: String,
    pub result: Value,
    #[serde(default)]
    pub images: Vec<ImageRef>,
    #[serde(default)]
    pub is_error: bool,
}

#[cfg(test)]
mod completed_call_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn completed_call_rejects_blank_identity_and_nonobject_arguments() {
        for (id, name, error) in [
            ("", "tool", ToolCallError::EmptyId),
            (" ", "tool", ToolCallError::EmptyId),
            ("call", "", ToolCallError::EmptyName),
            ("call", "\t", ToolCallError::EmptyName),
        ] {
            assert_eq!(ToolCall::new(id, name, json!({})), Err(error));
        }
        for arguments in [Value::Null, json!(false), json!(1), json!("{}"), json!([])] {
            assert_eq!(
                ToolCall::new("call", "tool", arguments),
                Err(ToolCallError::ArgumentsNotObject),
            );
        }
    }
}
