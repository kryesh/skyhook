use super::*;
use crate::provider::protocol::{AssistantItem, ResponseAssembler};

fn event(value: Value) -> SseEvent {
    SseEvent {
        event: None,
        data: value.to_string(),
    }
}
fn delta(value: Value) -> SseEvent {
    event(json!({"choices":[{"index":0,"delta":value}]}))
}
fn end(reason: &str) -> SseEvent {
    event(json!({"choices":[{"index":0,"delta":{},"finish_reason":reason}]}))
}
fn decode(frames: Vec<SseEvent>) -> (Vec<AssistantItem>, Usage, StopReason) {
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
fn request(items: Vec<AssistantItem>) -> ModelRequest {
    ModelRequest {
        model: "test-model".into(),
        system: vec![],
        messages: vec![Message::Assistant(items)],
        tools: vec![],
        response_schema: None,
        reasoning: None,
        max_output_tokens: None,
        correlation: None,
    }
}

#[test]
fn singleton_absent_index_llama_loading_is_reasoning() {
    let (items, _, _) = decode(vec![
        event(json!({"choices":[{"delta":{"reasoning_content":"loading"}}]})),
        end("stop"),
    ]);
    assert_eq!(
        items[0].blocks[0].content,
        BlockContent::Reasoning {
            text: "loading".into()
        }
    );
}

#[test]
fn invalid_choice_indices_and_multiple_choices_are_not_normalized() {
    for index in [
        Value::Null,
        json!("0"),
        json!(1),
        json!(-1),
        json!(0.5),
        json!(true),
    ] {
        let mut decoder = Decoder::new("test-model".into());
        assert!(
            decoder
                .decode(&event(json!({"choices":[{"index":index,"delta":{}}]})))
                .is_err(),
            "{index}"
        );
    }
    for choices in [
        json!([{"delta":{}},{"delta":{}}]),
        json!([{"index":0,"delta":{}},{"index":0,"delta":{}}]),
    ] {
        let mut decoder = Decoder::new("test-model".into());
        assert!(decoder.decode(&event(json!({"choices":choices}))).is_err());
    }
}

#[test]
fn hermes_repeated_index_header_and_arguments_are_sequential() {
    let (items, _, reason) = decode(vec![
        delta(json!({"tool_calls":[
            {"index":0,"id":"call-a","type":"function","function":{"name":"inspect","arguments":""}},
            {"index":0,"function":{"arguments":"{\"path\":"}},
            {"index":1,"id":"call-b","function":{"name":"other","arguments":"{}"}},
            {"index":0,"function":{"arguments":"\"file\"}"}}
        ]})),
        end("tool_calls"),
    ]);
    assert_eq!(reason, StopReason::ToolUse);
    assert_eq!(items.len(), 2);
    assert_eq!(
        items[0].blocks[0].content,
        BlockContent::ToolCall(ToolCall {
            id: "call-a".into(),
            name: "inspect".into(),
            arguments: json!({"path":"file"}),
        })
    );
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
        packet(json!({"cached_tokens":80}), 3),
        packet(Value::Null, 4),
        packet(json!({"cached_tokens":null}), 5),
        end("stop"),
    ]);
    assert_eq!(
        usage,
        Usage {
            input_tokens: 20,
            cached_input_tokens: 80,
            output_tokens: 5
        }
    );
}

#[test]
fn invalid_usage_and_actual_counter_regressions_fail() {
    for usage in [
        json!({"prompt_tokens":99,"completion_tokens":10,"prompt_tokens_details":{"cached_tokens":80}}),
        json!({"prompt_tokens":100,"completion_tokens":9,"prompt_tokens_details":{"cached_tokens":80}}),
        json!({"prompt_tokens":100,"completion_tokens":10,"prompt_tokens_details":{"cached_tokens":79}}),
        json!({"prompt_tokens":100,"completion_tokens":10,"prompt_tokens_details":{"cached_tokens":101}}),
        json!({"prompt_tokens":100,"completion_tokens":10,"total_tokens":109}),
        json!({"prompt_tokens":"100","completion_tokens":10}),
        json!({"prompt_tokens":100,"completion_tokens":10,"prompt_tokens_details":{"cached_tokens":-1}}),
    ] {
        let mut decoder = Decoder::new("test-model".into());
        decoder.decode(&event(json!({"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":10,"prompt_tokens_details":{"cached_tokens":80}}}))).unwrap();
        assert!(
            decoder
                .decode(&event(json!({"choices":[],"usage":usage})))
                .is_err(),
            "{usage}"
        );
    }
}

