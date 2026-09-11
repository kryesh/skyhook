//! Generic Responses compatibility regressions, shared by HTTP/SSE and WebSocket.
//!
//! These mocked events intentionally contain no gateway/vendor identifiers or
//! feature flags. Omitted indices and regenerated *output-item* IDs are tolerated
//! only when their meaning is unique. Explicit contradictory references, ambiguous
//! semantic matches, and changed executable tool call IDs remain protocol errors.

use super::*;
use crate::provider::protocol::{AssistantItem, ResponseAssembler, ToolResult};

fn request() -> ModelRequest {
    ModelRequest {
        model: "test-model".into(),
        system: vec![],
        messages: vec![],
        tools: vec![],
        response_schema: None,
        reasoning: None,
        max_output_tokens: None,
        correlation: None,
    }
}

fn message(id: &str, text: &str) -> Value {
    json!({"type":"message", "id":id, "role":"assistant", "status":"completed",
        "content":[{"type":"output_text", "text":text, "annotations":[]}]})
}

fn reasoning(id: &str, text: &str) -> Value {
    json!({"type":"reasoning", "id":id,
        "summary":[{"type":"summary_text", "text":text}]})
}

fn function(id: &str, call_id: &str, arguments: &str) -> Value {
    json!({"type":"function_call", "id":id, "call_id":call_id,
        "name":"lookup", "arguments":arguments, "status":"completed"})
}

fn added(position: usize, item: Value) -> Value {
    json!({"type":"response.output_item.added", "output_index":position, "item":item})
}

fn done(position: usize, item: Value) -> Value {
    json!({"type":"response.output_item.done", "output_index":position, "item":item})
}

fn completed(output: Vec<Value>) -> Value {
    json!({"type":"response.completed", "response":{"status":"completed", "output":output,
        "usage":{"input_tokens":12, "output_tokens":3,
            "input_tokens_details":{"cached_tokens":4}}}})
}

fn assemble(events: Vec<Value>) -> Result<(Vec<AssistantItem>, Usage, StopReason), ProviderError> {
    let mut decoder = Decoder::new("test-model".into());
    let mut assembler = ResponseAssembler::default();
    for event in events {
        for chunk in decoder.feed(event)? {
            assembler.push(&chunk)?;
        }
    }
    for chunk in decoder.finish()? {
        assembler.push(&chunk)?;
    }
    assembler.finish()
}

fn assert_protocol_error(events: Vec<Value>) {
    let mut decoder = Decoder::new("test-model".into());
    for event in events {
        if let Err(error) = decoder.feed(event) {
            assert_eq!(error.kind, ProviderErrorKind::Protocol, "{error:?}");
            return;
        }
    }
    // Do not call finish here: an unrelated missing-terminal error could mask
    // accidental acceptance of the invalid reference this test is exercising.
    panic!("unsafe compatibility normalization must fail while feeding events");
}

/// Missing indices on item starts/ends are resolved using distinct native IDs;
/// later events can address the second item without accidentally mutating the first.
#[test]
fn missing_output_indices_preserve_distinct_item_identity_and_order() {
    let first = message("message-a", "first");
    let second = message("message-b", "second");
    let (items, _, reason) = assemble(vec![
        json!({"type":"response.output_item.added", "item":message("message-a", "")}),
        json!({"type":"response.output_item.added", "item":message("message-b", "")}),
        json!({"type":"response.output_text.delta", "item_id":"message-b", "delta":"second"}),
        json!({"type":"response.output_text.delta", "item_id":"message-a", "delta":"first"}),
        json!({"type":"response.output_item.done", "item":second.clone()}),
        json!({"type":"response.output_item.done", "item":first.clone()}),
        completed(vec![first, second]),
    ])
    .unwrap();
    assert_eq!(reason, StopReason::EndTurn);
    assert_eq!(items.len(), 2);
    assert_eq!((&*items[0].id, items[0].position), ("message-a", 0));
    assert_eq!((&*items[1].id, items[1].position), ("message-b", 1));
    assert_eq!(items[0].text_content().as_deref(), Some("first"));
    assert_eq!(items[1].text_content().as_deref(), Some("second"));
}

