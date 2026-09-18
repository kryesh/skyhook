//! Shared Chat wire shapes, independent of compatible server brands.
//! Deserialization is deliberately forgiving: compatible servers add vendor
//! fields, vary placeholder types, and encode values loosely. Fields that carry
//! output are decoded when their intent is unambiguous; everything else is ignored.
use crate::provider::backends::common::lenient_u64;
use serde::{Deserialize, Deserializer, Serialize, de::DeserializeOwned};
use serde_json::Value;

#[derive(Default)]
pub(super) struct Chunk {
    /// The response ID shared by every chunk of one completion.
    pub id: Option<String>,
    pub choice: Option<Choice>,
    pub usage: Option<Usage>,
}

#[derive(Default, Deserialize)]
pub(super) struct Choice {
    #[serde(default, deserialize_with = "delta_or_default")]
    pub delta: Delta,
    #[serde(default, deserialize_with = "lenient_string")]
    pub finish_reason: Option<String>,
    // Non-streaming Chat output, sent by some servers in place of deltas.
    #[serde(default, deserialize_with = "lenient_delta")]
    pub message: Option<Delta>,
    // Legacy Completions output.
    #[serde(default, deserialize_with = "lenient_string")]
    pub text: Option<String>,
}

#[derive(Default, Deserialize)]
pub(super) struct Delta {
    #[serde(default, deserialize_with = "lenient_text")]
    pub content: Option<String>,
    #[serde(default, deserialize_with = "lenient_text")]
    pub refusal: Option<String>,
    #[serde(default, deserialize_with = "lenient_text")]
    pub reasoning_content: Option<String>,
    #[serde(default, deserialize_with = "lenient_text")]
    pub reasoning: Option<String>,
    /// Signed thinking blocks sent by some proxies; only their visible text is used.
    #[serde(default, deserialize_with = "lenient_thinking")]
    pub thinking_blocks: Option<String>,
    #[serde(default, deserialize_with = "lenient_tool_calls")]
    pub tool_calls: Option<Vec<ToolDelta>>,
    /// Legacy single function call, equivalent to a tool call at index 0.
    #[serde(default, deserialize_with = "lenient_function")]
    pub function_call: Option<Function>,
}

impl Delta {
    /// The reasoning text this delta carries. `reasoning_content` is primary;
    /// `reasoning` and thinking blocks are fallbacks for the same text.
    pub(super) fn reasoning_text(&self) -> Option<&str> {
        [
            &self.reasoning_content,
            &self.reasoning,
            &self.thinking_blocks,
        ]
        .into_iter()
        .filter_map(Option::as_deref)
        .find(|text| !text.is_empty())
    }

    /// The legacy `function_call`, as the tool call at index 0 it stands for.
    pub(super) fn legacy_call(&self) -> Option<ToolDelta> {
        self.function_call.clone().map(|function| ToolDelta {
            index: Some(0),
            id: None,
            function: Some(function),
        })
    }

    /// Whether this delta carries no output. Compatible servers vary between
    /// omitted, null, and empty placeholders, including repeated roles.
    pub(super) fn is_noop(&self) -> bool {
        [&self.content, &self.refusal]
            .into_iter()
            .all(|text| text.as_ref().is_none_or(String::is_empty))
            && self.reasoning_text().is_none()
            && self.tool_calls.as_ref().is_none_or(Vec::is_empty)
            && self.function_call.is_none()
    }
}

#[derive(Default, Deserialize)]
pub(super) struct ToolDelta {
    #[serde(default, deserialize_with = "lenient_index")]
    pub index: Option<u64>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub id: Option<String>,
    #[serde(default, deserialize_with = "lenient_function")]
    pub function: Option<Function>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct Function {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient_string"
    )]
    pub name: Option<String>,
    /// Normally a JSON text fragment; some servers send the decoded value.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "arguments_text"
    )]
    pub arguments: Option<String>,
}

#[derive(Default)]
pub(super) struct Usage {
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
}

impl Usage {
    pub(super) fn from_value(value: &Value) -> Option<Self> {
        let counter = |key: &str| value.get(key).and_then(lenient_u64);
        let usage = Self {
            prompt_tokens: counter("prompt_tokens").or_else(|| counter("input_tokens")),
            completion_tokens: counter("completion_tokens").or_else(|| counter("output_tokens")),
            cached_tokens: value
                .get("prompt_tokens_details")
                .and_then(|details| details.get("cached_tokens"))
                .and_then(lenient_u64)
                .or_else(|| counter("cache_read_input_tokens")),
        };
        (usage.prompt_tokens.is_some() || usage.completion_tokens.is_some()).then_some(usage)
    }
}

#[derive(Serialize)]
pub(super) struct Request<'a> {
    pub model: &'a str,
    pub messages: Vec<Value>,
    pub stream: bool,
    pub stream_options: StreamOptions,
}

#[derive(Serialize)]
pub(super) struct StreamOptions {
    pub include_usage: bool,
}

/// An object that deserializes as `T`; anything else is absent.
fn object_as<T: DeserializeOwned>(value: Value) -> Option<T> {
    value
        .is_object()
        .then(|| serde_json::from_value(value).ok())
        .flatten()
}

fn delta_or_default<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Delta, D::Error> {
    lenient_delta(deserializer).map(Option::unwrap_or_default)
}

fn lenient_string<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    Ok(match Value::deserialize(deserializer)? {
        Value::String(text) => Some(text),
        _ => None,
    })
}

/// Text, or an array of content parts whose `text` fields are concatenated.
fn lenient_text<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    Ok(match Value::deserialize(deserializer)? {
        Value::String(text) => Some(text),
        Value::Array(parts) => Some(
            parts
                .iter()
                .filter_map(|part| match part {
                    Value::String(text) => Some(text.as_str()),
                    part => part.get("text").and_then(Value::as_str),
                })
                .collect(),
        ),
        _ => None,
    })
}

fn lenient_thinking<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    Ok(match Value::deserialize(deserializer)? {
        Value::Array(blocks) => Some(
            blocks
                .iter()
                .filter_map(|block| block.get("thinking").and_then(Value::as_str))
                .collect(),
        ),
        _ => None,
    })
}

fn lenient_index<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<u64>, D::Error> {
    Ok(lenient_u64(&Value::deserialize(deserializer)?))
}

fn arguments_text<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    // Other JSON values stay as text so argument validation rejects them.
    Ok(match Value::deserialize(deserializer)? {
        Value::String(text) => Some(text),
        Value::Null => None,
        value => Some(value.to_string()),
    })
}

fn lenient_function<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Function>, D::Error> {
    Ok(object_as(Value::deserialize(deserializer)?))
}

fn lenient_tool_calls<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Vec<ToolDelta>>, D::Error> {
    Ok(match Value::deserialize(deserializer)? {
        Value::Array(calls) => Some(calls.into_iter().filter_map(object_as).collect()),
        _ => None,
    })
}

fn lenient_delta<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Delta>, D::Error> {
    Ok(object_as(Value::deserialize(deserializer)?))
}
