//! The standard Anthropic Messages wire protocol (no vendor or OAuth dialects).
use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::provider::{
    ProviderError, ProviderErrorKind,
    protocol::{
        BlockContent, BlockKind, ContentDelta, ItemKind, Message, ModelRequest, ReplayEnvelope,
        ResponseChunk, StopReason, ToolCall, Usage, UserContent,
    },
};

use super::{
    common::{anthropic_image, invalid, opaque_payload, reasoning_envelope, tool_text},
    transport::SseEvent,
};

pub(crate) fn encode(request: &ModelRequest) -> Result<Value, ProviderError> {
    if request.model.trim().is_empty() {
        return Err(invalid("Anthropic requires a nonempty model"));
    }
    let max_tokens = request
        .max_output_tokens
        .filter(|n| *n > 0)
        .ok_or_else(|| {
            invalid(
                "Anthropic requires max_output_tokens to be explicitly set to a positive integer",
            )
        })?;
    let mut body = json!({
        "model": request.model,
        "max_tokens": max_tokens,
        "stream": true,
    });
    let mut system = Vec::new();
    let mut cache_breakpoints = 0;
    for segment in &request.system {
        let mut block = json!({"type": "text", "text": segment.text});
        if segment.cache {
            cache_breakpoints += 1;
            block["cache_control"] = json!({"type": "ephemeral"});
        }
        system.push(block);
    }
    if cache_breakpoints > 4 {
        return Err(invalid("Anthropic supports at most four cache breakpoints"));
    }
    if !system.is_empty() {
        body["system"] = Value::Array(system);
    }
    let mut messages: Vec<Value> = Vec::new();
    for message in &request.messages {
        let (role, content) = match message {
            Message::User(items) => {
                let mut blocks = Vec::new();
                for item in items {
                    blocks.push(match item {
                        UserContent::Text { text }
                        | UserContent::Runtime { text }
                        | UserContent::ParentInput { text }
                        | UserContent::Compaction { text } => json!({"type":"text", "text":text}),
                        UserContent::Image { image } => anthropic_image(image)?,
                    });
                }
                ("user", blocks)
            }
            Message::Assistant(items) => {
                let mut blocks = Vec::new();
                for item in items {
                    // Replay belongs to the native item, not to each display block.
                    if let Some(payload) = opaque_payload(&item.replay, "anthropic", &request.model)
                    {
                        validate_thinking(payload).map_err(|error| invalid(error.message))?;
                        blocks.push(payload.clone());
                        continue;
                    }
                    for block in &item.blocks {
                        match &block.content {
                            BlockContent::Text { text } => {
                                blocks.push(json!({"type":"text", "text":text}))
                            }
                            // Unsigned/foreign private reasoning is display-only.
                            BlockContent::Reasoning { .. } => {}
                            BlockContent::ToolCall(call) => {
                                if call.id.is_empty()
                                    || call.name.is_empty()
                                    || !call.arguments.is_object()
                                {
                                    return Err(invalid(
                                        "Anthropic tool calls require nonempty id/name and object arguments",
                                    ));
                                }
                                blocks.push(json!({"type":"tool_use", "id":call.id, "name":call.name, "input":call.arguments}));
                            }
                        }
                    }
                }
                ("assistant", blocks)
            }
            Message::Tool(results) => {
                let mut blocks = Vec::new();
                for result in results {
                    if result.call_id.is_empty() {
                        return Err(invalid("Anthropic tool results require a nonempty call_id"));
                    }
                    let mut content = vec![json!({"type":"text", "text":tool_text(result)})];
                    for image in &result.images {
                        content.push(anthropic_image(image)?);
                    }
                    blocks.push(json!({"type":"tool_result", "tool_use_id":result.call_id,
                        "content":content, "is_error":result.is_error}));
                }
                ("user", blocks)
            }
        };
        if content.is_empty() {
            // Empty foreign redacted reasoning contributes no replayable content.
            if matches!(message, Message::Assistant(items) if !items.is_empty()
                && items.iter().all(|item| item.kind == ItemKind::Reasoning))
            {
                continue;
            }
            return Err(invalid(
                "Anthropic messages must contain at least one content block",
            ));
        }
        if let Some(previous) = messages
            .last_mut()
            .filter(|previous| previous["role"] == role)
        {
            previous["content"]
                .as_array_mut()
                .expect("constructed array")
                .extend(content);
        } else {
            messages.push(json!({"role":role, "content":content}));
        }
    }
    if messages.is_empty() {
        return Err(invalid("Anthropic requires at least one message"));
    }
    body["messages"] = Value::Array(messages);
    if !request.tools.is_empty() {
        let mut tools = Vec::new();
        for tool in &request.tools {
            if tool.name.is_empty() || !tool.input_schema.is_object() {
                return Err(invalid(
                    "Anthropic tools require a nonempty name and object input_schema",
                ));
            }
            tools.push(json!({"name":tool.name, "description":tool.description,
                "input_schema":tool.input_schema}));
        }
        body["tools"] = Value::Array(tools);
    }
    let mut output_config = serde_json::Map::new();
    if let Some(schema) = &request.response_schema {
        if !schema.schema.is_object() {
            return Err(invalid(
                "Anthropic response_schema must be a JSON Schema object",
            ));
        }
        // Messages uses output_config.format, not the retired output_format beta field.
        output_config.insert(
            "format".into(),
            json!({"type":"json_schema", "schema":schema.schema}),
        );
    }
    if let Some(reasoning) = &request.reasoning {
        match reasoning.as_str() {
            "off" => body["thinking"] = json!({"type":"disabled"}),
            "adaptive" => body["thinking"] = json!({"type":"adaptive"}),
            "low" | "medium" | "high" | "max" => {
                body["thinking"] = json!({"type":"adaptive"});
                output_config.insert("effort".into(), json!(reasoning));
            }
            value if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) => {
                let budget = value
                    .parse::<u64>()
                    .map_err(|_| invalid("Anthropic thinking budget exceeds u64"))?;
                if budget < 1024 || budget >= max_tokens {
                    return Err(invalid(
                        "Anthropic manual thinking budget must be >= 1024 and strictly less than max_output_tokens",
                    ));
                }
                body["thinking"] = json!({"type":"enabled", "budget_tokens":budget});
            }
            _ => {
                return Err(invalid(
                    "Unsupported Anthropic reasoning setting: use off, adaptive, low, medium, high, max, or an integer thinking-token budget",
                ));
            }
        }
    }
    if !output_config.is_empty() {
        body["output_config"] = Value::Object(output_config);
    }
    // correlation is local tracing information, not Anthropic user metadata.
    Ok(body)
}