/// Text and summary part envelopes/done events may omit their sole part index.
/// Repeated authoritative done snapshots must not duplicate already streamed text.
#[test]
fn missing_single_part_indices_work_for_text_and_reasoning_summaries() {
    for (native, family, part) in [
        (
            message("item", "visible"),
            "output_text",
            json!({"type":"output_text", "text":"visible"}),
        ),
        (
            reasoning("item", "visible"),
            "reasoning_summary_text",
            json!({"type":"summary_text", "text":"visible"}),
        ),
    ] {
        let part_family = if family == "output_text" {
            "content_part"
        } else {
            "reasoning_summary_part"
        };
        let (items, _, _) = assemble(vec![
            added(0, native.clone()),
            json!({"type":format!("response.{part_family}.added"), "item_id":"item",
                "part":{"type":part["type"], "text":""}}),
            json!({"type":format!("response.{family}.delta"), "item_id":"item", "delta":"visible"}),
            json!({"type":format!("response.{family}.done"), "item_id":"item", "text":"visible"}),
            json!({"type":format!("response.{part_family}.done"), "item_id":"item", "part":part}),
            json!({"type":"response.output_item.done", "item":native.clone()}),
            completed(vec![native]),
        ])
        .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].blocks.len(), 1);
        let content = &items[0].blocks[0].content;
        assert_eq!(
            content
                .text_content()
                .or_else(|| content.reasoning_content()),
            Some("visible")
        );
    }
}

/// An omitted part index can select the only existing part even when it is not
/// zero. Defaulting every omitted index to zero would corrupt this two-part item.
#[test]
fn omitted_part_index_selects_the_unique_existing_part() {
    let mut native = message("message", "first");
    native["content"]
        .as_array_mut()
        .unwrap()
        .push(json!({"type":"output_text", "text":"second"}));
    let (items, _, _) = assemble(vec![
        added(0, message("message", "")),
        json!({"type":"response.output_text.delta", "output_index":0, "item_id":"message",
            "content_index":1, "delta":"sec"}),
        json!({"type":"response.output_text.delta", "item_id":"message", "delta":"ond"}),
        completed(vec![native]),
    ])
    .unwrap();
    assert_eq!(items[0].blocks.len(), 2);
    assert_eq!(items[0].blocks[0].content.text_content(), Some("first"));
    assert_eq!(items[0].blocks[1].content.text_content(), Some("second"));
}

/// A typed text delta can lazily open its uniquely identified item and block;
/// terminal completion closes it even when no added/done envelopes were sent.
#[test]
fn lazy_text_start_from_delta_does_not_require_added_events() {
    let (items, _, reason) = assemble(vec![
        json!({"type":"response.output_text.delta", "output_index":0,
            "item_id":"lazy-message", "delta":"hello "}),
        json!({"type":"response.output_text.delta", "item_id":"lazy-message", "delta":"world"}),
        completed(vec![message("lazy-message", "hello world")]),
    ])
    .unwrap();
    assert_eq!(reason, StopReason::EndTurn);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].id, "lazy-message");
    assert_eq!(items[0].text_content().as_deref(), Some("hello world"));
}

