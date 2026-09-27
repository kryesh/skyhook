//! Stateful lifecycle for a single Chat choice and stream.
mod content;
mod native;
mod reasoning;
mod tools;

use super::{NATIVE, ReasoningFormat, wire};
use crate::media::BlobDigest;
use crate::provider::{
    ProviderError,
    codec::{
        common::{Finish, Settle, Settlement, StopReason},
        usage::{Counters, InputAccounting},
    },
    http::{errors::ErrorSignals, transport::SseEvent},
    protocol::{Completion, ResponseEvent, Scope},
};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

enum Block {
    Text(String),
    Reasoning(String),
    /// A native reasoning object retained whole for replay, its text field
    /// holding the text so far.
    Native {
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
    /// A thinking block, open to further fragments until its signature
    /// completes it; bound by its own signature.
    Thinking { complete: bool, signed: bool },
    /// Redacted thinking, whole on arrival and without readable text; bound
    /// by its data.
    Redacted { signed: bool },
    /// One detail of a sequence, continued by fragments at its index; the
    /// sequence is bound whole when any detail is signed.
    Detail {
        index: Option<u64>,
        kind: wire::DetailKind,
        signed: bool,
    },
}

impl NativeShape {
    fn text_field(self) -> Option<&'static str> {
        match self {
            Self::Thinking { .. } => Some("thinking"),
            Self::Redacted { .. } => None,
            Self::Detail { kind, .. } => Some(kind.text_field()),
        }
    }

    fn text(self, object: &Map<String, Value>) -> &str {
        self.text_field()
            .and_then(|field| object.get(field))
            .and_then(Value::as_str)
            .unwrap_or_default()
    }
}

/// One choice, one stream. IDs identify logical blocks rather than wire indexes.
pub(crate) struct Decoder {
    model: String,
    scope: Scope,
    format: ReasoningFormat,
    blocks: Vec<Block>,
    visible_id: Option<usize>,
    tool_ids: BTreeMap<u64, usize>,
    last_tool: Option<u64>,
    /// The server's response ID, from which missing call IDs are derived.
    response_id: Option<String>,
    /// Digest of the encoded request, distinguishing otherwise identical turns.
    request: BlobDigest,
    settlement: Settlement,
    usage: Counters,
    errors: ErrorSignals,
}

impl Decoder {
    pub(crate) fn new(
        model: String,
        scope: Scope,
        format: ReasoningFormat,
        errors: ErrorSignals,
        request: BlobDigest,
    ) -> Self {
        Self {
            model,
            scope,
            format,
            errors,
            request,
            blocks: Vec::new(),
            visible_id: None,
            tool_ids: BTreeMap::new(),
            last_tool: None,
            response_id: None,
            settlement: Settlement::Open,
            usage: Counters::new(InputAccounting::PromptTotal),
        }
    }

    pub(crate) fn decode(&mut self, event: &SseEvent) -> Result<Vec<ResponseEvent>, ProviderError> {
        if self.settlement == Settlement::Ended {
            return Ok(Vec::new());
        }
        let Some(wire::Chunk { id, choice, usage }) = native::decode(event, self.errors)? else {
            return self.close().map(|end| vec![end]);
        };
        if self.response_id.is_none() {
            self.response_id = id;
        }
        let mut events = Vec::new();
        if let Some(choice) = choice {
            let delta = if choice.delta.is_noop(self.format) {
                let message = choice.message.or_else(|| {
                    choice.text.map(|text| wire::Delta {
                        content: Some(text),
                        ..Default::default()
                    })
                });
                message
                    .map(|message| {
                        if self.blocks.is_empty() {
                            message
                        } else {
                            self.unstreamed(message)
                        }
                    })
                    .filter(|delta| !delta.is_noop(self.format))
            } else {
                Some(choice.delta)
            };
            if let Some(delta) = delta {
                // After the finish, metadata, usage and empty envelopes may still
                // arrive, but output would be silently lost.
                if self.settlement != Settlement::Open {
                    return Err(NATIVE.error("output received after finish_reason"));
                }
                self.delta(&delta, &mut events);
            }
            if let Some(reason) = choice
                .finish_reason
                .as_deref()
                .filter(|reason| !reason.is_empty())
            {
                self.reason(Finish::of(StopReason::read(reason)))?;
            }
        }
        // Keepalives do not complete a stream; EOF still requires a finish.
        if let Some(usage) = usage {
            events.push(ResponseEvent::Usage(self.usage.observe(usage)));
        }
        Ok(events)
    }

