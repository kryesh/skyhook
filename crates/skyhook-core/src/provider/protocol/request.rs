use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::Message;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SystemSegment {
    pub text: String,
    #[serde(default)]
    pub cache: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ModelRequest {
    pub model: String,
    pub system: Vec<SystemSegment>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub reasoning: Option<String>,
    pub max_output_tokens: Option<u64>,
    pub correlation: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}