fn protocol(message: impl Into<String>) -> ProviderError {
    ProviderError::protocol(format!("Anthropic: {}", message.into()))
}

fn string<'a>(value: &'a Value, field: &str) -> Result<&'a str, ProviderError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| protocol(format!("missing or invalid string field {field}")))
}

fn validate_thinking(value: &Value) -> Result<(), ProviderError> {
    match string(value, "type")? {
        "thinking" => {
            string(value, "thinking")?;
            if string(value, "signature")?.is_empty() {
                return Err(protocol("thinking block has an empty signature"));
            }
        }
        "redacted_thinking" => {
            if string(value, "data")?.is_empty() {
                return Err(protocol("redacted thinking block has empty data"));
            }
        }
        _ => return Err(protocol("opaque reasoning is not a native thinking block")),
    }
    Ok(())
}

struct Block {
    native: Value,
    partial_json: String,
    has_json_delta: bool,
    ended: bool,
    completed: Option<(BlockContent, Option<ReplayEnvelope>)>,
}

#[derive(Default, Clone, Copy)]
struct NativeUsage {
    input: u64,
    creation: u64,
    cached: u64,
    output: u64,
}

/// One decoder owns exactly one Messages response. Native indices identify blocks;
/// completed blocks are emitted in index order even if the SSE blocks interleave.
pub(crate) struct Decoder {
    model: String,
    started: bool,
    message_delta: bool,
    stopped: bool,
    failed: bool,
    stop_reason: Option<StopReason>,
    blocks: BTreeMap<usize, Block>,
    next_end: usize,
    usage: NativeUsage,
}

impl Decoder {
    pub(crate) fn new(model: String) -> Self {
        Self {
            model,
            started: false,
            message_delta: false,
            stopped: false,
            failed: false,
            stop_reason: None,
            blocks: BTreeMap::new(),
            next_end: 0,
            usage: NativeUsage::default(),
        }
    }

