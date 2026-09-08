//! Native OpenAI Responses wire codec, shared by HTTP/SSE and Codex WebSocket.
use std::collections::BTreeMap;

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
        "store":false, "include":["reasoning.encrypted_content"]});
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
            "none" | "minimal" | "low" | "medium" | "high" | "xhigh"
        ) {
            return Err(invalid(format!(
                "Unsupported Responses reasoning effort: {effort}"
            )));
        }
        body["reasoning"] = json!({"effort":effort, "summary":"auto"});
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
        Kind::Reasoning => array(item, "summary")?
            .iter()
            .map(|part| {
                if string(part, "type")? != "summary_text" {
                    return Err(protocol("unsupported reasoning summary part"));
                }
                Ok(BlockContent::Reasoning {
                    text: string(part, "text")?.into(),
                })
            })
            .collect(),
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
        let prefix = match self {
            Self::Text => "content",
            Self::Reasoning => "summary",
            Self::Function => "arguments",
        };
        format!("{prefix}_{position}")
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
    kind: Kind,
    ended: Option<Value>,
    parts: BTreeMap<usize, Part>,
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

    pub(crate) fn completed(&self) -> bool {
        self.completed
    }

    pub(crate) fn decode(
        &mut self,
        event: &super::transport::SseEvent,
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
        if let Some(name) = &event.event
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
        if native_id.is_empty() || self.items.values().any(|old| old.native_id == native_id) {
            return Err(protocol("empty or duplicate output item ID"));
        }
        let item_kind = kind(item)?;
        self.items.insert(
            id,
            Item {
                native_id: native_id.clone(),
                kind: item_kind,
                ended: None,
                parts: BTreeMap::new(),
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
        if string(native, "id")? != item.native_id || kind(native)? != item.kind {
            return Err(protocol("final output item identity changed"));
        }
        let parts = final_parts(native)?;
        if let Some(old) = &item.ended {
            if !terminal
                || final_parts(old)? != parts
                || (item.kind == Kind::Reasoning && old != native)
            {
                return Err(protocol("duplicate or conflicting final output item"));
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
    pub(crate) fn feed(&mut self, event: Value) -> Result<Vec<ResponseChunk>, ProviderError> {
        if self.completed {
            return Err(protocol("event after terminal response"));
        }
        let mut chunks = Vec::new();
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
            "response.reasoning_summary_text.delta" => {
                let id = self.active(&event, Kind::Reasoning)?;
                self.delta(
                    id,
                    index(&event, "summary_index")?,
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
            | "response.reasoning_summary_part.added"
            | "response.reasoning_summary_part.done" => {
                let name = string(&event, "type")?;
                let reasoning = name.starts_with("response.reasoning_summary_part.");
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
                let part = event
                    .get("part")
                    .ok_or_else(|| protocol("missing content part"))?;
                let text = match (reasoning, string(part, "type")?) {
                    (true, "summary_text") | (false, "output_text") => string(part, "text")?,
                    (false, "refusal") => string(part, "refusal")?,
                    _ => return Err(protocol("unsupported content part")),
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
                let truncated = string(&event, "type")? == "response.incomplete";
                if string(response, "status")? != if truncated { "incomplete" } else { "completed" }
                {
                    return Err(protocol("terminal response status disagrees with event"));
                }
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
                    && self.items.values().all(|item| item.ended.is_some());
                if !omitted && self.items.keys().any(|id| *id >= output.len()) {
                    return Err(protocol("terminal response omitted a streamed output item"));
                }
                for (id, native) in output.iter().enumerate() {
                    if !self.items.contains_key(&id) {
                        self.start(id, native, &mut chunks)?;
                    }
                    self.end(id, native, &mut chunks, true)?;
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
        req.system = vec![
            SystemSegment {
                text: "first".into(),
                cache: false,
            },
            SystemSegment {
                text: "second".into(),
                cache: false,
            },
        ];
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
        req.max_output_tokens = Some(800);
        req.correlation = Some("session".into());
        let body = encode(&req).unwrap();
        assert_eq!(body["instructions"], "first\n\nsecond");
        assert_eq!(
            body["tools"][0],
            json!({"type":"function", "name":"search", "description":"Search", "parameters":{"type":"object"}, "strict":false})
        );
        assert_eq!(
            body["text"]["format"]["schema"],
            req.response_schema.unwrap().schema
        );
        assert_eq!(body["text"]["format"]["strict"], true);
        assert_eq!(
            body["reasoning"],
            json!({"effort":"high", "summary":"auto"})
        );
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(body["prompt_cache_key"], "session");
        assert_eq!(body["max_output_tokens"], 800);
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn all_user_variants_and_tool_result_images_are_preserved() {
        let mut req = request();
        req.messages = vec![
            Message::User(vec![
                UserContent::Text {
                    text: "text".into(),
                },
                UserContent::Runtime {
                    text: "runtime".into(),
                },
                UserContent::ParentInput {
                    text: "parent".into(),
                },
                UserContent::Compaction {
                    text: "compact".into(),
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
        let parts = body["input"][0]["content"].as_array().unwrap();
        for (part, text) in parts.iter().zip(["text", "runtime", "parent", "compact"]) {
            assert_eq!(part, &json!({"type":"input_text", "text":text}));
        }
        assert_eq!(parts[4]["image_url"], "data:image/png;base64,YQ==");
        let output = &body["input"][1];
        assert_eq!(output["type"], "function_call_output");
        assert_eq!(output["call_id"], "call_1");
        let result: Value = serde_json::from_str(output["output"].as_str().unwrap()).unwrap();
        assert_eq!(result, json!({"result":{"answer":42}, "is_error":true}));
        assert!(
            body["input"][2]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("call_1")
        );
        assert_eq!(
            body["input"][2]["content"][1]["image_url"],
            parts[4]["image_url"]
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
    fn invalid_settings_and_missing_images_fail_instead_of_disappearing() {
        let mut req = request();
        req.system.push(SystemSegment {
            text: "cache".into(),
            cache: true,
        });
        assert_eq!(encode(&req).unwrap()["instructions"], "cache");
        req.system.clear();
        req.reasoning = Some("unlimited".into());
        assert!(encode(&req).is_err());
        req.reasoning = None;
        req.max_output_tokens = Some(0);
        assert!(encode(&req).is_err());
        req.max_output_tokens = None;
        let mut missing = image();
        missing.data_base64 = None;
        req.messages = vec![Message::User(vec![UserContent::Image { image: missing }])];
        assert!(encode(&req).is_err());
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
    fn opaque_only_reasoning_has_no_synthetic_display_block_and_replays_once() {
        let native = json!({"type":"reasoning", "id":"hidden", "summary":[], "encrypted_content":"ciphertext"});
        let items = assemble(vec![native.clone()]);
        assert!(items[0].blocks.is_empty());
        assert_eq!(items[0].replay.as_ref().unwrap().payload, native);
        let mut req = request();
        req.messages = vec![Message::Assistant(items)];
        assert_eq!(encode(&req).unwrap()["input"], json!([native]));
    }

    #[test]
    fn terminal_only_response_starts_and_ends_every_item_in_order() {
        let items = assemble(vec![
            reasoning_item(),
            text_item("msg_1", "answer"),
            call_item(),
        ]);
        assert_eq!(items.len(), 3);
        assert_eq!(
            items.iter().map(|item| item.position).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert_eq!(items[0].blocks.len(), 2);
        assert!(items[2].tool_call_ref().is_some());
    }

    #[test]
    fn observed_three_reasoning_items_have_three_three_two_independent_summary_blocks() {
        let mut decoder = Decoder::codex("gpt-5".into());
        let mut assembler = crate::provider::protocol::ResponseAssembler::default();
        let mut completed_items = Vec::new();
        for (item_index, count) in [3, 3, 2].into_iter().enumerate() {
            let id = format!("rs_{item_index}");
            let summaries = (0..count)
                .map(|i| json!({"type":"summary_text", "text":format!("summary {i}")}))
                .collect::<Vec<_>>();
            let native = json!({"type":"reasoning", "id":id, "summary":summaries, "encrypted_content":format!("cipher_{item_index}")});
            for chunk in decoder
                .feed(added(
                    item_index,
                    json!({"type":"reasoning", "id":id, "summary":[]}),
                ))
                .unwrap()
            {
                assembler.push(&chunk).unwrap();
            }
            for position in 0..count {
                let text = format!("summary {position}");
                let delta = json!({"type":"response.reasoning_summary_text.delta", "output_index":item_index, "item_id":id, "summary_index":position,"delta":text});
                for chunk in decoder.feed(delta).unwrap() {
                    assembler.push(&chunk).unwrap();
                }
                let end = json!({"type":"response.reasoning_summary_text.done", "output_index":item_index, "item_id":id, "summary_index":position,"text":text});
                let chunks = decoder.feed(end.clone()).unwrap();
                assert_eq!(chunks.len(), 1);
                assert!(matches!(&chunks[0], ResponseChunk::BlockEnded { .. }));
                for chunk in chunks {
                    assembler.push(&chunk).unwrap();
                }
                assert!(decoder.feed(end).unwrap().is_empty());
                assert!(decoder.feed(json!({"type":"response.reasoning_summary_part.done", "output_index":item_index, "item_id":id, "summary_index":position,"part":{"type":"summary_text", "text":text}})).unwrap().is_empty());
            }
            assert!(
                assembler
                    .snapshot()
                    .items
                    .iter()
                    .all(|item| item.replay.is_none())
            );
            completed_items.push(native);
        }
        // Item ends can arrive consecutively, long after all summaries closed.
        for (index, native) in completed_items.into_iter().enumerate() {
            let chunks = decoder.feed(done(index, native)).unwrap();
            assert_eq!(chunks.len(), 1);
            assert!(matches!(
                &chunks[0],
                ResponseChunk::ItemEnded {
                    replay: Some(_),
                    ..
                }
            ));
            assembler.push(&chunks[0]).unwrap();
        }
        for chunk in decoder.feed(completed(vec![])).unwrap() {
            assembler.push(&chunk).unwrap();
        }
        let (items, _, _) = assembler.finish().unwrap();
        assert_eq!(
            items
                .iter()
                .map(|item| item.blocks.len())
                .collect::<Vec<_>>(),
            vec![3, 3, 2]
        );
        let mut req = request();
        req.messages = vec![Message::Assistant(items)];
        assert_eq!(encode(&req).unwrap()["input"].as_array().unwrap().len(), 3);
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
    fn conflicting_duplicate_part_done_and_final_arguments_fail() {
        let mut decoder = Decoder::new("gpt-5".into());
        decoder.feed(added(0, text_item("msg", ""))).unwrap();
        decoder.feed(json!({"type":"response.output_text.done","output_index":0,"item_id":"msg","content_index":0,"text":"a"})).unwrap();
        assert!(decoder.feed(json!({"type":"response.content_part.done","output_index":0,"item_id":"msg","content_index":0,"part":{"type":"output_text","text":"b"}})).is_err());
        assert!(decoder.feed(done(0, text_item("msg", "b"))).is_err());
        let mut decoder = Decoder::new("gpt-5".into());
        decoder.feed(added(0, call_item())).unwrap();
        decoder.feed(json!({"type":"response.function_call_arguments.delta","output_index":0,"item_id":"fc_1","delta":"{\"wrong\":"})).unwrap();
        assert!(decoder.feed(done(0, call_item())).is_err());
    }

    #[test]
    fn refusal_is_visible_and_incomplete_is_explicit() {
        let item = json!({"type":"message", "id":"msg_1", "role":"assistant", "content":[{"type":"refusal", "refusal":"No."}]});
        for (reason, expected) in [
            ("max_output_tokens", StopReason::MaxTokens),
            ("content_filter", StopReason::ContentFilter),
        ] {
            let chunks = Decoder::new("gpt-5".into()).feed(json!({"type":"response.incomplete", "response":{"status":"incomplete", "output":[item], "incomplete_details":{"reason":reason}}})).unwrap();
            assert!(chunks.iter().any(|chunk| matches!(chunk, ResponseChunk::BlockEnded{content:BlockContent::Text{text},..} if text=="No.")));
            assert_eq!(
                chunks.last(),
                Some(&ResponseChunk::ResponseEnded {
                    stop_reason: expected
                })
            );
        }
    }

    #[test]
    fn final_only_arguments_duplicate_done_and_call_ids_are_validated() {
        let mut decoder = Decoder::new("gpt-5".into());
        decoder.feed(added(0, call_item())).unwrap();
        let done_arguments = json!({"type":"response.function_call_arguments.done", "output_index":0, "item_id":"fc_1", "arguments":"{\"query\":\"rust\"}"});
        let chunks = decoder.feed(done_arguments.clone()).unwrap();
        assert!(matches!(
            chunks.as_slice(),
            [
                ResponseChunk::BlockStarted { .. },
                ResponseChunk::BlockEnded { .. }
            ]
        ));
        assert!(decoder.feed(done_arguments.clone()).unwrap().is_empty());
        let mut conflict = done_arguments;
        conflict["arguments"] = json!("{\"query\":\"wrong\"}");
        assert!(decoder.feed(conflict).is_err());
        decoder.feed(done(0, call_item())).unwrap();
        let mut duplicate = call_item();
        duplicate["id"] = json!("fc_2");
        decoder.feed(added(1, duplicate.clone())).unwrap();
        assert!(decoder.feed(done(1, duplicate)).is_err());
    }

    #[test]
    fn initial_part_payload_streams_once_and_disagrees_with_final_is_rejected() {
        let mut decoder = Decoder::new("gpt-5".into());
        decoder.feed(added(0, text_item("msg", ""))).unwrap();
        let added = json!({"type":"response.content_part.added","output_index":0,"item_id":"msg","content_index":0,"part":{"type":"output_text","text":"hello"}});
        let chunks = decoder.feed(added.clone()).unwrap();
        assert!(
            matches!(chunks.as_slice(), [ResponseChunk::BlockStarted{..}, ResponseChunk::BlockDelta{delta:ContentDelta::Text(text),..}] if text == "hello")
        );
        assert!(decoder.feed(added).is_err());
        assert!(decoder.feed(json!({"type":"response.output_text.done","output_index":0,"item_id":"msg","content_index":0,"text":"different"})).is_err());
        decoder.feed(done(0, text_item("msg", "hello"))).unwrap();
    }

    #[test]
    fn malformed_unknown_and_out_of_lifecycle_events_fail() {
        for event in [
            json!({}),
            json!({"type":"future.event"}),
            json!({"type":"response.output_item.added", "output_index":0, "item":{"type":"web_search_call","id":"ws"}}),
            json!({"type":"response.output_text.delta", "output_index":0,"item_id":"msg", "content_index":0,"delta":"orphan"}),
            json!({"type":"response.completed", "response":{"status":"completed"}}),
        ] {
            assert_eq!(
                Decoder::new("gpt-5".into()).feed(event).unwrap_err().kind,
                ProviderErrorKind::Protocol
            );
        }
        let mut decoder = Decoder::new("gpt-5".into());
        decoder.feed(added(0, text_item("msg", ""))).unwrap();
        assert!(decoder.feed(added(0, text_item("msg", ""))).is_err());
        assert!(decoder.feed(json!({"type":"response.reasoning_summary_text.delta", "output_index":0,"item_id":"msg", "summary_index":0,"delta":"wrong kind"})).is_err());
        assert!(decoder.feed(json!({"type":"response.output_text.delta", "output_index":0,"item_id":"other", "content_index":0,"delta":"wrong id"})).is_err());
        decoder.feed(done(0, text_item("msg", "final"))).unwrap();
        assert!(decoder.feed(done(0, text_item("msg", "final"))).is_err());
        assert!(
            decoder
                .feed(completed(vec![text_item("msg", "conflict")]))
                .is_err()
        );
        assert!(decoder.feed(completed(vec![])).is_err());
        assert!(decoder.finish().is_err());
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

    #[test]
    fn native_lifecycle_metadata_and_empty_encrypted_reasoning_are_supported() {
        let mut decoder = Decoder::new("gpt-5".into());
        for status in ["created", "in_progress", "queued"] {
            assert!(
                decoder
                    .feed(json!({"type":format!("response.{status}"), "response":{"id":"resp_1"}}))
                    .unwrap()
                    .is_empty()
            );
        }
        let native = json!({"type":"reasoning", "id":"rs_1", "summary":[{"type":"summary_text","text":""}], "encrypted_content":"ciphertext"});
        decoder.feed(added(0, native.clone())).unwrap();
        for phase in ["added", "done"] {
            decoder.feed(json!({"type":format!("response.reasoning_summary_part.{phase}"), "output_index":0,
                "item_id":"rs_1", "summary_index":0,"part":{"type":"summary_text", "text":""}})).unwrap();
        }
        decoder
            .feed(
                json!({"type":"response.reasoning_summary_text.done", "output_index":0,
            "item_id":"rs_1", "summary_index":0, "text":""}),
            )
            .unwrap();
        let chunks = decoder.feed(completed(vec![native.clone()])).unwrap();
        assert_eq!(
            chunks[0],
            ResponseChunk::ItemEnded {
                id: "rs_1".into(),
                replay: Some(reasoning_envelope("responses", "gpt-5", native)),
            }
        );
    }

    #[test]
    fn all_upstream_error_content_is_redacted() {
        for event in [
            json!({"type":"error", "code":"sensitive-code", "message":"secret-credential"}),
            json!({"type":"response.failed", "response":{"error":{"code":"server_error", "message":"secret-credential"}}}),
            json!({"type":"secret-credential"}),
            added(0, json!({"id":"x", "type":"secret-credential"})),
        ] {
            let error = Decoder::new("gpt-5".into()).feed(event).unwrap_err();
            assert!(!error.message.contains("secret-credential"));
            assert!(!error.message.contains("sensitive-code"));
        }
    }

    #[test]
    fn malformed_usage_is_rejected() {
        for usage in [
            json!({}),
            json!({"input_tokens":-1,"output_tokens":1}),
            json!({"input_tokens":2,"output_tokens":1,"input_tokens_details":{"cached_tokens":3}}),
        ] {
            let mut event = completed(vec![]);
            event["response"]["usage"] = usage;
            assert!(Decoder::new("gpt-5".into()).feed(event).is_err());
        }
    }
}