/// An item-done snapshot is enough to start a missing reasoning/message/tool
/// item. Native output-item identity and executable tool call identity stay separate.
#[test]
fn lazy_item_done_starts_all_supported_item_kinds() {
    let output = vec![
        reasoning("reason", "plan"),
        message("text", "checking"),
        function("function", "call-stable", r#"{"key":"value"}"#),
    ];
    let mut events = output
        .iter()
        .map(|item| json!({"type":"response.output_item.done", "item":item}))
        .collect::<Vec<_>>();
    events.push(completed(output));
    let (items, _, reason) = assemble(events).unwrap();
    assert_eq!(reason, StopReason::ToolUse);
    assert_eq!(items.len(), 3);
    assert_eq!(items[0].reasoning_content().as_deref(), Some("plan"));
    assert_eq!(items[1].text_content().as_deref(), Some("checking"));
    assert_eq!(items[2].tool_call_ref().unwrap().id, "call-stable");
}

/// Tool argument deltas can omit output_index when their item_id is unique.
#[test]
fn tool_argument_events_resolve_by_native_item_id() {
    let call = function("function", "call-stable", r#"{"key":"value"}"#);
    let (items, _, reason) = assemble(vec![
        added(0, function("function", "call-stable", "")),
        json!({"type":"response.function_call_arguments.delta", "item_id":"function", "delta":"{\"key\":"}),
        json!({"type":"response.function_call_arguments.delta", "item_id":"function", "delta":"\"value\"}"}),
        json!({"type":"response.function_call_arguments.done", "item_id":"function", "arguments":call["arguments"]}),
        json!({"type":"response.output_item.done", "item":call.clone()}),
        completed(vec![call]),
    ]).unwrap();
    assert_eq!(reason, StopReason::ToolUse);
    let call = items[0].tool_call_ref().unwrap();
    assert_eq!(call.id, "call-stable");
    assert_eq!(call.arguments, json!({"key":"value"}));
}

/// Regenerated terminal IDs are safe for uniquely equivalent semantic items.
/// Inconsequential metadata and JSON object formatting are not semantic identity.
#[test]
fn terminal_regenerated_ids_preserve_streamed_identity_without_duplicate_items() {
    let streamed = [
        reasoning("reason-stream", "plan"),
        message("text-stream", "checking"),
        function("function-stream", "call-stable", r#"{"a":1,"b":2}"#),
    ];
    let mut terminal = vec![
        reasoning("reason-terminal", "plan"),
        message("text-terminal", "checking"),
        function("function-terminal", "call-stable", r#"{ "b": 2, "a": 1 }"#),
    ];
    terminal[1]["content"][0]["annotations"] =
        json!([{"type":"url_citation", "url":"https://example.invalid/"}]);
    let mut events = Vec::new();
    for (position, item) in streamed.iter().enumerate() {
        events.push(added(position, item.clone()));
        events.push(done(position, item.clone()));
    }
    events.push(completed(terminal));
    let (items, _, reason) = assemble(events).unwrap();
    assert_eq!(reason, StopReason::ToolUse);
    assert_eq!(
        items
            .iter()
            .map(|item| item.id.as_str())
            .collect::<Vec<_>>(),
        vec!["reason-stream", "text-stream", "function-stream"]
    );
    assert_eq!(items[2].tool_call_ref().unwrap().id, "call-stable");
    assert_eq!(
        items[2].tool_call_ref().unwrap().arguments,
        json!({"a":1, "b":2})
    );
}

/// Text equality is required, not just equal kind/position or a matching prefix.
#[test]
fn regenerated_terminal_id_cannot_replace_changed_text() {
    assert_protocol_error(vec![
        added(0, message("stream", "original")),
        done(0, message("stream", "original")),
        completed(vec![message("terminal", "replacement")]),
    ]);
}

/// Array position is not proof of identity when multiple streamed items have
/// identical content. A regenerated ID must not arbitrarily pick one of them.
#[test]
fn regenerated_terminal_ids_reject_ambiguous_equivalent_items() {
    assert_protocol_error(vec![
        added(0, message("first", "same")),
        done(0, message("first", "same")),
        added(1, message("second", "same")),
        done(1, message("second", "same")),
        completed(vec![
            message("new-first", "same"),
            message("new-second", "same"),
        ]),
    ]);
}

/// Known IDs still disambiguate equal content; semantic ambiguity is relevant
/// only when compatibility matching is actually needed.
#[test]
fn unchanged_terminal_ids_allow_identical_content() {
    let output = vec![message("first", "same"), message("second", "same")];
    let (items, _, _) = assemble(vec![
        added(0, output[0].clone()),
        done(0, output[0].clone()),
        added(1, output[1].clone()),
        done(1, output[1].clone()),
        completed(output),
    ])
    .unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].id, "first");
    assert_eq!(items[1].id, "second");
}

/// Output-item IDs may be aliases, but call_id is executable identity and can
/// never be repaired/replaced, even when name and arguments happen to match.
#[test]
fn tool_call_id_conflicts_fail_for_done_and_terminal_snapshots() {
    for change_output_id in [false, true] {
        for at_terminal in [false, true] {
            let original = function("function", "call-original", r#"{"key":"value"}"#);
            let changed = function(
                if change_output_id {
                    "regenerated"
                } else {
                    "function"
                },
                "call-changed",
                r#"{"key":"value"}"#,
            );
            let mut events = vec![added(0, original.clone())];
            if at_terminal {
                events.push(done(0, original));
                events.push(completed(vec![changed]));
            } else {
                events.push(done(0, changed.clone()));
                events.push(completed(vec![changed]));
            }
            assert_protocol_error(events);
        }
    }
}

/// Matching call_id alone cannot excuse a changed tool name or argument object.
#[test]
fn regenerated_tool_output_id_requires_equivalent_name_and_arguments() {
    for field in ["name", "arguments"] {
        let original = function("function", "call-stable", r#"{"key":"value"}"#);
        let mut changed = function("regenerated", "call-stable", r#"{"key":"value"}"#);
        changed[field] = if field == "name" {
            json!("different_tool")
        } else {
            json!(r#"{"key":"other"}"#)
        };
        assert_protocol_error(vec![
            added(0, original.clone()),
            done(0, original),
            completed(vec![changed]),
        ]);
    }
}

/// An alias attached to an already known item of the wrong kind is a conflict,
/// not evidence for swapping identities or rewriting tool IDs heuristically.
#[test]
fn terminal_id_owned_by_another_kind_is_not_repaired() {
    let text = message("text", "checking");
    let call = function("function", "call-stable", "{}");
    assert_protocol_error(vec![
        added(0, text.clone()),
        done(0, text),
        added(1, call.clone()),
        done(1, call),
        completed(vec![
            message("function", "checking"),
            function("text", "call-stable", "{}"),
        ]),
    ]);
}

/// Missing indices are not equivalent to explicit null/string/negative/fractional
/// indices. A valid item_id must not hide malformed explicit references.
#[test]
fn invalid_explicit_indices_are_never_treated_as_missing() {
    for invalid in [
        Value::Null,
        json!("0"),
        json!(-1),
        json!(0.5),
        json!(true),
        json!({}),
        json!([]),
    ] {
        for key in ["output_index", "content_index"] {
            let mut delta = json!({"type":"response.output_text.delta", "output_index":0,
                "content_index":0, "item_id":"text", "delta":"x"});
            delta[key] = invalid.clone();
            assert_protocol_error(vec![
                added(0, message("text", "")),
                delta,
                completed(vec![message("text", "x")]),
            ]);
        }
        let mut summary = json!({"type":"response.reasoning_summary_text.delta", "output_index":0,
            "summary_index":0, "item_id":"reason", "delta":"x"});
        summary["summary_index"] = invalid.clone();
        assert_protocol_error(vec![
            added(0, reasoning("reason", "")),
            summary,
            completed(vec![reasoning("reason", "x")]),
        ]);
        let mut start = added(0, message("text", "x"));
        start["output_index"] = invalid;
        assert_protocol_error(vec![start, completed(vec![message("text", "x")])]);
    }
}

/// Explicit output_index and item_id must agree; neither gets silently preferred.
#[test]
fn explicit_index_and_item_id_conflicts_are_errors() {
    assert_protocol_error(vec![
        added(0, message("first", "")),
        added(1, message("second", "")),
        json!({"type":"response.output_text.delta", "output_index":0,
            "item_id":"second", "content_index":0, "delta":"wrong target"}),
    ]);
}

/// Multiple existing parts make an omitted content/summary index ambiguous.
#[test]
fn omitted_part_index_rejects_multiple_candidate_parts() {
    for (native, family, index_key) in [
        (message("item", ""), "output_text", "content_index"),
        (
            reasoning("item", ""),
            "reasoning_summary_text",
            "summary_index",
        ),
    ] {
        let mut events = vec![added(0, native)];
        for position in 0..2 {
            let mut delta = json!({"type":format!("response.{family}.delta"),
                "output_index":0, "item_id":"item", "delta":"part"});
            delta[index_key] = json!(position);
            events.push(delta);
        }
        events.push(json!({"type":format!("response.{family}.delta"), "item_id":"item", "delta":"ambiguous"}));
        assert_protocol_error(events);
    }
}

/// With no index or item ID, a text event cannot select one of two text items.
#[test]
fn omitted_item_reference_rejects_multiple_candidate_items() {
    assert_protocol_error(vec![
        added(0, message("first", "")),
        added(1, message("second", "")),
        json!({"type":"response.output_text.delta", "content_index":0, "delta":"ambiguous"}),
    ]);
}

/// Even a syntactically complete tool is unsafe after an incomplete stop. Some
/// compatible endpoints put that status inside a response.completed envelope.
#[test]
fn completed_envelope_with_incomplete_status_discards_all_tools() {
    for already_done in [false, true] {
        for arguments in [r#"{"key":"value"}"#, r#"{"key":"#] {
            let text = message("text", "partial answer");
            let call = function("function", "call-stable", arguments);
            let mut events = vec![
                added(0, text.clone()),
                done(0, text.clone()),
                added(1, function("function", "call-stable", "")),
            ];
            if already_done {
                events.push(done(1, call.clone()));
            }
            let mut terminal = completed(vec![text, call]);
            terminal["response"]["status"] = json!("incomplete");
            terminal["response"]["incomplete_details"] = json!({"reason":"max_output_tokens"});
            events.push(terminal);
            let (items, usage, reason) = assemble(events).unwrap();
            assert_eq!(reason, StopReason::MaxTokens);
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].text_content().as_deref(), Some("partial answer"));
            assert!(items.iter().all(|item| item.tool_call_ref().is_none()));
            assert_eq!(
                usage,
                Usage {
                    input_tokens: 8,
                    cached_input_tokens: 4,
                    output_tokens: 3
                }
            );
        }
    }
}

/// All supported output kinds can be delivered entirely in the terminal snapshot.
/// The next request replays one native reasoning item and preserves call/result IDs.
#[test]
fn terminal_only_output_replays_across_tool_and_followup_turns() {
    let mut native_reasoning = reasoning("reason-terminal", "plan");
    native_reasoning["encrypted_content"] = json!("mock-replay-state");
    let (items, _, reason) = assemble(vec![completed(vec![
        native_reasoning.clone(),
        message("text-terminal", "checking"),
        function("function-terminal", "call-stable", r#"{"key":"value"}"#),
    ])])
    .unwrap();
    assert_eq!(reason, StopReason::ToolUse);
    assert_eq!(items.len(), 3);
    let mut req = request();
    req.messages = vec![
        Message::User(vec![UserContent::Text {
            text: "look it up".into(),
        }]),
        Message::Assistant(items),
        Message::Tool(vec![ToolResult {
            call_id: "call-stable".into(),
            name: "lookup".into(),
            result: json!({"value":42}),
            images: vec![],
            is_error: false,
        }]),
    ];
    let encoded = encode(&req).unwrap();
    let input = encoded["input"].as_array().unwrap();
    assert_eq!(
        input
            .iter()
            .filter(|item| item["type"] == "reasoning")
            .count(),
        1
    );
    assert_eq!(input[1], native_reasoning);
    assert_eq!(input[3]["type"], "function_call");
    assert_eq!(input[3]["call_id"], "call-stable");
    assert_eq!(input[4]["type"], "function_call_output");
    assert_eq!(input[4]["call_id"], "call-stable");

    let (followup, _, stop) = assemble(vec![completed(vec![message("answer", "42")])]).unwrap();
    assert_eq!(stop, StopReason::EndTurn);
    req.messages.push(Message::Assistant(followup));
    req.messages.push(Message::User(vec![UserContent::Text {
        text: "thanks".into(),
    }]));
    let replayed = encode(&req).unwrap();
    let replayed = replayed["input"].as_array().unwrap();
    assert_eq!(&replayed[..input.len()], input.as_slice());
    assert_eq!(replayed[5]["content"][0]["text"], "42");
    assert_eq!(replayed[6]["content"][0]["text"], "thanks");
}

/// Summary generation is always requested, independent of model spelling or
/// whether the caller set a reasoning effort. Compatibility needs no vendor flag.
#[test]
fn encoding_always_requests_reasoning_summaries() {
    for model in ["test-model", "another-model"] {
        for effort in [None, Some("none"), Some("medium")] {
            let mut req = request();
            req.model = model.into();
            req.reasoning = effort.map(str::to_owned);
            let encoded = encode(&req).unwrap();
            assert_eq!(encoded["reasoning"]["summary"], "auto");
            if let Some(effort) = effort {
                assert_eq!(encoded["reasoning"]["effort"], effort);
            }
        }
    }
}

/// Equal text is not proof of a shared identity when neither item appeared in
/// the stream. Each terminal-only item must retain its own position and ID.
#[test]
fn equal_terminal_only_items_remain_distinct() {
    let (items, _, _) = assemble(vec![completed(vec![
        message("first", "same"),
        message("second", "same"),
    ])])
    .unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].id, "first");
    assert_eq!(items[1].id, "second");
}

#[test]
fn equal_done_only_items_remain_distinct_with_or_without_indices() {
    for indexed in [false, true] {
        let first = message("first", "same");
        let second = message("second", "same");
        let mut events = vec![done(0, first.clone()), done(1, second.clone())];
        if !indexed {
            for event in &mut events {
                event.as_object_mut().unwrap().remove("output_index");
            }
        }
        events.push(completed(vec![first, second]));
        let (items, _, _) = assemble(events).unwrap();
        assert_eq!(items.len(), 2);
    }
}

/// An internal position allocated for an omitted index must not be mistaken
/// for an actual wire index. Later explicit evidence can establish that index.
#[test]
fn omitted_output_index_can_be_established_later_by_identity() {
    let output = message("msg", "hello");
    let (items, _, _) = assemble(vec![
        json!({"type":"response.output_item.added","item":message("msg", "")}),
        json!({"type":"response.output_text.delta","item_id":"msg","output_index":7,"delta":"hello"}),
        done(7, output.clone()),
        completed(vec![output]),
    ]).unwrap();
    assert_eq!(items[0].position, 0);
    assert_eq!(items[0].text_content().as_deref(), Some("hello"));
}

#[test]
fn invented_output_position_does_not_disambiguate_missing_identity() {
    assert_protocol_error(vec![
        json!({"type":"response.output_item.added","item":message("first", "")}),
        json!({"type":"response.output_item.added","item":message("second", "")}),
        json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"ambiguous"}),
    ]);
}

#[test]
fn identical_item_done_is_idempotent() {
    let output = message("msg", "hello");
    let (items, _, _) = assemble(vec![
        done(0, output.clone()),
        done(0, output.clone()),
        completed(vec![output]),
    ])
    .unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].text_content().as_deref(), Some("hello"));
}