#[test]
fn refusal_is_visible_completed_output() {
    let (items, _, reason) = decode(vec![
        delta(json!({"refusal":"I cannot "})),
        delta(json!({"refusal":"help with that."})),
        end("stop"),
    ]);
    assert_eq!(reason, StopReason::EndTurn);
    assert_eq!(
        items[0].blocks[0].content,
        BlockContent::Text {
            text: "I cannot help with that.".into()
        }
    );
}

fn phantom_usage_chunk() -> Value {
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

fn done() -> SseEvent {
    SseEvent {
        event: None,
        data: "[DONE]".into(),
    }
}

fn assert_post_finish_rejected(packet: Value) {
    let mut decoder = Decoder::new("test-model".into());
    decoder.decode(&delta(json!({"content":"answer"}))).unwrap();
    decoder.decode(&end("stop")).unwrap();
    assert!(decoder.decode(&event(packet.clone())).is_err(), "{packet}");
    assert!(decoder.finish().is_err(), "{packet}");
}

#[test]
fn trailing_usage_phantom_choice_preserves_response_at_eof_and_done() {
    // LiteLLM/sglang may send a singleton empty choice instead of choices: [].
    for with_done in [false, true] {
        for omit_index in [false, true] {
            for null_finish in [false, true] {
                for empty_delta in [
                    json!({}),
                    json!({"content":null}),
                    json!({
                        "role":null,"content":null,"refusal":null,
                        "reasoning_content":null,"reasoning":null,"tool_calls":null
                    }),
                ] {
                    for (finish, expected_stop) in [
                        ("stop", StopReason::EndTurn),
                        ("length", StopReason::MaxTokens),
                    ] {
                        let mut packet = phantom_usage_chunk();
                        packet["choices"][0]["delta"] = empty_delta.clone();
                        if omit_index {
                            packet["choices"][0]
                                .as_object_mut()
                                .unwrap()
                                .remove("index");
                        }
                        if null_finish {
                            packet["choices"][0]["finish_reason"] = Value::Null;
                        }
                        let mut frames = vec![
                            delta(json!({"content":"an"})),
                            delta(json!({"content":"swer"})),
                            end(finish),
                            event(packet),
                        ];
                        if with_done {
                            frames.push(done());
                        }
                        let (items, usage, stop) = decode(frames);
                        assert_eq!(items.len(), 1);
                        assert_eq!(items[0].blocks.len(), 1);
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
                                output_tokens: 10,
                            }
                        );
                        assert_eq!(stop, expected_stop);
                    }
                }
            }
        }
    }
}

