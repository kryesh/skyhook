//! Native OpenAI Responses wire codec, shared with Codex.
use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use super::common::{Finish, replay};
#[cfg(test)]
use crate::provider::ProviderErrorKind;
use crate::provider::{
    ProviderError,
    protocol::{
        AssistantItem, Binding, BlockId, BlockRef, CutReason, ItemId, ItemKind, ResponseEvent,
        Scope, TextBlock, ToolCall, Usage,
    },
};

mod decoder;
mod encoder;
mod events;
mod native;
mod normalization;
mod reasoning;
mod terminal;

use decoder::api_error;
pub(crate) use encoder::encode;
use native::{arguments, array, final_parts, index, kind, protocol, string};
use reasoning::{readable_reasoning, reasoning_parts, reasoning_position, reasoning_text};

/// A part's authoritative content, as the wire states it.
#[derive(Clone, Debug, PartialEq)]
enum Content {
    Text { text: String },
    Reasoning { text: String },
    ToolCall(ToolCall),
}

impl Content {
    fn text(&self) -> Option<&str> {
        match self {
            Self::Text { text } | Self::Reasoning { text } => Some(text),
            Self::ToolCall(_) => None,
        }
    }

    #[cfg(test)]
    fn tool_call_ref(&self) -> Option<&ToolCall> {
        match self {
            Self::ToolCall(call) => Some(call),
            _ => None,
        }
    }
}

enum Part {
    Streaming { text: String, added: bool },
    Completed(Content),
}
impl Default for Part {
    fn default() -> Self {
        Self::Streaming {
            text: String::new(),
            added: false,
        }
    }
}
impl Part {
    fn ended(&self) -> Option<&Content> {
        match self {
            Self::Completed(content) => Some(content),
            Self::Streaming { .. } => None,
        }
    }
    fn streamed(&self) -> &str {
        match self {
            Self::Streaming { text, .. } => text,
            Self::Completed(_) => "",
        }
    }
}

#[derive(Default)]
struct TextState {
    parts: BTreeMap<usize, Part>,
    snapshot: Option<Value>,
}
#[derive(Default)]
struct ReasoningState {
    parts: BTreeMap<usize, Part>,
    // Snapshot namespace migrations resolve to the original display block ID.
    aliases: BTreeMap<usize, usize>,
    snapshot: Option<Value>,
}

/// A malformed/partial arguments.done is retained until the stop reason is
/// known. Validated objects are never converted back to strings for checking.
enum FinalArguments {
    Object(serde_json::Map<String, Value>),
    Incomplete(String),
}
#[derive(Default)]
struct StreamingFunction {
    call_id: Option<String>,
    name: Option<String>,
    final_arguments: Option<FinalArguments>,
}
enum FunctionPhase {
    Streaming(StreamingFunction),
    Provisional(Value),
    Completed { native: Value, call: ToolCall },
}
impl Default for FunctionPhase {
    fn default() -> Self {
        Self::Streaming(StreamingFunction::default())
    }
}
#[derive(Default)]
struct FunctionState {
    // Function items have exactly one possible block, at position zero.
    part: Option<Part>,
    phase: FunctionPhase,
}
/// Variant accessors whose mismatch is a protocol error.
macro_rules! variant_accessors {
    ($field:ident, $message:literal, $($enum:ident::$variant:ident => $name:ident, $name_mut:ident: $ty:ty;)+) => {$(
        fn $name(&self) -> Result<&$ty, ProviderError> {
            match &self.$field {
                $enum::$variant(state) => Ok(state),
                _ => Err(protocol($message)),
            }
        }
        fn $name_mut(&mut self) -> Result<&mut $ty, ProviderError> {
            match &mut self.$field {
                $enum::$variant(state) => Ok(state),
                _ => Err(protocol($message)),
            }
        }
    )+};
}

impl FunctionState {
    variant_accessors!(phase, "event after output item ended",
        FunctionPhase::Streaming => streaming, streaming_mut: StreamingFunction;);
    fn snapshot(&self) -> Option<&Value> {
        match &self.phase {
            FunctionPhase::Streaming(_) => None,
            FunctionPhase::Provisional(native) | FunctionPhase::Completed { native, .. } => {
                Some(native)
            }
        }
    }
}

enum ItemBody {
    Text(TextState),
    Reasoning(ReasoningState),
    Function(FunctionState),
}
struct Item {
    native_id: ItemId,
    aliases: BTreeSet<String>,
    wire_index: Option<usize>,
    body: ItemBody,
}
impl Item {
    fn is(&self, native_id: &str) -> bool {
        self.native_id.as_str() == native_id || self.aliases.contains(native_id)
    }

    fn block_ref(&self, position: usize) -> BlockRef {
        BlockRef {
            item: self.native_id.clone(),
            block: BlockId::try_from(self.kind().part_id(position)).expect("part ids are nonblank"),
        }
    }

