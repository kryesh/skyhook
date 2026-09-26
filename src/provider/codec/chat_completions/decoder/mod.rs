//! Stateful lifecycle for a single Chat choice and stream.
mod content;
mod native;
mod usage;

use super::wire;
use crate::provider::{
    ProviderError,
    codec::{
        common::Finish,
        usage::{Counters, InputAccounting},
    },
    http::{errors::ErrorSignals, transport::SseEvent},
    protocol::{CutReason, ReplayFormat, ResponseEvent, Scope},
};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

#[derive(Clone)]
enum Block {
    Text(String),
    Reasoning(String),
    /// A native reasoning object retained whole for replay, with the text it
    /// carries so far.
    Native {
        text: String,
        object: Map<String, Value>,
        shape: NativeShape,
    },
    Tool {
        call_id: String,
        name: String,
        arguments: String,
    },
}

/// How a native reasoning object is continued and bound.
#[derive(Clone, Copy)]
enum NativeShape {
    /// A thinking block, open to further fragments until its signature (or
    /// redacted data) completes it; bound by its own signature.
    Thinking { complete: bool, signed: bool },
    /// One detail of a sequence, continued by fragments at its index; the
    /// sequence is bound whole when any detail is signed.
    Detail {
        index: Option<u64>,
        kind: wire::DetailKind,
        signed: bool,
    },
}

/// One choice, one stream. IDs identify logical blocks rather than wire indexes.
#[derive(Clone)]
pub(crate) struct Decoder {
    model: String,
    scope: Scope,
    /// The native reasoning field the dialect replays, or text.
    format: ReplayFormat,
    blocks: Vec<Block>,
    visible_id: Option<usize>,
    tool_ids: BTreeMap<u64, usize>,
    last_tool: Option<u64>,
    /// The server's response ID, from which missing call IDs are derived.
    response_id: Option<String>,
    /// Digest of the encoded request, distinguishing otherwise identical turns.
    request_digest: String,
    finish: Option<Finish>,
    usage: Counters,
    errors: ErrorSignals,
    done: bool,
}

impl Decoder {
    pub(crate) fn new(
        model: String,
        scope: Scope,
        format: ReplayFormat,
        errors: ErrorSignals,
    ) -> Self {
        Self {
            model,
            scope,
            format,
            errors,
            blocks: Vec::new(),
            visible_id: None,
            tool_ids: BTreeMap::new(),
            last_tool: None,
            response_id: None,
            request_digest: String::new(),
            finish: None,
            usage: Counters::new(InputAccounting::PromptTotal),
            done: false,
        }
    }

    /// Bind the decoder to the request it decodes the response of.
    pub(crate) fn for_request(mut self, body: &serde_json::Value) -> Self {
        self.request_digest = crate::sha256_hex(body.to_string());
        self
    }

    pub(crate) fn decode(&mut self, event: &SseEvent) -> Result<Vec<ResponseEvent>, ProviderError> {
        if self.done {
            return Ok(Vec::new());
        }
        let Some(wire::Chunk { id, choice, usage }) = native::decode(event, self.errors)? else {
            if self.finish.is_none() {
                // Without a finish reason, tool calls are not provably complete.
                let finish = if self.tool_ids.is_empty() {
                    Finish::Normal
                } else {
                    Finish::Cut(CutReason::Incomplete)
                };
                self.settle(finish)?;
            }
            return self.end().map(|end| vec![end]);
        };
        if self.response_id.is_none() {
            self.response_id = id;
        }
        let mut events = Vec::new();
        if let Some(mut choice) = choice {
            let message = choice.message.take().or_else(|| {
                choice.text.take().map(|text| wire::Delta {
                    content: Some(text),
                    ..Default::default()
                })
            });
            if let Some(message) = message
                && choice.delta.is_noop(self.format)
            {
                choice.delta = if self.blocks.is_empty() {
                    message
                } else {
                    self.unstreamed(message)
                };
            }
            if self.finish.is_some() {
                // Generation has ended, but the stream may still carry metadata,
                // usage, or repeated empty choice envelopes. Output after the
                // end would be silently lost, so it remains an error.
                if !choice.delta.is_noop(self.format) {
                    return Err(ProviderError::protocol(
                        "Chat output received after finish_reason",
                    ));
                }
            } else {
                self.delta(&choice.delta, &mut events)?;
            }
            if let Some(reason) = choice
                .finish_reason
                .as_deref()
                .filter(|reason| !reason.is_empty())
            {
                let finish = classify_finish(reason);
                if self.finish.is_none() {
                    self.settle(finish)?;
                } else {
                    self.revise(finish);
                }
            }
        }
        if let Some(usage) = usage {
            self.update_usage(usage, &mut events);
        }
        // Keepalives do not complete a stream; EOF still requires a finish.
        Ok(events)
    }

