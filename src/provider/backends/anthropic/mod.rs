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
        unreadable: bool,
    },
    // Unrepresentable block types (server tools, future types).
    Ignored,
}

enum CompletedBlock {
    Text(String),
    Reasoning {
        text: String,
        // None when the signature was stripped: displayed, never replayed.
        replay: Option<ReplayEnvelope>,
    },
    Tool(crate::provider::protocol::ToolCall),
}

enum Block {
    Streaming(StreamingBlock),
    Completed(CompletedBlock),
    // Incomplete tool input awaits the terminal stop reason: abnormal stops
    // discard it, ordinary stops surface the original error.
    PendingToolError(ProviderError),
    Skipped,
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

fn stream_error(value: &Value) -> ProviderError {
    let error = value.get("error").unwrap_or(value);
    let error_type = error.get("type").and_then(Value::as_str).unwrap_or("");
    let text = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let kind = match error_type {
        "authentication_error" | "permission_error" => ProviderErrorKind::Authentication,
        "rate_limit_error" => ProviderErrorKind::RateLimited,
        "invalid_request_error" if text.contains("prompt is too long") => {
            ProviderErrorKind::ContextWindowExceeded
        }
        "invalid_request_error" | "not_found_error" | "request_too_large" => {
            ProviderErrorKind::InvalidRequest
        }
        _ => ProviderErrorKind::Response,
    };
    let known = matches!(
        error_type,
        "authentication_error"
            | "permission_error"
            | "rate_limit_error"
            | "invalid_request_error"
            | "not_found_error"
            | "request_too_large"
            | "overloaded_error"
            | "api_error"
    );
    let mut message = if known {
        format!("Anthropic {error_type}")
    } else {
        "Anthropic server error".to_owned()
    };
    super::errors::append_server_message(&mut message, value);
    ProviderError {
        retry_after: None,
        kind,
        message,
    }
}

fn classify_stop(reason: &str) -> StopReason {
    match reason.to_ascii_lowercase().as_str() {
        "tool_use" => StopReason::ToolUse,
        "end_turn" => StopReason::EndTurn,
        "stop_sequence" => StopReason::StopSequence,
        "max_tokens" | "model_context_window_exceeded" => StopReason::MaxTokens,
        "refusal" => StopReason::ContentFilter,
        // Includes `pause_turn`: its calls are not final, so the turn ends
        // without executing them.
        _ => StopReason::Other(reason.into()),
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

/// Read a string field that later deltas append to, defaulting it to empty.
fn ensure_string(native: &mut Value, field: &str) -> String {
    match native.get(field).and_then(Value::as_str) {
        Some(text) => text.to_owned(),
        None => {
            native[field] = Value::String(String::new());
            String::new()
        }
    }
}

impl StreamingBlock {
    fn start(mut native: Value, id: usize) -> Result<(Self, Vec<ResponseChunk>), ProviderError> {
        let kind = native.get("type").and_then(Value::as_str).unwrap_or("");
        let (block, kind, block_kind, text) = match kind {
            "text" => {
                let text = ensure_string(&mut native, "text");
                (Self::Text(native), ItemKind::Text, BlockKind::Text, text)
            }
            "thinking" => {
                let text = ensure_string(&mut native, "thinking");
                ensure_string(&mut native, "signature");
                (
                    Self::Thinking(native),
                    ItemKind::Reasoning,
                    BlockKind::Reasoning,
                    text,
                )
            }
            "redacted_thinking"
                if native
                    .get("data")
                    .and_then(Value::as_str)
                    .is_some_and(|data| !data.is_empty()) =>
            {
                (
                    Self::RedactedThinking(native),
                    ItemKind::Reasoning,
                    BlockKind::Reasoning,
                    String::new(),
                )
            }
            "tool_use" => {
                if string(&native, "id")?.is_empty() || string(&native, "name")?.is_empty() {
                    return Err(protocol("tool_use requires nonempty id and name"));
                }
                initial_input(&native)?;
                (
                    Self::Tool {
                        native,
                        partial_json: None,
                        unreadable: false,
                    },
                    ItemKind::ToolCall,
                    BlockKind::ToolCallArguments,
                    String::new(),
                )
            }
            _ => return Ok((Self::Ignored, Vec::new())),
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
            Self::Thinking(native) => CompletedBlock::Reasoning {
                text: string(native, "thinking")?.into(),
                replay: (!string(native, "signature")?.is_empty())
                    .then(|| bound_envelope(model, native)),
            },
            Self::RedactedThinking(native) => CompletedBlock::Reasoning {
                // Preserve optional display text from the original raw block.
                text: native
                    .get("thinking")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .into(),
                replay: Some(bound_envelope(model, native)),
            },
            Self::Tool {
                unreadable: true, ..
            } => {
                return Ok(Block::PendingToolError(protocol(
                    "unsupported delta for tool input",
                )));
            }
            Self::Tool {
                native,
                partial_json,
                ..
            } => match tool_content(native, partial_json.as_deref()) {
                Ok(call) => CompletedBlock::Tool(call),
                Err(error) => return Ok(Block::PendingToolError(error)),
            },
            Self::Ignored => return Ok(Block::Skipped),
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
    tools_discarded: bool,
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
            tools_discarded: false,
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
        // The payload type wins over a missing or mislabeled SSE event name.
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .or(event.event.as_deref())
            .ok_or_else(|| protocol("missing event type"))?;
        if kind == "error" {
            return Err(stream_error(&value));
        }
        if kind == "ping" {
            return Ok(Vec::new());
        }
        let mut chunks = Vec::new();
        if kind == "message_start" {
            if self.started {
                return Err(protocol("duplicate message_start"));
            }
            self.started = true;
            let usage = value
                .get("message")
                .and_then(|message| message.get("usage"));
            self.push_usage(usage, &mut chunks)?;
            return Ok(chunks);
        }
        // A stream without message_start is still decoded.
        self.started = true;
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
                let (block, started) = StreamingBlock::start(native, id)?;
                self.blocks.insert(id, Block::Streaming(block));
                chunks.extend(started);
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
                let kind = delta.get("type").and_then(Value::as_str).unwrap_or("");
                match (block, kind) {
                    (StreamingBlock::Text(native), "text_delta")
                    | (StreamingBlock::Thinking(native), "thinking_delta") => {
                        let field = kind.trim_end_matches("_delta");
                        let text = string(delta, field)?;
                        append(native, field, text)?;
                        if !text.is_empty() {
                            chunks.push(block_delta(id, ContentDelta::Text(text.into())));
                        }
                    }
                    (StreamingBlock::Thinking(native), "signature_delta") => {
                        append(native, "signature", string(delta, "signature")?)?;
                    }
                    (StreamingBlock::Tool { partial_json, .. }, "input_json_delta") => {
                        let fragment = string(delta, "partial_json")?;
                        partial_json
                            .get_or_insert_with(String::new)
                            .push_str(fragment);
                        if !fragment.is_empty() {
                            chunks
                                .push(block_delta(id, ContentDelta::JsonFragment(fragment.into())));
                        }
                    }
                    (StreamingBlock::Tool { unreadable, .. }, _) => *unreadable = true,
                    _ => {}
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
                chunks.extend(self.emit_completed_blocks(false)?);
            }
            "message_delta" => {
                let reason = value
                    .get("delta")
                    .and_then(|delta| delta.get("stop_reason"))
                    .and_then(Value::as_str);
                // A reasonless delta only carries usage.
                if let Some(reason) = reason {
                    chunks = if self.message_delta {
                        self.revise(reason)
                    } else {
                        self.terminate(Some(reason))?
                    };
                }
                self.push_usage(value.get("usage"), &mut chunks)?;
            }
            "message_stop" => {
                if !self.message_delta {
                    chunks.extend(self.terminate(None)?);
                }
                self.push_usage(value.get("usage"), &mut chunks)?;
                chunks.push(self.end());
            }
            _ => {}
        }
        Ok(chunks)
    }

    /// Settle the stop reason. Unknown or missing reasons discard tool calls.
    fn terminate(&mut self, reason: Option<&str>) -> Result<Vec<ResponseChunk>, ProviderError> {
        self.require_all_blocks_ended()?;
        let stop_reason = match reason {
            Some(reason) => classify_stop(reason),
            None if self.blocks.values().any(Block::is_tool) => {
                StopReason::Other("missing_stop_reason".into())
            }
            None => StopReason::EndTurn,
        };
        let discard_tools = !stop_reason.authorizes_tools();
        let mut chunks = Vec::new();
        if discard_tools {
            // Even syntactically complete inputs are unsafe on abnormal
            // stops. Retract tools already emitted, as well as pending
            // ones, before the response can be promoted for execution.
            chunks = self.discard_tools();
        } else {
            for block in self.blocks.values() {
                if let Block::PendingToolError(error) = block {
                    return Err(error.clone());
                }
            }
        }
        chunks.extend(self.emit_completed_blocks(discard_tools)?);
        self.stop_reason = Some(stop_reason);
        self.tools_discarded = discard_tools;
        self.message_delta = true;
        Ok(chunks)
    }

    /// A later abnormal reason retracts tool calls; nothing revives them.
    fn revise(&mut self, reason: &str) -> Vec<ResponseChunk> {
        let stop_reason = classify_stop(reason);
        // Without tool calls the model already finished; the first reason stands.
        if stop_reason.authorizes_tools()
            || self.tools_discarded
            || !self.blocks.values().any(Block::is_tool)
        {
            return Vec::new();
        }
        self.tools_discarded = true;
        self.stop_reason = Some(stop_reason);
        self.discard_tools()
    }

    fn discard_tools(&self) -> Vec<ResponseChunk> {
        self.blocks
            .iter()
            .filter(|(_, block)| block.is_tool())
            .map(|(id, _)| ResponseChunk::ItemDiscarded { id: id.to_string() })
            .collect()
    }

    /// Only reached once `terminate` has settled the stop reason.
    fn end(&mut self) -> ResponseChunk {
        self.stopped = true;
        ResponseChunk::ResponseEnded {
            stop_reason: self.stop_reason.clone().expect("settled by terminate"),
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
            if matches!(block, Block::Skipped) {
                *block = Block::EmittedOther;
            } else if discard_tools && block.is_tool() {
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
                        (BlockContent::Reasoning { text }, replay)
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

    /// Merge a usage object, if present, and report the new totals.
    fn push_usage(
        &mut self,
        value: Option<&Value>,
        chunks: &mut Vec<ResponseChunk>,
    ) -> Result<(), ProviderError> {
        if let Some(value) = value.filter(|value| value.is_object()) {
            let usage = self.update_usage(value)?;
            chunks.push(ResponseChunk::UsageUpdated { usage });
        }
        Ok(())
    }

    fn update_usage(&mut self, value: &Value) -> Result<Usage, ProviderError> {
        let previous = self.usage;
        let next = NativeUsage {
            input: counter(value, "input_tokens", previous.input)?,
            creation: counter(value, "cache_creation_input_tokens", previous.creation)?,
            cached: counter(value, "cache_read_input_tokens", previous.cached)?,
            output: counter(value, "output_tokens", previous.output)?,
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
        if self.stopped {
            return Ok(Vec::new());
        }
        // Some proxies close the stream after the terminal message_delta.
        if self.message_delta {
            return Ok(vec![self.end()]);
        }
        self.failed = true;
        Err(protocol("unexpected EOF before message_stop"))
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
    fn mismatched_and_unknown_deltas_are_ignored() {
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
            assert!(decode(&mut decoder, vec![delta(0, wrong_delta)]).is_empty());
            decoder.decode(&event(stop(0))).unwrap();
        }
        let mut decoder = started();
        decoder
            .decode(&event(block_start(
                0,
                json!({"type":"text", "text":"cited"}),
            )))
            .unwrap();
        let citation = json!({"type":"citations_delta", "citation":{"cited_text":"x"}});
        assert!(decode(&mut decoder, vec![delta(0, citation)]).is_empty());
    }

    #[test]
    fn signed_completion_keeps_opaque_native_fields_and_unsigned_thinking_is_display_only() {
        let native = json!({"type":"thinking", "thinking":"reason", "signature":"signed", "x-vendor":{"nested":[1,null,"opaque"]}});
        let (streaming, _) = StreamingBlock::start(native.clone(), 0).unwrap();
        let Block::Completed(CompletedBlock::Reasoning { text, replay }) =
            streaming.complete("vendor-model").unwrap()
        else {
            panic!("reasoning completion required");
        };
        assert_eq!(
            (text.as_str(), &replay.unwrap().payload),
            ("reason", &native)
        );
        for unsigned in [
            json!({"type":"thinking", "thinking":"reason", "signature":""}),
            json!({"type":"thinking", "thinking":"reason"}),
        ] {
            let (streaming, _) = StreamingBlock::start(unsigned, 0).unwrap();
            let Block::Completed(CompletedBlock::Reasoning { text, replay }) =
                streaming.complete("vendor-model").unwrap()
            else {
                panic!("reasoning completion required");
            };
            assert_eq!((text.as_str(), replay.is_none()), ("reason", true));
        }
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
        for reason in ["tool_use", "end_turn", "stop_sequence"] {
            for input in ["{", "[]", "\"text\""] {
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
    fn truncated_streams_fail_eof_after_a_terminal_delta_ends_and_ping_is_a_noop() {
        let mut absent = Decoder::new("model".into());
        assert!(decode(&mut absent, vec![json!({"type":"ping"})]).is_empty());
        assert!(absent.finish().is_err());
        for frames in [
            vec![],
            vec![block_start(0, json!({"type":"text","text":"partial"}))],
        ] {
            let mut decoder = started();
            decode(&mut decoder, frames);
            assert!(decoder.finish().is_err());
        }
        let mut closed = started();
        decode(&mut closed, vec![terminal("end_turn", 1)]);
        assert_eq!(
            closed.finish().unwrap(),
            [ResponseChunk::ResponseEnded {
                stop_reason: StopReason::EndTurn
            }]
        );
        let mut complete = started();
        decode(&mut complete, vec![terminal("end_turn", 1), message_stop()]);
        assert!(complete.decode(&event(message_stop())).is_err());
    }

    #[test]
    fn argumentless_tool_call_is_an_empty_object() {
        let frames = vec![
            json!({"type":"message_start", "message":{"model":"model-a", "id":"msg_1",
                "type":"message", "role":"assistant", "content":[], "stop_reason":null,
                "stop_sequence":null, "stop_details":null,
                "usage":{"input_tokens":452, "cache_creation_input_tokens":0,
                    "cache_read_input_tokens":0, "output_tokens":16, "service_tier":"standard"}}}),
            block_start(
                0,
                json!({"type":"tool_use", "id":"toolu_1", "name":"run", "input":{}}),
            ),
            json_delta(0, ""),
            json_delta(0, "{\"cmd\": \""),
            json_delta(0, "ls -la /tmp\"}"),
            stop(0),
            block_start(
                1,
                json!({"type":"tool_use", "id":"toolu_2", "name":"list_jobs", "input":{}}),
            ),
            json_delta(1, ""),
            stop(1),
            json!({"type":"message_delta", "delta":{"stop_reason":"tool_use", "stop_sequence":null,
                "stop_details":null}, "usage":{"input_tokens":452, "cache_creation_input_tokens":0,
                "cache_read_input_tokens":0, "output_tokens":79,
                "output_tokens_details":{"thinking_tokens":0}}}),
            json!({"type":"message_stop", "usage":{"input_tokens":452, "output_tokens":79}}),
        ];
        let mut decoder = Decoder::new("model-a".into());
        let chunks = decode(&mut decoder, frames);
        decoder.finish().unwrap();
        let (items, _, stop_reason) = assembled(&chunks).finish().unwrap();
        assert_eq!(stop_reason, StopReason::ToolUse);
        let calls: Vec<_> = items
            .iter()
            .flat_map(|item| &item.blocks)
            .filter_map(|block| match &block.content {
                BlockContent::ToolCall(call) => Some((
                    call.name().to_owned(),
                    Value::Object(call.arguments().clone()),
                )),
                _ => None,
            })
            .collect();
        assert_eq!(
            calls,
            [
                ("run".to_owned(), json!({"cmd":"ls -la /tmp"})),
                ("list_jobs".to_owned(), json!({}))
            ]
        );
    }

    #[test]
    fn unknown_events_blocks_and_missing_envelope_fields_are_tolerated() {
        let frames = vec![
            // Minimal message_start: no id, model, content, or usage.
            json!({"type":"message_start", "message":{"type":"message", "role":"assistant"}}),
            json!({"type":"vendor_heartbeat", "detail":{}}),
            block_start(
                0,
                json!({"type":"server_tool_use", "id":"srv", "name":"web_search"}),
            ),
            delta(
                0,
                json!({"type":"input_json_delta", "partial_json":"{\"q\":1}"}),
            ),
            stop(0),
            block_start(
                1,
                json!({"type":"text", "text":"hi", "citations":[{"cited_text":"x"}]}),
            ),
            stop(1),
            // Counters as strings; a smaller late value does not lower the total.
            json!({"type":"message_delta", "delta":{"stop_reason":"pause_for_vendor"},
                "usage":{"output_tokens":"5", "input_tokens":"3"}}),
            json!({"type":"message_delta", "delta":{}, "usage":{"output_tokens":4}}),
            message_stop(),
        ];
        let mut decoder = Decoder::new("model-a".into());
        let chunks = decode(&mut decoder, frames);
        decoder.finish().unwrap();
        let (items, usage, stop_reason) = assembled(&chunks).finish().unwrap();
        assert_eq!(stop_reason, StopReason::Other("pause_for_vendor".into()));
        assert_eq!((usage.input_tokens, usage.output_tokens), (3, 5));
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].blocks[0].content,
            BlockContent::Text { text: "hi".into() }
        );
    }

    #[test]
    fn missing_or_unknown_stop_reasons_never_keep_tools() {
        let tool_frames = || {
            vec![
                start(),
                block_start(0, tool_use("call")),
                json_delta(0, "{}"),
                stop(0),
            ]
        };
        for terminal_frames in [
            vec![message_stop()],
            vec![json!({"type":"message_delta", "delta":{}}), message_stop()],
            vec![terminal("incomplete", 2), message_stop()],
            vec![terminal("guardrail_intervened", 2), message_stop()],
            vec![terminal("MAX_TOKENS", 2), message_stop()],
            vec![terminal("pause_turn", 2), message_stop()],
        ] {
            let mut frames = tool_frames();
            frames.extend(terminal_frames.clone());
            let mut decoder = Decoder::new("model-a".into());
            let chunks = decode(&mut decoder, frames);
            decoder.finish().unwrap();
            assert!(
                chunks.contains(&ResponseChunk::ItemDiscarded { id: "0".into() }),
                "{terminal_frames:?}"
            );
        }
        for reason in ["tool_use", "end_turn", "stop_sequence"] {
            let mut frames = tool_frames();
            frames.extend([terminal(reason, 2), message_stop()]);
            let mut decoder = Decoder::new("model-a".into());
            let chunks = decode(&mut decoder, frames);
            let discarded = chunks
                .iter()
                .any(|chunk| matches!(chunk, ResponseChunk::ItemDiscarded { .. }));
            assert!(!discarded, "{reason}");
        }
    }

    #[test]
    fn stream_errors_keep_the_server_message() {
        let mut decoder = started();
        let error = decoder
            .decode(&event(
                json!({"type":"error", "error":{"type":"overloaded_error",
                "message":"Overloaded, retry with Bearer abc.def"}}),
            ))
            .unwrap_err();
        assert_eq!(error.kind, ProviderErrorKind::Response);
        assert_eq!(
            error.message,
            "Anthropic overloaded_error: Overloaded, retry with Bearer abc.def"
        );
    }

    #[test]
    fn usage_only_deltas_do_not_settle_the_response() {
        let mut decoder = Decoder::new("model-a".into());
        let usage_only = json!({"type":"message_delta", "delta":{"stop_reason":null},
            "usage":{"output_tokens":2}});
        let chunks = decode(
            &mut decoder,
            vec![
                start(),
                block_start(0, tool_use("call")),
                json_delta(0, "{}"),
                stop(0),
                usage_only.clone(),
                block_start(1, json!({"type":"text","text":"more"})),
                stop(1),
                usage_only,
                terminal("max_tokens", 9),
                message_stop(),
            ],
        );
        decoder.finish().unwrap();
        assert_eq!(decoder.stop_reason, Some(StopReason::MaxTokens));
        assert!(chunks.contains(&ResponseChunk::ItemDiscarded { id: "0".into() }));
    }

    #[test]
    fn a_later_abnormal_reason_retracts_tools() {
        let mut decoder = Decoder::new("model-a".into());
        let chunks = decode(
            &mut decoder,
            vec![
                start(),
                block_start(0, tool_use("call")),
                json_delta(0, "{}"),
                stop(0),
                terminal("tool_use", 3),
                terminal("max_tokens", 4),
                message_stop(),
            ],
        );
        assert_eq!(decoder.stop_reason, Some(StopReason::MaxTokens));
        assert!(chunks.contains(&ResponseChunk::ItemDiscarded { id: "0".into() }));
        let (items, _, stop) = assembled(&chunks).finish().unwrap();
        assert_eq!((stop, items.len()), (StopReason::MaxTokens, 0));
    }

    #[test]
    fn unreadable_tool_input_never_executes() {
        let frames = |reason: &str| {
            vec![
                start(),
                block_start(0, tool_use("call")),
                delta(
                    0,
                    json!({"type":"vendor_input_delta", "value":{"path":"/etc"}}),
                ),
                stop(0),
                terminal(reason, 2),
                message_stop(),
            ]
        };
        let mut decoder = Decoder::new("model-a".into());
        let result: Result<Vec<_>, _> = frames("tool_use")
            .into_iter()
            .map(|frame| decoder.decode(&event(frame)))
            .collect();
        assert!(result.is_err());
        let mut decoder = Decoder::new("model-a".into());
        let chunks = decode(&mut decoder, frames("max_tokens"));
        assert!(chunks.contains(&ResponseChunk::ItemDiscarded { id: "0".into() }));
    }

    #[test]
    fn a_later_abnormal_reason_does_not_revise_a_text_response() {
        let mut decoder = Decoder::new("model-a".into());
        decode(
            &mut decoder,
            vec![
                start(),
                block_start(0, json!({"type":"text","text":"partial"})),
                stop(0),
                terminal("end_turn", 2),
                terminal("max_tokens", 3),
                message_stop(),
            ],
        );
        assert_eq!(decoder.stop_reason, Some(StopReason::EndTurn));
    }
}
