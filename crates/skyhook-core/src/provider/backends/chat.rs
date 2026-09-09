//! Native OpenAI Chat Completions with compatible visible reasoning deltas.

#[cfg(test)]
mod compatibility_tests;
mod wire;

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use crate::provider::{
    ProviderError,
    backends::ChatReasoningReplay,
    protocol::{
        BlockContent, BlockKind, ContentDelta, ItemKind, Message, ModelRequest, ResponseChunk,
        StopReason, ToolCall, Usage, UserContent,
    },
};

use super::{
    common::{image_url, invalid, opaque_payload, reasoning_envelope, tool_text},
    transport::SseEvent,
};

pub(crate) fn encode(
    request: &ModelRequest,
    replay: ChatReasoningReplay,
) -> Result<Value, ProviderError> {
    if request.model.is_empty() {
        return Err(invalid("Chat Completions requires a model"));
    }
    // SystemSegment.cache is a cross-protocol hint. OpenAI automatically caches
    // eligible prefixes and has no per-segment cache-control wire field.
    let mut messages = Vec::new();
    for segment in &request.system {
        messages.push(json!({"role": "system", "content": segment.text}));
    }
    for message in &request.messages {
        match message {
            Message::User(parts) => {
                let mut content = Vec::new();
                for part in parts {
                    content.push(match part {
                        UserContent::Text { text }
                        | UserContent::Runtime { text }
                        | UserContent::ParentInput { text }
                        | UserContent::Compaction { text } => json!({"type": "text", "text": text}),
                        UserContent::Image { image } => {
                            json!({"type": "image_url", "image_url": {"url": image_url(image)?}})
                        }
                    });
                }
                messages.push(json!({"role": "user", "content": content}));
            }
            Message::Assistant(parts) => {
                let mut text = String::new();
                let mut calls = Vec::new();
                let mut reasoning = String::new();
                for item in parts {
                    if replay != ChatReasoningReplay::Unsupported
                        && let Some(payload) =
                            opaque_payload(&item.replay, "chat_completions", &request.model)
                        && let Some(text) = payload.get("text").and_then(Value::as_str)
                    {
                        reasoning.push_str(text);
                    }
                    for part in &item.blocks {
                        match &part.content {
                            BlockContent::Text { text: fragment } => text.push_str(fragment),
                            // Only the originating envelope is replayable. Visible
                            // reasoning (including foreign summaries) is not provenance.
                            BlockContent::Reasoning { .. } => {}
                            BlockContent::ToolCall(call) => {
                                if call.id.is_empty()
                                    || !valid_name(&call.name)
                                    || !call.arguments.is_object()
                                {
                                    return Err(invalid(
                                        "Chat tool calls require an ID, a valid function name, and object arguments",
                                    ));
                                }
                                calls.push(json!({
                                "id": call.id, "type": "function",
                                "function": {"name": call.name, "arguments": call.arguments.to_string()}
                            }));
                            }
                        }
                    }
                }
                // Preserve reasoning-only turns when the selected profile can
                // replay their provider-bound state; never turn thoughts into content.
                if !text.is_empty() || !calls.is_empty() || !reasoning.is_empty() {
                    let mut message = json!({"role": "assistant", "content": if text.is_empty() { Value::Null } else { Value::String(text) }});
                    if !calls.is_empty() {
                        message["tool_calls"] = Value::Array(calls);
                    }
                    if !reasoning.is_empty() {
                        let field = match replay {
                            ChatReasoningReplay::ReasoningContent => "reasoning_content",
                            ChatReasoningReplay::Reasoning => "reasoning",
                            ChatReasoningReplay::Unsupported => unreachable!(),
                        };
                        message[field] = Value::String(reasoning);
                    }
                    messages.push(message);
                }
            }
            Message::Tool(results) => {
                // All tool replies must precede the synthetic user image message.
                let mut images = Vec::new();
                for result in results {
                    if result.call_id.is_empty() {
                        return Err(invalid("Chat tool results require a call ID"));
                    }
                    messages.push(json!({"role": "tool", "tool_call_id": result.call_id, "content": tool_text(result)}));
                    if !result.images.is_empty() {
                        images.push(json!({"type": "text", "text": format!("Images returned by tool call {}:", result.call_id)}));
                        for image in &result.images {
                            images.push(json!({"type": "image_url", "image_url": {"url": image_url(image)?}}));
                        }
                    }
                }
                if !images.is_empty() {
                    messages.push(json!({"role": "user", "content": images}));
                }
            }
        }
    }
    let mut body = serde_json::to_value(wire::Request {
        model: &request.model,
        messages,
        n: 1,
        stream: true,
        stream_options: wire::StreamOptions {
            include_usage: true,
        },
    })
    .map_err(|_| invalid("Unable to serialize Chat request"))?;
    if let Some(effort) = &request.reasoning {
        if !matches!(
            effort.as_str(),
            "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
        ) {
            return Err(invalid(format!(
                "Unsupported standard Chat reasoning_effort: {effort}"
            )));
        }
        body["reasoning_effort"] = json!(effort);
    }
    if let Some(limit) = request.max_output_tokens {
        if limit == 0 {
            return Err(invalid("max_output_tokens must be positive"));
        }
        body["max_completion_tokens"] = json!(limit);
    }
    if !request.tools.is_empty() {
        let mut names = BTreeSet::new();
        let mut tools = Vec::new();
        for tool in &request.tools {
            if !valid_name(&tool.name) || !names.insert(&tool.name) {
                return Err(invalid(
                    "Chat tools require unique function names of 1–64 ASCII letters, digits, underscores or hyphens",
                ));
            }
            if !tool.input_schema.is_object() {
                return Err(invalid(
                    "Chat function parameters must be a JSON Schema object",
                ));
            }
            tools.push(json!({"type": "function", "function": {
                "name": tool.name, "description": tool.description, "parameters": tool.input_schema
            }}));
        }
        body["tools"] = Value::Array(tools);
    }
    if let Some(schema) = &request.response_schema {
        if !valid_name(&schema.name) {
            return Err(invalid(
                "Chat response schema requires a name of 1–64 ASCII letters, digits, underscores or hyphens",
            ));
        }
        if schema.schema.get("type").and_then(Value::as_str) != Some("object")
            || schema.schema.get("anyOf").is_some()
        {
            return Err(invalid(
                "Chat strict response schema root must be an object, not anyOf",
            ));
        }
        validate_schema(&schema.schema, &schema.schema, 0)?;
        body["response_format"] = json!({"type": "json_schema", "json_schema": {
            "name": schema.name, "strict": true, "schema": schema.schema
        }});
    }
    Ok(body)
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

/// Deliberately conservative subset of strict Structured Outputs. Do not silently
/// rewrite optional properties, open objects, or unsupported schema constraints.
fn validate_schema(schema: &Value, root: &Value, depth: usize) -> Result<(), ProviderError> {
    if depth > 64 {
        return Err(invalid("Chat response schema nesting exceeds 64 levels"));
    }
    let object = schema
        .as_object()
        .ok_or_else(|| invalid("Chat strict schemas must be objects"))?;
    for key in object.keys() {
        if !matches!(
            key.as_str(),
            "type"
                | "properties"
                | "required"
                | "additionalProperties"
                | "items"
                | "enum"
                | "anyOf"
                | "$ref"
                | "$defs"
                | "description"
                | "title"
        ) {
            return Err(invalid(format!(
                "Unsupported Chat strict schema keyword: {key}"
            )));
        }
    }
    for key in ["description", "title"] {
        if object.get(key).is_some_and(|value| !value.is_string()) {
            return Err(invalid(format!("Chat schema {key} must be a string")));
        }
    }
    if let Some(defs) = object.get("$defs") {
        for definition in defs
            .as_object()
            .ok_or_else(|| invalid("Chat schema $defs must be an object"))?
            .values()
        {
            validate_schema(definition, root, depth + 1)?;
        }
    }
    if let Some(reference) = object.get("$ref") {
        let reference = reference
            .as_str()
            .ok_or_else(|| invalid("Chat schema $ref must be a string"))?;
        let pointer = reference
            .strip_prefix('#')
            .ok_or_else(|| invalid("Chat schemas support only local references"))?;
        if !pointer.is_empty() && !pointer.starts_with('/') {
            return Err(invalid("Chat schema references must be JSON pointers"));
        }
        if !root.pointer(pointer).is_some_and(Value::is_object) {
            return Err(invalid(
                "Chat schema reference does not resolve to a schema object",
            ));
        }
    }
    if let Some(variants) = object.get("anyOf") {
        let variants = variants
            .as_array()
            .filter(|values| !values.is_empty())
            .ok_or_else(|| invalid("Chat schema anyOf must be a nonempty array"))?;
        for variant in variants {
            validate_schema(variant, root, depth + 1)?;
        }
    }
    let mut types = BTreeSet::new();
    if let Some(kind) = object.get("type") {
        match kind {
            Value::String(kind) => {
                types.insert(kind.as_str());
            }
            Value::Array(kinds) if !kinds.is_empty() => {
                for kind in kinds {
                    let kind = kind
                        .as_str()
                        .ok_or_else(|| invalid("Chat schema type entries must be strings"))?;
                    if !types.insert(kind) {
                        return Err(invalid("Chat schema type entries must be unique"));
                    }
                }
            }
            _ => return Err(invalid("Invalid Chat schema type")),
        }
        if types.iter().any(|kind| {
            !matches!(
                *kind,
                "object" | "array" | "string" | "number" | "integer" | "boolean" | "null"
            )
        }) {
            return Err(invalid("Unsupported Chat schema type"));
        }
    } else if !object.contains_key("$ref") && !object.contains_key("anyOf") {
        return Err(invalid("Chat strict schema needs type, $ref, or anyOf"));
    }
    if types.contains("object") {
        if object.get("additionalProperties") != Some(&Value::Bool(false)) {
            return Err(invalid(
                "Every Chat strict schema object requires additionalProperties: false",
            ));
        }
        let properties = object
            .get("properties")
            .and_then(Value::as_object)
            .ok_or_else(|| invalid("Chat strict schema objects require properties"))?;
        let required = object
            .get("required")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                invalid("Chat strict schema objects require all properties to be required")
            })?;
        let mut names = BTreeSet::new();
        for name in required {
            let name = name
                .as_str()
                .ok_or_else(|| invalid("Chat schema required entries must be strings"))?;
            if !names.insert(name) || !properties.contains_key(name) {
                return Err(invalid(
                    "Chat schema required must list each property exactly once",
                ));
            }
        }
        if names.len() != properties.len() {
            return Err(invalid(
                "Chat strict schema cannot contain optional properties",
            ));
        }
        for property in properties.values() {
            validate_schema(property, root, depth + 1)?;
        }
    } else if ["properties", "required", "additionalProperties"]
        .iter()
        .any(|key| object.contains_key(*key))
    {
        return Err(invalid("Chat object schema keywords require object type"));
    }
    if types.contains("array") {
        validate_schema(
            object
                .get("items")
                .ok_or_else(|| invalid("Chat array schemas require items"))?,
            root,
            depth + 1,
        )?;
    } else if object.contains_key("items") {
        return Err(invalid("Chat items requires array type"));
    }
    if let Some(variants) = object.get("enum") {
        let variants = variants
            .as_array()
            .filter(|values| !values.is_empty())
            .ok_or_else(|| invalid("Chat schema enum must be nonempty"))?;
        for variant in variants {
            let matches_type = types.is_empty()
                || types.iter().any(|kind| match *kind {
                    "null" => variant.is_null(),
                    "boolean" => variant.is_boolean(),
                    "string" => variant.is_string(),
                    "number" => variant.is_number(),
                    "integer" => variant.is_i64() || variant.is_u64(),
                    "object" => variant.is_object(),
                    "array" => variant.is_array(),
                    _ => false,
                });
            if !matches_type {
                return Err(invalid("Chat schema enum value does not match its type"));
            }
        }
    }
    Ok(())
}

