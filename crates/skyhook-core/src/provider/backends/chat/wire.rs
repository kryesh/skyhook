//! Shared Chat wire shapes, independent of compatible server brands.
//! Deserialization captures shape; the codec explicitly validates semantics.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Deserialize)]
pub(super) struct Chunk {
    pub object: Option<String>,
    pub choices: Vec<Choice>,
    pub usage: Option<Usage>,
}

/// Option<u64> would conflate an explicitly null index with omission.
/// Only omission is allowed, after validating that choices is a singleton.
#[derive(Default, Deserialize)]
#[serde(untagged)]
pub(super) enum ChoiceIndex {
    Number(u64),
    Null(()),
    #[default]
    #[serde(skip)]
    Missing,
}

#[derive(Deserialize)]
pub(super) struct Choice {
    #[serde(default)]
    pub index: ChoiceIndex,
    pub delta: Delta,
    pub finish_reason: Option<String>,
}

#[derive(Default, Deserialize)]
pub(super) struct Delta {
    pub role: Option<String>,
    pub content: Option<String>,
    pub refusal: Option<String>,
    pub reasoning_content: Option<String>,
    pub reasoning: Option<String>,
    pub tool_calls: Option<Vec<ToolDelta>>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ToolDelta {
    pub index: u64,
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub function: Option<Function>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Function {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: Option<u64>,
    pub prompt_tokens_details: Option<PromptDetails>,
}

#[derive(Deserialize)]
pub(super) struct PromptDetails {
    pub cached_tokens: Option<u64>,
}

#[derive(Serialize)]
pub(super) struct Request<'a> {
    pub model: &'a str,
    pub messages: Vec<Value>,
    pub n: u8,
    pub stream: bool,
    pub stream_options: StreamOptions,
}

#[derive(Serialize)]
pub(super) struct StreamOptions {
    pub include_usage: bool,
}