    /// The complete item at the terminal event. Every part must have settled by
    /// then; a function call is either executable or a failed response.
    fn build(
        &self,
        index: usize,
        model: &str,
        scope: &Scope,
    ) -> Result<AssistantItem, ProviderError> {
        let id = self.native_id.clone();
        let position = crate::provider::backends::common::position(index)?;
        let blocks = |parts: &BTreeMap<usize, Part>| {
            parts
                .iter()
                .map(|(part, content)| {
                    let text = match content {
                        Part::Completed(content) => content
                            .text()
                            .ok_or_else(|| protocol("content part is not text"))?,
                        Part::Streaming { .. } => {
                            return Err(protocol("output item ended with an open content part"));
                        }
                    };
                    Ok(TextBlock {
                        id: self.block_ref(*part).block,
                        position: crate::provider::backends::common::position(*part)?,
                        text: text.to_owned(),
                    })
                })
                .collect::<Result<Vec<_>, ProviderError>>()
        };
        Ok(match &self.body {
            ItemBody::Text(state) => AssistantItem::Text {
                id,
                position,
                blocks: blocks(&state.parts)?,
            },
            ItemBody::Reasoning(state) => AssistantItem::Reasoning {
                id,
                position,
                blocks: blocks(&state.parts)?,
                replay: state
                    .snapshot
                    .as_ref()
                    .map(|native| replay("responses", model, scope, native.clone(), Binding::Free)),
            },
            ItemBody::Function(state) => match &state.phase {
                FunctionPhase::Completed { call, .. } => AssistantItem::ToolCall {
                    id,
                    position,
                    call: call.clone(),
                },
                // A malformed call held for the stop reason fails a normal finish.
                FunctionPhase::Provisional(native) => {
                    native::function_call(native)?;
                    return Err(protocol("conflicting final output item"));
                }
                FunctionPhase::Streaming(_) => {
                    return Err(protocol("terminal response omitted a streamed output item"));
                }
            },
        })
    }

    fn kind(&self) -> ItemKind {
        match self.body {
            ItemBody::Text(_) => ItemKind::Text,
            ItemBody::Reasoning(_) => ItemKind::Reasoning,
            ItemBody::Function(_) => ItemKind::ToolCall,
        }
    }
    fn snapshot(&self) -> Option<&Value> {
        match &self.body {
            ItemBody::Text(state) => state.snapshot.as_ref(),
            ItemBody::Reasoning(state) => state.snapshot.as_ref(),
            ItemBody::Function(state) => state.snapshot(),
        }
    }
    variant_accessors!(body, "event does not match output item kind",
        ItemBody::Function => function, function_mut: FunctionState;
        ItemBody::Reasoning => reasoning, reasoning_mut: ReasoningState;);
    fn parts(&self) -> impl Iterator<Item = (&usize, &Part)> {
        let (parts, function) = match &self.body {
            ItemBody::Text(state) => (Some(&state.parts), None),
            ItemBody::Reasoning(state) => (Some(&state.parts), None),
            ItemBody::Function(state) => (None, state.part.as_ref()),
        };
        parts
            .into_iter()
            .flat_map(|parts| parts.iter())
            .chain(function.map(|part| (&0, part)))
    }
}

pub(crate) struct Decoder {
    // Codex sends completed items separately and an empty terminal output array.
    allow_omitted_terminal_output: bool,
    model: String,
    scope: Scope,
    items: BTreeMap<usize, Item>,
    completed: bool,
}

#[cfg(test)]
mod fixtures {
    use super::*;
    pub(super) use crate::provider::backends::common::tests::{Reduced, reduce, scope};
    pub(super) use crate::provider::protocol::Outcome;

    pub(super) fn completed(output: Vec<Value>) -> Value {
        json!({"type":"response.completed", "response":{"status":"completed", "output":output,
            "usage":{"input_tokens":20, "output_tokens":7, "input_tokens_details":{"cached_tokens":12}}}})
    }

    pub(super) fn done(index: usize, item: Value) -> Value {
        json!({"type":"response.output_item.done", "output_index":index, "item":item})
    }

    pub(super) fn added(index: usize, item: Value) -> Value {
        json!({"type":"response.output_item.added", "output_index":index, "item":item})
    }

    pub(super) fn function(id: &str, call_id: &str, arguments: &str) -> Value {
        json!({"type":"function_call", "id":id, "call_id":call_id,
            "name":"lookup", "arguments":arguments, "status":"completed"})
    }

    pub(super) fn call_item() -> Value {
        json!({"type":"function_call", "id":"fc_1", "call_id":"call_1", "name":"search",
            "arguments":"{\"query\":\"rust\"}", "status":"completed"})
    }

    pub(super) fn reasoning(id: &str, text: &str) -> Value {
        json!({"type":"reasoning", "id":id,
            "summary":[{"type":"summary_text", "text":text}]})
    }

    pub(super) fn reasoning_item() -> Value {
        json!({"type":"reasoning", "id":"rs_1", "encrypted_content":"secret",
            "summary":[{"type":"summary_text", "text":"first"}, {"type":"summary_text", "text":"second"}]})
    }

    pub(super) fn message(id: &str, text: &str) -> Value {
        json!({"type":"message", "id":id, "role":"assistant", "status":"completed",
            "content":[{"type":"output_text", "text":text, "annotations":[]}]})
    }

    pub(super) fn decoder() -> Decoder {
        Decoder::new("test-model".into(), scope())
    }

    /// Feed every event, then reduce as the runtime would.
    pub(super) fn assemble(events: Vec<Value>) -> Result<Reduced, ProviderError> {
        assemble_with(decoder(), events)
    }

    pub(super) fn assemble_with(
        mut decoder: Decoder,
        frames: Vec<Value>,
    ) -> Result<Reduced, ProviderError> {
        let mut events = Vec::new();
        for frame in frames {
            events.extend(decoder.feed(frame)?);
        }
        events.extend(decoder.finish()?);
        Ok(reduce(events))
    }
}
