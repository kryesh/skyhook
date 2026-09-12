//! Anthropic Messages streaming lifecycle and response decoding.
use std::collections::BTreeMap;

use serde_json::Value;

use crate::provider::{
    ProviderError, ProviderErrorKind,
    protocol::{
        BlockContent, BlockKind, ContentDelta, ItemKind, ReplayEnvelope, ResponseChunk, StopReason,
        Usage,
    },
};

use super::{common::reasoning_envelope, transport::SseEvent};

mod encoder;
mod native;
pub(crate) use encoder::encode;
use native::*;

struct Block {
    native: Value,
    partial_json: String,
    has_json_delta: bool,
    ended: bool,
    completed: Option<(BlockContent, Option<ReplayEnvelope>)>,
    tool_error: Option<ProviderError>,
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
                retry_after: None,
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
                        tool_error: None,
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
                        // The terminal reason arrives after content_block_stop. An
                        // incomplete input is truncation, not a protocol failure,
                        // when that reason is max_tokens or refusal.
                        match tool_content(block) {
                            Ok(content) => content,
                            Err(error) => {
                                block.ended = true;
                                block.tool_error = Some(error);
                                return self.emit_completed_blocks(false);
                            }
                        }
                    }
                    _ => return Err(protocol("unsupported completed content block")),
                };
                block.ended = true;
                block.completed = Some((completed, replay));
                self.emit_completed_blocks(false)
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
                let discard_tools = matches!(
                    stop_reason,
                    StopReason::MaxTokens | StopReason::ContentFilter
                );
                let mut chunks = Vec::new();
                if discard_tools {
                    // Even syntactically complete inputs are unsafe on abnormal
                    // stops. Retract tools already emitted, as well as pending
                    // ones, before the response can be promoted for execution.
                    for (id, block) in &self.blocks {
                        if block.native["type"] == "tool_use" {
                            chunks.push(ResponseChunk::ItemDiscarded { id: id.to_string() });
                        }
                    }
                } else {
                    for block in self.blocks.values_mut() {
                        if let Some(error) = block.tool_error.take() {
                            return Err(error);
                        }
                    }
                }
                chunks.extend(self.emit_completed_blocks(discard_tools)?);
                self.stop_reason = Some(stop_reason);
                self.message_delta = true;
                chunks.push(ResponseChunk::UsageUpdated { usage });
                Ok(chunks)
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

    fn emit_completed_blocks(
        &mut self,
        discard_tools: bool,
    ) -> Result<Vec<ResponseChunk>, ProviderError> {
        let mut chunks = Vec::new();
        while let Some(block) = self
            .blocks
            .get_mut(&self.next_end)
            .filter(|block| block.ended)
        {
            if discard_tools && block.native["type"] == "tool_use" {
                block.completed = None;
                block.tool_error = None;
            } else {
                if block.tool_error.is_some() {
                    // Keep index ordering while awaiting the terminal reason.
                    break;
                }
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
            }
            self.next_end = self
                .next_end
                .checked_add(1)
                .ok_or_else(|| protocol("block index overflow"))?;
        }
        Ok(chunks)
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
        if self.blocks.keys().copied().ne(0..self.blocks.len()) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::protocol::{Message, ModelRequest, ToolResult};
    use serde_json::json;
    fn request() -> ModelRequest {
        crate::provider::backends::common::tests::request("claude-test")
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

    #[tokio::test]
    async fn signed_and_redacted_reasoning_tool_roundtrip_survives_save_resume() {
        use crate::provider::backends::common::{
            bind_reasoning_scope, filter_reasoning_scope, reasoning_scope, tests::resume_request,
        };
        let mut request = request();
        let scope = reasoning_scope("anthropic", "https://api.example/v1/messages");
        let native = json!({"type":"thinking", "thinking":"original private text", "signature":"sig+/=",
            "future_field":{"opaque":"preserve"}});
        let redacted =
            json!({"type":"redacted_thinking", "data":"encrypted+/=", "future_field":42});
        let tool =
            json!({"type":"tool_use", "id":"call_1", "name":"inspect", "input":{"path":"test"}});
        let mut decoder = Decoder::new(request.model.clone());
        let mut assembler = crate::provider::protocol::ResponseAssembler::default();
        let mut streamed_thinking = String::new();
        for frame in [
            start(),
            block_start(
                0,
                json!({"type":"thinking", "thinking":"original ", "signature":"",
                "future_field":{"opaque":"preserve"}}),
            ),
            delta(
                0,
                json!({"type":"thinking_delta", "thinking":"private text"}),
            ),
            delta(0, json!({"type":"signature_delta", "signature":"sig+"})),
            delta(0, json!({"type":"signature_delta", "signature":"/="})),
            stop(0),
            block_start(1, redacted.clone()),
            stop(1),
            block_start(2, tool.clone()),
            stop(2),
            terminal("tool_use", 12),
            json!({"type":"message_stop"}),
        ] {
            for mut chunk in decoder.decode(&event(frame)).unwrap() {
                if let ResponseChunk::BlockDelta {
                    item,
                    delta: ContentDelta::Text(text),
                    ..
                } = &chunk
                    && item == "0"
                {
                    streamed_thinking.push_str(text);
                }
                bind_reasoning_scope(&mut chunk, &scope);
                assembler.push(&chunk).unwrap();
            }
        }
        let (mut items, _, reason) = assembler.finish().unwrap();
        assert_eq!(reason, StopReason::ToolUse);
        assert_eq!(streamed_thinking, "original private text");
        assert_eq!(
            items[0].blocks[0].content,
            BlockContent::Reasoning {
                text: streamed_thinking,
            }
        );
        assert_eq!(items[0].replay.as_ref().unwrap().payload, native);
        assert_eq!(items[1].replay.as_ref().unwrap().payload, redacted);
        // UI summaries are not authoritative signed thinking and must never be
        // used to reconstruct the native payload, even after persistence.
        items[0].blocks[0].content = BlockContent::Reasoning {
            text: "display summary only".into(),
        };
        request.messages.push(Message::Assistant(items));
        request.messages.push(Message::Tool(vec![ToolResult {
            call_id: "call_1".into(),
            name: "inspect".into(),
            result: json!({"ok":true}),
            images: vec![],
            is_error: false,
        }]));
        let original = resume_request(&request).await;
        let mut matching = original.clone();
        filter_reasoning_scope(&mut matching, &scope);
        let body = encode(&matching).unwrap();
        assert_eq!(
            body["messages"][1]["content"],
            json!([native, redacted, tool])
        );
        assert_eq!(body["messages"][2]["content"][0]["tool_use_id"], "call_1");
        assert!(body.get("thinking").is_none()); // replay does not require enabling new thinking
        for foreign_scope in [
            reasoning_scope("other-provider", "https://api.example/v1/messages"),
            reasoning_scope("anthropic", "https://other.example/v1/messages"),
        ] {
            let mut foreign = original.clone();
            filter_reasoning_scope(&mut foreign, &foreign_scope);
            let Message::Assistant(items) = &foreign.messages[1] else {
                unreachable!()
            };
            assert_eq!(items.len(), 3);
            assert_eq!(
                items[0].blocks[0].content,
                BlockContent::Reasoning {
                    text: "display summary only".into()
                }
            );
            assert!(items[0].replay.is_none());
            assert_eq!(
                encode(&foreign).unwrap()["messages"][1]["content"],
                json!([tool])
            );
        }
        let mut foreign = original.clone();
        foreign.model = "different-model".into();
        assert_eq!(
            encode(&foreign).unwrap()["messages"][1]["content"],
            json!([tool])
        );
        assert_eq!(
            encode(&original).unwrap()["messages"][1]["content"][0],
            native
        );
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
    fn abnormal_tool_stops_preserve_completed_reasoning_and_final_usage() {
        use crate::provider::protocol::ResponseAssembler;

        for (reason, expected) in [
            ("max_tokens", StopReason::MaxTokens),
            ("model_context_window_exceeded", StopReason::MaxTokens),
            ("refusal", StopReason::ContentFilter),
        ] {
            // A valid object is also provisional until the terminal reason.
            for input in [r#"{"path":"part"#, r#"{"path":"complete"}"#, "[]"] {
                let mut decoder = Decoder::new("claude-test".into());
                let mut assembler = ResponseAssembler::default();
                let signed = json!({"type":"thinking", "thinking":"private text",
                    "signature":"signed+/=", "future_field":{"opaque":true}});
                let redacted = json!({"type":"redacted_thinking", "data":"encrypted+/=",
                    "future_field":42});
                let mut chunks = Vec::new();
                for frame in [
                    start(),
                    block_start(0, signed.clone()),
                    stop(0),
                    block_start(1, redacted.clone()),
                    stop(1),
                    block_start(
                        2,
                        json!({"type":"tool_use", "id":"call_1",
                        "name":"inspect", "input":{}}),
                    ),
                    delta(2, json!({"type":"input_json_delta", "partial_json":input})),
                    stop(2),
                    terminal(reason, 19),
                    json!({"type":"message_stop"}),
                ] {
                    for chunk in decoder.decode(&event(frame)).unwrap() {
                        assembler.push(&chunk).unwrap();
                        chunks.push(chunk);
                    }
                }
                decoder.finish().unwrap();
                assert!(chunks.contains(&ResponseChunk::ItemDiscarded { id: "2".into() }));
                let (items, usage, stop_reason) = assembler.finish().unwrap();
                assert_eq!(stop_reason, expected);
                assert_eq!(
                    usage,
                    Usage {
                        input_tokens: 24,
                        cached_input_tokens: 17,
                        output_tokens: 19,
                    }
                );
                assert_eq!(items.len(), 2);
                assert!(items.iter().all(|item| item.kind == ItemKind::Reasoning));
                assert_eq!(items[0].replay.as_ref().unwrap().payload, signed);
                assert_eq!(items[1].replay.as_ref().unwrap().payload, redacted);
            }
        }
    }

    #[test]
    fn truncated_tool_does_not_block_later_completed_reasoning() {
        use crate::provider::protocol::ResponseAssembler;

        let mut decoder = Decoder::new("claude-test".into());
        let mut assembler = ResponseAssembler::default();
        let signed = json!({"type":"thinking", "thinking":"private", "signature":"signed"});
        for frame in [
            start(),
            block_start(
                0,
                json!({"type":"tool_use", "id":"call_1",
                "name":"inspect", "input":{}}),
            ),
            delta(0, json!({"type":"input_json_delta", "partial_json":"{"})),
            stop(0),
            block_start(1, signed.clone()),
            stop(1),
            terminal("max_tokens", 19),
            json!({"type":"message_stop"}),
        ] {
            for chunk in decoder.decode(&event(frame)).unwrap() {
                assembler.push(&chunk).unwrap();
            }
        }
        let (items, usage, reason) = assembler.finish().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].replay.as_ref().unwrap().payload, signed);
        assert_eq!(usage.output_tokens, 19);
        assert_eq!(reason, StopReason::MaxTokens);
    }

    #[test]
    fn malformed_tool_input_still_errors_on_normal_terminal_reason() {
        for reason in ["tool_use", "end_turn", "stop_sequence", "pause_turn"] {
            for input in ["{", "[]", ""] {
                let mut decoder = started();
                decoder
                    .decode(&event(block_start(
                        0,
                        json!({"type":"tool_use",
                    "id":"call_1", "name":"inspect", "input":{}}),
                    )))
                    .unwrap();
                decoder
                    .decode(&event(delta(
                        0,
                        json!({"type":"input_json_delta",
                    "partial_json":input}),
                    )))
                    .unwrap();
                assert!(decoder.decode(&event(stop(0))).unwrap().is_empty());
                assert!(decoder.decode(&event(terminal(reason, 19))).is_err());
                assert!(decoder.finish().is_err());
            }
        }
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