#[derive(Clone)]
enum Block {
    Text(String),
    Reasoning(String),
    Tool {
        call_id: String,
        name: String,
        arguments: String,
    },
}

/// One choice, one stream. IDs identify logical blocks rather than wire indexes.
#[derive(Clone)]
pub(crate) struct Decoder {
    model: String,
    blocks: Vec<Block>,
    visible_id: Option<usize>,
    ended: BTreeSet<usize>,
    tool_ids: BTreeMap<u64, usize>,
    finish_reason: Option<StopReason>,
    usage: Usage,
    raw_prompt_tokens: u64,
    done: bool,
    failed: bool,
}

impl Decoder {
    pub(crate) fn new(model: String) -> Self {
        Self {
            model,
            blocks: Vec::new(),
            visible_id: None,
            ended: BTreeSet::new(),
            tool_ids: BTreeMap::new(),
            finish_reason: None,
            usage: Usage::default(),
            raw_prompt_tokens: 0,
            done: false,
            failed: false,
        }
    }

    pub(crate) fn decode(&mut self, event: &SseEvent) -> Result<Vec<ResponseChunk>, ProviderError> {
        if self.failed {
            return Err(ProviderError::protocol("Chat stream already failed"));
        }
        match self.decode_event(event) {
            Ok(chunks) => Ok(chunks),
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }

    fn decode_event(&mut self, event: &SseEvent) -> Result<Vec<ResponseChunk>, ProviderError> {
        if self.done {
            return Err(ProviderError::protocol("Chat data received after [DONE]"));
        }
        if let Some(name) = event.event.as_deref()
            && name != "message"
            && name != "error"
        {
            return Err(ProviderError::protocol("Unsupported Chat SSE event"));
        }
        if event.data.trim() == "[DONE]" {
            if event.event.as_deref() == Some("error") {
                return Err(ProviderError::protocol("Chat error event contained [DONE]"));
            }
            let stop_reason = self
                .finish_reason
                .clone()
                .ok_or_else(|| ProviderError::protocol("Chat [DONE] before finish_reason"))?;
            self.done = true;
            return Ok(vec![ResponseChunk::ResponseEnded { stop_reason }]);
        }
        let value: Value = serde_json::from_str(&event.data)
            .map_err(|_| ProviderError::protocol("Invalid Chat SSE JSON"))?;
        if value.get("error").is_some_and(|value| !value.is_null())
            || event.event.as_deref() == Some("error")
        {
            return Err(super::errors::classify_error(None, &value));
        }
        let wire::Chunk {
            object,
            choices,
            usage,
        } = serde_json::from_value(value)
            .map_err(|_| ProviderError::protocol("Invalid Chat chunk shape"))?;
        if object
            .as_deref()
            .is_some_and(|kind| kind != "chat.completion.chunk")
        {
            return Err(ProviderError::protocol("Expected chat.completion.chunk"));
        }
        if choices.len() > 1 {
            return Err(ProviderError::protocol(
                "Chat codec supports exactly one choice",
            ));
        }
        let mut chunks = Vec::new();
        if let Some(choice) = choices.first() {
            if self.finish_reason.is_some() {
                return Err(ProviderError::protocol(
                    "Chat choice received after finish_reason",
                ));
            }
            if !matches!(
                choice.index,
                wire::ChoiceIndex::Missing | wire::ChoiceIndex::Number(0)
            ) {
                return Err(ProviderError::protocol(
                    "Chat choice index must be zero or absent",
                ));
            }
            self.delta(&choice.delta, &mut chunks)?;
            if let Some(reason) = choice.finish_reason.as_deref() {
                let stop_reason = match reason {
                    "stop" => StopReason::EndTurn,
                    "length" => StopReason::MaxTokens,
                    "abort" => StopReason::Aborted,
                    "tool_calls" if !self.tool_ids.is_empty() => StopReason::ToolUse,
                    "tool_calls" => {
                        return Err(ProviderError::protocol(
                            "Chat finished tool_calls without any tool calls",
                        ));
                    }
                    "content_filter" => StopReason::ContentFilter,
                    _ => {
                        return Err(ProviderError::protocol("Unsupported Chat finish_reason"));
                    }
                };
                self.end_blocks(
                    &mut chunks,
                    matches!(
                        stop_reason,
                        StopReason::MaxTokens | StopReason::Aborted | StopReason::ContentFilter
                    ),
                )?;
                self.finish_reason = Some(stop_reason);
            }
        }
        if let Some(usage) = usage {
            // Prompt totals are monotone, but uncached input may fall when a
            // later packet refines the cached-token breakdown.
            let prompt = usage.prompt_tokens;
            let usage = decode_usage(usage, self.usage.cached_input_tokens)?;
            if prompt < self.raw_prompt_tokens
                || usage.cached_input_tokens < self.usage.cached_input_tokens
                || usage.output_tokens < self.usage.output_tokens
            {
                return Err(ProviderError::protocol("Chat usage counters regressed"));
            }
            self.raw_prompt_tokens = prompt;
            self.usage = usage;
            chunks.push(ResponseChunk::UsageUpdated { usage });
        } else if choices.is_empty() {
            return Err(ProviderError::protocol("Empty Chat choices without usage"));
        }
        Ok(chunks)
    }

    fn delta(
        &mut self,
        delta: &wire::Delta,
        chunks: &mut Vec<ResponseChunk>,
    ) -> Result<(), ProviderError> {
        // Null placeholders carry no semantics; reject non-null unknown fields
        // rather than silently losing audio, legacy function_call, etc.
        if delta.extra.values().any(|value| !value.is_null()) {
            return Err(ProviderError::protocol("Unsupported Chat delta field"));
        }
        if delta
            .role
            .as_deref()
            .is_some_and(|role| role != "assistant")
        {
            return Err(ProviderError::protocol("Chat delta role must be assistant"));
        }
        let primary = delta
            .reasoning_content
            .as_deref()
            .filter(|text| !text.is_empty());
        let alias = delta.reasoning.as_deref().filter(|text| !text.is_empty());
        if let (Some(primary), Some(alias)) = (primary, alias)
            && primary != alias
        {
            return Err(ProviderError::protocol(
                "Conflicting Chat reasoning delta aliases",
            ));
        }
        if let Some(text) = primary.or(alias) {
            self.visible_delta(text, true, chunks)?;
        }
        for text in [delta.content.as_deref(), delta.refusal.as_deref()]
            .into_iter()
            .flatten()
        {
            if !text.is_empty() {
                self.visible_delta(text, false, chunks)?;
            }
        }
        for call in delta.tool_calls.iter().flatten() {
            if call.kind.as_deref().is_some_and(|kind| kind != "function") {
                return Err(ProviderError::protocol(
                    "Only Chat function tool calls are supported",
                ));
            }
            // A Hermes delta may contain a header and argument fragments for
            // the same index. Apply each entry in wire order, not as a map.
            let id = if let Some(id) = self.tool_ids.get(&call.index) {
                *id
            } else {
                let id = self.blocks.len();
                self.blocks.push(Block::Tool {
                    call_id: String::new(),
                    name: String::new(),
                    arguments: String::new(),
                });
                self.tool_ids.insert(call.index, id);
                start_item(chunks, id, ItemKind::ToolCall, BlockKind::ToolCallArguments);
                id
            };
            let Block::Tool {
                call_id,
                name,
                arguments,
            } = &mut self.blocks[id]
            else {
                unreachable!()
            };
            if let Some(fragment) = &call.id {
                call_id.push_str(fragment);
            }
            if let Some(function) = &call.function {
                if let Some(fragment) = &function.name {
                    name.push_str(fragment);
                }
                if let Some(fragment) = &function.arguments {
                    arguments.push_str(fragment);
                    if !fragment.is_empty() {
                        chunks.push(ResponseChunk::BlockDelta {
                            item: id.to_string(),
                            block: "0".into(),
                            delta: ContentDelta::JsonFragment(fragment.clone()),
                        });
                    }
                }
            }
        }
        Ok(())
    }

    fn visible_delta(
        &mut self,
        text: &str,
        reasoning: bool,
        chunks: &mut Vec<ResponseChunk>,
    ) -> Result<(), ProviderError> {
        let same = self.visible_id.filter(|id| {
            matches!(
                (&self.blocks[*id], reasoning),
                (Block::Reasoning(_), true) | (Block::Text(_), false)
            )
        });
        let id = if let Some(id) = same {
            id
        } else {
            if let Some(id) = self.visible_id.take() {
                let content = match &self.blocks[id] {
                    Block::Text(text) => BlockContent::Text { text: text.clone() },
                    Block::Reasoning(text) => BlockContent::Reasoning { text: text.clone() },
                    _ => unreachable!(),
                };
                self.end_item(chunks, id, content);
                self.ended.insert(id);
            }
            let id = self.blocks.len();
            self.blocks.push(if reasoning {
                Block::Reasoning(String::new())
            } else {
                Block::Text(String::new())
            });
            self.visible_id = Some(id);
            start_item(
                chunks,
                id,
                if reasoning {
                    ItemKind::Reasoning
                } else {
                    ItemKind::Text
                },
                if reasoning {
                    BlockKind::Reasoning
                } else {
                    BlockKind::Text
                },
            );
            id
        };
        match &mut self.blocks[id] {
            Block::Text(full) | Block::Reasoning(full) => full.push_str(text),
            _ => unreachable!(),
        }
        chunks.push(ResponseChunk::BlockDelta {
            item: id.to_string(),
            block: "0".into(),
            delta: ContentDelta::Text(text.into()),
        });
        Ok(())
    }

    fn end_blocks(
        &self,
        chunks: &mut Vec<ResponseChunk>,
        discard_tools: bool,
    ) -> Result<(), ProviderError> {
        let mut call_ids = BTreeSet::new();
        for (id, block) in self.blocks.iter().enumerate() {
            if self.ended.contains(&id) {
                continue;
            }
            let block = match block {
                Block::Text(text) => BlockContent::Text { text: text.clone() },
                Block::Reasoning(text) => BlockContent::Reasoning { text: text.clone() },
                Block::Tool {
                    call_id,
                    name,
                    arguments,
                } => {
                    if discard_tools {
                        chunks.push(ResponseChunk::ItemDiscarded { id: id.to_string() });
                        continue;
                    }
                    if call_id.is_empty() || !call_ids.insert(call_id) || !valid_name(name) {
                        return Err(ProviderError::protocol(
                            "Chat tool calls require distinct nonempty IDs and valid function names",
                        ));
                    }
                    let arguments: Value = serde_json::from_str(arguments)
                        .map_err(|_| ProviderError::protocol("Invalid Chat tool arguments JSON"))?;
                    if !arguments.is_object() {
                        return Err(ProviderError::protocol(
                            "Chat tool arguments must be a JSON object",
                        ));
                    }
                    BlockContent::ToolCall(ToolCall {
                        id: call_id.clone(),
                        name: name.clone(),
                        arguments,
                    })
                }
            };
            self.end_item(chunks, id, block);
        }
        Ok(())
    }

    pub(crate) fn finish(&mut self) -> Result<Vec<ResponseChunk>, ProviderError> {
        if self.failed {
            return Err(ProviderError::protocol("Chat stream already failed"));
        }
        if self.done {
            return Ok(Vec::new());
        }
        let Some(stop_reason) = self.finish_reason.clone() else {
            self.failed = true;
            return Err(ProviderError::protocol(
                "Chat stream ended before finish_reason",
            ));
        };
        self.done = true;
        Ok(vec![ResponseChunk::ResponseEnded { stop_reason }])
    }
}

fn start_item(chunks: &mut Vec<ResponseChunk>, id: usize, kind: ItemKind, block_kind: BlockKind) {
    chunks.push(ResponseChunk::ItemStarted {
        id: id.to_string(),
        position: id,
        kind,
    });
    chunks.push(ResponseChunk::BlockStarted {
        item: id.to_string(),
        id: "0".into(),
        position: 0,
        kind: block_kind,
    });
}

impl Decoder {
    fn end_item(&self, chunks: &mut Vec<ResponseChunk>, id: usize, content: BlockContent) {
        // The transport wrapper binds the provider+endpoint scope. llama-swap
        // injected reasoning is indistinguishable on this wire and receives
        // the same origin scope, never a fabricated separate provenance.
        let replay = match &content {
            BlockContent::Reasoning { text } => Some(reasoning_envelope(
                "chat_completions",
                &self.model,
                json!({"text": text}),
            )),
            _ => None,
        };
        chunks.push(ResponseChunk::BlockEnded {
            item: id.to_string(),
            block: "0".into(),
            content,
        });
        chunks.push(ResponseChunk::ItemEnded {
            id: id.to_string(),
            replay,
        });
    }
}

fn decode_usage(value: wire::Usage, previous_cached: u64) -> Result<Usage, ProviderError> {
    let input_tokens = value.prompt_tokens;
    let output_tokens = value.completion_tokens;
    if let Some(total) = value.total_tokens
        && input_tokens.checked_add(output_tokens) != Some(total)
    {
        return Err(ProviderError::protocol(
            "Chat usage total_tokens does not match prompt + completion",
        ));
    }
    // Missing/null cache details do not erase a previously reported breakdown.
    let cached_input_tokens = value
        .prompt_tokens_details
        .and_then(|details| details.cached_tokens)
        .unwrap_or(previous_cached);
    if cached_input_tokens > input_tokens {
        return Err(ProviderError::protocol(
            "Chat cached_tokens exceeds prompt_tokens",
        ));
    }
    Ok(Usage {
        input_tokens: input_tokens - cached_input_tokens,
        cached_input_tokens,
        output_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::protocol::AssistantItem;
    use crate::{
        media::ImageReference,
        provider::protocol::{SystemSegment, ToolDefinition, ToolResult},
    };

    fn encode(request: &ModelRequest) -> Result<Value, ProviderError> {
        super::encode(request, ChatReasoningReplay::Unsupported)
    }

    fn request() -> ModelRequest {
        ModelRequest {
            model: "gpt-5".into(),
            system: vec![],
            messages: vec![],
            tools: vec![],
            response_schema: None,
            reasoning: None,
            max_output_tokens: None,
            correlation: Some("internal-do-not-transmit".into()),
        }
    }

    fn image() -> ImageReference {
        ImageReference {
            sha256: "digest".into(),
            media_type: "image/png".into(),
            name: "image.png".into(),
            bytes: 3,
            data_base64: Some("AQID".into()),
        }
    }

    fn event(value: Value) -> SseEvent {
        SseEvent {
            event: None,
            data: value.to_string(),
        }
    }

    fn delta(value: Value) -> SseEvent {
        event(
            json!({"object":"chat.completion.chunk", "choices":[{"index":0,"delta":value,"finish_reason":null}]}),
        )
    }

    fn end(reason: &str) -> SseEvent {
        event(json!({"choices":[{"index":0,"delta":{},"finish_reason":reason}]}))
    }

    fn done() -> SseEvent {
        SseEvent {
            event: None,
            data: "[DONE]".into(),
        }
    }

    fn usage() -> SseEvent {
        event(
            json!({"choices":[],"usage":{"prompt_tokens":20,"completion_tokens":5,
            "total_tokens":25,"prompt_tokens_details":{"cached_tokens":8},
            "completion_tokens_details":{"reasoning_tokens":3}}}),
        )
    }

    // Captured shapes from the authorized local qwen36-35 service: role/content
    // null, reasoning_content strings, answer strings, empty finish delta, usage.
    // Text is synthetic: regressions never persist private live reasoning.
    #[test]
    fn qwen_reasoning_stream_is_visible_separate_and_not_replayed() {
        use crate::provider::protocol::ResponseAssembler;
        let mut decoder = Decoder::new("qwen36-35".into());
        let mut assembler = ResponseAssembler::default();
        for frame in [
            delta(json!({"reasoning_content":"private one"})),
            delta(json!({"content":"hello "})),
            delta(json!({"reasoning":"private two"})),
            delta(json!({"content":"世界"})),
            end("stop"),
            usage(),
            done(),
        ] {
            for chunk in decoder.decode(&frame).unwrap() {
                assembler.push(&chunk).unwrap();
            }
        }
        let (items, usage, reason) = assembler.finish().unwrap();
        assert_eq!(reason, StopReason::EndTurn);
        assert_eq!(
            usage,
            Usage {
                input_tokens: 12,
                cached_input_tokens: 8,
                output_tokens: 5
            }
        );
        assert_eq!(
            items.iter().map(|i| i.kind).collect::<Vec<_>>(),
            vec![
                ItemKind::Reasoning,
                ItemKind::Text,
                ItemKind::Reasoning,
                ItemKind::Text
            ]
        );
        assert!(items.iter().enumerate().all(|(n, i)| i.position == n
            && i.blocks.len() == 1
            && i.replay.is_some() == (i.kind == ItemKind::Reasoning)));
        let mut request = request();
        request.messages = vec![Message::Assistant(items)];
        let wire = encode(&request).unwrap();
        assert_eq!(wire["messages"][0]["content"], "hello 世界");
        assert!(!wire.to_string().contains("private"));
    }

    #[test]
    fn request_preserves_text_tools_images_and_reasoning_settings() {
        let mut request = request();
        request.system = vec![SystemSegment {
            text: "system".into(),
            cache: true,
        }];
        request.reasoning = Some("high".into());
        request.tools = vec![ToolDefinition {
            name: "inspect".into(),
            description: "Inspect".into(),
            input_schema: json!({"type":"object"}),
        }];
        request.messages = vec![
            Message::User(vec![
                UserContent::Text {
                    text: "look".into(),
                },
                UserContent::Image { image: image() },
            ]),
            Message::Assistant(vec![
                AssistantItem::text("text", 0, "Checking"),
                AssistantItem::tool_call(
                    "call",
                    1,
                    ToolCall {
                        id: "call-a".into(),
                        name: "inspect".into(),
                        arguments: json!({"path":"a"}),
                    },
                ),
            ]),
            Message::Tool(vec![ToolResult {
                call_id: "call-a".into(),
                name: "inspect".into(),
                result: json!({"ok":false}),
                is_error: true,
                images: vec![image()],
            }]),
        ];
        let body = encode(&request).unwrap();
        assert_eq!(body["messages"][0]["content"], "system");
        assert_eq!(body["messages"][1]["content"][0]["text"], "look");
        assert_eq!(
            body["messages"][1]["content"][1]["image_url"]["url"],
            "data:image/png;base64,AQID"
        );
        assert_eq!(body["messages"][2]["content"], "Checking");
        assert_eq!(body["messages"][2]["tool_calls"][0]["id"], "call-a");
        assert_eq!(body["messages"][3]["tool_call_id"], "call-a");
        let result: Value =
            serde_json::from_str(body["messages"][3]["content"].as_str().unwrap()).unwrap();
        assert_eq!(result, json!({"result":{"ok":false},"is_error":true}));
        assert_eq!(
            body["messages"][4]["content"][1]["image_url"]["url"],
            "data:image/png;base64,AQID"
        );
        assert_eq!(body["tools"][0]["function"]["name"], "inspect");
        assert_eq!(body["reasoning_effort"], "high");
    }

    #[test]
    fn interleaved_tools_get_first_seen_ids_and_authoritative_arguments() {
        use crate::provider::protocol::ResponseAssembler;
        let mut decoder = Decoder::new("gpt-5".into());
        let mut assembler = ResponseAssembler::default();
        let frames = [
            delta(
                json!({"tool_calls":[{"index":7,"id":"call-","function":{"name":"fir","arguments":"{\"a\":"}},{"index":2,"id":"call-b","function":{"name":"second","arguments":"{"}}]}),
            ),
            delta(json!({"content":"working"})),
            delta(
                json!({"tool_calls":[{"index":2,"function":{"arguments":"}"}},{"index":7,"id":"a","function":{"name":"st","arguments":"1}"}}]}),
            ),
            end("tool_calls"),
            done(),
        ];
        let mut fragments = 0;
        for frame in frames {
            for chunk in decoder.decode(&frame).unwrap() {
                if matches!(
                    chunk,
                    ResponseChunk::BlockDelta {
                        delta: ContentDelta::JsonFragment(_),
                        ..
                    }
                ) {
                    fragments += 1;
                }
                assembler.push(&chunk).unwrap();
            }
        }
        let (items, _, reason) = assembler.finish().unwrap();
        assert_eq!(fragments, 4);
        assert_eq!(reason, StopReason::ToolUse);
        assert_eq!(
            items[0].blocks[0].content,
            BlockContent::ToolCall(ToolCall {
                id: "call-a".into(),
                name: "first".into(),
                arguments: json!({"a":1})
            })
        );
        assert_eq!(
            items[1].blocks[0].content,
            BlockContent::ToolCall(ToolCall {
                id: "call-b".into(),
                name: "second".into(),
                arguments: json!({})
            })
        );
    }

    #[test]
    fn eof_after_finish_reason_flushes_after_usage() {
        for reason in ["stop", "length"] {
            let mut decoder = Decoder::new("gpt-5".into());
            decoder.decode(&delta(json!({"content":"answer"}))).unwrap();
            let terminal = decoder.decode(&end(reason)).unwrap();
            assert!(matches!(
                &terminal[..],
                [
                    ResponseChunk::BlockEnded { .. },
                    ResponseChunk::ItemEnded { .. }
                ]
            ));
            decoder.decode(&usage()).unwrap();
            assert_eq!(
                decoder.finish().unwrap(),
                vec![ResponseChunk::ResponseEnded {
                    stop_reason: if reason == "length" {
                        StopReason::MaxTokens
                    } else {
                        StopReason::EndTurn
                    }
                }]
            );
        }
    }

    #[test]
    fn rejects_premature_eof_done_and_post_terminal_data() {
        let mut decoder = Decoder::new("gpt-5".into());
        assert!(decoder.finish().is_err());
        let mut decoder = Decoder::new("gpt-5".into());
        decoder
            .decode(&delta(json!({"content":"partial"})))
            .unwrap();
        assert!(decoder.decode(&done()).is_err());
        assert!(decoder.finish().is_err());
        let mut decoder = Decoder::new("gpt-5".into());
        decoder.decode(&end("stop")).unwrap();
        assert!(decoder.decode(&delta(json!({"content":"late"}))).is_err());
        let mut decoder = Decoder::new("gpt-5".into());
        decoder.decode(&end("stop")).unwrap();
        decoder.decode(&done()).unwrap();
        assert!(decoder.decode(&usage()).is_err());
    }
}
