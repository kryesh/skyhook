//! Anthropic Messages streaming lifecycle and response decoding.
use std::collections::BTreeMap;

use serde_json::Value;

use crate::provider::{
    ProviderError, ProviderErrorKind,
    protocol::{AssistantItem, Binding, CutReason, ItemKind, Replay, ResponseEvent, Scope, Usage},
};

use super::{
    common::{Finish, block_ref, delta, item_id, position, replay, single_block},
    transport::SseEvent,
};

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
        replay: Option<Replay>,
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
}

/// Signed thinking is bound to the exact conversation before it, so compaction and mode switches drop it.
fn bound_replay(model: &str, scope: &Scope, native: &Value) -> Replay {
    replay(
        "anthropic",
        model,
        scope,
        native.clone(),
        Binding::Conversation,
    )
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
        "rate_limit_error" => ProviderErrorKind::RateLimited { retry_after: None },
        "invalid_request_error" if text.contains("prompt is too long") => {
            ProviderErrorKind::ContextWindowExceeded
        }
        "invalid_request_error" | "not_found_error" | "request_too_large" => {
            ProviderErrorKind::InvalidRequest
        }
        _ => ProviderErrorKind::Unavailable { retry_after: None },
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
    ProviderError { kind, message }
}

/// Unknown reasons, including `pause_turn` (its calls are not final), end the turn
/// without executing tools.
fn classify_stop(reason: &str) -> Finish {
    match reason.to_ascii_lowercase().as_str() {
        "tool_use" | "end_turn" | "stop_sequence" => Finish::Normal,
        "max_tokens" | "model_context_window_exceeded" => Finish::Cut(CutReason::MaxTokens),
        "refusal" => Finish::Cut(CutReason::Refusal),
        _ => Finish::Cut(CutReason::Incomplete),
    }
}