    pub(crate) fn finish(&mut self) -> Result<Vec<ResponseEvent>, ProviderError> {
        self.eof(|| NATIVE.error("stream ended before finish_reason"))
    }
}

impl Settle for Decoder {
    fn settlement(&mut self) -> &mut Settlement {
        &mut self.settlement
    }

    fn has_tools(&self) -> bool {
        !self.tool_ids.is_empty()
    }

    /// A normal finish must leave every tool call usable.
    fn admit(&self, finish: Finish) -> Result<(), ProviderError> {
        if finish == Finish::Normal {
            for block in &self.blocks {
                if let Block::Tool {
                    name, arguments, ..
                } = block
                {
                    tools::tool_arguments(name, arguments)?;
                }
            }
        }
        Ok(())
    }

    fn completion(&mut self, finish: Finish) -> Result<Completion, ProviderError> {
        finish.complete(self.items(finish != Finish::Normal)?)
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::provider::ProviderErrorKind;
    use crate::provider::codec::common::tests::{Reduced, reduce, scope};
    use crate::provider::protocol::{AssistantItem, CutReason, Outcome, ToolCall, Usage};
    use serde_json::{Value, json};

    pub(super) fn raw(data: &str) -> SseEvent {
        SseEvent {
            event: None,
            data: data.into(),
        }
    }

    pub(super) fn event(value: Value) -> SseEvent {
        raw(&value.to_string())
    }

    pub(in crate::provider::codec::chat_completions) fn delta(value: Value) -> SseEvent {
        event(json!({"choices":[{"index":0,"delta":value}]}))
    }

    pub(in crate::provider::codec::chat_completions) fn end(reason: &str) -> SseEvent {
        event(json!({"choices":[{"index":0,"delta":{},"finish_reason":reason}]}))
    }

    pub(super) fn decoder() -> Decoder {
        decoder_for(ReasoningFormat::Text)
    }

    pub(super) fn decoder_for(format: ReasoningFormat) -> Decoder {
        let (model, request) = ("test-model".into(), BlobDigest::of(b""));
        Decoder::new(model, scope(), format, ErrorSignals::NONE, request)
    }

    /// Everything a consumer sees of `frames`, including the EOF finish.
    pub(in crate::provider::codec::chat_completions) fn reduced_as(
        format: ReasoningFormat,
        frames: Vec<SseEvent>,
    ) -> Reduced {
        let mut decoder = decoder_for(format);
        let mut events = Vec::new();
        for frame in &frames {
            events.extend(decoder.decode(frame).unwrap());
        }
        events.extend(decoder.finish().unwrap());
        reduce(events)
    }

    pub(super) fn decode(frames: Vec<SseEvent>) -> (Vec<AssistantItem>, Usage, Outcome) {
        let reduced = reduced_as(ReasoningFormat::Text, frames);
        let completion = &reduced.completion;
        (
            completion.items().to_vec(),
            reduced.usage,
            completion.outcome(),
        )
    }

    pub(super) fn done() -> SseEvent {
        raw("[DONE]")
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

    pub(super) fn tool_delta(calls: Value) -> SseEvent {
        delta(json!({ "tool_calls": calls }))
    }

    pub(super) fn reasoning(text: &str) -> Content {
        Content::Reasoning(text.into())
    }

    pub(super) fn tool(id: &str, name: &str, arguments: Value) -> Content {
        Content::Tool(ToolCall::new(id, name, arguments).unwrap())
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
                ReasoningFormat::ThinkingBlocks,
            ),
            (
                "reasoning_details",
                json!([{"type":"reasoning.encrypted","data":"opaque"}]),
                ReasoningFormat::Details,
            ),
        ] {
            let mut packet = phantom_usage_chunk();
            packet["choices"][0]["delta"][field] = value;
            for (format, rejected) in [(ReasoningFormat::Text, false), (native, true)] {
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
                assert!(decoder.finish().is_err(), "{packet}");
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
    fn a_context_window_finish_is_the_context_window_error() {
        let mut decoder = decoder();
        decoder
            .decode(&delta(json!({"content":"partial"})))
            .unwrap();
        decoder
            .decode(&end("model_context_window_exceeded"))
            .unwrap();
        let error = decoder.finish().unwrap_err();
        assert_eq!(error.kind(), ProviderErrorKind::ContextWindowExceeded);
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
        // A late overflow retracts them without failing the response.
        let late = end("model_context_window_exceeded");
        let (items, _, outcome) = decode(vec![tool(), end("stop"), late, done()]);
        let incomplete = Outcome::Cut(CutReason::Incomplete);
        assert_eq!((outcome, items.len()), (incomplete, 0));
        // A later normal reason cannot revive discarded tools.
        let (items, _, outcome) = decode(vec![tool(), end("length"), end("stop"), done()]);
        assert_eq!((outcome, items.len()), (MAX_TOKENS, 0));
    }

    #[test]
    fn late_cache_refinement_and_null_cache_details_preserve_totals() {
        let packet = |details: Value, output| {
            event(json!({"choices":[],"usage":{
                "prompt_tokens":100,"completion_tokens":output,"prompt_tokens_details":details
            }}))
        };
        let (_, usage, _) = decode(vec![
            delta(json!({"content":"answer"})),
            event(json!({"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":1}})),
            packet(json!({"cached_tokens":null}), 2),
            packet(json!({"cached_tokens":80, "cache_write_tokens":5}), 3),
            packet(Value::Null, 4),
            packet(json!({"cached_tokens":null}), 5),
            end("stop"),
        ]);
        let expected = Usage {
            output_tokens: 5,
            cache_write_input_tokens: 5,
            ..USAGE
        };
        assert_eq!(usage, expected);
    }

    #[test]
    fn loose_usage_is_merged_monotonically_at_every_stage() {
        for usage in [
            // Mismatched totals, string counters, and alternate key names.
            json!({"prompt_tokens":100,"completion_tokens":10,"total_tokens":1}),
            json!({"prompt_tokens":"100","completion_tokens":10.0}),
            json!({"input_tokens":100,"output_tokens":10,"cache_read_input_tokens":80}),
            // Regressions and missing or unusable counters keep the previous totals.
            json!({"prompt_tokens":99,"completion_tokens":9,"prompt_tokens_details":{"cached_tokens":79}}),
            json!({"prompt_tokens":100}),
            json!({"completion_tokens":-1, "prompt_tokens":100}),
            json!({"prompt_tokens":"garbage","completion_tokens":10}),
        ] {
            for stage in 0..3 {
                let mut decoder = decoder();
                let mut packet = phantom_usage_chunk();
                packet["choices"] = json!([]);
                decoder.decode(&event(packet.clone())).unwrap();
                if stage > 0 {
                    decoder.decode(&delta(json!({"content":"answer"}))).unwrap();
                    decoder.decode(&end("length")).unwrap();
                    packet["choices"] = json!([{"index":0,"delta":{}}]);
                }
                if stage == 2 {
                    packet["choices"][0]["finish_reason"] = json!("length");
                }
                packet["usage"] = usage.clone();
                decoder.decode(&event(packet)).unwrap();
                assert_eq!(decoder.usage.usage(), USAGE, "stage {stage}: {usage}");
            }
        }
        // Cached tokens never exceed the prompt.
        let mut decoder = decoder();
        let packet = json!({"usage":{"prompt_tokens":10,"completion_tokens":1,
            "prompt_tokens_details":{"cached_tokens":50}}});
        decoder.decode(&event(packet)).unwrap();
        assert_eq!(decoder.usage.usage().cached_input_tokens, 10);
        assert_eq!(decoder.usage.usage().input_tokens, 0);
    }

    #[test]
    fn repeated_finish_can_refine_usage() {
        let packet = |completion, cached: Value| {
            event(json!({
                "choices":[{"finish_reason":"length"}],
                "usage":{"prompt_tokens":100,"completion_tokens":completion,
                    "prompt_tokens_details":{"cached_tokens":cached}}
            }))
        };
        let (_, usage, outcome) = decode(vec![
            delta(json!({"content":"partial"})),
            end("length"),
            packet(9, Value::Null),
            packet(10, json!(80)),
            event(json!({"choices":[{"finish_reason":"length","delta":null}]})),
        ]);
        assert_eq!((outcome, usage), (MAX_TOKENS, USAGE));
    }
}