    pub(crate) fn decode(&mut self, event: &SseEvent) -> Result<Vec<ResponseChunk>, ProviderError> {
        if self.failed {
            return Err(protocol("decoder has already failed"));
        }
        let result = self.decode_inner(event);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn decode_inner(&mut self, event: &SseEvent) -> Result<Vec<ResponseChunk>, ProviderError> {
        if self.stopped {
            return Err(protocol("event after message_stop"));
        }
        let value: Value =
            serde_json::from_str(&event.data).map_err(|_| protocol("invalid SSE JSON"))?;
        let kind = string(&value, "type")?;
        if let Some(name) = &event.event
            && name != kind
        {
            return Err(protocol("SSE event name disagrees with data type"));
        }
        if kind == "error" {
            let error = value
                .get("error")
                .ok_or_else(|| protocol("missing error object"))?;
            let error_type = string(error, "type")?;
            let message = string(error, "message")?.to_owned();
            let kind = match error_type {
                "authentication_error" | "permission_error" => ProviderErrorKind::Authentication,
                "rate_limit_error" => ProviderErrorKind::RateLimited,
                "invalid_request_error"
                    if message.to_ascii_lowercase().contains("prompt is too long") =>
                {
                    ProviderErrorKind::ContextWindowExceeded
                }
                "invalid_request_error" | "not_found_error" | "request_too_large" => {
                    ProviderErrorKind::InvalidRequest
                }
                _ => ProviderErrorKind::Response,
            };
            return Err(ProviderError {
                kind,
                message: match error_type {
                    "authentication_error" => "Anthropic authentication_error",
                    "permission_error" => "Anthropic permission_error",
                    "rate_limit_error" => "Anthropic rate_limit_error",
                    "invalid_request_error" => "Anthropic invalid_request_error",
                    "not_found_error" => "Anthropic not_found_error",
                    "request_too_large" => "Anthropic request_too_large",
                    "overloaded_error" => "Anthropic overloaded_error",
                    "api_error" => "Anthropic api_error",
                    _ => "Anthropic server error",
                }
                .into(),
            });
        }
        if kind == "ping" {
            return Ok(Vec::new());
        }
        if kind == "message_start" {
            if self.started {
                return Err(protocol("duplicate message_start"));
            }
            let message = value
                .get("message")
                .ok_or_else(|| protocol("missing message"))?;
            if string(message, "type")? != "message" || string(message, "role")? != "assistant" {
                return Err(protocol("message_start is not an assistant message"));
            }
            if string(message, "id")?.is_empty() || string(message, "model")?.is_empty() {
                return Err(protocol("message_start requires nonempty id and model"));
            }
            match message.get("content").and_then(Value::as_array) {
                Some(content) if content.is_empty() => {}
                _ => return Err(protocol("message_start content must be an empty array")),
            }
            if message
                .get("stop_reason")
                .is_some_and(|reason| !reason.is_null())
            {
                return Err(protocol("message_start already has a stop_reason"));
            }
            let usage = self.update_usage(
                message
                    .get("usage")
                    .ok_or_else(|| protocol("message_start missing usage"))?,
                true,
            )?;
            self.started = true;
            return Ok(vec![ResponseChunk::UsageUpdated { usage }]);
        }
        if !self.started {
            return Err(protocol("event before message_start"));
        }
        match kind {
            "content_block_start" => {
                self.require_content_phase()?;
                let id = index(&value)?;
                if self.blocks.contains_key(&id) {
                    return Err(protocol(format!("duplicate content block index {id}")));
                }
                let native = value
                    .get("content_block")
                    .ok_or_else(|| protocol("missing content_block"))?
                    .clone();
                let (kind, block_kind) = match string(&native, "type")? {
                    "text" => (ItemKind::Text, BlockKind::Text),
                    "thinking" | "redacted_thinking" => (ItemKind::Reasoning, BlockKind::Reasoning),
                    "tool_use" => (ItemKind::ToolCall, BlockKind::ToolCallArguments),
                    _ => return Err(protocol("unsupported content block type")),
                };
                let mut chunks = vec![
                    ResponseChunk::ItemStarted {
                        id: id.to_string(),
                        position: id,
                        kind,
                    },
                    ResponseChunk::BlockStarted {
                        item: id.to_string(),
                        id: "0".into(),
                        position: 0,
                        kind: block_kind,
                    },
                ];
                match string(&native, "type")? {
                    "text" => {
                        reject_citations(&native)?;
                        let text = string(&native, "text")?;
                        if !text.is_empty() {
                            chunks.push(ResponseChunk::BlockDelta {
                                item: id.to_string(),
                                block: "0".into(),
                                delta: ContentDelta::Text(text.into()),
                            });
                        }
                    }
                    "thinking" => {
                        let text = string(&native, "thinking")?;
                        string(&native, "signature")?;
                        if !text.is_empty() {
                            chunks.push(ResponseChunk::BlockDelta {
                                item: id.to_string(),
                                block: "0".into(),
                                delta: ContentDelta::Text(text.into()),
                            });
                        }
                    }
                    "redacted_thinking" => validate_thinking(&native)?,
                    "tool_use" => {
                        if string(&native, "id")?.is_empty()
                            || string(&native, "name")?.is_empty()
                            || !native.get("input").is_some_and(Value::is_object)
                        {
                            return Err(protocol(
                                "tool_use requires nonempty id/name and object input",
                            ));
                        }
                    }
                    _ => return Err(protocol("unsupported content block type")),
                }
                self.blocks.insert(
                    id,
                    Block {
                        native,
                        partial_json: String::new(),
                        has_json_delta: false,
                        ended: false,
                        completed: None,
                    },
                );
                Ok(chunks)
            }
            "content_block_delta" => {
                self.require_content_phase()?;
                let id = index(&value)?;
                let block = self
                    .blocks
                    .get_mut(&id)
                    .filter(|block| !block.ended)
                    .ok_or_else(|| protocol(format!("delta for unopened or ended block {id}")))?;
                let delta = value
                    .get("delta")
                    .ok_or_else(|| protocol("missing content delta"))?;
                let kind = string(delta, "type")?;
                let block_kind = string(&block.native, "type")?;
                match (block_kind, kind) {
                    ("text", "text_delta") => {
                        let text = string(delta, "text")?;
                        append(&mut block.native, "text", text)?;
                        Ok(vec![ResponseChunk::BlockDelta {
                            item: id.to_string(),
                            block: "0".into(),
                            delta: ContentDelta::Text(text.into()),
                        }])
                    }
                    ("thinking", "thinking_delta") => {
                        let text = string(delta, "thinking")?;
                        append(&mut block.native, "thinking", text)?;
                        Ok(vec![ResponseChunk::BlockDelta {
                            item: id.to_string(),
                            block: "0".into(),
                            delta: ContentDelta::Text(text.into()),
                        }])
                    }
                    ("thinking", "signature_delta") => {
                        append(&mut block.native, "signature", string(delta, "signature")?)?;
                        Ok(Vec::new())
                    }
                    ("tool_use", "input_json_delta") => {
                        if block.native["input"]
                            .as_object()
                            .is_some_and(|input| !input.is_empty())
                        {
                            return Err(protocol(
                                "tool_use has both initial input and streamed input",
                            ));
                        }
                        block.partial_json.push_str(string(delta, "partial_json")?);
                        block.has_json_delta = true;
                        Ok(vec![ResponseChunk::BlockDelta {
                            item: id.to_string(),
                            block: "0".into(),
                            delta: ContentDelta::JsonFragment(
                                string(delta, "partial_json")?.into(),
                            ),
                        }])
                    }
                    _ => Err(protocol("unsupported delta for content block type")),
                }
            }
            "content_block_stop" => {
                self.require_content_phase()?;
                let id = index(&value)?;
                let block = self
                    .blocks
                    .get_mut(&id)
                    .filter(|block| !block.ended)
                    .ok_or_else(|| protocol(format!("stop for unopened or ended block {id}")))?;
                let mut replay = None;
                let completed = match string(&block.native, "type")? {
                    "text" => BlockContent::Text {
                        text: string(&block.native, "text")?.into(),
                    },
                    "thinking" | "redacted_thinking" => {
                        validate_thinking(&block.native)?;
                        replay = Some(reasoning_envelope(
                            "anthropic",
                            &self.model,
                            block.native.clone(),
                        ));
                        BlockContent::Reasoning {
                            text: block
                                .native
                                .get("thinking")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .into(),
                        }
                    }
                    "tool_use" => {
                        let arguments = if block.has_json_delta {
                            serde_json::from_str::<Value>(&block.partial_json)
                                .map_err(|_| protocol("invalid tool input JSON"))?
                        } else {
                            block.native["input"].clone()
                        };
                        if !arguments.is_object() {
                            return Err(protocol("tool input must be a JSON object"));
                        }
                        BlockContent::ToolCall(ToolCall {
                            id: string(&block.native, "id")?.into(),
                            name: string(&block.native, "name")?.into(),
                            arguments,
                        })
                    }
                    _ => return Err(protocol("unsupported completed content block")),
                };
                block.ended = true;
                block.completed = Some((completed, replay));
                let mut chunks = Vec::new();
                while let Some(block) = self
                    .blocks
                    .get_mut(&self.next_end)
                    .filter(|block| block.ended)
                {
                    let (content, replay) = block
                        .completed
                        .take()
                        .ok_or_else(|| protocol("block was already emitted"))?;
                    chunks.push(ResponseChunk::BlockEnded {
                        item: self.next_end.to_string(),
                        block: "0".into(),
                        content,
                    });
                    chunks.push(ResponseChunk::ItemEnded {
                        id: self.next_end.to_string(),
                        replay,
                    });
                    self.next_end = self
                        .next_end
                        .checked_add(1)
                        .ok_or_else(|| protocol("block index overflow"))?;
                }
                Ok(chunks)
            }
            "message_delta" => {
                self.require_all_blocks_ended()?;
                let delta = value
                    .get("delta")
                    .ok_or_else(|| protocol("missing message delta"))?;
                let reason = string(delta, "stop_reason")?;
                let stop_reason = match reason {
                    "max_tokens" | "model_context_window_exceeded" => StopReason::MaxTokens,
                    "end_turn" => StopReason::EndTurn,
                    "stop_sequence" => StopReason::StopSequence,
                    "tool_use" => StopReason::ToolUse,
                    "refusal" => StopReason::ContentFilter,
                    "pause_turn" => StopReason::Other(reason.into()),
                    _ => return Err(protocol("unsupported stop_reason")),
                };
                if self.message_delta {
                    return Err(protocol("duplicate terminal message_delta"));
                }
                let usage = self.update_usage(
                    value
                        .get("usage")
                        .ok_or_else(|| protocol("message_delta missing usage"))?,
                    false,
                )?;
                self.stop_reason = Some(stop_reason);
                self.message_delta = true;
                Ok(vec![ResponseChunk::UsageUpdated { usage }])
            }
            "message_stop" => {
                self.require_all_blocks_ended()?;
                if !self.message_delta {
                    return Err(protocol("message_stop before terminal message_delta"));
                }
                self.stopped = true;
                Ok(vec![ResponseChunk::ResponseEnded {
                    stop_reason: self
                        .stop_reason
                        .clone()
                        .ok_or_else(|| protocol("missing stop reason"))?,
                }])
            }
            _ => Err(protocol("unsupported SSE event")),
        }
    }

    fn require_content_phase(&self) -> Result<(), ProviderError> {
        if self.message_delta {
            Err(protocol("content event after terminal message_delta"))
        } else {
            Ok(())
        }
    }

    fn require_all_blocks_ended(&self) -> Result<(), ProviderError> {
        if self.blocks.values().any(|block| !block.ended) {
            return Err(protocol("message ended with open content blocks"));
        }
        if self.next_end != self.blocks.len() {
            return Err(protocol(
                "content block indices are not contiguous from zero",
            ));
        }
        Ok(())
    }

    fn update_usage(&mut self, value: &Value, initial: bool) -> Result<Usage, ProviderError> {
        if !value.is_object() {
            return Err(protocol("usage must be an object"));
        }
        let previous = self.usage;
        let next = NativeUsage {
            input: counter(value, "input_tokens", previous.input, initial)?,
            creation: counter(
                value,
                "cache_creation_input_tokens",
                previous.creation,
                false,
            )?,
            cached: counter(value, "cache_read_input_tokens", previous.cached, false)?,
            output: counter(value, "output_tokens", previous.output, true)?,
        };
        // Runtime adds cache reads separately; cache writes are uncached input.
        let input_tokens = next
            .input
            .checked_add(next.creation)
            .ok_or_else(|| protocol("input token count overflow"))?;
        self.usage = next;
        Ok(Usage {
            input_tokens,
            cached_input_tokens: next.cached,
            output_tokens: next.output,
        })
    }

    pub(crate) fn finish(&mut self) -> Result<Vec<ResponseChunk>, ProviderError> {
        if self.failed {
            return Err(protocol("decoder has already failed"));
        }
        if !self.stopped {
            self.failed = true;
            return Err(protocol("unexpected EOF before message_stop"));
        }
        Ok(Vec::new())
    }
}

fn index(value: &Value) -> Result<usize, ProviderError> {
    value
        .get("index")
        .and_then(Value::as_u64)
        .and_then(|index| usize::try_from(index).ok())
        .ok_or_else(|| protocol("missing or invalid content block index"))
}

fn append(value: &mut Value, field: &str, suffix: &str) -> Result<(), ProviderError> {
    match value.get_mut(field) {
        Some(Value::String(text)) => {
            text.push_str(suffix);
            Ok(())
        }
        _ => Err(protocol(format!("missing or invalid string field {field}"))),
    }
}

fn reject_citations(value: &Value) -> Result<(), ProviderError> {
    if let Some(citations) = value.get("citations").filter(|value| !value.is_null())
        && !citations.as_array().is_some_and(Vec::is_empty)
    {
        return Err(protocol(
            "citations cannot be represented by the response protocol",
        ));
    }
    Ok(())
}

fn counter(value: &Value, key: &str, previous: u64, required: bool) -> Result<u64, ProviderError> {
    match value.get(key) {
        Some(number) => {
            let next = number
                .as_u64()
                .ok_or_else(|| protocol(format!("invalid usage counter {key}")))?;
            if next < previous {
                return Err(protocol(format!("usage counter {key} decreased")));
            }
            Ok(next)
        }
        None if required => Err(protocol(format!("missing usage counter {key}"))),
        None => Ok(previous),
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
            model: "claude-test".into(),
            system: Vec::new(),
            messages: vec![Message::User(vec![UserContent::Text {
                text: "hello".into(),
            }])],
            tools: Vec::new(),
            response_schema: None,
            reasoning: None,
            max_output_tokens: Some(8192),
            correlation: Some("local-trace".into()),
        }
    }

