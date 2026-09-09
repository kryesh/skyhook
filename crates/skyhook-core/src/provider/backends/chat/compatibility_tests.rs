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
