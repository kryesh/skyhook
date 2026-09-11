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
        let Some(wire::Chunk { choices, usage, .. }) = native::decode(event)? else {
            let stop_reason = self
                .finish_reason
                .clone()
                .ok_or_else(|| ProviderError::protocol("Chat [DONE] before finish_reason"))?;
            self.done = true;
            return Ok(vec![ResponseChunk::ResponseEnded { stop_reason }]);
        };
        let mut chunks = Vec::new();
        if let Some(choice) = choices.first() {
            if self.finish_reason.is_some() {
                // Generation has ended, but the stream may still carry metadata,
                // usage, or repeated empty choice envelopes. Ignore only deltas
                // that carry no output, independently of the provider or usage.
                if !choice.delta.is_noop() {
                    return Err(ProviderError::protocol(
                        "Chat output received after finish_reason",
                    ));
                }
            } else {
                self.delta(&choice.delta, &mut chunks)?;
            }
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
                if let Some(previous) = &self.finish_reason {
                    if previous != &stop_reason {
                        return Err(ProviderError::protocol("Conflicting Chat finish_reason"));
                    }
                } else {
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
        }
        if let Some(usage) = usage {
            self.update_usage(usage, &mut chunks)?;
        }
        // Keepalives do not complete a stream; EOF still requires finish_reason.
        Ok(chunks)
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

    pub(super) fn assert_post_finish_rejected(packet: Value) {
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

    #[test]
    fn post_finish_rejects_substantive_and_malformed_known_delta_fields() {
        for field in [
            "role",
            "content",
            "refusal",
            "reasoning_content",
            "reasoning",
        ] {
            let values = if field == "role" {
                vec![
                    json!(""),
                    json!("user"),
                    json!("system"),
                    json!(false),
                    json!({}),
                ]
            } else {
                vec![json!("late"), json!(0), json!(false), json!([]), json!({})]
            };
            for value in values {
                let mut packet = phantom_usage_chunk();
                packet["choices"][0]["delta"][field] = value;
                assert_post_finish_rejected(packet);
            }
        }
        for calls in [
            json!({}),
            json!(""),
            json!([{}]),
            json!([{"index":0}]),
            json!([{"index":0,"id":"call","function":{"name":"inspect","arguments":"{}"}}]),
        ] {
            let mut packet = phantom_usage_chunk();
            packet["choices"][0]["delta"]["tool_calls"] = calls;
            assert_post_finish_rejected(packet);
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
                        if with_done {
                            frames.push(done());
                        }
                        let (items, actual_usage, stop) = decode(frames);
                        assert_eq!(stop, expected_stop, "{packet}");
                        assert_eq!(items.len(), 2, "{packet}");
                        assert_eq!(
                            items[0].blocks[0].content,
                            BlockContent::Reasoning {
                                text: "think".into()
                            }
                        );
                        assert_eq!(
                            items[1].blocks[0].content,
                            BlockContent::Text {
                                text: "answer".into()
                            }
                        );
                        assert_eq!(
                            actual_usage,
                            if usage.as_ref().is_some_and(|usage| !usage.is_null()) {
                                Usage {
                                    input_tokens: 20,
                                    cached_input_tokens: 80,
                                    output_tokens: 10,
                                }
                            } else {
                                Usage::default()
                            }
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
                        assert!(
                            !chunks
                                .iter()
                                .any(|chunk| matches!(chunk, ResponseChunk::ResponseEnded { .. })),
                            "{packet}"
                        );
                        if with_done {
                            assert!(decoder.decode(&done()).is_err(), "{packet}");
                        }
                        assert!(decoder.finish().is_err(), "{packet}");
                    }
                }
            }
        }
    }

    #[test]
    fn noop_metadata_and_repeated_finish_cannot_cross_done_boundary() {
        let mut packets = noop_packets();
        packets.push(phantom_usage_chunk());
        packets.push(json!({"choices":[{"finish_reason":"stop"}]}));
        for packet in packets {
            let mut decoder = Decoder::new("test-model".into());
            decoder.decode(&delta(json!({"content":"answer"}))).unwrap();
            decoder.decode(&end("stop")).unwrap();
            decoder.decode(&done()).unwrap();
            assert!(decoder.decode(&event(packet.clone())).is_err(), "{packet}");
            assert!(decoder.finish().is_err(), "{packet}");
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
            for mut packet in noop_packets().into_iter().filter(|packet| {
                packet["choices"]
                    .as_array()
                    .is_some_and(|choices| !choices.is_empty())
            }) {
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
                        if with_done {
                            frames.push(done());
                        }
                        let (items, usage, stop) = decode(frames);
                        assert_eq!(stop, expected_stop);
                        assert_eq!(items.len(), 1);
                        assert_eq!(
                            items[0].blocks[0].content,
                            BlockContent::Text {
                                text: "answer".into()
                            }
                        );
                        assert_eq!(
                            usage,
                            Usage {
                                input_tokens: 20,
                                cached_input_tokens: 80,
                                output_tokens: 10
                            }
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn repeated_finish_rejects_conflicts_and_substantive_deltas() {
        for finish in [
            "length",
            "abort",
            "content_filter",
            "tool_calls",
            "function_call",
            "",
        ] {
            let mut packet = phantom_usage_chunk();
            packet["choices"][0]["finish_reason"] = json!(finish);
            assert_post_finish_rejected(packet);
        }
        for value in [
            json!({"content":"late"}),
            json!({"refusal":"late"}),
            json!({"reasoning":"late"}),
            json!({"reasoning_content":"late"}),
            json!({"tool_calls":[{"index":0,"id":"call","function":{"name":"inspect","arguments":"{}"}}]}),
        ] {
            let mut packet = phantom_usage_chunk();
            packet["choices"][0]["finish_reason"] = json!("stop");
            packet["choices"][0]["delta"] = value;
            assert_post_finish_rejected(packet);
        }
    }

    #[test]
    fn missing_and_null_finish_delta_close_output_normally() {
        for mut packet in [json!({"choices":[{}]}), json!({"choices":[{"delta":null}]})] {
            packet["choices"][0]["finish_reason"] = json!("stop");
            let (items, _, stop) = decode(vec![delta(json!({"content":"answer"})), event(packet)]);
            assert_eq!(stop, StopReason::EndTurn);
            assert_eq!(items.len(), 1);
            assert_eq!(
                items[0].blocks[0].content,
                BlockContent::Text {
                    text: "answer".into()
                }
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
    }
}