impl Block {
    fn is_tool(&self) -> bool {
        matches!(
            self,
            Self::Streaming(StreamingBlock::Tool { .. })
                | Self::Completed(CompletedBlock::Tool(_))
                | Self::PendingToolError(_)
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
    fn start(mut native: Value, id: usize) -> Result<(Self, Vec<ResponseEvent>), ProviderError> {
        let kind = native.get("type").and_then(Value::as_str).unwrap_or("");
        let (block, kind, text) = match kind {
            "text" => {
                let text = ensure_string(&mut native, "text");
                (Self::Text(native), ItemKind::Text, text)
            }
            "thinking" => {
                let text = ensure_string(&mut native, "thinking");
                ensure_string(&mut native, "signature");
                (Self::Thinking(native), ItemKind::Reasoning, text)
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
                    String::new(),
                )
            }
            _ => return Ok((Self::Ignored, Vec::new())),
        };
        let mut events = Vec::new();
        if !text.is_empty() {
            events.push(delta(block_ref(id), kind, text));
        }
        Ok((block, events))
    }

    fn complete(&self, model: &str, scope: &Scope) -> Result<Block, ProviderError> {
        let content = match self {
            Self::Text(native) => CompletedBlock::Text(string(native, "text")?.into()),
            Self::Thinking(native) => CompletedBlock::Reasoning {
                text: string(native, "thinking")?.into(),
                replay: (!string(native, "signature")?.is_empty())
                    .then(|| bound_replay(model, scope, native)),
            },
            Self::RedactedThinking(native) => CompletedBlock::Reasoning {
                // Preserve optional display text from the original raw block.
                text: native
                    .get("thinking")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .into(),
                replay: Some(bound_replay(model, scope, native)),
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
/// the completion lists them in index order even if the SSE blocks interleave.
pub(crate) struct Decoder {
    model: String,
    scope: Scope,
    started: bool,
    message_delta: bool,
    stopped: bool,
    finish: Option<Finish>,
    blocks: BTreeMap<usize, Block>,
    usage: NativeUsage,
}

impl Decoder {
    pub(crate) fn new(model: String, scope: Scope) -> Self {
        Self {
            model,
            scope,
            started: false,
            message_delta: false,
            stopped: false,
            finish: None,
            blocks: BTreeMap::new(),
            usage: NativeUsage::default(),
        }
    }

    pub(crate) fn decode(&mut self, event: &SseEvent) -> Result<Vec<ResponseEvent>, ProviderError> {
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
        let mut events = Vec::new();
        if kind == "message_start" {
            if self.started {
                return Err(protocol("duplicate message_start"));
            }
            self.started = true;
            let usage = value
                .get("message")
                .and_then(|message| message.get("usage"));
            self.push_usage(usage, &mut events)?;
            return Ok(events);
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
                events.extend(started);
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
                let value = value
                    .get("delta")
                    .ok_or_else(|| protocol("missing content delta"))?;
                let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
                match (block, kind) {
                    (StreamingBlock::Text(native), "text_delta")
                    | (StreamingBlock::Thinking(native), "thinking_delta") => {
                        let field = kind.trim_end_matches("_delta");
                        let text = string(value, field)?;
                        append(native, field, text)?;
                        let kind = if field == "text" {
                            ItemKind::Text
                        } else {
                            ItemKind::Reasoning
                        };
                        if !text.is_empty() {
                            events.push(delta(block_ref(id), kind, text));
                        }
                    }
                    (StreamingBlock::Thinking(native), "signature_delta") => {
                        append(native, "signature", string(value, "signature")?)?;
                    }
                    (StreamingBlock::Tool { partial_json, .. }, "input_json_delta") => {
                        let fragment = string(value, "partial_json")?;
                        partial_json
                            .get_or_insert_with(String::new)
                            .push_str(fragment);
                        if !fragment.is_empty() {
                            events.push(delta(block_ref(id), ItemKind::ToolCall, fragment));
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
                *block = streaming.complete(&self.model, &self.scope)?;
            }
            "message_delta" => {
                let reason = value
                    .get("delta")
                    .and_then(|delta| delta.get("stop_reason"))
                    .and_then(Value::as_str);
                // A reasonless delta only carries usage.
                if let Some(reason) = reason {
                    if self.message_delta {
                        self.revise(reason);
                    } else {
                        self.terminate(Some(reason))?;
                    }
                }
                self.push_usage(value.get("usage"), &mut events)?;
            }
            "message_stop" => {
                if !self.message_delta {
                    self.terminate(None)?;
                }
                self.push_usage(value.get("usage"), &mut events)?;
                events.push(self.end()?);
            }
            _ => {}
        }
        Ok(events)
    }

    /// Settle the finish. Unknown or missing reasons cut tool calls; a normal finish
    /// surfaces a tool input that never became a call.
    fn terminate(&mut self, reason: Option<&str>) -> Result<(), ProviderError> {
        self.require_all_blocks_ended()?;
        let finish = match reason {
            Some(reason) => classify_stop(reason),
            None if self.blocks.values().any(Block::is_tool) => Finish::Cut(CutReason::Incomplete),
            None => Finish::Normal,
        };
        if finish == Finish::Normal {
            for block in self.blocks.values() {
                if let Block::PendingToolError(error) = block {
                    return Err(error.clone());
                }
            }
        }
        self.finish = Some(finish);
        self.message_delta = true;
        Ok(())
    }

    /// A later abnormal reason retracts tool calls; nothing revives them. Without
    /// tool calls the model already finished, so the first reason stands.
    fn revise(&mut self, reason: &str) {
        if let (Some(Finish::Normal), cut @ Finish::Cut(_)) = (self.finish, classify_stop(reason))
            && self.blocks.values().any(Block::is_tool)
        {
            self.finish = Some(cut);
        }
    }

    /// The completion in native index order. Only reached once `terminate` settled
    /// the finish; a cut leaves out every tool call, complete or not.
    fn end(&mut self) -> Result<ResponseEvent, ProviderError> {
        self.stopped = true;
        let finish = self.finish.expect("settled by terminate");
        let mut items = Vec::new();
        for (index, block) in std::mem::take(&mut self.blocks) {
            let (id, position) = (item_id(index), position(index)?);
            let item = match block {
                Block::Completed(CompletedBlock::Text(text)) => AssistantItem::Text {
                    id,
                    position,
                    blocks: single_block(index, text),
                },
                Block::Completed(CompletedBlock::Reasoning { text, replay }) => {
                    AssistantItem::Reasoning {
                        id,
                        position,
                        blocks: single_block(index, text),
                        replay,
                    }
                }
                Block::Completed(CompletedBlock::Tool(call)) if finish == Finish::Normal => {
                    AssistantItem::ToolCall { id, position, call }
                }
                // A pending tool error survives only into a cut: `terminate` raised it
                // for a normal finish.
                Block::Completed(CompletedBlock::Tool(_))
                | Block::PendingToolError(_)
                | Block::Skipped => continue,
                Block::Streaming(_) => {
                    return Err(protocol("message ended with open content blocks"));
                }
            };
            items.push(item);
        }
        Ok(ResponseEvent::End(finish.complete(items)?))
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
        events: &mut Vec<ResponseEvent>,
    ) -> Result<(), ProviderError> {
        if let Some(value) = value.filter(|value| value.is_object()) {
            let usage = self.update_usage(value)?;
            events.push(ResponseEvent::Usage(usage));
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
        let input_tokens = next.input.saturating_add(next.creation);
        self.usage = next;
        Ok(Usage {
            input_tokens,
            cached_input_tokens: next.cached,
            output_tokens: next.output,
        })
    }

    pub(crate) fn finish(&mut self) -> Result<Vec<ResponseEvent>, ProviderError> {
        if self.stopped {
            return Ok(Vec::new());
        }
        // Some proxies close the stream after the terminal message_delta.
        if self.message_delta {
            return Ok(vec![self.end()?]);
        }
        Err(protocol("unexpected EOF before message_stop"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::backends::common::tests::{Reduced, reduce, scope};
    use crate::provider::protocol::{Completion, Message, ModelRequest, Outcome, ToolResult};
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
        let mut decoder = Decoder::new("claude-test".into(), scope());
        let usage = usage(1);
        let events = decoder.decode(&event(start())).unwrap();
        assert_eq!(events, [ResponseEvent::Usage(usage)]);
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

    fn decode(decoder: &mut Decoder, frames: Vec<Value>) -> Vec<ResponseEvent> {
        let mut events = Vec::new();
        for frame in frames {
            events.extend(decoder.decode(&event(frame)).unwrap());
        }
        events
    }

    /// Decode a whole message and reduce it as the runtime would.
    fn reduced(model: &str, frames: Vec<Value>) -> Reduced {
        let mut decoder = Decoder::new(model.into(), scope());
        let events = decode(&mut decoder, frames);
        decoder.finish().unwrap();
        reduce(events)
    }

    fn calls(reduced: &Reduced) -> Vec<(String, Value)> {
        reduced
            .items()
            .iter()
            .filter_map(AssistantItem::call)
            .map(|call| {
                (
                    call.name().to_owned(),
                    Value::Object(call.arguments().clone()),
                )
            })
            .collect()
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
            streaming.complete("vendor-model", &scope()).unwrap()
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
                streaming.complete("vendor-model", &scope()).unwrap()
            else {
                panic!("reasoning completion required");
            };
            assert_eq!((text.as_str(), replay.is_none()), ("reason", true));
        }
    }

    #[test]
    fn text_lifecycle_preserves_initial_and_streamed_text_and_usage() {
        let reduced = reduced(
            "claude",
            vec![
                start(),
                block_start(0, json!({"type":"text","text":"hel"})),
                delta(0, json!({"type":"text_delta","text":"lo"})),
                stop(0),
                terminal("end_turn", 7),
                message_stop(),
            ],
        );
        // The initial text streams as a delta like any later fragment.
        assert_eq!(reduced.streamed(ItemKind::Text), ["hello"]);
        let item = &reduced.items()[0];
        assert_eq!(item.id().as_str(), "0");
        assert_eq!(item.text_content().as_deref(), Some("hello"));
        assert_eq!(
            (reduced.usage.output_tokens, reduced.completion.outcome()),
            (7, Outcome::Answer)
        );
    }

    #[test]
    fn signed_and_redacted_reasoning_tool_roundtrip_is_scoped() {
        use crate::provider::backends::common::{filter_reasoning_scope, reasoning_scope};
        let mut request = crate::provider::backends::common::tests::request("claude-test");
        let scope = reasoning_scope("anthropic", "https://api.example/v1/messages");
        let native = json!({"type":"thinking", "thinking":"original private text", "signature":"sig+/=",
            "future_field":{"opaque":"preserve"}});
        let redacted =
            json!({"type":"redacted_thinking", "data":"encrypted+/=", "future_field":42});
        let tool =
            json!({"type":"tool_use", "id":"call_1", "name":"inspect", "input":{"path":"test"}});
        let events = decode(
            &mut Decoder::new(request.model.clone(), scope.clone()),
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
        let reduced = reduce(events);
        assert_eq!(
            reduced.streamed(ItemKind::Reasoning),
            ["original private text"]
        );
        assert_eq!(reduced.completion.outcome(), Outcome::ToolUse);
        let mut items = reduced.items().to_vec();
        assert_eq!(
            items[0].reasoning_text().as_deref(),
            Some("original private text")
        );
        assert_eq!(items[0].replay().unwrap().payload, native);
        assert_eq!(items[1].replay().unwrap().payload, redacted);
        // UI summaries are not authoritative signed thinking and must never be
        // used to reconstruct the native payload, even after persistence.
        let AssistantItem::Reasoning { blocks, .. } = &mut items[0] else {
            panic!("signed thinking is a reasoning item")
        };
        blocks[0].text = "display summary only".into();
        request.history.push(Message::Assistant(items));
        request.history.push(Message::Tool(vec![ToolResult {
            call_id: "call_1".into(),
            name: "inspect".into(),
            result: json!({"ok":true}),
            images: vec![],
            is_error: false,
        }]));
        let original = request;
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
                items[0].reasoning_text().as_deref(),
                Some("display summary only")
            );
            assert!(items[0].replay().is_none());
            assert_eq!(assistant(&foreign), json!([tool]));
        }
        let mut foreign = original.clone();
        foreign.model = "different-model".into();
        assert_eq!(assistant(&foreign), json!([tool]));
        assert_eq!(assistant(&original)[0], native);
    }

    #[test]
    fn interleaved_tools_are_completed_in_native_index_order() {
        let mut decoder = started();
        decode(
            &mut decoder,
            vec![
                block_start(1, tool_use("call-1")),
                block_start(0, tool_use("call-0")),
            ],
        );
        let events = decode(&mut decoder, vec![json_delta(1, "{\"x\":1}")]);
        assert_eq!(
            events,
            [crate::provider::backends::common::delta(
                block_ref(1),
                ItemKind::ToolCall,
                "{\"x\":1}"
            )]
        );
        // Block ends emit nothing: the completion carries the calls.
        assert!(decode(&mut decoder, vec![stop(1), stop(0)]).is_empty());
        let events = decode(&mut decoder, vec![terminal("tool_use", 3), message_stop()]);
        decoder.finish().unwrap();
        let reduced = reduce(events);
        let ids: Vec<_> = reduced
            .items()
            .iter()
            .map(|item| (item.id().as_str(), item.call().unwrap().id()))
            .collect();
        assert_eq!(ids, [("0", "call-0"), ("1", "call-1")]);
        assert_eq!(calls(&reduced)[1].1, json!({"x":1}));
    }

    #[test]
    fn abnormal_tool_stops_preserve_completed_reasoning_and_final_usage() {
        let signed = json!({"type":"thinking", "thinking":"private text",
            "signature":"signed+/=", "future_field":{"opaque":true}});
        let redacted =
            json!({"type":"redacted_thinking", "data":"encrypted+/=", "future_field":42});
        // A valid object is also provisional until the terminal reason, and a
        // truncated tool must not block later completed reasoning.
        for (reason, expected, input, tool_first) in [
            (
                "max_tokens",
                CutReason::MaxTokens,
                r#"{"path":"part"#,
                false,
            ),
            (
                "model_context_window_exceeded",
                CutReason::MaxTokens,
                r#"{"path":"complete"}"#,
                true,
            ),
            ("refusal", CutReason::Refusal, "[]", false),
        ] {
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
            let reduced = reduced("claude-test", frames);
            assert_eq!(
                (reduced.completion.outcome(), reduced.usage),
                (Outcome::Cut(expected), usage(19))
            );
            // The tool streamed its input but never became a call.
            assert!(!reduced.streamed(ItemKind::ToolCall).is_empty());
            let payloads: Vec<_> = reduced
                .items()
                .iter()
                .map(|item| (item.kind(), &item.replay().unwrap().payload))
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
        let mut absent = Decoder::new("model".into(), scope());
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
            [ResponseEvent::End(Completion::answer(Vec::new()).unwrap())]
        );
        let mut complete = started();
        decode(&mut complete, vec![terminal("end_turn", 1), message_stop()]);
        assert!(complete.decode(&event(message_stop())).is_err());
    }

    #[test]
    fn argumentless_tool_call_is_an_empty_object() {
        let frames = vec![
            start(),
            block_start(0, tool_use("toolu_1")),
            json_delta(0, ""),
            json_delta(0, "{\"cmd\": \""),
            json_delta(0, "ls -la /tmp\"}"),
            stop(0),
            block_start(1, tool_use("toolu_2")),
            json_delta(1, ""),
            stop(1),
            terminal("tool_use", 79),
            message_stop(),
        ];
        let reduced = reduced("model-a", frames);
        assert_eq!(reduced.completion.outcome(), Outcome::ToolUse);
        assert_eq!(
            calls(&reduced),
            [
                ("inspect".to_owned(), json!({"cmd":"ls -la /tmp"})),
                ("inspect".to_owned(), json!({}))
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
        let reduced = reduced("model-a", frames);
        assert_eq!(
            reduced.completion.outcome(),
            Outcome::Cut(CutReason::Incomplete)
        );
        assert_eq!(
            (reduced.usage.input_tokens, reduced.usage.output_tokens),
            (3, 5)
        );
        let items = reduced.items();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text_content().as_deref(), Some("hi"));
    }

    #[test]
    fn only_a_normal_final_stop_reason_keeps_tools() {
        let usage_only = json!({"type":"message_delta", "delta":{"stop_reason":null},
            "usage":{"output_tokens":2}});
        let max_tokens = Some(Outcome::Cut(CutReason::MaxTokens));
        for (tail, outcome, discarded) in [
            (vec![], None, true),
            (
                vec![json!({"type":"message_delta", "delta":{}})],
                None,
                true,
            ),
            (vec![terminal("incomplete", 2)], None, true),
            (vec![terminal("guardrail_intervened", 2)], None, true),
            (vec![terminal("MAX_TOKENS", 2)], None, true),
            (vec![terminal("pause_turn", 2)], None, true),
            // Usage-only deltas do not settle the response.
            (
                vec![
                    usage_only.clone(),
                    block_start(1, json!({"type":"text","text":"more"})),
                    stop(1),
                    usage_only,
                    terminal("max_tokens", 9),
                ],
                max_tokens,
                true,
            ),
            // A later abnormal reason retracts tools.
            (
                vec![terminal("tool_use", 3), terminal("max_tokens", 4)],
                max_tokens,
                true,
            ),
            (vec![terminal("tool_use", 2)], None, false),
            (vec![terminal("end_turn", 2)], None, false),
            (vec![terminal("stop_sequence", 2)], None, false),
        ] {
            let mut frames = vec![
                start(),
                block_start(0, tool_use("call")),
                json_delta(0, "{}"),
                stop(0),
            ];
            frames.extend(tail.clone());
            frames.push(message_stop());
            let reduced = reduced("model-a", frames);
            let actual = reduced.completion.outcome();
            assert_eq!(matches!(actual, Outcome::Cut(_)), discarded, "{tail:?}");
            if let Some(outcome) = outcome {
                assert_eq!(actual, outcome, "{tail:?}");
            }
            let tools = reduced.items().iter().filter(|item| item.call().is_some());
            assert_eq!(tools.count(), usize::from(!discarded), "{tail:?}");
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
        assert_eq!(
            error.kind,
            ProviderErrorKind::Unavailable { retry_after: None }
        );
        assert_eq!(
            error.message,
            "Anthropic overloaded_error: Overloaded, retry with Bearer abc.def"
        );
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
        let mut decoder = Decoder::new("model-a".into(), scope());
        let result: Result<Vec<_>, _> = frames("tool_use")
            .into_iter()
            .map(|frame| decoder.decode(&event(frame)))
            .collect();
        assert!(result.is_err());
        let reduced = reduced("model-a", frames("max_tokens"));
        assert_eq!(
            reduced.completion.outcome(),
            Outcome::Cut(CutReason::MaxTokens)
        );
        assert!(reduced.items().is_empty());
    }

    #[test]
    fn a_later_abnormal_reason_does_not_revise_a_text_response() {
        let reduced = reduced(
            "model-a",
            vec![
                start(),
                block_start(0, json!({"type":"text","text":"partial"})),
                stop(0),
                terminal("end_turn", 2),
                terminal("max_tokens", 3),
                message_stop(),
            ],
        );
        assert_eq!(reduced.completion.outcome(), Outcome::Answer);
        assert_eq!(
            reduced.items()[0].text_content().as_deref(),
            Some("partial")
        );
    }
}