    fn event(value: Value) -> SseEvent {
        SseEvent {
            event: value["type"].as_str().map(str::to_owned),
            data: value.to_string(),
        }
    }

    fn start() -> Value {
        json!({"type":"message_start", "message":{
            "id":"msg_1", "type":"message", "role":"assistant", "model":"claude-test-20260101",
            "content":[], "stop_reason":null, "stop_sequence":null,
            "usage":{"input_tokens":11, "cache_creation_input_tokens":13,
                "cache_read_input_tokens":17, "output_tokens":1}
        }})
    }

    fn started() -> Decoder {
        let mut decoder = Decoder::new("claude-test".into());
        assert_eq!(
            decoder.decode(&event(start())).unwrap(),
            vec![ResponseChunk::UsageUpdated {
                usage: Usage {
                    input_tokens: 24,
                    cached_input_tokens: 17,
                    output_tokens: 1
                }
            }]
        );
        decoder
    }

    fn block_start(index: usize, block: Value) -> Value {
        json!({"type":"content_block_start", "index":index, "content_block":block})
    }

    fn delta(index: usize, delta: Value) -> Value {
        json!({"type":"content_block_delta", "index":index, "delta":delta})
    }

    fn stop(index: usize) -> Value {
        json!({"type":"content_block_stop", "index":index})
    }

