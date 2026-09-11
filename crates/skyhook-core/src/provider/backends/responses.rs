//! Native OpenAI Responses wire codec, shared by HTTP/SSE and Codex WebSocket.
use std::collections::{BTreeMap, BTreeSet};

#[cfg(test)]
mod compatibility_tests;
mod normalization;

use serde_json::{Value, json};

use crate::provider::{
    ProviderError, ProviderErrorKind,
    protocol::{
        BlockContent, BlockKind, ContentDelta, ItemKind, Message, ModelRequest, ResponseChunk,
        StopReason, ToolCall, Usage, UserContent,
    },
};

use super::common::{image_url, invalid, opaque_payload, reasoning_envelope, tool_text};

pub(crate) fn encode(request: &ModelRequest) -> Result<Value, ProviderError> {
    if request.model.trim().is_empty() {
        return Err(invalid("Responses requires a nonempty model"));
    }
    // System cache flags are cross-provider hints. OpenAI automatically caches
    // matching prefixes and has no per-segment cache-control field.
    let mut input = Vec::new();
    for message in &request.messages {
        match message {
            Message::User(parts) => {
                let content = parts
                    .iter()
                    .map(|part| match part {
                        UserContent::Text { text }
                        | UserContent::Runtime { text }
                        | UserContent::ParentInput { text }
                        | UserContent::Compaction { text } => {
                            Ok(json!({"type":"input_text", "text":text}))
                        }
                        UserContent::Image { image } => {
                            Ok(json!({"type":"input_image", "image_url":image_url(image)?}))
                        }
                    })
                    .collect::<Result<Vec<_>, ProviderError>>()?;
                input.push(json!({"role":"user", "content":content}));
            }
            Message::Assistant(items) => {
                for item in items {
                    if item.kind == ItemKind::Reasoning {
                        // Private replay belongs to the item, never to each display summary.
                        if let Some(native) =
                            opaque_payload(&item.replay, "responses", &request.model)
                        {
                            if kind(native).map_err(|error| invalid(error.message))?
                                != Kind::Reasoning
                            {
                                return Err(invalid(
                                    "Responses reasoning envelope contains a non-reasoning item",
                                ));
                            }
                            final_parts(native).map_err(|error| invalid(error.message))?;
                            input.push(native.clone());
                        }
                        continue;
                    }
                    for part in &item.blocks {
                        match &part.content {
                            BlockContent::Text { text } => input.push(json!({
                                "role":"assistant", "content":[{"type":"output_text", "text":text}]
                            })),
                            BlockContent::ToolCall(call) => {
                                if call.id.is_empty()
                                    || call.name.is_empty()
                                    || !call.arguments.is_object()
                                {
                                    return Err(invalid(
                                        "Responses function calls require call ID, name and object arguments",
                                    ));
                                }
                                input.push(json!({"type":"function_call", "call_id":call.id,
                                    "name":call.name, "arguments":call.arguments.to_string()}));
                            }
                            BlockContent::Reasoning { .. } => {}
                        }
                    }
                }
            }
            Message::Tool(results) => {
                for result in results {
                    if result.call_id.is_empty() {
                        return Err(invalid("Responses tool output requires a call ID"));
                    }
                    input.push(
                        json!({"type":"function_call_output", "call_id":result.call_id,
                        "output":tool_text(result)}),
                    );
                    if !result.images.is_empty() {
                        let mut content = vec![json!({"type":"input_text", "text":format!(
                            "Images from tool {} (call_id: {}):", result.name, result.call_id)})];
                        for image in &result.images {
                            content
                                .push(json!({"type":"input_image", "image_url":image_url(image)?}));
                        }
                        input.push(json!({"role":"user", "content":content}));
                    }
                }
            }
        }
    }
    let mut body = json!({"model":request.model, "input":input, "stream":true,
        "store":false, "include":["reasoning.encrypted_content"],
        "reasoning":{"summary":"auto"}});
    if !request.system.is_empty() {
        body["instructions"] = Value::String(
            request
                .system
                .iter()
                .map(|s| s.text.as_str())
                .collect::<Vec<_>>()
                .join("\n\n"),
        );
    }
    if !request.tools.is_empty() {
        let mut tools = Vec::new();
        for tool in &request.tools {
            if tool.name.is_empty() || !tool.input_schema.is_object() {
                return Err(invalid(
                    "Responses tools require a name and object JSON Schema",
                ));
            }
            tools.push(json!({"type":"function", "name":tool.name,
                "description":tool.description, "parameters":tool.input_schema, "strict":false}));
        }
        body["tools"] = Value::Array(tools);
    }
    if let Some(schema) = &request.response_schema {
        if schema.name.is_empty() || !schema.schema.is_object() {
            return Err(invalid(
                "Responses structured output requires a name and object JSON Schema",
            ));
        }
        body["text"] = json!({"format":{"type":"json_schema", "name":schema.name,
            "schema":schema.schema, "strict":true}});
    }
    if let Some(effort) = &request.reasoning {
        if !matches!(
            effort.as_str(),
            "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
        ) {
            return Err(invalid(format!(
                "Unsupported Responses reasoning effort: {effort}"
            )));
        }
        body["reasoning"]["effort"] = json!(effort);
    }
    if let Some(max) = request.max_output_tokens {
        if max == 0 {
            return Err(invalid("Responses max_output_tokens must be positive"));
        }
        body["max_output_tokens"] = json!(max);
    }
    if let Some(correlation) = &request.correlation {
        body["prompt_cache_key"] = json!(correlation);
    }
    Ok(body)
}

fn protocol(message: impl Into<String>) -> ProviderError {
    ProviderError::protocol(format!("Responses: {}", message.into()))
}

fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str, ProviderError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| protocol(format!("missing or invalid {key}")))
}

fn index(value: &Value, key: &str) -> Result<usize, ProviderError> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(|| protocol(format!("missing or invalid {key}")))
}

fn array<'a>(value: &'a Value, key: &str) -> Result<&'a Vec<Value>, ProviderError> {
    value
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| protocol(format!("missing or invalid {key}")))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Text,
    Reasoning,
    Function,
}

fn kind(item: &Value) -> Result<Kind, ProviderError> {
    match string(item, "type")? {
        "message" => Ok(Kind::Text),
        "reasoning" => Ok(Kind::Reasoning),
        "function_call" => Ok(Kind::Function),
        _ => Err(protocol("unsupported output item type")),
    }
}

fn final_parts(item: &Value) -> Result<Vec<BlockContent>, ProviderError> {
    match kind(item)? {
        Kind::Text => {
            if string(item, "role")? != "assistant" {
                return Err(protocol("output message role is not assistant"));
            }
            array(item, "content")?
                .iter()
                .map(|part| {
                    let text = match string(part, "type")? {
                        "output_text" => string(part, "text")?,
                        "refusal" => string(part, "refusal")?,
                        _ => return Err(protocol("unsupported message content")),
                    };
                    Ok(BlockContent::Text { text: text.into() })
                })
                .collect()
        }
        Kind::Reasoning => Ok(reasoning_parts(item)?.into_values().collect()),
        Kind::Function => {
            let arguments = arguments(string(item, "arguments")?)?;
            let id = string(item, "call_id")?;
            let name = string(item, "name")?;
            if id.is_empty() || name.is_empty() {
                return Err(protocol("empty function call ID or name"));
            }
            Ok(vec![BlockContent::ToolCall(ToolCall {
                id: id.into(),
                name: name.into(),
                arguments,
            })])
        }
    }
}

