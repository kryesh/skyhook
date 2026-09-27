//! Anthropic Messages streaming lifecycle and response decoding.
use std::collections::BTreeMap;

use serde_json::Value;

use super::{
    Code, NATIVE,
    native::{BlockType, initial_input, tool_content},
};
use crate::{
    named_enum::{NamedEnum, named_enum},
    provider::{
        ProviderError,
        codec::{
            common::{
                Finish, Settle, Settlement, StopReason, delta, is_signed, position, replay, tagged,
            },
            openai,
            usage::{Counters, InputAccounting, Observed, Spelling},
        },
        http::{
            errors::{self, ErrorSignals},
            transport::SseEvent,
        },
        protocol::{
            AssistantItem, Binding, Completion, ItemKind, Replay, ReplayFormat, ResponseEvent,
            Scope, ToolCall,
        },
    },
};

named_enum! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    parsed enum Event {
        MessageStart = "message_start",
        BlockStart = "content_block_start",
        BlockDelta = "content_block_delta",
        BlockStop = "content_block_stop",
        MessageDelta = "message_delta",
        MessageStop = "message_stop",
        Ping = "ping",
        Error = "error",
    }
}

named_enum! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    parsed enum DeltaType {
        Text = "text_delta",
        Thinking = "thinking_delta",
        Signature = "signature_delta",
        InputJson = "input_json_delta",
    }
}

const USAGE: Spelling = Spelling {
    input: &["/input_tokens"],
    cached: &["/cache_read_input_tokens"],
    written: &["/cache_creation_input_tokens"],
    output: &["/output_tokens"],
};

// Native kind is decoded once at block admission. Thinking keeps its opaque
// JSON so signed reasoning is never regenerated; unsigned thinking is rebuilt.
enum StreamingBlock {
    Text(String),
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
    Reasoning { text: String, replay: Replay },
    Tool(ToolCall),
}

enum Block {
    Streaming(StreamingBlock),
    Completed(CompletedBlock),
    // Incomplete tool input awaits the terminal stop reason: abnormal stops
    // discard it, ordinary stops surface the original error.
    PendingToolError(ProviderError),
    Skipped,
}

