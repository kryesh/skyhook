//! Stateful lifecycle for a single Chat choice and stream.
mod content;
mod native;
mod usage;

use super::wire;
use crate::provider::{
    ProviderError,
    backends::transport::SseEvent,
    protocol::{ResponseChunk, StopReason, Usage},
};
use std::collections::{BTreeMap, BTreeSet};

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
    last_tool: Option<u64>,
    tools_discarded: bool,
    /// The server's response ID, from which missing call IDs are derived.
    response_id: Option<String>,
    /// Digest of the encoded request, distinguishing otherwise identical turns.
    request_digest: String,
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
            last_tool: None,
            tools_discarded: false,
            response_id: None,
            request_digest: String::new(),
            finish_reason: None,
            usage: Usage::default(),
            raw_prompt_tokens: 0,
            done: false,
            failed: false,
        }
    }

    /// Bind the decoder to the request it decodes the response of.
    pub(crate) fn for_request(mut self, body: &serde_json::Value) -> Self {
        use sha2::{Digest, Sha256};
        self.request_digest = hex(&Sha256::digest(body.to_string().as_bytes()));
        self
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
            return Ok(Vec::new());
        }
        let Some(wire::Chunk { id, choice, usage }) = native::decode(event)? else {
            let mut chunks = Vec::new();
            if self.finish_reason.is_none() {
                // Without a finish reason, tool calls are not provably complete.
                let stop = if self.tool_ids.is_empty() {
                    StopReason::EndTurn
                } else {
                    StopReason::Other("missing_finish_reason".into())
                };
                self.settle(stop, &mut chunks)?;
            }
            self.done = true;
            chunks.push(ResponseChunk::ResponseEnded {
                stop_reason: self.finish_reason.clone().expect("settled above"),
            });
            return Ok(chunks);
        };
        if self.response_id.is_none() {
            self.response_id = id;
        }
        let mut chunks = Vec::new();
        if let Some(mut choice) = choice {
            let message = choice.message.take().or_else(|| {
                choice.text.take().map(|text| wire::Delta {
                    content: Some(text),
                    ..Default::default()
                })
            });
            if let Some(message) = message
                && choice.delta.is_noop()
            {
                choice.delta = if self.blocks.is_empty() {
                    message
                } else {
                    self.unstreamed(message)
                };
            }
            if self.finish_reason.is_some() {
                // Generation has ended, but the stream may still carry metadata,
                // usage, or repeated empty choice envelopes. Output after the
                // end would be silently lost, so it remains an error.
                if !choice.delta.is_noop() {
                    return Err(ProviderError::protocol(
                        "Chat output received after finish_reason",
                    ));
                }
            } else {
                self.delta(&choice.delta, &mut chunks)?;
            }
            if let Some(reason) = choice
                .finish_reason
                .as_deref()
                .filter(|reason| !reason.is_empty())
            {
                let stop_reason = self.classify_finish(reason);
                if self.finish_reason.is_none() {
                    self.settle(stop_reason, &mut chunks)?;
                } else {
                    self.revise(stop_reason, &mut chunks);
                }
            }
        }
        if let Some(usage) = usage {
            self.update_usage(usage, &mut chunks);
        }
        // Keepalives do not complete a stream; EOF still requires a finish.
        Ok(chunks)
    }

    fn classify_finish(&self, reason: &str) -> StopReason {
        let has_tools = !self.tool_ids.is_empty();
        match reason.to_ascii_lowercase().as_str() {
            "stop" | "eos" | "end_turn" | "stop_sequence" | "tool_calls" | "function_call"
            | "tool_use"
                if has_tools =>
            {
                StopReason::ToolUse
            }
            "stop_sequence" => StopReason::StopSequence,
            "stop" | "eos" | "end_turn" | "tool_calls" | "function_call" | "tool_use" => {
                StopReason::EndTurn
            }
            "length" | "max_tokens" | "max_output_tokens" | "model_length" => StopReason::MaxTokens,
            "abort" | "aborted" | "cancelled" | "canceled" => StopReason::Aborted,
            "content_filter" | "safety" | "refusal" => StopReason::ContentFilter,
            _ => StopReason::Other(reason.into()),
        }
    }

    /// Settle the stop reason. Unknown or missing reasons discard tool calls.
    fn settle(
        &mut self,
        stop_reason: StopReason,
        chunks: &mut Vec<ResponseChunk>,
    ) -> Result<(), ProviderError> {
        let discard_tools = !stop_reason.authorizes_tools();
        self.end_blocks(chunks, discard_tools)?;
        self.tools_discarded = discard_tools;
        self.finish_reason = Some(stop_reason);
        Ok(())
    }

    /// A later abnormal reason retracts tool calls; nothing revives them.
    fn revise(&mut self, stop_reason: StopReason, chunks: &mut Vec<ResponseChunk>) {
        if stop_reason.authorizes_tools() || self.tools_discarded || self.tool_ids.is_empty() {
            return;
        }
        for (id, block) in self.blocks.iter().enumerate() {
            if matches!(block, Block::Tool { .. }) {
                chunks.push(ResponseChunk::ItemDiscarded { id: id.to_string() });
            }
        }
        self.tools_discarded = true;
        self.finish_reason = Some(stop_reason);
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

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::provider::protocol::{
        AssistantItem, BlockContent, ResponseAssembler, StopReason, Usage,
    };
    use serde_json::{Value, json};

    pub(super) fn event(value: Value) -> SseEvent {
        SseEvent {
            event: None,
            data: value.to_string(),
        }
    }

    pub(in crate::provider::backends::chat) fn delta(value: Value) -> SseEvent {
        event(json!({"choices":[{"index":0,"delta":value}]}))
    }

    pub(in crate::provider::backends::chat) fn end(reason: &str) -> SseEvent {
        event(json!({"choices":[{"index":0,"delta":{},"finish_reason":reason}]}))
    }

    pub(in crate::provider::backends::chat) fn decode(
        frames: Vec<SseEvent>,
    ) -> (Vec<AssistantItem>, Usage, StopReason) {
        let mut decoder = Decoder::new("test-model".into());
        let mut assembler = ResponseAssembler::default();
        for frame in frames {
            for chunk in decoder.decode(&frame).unwrap() {
                assembler.push(&chunk).unwrap();
            }
        }
        for chunk in decoder.finish().unwrap() {
            assembler.push(&chunk).unwrap();
        }
        assembler.finish().unwrap()
    }

    pub(super) fn done() -> SseEvent {
        SseEvent {
            event: None,
            data: "[DONE]".into(),
        }
    }

    pub(super) fn phantom_usage_chunk() -> Value {
        json!({
            "choices":[{"index":0,"delta":{}}],
            "usage":{
                "prompt_tokens":100,
                "completion_tokens":10,
                "total_tokens":110,
                "prompt_tokens_details":{"cached_tokens":80}
            }
        })
    }

    fn assert_post_finish_rejected(packet: Value) {
        let mut decoder = Decoder::new("test-model".into());
        decoder.decode(&delta(json!({"content":"answer"}))).unwrap();
        decoder.decode(&end("stop")).unwrap();
        assert!(decoder.decode(&event(packet.clone())).is_err(), "{packet}");
        assert!(decoder.finish().is_err(), "{packet}");
    }

    pub(super) fn noop_packets() -> Vec<Value> {
        let mut packets = vec![json!({}), json!({"choices":null}), json!({"choices":[]})];
        let deltas = [
            Value::Null,
            json!({}),
            json!({"content":null}),
            json!({"role":"assistant"}),
            json!({"role":null,"content":null,"refusal":null,"reasoning":null,"reasoning_content":null,"tool_calls":null}),
            json!({"content":"","refusal":"","reasoning":"","reasoning_content":"","tool_calls":[]}),
            json!({"role":"assistant","content":"","refusal":null,"reasoning":"","reasoning_content":null,"tool_calls":[],"audio":null,"function_call":null,"unknown":null}),
        ];
        for omit_index in [false, true] {
            for null_finish in [false, true] {
                let mut choice = json!({});
                if !omit_index {
                    choice["index"] = json!(0);
                }
                if null_finish {
                    choice["finish_reason"] = Value::Null;
                }
                packets.push(json!({"choices":[choice.clone()]}));
                for value in &deltas {
                    choice["delta"] = value.clone();
                    packets.push(json!({"choices":[choice.clone()]}));
                }
            }
        }
        packets
    }

    pub(super) const USAGE: Usage = Usage {
        input_tokens: 20,
        cached_input_tokens: 80,
        output_tokens: 10,
    };

    pub(super) fn contents(items: &[AssistantItem]) -> Vec<&BlockContent> {
        items.iter().map(|item| &item.blocks[0].content).collect()
    }

    pub(super) fn text(text: &str) -> BlockContent {
        BlockContent::Text { text: text.into() }
    }

    #[test]
    fn post_finish_output_is_rejected() {
        let mut packets = Vec::new();
        let mut with_delta = |field: &str, value: Value, finish: Option<&str>| {
            let mut packet = phantom_usage_chunk();
            packet["choices"][0]["delta"][field] = value;
            if let Some(finish) = finish {
                packet["choices"][0]["finish_reason"] = json!(finish);
            }
            packets.push(packet);
        };
        // Output after the finish would be silently lost, so it is rejected,
        // including on a repeated finish.
        for field in ["content", "refusal", "reasoning_content", "reasoning"] {
            with_delta(field, json!("late"), None);
            with_delta(field, json!([{"type":"text","text":"late"}]), None);
            with_delta(field, json!("late"), Some("stop"));
        }
        let call = json!([{"index":0,"id":"call","function":{"name":"inspect","arguments":"{}"}}]);
        with_delta("tool_calls", call.clone(), None);
        with_delta("tool_calls", call, Some("stop"));
        with_delta("function_call", json!({"name":"inspect"}), None);
        packets.into_iter().for_each(assert_post_finish_rejected);
    }

    #[test]
    fn post_finish_placeholders_and_conflicting_finishes_are_ignored() {
        let mut packets = Vec::new();
        for field in [
            "content",
            "refusal",
            "reasoning_content",
            "reasoning",
            "role",
        ] {
            for value in [json!(0), json!(false), json!({}), json!("")] {
                let mut packet = phantom_usage_chunk();
                packet["choices"][0]["delta"][field] = value;
                packets.push(packet);
            }
        }
        for finish in ["length", "abort", "content_filter", "tool_calls", ""] {
            let mut packet = phantom_usage_chunk();
            packet["choices"][0]["finish_reason"] = json!(finish);
            packets.push(packet);
        }
        for packet in packets {
            let (items, usage, stop) = decode(vec![
                delta(json!({"content":"answer"})),
                end("stop"),
                event(packet),
                done(),
            ]);
            assert_eq!((stop, usage), (StopReason::EndTurn, USAGE));
            assert_eq!(contents(&items), [&text("answer")]);
        }
    }

    #[test]
    fn noop_metadata_before_during_and_after_generation_preserves_output() {
        for mut packet in noop_packets() {
            // Envelope/choice metadata is ignored; unknown non-null delta output is not.
            packet["provider_metadata"] = json!({"stage":"heartbeat"});
            if let Some(choice) = packet["choices"].get_mut(0) {
                choice["logprobs"] = json!({"provider_extension":true});
            }
            for usage in [
                None,
                Some(Value::Null),
                Some(json!({
                    "prompt_tokens":100,"completion_tokens":10,
                    "prompt_tokens_details":{"cached_tokens":80,"extension":true},
                    "provider_extension":{"ignored":true}
                })),
            ] {
                let mut packet = packet.clone();
                if let Some(usage) = &usage {
                    packet["usage"] = usage.clone();
                }
                let expected_usage = match usage {
                    Some(usage) if !usage.is_null() => USAGE,
                    _ => Usage::default(),
                };
                for (finish, expected_stop) in [
                    ("stop", StopReason::EndTurn),
                    ("length", StopReason::MaxTokens),
                ] {
                    for with_done in [false, true] {
                        let mut frames = vec![
                            event(packet.clone()),
                            delta(json!({"reasoning_content":"think"})),
                            event(packet.clone()),
                            delta(json!({"content":"an"})),
                            event(packet.clone()),
                            delta(json!({"refusal":"swer"})),
                            end(finish),
                            event(packet.clone()),
                        ];
                        frames.extend(with_done.then(done));
                        let (items, actual_usage, stop) = decode(frames);
                        let reasoning = BlockContent::Reasoning {
                            text: "think".into(),
                        };
                        assert_eq!(contents(&items), [&reasoning, &text("answer")], "{packet}");
                        assert_eq!(
                            (stop, actual_usage),
                            (expected_stop.clone(), expected_usage)
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn noop_metadata_never_supplies_a_finish_or_ends_response_early() {
        for packet in noop_packets() {
            for with_content in [false, true] {
                for with_usage in [false, true] {
                    for with_done in [false, true] {
                        let mut decoder = Decoder::new("test-model".into());
                        if with_content {
                            decoder
                                .decode(&delta(json!({"content":"partial"})))
                                .unwrap();
                        }
                        let mut packet = packet.clone();
                        if with_usage {
                            packet["usage"] = phantom_usage_chunk()["usage"].clone();
                        }
                        let chunks = decoder.decode(&event(packet.clone())).unwrap();
                        let ended = |chunk: &ResponseChunk| {
                            matches!(chunk, ResponseChunk::ResponseEnded { .. })
                        };
                        assert!(!chunks.iter().any(ended), "{packet}");
                        if with_done {
                            let chunks = decoder.decode(&done()).unwrap();
                            assert!(chunks.iter().any(ended), "{packet}");
                            decoder.finish().unwrap();
                        } else {
                            assert!(decoder.finish().is_err(), "{packet}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn packets_after_done_are_ignored() {
        let mut packets = noop_packets();
        packets.push(phantom_usage_chunk());
        packets.push(json!({"choices":[{"finish_reason":"stop"}]}));
        for packet in packets {
            let mut decoder = Decoder::new("test-model".into());
            for frame in [delta(json!({"content":"answer"})), end("stop"), done()] {
                decoder.decode(&frame).unwrap();
            }
            assert!(decoder.decode(&event(packet.clone())).unwrap().is_empty());
            assert!(decoder.finish().unwrap().is_empty(), "{packet}");
        }
    }

    #[test]
    fn repeated_identical_finish_with_noop_delta_preserves_stop_and_usage() {
        for (finish, expected_stop) in [
            ("stop", StopReason::EndTurn),
            ("length", StopReason::MaxTokens),
            ("abort", StopReason::Aborted),
            ("content_filter", StopReason::ContentFilter),
        ] {
            let with_choice = |packet: &Value| {
                packet["choices"]
                    .as_array()
                    .is_some_and(|choices| !choices.is_empty())
            };
            for mut packet in noop_packets().into_iter().filter(with_choice) {
                packet["choices"][0]["finish_reason"] = json!(finish);
                for with_usage in [false, true] {
                    for with_done in [false, true] {
                        let mut repeated = packet.clone();
                        if with_usage {
                            repeated["usage"] = phantom_usage_chunk()["usage"].clone();
                        }
                        let mut frames = vec![
                            delta(json!({"content":"answer"})),
                            end(finish),
                            event(phantom_usage_chunk()),
                            event(repeated.clone()),
                            event(repeated),
                        ];
                        frames.extend(with_done.then(done));
                        let (items, usage, stop) = decode(frames);
                        assert_eq!(contents(&items), [&text("answer")]);
                        assert_eq!((stop, usage), (expected_stop.clone(), USAGE));
                    }
                }
            }
        }
    }

    #[test]
    fn missing_and_null_finish_delta_close_output_normally() {
        for mut packet in [json!({"choices":[{}]}), json!({"choices":[{"delta":null}]})] {
            packet["choices"][0]["finish_reason"] = json!("stop");
            let (items, _, stop) = decode(vec![delta(json!({"content":"answer"})), event(packet)]);
            assert_eq!(stop, StopReason::EndTurn);
            assert_eq!(contents(&items), [&text("answer")]);
        }
    }

    #[test]
    fn rejects_premature_eof_and_post_terminal_data() {
        assert!(Decoder::new("gpt-5".into()).finish().is_err());
        let mut decoder = Decoder::new("gpt-5".into());
        decoder
            .decode(&delta(json!({"content":"partial"})))
            .unwrap();
        assert!(decoder.finish().is_err());
        // [DONE] without a finish reason keeps text but discards tools.
        let (items, _, stop) = decode(vec![delta(json!({"content":"answer"})), done()]);
        assert_eq!(
            (stop, contents(&items)),
            (StopReason::EndTurn, vec![&text("answer")])
        );
        let tool = json!({"tool_calls":[{"index":0,"id":"c","function":{"name":"list","arguments":"{}"}}]});
        let (items, _, stop) = decode(vec![
            delta(json!({"content":"partial"})),
            delta(tool),
            done(),
        ]);
        assert_eq!(stop, StopReason::Other("missing_finish_reason".into()));
        assert_eq!(contents(&items), [&text("partial")]);
        let mut decoder = Decoder::new("gpt-5".into());
        decoder.decode(&end("stop")).unwrap();
        assert!(decoder.decode(&delta(json!({"content":"late"}))).is_err());
    }

    #[test]
    fn only_normal_finish_reasons_keep_tools_executable() {
        let tool = || {
            delta(
                json!({"tool_calls":[{"index":0,"id":"c","function":{"name":"run","arguments":"{}"}}]}),
            )
        };
        for reason in [
            "stop",
            "tool_calls",
            "STOP",
            "eos",
            "end_turn",
            "tool_use",
            "function_call",
        ] {
            let (items, _, stop) = decode(vec![tool(), end(reason)]);
            assert_eq!((stop, items.len()), (StopReason::ToolUse, 1), "{reason}");
        }
        for (reason, expected) in [
            ("length", StopReason::MaxTokens),
            ("MAX_TOKENS", StopReason::MaxTokens),
            ("aborted", StopReason::Aborted),
            ("canceled", StopReason::Aborted),
            ("SAFETY", StopReason::ContentFilter),
            ("error", StopReason::Other("error".into())),
            ("incomplete", StopReason::Other("incomplete".into())),
        ] {
            let (items, _, stop) =
                decode(vec![delta(json!({"content":"text"})), tool(), end(reason)]);
            assert_eq!(stop, expected, "{reason}");
            assert_eq!(contents(&items), [&text("text")], "{reason}");
        }
    }

    #[test]
    fn a_later_abnormal_finish_retracts_tools() {
        let tool = || {
            delta(
                json!({"tool_calls":[{"index":0,"id":"c","function":{"name":"run","arguments":"{}"}}]}),
            )
        };
        let (items, _, stop) = decode(vec![tool(), end("stop"), end("length"), done()]);
        assert_eq!((stop, items.len()), (StopReason::MaxTokens, 0));
        // A later normal reason cannot revive discarded tools.
        let (items, _, stop) = decode(vec![tool(), end("length"), end("stop"), done()]);
        assert_eq!((stop, items.len()), (StopReason::MaxTokens, 0));
    }
}