    /// Settle the finish. A normal finish must leave every tool call usable.
    fn settle(&mut self, finish: Finish) -> Result<(), ProviderError> {
        if finish == Finish::Normal {
            self.items(false)?;
        }
        self.finish = Some(finish);
        Ok(())
    }

    /// A later abnormal reason retracts tool calls; nothing revives them, and a
    /// response without calls keeps its settled finish.
    fn revise(&mut self, finish: Finish) {
        if finish == Finish::Normal
            || self.tool_ids.is_empty()
            || !matches!(self.finish, Some(Finish::Normal))
        {
            return;
        }
        self.finish = Some(finish);
    }

    /// The authoritative response, once the finish has settled.
    fn end(&mut self) -> Result<ResponseEvent, ProviderError> {
        let finish = self
            .finish
            .expect("the response ends only after its finish settled");
        let completion = finish.complete(self.items(finish != Finish::Normal)?)?;
        self.done = true;
        Ok(ResponseEvent::End(completion))
    }

    pub(crate) fn finish(&mut self) -> Result<Vec<ResponseEvent>, ProviderError> {
        if self.done {
            return Ok(Vec::new());
        }
        if self.finish.is_none() {
            return Err(ProviderError::protocol(
                "Chat stream ended before finish_reason",
            ));
        }
        self.end().map(|end| vec![end])
    }
}

