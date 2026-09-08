use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::media::ImageReference;

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
    Image { image: ImageReference },
    Runtime { text: String },
    ParentInput { text: String },
    Compaction { text: String },
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

    /// Concatenates visible text without flattening the stored item boundaries.
    pub fn text_content(&self) -> Option<String> {
        (self.kind == ItemKind::Text).then(|| {
            self.blocks
                .iter()
                .filter_map(|b| b.content.text_content())
                .collect()
        })
    }

    pub fn reasoning_content(&self) -> Option<String> {
        (self.kind == ItemKind::Reasoning).then(|| {
            self.blocks
                .iter()
                .filter_map(|b| b.content.reasoning_content())
                .collect()
        })
    }

    pub fn tool_call_ref(&self) -> Option<&ToolCall> {
        self.blocks.iter().find_map(|b| b.content.tool_call_ref())
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

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ToolResult {
    pub call_id: String,
    pub name: String,
    pub result: Value,
    #[serde(default)]
    pub images: Vec<ImageReference>,
    #[serde(default)]
    pub is_error: bool,
}
