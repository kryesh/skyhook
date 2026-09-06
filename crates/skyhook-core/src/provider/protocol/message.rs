use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::media::ImageReference;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "role", content = "content", rename_all = "snake_case")]
pub enum Message {
    User(Vec<UserContent>),
    Assistant(Vec<AssistantContent>),
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

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssistantContent {
    Text { text: String },
    Reasoning { text: String, opaque: Option<Value> },
    ToolCall(ToolCall),
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
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub console_output: String,
    #[serde(default)]
    pub images: Vec<ImageReference>,
    #[serde(default)]
    pub is_error: bool,
}