/// Readable content and summaries are independent wire namespaces. Interleaved
/// internal positions preserve both namespaces without exposing native state.
fn reasoning_position(position: usize, content: bool) -> Result<usize, ProviderError> {
    position
        .checked_mul(2)
        .and_then(|position| position.checked_add(usize::from(content)))
        .ok_or_else(|| protocol("reasoning part index overflow"))
}

fn readable_reasoning(part: &Value, summary: bool) -> Result<&str, ProviderError> {
    match (summary, string(part, "type")?) {
        (true, "summary_text") | (false, "reasoning_text" | "output_text") => {}
        _ => return Err(protocol("unsupported reasoning content part")),
    }
    reasoning_text(part)
}

/// Some readable parts use `reasoning` instead of `text`. Do not guess when
/// both fields supply conflicting values, or silently accept invalid types.
fn reasoning_text(value: &Value) -> Result<&str, ProviderError> {
    let field = |key| match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.as_str())),
        _ => Err(protocol("invalid readable reasoning text")),
    };
    match (field("text")?, field("reasoning")?) {
        (Some(text), None) | (None, Some(text)) => Ok(text),
        (Some(text), Some(reasoning)) if text == reasoning => Ok(text),
        _ => Err(protocol("missing or ambiguous readable reasoning text")),
    }
}

fn reasoning_parts(item: &Value) -> Result<BTreeMap<usize, BlockContent>, ProviderError> {
    let mut parts = BTreeMap::new();
    for (field, content) in [("summary", false), ("content", true)] {
        let values = match item.get(field) {
            None | Some(Value::Null) => continue,
            Some(Value::Array(values)) => values,
            _ => return Err(protocol(format!("invalid reasoning {field}"))),
        };
        for (position, part) in values.iter().enumerate() {
            parts.insert(
                reasoning_position(position, content)?,
                BlockContent::Reasoning {
                    text: readable_reasoning(part, !content)?.into(),
                },
            );
        }
    }
    Ok(parts)
}

/// Plaintext is reconciled separately against display blocks. Native replay is
/// always an unchanged received snapshot, not a reconstruction from those blocks.
fn native_enrichment(previous: &Value, terminal: &Value) -> bool {
    fn enrich(previous: &Value, terminal: &Value) -> bool {
        if previous == terminal || previous.is_null() {
            return true;
        }
        match (previous.as_object(), terminal.as_object()) {
            (Some(previous), Some(terminal)) => previous.iter().all(|(key, value)| {
                terminal.get(key).is_some_and(|next| {
                    (key == "encrypted_content" && value.as_str() == Some("") && next.is_string())
                        || enrich(value, next)
                })
            }),
            _ => false,
        }
    }
    match (previous.as_object(), terminal.as_object()) {
        (Some(previous), Some(terminal)) => previous.iter().all(|(key, value)| {
            // Identity aliases are checked by end(); status is lifecycle metadata.
            matches!(key.as_str(), "summary" | "content" | "id" | "status")
                || terminal.get(key).is_some_and(|next| {
                    (key == "encrypted_content" && value.as_str() == Some("") && next.is_string())
                        || enrich(value, next)
                })
        }),
        _ => false,
    }
}

fn arguments(text: &str) -> Result<Value, ProviderError> {
    let value: Value =
        serde_json::from_str(text).map_err(|_| protocol("invalid function arguments JSON"))?;
    if !value.is_object() {
        return Err(protocol("function arguments must be a JSON object"));
    }
    Ok(value)
}

impl Kind {
    fn item_kind(self) -> ItemKind {
        match self {
            Self::Text => ItemKind::Text,
            Self::Reasoning => ItemKind::Reasoning,
            Self::Function => ItemKind::ToolCall,
        }
    }
    fn block_kind(self) -> BlockKind {
        match self {
            Self::Text => BlockKind::Text,
            Self::Reasoning => BlockKind::Reasoning,
            Self::Function => BlockKind::ToolCallArguments,
        }
    }
    fn part_id(self, position: usize) -> String {
        match self {
            Self::Text => format!("content_{position}"),
            Self::Reasoning if position.is_multiple_of(2) => format!("summary_{}", position / 2),
            Self::Reasoning => format!("content_{}", position / 2),
            Self::Function => format!("arguments_{position}"),
        }
    }
}

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

impl Decoder {
    pub(crate) fn new(model: String) -> Self {
        Self {
            allow_omitted_terminal_output: false,
            model,
            items: BTreeMap::new(),
            completed: false,
        }
    }

    pub(crate) fn codex(model: String) -> Self {
        Self {
            allow_omitted_terminal_output: true,
            ..Self::new(model)
        }
    }

    pub(crate) fn decode(
        &mut self,
        event: &super::transport::SseEvent,
    ) -> Result<Vec<ResponseChunk>, ProviderError> {
        self.decode_filtered(event, |_| false)
    }

    pub(super) fn decode_filtered(
        &mut self,
        event: &super::transport::SseEvent,
        ignore: impl FnOnce(&Value) -> bool,
    ) -> Result<Vec<ResponseChunk>, ProviderError> {
        if event.data.trim() == "[DONE]" {
            return if self.completed {
                Ok(vec![])
            } else {
                Err(protocol("[DONE] before terminal response"))
            };
        }
        let value: Value =
            serde_json::from_str(&event.data).map_err(|_| protocol("invalid SSE JSON"))?;
        if ignore(&value) {
            return Ok(vec![]);
        }
        if let Some(name) = &event.event
            && !name.is_empty()
            && name != "message"
            && Some(name.as_str()) != value.get("type").and_then(Value::as_str)
        {
            return Err(protocol("SSE event name disagrees with payload type"));
        }
        self.feed(value)
    }