    fn terminal(reason: &str, output_tokens: u64) -> Value {
        json!({"type":"message_delta", "delta":{"stop_reason":reason,"stop_sequence":null},
            "usage":{"output_tokens":output_tokens}})
    }

    fn image() -> ImageReference {
        ImageReference {
            sha256: "hash".into(),
            media_type: "image/png".into(),
            name: "test.png".into(),
            bytes: 1,
            data_base64: Some("eA==".into()),
        }
    }

    #[test]
    fn request_maps_native_schema_tools_system_cache_and_adaptive_effort() {
        let mut request = request();
        request.system = vec![SystemSegment {
            text: "cached".into(),
            cache: true,
        }];
        request.tools = vec![ToolDefinition {
            name: "look".into(),
            description: "Look".into(),
            input_schema: json!({"type":"object"}),
        }];
        request.response_schema = Some(ResponseSchema {
            name: "answer".into(),
            schema: json!({"type":"object", "properties":{}, "additionalProperties":false}),
        });
        request.reasoning = Some("high".into());
        request.messages = vec![
            Message::User(vec![
                UserContent::Text {
                    text: "look".into(),
                },
                UserContent::Image { image: image() },
            ]),
            Message::Assistant(vec![AssistantItem::tool_call(
                "0",
                0,
                ToolCall {
                    id: "tool_1".into(),
                    name: "look".into(),
                    arguments: json!({"path":"test.png"}),
                },
            )]),
            Message::Tool(vec![ToolResult {
                call_id: "tool_1".into(),
                name: "look".into(),
                result: json!({"ok":false}),
                images: vec![image()],
                is_error: true,
            }]),
        ];
        let body = encode(&request).unwrap();
        assert_eq!(body["max_tokens"], 8192);
        assert_eq!(
            body["system"][0]["cache_control"],
            json!({"type":"ephemeral"})
        );
        assert_eq!(
            body["tools"][0]["input_schema"],
            request.tools[0].input_schema
        );
        assert_eq!(
            body["output_config"]["format"]["schema"],
            request.response_schema.unwrap().schema
        );
        assert_eq!(body["output_config"]["effort"], "high");
        assert_eq!(body["thinking"], json!({"type":"adaptive"}));
        assert_eq!(body["messages"][0]["content"][0]["text"], "look");
        assert_eq!(body["messages"][0]["content"][1]["source"]["data"], "eA==");
        assert_eq!(body["messages"][1]["content"][0]["type"], "tool_use");
        let result = &body["messages"][2]["content"][0];
        assert_eq!(result["tool_use_id"], "tool_1");
        assert_eq!(result["is_error"], true);
        assert_eq!(result["content"][1]["type"], "image");
    }

