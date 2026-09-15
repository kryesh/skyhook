use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::Message;
use crate::media::LoadedBlobs;

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
    /// Committed conversation. Within one context it is an unchanged prefix of every
    /// later request until compaction replaces it, so it is safe to cache.
    pub history: Vec<Message>,
    /// Content rebuilt for this request only (runtime state, instructions). Sent after
    /// history and never cacheable.
    pub tail: Vec<Message>,
    /// Whether later requests in this context extend `history`.
    #[serde(default)]
    pub history_lifetime: HistoryLifetime,
    pub tools: Vec<ToolDefinition>,
    /// Optional JSON Schema for the final answer; reasoning remains a separate stream.
    pub response_schema: Option<ResponseSchema>,
    pub reasoning: Option<String>,
    pub max_output_tokens: Option<u64>,
    pub correlation: Option<String>,
    /// Blob contents this request references, loaded by the session store for
    /// provider encoding. Never serialized.
    #[serde(skip)]
    pub blobs: LoadedBlobs,
}

impl ModelRequest {
    /// Every message in send order: history, then tail.
    pub fn messages(&self) -> impl DoubleEndedIterator<Item = &Message> + Clone {
        self.history.iter().chain(&self.tail)
    }

    pub fn messages_mut(&mut self) -> impl DoubleEndedIterator<Item = &mut Message> {
        self.history.iter_mut().chain(&mut self.tail)
    }
}

/// Whether later requests in this model context will extend this request's history.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryLifetime {
    /// Later requests in this context extend this history.
    #[default]
    Continuing,
    /// This history is replaced after this request (compaction), so caching it cannot pay off.
    Ending,
}

/// A named JSON Schema that a provider must enforce for the final response text.
/// Providers that cannot transmit this constraint must return an error.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ResponseSchema {
    pub name: String,
    pub schema: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}
