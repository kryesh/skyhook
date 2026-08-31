use serde::{Deserialize, Serialize};

use super::AssistantContent;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseChunk {
    MessageStart { model: String },
    TextDelta { text: String },
    ReasoningDelta { text: String },
    ToolInputDelta { name: String, partial_json: String },
    Block { block: AssistantContent },
    Usage { usage: Usage },
    Diagnostic { detail: String, dropped_frames: u32 },
    Done { stop_reason: Option<StopReason> },
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Complete,
    ToolUse,
    MaxTokens,
    Refusal,
    Other,
}