    fn start(
        &mut self,
        id: usize,
        item: &Value,
        chunks: &mut Vec<ResponseChunk>,
    ) -> Result<(), ProviderError> {
        if self.items.contains_key(&id) {
            return Err(protocol("duplicate output item index"));
        }
        let native_id = string(item, "id")?.to_owned();
        if native_id.is_empty()
            || self
                .items
                .values()
                .any(|old| old.native_id == native_id || old.aliases.contains(&native_id))
        {
            return Err(protocol("empty or duplicate output item ID"));
        }
        let item_kind = kind(item)?;
        self.items.insert(
            id,
            Item {
                native_id: native_id.clone(),
                aliases: BTreeSet::new(),
                wire_index: Some(id),
                kind: item_kind,
                ended: None,
                parts: BTreeMap::new(),
                reasoning_aliases: BTreeMap::new(),
                call_id: item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned),
                name: item
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned),
                final_arguments: None,
            },
        );
        chunks.push(ResponseChunk::ItemStarted {
            id: native_id,
            position: id,
            kind: item_kind.item_kind(),
        });
        Ok(())
    }

    fn part(&mut self, id: usize, position: usize, chunks: &mut Vec<ResponseChunk>) -> &mut Part {
        let item = self.items.get_mut(&id).expect("checked item");
        item.parts.entry(position).or_insert_with(|| {
            chunks.push(ResponseChunk::BlockStarted {
                item: item.native_id.clone(),
                id: item.kind.part_id(position),
                position,
                kind: item.kind.block_kind(),
            });
            Part::default()
        })
    }

    fn delta(
        &mut self,
        id: usize,
        position: usize,
        text: &str,
        chunks: &mut Vec<ResponseChunk>,
    ) -> Result<(), ProviderError> {
        let part = self.part(id, position, chunks);
        if part.ended.is_some() {
            return Err(protocol("delta after content part ended"));
        }
        part.streamed.push_str(text);
        let item = &self.items[&id];
        chunks.push(ResponseChunk::BlockDelta {
            item: item.native_id.clone(),
            block: item.kind.part_id(position),
            delta: if item.kind == Kind::Function {
                ContentDelta::JsonFragment(text.into())
            } else {
                ContentDelta::Text(text.into())
            },
        });
        Ok(())
    }

    fn close_part(
        &mut self,
        id: usize,
        position: usize,
        content: BlockContent,
        chunks: &mut Vec<ResponseChunk>,
    ) -> Result<(), ProviderError> {
        let part = self.part(id, position, chunks);
        if let Some(old) = &part.ended {
            if old != &content {
                return Err(protocol("conflicting final content part"));
            }
            return Ok(());
        }
        if !part.streamed.is_empty() {
            let valid = match &content {
                BlockContent::Text { text } | BlockContent::Reasoning { text } => {
                    text == &part.streamed
                }
                BlockContent::ToolCall(call) => {
                    arguments(&part.streamed).ok().as_ref() == Some(&call.arguments)
                }
            };
            if !valid {
                return Err(protocol("final content disagrees with streamed deltas"));
            }
        }
        part.ended = Some(content.clone());
        let item = &self.items[&id];
        chunks.push(ResponseChunk::BlockEnded {
            item: item.native_id.clone(),
            block: item.kind.part_id(position),
            content,
        });
        Ok(())
    }

    fn active(&self, event: &Value, expected: Kind) -> Result<usize, ProviderError> {
        let id = index(event, "output_index")?;
        let item = self
            .items
            .get(&id)
            .ok_or_else(|| protocol("event for an unstarted output item"))?;
        if item.ended.is_some() {
            return Err(protocol("event after output item ended"));
        }
        if item.kind != expected {
            return Err(protocol("event does not match output item kind"));
        }
        if string(event, "item_id")? != item.native_id {
            return Err(protocol("event item ID mismatch"));
        }
        Ok(id)
    }

    fn end(
        &mut self,
        id: usize,
        native: &Value,
        chunks: &mut Vec<ResponseChunk>,
        terminal: bool,
    ) -> Result<(), ProviderError> {
        let item = self
            .items
            .get(&id)
            .ok_or_else(|| protocol("end of unstarted output item"))?;
        if (string(native, "id")? != item.native_id
            && !item.aliases.contains(string(native, "id")?))
            || kind(native)? != item.kind
        {
            return Err(protocol("final output item identity changed"));
        }
        if item.kind == Kind::Reasoning {
            return self.end_reasoning(id, native, chunks, terminal);
        }
        // A tool may end with partial JSON before the terminal max-token or
        // filter reason arrives. Keep it provisional; an abnormal stop discards
        // it, while a normal terminal response must still validate its JSON.
        if !terminal
            && item.kind == Kind::Function
            && arguments(string(native, "arguments")?).is_err()
        {
            if item.ended.is_some() {
                return Err(protocol("duplicate final output item"));
            }
            self.items.get_mut(&id).expect("checked item").ended = Some(native.clone());
            return Ok(());
        }
        let parts = final_parts(native)?;
        if let Some(old) = &item.ended {
            if final_parts(old)? != parts {
                return Err(protocol("conflicting final output item"));
            }
            return Ok(());
        }
        if item.parts.keys().any(|position| *position >= parts.len()) {
            return Err(protocol("final item omitted a streamed content part"));
        }
        if item.kind == Kind::Function {
            if self.items.iter().any(|(other_id, other)| {
                *other_id != id
                    && other.kind == Kind::Function
                    && other
                        .ended
                        .as_ref()
                        .is_some_and(|other| other.get("call_id") == native.get("call_id"))
            }) {
                return Err(protocol("duplicate function call ID"));
            }
            if item
                .call_id
                .as_deref()
                .is_some_and(|id| Some(id) != native.get("call_id").and_then(Value::as_str))
                || item
                    .name
                    .as_deref()
                    .is_some_and(|name| Some(name) != native.get("name").and_then(Value::as_str))
            {
                return Err(protocol("final function identity changed"));
            }
            self.validate_arguments(id, string(native, "arguments")?)?;
        }
        for (position, content) in parts.into_iter().enumerate() {
            self.close_part(id, position, content, chunks)?;
        }
        let item = self.items.get_mut(&id).expect("checked item");
        item.ended = Some(native.clone());
        chunks.push(ResponseChunk::ItemEnded {
            id: item.native_id.clone(),
            replay: (item.kind == Kind::Reasoning)
                .then(|| reasoning_envelope("responses", &self.model, native.clone())),
        });
        Ok(())
    }

    fn end_reasoning(
        &mut self,
        id: usize,
        native: &Value,
        chunks: &mut Vec<ResponseChunk>,
        terminal: bool,
    ) -> Result<(), ProviderError> {
        let supplied = reasoning_parts(native)?;
        let item = &self.items[&id];
        if let Some(previous) = &item.ended
            && !native_enrichment(previous, native)
        {
            return Err(protocol("conflicting final reasoning state"));
        }
        // A migration alias is only valid while the snapshots present one
        // namespace. Once both are explicit, each needs its own display block.
        // Retain the original block at its original position: close_part still
        // rejects any attempt to rewrite text actually received there. The
        // formerly aliased namespace may now supply its own distinct text.
        self.items
            .get_mut(&id)
            .expect("checked item")
            .reasoning_aliases
            .retain(|position, _| {
                !(supplied.contains_key(position) && supplied.contains_key(&(position ^ 1)))
            });
        for (position, content) in &supplied {
            let item = &self.items[&id];
            let mut target = item
                .reasoning_aliases
                .get(position)
                .copied()
                .unwrap_or(*position);
            if !item.parts.contains_key(&target) && !supplied.contains_key(&(position ^ 1)) {
                // A snapshot can move the same readable text from summary to
                // content (or vice versa). Reuse its live block, but do not
                // collapse independently supplied summary/content namespaces.
                if let Some(other) = item.parts.get(&(position ^ 1)) {
                    let text_matches = match content {
                        BlockContent::Reasoning { text } => other
                            .ended
                            .as_ref()
                            .map_or(other.streamed == *text, |old| old == content),
                        _ => false,
                    };
                    if text_matches {
                        target = position ^ 1;
                        self.items
                            .get_mut(&id)
                            .expect("checked item")
                            .reasoning_aliases
                            .insert(*position, target);
                    }
                }
            }
            self.close_part(id, target, content.clone(), chunks)?;
        }
        // Missing final plaintext is not evidence that live display was wrong.
        // Close received display locally without adding it to the native replay.
        let remaining: Vec<_> = self.items[&id]
            .parts
            .iter()
            .filter(|(_, part)| part.ended.is_none())
            .map(|(position, part)| (*position, part.streamed.clone()))
            .collect();
        for (position, text) in remaining {
            self.close_part(id, position, BlockContent::Reasoning { text }, chunks)?;
        }
        let item = self.items.get_mut(&id).expect("checked item");
        item.ended = Some(native.clone());
        // Keep the display item open until the terminal snapshot: it may supply
        // additional readable content absent from output_item.done.
        if terminal {
            chunks.push(ResponseChunk::ItemEnded {
                id: item.native_id.clone(),
                replay: Some(reasoning_envelope("responses", &self.model, native.clone())),
            });
        }
        Ok(())
    }

    fn validate_arguments(&self, id: usize, text: &str) -> Result<(), ProviderError> {
        let value = arguments(text)?;
        let item = &self.items[&id];
        if let Some(old) = &item.final_arguments
            && arguments(old)? != value
        {
            return Err(protocol("conflicting final function arguments"));
        }
        if let Some(part) = item.parts.get(&0)
            && !part.streamed.is_empty()
            && arguments(&part.streamed).ok().as_ref() != Some(&value)
        {
            return Err(protocol("final function arguments disagree with deltas"));
        }
        Ok(())
    }

    /// Feed a native Responses event (also used by Codex WebSocket transport).
    pub(crate) fn feed(&mut self, mut event: Value) -> Result<Vec<ResponseChunk>, ProviderError> {
        if self.completed {
            return Err(protocol("event after terminal response"));
        }
        let mut chunks = Vec::new();
        let supplied_output_index = event
            .get("output_index")
            .map(|_| index(&event, "output_index"))
            .transpose()?;
        self.normalize_event(&mut event, &mut chunks)?;
        match string(&event, "type")? {
            "response.created" | "response.in_progress" | "response.queued" => {
                if !event.get("response").is_some_and(Value::is_object) {
                    return Err(protocol("missing response object"));
                }
            }
            "response.output_item.added" => {
                let id = index(&event, "output_index")?;
                self.start(
                    id,
                    event.get("item").ok_or_else(|| protocol("missing item"))?,
                    &mut chunks,
                )?;
                self.items.get_mut(&id).expect("started item").wire_index = supplied_output_index;
            }
            "response.output_item.done" => {
                let id = index(&event, "output_index")?;
                self.end(
                    id,
                    event.get("item").ok_or_else(|| protocol("missing item"))?,
                    &mut chunks,
                    false,
                )?;
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                let id = self.active(&event, Kind::Text)?;
                self.delta(
                    id,
                    index(&event, "content_index")?,
                    string(&event, "delta")?,
                    &mut chunks,
                )?;
            }
            "response.reasoning_text.delta" | "response.reasoning_text.done" => {
                let id = self.active(&event, Kind::Reasoning)?;
                let position = reasoning_position(index(&event, "content_index")?, true)?;
                if string(&event, "type")?.ends_with(".delta") {
                    self.delta(id, position, string(&event, "delta")?, &mut chunks)?;
                } else {
                    self.close_part(
                        id,
                        position,
                        BlockContent::Reasoning {
                            text: reasoning_text(&event)?.into(),
                        },
                        &mut chunks,
                    )?;
                }
            }
            "response.reasoning_summary_text.delta" => {
                let id = self.active(&event, Kind::Reasoning)?;
                self.delta(
                    id,
                    reasoning_position(index(&event, "summary_index")?, false)?,
                    string(&event, "delta")?,
                    &mut chunks,
                )?;
            }
            "response.function_call_arguments.delta" => {
                let id = self.active(&event, Kind::Function)?;
                if self.items[&id].final_arguments.is_some() {
                    return Err(protocol("arguments delta after done"));
                }
                self.delta(id, 0, string(&event, "delta")?, &mut chunks)?;
            }
            "response.function_call_arguments.done" => {
                let id = self.active(&event, Kind::Function)?;
                let text = string(&event, "arguments")?;
                if arguments(text).is_err() {
                    let item = self.items.get_mut(&id).expect("checked item");
                    if item.final_arguments.is_some() {
                        return Err(protocol("conflicting final function arguments"));
                    }
                    item.final_arguments = Some(text.into());
                    return Ok(chunks);
                }
                self.validate_arguments(id, text)?;
                let item = self.items.get_mut(&id).expect("checked item");
                item.final_arguments = Some(text.into());
                if let (Some(call_id), Some(name)) = (&item.call_id, &item.name) {
                    let content = BlockContent::ToolCall(ToolCall {
                        id: call_id.clone(),
                        name: name.clone(),
                        arguments: arguments(text)?,
                    });
                    self.close_part(id, 0, content, &mut chunks)?;
                }
            }
            "response.output_text.done"
            | "response.refusal.done"
            | "response.reasoning_summary_text.done" => {
                let reasoning = string(&event, "type")? == "response.reasoning_summary_text.done";
                let id = self.active(
                    &event,
                    if reasoning {
                        Kind::Reasoning
                    } else {
                        Kind::Text
                    },
                )?;
                let position = index(
                    &event,
                    if reasoning {
                        "summary_index"
                    } else {
                        "content_index"
                    },
                )?;
                let position = if reasoning {
                    reasoning_position(position, false)?
                } else {
                    position
                };
                let text = string(
                    &event,
                    if string(&event, "type")? == "response.refusal.done" {
                        "refusal"
                    } else {
                        "text"
                    },
                )?
                .into();
                let content = if reasoning {
                    BlockContent::Reasoning { text }
                } else {
                    BlockContent::Text { text }
                };
                self.close_part(id, position, content, &mut chunks)?;
            }
            "response.content_part.added"
            | "response.content_part.done"
            | "response.reasoning_part.added"
            | "response.reasoning_part.done"
            | "response.reasoning_summary_part.added"
            | "response.reasoning_summary_part.done" => {
                let name = string(&event, "type")?;
                let summary = name.starts_with("response.reasoning_summary_part.");
                let output_index = index(&event, "output_index")?;
                let expected = if name.starts_with("response.reasoning_") {
                    Kind::Reasoning
                } else {
                    self.items
                        .get(&output_index)
                        .ok_or_else(|| protocol("event for an unstarted output item"))?
                        .kind
                };
                if expected == Kind::Function {
                    return Err(protocol("content part on function item"));
                }
                let id = self.active(&event, expected)?;
                let reasoning = expected == Kind::Reasoning;
                let position = index(
                    &event,
                    if summary {
                        "summary_index"
                    } else {
                        "content_index"
                    },
                )?;
                let position = if reasoning {
                    reasoning_position(position, !summary)?
                } else {
                    position
                };
                let part = event
                    .get("part")
                    .ok_or_else(|| protocol("missing content part"))?;
                let text = if reasoning {
                    readable_reasoning(part, summary)?
                } else {
                    match string(part, "type")? {
                        "output_text" => string(part, "text")?,
                        "refusal" => string(part, "refusal")?,
                        _ => return Err(protocol("unsupported content part")),
                    }
                }
                .to_owned();
                if name.ends_with(".done") {
                    let content = if reasoning {
                        BlockContent::Reasoning { text }
                    } else {
                        BlockContent::Text { text }
                    };
                    self.close_part(id, position, content, &mut chunks)?;
                } else {
                    let part = self.part(id, position, &mut chunks);
                    if part.added || part.ended.is_some() || !part.streamed.is_empty() {
                        return Err(protocol("duplicate or late content part added"));
                    }
                    part.added = true;
                    if !text.is_empty() {
                        self.delta(id, position, &text, &mut chunks)?;
                    }
                }
            }
            "response.output_text.annotation.added" => {
                // Annotations decorate text; they are not independent assistant content.
                self.active(&event, Kind::Text)?;
                index(&event, "content_index")?;
                index(&event, "annotation_index")?;
                if !event.get("annotation").is_some_and(Value::is_object) {
                    return Err(protocol("missing annotation object"));
                }
            }
            "response.completed" | "response.incomplete" => {
                let response = event
                    .get("response")
                    .ok_or_else(|| protocol("missing final response"))?;
                // A terminal event ends the stream; its status determines whether
                // the generation succeeded. Never turn an incomplete snapshot into
                // a successful tool-bearing response merely because of its tag.
                let truncated = match string(response, "status")? {
                    "incomplete" => true,
                    "completed" if string(&event, "type")? == "response.completed" => false,
                    _ => return Err(protocol("terminal response status disagrees with event")),
                };
                if truncated {
                    let details = response
                        .get("incomplete_details")
                        .ok_or_else(|| protocol("missing incomplete details"))?;
                    match string(details, "reason")? {
                        "max_output_tokens" | "content_filter" => {}
                        _ => return Err(protocol("unsupported incomplete reason")),
                    }
                }
                let empty = Vec::new();
                let output =
                    if self.allow_omitted_terminal_output && response.get("output").is_none() {
                        &empty
                    } else {
                        array(response, "output")?
                    };
                let omitted = self.allow_omitted_terminal_output
                    && output.is_empty()
                    && self.items.values().all(|item| {
                        item.ended.is_some() || (truncated && item.kind == Kind::Function)
                    });
                // Reserve stable identities before attempting semantic aliases:
                // a new terminal-only item with equal text must not steal an
                // existing item that is also explicitly present in the output.
                let alias_candidates = self
                    .items
                    .iter()
                    .filter_map(|(id, item)| {
                        let retained = output.iter().any(|native| {
                            native
                                .get("id")
                                .and_then(Value::as_str)
                                .is_some_and(|native_id| {
                                    native_id == item.native_id || item.aliases.contains(native_id)
                                })
                        });
                        (!retained).then_some(*id)
                    })
                    .collect();
                let mut seen = BTreeSet::new();
                for native in output {
                    // Stable native IDs take precedence over terminal array
                    // position. Regenerated IDs require unique semantic evidence.
                    let id =
                        self.snapshot_index(native, None, Some(&alias_candidates), &mut chunks)?;
                    if !seen.insert(id) {
                        return Err(protocol("duplicate terminal output item"));
                    }
                    if truncated && self.items[&id].kind == Kind::Function {
                        if kind(native)? != Kind::Function {
                            return Err(protocol("final output item identity changed"));
                        }
                        continue;
                    }
                    self.end(id, native, &mut chunks, true)?;
                }
                if !omitted
                    && self.items.iter().any(|(id, item)| {
                        !seen.contains(id) && !(truncated && item.kind == Kind::Function)
                    })
                {
                    return Err(protocol("terminal response omitted a streamed output item"));
                }
                if truncated {
                    // Even syntactically complete tools are unsafe on an
                    // abnormal stop; never expose them as executable calls.
                    for item in self
                        .items
                        .values()
                        .filter(|item| item.kind == Kind::Function)
                    {
                        chunks.push(ResponseChunk::ItemDiscarded {
                            id: item.native_id.clone(),
                        });
                    }
                }
                // Reasoning display stays open through item.done so the final
                // snapshot can supply previously absent readable content. Close
                // it from its actual received snapshot on omitted-output streams.
                if omitted {
                    let snapshots: Vec<_> = self
                        .items
                        .iter()
                        .filter(|(_, item)| !(truncated && item.kind == Kind::Function))
                        .map(|(id, item)| (*id, item.ended.clone().expect("checked ended")))
                        .collect();
                    for (id, native) in snapshots {
                        self.end(id, &native, &mut chunks, true)?;
                    }
                }
                if let Some(usage) = response.get("usage").filter(|u| !u.is_null()) {
                    let count = |key: &str| {
                        usage
                            .get(key)
                            .and_then(Value::as_u64)
                            .ok_or_else(|| protocol(format!("missing or invalid usage.{key}")))
                    };
                    let cached = match usage.get("input_tokens_details").filter(|v| !v.is_null()) {
                        Some(details) => details
                            .get("cached_tokens")
                            .and_then(Value::as_u64)
                            .ok_or_else(|| protocol("invalid cached input token usage"))?,
                        None => 0,
                    };
                    let total_input = count("input_tokens")?;
                    let uncached_input = total_input
                        .checked_sub(cached)
                        .ok_or_else(|| protocol("cached tokens exceed input tokens"))?;
                    // Internal input_tokens excludes reads served from cache.
                    let usage = Usage {
                        input_tokens: uncached_input,
                        cached_input_tokens: cached,
                        output_tokens: count("output_tokens")?,
                    };
                    chunks.push(ResponseChunk::UsageUpdated { usage });
                }
                self.completed = true;
                let stop_reason = if truncated {
                    match response["incomplete_details"]["reason"].as_str() {
                        Some("max_output_tokens") => StopReason::MaxTokens,
                        Some("content_filter") => StopReason::ContentFilter,
                        _ => unreachable!("validated reason"),
                    }
                } else if self.items.values().any(|item| item.kind == Kind::Function) {
                    StopReason::ToolUse
                } else {
                    StopReason::EndTurn
                };
                chunks.push(ResponseChunk::ResponseEnded { stop_reason });
            }
            "response.failed" => {
                let response = event
                    .get("response")
                    .ok_or_else(|| protocol("missing failed response"))?;
                return Err(api_error(
                    response
                        .get("error")
                        .ok_or_else(|| protocol("missing response error"))?,
                ));
            }
            "error" => return Err(api_error(event.get("error").unwrap_or(&event))),
            other => {
                let name: String = other
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_'))
                    .take(96)
                    .collect();
                return Err(protocol(format!("unsupported event: {name}")));
            }
        }
        Ok(chunks)
    }

    pub(crate) fn finish(&mut self) -> Result<Vec<ResponseChunk>, ProviderError> {
        if !self.completed {
            return Err(protocol("stream ended before a terminal response"));
        }
        Ok(vec![])
    }
}

