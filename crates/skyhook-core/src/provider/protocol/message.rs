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

impl Message {
    /// An assistant message that yields no content blocks at all, and so cannot be
    /// encoded into a later request: Anthropic rejects it outright, and Chat/Responses
    /// drop it silently. It must never reach the append-only journal, because a
    /// committed one makes every subsequent request fail.
    ///
    /// Emptiness here is structural, never a judgement about text. An empty or
    /// whitespace-only text block is content: providers legitimately emit one
    /// alongside tool calls on a non-final turn, and it replays without complaint.
    /// Reasoning is content whenever it carries replay state, or a block that an
    /// encoder may render.
    #[must_use]
    pub fn is_content_free(&self) -> bool {
        match self {
            Self::Assistant(items) => items
                .iter()
                .all(|item| item.blocks.is_empty() && item.replay.is_none()),
            Self::User(_) | Self::Tool(_) => false,
        }
    }

    /// Drop replay bound to the conversation that produced it, keeping display text. Changing
    /// that conversation, as compaction does, invalidates such replay; other replay is kept.
    pub fn strip_bound_reasoning(&mut self) {
        if let Self::Assistant(items) = self {
            for item in items {
                if item
                    .replay
                    .as_ref()
                    .is_some_and(|replay| replay.conversation_bound)
                {
                    item.replay = None;
                }
            }
        }
    }
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
    /// Signed state bound to the exact conversation before it; changing that history, as
    /// compaction does, invalidates it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub conversation_bound: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
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
mod content_free_tests {
    use super::*;
    use serde_json::json;

    fn envelope() -> ReplayEnvelope {
        ReplayEnvelope {
            version: 1,
            protocol: "anthropic".into(),
            model: "model".into(),
            scope: "scope".into(),
            payload: json!({"type":"thinking","thinking":"private","signature":"signed"}),
            conversation_bound: true,
        }
    }

    /// An item whose blocks were all dropped, as a malformed decode would leave it.
    fn blockless(kind: ItemKind, replay: Option<ReplayEnvelope>) -> AssistantItem {
        AssistantItem {
            id: "item".into(),
            position: 0,
            kind,
            blocks: Vec::new(),
            replay,
        }
    }

    #[test]
    fn content_free_means_no_blocks_at_all_not_empty_text() {
        // The only unencodable shapes: no items, or items carrying no blocks and
        // no replay state.
        assert!(Message::Assistant(Vec::new()).is_content_free());
        for kind in [ItemKind::Text, ItemKind::Reasoning, ItemKind::ToolCall] {
            assert!(Message::Assistant(vec![blockless(kind, None)]).is_content_free());
        }
        // Only assistant messages can be content-free.
        assert!(!Message::User(Vec::new()).is_content_free());
        assert!(!Message::Tool(Vec::new()).is_content_free());
    }

    #[test]
    fn empty_and_whitespace_text_blocks_remain_content() {
        // Providers emit an empty or blank text block alongside tool calls on a
        // non-final turn. Such a block encodes and replays, so it is not a failure.
        for text in ["", " ", "\n\t "] {
            let item = AssistantItem::text("answer", 0, text);
            assert!(!Message::Assistant(vec![item]).is_content_free());
        }
        let call = ToolCall::new("call", "shell", json!({})).unwrap();
        let non_final = vec![
            AssistantItem::text("answer", 0, ""),
            AssistantItem::tool_call("tool-1", 1, call),
        ];
        assert!(!Message::Assistant(non_final).is_content_free());
        // Reasoning is content through a rendered block or through replay state.
        let blank_prose = AssistantItem::reasoning("thought", 0, "   ", None);
        assert!(!Message::Assistant(vec![blank_prose]).is_content_free());
        let signed = blockless(ItemKind::Reasoning, Some(envelope()));
        assert!(!Message::Assistant(vec![signed]).is_content_free());
        let visible = AssistantItem::text("answer", 0, "hello");
        assert!(!Message::Assistant(vec![visible]).is_content_free());
    }
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
