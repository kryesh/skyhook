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

// Native kind is decoded once at block admission. The opaque JSON remains
// attached to its kind so signed reasoning/vendor fields are never regenerated.
enum StreamingBlock {
    Text(Value),
    Thinking(Value),
    RedactedThinking(Value),
    Tool {
        native: Value,
        partial_json: Option<String>,
    },
}

enum CompletedBlock {
    Text(String),
    Reasoning {
        text: String,
        replay: ReplayEnvelope,
    },
    Tool(crate::provider::protocol::ToolCall),
}

enum Block {
    Streaming(StreamingBlock),
    Completed(CompletedBlock),
    // Incomplete tool input awaits the terminal stop reason: abnormal stops
    // discard it, ordinary stops surface the original error.
    PendingToolError(ProviderError),
    EmittedTool,
    EmittedOther,
}

/// Signed thinking is bound to the exact conversation before it, so compaction drops it.
fn bound_envelope(model: &str, native: &Value) -> ReplayEnvelope {
    ReplayEnvelope {
        conversation_bound: true,
        ..reasoning_envelope("anthropic", model, native.clone())
    }
}

fn block_delta(item: usize, delta: ContentDelta) -> ResponseChunk {
    ResponseChunk::BlockDelta {
        item: item.to_string(),
        block: "0".into(),
        delta,
    }
}

impl Block {
    fn is_tool(&self) -> bool {
        matches!(
            self,
            Self::Streaming(StreamingBlock::Tool { .. })
                | Self::Completed(CompletedBlock::Tool(_))
                | Self::PendingToolError(_)
                | Self::EmittedTool
        )
    }
}