    #[test]
    fn opaque_reasoning_replays_only_matching_version_protocol_and_model() {
        use crate::provider::protocol::AssistantBlock;
        let native = json!({"type":"thinking", "thinking":"private", "signature":"signature", "future_field":42});
        let mut request = request();
        let envelope = reasoning_envelope("anthropic", &request.model, native.clone());
        let mut envelopes = vec![(Some(envelope.clone()), true), (None, false)];
        let mut foreign = envelope.clone();
        foreign.version = 2;
        envelopes.push((Some(foreign), false));
        let mut foreign = envelope.clone();
        foreign.protocol = "responses".into();
        envelopes.push((Some(foreign), false));
        let mut foreign = envelope.clone();
        foreign.model = "another-model".into();
        envelopes.push((Some(foreign), false));
        for (replay, matches) in envelopes {
            let mut item = AssistantItem::reasoning("r", 0, "visible", replay);
            item.blocks.push(AssistantBlock {
                id: "second".into(),
                position: 1,
                content: BlockContent::Reasoning {
                    text: "second summary".into(),
                },
            });
            request.messages = vec![
                Message::Assistant(vec![item]),
                Message::User(vec![UserContent::Text {
                    text: "continue".into(),
                }]),
            ];
            let body = encode(&request).unwrap();
            if matches {
                assert_eq!(body["messages"][0]["content"], json!([native]));
            } else {
                assert!(!body.to_string().contains("visible"));
                assert!(!body.to_string().contains("private"));
            }
        }
        request.messages = vec![Message::Assistant(vec![AssistantItem::reasoning(
            "r",
            0,
            "",
            Some(reasoning_envelope(
                "anthropic",
                &request.model,
                json!({"type":"thinking","thinking":"x"}),
            )),
        )])];
        assert!(encode(&request).is_err());
    }

