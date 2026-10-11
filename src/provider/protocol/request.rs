use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Entry, Message};
use crate::{
    identity::AgentId,
    media::LoadedBlobs,
    named_enum::named_enum,
    newtype::{Blank, nonblank, string_newtype},
};

string_newtype! {
    /// The model identifier a provider's API is sent: free-form, never blank.
    pub struct WireModel(Blank) = |model| nonblank("model", model);
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SystemSegment {
    pub text: String,
    #[serde(default)]
    pub cache: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ModelRequest {
    pub model: WireModel,
    pub system: Vec<SystemSegment>,
    /// The context window in send order. Everything before its first current state
    /// is, within one provider context, a prefix later requests extend until compaction
    /// replaces it, so it is safe to cache. Only a state whose request produced no
    /// response drops out of it later, since nothing is bound to it.
    pub context: Vec<Entry>,
    /// Whether later requests in this context extend this one.
    #[serde(default)]
    pub history_lifetime: HistoryLifetime,
    pub tools: Vec<ToolDefinition>,
    /// Optional JSON Schema for the final answer; reasoning remains a separate stream.
    pub response_schema: Option<ResponseSchema>,
    pub reasoning: Option<String>,
    pub max_output_tokens: Option<std::num::NonZeroU64>,
    /// Blob contents this request references, loaded by the session store for
    /// provider encoding. Never serialized.
    #[serde(skip)]
    pub blobs: LoadedBlobs,
}

impl ModelRequest {
    /// An empty request for `model`: no history, tools, schema or limits.
    #[cfg(test)]
    pub(crate) fn test(model: &str) -> Self {
        Self {
            model: model.parse().unwrap(),
            system: Vec::new(),
            context: Vec::new(),
            history_lifetime: HistoryLifetime::default(),
            tools: Vec::new(),
            response_schema: None,
            reasoning: None,
            max_output_tokens: None,
            blobs: LoadedBlobs::default(),
        }
    }

    /// The context's messages in send order, without its states.
    pub fn messages(&self) -> impl DoubleEndedIterator<Item = &Message> + Clone {
        self.context.iter().filter_map(Entry::message)
    }

    pub fn messages_mut(&mut self) -> impl DoubleEndedIterator<Item = &mut Message> {
        self.context.iter_mut().filter_map(|entry| match entry {
            Entry::Message(message) => Some(message),
            Entry::State(_) => None,
        })
    }

    /// The states in send order.
    #[cfg(test)]
    pub(crate) fn states(&self) -> impl Iterator<Item = &super::State> {
        self.context.iter().filter_map(|entry| match entry {
            Entry::State(state) => Some(state),
            Entry::Message(_) => None,
        })
    }
}

named_enum! {
    /// Whether later requests in this model context will extend this request's history.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
    pub enum HistoryLifetime {
        /// Later requests in this context extend this history.
        #[default]
        Extends = "extends",
        /// This history is sent once under settings no other request in the context shares
        /// (compaction summaries), so there is no cached prefix to read or extend.
        Detached = "detached",
    }
}

string_newtype! {
    /// Stable identity of one conversation's provider context. Providers may send it as
    /// cache-affinity metadata; it never selects history.
    pub struct ContextId(Blank) = |id| nonblank("context identity", id);
}

impl From<&AgentId> for ContextId {
    fn from(agent: &AgentId) -> Self {
        Self(agent.to_string())
    }
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