fn classify_finish(reason: &str) -> Finish {
    match reason.to_ascii_lowercase().as_str() {
        "stop" | "eos" | "end_turn" | "stop_sequence" | "tool_calls" | "function_call"
        | "tool_use" => Finish::Normal,
        "length" | "max_tokens" | "max_output_tokens" | "model_length" => {
            Finish::Cut(CutReason::MaxTokens)
        }
        "abort" | "aborted" | "cancelled" | "canceled" => Finish::Cut(CutReason::Aborted),
        "content_filter" | "safety" | "refusal" => Finish::Cut(CutReason::Refusal),
        _ => Finish::Cut(CutReason::Incomplete),
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::provider::codec::common::tests::{Reduced, reduce, scope};
    use crate::provider::protocol::{AssistantItem, Outcome, ToolCall, Usage};
    use serde_json::{Value, json};

    pub(super) fn event(value: Value) -> SseEvent {
        SseEvent {
            event: None,
            data: value.to_string(),
        }
    }

    pub(in crate::provider::codec::chat_completions) fn delta(value: Value) -> SseEvent {
        event(json!({"choices":[{"index":0,"delta":value}]}))
    }

    pub(in crate::provider::codec::chat_completions) fn end(reason: &str) -> SseEvent {
        event(json!({"choices":[{"index":0,"delta":{},"finish_reason":reason}]}))
    }

    pub(super) fn decoder() -> Decoder {
        decoder_for(ReplayFormat::ChatText)
    }

    pub(super) fn decoder_for(format: ReplayFormat) -> Decoder {
        Decoder::new("test-model".into(), scope(), format, ErrorSignals::NONE)
    }

    /// Everything a consumer sees of `frames`, including the EOF finish.
    pub(in crate::provider::codec::chat_completions) fn reduced(frames: Vec<SseEvent>) -> Reduced {
        let mut decoder = decoder();
        let mut events = Vec::new();
        for frame in frames {
            events.extend(decoder.decode(&frame).unwrap());
        }
        events.extend(decoder.finish().unwrap());
        reduce(events)
    }

    pub(in crate::provider::codec::chat_completions) fn decode(
        frames: Vec<SseEvent>,
    ) -> (Vec<AssistantItem>, Usage, Outcome) {
        let reduced = reduced(frames);
        let completion = &reduced.completion;
        (
            completion.items().to_vec(),
            reduced.usage,
            completion.outcome(),
        )
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
        let mut decoder = decoder();
        decoder.decode(&delta(json!({"content":"answer"}))).unwrap();
        decoder.decode(&end("stop")).unwrap();
        assert!(decoder.decode(&event(packet.clone())).is_err(), "{packet}");
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
        cache_write_input_tokens: 0,
        output_tokens: 10,
    };

    pub(super) const MAX_TOKENS: Outcome = Outcome::Cut(CutReason::MaxTokens);

    /// The content of a single-block item, for order-sensitive comparisons.
    #[derive(Clone, Debug, PartialEq)]
    pub(super) enum Content {
        Text(String),
        Reasoning(String),
        Tool(ToolCall),
    }

    pub(super) fn contents(items: &[AssistantItem]) -> Vec<Content> {
        items
            .iter()
            .map(|item| match item {
                AssistantItem::Text { .. } => Content::Text(item.text_content().unwrap()),
                AssistantItem::Reasoning { .. } => {
                    Content::Reasoning(item.reasoning_text().unwrap())
                }
                AssistantItem::ToolCall { call, .. } => Content::Tool(call.clone()),
            })
            .collect()
    }

    pub(super) fn text(text: &str) -> Content {
        Content::Text(text.into())
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
    fn late_native_reasoning_is_output_only_where_the_dialect_replays_it() {
        for (field, value, native) in [
            (
                "thinking_blocks",
                json!([{"type":"thinking","thinking":"","signature":"sig"}]),
                ReplayFormat::ChatThinkingBlock,
            ),
            (
                "reasoning_details",
                json!([{"type":"reasoning.encrypted","data":"opaque"}]),
                ReplayFormat::ChatReasoningDetail,
            ),
        ] {
            let mut packet = phantom_usage_chunk();
            packet["choices"][0]["delta"][field] = value;
            for (format, rejected) in [(ReplayFormat::ChatText, false), (native, true)] {
                let mut decoder = decoder_for(format);
                decoder.decode(&delta(json!({"content":"answer"}))).unwrap();
                decoder.decode(&end("stop")).unwrap();
                let late = decoder.decode(&event(packet.clone()));
                assert_eq!(late.is_err(), rejected, "{packet}");
            }
        }
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
            let (items, usage, outcome) = decode(vec![
                delta(json!({"content":"answer"})),
                end("stop"),
                event(packet),
                done(),
            ]);
            assert_eq!((outcome, usage), (Outcome::Answer, USAGE));
            assert_eq!(contents(&items), [text("answer")]);
        }
    }

    #[test]
    fn noop_metadata_is_ignored_at_every_stage_and_never_finishes_a_response() {
        let ended = |event: &ResponseEvent| matches!(event, ResponseEvent::End(_));
        for mut packet in noop_packets() {
            // Envelope/choice metadata is ignored; unknown non-null delta output is not.
            packet["provider_metadata"] = json!({"stage":"heartbeat"});
            if let Some(choice) = packet["choices"].get_mut(0) {
                choice["logprobs"] = json!({"provider_extension":true});
            }
            let mut with_usage = packet.clone();
            with_usage["usage"] = phantom_usage_chunk()["usage"].clone();
            let mut null_usage = packet.clone();
            null_usage["usage"] = Value::Null;

            // Before, during and after generation, with and without [DONE].
            for (packet, expected_usage, with_done) in [
                (&packet, Usage::default(), false),
                (&null_usage, Usage::default(), true),
                (&with_usage, USAGE, true),
            ] {
                let mut frames = vec![
                    event(packet.clone()),
                    delta(json!({"reasoning_content":"think"})),
                    event(packet.clone()),
                    delta(json!({"content":"an"})),
                    delta(json!({"refusal":"swer"})),
                    end("length"),
                    event(packet.clone()),
                ];
                frames.extend(with_done.then(done));
                let (items, usage, outcome) = decode(frames);
                let reasoning = Content::Reasoning("think".into());
                assert_eq!(contents(&items), [reasoning, text("answer")], "{packet}");
                assert_eq!((outcome, usage), (MAX_TOKENS, expected_usage));
            }

            // Metadata never supplies a finish reason; only [DONE] ends the response.
            for packet in [&packet, &with_usage] {
                let mut decoder = decoder();
                let partial = delta(json!({"content":"partial"}));
                decoder.decode(&partial).unwrap();
                let events = decoder.decode(&event(packet.clone())).unwrap();
                assert!(!events.iter().any(ended), "{packet}");
                assert!(decoder.clone().finish().is_err(), "{packet}");
                assert!(decoder.decode(&done()).unwrap().iter().any(ended));
                // Packets after [DONE] are ignored.
                assert!(decoder.decode(&event(packet.clone())).unwrap().is_empty());
                assert!(decoder.finish().unwrap().is_empty(), "{packet}");
            }

            // A repeated identical finish on a no-op delta keeps the outcome and usage.
            if packet["choices"].get(0).is_some() {
                for (finish, expected) in [
                    ("stop", Outcome::Answer),
                    ("abort", Outcome::Cut(CutReason::Aborted)),
                    ("content_filter", Outcome::Cut(CutReason::Refusal)),
                ] {
                    let mut repeated = packet.clone();
                    repeated["choices"][0]["finish_reason"] = json!(finish);
                    let (items, usage, outcome) = decode(vec![
                        delta(json!({"content":"answer"})),
                        end(finish),
                        event(phantom_usage_chunk()),
                        event(repeated.clone()),
                        event(repeated),
                    ]);
                    assert_eq!(contents(&items), [text("answer")]);
                    assert_eq!((outcome, usage), (expected, USAGE));
                }
            }
        }
        // A finish or usage after [DONE] is ignored too.
        for packet in [
            phantom_usage_chunk(),
            json!({"choices":[{"finish_reason":"stop"}]}),
        ] {
            let mut decoder = decoder();
            for frame in [delta(json!({"content":"answer"})), end("stop"), done()] {
                decoder.decode(&frame).unwrap();
            }
            assert!(decoder.decode(&event(packet)).unwrap().is_empty());
        }
    }

    #[test]
    fn missing_and_null_finish_delta_close_output_normally() {
        for mut packet in [json!({"choices":[{}]}), json!({"choices":[{"delta":null}]})] {
            packet["choices"][0]["finish_reason"] = json!("stop");
            let (items, _, outcome) =
                decode(vec![delta(json!({"content":"answer"})), event(packet)]);
            assert_eq!(outcome, Outcome::Answer);
            assert_eq!(contents(&items), [text("answer")]);
        }
    }

    #[test]
    fn rejects_premature_eof_and_post_terminal_data() {
        assert!(decoder().finish().is_err());
        let mut decoder = decoder();
        decoder
            .decode(&delta(json!({"content":"partial"})))
            .unwrap();
        assert!(decoder.finish().is_err());
        // [DONE] without a finish reason keeps text but discards tools.
        let (items, _, outcome) = decode(vec![delta(json!({"content":"answer"})), done()]);
        assert_eq!(
            (outcome, contents(&items)),
            (Outcome::Answer, vec![text("answer")])
        );
        let tool = json!({"tool_calls":[{"index":0,"id":"c","function":{"name":"list","arguments":"{}"}}]});
        let (items, _, outcome) = decode(vec![
            delta(json!({"content":"partial"})),
            delta(tool),
            done(),
        ]);
        assert_eq!(outcome, Outcome::Cut(CutReason::Incomplete));
        assert_eq!(contents(&items), [text("partial")]);
        let mut decoder = self::decoder();
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
            let (items, _, outcome) = decode(vec![tool(), end(reason)]);
            assert_eq!((outcome, items.len()), (Outcome::ToolUse, 1), "{reason}");
        }
        // A cut discards the call whatever its arguments look like, once,
        // even when the finish is repeated after usage.
        for (reason, expected) in [
            ("length", CutReason::MaxTokens),
            ("MAX_TOKENS", CutReason::MaxTokens),
            ("aborted", CutReason::Aborted),
            ("canceled", CutReason::Aborted),
            ("SAFETY", CutReason::Refusal),
            ("content_filter", CutReason::Refusal),
            ("error", CutReason::Incomplete),
            ("incomplete", CutReason::Incomplete),
        ] {
            for arguments in ["{", "{}", "not json", "[]"] {
                for repeated in [false, true] {
                    let call = json!([{"index":0,"id":"c","function":{"name":"run","arguments":arguments}}]);
                    let mut frames = vec![
                        delta(json!({"content":"text","tool_calls":call})),
                        end(reason),
                    ];
                    if repeated {
                        frames.extend([
                            event(json!({"choices":[{"finish_reason":reason,"delta":{"role":"assistant","tool_calls":[]}}]})),
                            event(phantom_usage_chunk()),
                            event(json!({"choices":[{"finish_reason":reason}]})),
                            done(),
                        ]);
                    }
                    let (items, usage, outcome) = decode(frames);
                    let expected_usage = if repeated { USAGE } else { Usage::default() };
                    assert_eq!((outcome, usage), (Outcome::Cut(expected), expected_usage));
                    assert_eq!(contents(&items), [text("text")], "{reason}");
                }
            }
        }
    }

    #[test]
    fn a_later_abnormal_finish_retracts_tools() {
        let tool = || {
            delta(
                json!({"tool_calls":[{"index":0,"id":"c","function":{"name":"run","arguments":"{}"}}]}),
            )
        };
        let (items, _, outcome) = decode(vec![tool(), end("stop"), end("length"), done()]);
        assert_eq!((outcome, items.len()), (MAX_TOKENS, 0));
        // A later normal reason cannot revive discarded tools.
        let (items, _, outcome) = decode(vec![tool(), end("length"), end("stop"), done()]);
        assert_eq!((outcome, items.len()), (MAX_TOKENS, 0));
    }
}