    #[test]
    fn text_lifecycle_preserves_initial_and_streamed_text_and_usage() {
        use crate::provider::protocol::ResponseAssembler;
        let mut decoder = Decoder::new("claude".into());
        let mut assembler = ResponseAssembler::default();
        for frame in [
            start(),
            block_start(0, json!({"type":"text","text":"hel"})),
            delta(0, json!({"type":"text_delta","text":"lo"})),
            stop(0),
            terminal("end_turn", 7),
            json!({"type":"message_stop"}),
        ] {
            for chunk in decoder.decode(&event(frame)).unwrap() {
                assembler.push(&chunk).unwrap();
            }
        }
        let (items, usage, reason) = assembler.finish().unwrap();
        assert_eq!(items[0].id, "0");
        assert_eq!(
            items[0].blocks[0].content,
            BlockContent::Text {
                text: "hello".into()
            }
        );
        assert_eq!(usage.output_tokens, 7);
        assert_eq!(reason, StopReason::EndTurn);
    }

    #[test]
    fn thinking_and_redacted_blocks_retain_full_native_payload() {
        let mut decoder = started();
        decoder
            .decode(&event(block_start(
                0,
                json!({"type":"thinking","thinking":"initial", "signature":""}),
            )))
            .unwrap();
        let chunks = decoder
            .decode(&event(delta(
                0,
                json!({"type":"thinking_delta","thinking":" summary"}),
            )))
            .unwrap();
        assert_eq!(
            chunks,
            vec![ResponseChunk::BlockDelta {
                item: "0".into(),
                block: "0".into(),
                delta: ContentDelta::Text(" summary".into())
            }]
        );
        decoder
            .decode(&event(delta(
                0,
                json!({"type":"signature_delta","signature":"signed"}),
            )))
            .unwrap();
        let chunks = decoder.decode(&event(stop(0))).unwrap();
        assert_eq!(
            chunks[0],
            ResponseChunk::BlockEnded {
                item: "0".into(),
                block: "0".into(),
                content: BlockContent::Reasoning {
                    text: "initial summary".into()
                }
            }
        );
        let ResponseChunk::ItemEnded {
            replay: Some(replay),
            ..
        } = &chunks[1]
        else {
            panic!("missing item replay")
        };
        assert_eq!(
            replay.payload,
            json!({"type":"thinking","thinking":"initial summary","signature":"signed"})
        );
        let redacted = json!({"type":"redacted_thinking","data":"encrypted"});
        decoder
            .decode(&event(block_start(1, redacted.clone())))
            .unwrap();
        let chunks = decoder.decode(&event(stop(1))).unwrap();
        let ResponseChunk::ItemEnded {
            replay: Some(replay),
            ..
        } = &chunks[1]
        else {
            panic!("missing redacted replay")
        };
        assert_eq!(replay.payload, redacted);
    }

