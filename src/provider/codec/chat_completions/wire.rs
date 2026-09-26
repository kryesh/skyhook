//! Shared Chat wire shapes, independent of compatible server brands.
//! Deserialization is deliberately forgiving: compatible servers add vendor
//! fields, vary placeholder types, and encode values loosely. Fields that carry
//! output are decoded when their intent is unambiguous; everything else is ignored.
use crate::provider::{
    codec::common::{is_signed, lenient, lenient_u64},
    protocol::ReplayFormat,
};
use serde::{Deserialize, Deserializer, de::DeserializeOwned};
use serde_json::{Map, Value};

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
    #[serde(default, deserialize_with = "lenient")]
    pub finish_reason: Option<String>,
    // Non-streaming Chat output, sent by some servers in place of deltas.
    #[serde(default, deserialize_with = "lenient_delta")]
    pub message: Option<Delta>,
    // Legacy Completions output.
    #[serde(default, deserialize_with = "lenient")]
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
    /// Signed thinking blocks sent by some proxies, each carrying a fragment of
    /// its `thinking` text and, in a trailing delta, its `signature`.
    #[serde(default, deserialize_with = "lenient_objects")]
    pub thinking_blocks: Option<Vec<Native<ThinkingBlock>>>,
    /// A router's reasoning objects, each addressed by `index` across deltas.
    #[serde(default, deserialize_with = "lenient_objects")]
    pub reasoning_details: Option<Vec<Native<Detail>>>,
    #[serde(default, deserialize_with = "lenient_tool_calls")]
    pub tool_calls: Option<Vec<ToolDelta>>,
    /// Legacy single function call, equivalent to a tool call at index 0.
    #[serde(default, deserialize_with = "lenient_function")]
    pub function_call: Option<Function>,
}

impl Delta {
    /// The reasoning text this delta carries. `reasoning_content` is primary;
    /// `reasoning` and the thinking blocks' fragments are fallbacks for the same
    /// text. A completing block re-sends text already carried, so it adds none.
    pub(super) fn reasoning_text(&self) -> Option<String> {
        [&self.reasoning_content, &self.reasoning]
            .into_iter()
            .filter_map(Option::as_deref)
            .find(|text| !text.is_empty())
            .map(str::to_owned)
            .or_else(|| {
                let text: String = self
                    .thinking_blocks
                    .iter()
                    .flatten()
                    .filter(|block| !block.view.completes())
                    .filter_map(|block| block.view.text())
                    .collect();
                (!text.is_empty()).then_some(text)
            })
    }

    /// Whether the native reasoning field `format` replays carries anything:
    /// thinking text or a block completion, or a detail entry of any kind.
    fn has_native_reasoning(&self, format: ReplayFormat) -> bool {
        match format {
            ReplayFormat::ChatThinkingBlock => self.thinking_blocks.iter().flatten().any(|block| {
                block.view.completes() || block.view.text().is_some_and(|text| !text.is_empty())
            }),
            ReplayFormat::ChatReasoningDetail => self
                .reasoning_details
                .as_ref()
                .is_some_and(|details| !details.is_empty()),
            _ => false,
        }
    }

    /// The legacy `function_call`, as the tool call at index 0 it stands for.
    pub(super) fn legacy_call(&self) -> Option<ToolDelta> {
        self.function_call.clone().map(|function| ToolDelta {
            index: Some(0),
            id: None,
            function: Some(function),
        })
    }

    /// Whether this delta carries no output a decoder of `format` reads.
    /// Compatible servers vary between omitted, null, and empty placeholders,
    /// including repeated roles.
    pub(super) fn is_noop(&self, format: ReplayFormat) -> bool {
        [&self.content, &self.refusal]
            .into_iter()
            .all(|text| text.as_ref().is_none_or(String::is_empty))
            && self.reasoning_text().is_none()
            && !self.has_native_reasoning(format)
            && self.tool_calls.as_ref().is_none_or(Vec::is_empty)
            && self.function_call.is_none()
    }
}

/// A native reasoning object: its typed view, and the object itself for replay.
pub(super) struct Native<T> {
    pub view: T,
    pub raw: Map<String, Value>,
}

/// A `thinking_blocks` entry.
pub(super) enum ThinkingBlock {
    /// Thinking text, sent in fragments. A trailing entry carries the
    /// signature, and with it the block's text so far or an empty marker.
    Thinking {
        text: Option<String>,
        signature: Option<String>,
    },
    /// Opaque redacted thinking, whole on arrival.
    Redacted { data: String },
    /// An entry of another type, or redacted thinking without its data;
    /// ignored.
    Other,
}