fn api_error(error: &Value) -> ProviderError {
    let code = error.get("code").and_then(Value::as_str).unwrap_or("");
    let kind = match code {
        "context_length_exceeded" | "context_window_exceeded" => {
            ProviderErrorKind::ContextWindowExceeded
        }
        "invalid_api_key" | "authentication_error" => ProviderErrorKind::Authentication,
        "rate_limit_exceeded" | "rate_limit_error" => ProviderErrorKind::RateLimited,
        "timeout" | "request_timeout" => ProviderErrorKind::Timeout,
        _ => ProviderErrorKind::Response,
    };
    // Never echo upstream messages or arbitrary codes: they may reflect prompts
    // or credentials. Only the locally classified category is safe to surface.
    ProviderError {
        kind,
        message: format!("Responses request failed ({kind:?})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::protocol::AssistantItem;
    use crate::{
        media::ImageReference,
        provider::protocol::{ResponseSchema, SystemSegment, ToolDefinition, ToolResult},
    };

    fn request() -> ModelRequest {
        ModelRequest {
            model: "gpt-5".into(),
            system: vec![],
            messages: vec![],
            tools: vec![],
            response_schema: None,
            reasoning: None,
            max_output_tokens: None,
            correlation: None,
        }
    }

    fn image() -> ImageReference {
        ImageReference {
            sha256: "hash".into(),
            media_type: "image/png".into(),
            name: "image.png".into(),
            bytes: 1,
            data_base64: Some("YQ==".into()),
        }
    }

    fn text_item(id: &str, text: &str) -> Value {
        json!({"type":"message", "id":id, "role":"assistant", "status":"completed",
            "content":[{"type":"output_text", "text":text, "annotations":[]}]})
    }

    fn reasoning_item() -> Value {
        json!({"type":"reasoning", "id":"rs_1", "encrypted_content":"secret",
            "summary":[{"type":"summary_text", "text":"first"}, {"type":"summary_text", "text":"second"}]})
    }

    fn call_item() -> Value {
        json!({"type":"function_call", "id":"fc_1", "call_id":"call_1", "name":"search",
            "arguments":"{\"query\":\"rust\"}", "status":"completed"})
    }

    fn added(id: usize, item: Value) -> Value {
        json!({"type":"response.output_item.added", "output_index":id, "item":item})
    }

    fn done(id: usize, item: Value) -> Value {
        json!({"type":"response.output_item.done", "output_index":id, "item":item})
    }

    fn completed(output: Vec<Value>) -> Value {
        json!({"type":"response.completed", "response":{"status":"completed", "output":output,
            "usage":{"input_tokens":20, "output_tokens":7, "input_tokens_details":{"cached_tokens":12}}}})
    }

    #[test]
    fn request_transmits_native_tools_schema_reasoning_and_cache_key() {
        let mut req = request();
        req.system = vec![SystemSegment {
            text: "system".into(),
            cache: false,
        }];
        req.tools = vec![ToolDefinition {
            name: "search".into(),
            description: "Search".into(),
            input_schema: json!({"type":"object"}),
        }];
        req.response_schema = Some(ResponseSchema {
            name: "answer".into(),
            schema: json!({"type":"object", "properties":{}, "additionalProperties":false}),
        });
        req.reasoning = Some("high".into());
        req.correlation = Some("session".into());
        req.messages = vec![
            Message::User(vec![
                UserContent::Text {
                    text: "look".into(),
                },
                UserContent::Image { image: image() },
            ]),
            Message::Tool(vec![ToolResult {
                call_id: "call_1".into(),
                name: "search".into(),
                result: json!({"answer":42}),
                images: vec![image()],
                is_error: true,
            }]),
        ];
        let body = encode(&req).unwrap();
        assert_eq!(body["instructions"], "system");
        assert_eq!(body["tools"][0]["name"], "search");
        assert_eq!(
            body["text"]["format"]["schema"],
            req.response_schema.unwrap().schema
        );
        assert_eq!(
            body["reasoning"],
            json!({"effort":"high", "summary":"auto"})
        );
        assert_eq!(body["prompt_cache_key"], "session");
        assert_eq!(body["input"][0]["content"][0]["text"], "look");
        assert_eq!(
            body["input"][0]["content"][1]["image_url"],
            "data:image/png;base64,YQ=="
        );
        assert_eq!(body["input"][1]["call_id"], "call_1");
        let result: Value =
            serde_json::from_str(body["input"][1]["output"].as_str().unwrap()).unwrap();
        assert_eq!(result, json!({"result":{"answer":42},"is_error":true}));
        assert_eq!(
            body["input"][2]["content"][1]["image_url"],
            "data:image/png;base64,YQ=="
        );
    }

    fn assemble(output: Vec<Value>) -> Vec<AssistantItem> {
        let mut assembler = crate::provider::protocol::ResponseAssembler::default();
        for event in Decoder::new("gpt-5".into())
            .feed(completed(output))
            .unwrap()
        {
            assembler.push(&event).unwrap();
        }
        assembler.finish().unwrap().0
    }

    #[test]
    fn reasoning_replays_full_native_item_only_for_matching_provenance() {
        let mut req = request();
        let native = reasoning_item();
        let items = assemble(vec![native.clone()]);
        assert_eq!(items[0].blocks.len(), 2);
        assert_eq!(items[0].replay.as_ref().unwrap().payload, native);
        req.messages = vec![Message::Assistant(items)];
        assert_eq!(encode(&req).unwrap()["input"], json!([native]));
        req.model = "different-model".into();
        assert_eq!(encode(&req).unwrap()["input"], json!([]));
        req.messages = vec![Message::Assistant(vec![AssistantItem::reasoning(
            "r",
            0,
            "private",
            Some(reasoning_envelope(
                "anthropic",
                &req.model,
                reasoning_item(),
            )),
        )])];
        assert_eq!(encode(&req).unwrap()["input"], json!([]));
    }

    #[tokio::test]
    async fn encrypted_reasoning_and_tools_survive_save_resume_and_scope_changes() {
        use super::super::common::{
            bind_reasoning_scope, filter_reasoning_scope, reasoning_scope, tests::resume_request,
        };
        let mut req = request();
        let scope = reasoning_scope("openai", "https://api.example/v1/responses");
        // No display summary is required for native reasoning to be replayable.
        let native = json!({"type":"reasoning", "id":"rs_opaque", "summary":[],
            "encrypted_content":"opaque+/=", "future_state":{"signature":"unchanged"},
            "content":[{"type":"reasoning_text", "text":"native reasoning text"}]});
        let mut assembler = crate::provider::protocol::ResponseAssembler::default();
        let mut decoder = Decoder::new(req.model.clone());
        for frame in [
            added(
                0,
                json!({"type":"reasoning", "id":"rs_opaque", "summary":[]}),
            ),
            json!({"type":"response.reasoning_text.delta", "output_index":0,
                "item_id":"rs_opaque", "content_index":0, "delta":"native reasoning "}),
            json!({"type":"response.reasoning_text.delta", "output_index":0,
                "item_id":"rs_opaque", "content_index":0, "delta":"text"}),
            json!({"type":"response.reasoning_text.done", "output_index":0,
                "item_id":"rs_opaque", "content_index":0, "text":"native reasoning text"}),
            // Ciphertext and future replay state arrive only at completion.
            done(
                0,
                json!({"type":"reasoning", "id":"rs_opaque", "summary":[]}),
            ),
            added(1, call_item()),
            done(1, call_item()),
            completed(vec![native.clone(), call_item()]),
        ] {
            for mut chunk in decoder.feed(frame).unwrap() {
                bind_reasoning_scope(&mut chunk, &scope);
                assembler.push(&chunk).unwrap();
            }
        }
        let (items, _, reason) = assembler.finish().unwrap();
        assert_eq!(reason, StopReason::ToolUse);
        assert_eq!(items[0].blocks.len(), 1);
        req.messages = vec![
            Message::Assistant(items),
            Message::Tool(vec![ToolResult {
                call_id: "call_1".into(),
                name: "search".into(),
                result: json!({"found":true}),
                images: vec![],
                is_error: false,
            }]),
        ];
        let original = resume_request(&req).await;
        let mut matching = original.clone();
        filter_reasoning_scope(&mut matching, &scope);
        let body = encode(&matching).unwrap();
        assert_eq!(body["input"][0], native);
        assert_eq!(body["input"][1]["type"], "function_call");
        assert_eq!(body["input"][2]["type"], "function_call_output");
        assert_eq!(body["input"][1]["call_id"], body["input"][2]["call_id"]);
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));

        for foreign_scope in [
            reasoning_scope("other-provider", "https://api.example/v1/responses"),
            reasoning_scope("openai", "https://other.example/v1/responses"),
        ] {
            let mut foreign = original.clone();
            filter_reasoning_scope(&mut foreign, &foreign_scope);
            let Message::Assistant(items) = &foreign.messages[0] else {
                unreachable!()
            };
            assert_eq!(items.len(), 2);
            assert_eq!(items[0].blocks.len(), 1);
            assert!(items[0].replay.is_none());
            assert_eq!(
                encode(&foreign).unwrap()["input"],
                json!([body["input"][1].clone(), body["input"][2].clone()])
            );
        }
        let mut foreign = original.clone();
        foreign.model = "different-model".into();
        assert_eq!(
            encode(&foreign).unwrap()["input"].as_array().unwrap().len(),
            2
        );
        // Filtering a call-time clone must never destroy resumable journal state.
        assert_eq!(encode(&original).unwrap()["input"][0], native);
    }

    #[test]
    fn native_reasoning_text_displays_without_requiring_final_plaintext() {
        // Shapes follow openai-python's ResponseReasoningTextDeltaEvent,
        // ResponseReasoningTextDoneEvent, and ResponseReasoningItem.content.
        for codex in [false, true] {
            for (content, valid) in [
                (None, true),
                (
                    Some(json!([{"type":"reasoning_text", "text":"different"}])),
                    false,
                ),
                (
                    Some(json!([{"type":"reasoning_text", "text":"native"}])),
                    true,
                ),
            ] {
                let mut decoder = if codex {
                    Decoder::codex("gpt-5".into())
                } else {
                    Decoder::new("gpt-5".into())
                };
                let mut native = json!({"type":"reasoning", "id":"rs_1", "summary":[]});
                decoder.feed(added(0, native.clone())).unwrap();
                assert!(
                    decoder
                        .feed(json!({"type":"response.reasoning_text.delta",
                    "output_index":0, "item_id":"rs_1", "content_index":0, "delta":"native"}))
                        .unwrap()
                        .iter()
                        .any(|chunk| matches!(
                            chunk,
                            ResponseChunk::BlockStarted { .. } | ResponseChunk::BlockEnded { .. }
                        ))
                );
                assert!(
                    decoder
                        .feed(json!({"type":"response.reasoning_text.done",
                    "output_index":0, "item_id":"rs_1", "content_index":0, "text":"native"}))
                        .unwrap()
                        .iter()
                        .any(|chunk| matches!(
                            chunk,
                            ResponseChunk::BlockStarted { .. } | ResponseChunk::BlockEnded { .. }
                        ))
                );
                if let Some(content) = content {
                    native["content"] = content;
                }
                let result = decoder.feed(done(0, native.clone())).and_then(|_| {
                    decoder.feed(completed(if codex { vec![] } else { vec![native] }))
                });
                assert_eq!(result.is_ok(), valid, "codex={codex}: {result:?}");
            }
        }
        let mut decoder = Decoder::new("gpt-5".into());
        decoder.feed(added(0, reasoning_item())).unwrap();
        decoder
            .feed(json!({"type":"response.reasoning_text.delta",
            "output_index":0, "item_id":"rs_1", "content_index":0, "delta":"native"}))
            .unwrap();
        assert!(
            decoder
                .feed(json!({"type":"response.reasoning_text.done",
            "output_index":0, "item_id":"rs_1", "content_index":0, "text":"changed"}))
                .is_err()
        );
    }

    #[test]
    fn terminal_reasoning_may_enrich_but_not_replace_native_state() {
        for placeholder in [Value::Null, json!("")] {
            let old = json!({"type":"reasoning", "id":"rs_1", "summary":[], "encrypted_content":placeholder});
            let terminal = json!({"type":"reasoning", "id":"rs_1", "summary":[], "encrypted_content":"ciphertext"});
            let mut decoder = Decoder::new("gpt-5".into());
            decoder.feed(added(0, old.clone())).unwrap();
            decoder.feed(done(0, old)).unwrap();
            let chunks = decoder.feed(completed(vec![terminal.clone()])).unwrap();
            assert!(chunks.iter().any(|chunk| matches!(chunk,
                ResponseChunk::ItemEnded { replay: Some(replay), .. } if replay.payload == terminal)));
        }
        let old = reasoning_item();
        for terminal in [
            {
                let mut v = old.clone();
                v["encrypted_content"] = json!("different");
                v
            },
            {
                let mut v = old.clone();
                v.as_object_mut().unwrap().remove("encrypted_content");
                v
            },
            {
                let mut v = old.clone();
                v["summary"][0]["text"] = json!("different");
                v
            },
        ] {
            let mut decoder = Decoder::new("gpt-5".into());
            decoder.feed(added(0, old.clone())).unwrap();
            decoder.feed(done(0, old.clone())).unwrap();
            assert!(decoder.feed(completed(vec![terminal])).is_err());
        }
    }

    #[test]
    fn assistant_text_and_function_calls_encode_as_native_items() {
        let mut req = request();
        req.messages = vec![Message::Assistant(vec![
            AssistantItem::text("t", 0, "hello"),
            AssistantItem::tool_call(
                "f",
                1,
                ToolCall {
                    id: "call_1".into(),
                    name: "search".into(),
                    arguments: json!({"query":"rust"}),
                },
            ),
        ])];
        let body = encode(&req).unwrap();
        assert_eq!(
            body["input"][0]["content"][0],
            json!({"type":"output_text", "text":"hello"})
        );
        assert_eq!(
            body["input"][1],
            json!({"type":"function_call", "call_id":"call_1", "name":"search", "arguments":"{\"query\":\"rust\"}"})
        );
    }

    #[test]
    fn out_of_order_items_keep_provider_indices_and_authoritative_blocks() {
        let mut decoder = Decoder::new("gpt-5".into());
        let mut assembler = crate::provider::protocol::ResponseAssembler::default();
        let reasoning = reasoning_item();
        let text = text_item("msg_1", "authoritative");
        for event in [
            added(1, text_item("msg_1", "")),
            added(0, reasoning.clone()),
            json!({"type":"response.output_text.delta", "output_index":1, "item_id":"msg_1", "content_index":0, "delta":"authoritative"}),
            done(1, text.clone()),
            completed(vec![reasoning, text]),
        ] {
            for chunk in decoder.feed(event).unwrap() {
                assembler.push(&chunk).unwrap();
            }
        }
        let (items, usage, stop) = assembler.finish().unwrap();
        assert_eq!(items[0].id, "rs_1");
        assert_eq!(items[1].text_content().as_deref(), Some("authoritative"));
        assert_eq!(
            usage,
            Usage {
                input_tokens: 8,
                cached_input_tokens: 12,
                output_tokens: 7
            }
        );
        assert_eq!(stop, StopReason::EndTurn);
        assert!(decoder.finish().unwrap().is_empty());
    }

    #[test]
    fn function_arguments_are_typed_validated_and_identity_can_arrive_at_item_end() {
        let mut decoder = Decoder::new("gpt-5".into());
        decoder
            .feed(added(
                0,
                json!({"id":"fc_1","type":"function_call","arguments":""}),
            ))
            .unwrap();
        let chunks = decoder.feed(json!({"type":"response.function_call_arguments.delta", "output_index":0, "item_id":"fc_1", "delta":"{\"query\":\"rust\"}"})).unwrap();
        assert!(
            matches!(&chunks[1], ResponseChunk::BlockDelta{delta:ContentDelta::JsonFragment(text),..} if text == "{\"query\":\"rust\"}")
        );
        assert!(decoder.feed(json!({"type":"response.function_call_arguments.done", "output_index":0, "item_id":"fc_1", "arguments":"{\"query\":\"rust\"}"})).unwrap().is_empty());
        let result = decoder.feed(done(0, call_item())).unwrap();
        assert!(
            matches!(&result[0], ResponseChunk::BlockEnded{content:BlockContent::ToolCall(call),..} if call.name == "search")
        );
        assert!(matches!(&result[1], ResponseChunk::ItemEnded { .. }));
        let terminal = decoder.feed(completed(vec![call_item()])).unwrap();
        assert!(matches!(
            terminal.last(),
            Some(ResponseChunk::ResponseEnded {
                stop_reason: StopReason::ToolUse
            })
        ));
        for args in ["", "not json", "[]", "null"] {
            let mut item = call_item();
            item["arguments"] = json!(args);
            assert!(
                Decoder::new("gpt-5".into())
                    .feed(completed(vec![item]))
                    .is_err()
            );
        }
    }

    #[test]
    fn abnormal_terminal_discards_tools_but_preserves_reasoning_and_usage() {
        for codex in [false, true] {
            for output_done in [false, true] {
                for (detail, reason) in [
                    ("max_output_tokens", StopReason::MaxTokens),
                    ("content_filter", StopReason::ContentFilter),
                ] {
                    for args in ["{\"query\":", "{\"query\":\"rust\"}"] {
                        let mut decoder = if codex {
                            Decoder::codex("gpt-5".into())
                        } else {
                            Decoder::new("gpt-5".into())
                        };
                        let native = reasoning_item();
                        let mut call = call_item();
                        call["arguments"] = json!(args);
                        let mut initial_call = call.clone();
                        initial_call["arguments"] = json!("");
                        let mut frames = vec![
                            added(0, native.clone()),
                            done(0, native.clone()),
                            added(1, initial_call),
                            json!({"type":"response.function_call_arguments.delta", "output_index":1,
                                "item_id":"fc_1", "delta":args}),
                            json!({"type":"response.function_call_arguments.done", "output_index":1,
                                "item_id":"fc_1", "arguments":args}),
                        ];
                        if output_done {
                            frames.push(done(1, call.clone()));
                        }
                        frames.push(json!({"type":"response.incomplete", "response":{
                            "status":"incomplete", "incomplete_details":{"reason":detail},
                            "output":if codex { vec![] } else { vec![native.clone(), call] },
                            "usage":{"input_tokens":20,"output_tokens":11}}}));
                        let mut assembler = crate::provider::protocol::ResponseAssembler::default();
                        for frame in frames {
                            for chunk in decoder.feed(frame).unwrap() {
                                assembler.push(&chunk).unwrap();
                            }
                        }
                        decoder.finish().unwrap();
                        let (items, usage, actual_reason) = assembler.finish().unwrap();
                        assert_eq!(actual_reason, reason);
                        assert_eq!(usage.output_tokens, 11);
                        assert_eq!(usage.input_tokens, 20);
                        assert_eq!(items.len(), 1);
                        assert_eq!(items[0].replay.as_ref().unwrap().payload, native);
                    }
                }
            }
        }
    }

    #[test]
    fn deferred_malformed_tools_still_fail_on_normal_terminal() {
        for codex in [false, true] {
            let mut decoder = if codex {
                Decoder::codex("gpt-5".into())
            } else {
                Decoder::new("gpt-5".into())
            };
            let mut call = call_item();
            call["arguments"] = json!("{\"query\":");
            decoder.feed(added(0, call.clone())).unwrap();
            assert!(decoder.feed(done(0, call.clone())).unwrap().is_empty());
            assert!(
                decoder
                    .feed(completed(if codex { vec![] } else { vec![call] }))
                    .is_err()
            );
        }
    }

    #[test]
    fn request_always_requests_reasoning_summary_without_defaulting_effort() {
        let body = encode(&request()).unwrap();
        assert_eq!(body["reasoning"], json!({"summary":"auto"}));
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    }

    #[test]
    fn request_reasoning_effort_is_encoded_without_restricting_model_capabilities() {
        let mut request = request();
        for effort in ["none", "minimal", "low", "medium", "high", "xhigh", "max"] {
            request.reasoning = Some(effort.into());
            let body = encode(&request).unwrap();
            assert_eq!(
                body["reasoning"],
                json!({"effort":effort, "summary":"auto"})
            );
            assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        }
    }

    #[test]
    fn request_rejects_unsupported_reasoning_effort() {
        let mut request = request();
        request.reasoning = Some("invalid".into());
        assert!(encode(&request).is_err());
    }

    #[test]
    fn sse_validation_eof_and_errors_are_not_silent_success() {
        let mut decoder = Decoder::new("gpt-5".into());
        assert!(decoder.finish().is_err());
        for (name, data) in [
            (None, "[DONE]"),
            (None, "{"),
            (
                Some("response.created"),
                "{\"type\":\"response.completed\"}",
            ),
        ] {
            assert!(
                decoder
                    .decode(&super::super::transport::SseEvent {
                        event: name.map(str::to_owned),
                        data: data.into()
                    })
                    .is_err()
            );
        }
        for code in [
            "context_length_exceeded",
            "rate_limit_exceeded",
            "invalid_api_key",
            "server_error",
        ] {
            let error = decoder
                .feed(json!({"type":"error", "code":code,"message":"details"}))
                .unwrap_err();
            assert!(!error.message.contains("details"));
            assert_eq!(
                error.kind,
                match code {
                    "context_length_exceeded" => ProviderErrorKind::ContextWindowExceeded,
                    "rate_limit_exceeded" => ProviderErrorKind::RateLimited,
                    "invalid_api_key" => ProviderErrorKind::Authentication,
                    _ => ProviderErrorKind::Response,
                }
            );
        }
        decoder
            .decode(&super::super::transport::SseEvent {
                event: Some("response.completed".into()),
                data: completed(vec![]).to_string(),
            })
            .unwrap();
        assert!(
            decoder
                .decode(&super::super::transport::SseEvent {
                    event: None,
                    data: "[DONE]".into()
                })
                .unwrap()
                .is_empty()
        );
        assert!(decoder.feed(completed(vec![])).is_err());
    }
}

#[cfg(test)]
#[path = "responses/reasoning_tests.rs"]
mod reasoning_tests;