#[test]
fn empty_sse_event_name_means_default_message_framing() {
    let mut decoder = Decoder::new("test-model".into());
    let mut assembler = ResponseAssembler::default();
    let event = super::super::transport::SseEvent {
        event: Some(String::new()),
        data: completed(vec![message("msg", "hello")]).to_string(),
    };
    for chunk in decoder.decode(&event).unwrap() {
        assembler.push(&chunk).unwrap();
    }
    let (items, _, _) = assembler.finish().unwrap();
    assert_eq!(items[0].text_content().as_deref(), Some("hello"));
}

#[test]
fn explicit_wire_owner_precedes_unbound_items() {
    let first = message("first", "hello");
    let second = message("second", "world");
    let (items, _, _) = assemble(vec![
        added(0, message("first", "")),
        json!({"type":"response.output_item.added","item":message("second", "")}),
        json!({"type":"response.output_text.delta","output_index":0,"delta":"hello"}),
        done(0, first.clone()),
        json!({"type":"response.output_item.done","item":second.clone()}),
        completed(vec![first, second]),
    ])
    .unwrap();
    assert_eq!(items[0].text_content().as_deref(), Some("hello"));
    assert_eq!(items[1].text_content().as_deref(), Some("world"));
}

#[test]
fn generic_reasoning_parts_infer_kind_from_explicit_wire_owner() {
    let output =
        json!({"type":"reasoning","id":"r","content":[{"type":"output_text","text":"thinking"}]});
    let (items, _, _) = assemble(vec![
        added(0, json!({"type":"reasoning","id":"r"})),
        json!({"type":"response.content_part.added","output_index":0,"content_index":0,
            "part":{"type":"output_text","text":"thinking"}}),
        json!({"type":"response.content_part.done","output_index":0,"content_index":0,
            "part":{"type":"output_text","text":"thinking"}}),
        done(0, output.clone()),
        completed(vec![output.clone()]),
    ])
    .unwrap();
    assert_eq!(items[0].reasoning_content().as_deref(), Some("thinking"));
    assert_eq!(items[0].replay.as_ref().unwrap().payload, output);
}

/// A terminal-only item must not steal a same-text streamed item's identity
/// when the terminal array also explicitly retains that streamed item.
#[test]
fn stable_terminal_ids_are_reserved_before_semantic_alias_matching() {
    let retained = message("retained", "same");
    let fresh = message("fresh", "same");
    for output in [
        vec![fresh.clone(), retained.clone()],
        vec![retained.clone(), fresh.clone()],
    ] {
        let (items, _, _) = assemble(vec![
            added(0, retained.clone()),
            done(0, retained.clone()),
            completed(output),
        ])
        .unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id, "retained");
        assert_eq!(items[1].id, "fresh");
    }
}
