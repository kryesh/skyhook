//! Native OpenAI Responses wire codec, shared by HTTP/SSE and Codex WebSocket.
use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use super::common::reasoning_envelope;
use crate::provider::{
    ProviderError, ProviderErrorKind,
    protocol::{
        BlockContent, BlockKind, ContentDelta, ItemKind, ResponseChunk, StopReason, ToolCall, Usage,
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
use native::{Kind, arguments, array, final_parts, index, kind, protocol, string};
use reasoning::{readable_reasoning, reasoning_parts, reasoning_position, reasoning_text};

#[derive(Default)]
struct Part {
    streamed: String,
    ended: Option<BlockContent>,
    added: bool,
}

struct Item {
    native_id: String,
    aliases: BTreeSet<String>,
    wire_index: Option<usize>,
    kind: Kind,
    ended: Option<Value>,
    parts: BTreeMap<usize, Part>,
    // Snapshot namespace migrations resolve to the original display block ID.
    reasoning_aliases: BTreeMap<usize, usize>,
    call_id: Option<String>,
    name: Option<String>,
    final_arguments: Option<String>,
}

pub(crate) struct Decoder {
    // Codex sends completed items separately and an empty terminal output array.
    allow_omitted_terminal_output: bool,
    model: String,
    items: BTreeMap<usize, Item>,
    completed: bool,
}