impl ParseNative for ThinkingBlock {
    fn parse(raw: &Map<String, Value>) -> Self {
        let field = |name| raw.get(name).and_then(Value::as_str).map(str::to_owned);
        match (raw.get("type").and_then(Value::as_str), field("data")) {
            (Some("redacted_thinking"), Some(data)) => Self::Redacted { data },
            (Some("thinking") | None, _) => Self::Thinking {
                text: field("thinking"),
                signature: field("signature"),
            },
            _ => Self::Other,
        }
    }
}

impl ThinkingBlock {
    pub(super) fn text(&self) -> Option<&str> {
        match self {
            Self::Thinking { text, .. } => text.as_deref(),
            Self::Redacted { .. } | Self::Other => None,
        }
    }

    /// Whether this entry ends its block: a signature, even an empty one, or
    /// redacted data. The next fragment opens another block.
    pub(super) fn completes(&self) -> bool {
        matches!(
            self,
            Self::Thinking {
                signature: Some(_),
                ..
            } | Self::Redacted { .. }
        )
    }

    pub(super) fn is_signed(&self) -> bool {
        match self {
            Self::Thinking { signature, .. } => is_signed(signature.as_deref(), None),
            Self::Redacted { data } => is_signed(None, Some(data)),
            Self::Other => false,
        }
    }
}

/// What a `reasoning_details` entry holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DetailKind {
    /// `reasoning.text`, and entries of unknown type.
    Text,
    Summary,
    Encrypted,
}

impl DetailKind {
    /// The field carrying the entry's readable text.
    pub(super) fn text_field(self) -> &'static str {
        match self {
            Self::Summary => "summary",
            Self::Text | Self::Encrypted => "text",
        }
    }
}

/// A `reasoning_details` entry, a fragment of the detail at `index`.
pub(super) struct Detail {
    pub kind: DetailKind,
    pub index: Option<u64>,
    pub text: Option<String>,
    pub signed: bool,
}

impl ParseNative for Detail {
    fn parse(raw: &Map<String, Value>) -> Self {
        let field = |name| raw.get(name).and_then(Value::as_str);
        let kind = match field("type") {
            Some("reasoning.summary") => DetailKind::Summary,
            Some("reasoning.encrypted") => DetailKind::Encrypted,
            _ => DetailKind::Text,
        };
        Self {
            kind,
            index: raw.get("index").and_then(lenient_u64),
            text: field(kind.text_field()).map(str::to_owned),
            signed: is_signed(field("signature"), field("data")),
        }
    }
}

#[derive(Default, Deserialize)]
pub(super) struct ToolDelta {
    #[serde(default, deserialize_with = "lenient_index")]
    pub index: Option<u64>,
    #[serde(default, deserialize_with = "lenient")]
    pub id: Option<String>,
    #[serde(default, deserialize_with = "lenient_function")]
    pub function: Option<Function>,
}

#[derive(Clone, Default, Deserialize)]
pub(super) struct Function {
    #[serde(default, deserialize_with = "lenient")]
    pub name: Option<String>,
    /// Normally a JSON text fragment; some servers send the decoded value.
    #[serde(default, deserialize_with = "arguments_text")]
    pub arguments: Option<String>,
}

#[derive(Default)]
pub(super) struct Usage {
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
}

impl Usage {
    pub(super) fn from_value(value: &Value) -> Option<Self> {
        let counter = |key: &str| value.get(key).and_then(lenient_u64);
        let detail = |key: &str| {
            value
                .get("prompt_tokens_details")
                .and_then(|details| details.get(key))
                .and_then(lenient_u64)
        };
        let usage = Self {
            prompt_tokens: counter("prompt_tokens").or_else(|| counter("input_tokens")),
            completion_tokens: counter("completion_tokens").or_else(|| counter("output_tokens")),
            cached_tokens: detail("cached_tokens").or_else(|| counter("cache_read_input_tokens")),
            cache_write_tokens: detail("cache_write_tokens")
                .or_else(|| counter("cache_creation_input_tokens")),
        };
        (usage.prompt_tokens.is_some() || usage.completion_tokens.is_some()).then_some(usage)
    }
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

/// Native reasoning objects typed by `parse`; other entries are ignored.
fn lenient_objects<'de, D: Deserializer<'de>, T: ParseNative>(
    deserializer: D,
) -> Result<Option<Vec<Native<T>>>, D::Error> {
    Ok(match Value::deserialize(deserializer)? {
        Value::Array(blocks) => Some(
            blocks
                .into_iter()
                .filter_map(|block| match block {
                    Value::Object(raw) => Some(Native {
                        view: T::parse(&raw),
                        raw,
                    }),
                    _ => None,
                })
                .collect(),
        ),
        _ => None,
    })
}

/// A typed view of a native reasoning object.
trait ParseNative {
    fn parse(raw: &Map<String, Value>) -> Self;
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