/// Signed thinking is bound to the exact conversation before it, so compaction
/// and mode switches drop it. Unsigned thinking (compatible servers) replays
/// freely, without the empty signature, until the context holds a signed block.
fn thinking_replay(model: &str, scope: &Scope, native: &Value) -> Replay {
    let field = |name| native.get(name).and_then(Value::as_str);
    if is_signed(field("signature"), field("data")) {
        return replay(
            ReplayFormat::Messages,
            model,
            scope,
            native.clone(),
            Binding::Conversation,
        );
    }
    let text = native.get("thinking").cloned().unwrap_or_default();
    replay(
        ReplayFormat::Messages,
        model,
        scope,
        serde_json::json!({"type": "thinking", "thinking": text}),
        Binding::Free,
    )
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

/// Append to a string field that the block's start ensured.
fn append(native: &mut Value, field: &str, suffix: &str) {
    if let Some(Value::String(text)) = native.get_mut(field) {
        text.push_str(suffix);
    }
}

impl StreamingBlock {
    fn start(
        mut native: Value,
        id: usize,
        events: &mut Vec<ResponseEvent>,
    ) -> Result<Self, ProviderError> {
        let mut stream = |kind, text: &str| {
            if !text.is_empty() {
                events.push(delta(id, kind, text));
            }
        };
        let field = |native: &Value, name| {
            native
                .get(name)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        Ok(match tagged(&native) {
            Some(BlockType::Text) => {
                let text = field(&native, "text");
                stream(ItemKind::Text, &text);
                Self::Text(text)
            }
            Some(BlockType::Thinking) => {
                // Later deltas append to both fields.
                for name in ["thinking", "signature"] {
                    native[name] = Value::String(field(&native, name));
                }
                stream(ItemKind::Reasoning, &field(&native, "thinking"));
                Self::Thinking(native)
            }
            Some(BlockType::RedactedThinking) if !field(&native, "data").is_empty() => {
                Self::RedactedThinking(native)
            }
            Some(BlockType::ToolUse) => {
                if NATIVE.string(&native, "id")?.is_empty()
                    || NATIVE.string(&native, "name")?.is_empty()
                {
                    return Err(NATIVE.error("tool_use requires nonempty id and name"));
                }
                initial_input(&native)?;
                Self::Tool {
                    native,
                    partial_json: None,
                    unreadable: false,
                }
            }
            Some(BlockType::RedactedThinking) | None => Self::Ignored,
        })
    }

    fn delta(
        &mut self,
        id: usize,
        value: &Value,
        events: &mut Vec<ResponseEvent>,
    ) -> Result<(), ProviderError> {
        let mut stream = |kind, text: &str| {
            if !text.is_empty() {
                events.push(delta(id, kind, text));
            }
        };
        match (self, tagged(value)) {
            (Self::Text(text), Some(DeltaType::Text)) => {
                let fragment = NATIVE.string(value, "text")?;
                text.push_str(fragment);
                stream(ItemKind::Text, fragment);
            }
            (Self::Thinking(native), Some(DeltaType::Thinking)) => {
                let fragment = NATIVE.string(value, "thinking")?;
                append(native, "thinking", fragment);
                stream(ItemKind::Reasoning, fragment);
            }
            (Self::Thinking(native), Some(DeltaType::Signature)) => {
                append(native, "signature", NATIVE.string(value, "signature")?);
            }
            (Self::Tool { partial_json, .. }, Some(DeltaType::InputJson)) => {
                let fragment = NATIVE.string(value, "partial_json")?;
                partial_json
                    .get_or_insert_with(String::new)
                    .push_str(fragment);
                stream(ItemKind::ToolCall, fragment);
            }
            (Self::Tool { unreadable, .. }, _) => *unreadable = true,
            _ => {}
        }
        Ok(())
    }

    fn complete(&self, model: &str, scope: &Scope) -> Result<Block, ProviderError> {
        let content = match self {
            Self::Text(text) => CompletedBlock::Text(text.clone()),
            Self::Thinking(native) => CompletedBlock::Reasoning {
                text: NATIVE.string(native, "thinking")?.into(),
                replay: thinking_replay(model, scope, native),
            },
            Self::RedactedThinking(native) => CompletedBlock::Reasoning {
                // Preserve optional display text from the original raw block.
                text: native
                    .get("thinking")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .into(),
                replay: thinking_replay(model, scope, native),
            },
            Self::Tool {
                unreadable: true, ..
            } => {
                return Ok(Block::PendingToolError(
                    NATIVE.error("unsupported delta for tool input"),
                ));
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

/// One decoder owns exactly one Messages response. Native indices identify blocks;
/// the completion lists them in index order even if the SSE blocks interleave.
pub(crate) struct Decoder {
    model: String,
    scope: Scope,
    started: bool,
    settlement: Settlement,
    blocks: BTreeMap<usize, Block>,
    usage: Counters,
    errors: ErrorSignals,
}

impl Decoder {
    pub(crate) fn new(model: String, scope: Scope, errors: ErrorSignals) -> Self {
        Self {
            model,
            scope,
            errors,
            started: false,
            settlement: Settlement::Open,
            blocks: BTreeMap::new(),
            usage: Counters::new(InputAccounting::FreshPrompt),
        }
    }

    pub(crate) fn decode(&mut self, event: &SseEvent) -> Result<Vec<ResponseEvent>, ProviderError> {
        if self.settlement == Settlement::Ended {
            return Err(NATIVE.error("event after message_stop"));
        }
        let value: Value =
            serde_json::from_str(&event.data).map_err(|_| NATIVE.error("invalid SSE JSON"))?;
        // The payload type wins over a missing or mislabeled SSE event name.
        let name = value
            .get("type")
            .and_then(Value::as_str)
            .or(event.event.as_deref())
            .ok_or_else(|| NATIVE.error("missing event type"))?;
        let kind = Event::parse(name);
        if matches!(
            kind,
            Some(Event::BlockStart | Event::BlockDelta | Event::BlockStop)
        ) && self.settlement != Settlement::Open
        {
            return Err(NATIVE.error("content event after terminal message_delta"));
        }
        let mut events = Vec::new();
        match kind {
            Some(Event::Error) => {
                let reading = openai::read::<Code>(&value);
                return Err(errors::classify(None, &value, reading, self.errors, None));
            }
            Some(Event::MessageStart) => {
                if self.started {
                    return Err(NATIVE.error("duplicate message_start"));
                }
                self.push_usage(value.pointer("/message/usage"), &mut events);
            }
            Some(Event::BlockStart) => {
                let id = NATIVE.index(&value, "index")?;
                if self.blocks.contains_key(&id) {
                    return Err(NATIVE.error(format_args!("duplicate content block index {id}")));
                }
                let native = value
                    .get("content_block")
                    .ok_or_else(|| NATIVE.error("missing content_block"))?
                    .clone();
                let block = StreamingBlock::start(native, id, &mut events)?;
                self.blocks.insert(id, Block::Streaming(block));
            }
            Some(Event::BlockDelta) => {
                let id = NATIVE.index(&value, "index")?;
                let Some(Block::Streaming(block)) = self.blocks.get_mut(&id) else {
                    return Err(
                        NATIVE.error(format_args!("delta for unopened or ended block {id}"))
                    );
                };
                let value = value
                    .get("delta")
                    .ok_or_else(|| NATIVE.error("missing content delta"))?;
                block.delta(id, value, &mut events)?;
            }
            Some(Event::BlockStop) => {
                let id = NATIVE.index(&value, "index")?;
                let Some(Block::Streaming(streaming)) = self.blocks.get(&id) else {
                    return Err(NATIVE.error(format_args!("stop for unopened or ended block {id}")));
                };
                let completed = streaming.complete(&self.model, &self.scope)?;
                self.blocks.insert(id, completed);
            }
            Some(Event::MessageDelta) => {
                // A reasonless delta only carries usage.
                if let Some(reason) = value.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.reason(Finish::of(StopReason::read(reason)))?;
                }
                self.push_usage(value.get("usage"), &mut events);
            }
            Some(Event::MessageStop) => {
                self.push_usage(value.get("usage"), &mut events);
                events.push(self.close()?);
            }
            Some(Event::Ping) => return Ok(events),
            None => {}
        }
        // A stream without message_start is still decoded.
        self.started = true;
        Ok(events)
    }

    /// Merge a usage report, if present, and report the new totals.
    fn push_usage(&mut self, value: Option<&Value>, events: &mut Vec<ResponseEvent>) {
        if let Some(observed) = value.and_then(|value| Observed::read(value, &USAGE)) {
            events.push(ResponseEvent::Usage(self.usage.observe(observed)));
        }
    }

    /// Some proxies close the stream after the terminal message_delta.
    pub(crate) fn finish(&mut self) -> Result<Vec<ResponseEvent>, ProviderError> {
        self.eof(|| NATIVE.error("unexpected EOF before message_stop"))
    }
}

impl Settle for Decoder {
    fn settlement(&mut self) -> &mut Settlement {
        &mut self.settlement
    }

    fn has_tools(&self) -> bool {
        self.blocks.values().any(Block::is_tool)
    }

    /// Every block must have ended; a normal finish surfaces a tool input that
    /// never became a call.
    fn admit(&self, finish: Finish) -> Result<(), ProviderError> {
        if self
            .blocks
            .values()
            .any(|block| matches!(block, Block::Streaming(_)))
        {
            return Err(NATIVE.error("message ended with open content blocks"));
        }
        if self.blocks.keys().copied().ne(0..self.blocks.len()) {
            return Err(NATIVE.error("content block indices are not contiguous from zero"));
        }
        if finish == Finish::Normal {
            for block in self.blocks.values() {
                if let Block::PendingToolError(error) = block {
                    return Err(error.clone());
                }
            }
        }
        Ok(())
    }

    /// The completion in native index order; a cut leaves out every tool call,
    /// complete or not.
    fn completion(&mut self, finish: Finish) -> Result<Completion, ProviderError> {
        let mut items = Vec::new();
        for (index, block) in std::mem::take(&mut self.blocks) {
            let (id, position) = (index.to_string(), position(index)?);
            items.push(match block {
                Block::Completed(CompletedBlock::Text(text)) => {
                    AssistantItem::text(id, position, text)
                }
                Block::Completed(CompletedBlock::Reasoning { text, replay }) => {
                    AssistantItem::reasoning(id, position, text, Some(replay))
                }
                Block::Completed(CompletedBlock::Tool(call)) if finish == Finish::Normal => {
                    AssistantItem::tool_call(id, position, call)
                }
                // Admission raised a pending tool error for a normal finish, and
                // required every block ended.
                Block::Completed(CompletedBlock::Tool(_))
                | Block::PendingToolError(_)
                | Block::Skipped
                | Block::Streaming(_) => continue,
            });
        }
        finish.complete(items)
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Dialect, encode};
    use super::*;
    use crate::provider::ProviderErrorKind;
    use crate::provider::codec::common::tests::{Reduced, reduce, scope};
    use crate::provider::protocol::{CutReason, Message, ModelRequest, Outcome, ToolResult, Usage};
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
            cache_write_input_tokens: 13,
            output_tokens,
        }
    }

    fn started() -> Decoder {
        let mut decoder = Decoder::new("claude-test".into(), scope(), ErrorSignals::NONE);
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
        let mut decoder = Decoder::new(model.into(), scope(), ErrorSignals::NONE);
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
    fn signed_completion_keeps_opaque_native_fields_and_unsigned_thinking_replays_free() {
        let native = json!({"type":"thinking", "thinking":"reason", "signature":"signed", "x-vendor":{"nested":[1,null,"opaque"]}});
        let streaming = StreamingBlock::start(native.clone(), 0, &mut Vec::new()).unwrap();
        let Block::Completed(CompletedBlock::Reasoning { text, replay }) =
            streaming.complete("vendor-model", &scope()).unwrap()
        else {
            panic!("reasoning completion required");
        };
        assert_eq!(
            (text.as_str(), &replay.payload, replay.binding),
            ("reason", &native, Binding::Conversation)
        );
        // Servers that cannot sign still get their thinking back, without the
        // empty signature, and it is not bound to the conversation.
        for unsigned in [
            json!({"type":"thinking", "thinking":"reason", "signature":""}),
            json!({"type":"thinking", "thinking":"reason"}),
        ] {
            let streaming = StreamingBlock::start(unsigned, 0, &mut Vec::new()).unwrap();
            let Block::Completed(CompletedBlock::Reasoning { text, replay }) =
                streaming.complete("vendor-model", &scope()).unwrap()
            else {
                panic!("reasoning completion required");
            };
            assert_eq!(
                (text.as_str(), &replay.payload, replay.binding),
                (
                    "reason",
                    &json!({"type":"thinking", "thinking":"reason"}),
                    Binding::Free
                )
            );
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
    fn signed_and_redacted_reasoning_tool_roundtrip_replays_to_its_model() {
        let mut request = crate::provider::codec::common::tests::request("claude-test");
        let native = json!({"type":"thinking", "thinking":"original private text", "signature":"sig+/=",
            "future_field":{"opaque":"preserve"}});
        let redacted =
            json!({"type":"redacted_thinking", "data":"encrypted+/=", "future_field":42});
        let tool =
            json!({"type":"tool_use", "id":"call_1", "name":"inspect", "input":{"path":"test"}});
        let events = decode(
            &mut Decoder::new(request.model.to_string(), scope(), ErrorSignals::NONE),
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
        let body = encode(&original, &Dialect::anthropic()).unwrap();
        assert_eq!(
            body["messages"][1]["content"],
            json!([native, redacted, tool])
        );
        assert_eq!(body["messages"][2]["content"][0]["tool_use_id"], "call_1");
        assert!(body.get("thinking").is_none()); // replay does not require enabling new thinking
        let assistant = |request: &ModelRequest| {
            encode(request, &Dialect::anthropic()).unwrap()["messages"][1]["content"].clone()
        };
        let mut foreign = original.clone();
        foreign.model = "different-model".parse().unwrap();
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
            [crate::provider::codec::common::delta(
                1,
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
    fn context_window_finish_is_the_context_window_error() {
        // The condition fails the attempt; the streamed output is not kept as a
        // cut, and a block the overflow left open does not hide it.
        for frames in [vec![stop(0), stop(1)], vec![json_delta(1, "{")]] {
            let mut decoder = started();
            decode(
                &mut decoder,
                vec![
                    block_start(0, json!({"type":"text","text":"partial"})),
                    block_start(1, tool_use("call")),
                ],
            );
            decode(&mut decoder, frames);
            let result: Result<Vec<_>, _> = [
                terminal("model_context_window_exceeded", 19),
                message_stop(),
            ]
            .into_iter()
            .map(|frame| decoder.decode(&event(frame)))
            .collect();
            assert_eq!(
                result.unwrap_err().kind(),
                ProviderErrorKind::ContextWindowExceeded
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
        let mut absent = Decoder::new("model".into(), scope(), ErrorSignals::NONE);
        assert!(decode(&mut absent, vec![json!({"type":"ping"})]).is_empty());
        assert!(absent.finish().is_err());
        let mut pinged = Decoder::new("model".into(), scope(), ErrorSignals::NONE);
        decode(&mut pinged, vec![json!({"type":"ping"}), start()]);
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
            // Counters as strings; a smaller late value or an unusable one does
            // not lower the total.
            json!({"type":"message_delta", "delta":{"stop_reason":"pause_for_vendor"},
                "usage":{"output_tokens":"5", "input_tokens":"3"}}),
            json!({"type":"message_delta", "delta":{},
                "usage":{"output_tokens":4, "input_tokens":"garbage"}}),
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
            // A later cut retracts tools, and so does a later overflow, without
            // failing the response.
            (
                vec![terminal("tool_use", 3), terminal("max_tokens", 4)],
                max_tokens,
                true,
            ),
            (
                vec![
                    terminal("tool_use", 3),
                    terminal("model_context_window_exceeded", 4),
                ],
                Some(Outcome::Cut(CutReason::Incomplete)),
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
        assert_eq!(error.kind(), ProviderErrorKind::Unavailable);
        assert_eq!(
            error.message,
            "provider stream error [code=overloaded_error]: Overloaded, retry with Bearer abc.def"
        );
        // Anthropic names overflow only in the message, which the codec's error
        // reader recognises.
        let error = started()
            .decode(&event(
                json!({"type":"error", "error":{"type":"invalid_request_error",
                "message":"prompt is too long: 213 tokens > 200 maximum"}}),
            ))
            .unwrap_err();
        assert_eq!(error.kind(), ProviderErrorKind::ContextWindowExceeded);
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
        let mut decoder = Decoder::new("model-a".into(), scope(), ErrorSignals::NONE);
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