#[test]
fn trailing_usage_phantom_choice_rejects_invalid_indices_and_multiple_choices() {
    for index in [
        Value::Null,
        json!("0"),
        json!(1),
        json!(-1),
        json!(0.5),
        json!(true),
    ] {
        let mut packet = phantom_usage_chunk();
        packet["choices"][0]["index"] = index;
        assert_post_finish_rejected(packet);
    }
    for choices in [
        json!([{"delta":{}},{"delta":{}}]),
        json!([{"index":0,"delta":{}},{"index":0,"delta":{}}]),
        json!([{"index":0,"delta":{}},{"index":1,"delta":{}}]),
    ] {
        let mut packet = phantom_usage_chunk();
        packet["choices"] = choices;
        assert_post_finish_rejected(packet);
    }
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
fn unknown_non_null_delta_fields_are_errors_at_every_stage() {
    for field in ["unknown", "function_call", "audio"] {
        for value in [
            json!(""),
            json!(false),
            json!(0),
            json!([]),
            json!({}),
            json!({"data":"x"}),
        ] {
            for stage in 0..3 {
                let mut decoder = Decoder::new("test-model".into());
                if stage > 0 {
                    decoder.decode(&delta(json!({"content":"answer"}))).unwrap();
                }
                if stage > 1 {
                    decoder.decode(&end("stop")).unwrap();
                }
                let mut payload = json!({});
                payload[field] = value.clone();
                assert!(
                    decoder.decode(&delta(payload)).is_err(),
                    "{stage}: {field}={value}"
                );
                assert!(decoder.finish().is_err());
            }
        }
    }
}

#[test]
fn trailing_usage_phantom_choice_retains_usage_validation() {
    for usage in [
        json!({}),
        json!({"prompt_tokens":100}),
        json!({"prompt_tokens":"100","completion_tokens":10}),
        json!({"prompt_tokens":100,"completion_tokens":-1}),
        json!({"prompt_tokens":100,"completion_tokens":10,"total_tokens":109}),
        json!({"prompt_tokens":100,"completion_tokens":10,"prompt_tokens_details":{"cached_tokens":101}}),
        json!({"prompt_tokens":100,"completion_tokens":10,"prompt_tokens_details":{"cached_tokens":-1}}),
        json!({"prompt_tokens":99,"completion_tokens":10,"prompt_tokens_details":{"cached_tokens":80}}),
        json!({"prompt_tokens":100,"completion_tokens":9,"prompt_tokens_details":{"cached_tokens":80}}),
        json!({"prompt_tokens":100,"completion_tokens":10,"prompt_tokens_details":{"cached_tokens":79}}),
    ] {
        let mut decoder = Decoder::new("test-model".into());
        let mut initial = phantom_usage_chunk();
        initial["choices"] = json!([]);
        decoder.decode(&event(initial)).unwrap();
        decoder.decode(&delta(json!({"content":"answer"}))).unwrap();
        decoder.decode(&end("stop")).unwrap();
        let mut packet = phantom_usage_chunk();
        packet["usage"] = usage.clone();
        assert!(decoder.decode(&event(packet)).is_err(), "{usage}");
        assert!(decoder.finish().is_err(), "{usage}");
    }
}

// All these wire representations are semantic no-ops, independent of usage.
fn noop_packets() -> Vec<Value> {
    let mut packets = vec![json!({}), json!({"choices":null}), json!({"choices":[]})];
    let deltas = [
        Value::Null,
        json!({}),
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
fn noop_metadata_before_during_and_after_generation_preserves_output() {
    for mut packet in noop_packets() {
        // Unknown envelope and choice metadata remain ignored, even non-null.
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
            for with_done in [false, true] {
                let mut frames = vec![
                    event(packet.clone()),
                    delta(json!({"reasoning_content":"think"})),
                    event(packet.clone()),
                    delta(json!({"content":"an"})),
                    event(packet.clone()),
                    delta(json!({"refusal":"swer"})),
                    end("length"),
                    event(packet.clone()),
                ];
                if with_done {
                    frames.push(done());
                }
                let (items, actual_usage, stop) = decode(frames);
                assert_eq!(stop, StopReason::MaxTokens, "{packet}");
                assert_eq!(items.len(), 2, "{packet}");
                assert_eq!(items[0].blocks.len(), 1);
                assert_eq!(
                    items[0].blocks[0].content,
                    BlockContent::Reasoning {
                        text: "think".into()
                    }
                );
                assert_eq!(items[1].blocks.len(), 1);
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
fn repeated_finish_can_refine_usage_but_not_regress_counters() {
    let packet = |prompt, completion, cached| {
        json!({
            "choices":[{"finish_reason":"length"}],
            "usage":{
                "prompt_tokens":prompt,"completion_tokens":completion,
                "prompt_tokens_details":{"cached_tokens":cached}
            }
        })
    };
    let initial = packet(100, 9, Value::Null);
    let refined = packet(100, 10, json!(80));
    let (_, usage, stop) = decode(vec![
        delta(json!({"content":"partial"})),
        end("length"),
        event(initial.clone()),
        event(refined.clone()),
        event(json!({"choices":[{"finish_reason":"length","delta":null}]})),
    ]);
    assert_eq!(stop, StopReason::MaxTokens);
    assert_eq!(
        usage,
        Usage {
            input_tokens: 20,
            cached_input_tokens: 80,
            output_tokens: 10
        }
    );
    for invalid in [
        packet(99, 10, json!(80)),
        packet(100, 9, json!(80)),
        packet(100, 10, json!(79)),
        packet(100, 10, json!(101)),
    ] {
        let mut decoder = Decoder::new("test-model".into());
        decoder.decode(&end("length")).unwrap();
        decoder.decode(&event(initial.clone())).unwrap();
        decoder.decode(&event(refined.clone())).unwrap();
        assert!(
            decoder.decode(&event(invalid.clone())).is_err(),
            "{invalid}"
        );
        assert!(decoder.finish().is_err());
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
fn noop_metadata_and_repeated_tool_finish_preserve_one_complete_tool() {
    let mut frames = vec![];
    frames.extend(noop_packets().into_iter().map(event));
    frames.push(delta(json!({"tool_calls":[{"index":0,"id":"call","function":{"name":"inspect","arguments":"{\"path\":"}}]})));
    frames.extend(noop_packets().into_iter().map(event));
    frames.push(delta(
        json!({"tool_calls":[{"index":0,"function":{"arguments":"\"file\"}"}}]}),
    ));
    frames.push(end("tool_calls"));
    frames.extend(noop_packets().into_iter().map(event));
    frames.push(event(json!({"choices":[{"finish_reason":"tool_calls","delta":{"role":"assistant","tool_calls":[]}}]})));
    frames.push(event(phantom_usage_chunk()));
    frames.push(event(
        json!({"choices":[{"finish_reason":"tool_calls","delta":null}]}),
    ));
    frames.push(done());
    let (items, usage, stop) = decode(frames);
    assert_eq!(stop, StopReason::ToolUse);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].blocks.len(), 1);
    assert_eq!(
        items[0].blocks[0].content,
        BlockContent::ToolCall(ToolCall {
            id: "call".into(),
            name: "inspect".into(),
            arguments: json!({"path":"file"}),
        })
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

#[test]
fn malformed_known_chunk_choice_and_delta_values_still_fail() {
    let mut packets = vec![
        Value::Null,
        json!(false),
        json!(0),
        json!(""),
        json!([]),
        json!([[], null]),
        json!({"choices":[[0, {}, null]]}),
        json!({"usage":[1, 1, 2]}),
    ];
    for value in [json!(false), json!(0), json!(""), json!({})] {
        packets.push(json!({"choices":value}));
    }
    for value in [Value::Null, json!(false), json!(0), json!(""), json!([])] {
        packets.push(json!({"choices":[value]}));
    }
    for value in [json!(false), json!(0), json!(""), json!([])] {
        packets.push(json!({"choices":[{"delta":value}]}));
    }
    for field in [
        "role",
        "content",
        "refusal",
        "reasoning",
        "reasoning_content",
        "tool_calls",
    ] {
        for value in [json!(false), json!(0), json!({})] {
            let mut packet = json!({"choices":[{"delta":{}}]});
            packet["choices"][0]["delta"][field] = value;
            packets.push(packet);
        }
    }
    for value in [
        json!(false),
        json!(0),
        json!([]),
        json!({}),
        json!("unknown"),
    ] {
        packets.push(json!({"choices":[{"finish_reason":value}]}));
    }
    for packet in packets {
        for stage in 0..3 {
            let mut decoder = Decoder::new("test-model".into());
            if stage > 0 {
                decoder.decode(&delta(json!({"content":"answer"}))).unwrap();
            }
            if stage > 1 {
                decoder.decode(&end("stop")).unwrap();
            }
            assert!(
                decoder.decode(&event(packet.clone())).is_err(),
                "{stage}: {packet}"
            );
            assert!(decoder.finish().is_err());
        }
    }
}

#[test]
fn empty_choices_do_not_bypass_usage_or_native_error_validation() {
    for choices in [None, Some(Value::Null), Some(json!([]))] {
        for payload in [
            json!({"usage":false}),
            json!({"usage":[]}),
            json!({"usage":{}}),
            json!({"usage":{"prompt_tokens":null,"completion_tokens":1}}),
            json!({"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":3}}),
            json!({"usage":{"prompt_tokens":1,"completion_tokens":1,"prompt_tokens_details":[]}}),
            json!({"error":{"type":"server_error","message":"secret"}}),
        ] {
            for stage in 0..3 {
                let mut decoder = Decoder::new("test-model".into());
                if stage > 0 {
                    decoder.decode(&delta(json!({"content":"answer"}))).unwrap();
                }
                if stage > 1 {
                    decoder.decode(&end("stop")).unwrap();
                }
                let mut packet = payload.clone();
                if let Some(choices) = &choices {
                    packet["choices"] = choices.clone();
                }
                assert!(
                    decoder.decode(&event(packet.clone())).is_err(),
                    "{stage}: {packet}"
                );
                assert!(decoder.finish().is_err());
            }
        }
    }
}

#[test]
fn non_stream_choice_output_is_not_silently_ignored() {
    for field in ["message", "text"] {
        for value in [
            json!(""),
            json!("answer"),
            json!({"role":"assistant","content":"answer"}),
            json!({}),
            json!(false),
        ] {
            for delta_value in [None, Some(Value::Null), Some(json!({}))] {
                for stage in 0..3 {
                    let mut decoder = Decoder::new("test-model".into());
                    if stage > 0 {
                        decoder.decode(&delta(json!({"content":"answer"}))).unwrap();
                    }
                    if stage > 1 {
                        decoder.decode(&end("stop")).unwrap();
                    }
                    let mut choice = json!({});
                    choice[field] = value.clone();
                    if let Some(value) = &delta_value {
                        choice["delta"] = value.clone();
                    }
                    assert!(
                        decoder.decode(&event(json!({"choices":[choice]}))).is_err(),
                        "{stage}: {field}={value}"
                    );
                    assert!(decoder.finish().is_err());
                }
            }
        }
    }
    let (items, _, stop) = decode(vec![
        event(json!({"choices":[{"message":null,"text":null}]})),
        delta(json!({"content":"answer"})),
        end("stop"),
        event(json!({"choices":[{"message":null,"text":null}]})),
    ]);
    assert_eq!(stop, StopReason::EndTurn);
    assert_eq!(items.len(), 1);
    assert_eq!(
        items[0].blocks[0].content,
        BlockContent::Text {
            text: "answer".into()
        }
    );
}

#[test]
fn empty_sse_event_name_uses_default_message_framing() {
    let frames = [
        event(json!({})),
        delta(json!({"content":"answer"})),
        end("stop"),
        done(),
    ]
    .into_iter()
    .map(|mut frame| {
        frame.event = Some(String::new());
        frame
    })
    .collect();
    let (items, _, stop) = decode(frames);
    assert_eq!(stop, StopReason::EndTurn);
    assert_eq!(items.len(), 1);
    assert_eq!(
        items[0].blocks[0].content,
        BlockContent::Text {
            text: "answer".into()
        }
    );
}

#[test]
fn repeated_abnormal_finish_does_not_finalize_discarded_tools_twice() {
    for (finish, expected_stop) in [
        ("length", StopReason::MaxTokens),
        ("abort", StopReason::Aborted),
        ("content_filter", StopReason::ContentFilter),
    ] {
        let (items, usage, stop) = decode(vec![
            delta(
                json!({"content":"partial","tool_calls":[{"index":0,"id":"call","function":{"name":"inspect","arguments":"{"}}]}),
            ),
            end(finish),
            event(
                json!({"choices":[{"finish_reason":finish,"delta":{"role":"assistant","tool_calls":[]}}]}),
            ),
            event(phantom_usage_chunk()),
            event(json!({"choices":[{"finish_reason":finish}]})),
            done(),
        ]);
        assert_eq!(stop, expected_stop);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].blocks.len(), 1);
        assert_eq!(
            items[0].blocks[0].content,
            BlockContent::Text {
                text: "partial".into()
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

#[test]
fn abnormal_finishes_discard_complete_and_incomplete_tools() {
    for (finish, expected) in [
        ("abort", StopReason::Aborted),
        ("length", StopReason::MaxTokens),
        ("content_filter", StopReason::ContentFilter),
    ] {
        for arguments in ["{", "{}", "not json", "[]"] {
            let (items, _, reason) = decode(vec![
                delta(
                    json!({"content":"partial answer","tool_calls":[{"index":0,"id":"call","function":{"name":"inspect","arguments":arguments}}]}),
                ),
                end(finish),
            ]);
            assert_eq!(reason, expected);
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].kind, ItemKind::Text);
        }
    }
}

#[test]
fn normal_finish_requires_complete_valid_tools() {
    for arguments in ["{", "[]", "null"] {
        let mut decoder = Decoder::new("test-model".into());
        decoder.decode(&delta(json!({"tool_calls":[{"index":0,"id":"call","function":{"name":"inspect","arguments":arguments}}]}))).unwrap();
        assert!(decoder.decode(&end("tool_calls")).is_err());
        assert!(decoder.finish().is_err());
    }
}

#[test]
fn reasoning_envelope_roundtrip_is_scoped_and_policy_selected() {
    use super::super::common::{bind_reasoning_scope, filter_reasoning_scope, reasoning_scope};
    let scope = reasoning_scope("local", "http://localhost/v1/chat/completions");
    let mut decoder = Decoder::new("test-model".into());
    let mut assembler = ResponseAssembler::default();
    for frame in [
        delta(json!({"reasoning_content":"first "})),
        delta(json!({"reasoning":"second"})),
        end("stop"),
    ] {
        for mut chunk in decoder.decode(&frame).unwrap() {
            bind_reasoning_scope(&mut chunk, &scope);
            assembler.push(&chunk).unwrap();
        }
    }
    for chunk in decoder.finish().unwrap() {
        assembler.push(&chunk).unwrap();
    }
    let (items, _, _) = assembler.finish().unwrap();
    let envelope = items[0].replay.as_ref().unwrap();
    assert_eq!(envelope.version, 1);
    assert_eq!(envelope.protocol, "chat_completions");
    assert_eq!(envelope.model, "test-model");
    assert_eq!(envelope.scope, scope);
    assert_eq!(envelope.payload, json!({"text":"first second"}));
    let original = request(items);
    for (policy, field, absent) in [
        (
            ChatReasoningReplay::ReasoningContent,
            "reasoning_content",
            "reasoning",
        ),
        (
            ChatReasoningReplay::Reasoning,
            "reasoning",
            "reasoning_content",
        ),
    ] {
        let mut request = original.clone();
        filter_reasoning_scope(&mut request, &scope);
        let body = encode(&request, policy).unwrap();
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        assert!(body["messages"][0]["content"].is_null());
        assert_eq!(body["messages"][0][field], "first second");
        assert!(body["messages"][0].get(absent).is_none());
        assert_eq!(body["n"], 1);
    }
    assert_eq!(
        encode(&original, ChatReasoningReplay::Unsupported).unwrap()["messages"],
        json!([])
    );
    for mutation in [
        "scope", "model", "protocol", "version", "missing", "payload",
    ] {
        let mut request = original.clone();
        let Message::Assistant(items) = &mut request.messages[0] else {
            unreachable!()
        };
        let envelope = items[0].replay.as_mut().unwrap();
        match mutation {
            "scope" => envelope.scope = "elsewhere".into(),
            "model" => envelope.model = "different-model".into(),
            "protocol" => envelope.protocol = "responses".into(),
            "version" => envelope.version += 1,
            "payload" => envelope.payload = json!({"text":42}),
            "missing" => items[0].replay = None,
            _ => unreachable!(),
        }
        filter_reasoning_scope(&mut request, &scope);
        assert_eq!(
            encode(&request, ChatReasoningReplay::ReasoningContent).unwrap()["messages"],
            json!([]),
            "{mutation}"
        );
    }
}

#[test]
fn replay_uses_payload_not_visible_blocks_and_keeps_text_and_tools() {
    let item = AssistantItem::reasoning(
        "r",
        0,
        "visible summary not original",
        Some(reasoning_envelope(
            "chat_completions",
            "test-model",
            json!({"text":"original private thought"}),
        )),
    );
    let req = request(vec![
        item,
        AssistantItem::text("t", 1, "answer"),
        AssistantItem::tool_call(
            "c",
            2,
            ToolCall {
                id: "call".into(),
                name: "inspect".into(),
                arguments: json!({}),
            },
        ),
    ]);
    let body = encode(&req, ChatReasoningReplay::Reasoning).unwrap();
    assert_eq!(body["messages"][0]["reasoning"], "original private thought");
    assert_eq!(body["messages"][0]["content"], "answer");
    assert_eq!(body["messages"][0]["tool_calls"][0]["id"], "call");
    assert!(!body.to_string().contains("visible summary"));
}

#[test]
fn reasoning_aliases_are_validated_and_not_duplicated() {
    let (items, _, _) = decode(vec![
        delta(json!({"reasoning":"same","reasoning_content":"same"})),
        end("stop"),
    ]);
    assert_eq!(
        items[0].blocks[0].content,
        BlockContent::Reasoning {
            text: "same".into()
        }
    );
    for value in [
        json!({"reasoning":"a","reasoning_content":"b"}),
        json!({"reasoning":{}}),
        json!({"content":[]}),
        json!({"audio":{"data":"x"}}),
    ] {
        let mut decoder = Decoder::new("test-model".into());
        assert!(decoder.decode(&delta(value)).is_err());
    }
    decode(vec![
        delta(json!({"role":null,"content":null,"reasoning":null,"function_call":null})),
        end("stop"),
    ]);
}

#[test]
fn max_reasoning_effort_is_allowed_but_unknown_values_are_not() {
    let mut req = request(vec![]);
    req.reasoning = Some("max".into());
    assert_eq!(
        encode(&req, ChatReasoningReplay::Unsupported).unwrap()["reasoning_effort"],
        "max"
    );
    req.reasoning = Some("unbounded".into());
    assert!(encode(&req, ChatReasoningReplay::Unsupported).is_err());
}

#[test]
fn native_in_band_errors_are_classified_without_exposing_server_text() {
    use crate::provider::ProviderErrorKind;
    for named_event in [false, true] {
        let error = json!({"type":"exceed_context_size_error","code":400,"message":"secret prompt and API key"});
        let mut frame = if named_event {
            event(error)
        } else {
            event(json!({"error":error}))
        };
        if named_event {
            frame.event = Some("error".into());
        }
        let mut decoder = Decoder::new("test-model".into());
        let error = decoder.decode(&frame).unwrap_err();
        assert_eq!(error.kind, ProviderErrorKind::ContextWindowExceeded);
        assert!(!error.message.contains("secret"));
        assert!(decoder.finish().is_err());
    }
}

#[test]
fn sequential_tool_updates_still_validate_headers_and_distinct_call_ids() {
    for calls in [
        json!([{"index":0,"id":"same","function":{"name":"one","arguments":"{}"}},
               {"index":1,"id":"same","function":{"name":"two","arguments":"{}"}}]),
        json!([{"index":0,"function":{"arguments":"{}"}}]),
        json!([{"index":0,"id":"call","function":{"name":"invalid name","arguments":"{}"}}]),
    ] {
        let mut decoder = Decoder::new("test-model".into());
        decoder.decode(&delta(json!({"tool_calls":calls}))).unwrap();
        assert!(decoder.decode(&end("tool_calls")).is_err());
    }
    for call in [
        json!({"index":null}),
        json!({"index":"0"}),
        json!({"index":0,"type":"custom"}),
        json!({"index":0,"function":{"arguments":{}}}),
        json!({"index":0,"unknown":1}),
    ] {
        let mut decoder = Decoder::new("test-model".into());
        assert!(
            decoder
                .decode(&delta(json!({"tool_calls":[call]})))
                .is_err()
        );
    }
}