    #[test]
    fn interleaved_tools_are_finalized_in_native_index_order() {
        let mut decoder = started();
        for id in [1, 0] {
            decoder.decode(&event(block_start(id, json!({"type":"tool_use","id":format!("call-{id}"),"name":"inspect","input":{}})))).unwrap();
        }
        let chunks = decoder
            .decode(&event(delta(
                1,
                json!({"type":"input_json_delta","partial_json":"{\"x\":1}"}),
            )))
            .unwrap();
        assert_eq!(
            chunks,
            vec![ResponseChunk::BlockDelta {
                item: "1".into(),
                block: "0".into(),
                delta: ContentDelta::JsonFragment("{\"x\":1}".into())
            }]
        );
        assert!(decoder.decode(&event(stop(1))).unwrap().is_empty());
        let chunks = decoder.decode(&event(stop(0))).unwrap();
        assert_eq!(chunks.len(), 4);
        assert!(matches!(&chunks[0],ResponseChunk::BlockEnded { item, .. } if item == "0"));
        assert!(
            matches!(&chunks[2],ResponseChunk::BlockEnded { item, content:BlockContent::ToolCall(call), .. } if item == "1" && call.arguments == json!({"x":1}))
        );
    }

    #[test]
    fn truncated_or_absent_stream_is_an_error_and_ping_is_a_noop() {
        let mut absent = Decoder::new("model".into());
        assert!(
            absent
                .decode(&event(json!({"type":"ping"})))
                .unwrap()
                .is_empty()
        );
        assert!(absent.finish().is_err());
        assert!(started().finish().is_err());
        let mut open = started();
        open.decode(&event(block_start(
            0,
            json!({"type":"text","text":"partial"}),
        )))
        .unwrap();
        assert!(open.finish().is_err());
        let mut missing_stop = started();
        missing_stop
            .decode(&event(terminal("end_turn", 1)))
            .unwrap();
        assert!(missing_stop.finish().is_err());
        let mut complete = started();
        complete.decode(&event(terminal("end_turn", 1))).unwrap();
        complete
            .decode(&event(json!({"type":"message_stop"})))
            .unwrap();
        assert!(
            complete
                .decode(&event(json!({"type":"message_stop"})))
                .is_err()
        );
    }
}