impl StreamingBlock {
    fn start(native: Value, id: usize) -> Result<(Self, Vec<ResponseChunk>), ProviderError> {
        let (block, kind, block_kind, text) = match string(&native, "type")? {
            "text" => {
                reject_citations(&native)?;
                let text = string(&native, "text")?.to_owned();
                (Self::Text(native), ItemKind::Text, BlockKind::Text, text)
            }
            "thinking" => {
                let text = string(&native, "thinking")?.to_owned();
                string(&native, "signature")?;
                (
                    Self::Thinking(native),
                    ItemKind::Reasoning,
                    BlockKind::Reasoning,
                    text,
                )
            }
            "redacted_thinking" => {
                if string(&native, "data")?.is_empty() {
                    return Err(protocol("redacted thinking block has empty data"));
                }
                (
                    Self::RedactedThinking(native),
                    ItemKind::Reasoning,
                    BlockKind::Reasoning,
                    String::new(),
                )
            }
            "tool_use" => {
                if string(&native, "id")?.is_empty()
                    || string(&native, "name")?.is_empty()
                    || !native.get("input").is_some_and(Value::is_object)
                {
                    return Err(protocol(
                        "tool_use requires nonempty id/name and object input",
                    ));
                }
                (
                    Self::Tool {
                        native,
                        partial_json: None,
                    },
                    ItemKind::ToolCall,
                    BlockKind::ToolCallArguments,
                    String::new(),
                )
            }
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
        if !text.is_empty() {
            chunks.push(block_delta(id, ContentDelta::Text(text)));
        }
        Ok((block, chunks))
    }

    fn complete(&self, model: &str) -> Result<Block, ProviderError> {
        let content = match self {
            Self::Text(native) => CompletedBlock::Text(string(native, "text")?.into()),
            Self::Thinking(native) => {
                if string(native, "signature")?.is_empty() {
                    return Err(protocol("thinking block has an empty signature"));
                }
                CompletedBlock::Reasoning {
                    text: string(native, "thinking")?.into(),
                    replay: bound_envelope(model, native),
                }
            }
            Self::RedactedThinking(native) => CompletedBlock::Reasoning {
                // Preserve optional display text from the original raw block.
                text: native
                    .get("thinking")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .into(),
                replay: bound_envelope(model, native),
            },
            Self::Tool {
                native,
                partial_json,
            } => match tool_content(native, partial_json.as_deref()) {
                Ok(call) => CompletedBlock::Tool(call),
                Err(error) => return Ok(Block::PendingToolError(error)),
            },
        };
        Ok(Block::Completed(content))
    }
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
                let (block, chunks) = StreamingBlock::start(native, id)?;
                self.blocks.insert(id, Block::Streaming(block));
                Ok(chunks)
            }
            "content_block_delta" => {
                self.require_content_phase()?;
                let id = index(&value)?;
                let block = self
                    .blocks
                    .get_mut(&id)
                    .and_then(|block| match block {
                        Block::Streaming(block) => Some(block),
                        _ => None,
                    })
                    .ok_or_else(|| protocol(format!("delta for unopened or ended block {id}")))?;
                let delta = value
                    .get("delta")
                    .ok_or_else(|| protocol("missing content delta"))?;
                let kind = string(delta, "type")?;
                match (block, kind) {
                    (StreamingBlock::Text(native), "text_delta")
                    | (StreamingBlock::Thinking(native), "thinking_delta") => {
                        let field = kind.trim_end_matches("_delta");
                        let text = string(delta, field)?;
                        append(native, field, text)?;
                        Ok(vec![block_delta(id, ContentDelta::Text(text.into()))])
                    }
                    (StreamingBlock::Thinking(native), "signature_delta") => {
                        append(native, "signature", string(delta, "signature")?)?;
                        Ok(Vec::new())
                    }
                    (
                        StreamingBlock::Tool {
                            native,
                            partial_json,
                        },
                        "input_json_delta",
                    ) => {
                        if native["input"]
                            .as_object()
                            .is_some_and(|input| !input.is_empty())
                        {
                            return Err(protocol(
                                "tool_use has both initial input and streamed input",
                            ));
                        }
                        let fragment = string(delta, "partial_json")?;
                        partial_json
                            .get_or_insert_with(String::new)
                            .push_str(fragment);
                        Ok(vec![block_delta(
                            id,
                            ContentDelta::JsonFragment(fragment.into()),
                        )])
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
                    .ok_or_else(|| protocol(format!("stop for unopened or ended block {id}")))?;
                let Block::Streaming(streaming) = block else {
                    return Err(protocol(format!("stop for unopened or ended block {id}")));
                };
                *block = streaming.complete(&self.model)?;
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
                        if block.is_tool() {
                            chunks.push(ResponseChunk::ItemDiscarded { id: id.to_string() });
                        }
                    }
                } else {
                    for block in self.blocks.values() {
                        if let Block::PendingToolError(error) = block {
                            return Err(error.clone());
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
            .filter(|block| !matches!(block, Block::Streaming(_)))
        {
            if discard_tools && block.is_tool() {
                *block = Block::EmittedTool;
            } else {
                if matches!(block, Block::PendingToolError(_)) {
                    // Keep index ordering while awaiting the terminal reason.
                    break;
                }
                let completed = std::mem::replace(block, Block::EmittedOther);
                let (content, replay) = match completed {
                    Block::Completed(CompletedBlock::Text(text)) => {
                        (BlockContent::Text { text }, None)
                    }
                    Block::Completed(CompletedBlock::Reasoning { text, replay }) => {
                        (BlockContent::Reasoning { text }, Some(replay))
                    }
                    Block::Completed(CompletedBlock::Tool(call)) => {
                        *block = Block::EmittedTool;
                        (BlockContent::ToolCall(call), None)
                    }
                    _ => return Err(protocol("block was already emitted")),
                };
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
        if self
            .blocks
            .values()
            .any(|block| matches!(block, Block::Streaming(_)))
        {
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
    use crate::provider::protocol::{Message, ModelRequest, ResponseAssembler, ToolResult};
    use serde_json::json;

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

    fn usage(output_tokens: u64) -> Usage {
        Usage {
            input_tokens: 24,
            cached_input_tokens: 17,
            output_tokens,
        }
    }

    fn started() -> Decoder {
        let mut decoder = Decoder::new("claude-test".into());
        let usage = usage(1);
        let chunks = decoder.decode(&event(start())).unwrap();
        assert_eq!(chunks, [ResponseChunk::UsageUpdated { usage }]);
        decoder
    }

    fn block_start(index: usize, block: Value) -> Value {
        json!({"type":"content_block_start", "index":index, "content_block":block})
    }

    fn delta(index: usize, delta: Value) -> Value {
        json!({"type":"content_block_delta", "index":index, "delta":delta})
    }

    fn json_delta(index: usize, partial: &str) -> Value {
        delta(
            index,
            json!({"type":"input_json_delta", "partial_json":partial}),
        )
    }

    fn stop(index: usize) -> Value {
        json!({"type":"content_block_stop", "index":index})
    }

    fn terminal(reason: &str, output_tokens: u64) -> Value {
        json!({"type":"message_delta", "delta":{"stop_reason":reason,"stop_sequence":null},
            "usage":{"output_tokens":output_tokens}})
    }

    fn message_stop() -> Value {
        json!({"type":"message_stop"})
    }

    fn tool_use(id: &str) -> Value {
        json!({"type":"tool_use", "id":id, "name":"inspect", "input":{}})
    }

    fn decode(decoder: &mut Decoder, frames: Vec<Value>) -> Vec<ResponseChunk> {
        let mut chunks = Vec::new();
        for frame in frames {
            chunks.extend(decoder.decode(&event(frame)).unwrap());
        }
        chunks
    }

    fn assembled(chunks: &[ResponseChunk]) -> ResponseAssembler {
        let mut assembler = ResponseAssembler::default();
        for chunk in chunks {
            assembler.push(chunk).unwrap();
        }
        assembler
    }

    #[test]
    fn kind_specific_deltas_are_rejected_and_poison_the_decoder() {
        for (native, wrong_delta) in [
            (
                json!({"type":"text", "text":"initial"}),
                json!({"type":"input_json_delta", "partial_json":"{}"}),
            ),
            (
                json!({"type":"thinking", "thinking":"", "signature":""}),
                json!({"type":"text_delta", "text":"wrong"}),
            ),
            (
                json!({"type":"redacted_thinking", "data":"opaque"}),
                json!({"type":"signature_delta", "signature":"wrong"}),
            ),
            (
                tool_use("call"),
                json!({"type":"thinking_delta", "thinking":"wrong"}),
            ),
        ] {
            let mut decoder = started();
            decoder.decode(&event(block_start(0, native))).unwrap();
            let error = decoder.decode(&event(delta(0, wrong_delta))).unwrap_err();
            assert_eq!(error.kind, ProviderErrorKind::Protocol);
            let poisoned = decoder.decode(&event(stop(0))).unwrap_err();
            assert!(poisoned.message.contains("already failed"));
        }
    }

    #[test]
    fn signed_completion_keeps_opaque_native_fields_and_requires_final_signature() {
        let native = json!({"type":"thinking", "thinking":"reason", "signature":"signed", "x-vendor":{"nested":[1,null,"opaque"]}});
        let (streaming, _) = StreamingBlock::start(native.clone(), 0).unwrap();
        let Block::Completed(CompletedBlock::Reasoning { text, replay }) =
            streaming.complete("vendor-model").unwrap()
        else {
            panic!("reasoning completion required");
        };
        assert_eq!((text.as_str(), &replay.payload), ("reason", &native));
        let unsigned = json!({"type":"thinking", "thinking":"reason", "signature":""});
        let (streaming, _) = StreamingBlock::start(unsigned, 0).unwrap();
        assert!(streaming.complete("vendor-model").is_err());
    }

    #[test]
    fn text_lifecycle_preserves_initial_and_streamed_text_and_usage() {
        let chunks = decode(
            &mut Decoder::new("claude".into()),
            vec![
                start(),
                block_start(0, json!({"type":"text","text":"hel"})),
                delta(0, json!({"type":"text_delta","text":"lo"})),
                stop(0),
                terminal("end_turn", 7),
                message_stop(),
            ],
        );
        let (items, usage, reason) = assembled(&chunks).finish().unwrap();
        assert_eq!(items[0].id, "0");
        assert_eq!(items[0].blocks[0].content.text_content(), Some("hello"));
        assert_eq!((usage.output_tokens, reason), (7, StopReason::EndTurn));
    }

    #[tokio::test]
    async fn signed_and_redacted_reasoning_tool_roundtrip_survives_save_resume() {
        use crate::provider::backends::common::{
            bind_reasoning_scope, filter_reasoning_scope, reasoning_scope, tests::resume_request,
        };
        let mut request = crate::provider::backends::common::tests::request("claude-test");
        let scope = reasoning_scope("anthropic", "https://api.example/v1/messages");
        let native = json!({"type":"thinking", "thinking":"original private text", "signature":"sig+/=",
            "future_field":{"opaque":"preserve"}});
        let redacted =
            json!({"type":"redacted_thinking", "data":"encrypted+/=", "future_field":42});
        let tool =
            json!({"type":"tool_use", "id":"call_1", "name":"inspect", "input":{"path":"test"}});
        let mut chunks = decode(
            &mut Decoder::new(request.model.clone()),
            vec![
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
                message_stop(),
            ],
        );
        let streamed_thinking: String = chunks
            .iter()
            .filter_map(|chunk| match chunk {
                ResponseChunk::BlockDelta {
                    item,
                    delta: ContentDelta::Text(text),
                    ..
                } if item == "0" => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(streamed_thinking, "original private text");
        chunks
            .iter_mut()
            .for_each(|chunk| bind_reasoning_scope(chunk, &scope));
        let (mut items, _, reason) = assembled(&chunks).finish().unwrap();
        assert_eq!(reason, StopReason::ToolUse);
        assert_eq!(
            items[0].blocks[0].content.reasoning_content(),
            Some("original private text")
        );
        assert_eq!(items[0].replay.as_ref().unwrap().payload, native);
        assert_eq!(items[1].replay.as_ref().unwrap().payload, redacted);
        // UI summaries are not authoritative signed thinking and must never be
        // used to reconstruct the native payload, even after persistence.
        items[0].blocks[0].content = BlockContent::Reasoning {
            text: "display summary only".into(),
        };
        request.history.push(Message::Assistant(items));
        request.history.push(Message::Tool(vec![ToolResult {
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
        let assistant =
            |request: &ModelRequest| encode(request).unwrap()["messages"][1]["content"].clone();
        for foreign_scope in [
            reasoning_scope("other-provider", "https://api.example/v1/messages"),
            reasoning_scope("anthropic", "https://other.example/v1/messages"),
        ] {
            let mut foreign = original.clone();
            filter_reasoning_scope(&mut foreign, &foreign_scope);
            let Message::Assistant(items) = &foreign.history[1] else {
                unreachable!()
            };
            assert_eq!(items.len(), 3);
            assert_eq!(
                items[0].blocks[0].content.reasoning_content(),
                Some("display summary only")
            );
            assert!(items[0].replay.is_none());
            assert_eq!(assistant(&foreign), json!([tool]));
        }
        let mut foreign = original.clone();
        foreign.model = "different-model".into();
        assert_eq!(assistant(&foreign), json!([tool]));
        assert_eq!(assistant(&original)[0], native);
    }

    #[test]
    fn interleaved_tools_are_finalized_in_native_index_order() {
        let mut decoder = started();
        decode(
            &mut decoder,
            vec![
                block_start(1, tool_use("call-1")),
                block_start(0, tool_use("call-0")),
            ],
        );
        let chunks = decode(&mut decoder, vec![json_delta(1, "{\"x\":1}")]);
        assert_eq!(
            chunks,
            [ResponseChunk::BlockDelta {
                item: "1".into(),
                block: "0".into(),
                delta: ContentDelta::JsonFragment("{\"x\":1}".into())
            }]
        );
        assert!(decode(&mut decoder, vec![stop(1)]).is_empty());
        let chunks = decode(&mut decoder, vec![stop(0)]);
        assert_eq!(chunks.len(), 4);
        assert!(matches!(&chunks[0],ResponseChunk::BlockEnded { item, .. } if item == "0"));
        assert!(
            matches!(&chunks[2],ResponseChunk::BlockEnded { item, content:BlockContent::ToolCall(call), .. } if item == "1" && Value::Object(call.arguments().clone()) == json!({"x":1}))
        );
    }

    #[test]
    fn abnormal_tool_stops_preserve_completed_reasoning_and_final_usage() {
        let signed = json!({"type":"thinking", "thinking":"private text",
            "signature":"signed+/=", "future_field":{"opaque":true}});
        let redacted =
            json!({"type":"redacted_thinking", "data":"encrypted+/=", "future_field":42});
        for (reason, expected) in [
            ("max_tokens", StopReason::MaxTokens),
            ("model_context_window_exceeded", StopReason::MaxTokens),
            ("refusal", StopReason::ContentFilter),
        ] {
            // A valid object is also provisional until the terminal reason, and a
            // truncated tool must not block later completed reasoning.
            for input in [r#"{"path":"part"#, r#"{"path":"complete"}"#, "[]"] {
                for tool_first in [false, true] {
                    let (tool, reasoning) = if tool_first { (0, [1, 2]) } else { (2, [0, 1]) };
                    let mut frames = vec![start()];
                    for index in 0..3 {
                        if index == tool {
                            frames.extend([
                                block_start(index, tool_use("call_1")),
                                json_delta(index, input),
                            ]);
                        } else {
                            let native = if index == reasoning[0] {
                                &signed
                            } else {
                                &redacted
                            };
                            frames.push(block_start(index, native.clone()));
                        }
                        frames.push(stop(index));
                    }
                    frames.extend([terminal(reason, 19), message_stop()]);
                    let mut decoder = Decoder::new("claude-test".into());
                    let chunks = decode(&mut decoder, frames);
                    decoder.finish().unwrap();
                    assert!(chunks.contains(&ResponseChunk::ItemDiscarded {
                        id: tool.to_string()
                    }));
                    let (items, actual_usage, stop_reason) = assembled(&chunks).finish().unwrap();
                    assert_eq!((stop_reason, actual_usage), (expected.clone(), usage(19)));
                    let payloads: Vec<_> = items
                        .iter()
                        .map(|item| (item.kind, &item.replay.as_ref().unwrap().payload))
                        .collect();
                    assert_eq!(
                        payloads,
                        [
                            (ItemKind::Reasoning, &signed),
                            (ItemKind::Reasoning, &redacted)
                        ]
                    );
                }
            }
        }
    }

    #[test]
    fn malformed_tool_input_still_errors_on_normal_terminal_reason() {
        for reason in ["tool_use", "end_turn", "stop_sequence", "pause_turn"] {
            for input in ["{", "[]", ""] {
                let mut decoder = started();
                decode(
                    &mut decoder,
                    vec![block_start(0, tool_use("call_1")), json_delta(0, input)],
                );
                assert!(decode(&mut decoder, vec![stop(0)]).is_empty());
                assert!(decoder.decode(&event(terminal(reason, 19))).is_err());
                assert!(decoder.finish().is_err());
            }
        }
    }

    #[test]
    fn truncated_or_absent_stream_is_an_error_and_ping_is_a_noop() {
        let mut absent = Decoder::new("model".into());
        assert!(decode(&mut absent, vec![json!({"type":"ping"})]).is_empty());
        assert!(absent.finish().is_err());
        for frames in [
            vec![],
            vec![block_start(0, json!({"type":"text","text":"partial"}))],
            vec![terminal("end_turn", 1)],
        ] {
            let mut decoder = started();
            decode(&mut decoder, frames);
            assert!(decoder.finish().is_err());
        }
        let mut complete = started();
        decode(&mut complete, vec![terminal("end_turn", 1), message_stop()]);
        assert!(complete.decode(&event(message_stop())).is_err());
    }
}
