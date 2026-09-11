//! Readable reasoning is display content, not a source for native replay.
use super::*;
use crate::provider::protocol::{AssistantItem, ResponseAssembler};

fn item(summary: Value, content: Value) -> Value {
    json!({"type":"reasoning", "id":"r", "summary":summary, "content":content})
}

fn snapshot(phase: &str, native: Value) -> Value {
    json!({"type":format!("response.output_item.{phase}"), "output_index":0, "item":native})
}

fn terminal(native: Value) -> Value {
    json!({"type":"response.completed", "response":{"status":"completed", "output":[native]}})
}

fn event(name: &str, index_key: &str, value_key: &str, value: Value) -> Value {
    let mut event = json!({"type":format!("response.{name}"), "output_index":0, "item_id":"r"});
    event[index_key] = json!(0);
    event[value_key] = value;
    event
}

fn assemble(events: Vec<Value>) -> Vec<AssistantItem> {
    let mut decoder = Decoder::new("model".into());
    let mut assembler = ResponseAssembler::default();
    for event in events {
        for chunk in decoder.feed(event).unwrap() {
            assembler.push(&chunk).unwrap();
        }
    }
    assembler.finish().unwrap().0
}

#[test]
fn native_text_is_live_and_independent_of_summary() {
    let mut decoder = Decoder::new("model".into());
    let mut assembler = ResponseAssembler::default();
    for chunk in decoder
        .feed(snapshot("added", item(json!([]), Value::Null)))
        .unwrap()
    {
        assembler.push(&chunk).unwrap();
    }
    let chunks = decoder
        .feed(event(
            "reasoning_text.delta",
            "content_index",
            "delta",
            json!("raw"),
        ))
        .unwrap();
    assert!(chunks.iter().any(|chunk| matches!(chunk,
        ResponseChunk::BlockStarted { id, kind: BlockKind::Reasoning, .. } if id == "content_0")));
    assert!(chunks.iter().any(|chunk| matches!(chunk,
        ResponseChunk::BlockDelta { block, delta: ContentDelta::Text(text), .. }
            if block == "content_0" && text == "raw")));
    for chunk in chunks {
        assembler.push(&chunk).unwrap();
    }
    let native = item(
        json!([{"type":"summary_text", "text":"brief"}]),
        json!([{"type":"reasoning_text", "text":"raw"}]),
    );
    for value in [
        event(
            "reasoning_summary_text.delta",
            "summary_index",
            "delta",
            json!("brief"),
        ),
        event("reasoning_text.done", "content_index", "text", json!("raw")),
        snapshot("done", native.clone()),
        snapshot("done", native.clone()),
        terminal(native.clone()),
    ] {
        for chunk in decoder.feed(value).unwrap() {
            assembler.push(&chunk).unwrap();
        }
    }
    let items = assembler.finish().unwrap().0;
    assert_eq!(items[0].blocks.len(), 2);
    assert_eq!(items[0].blocks[0].id, "summary_0");
    assert_eq!(items[0].blocks[1].id, "content_0");
    assert_eq!(items[0].replay.as_ref().unwrap().payload, native);
}

#[test]
fn terminal_only_readable_content_and_optional_arrays() {
    for content_type in ["reasoning_text", "output_text"] {
        for summary in [None, Some(Value::Null), Some(json!([]))] {
            let mut native = json!({"type":"reasoning", "id":"r",
                "content":[{"type":content_type, "text":"visible"}],
                "encrypted_content":"opaque", "future":{"signature":"untouched"}});
            if let Some(summary) = summary {
                native["summary"] = summary;
            }
            let items = assemble(vec![terminal(native.clone())]);
            assert_eq!(items[0].blocks.len(), 1);
            assert_eq!(items[0].blocks[0].id, "content_0");
            assert_eq!(
                items[0].blocks[0].content,
                BlockContent::Reasoning {
                    text: "visible".into()
                }
            );
            assert_eq!(items[0].replay.as_ref().unwrap().payload, native);
        }
    }
    for native in [
        json!({"type":"reasoning", "id":"r"}),
        item(Value::Null, Value::Null),
    ] {
        let items = assemble(vec![terminal(native.clone())]);
        assert!(items[0].blocks.is_empty());
        assert_eq!(items[0].replay.as_ref().unwrap().payload, native);
    }
}

#[test]
fn namespace_migration_reuses_live_block_and_preserves_exact_replay() {
    for stream_content in [false, true] {
        let (name, index_key, block) = if stream_content {
            ("reasoning_text.delta", "content_index", "content_0")
        } else {
            ("reasoning_summary_text.delta", "summary_index", "summary_0")
        };
        let native = if stream_content {
            item(json!([{"type":"summary_text", "text":"same"}]), Value::Null)
        } else {
            item(Value::Null, json!([{"type":"output_text", "text":"same"}]))
        };
        let items = assemble(vec![
            snapshot("added", item(Value::Null, Value::Null)),
            event(name, index_key, "delta", json!("same")),
            snapshot("done", native.clone()),
            snapshot("done", native.clone()),
            terminal(native.clone()),
        ]);
        assert_eq!(items[0].blocks.len(), 1);
        assert_eq!(items[0].blocks[0].id, block);
        assert_eq!(items[0].replay.as_ref().unwrap().payload, native);
    }
}

#[test]
fn terminal_can_add_plaintext_after_done_or_omit_received_display() {
    for final_has_text in [false, true] {
        let mut native = json!({"type":"reasoning", "id":"r", "encrypted_content":"cipher"});
        if final_has_text {
            native["content"] = json!([{"type":"reasoning_text", "text":"visible"}]);
        }
        let mut events = vec![snapshot("added", item(Value::Null, Value::Null))];
        if !final_has_text {
            events.push(event(
                "reasoning_text.delta",
                "content_index",
                "delta",
                json!("visible"),
            ));
        }
        events.extend([
            snapshot("done", item(Value::Null, Value::Null)),
            terminal(native.clone()),
        ]);
        let items = assemble(events);
        assert_eq!(items[0].blocks.len(), 1);
        assert_eq!(
            items[0].blocks[0].content,
            BlockContent::Reasoning {
                text: "visible".into()
            }
        );
        assert_eq!(items[0].replay.as_ref().unwrap().payload, native);
    }
}

#[test]
fn readable_part_events_accept_unambiguous_reasoning_alias() {
    for family in ["reasoning_part", "content_part"] {
        for part_type in ["reasoning_text", "output_text"] {
            let native = item(
                Value::Null,
                json!([{"type":part_type,"reasoning":"visible"}]),
            );
            let items = assemble(vec![
                snapshot("added", item(Value::Null, Value::Null)),
                event(
                    &format!("{family}.added"),
                    "content_index",
                    "part",
                    json!({"type":part_type,"reasoning":"vis"}),
                ),
                event(
                    "reasoning_text.delta",
                    "content_index",
                    "delta",
                    json!("ible"),
                ),
                event(
                    &format!("{family}.done"),
                    "content_index",
                    "part",
                    json!({"type":part_type,"reasoning":"visible"}),
                ),
                event(
                    &format!("{family}.done"),
                    "content_index",
                    "part",
                    json!({"type":part_type,"text":"visible","reasoning":"visible"}),
                ),
                snapshot("done", native.clone()),
                terminal(native.clone()),
            ]);
            assert_eq!(items[0].blocks.len(), 1);
            assert_eq!(
                items[0].blocks[0].content,
                BlockContent::Reasoning {
                    text: "visible".into()
                }
            );
            assert_eq!(items[0].replay.as_ref().unwrap().payload, native);
        }
    }
}

#[test]
fn conflicting_supplied_plaintext_ciphertext_and_aliases_fail() {
    for conflicting in [
        json!({"type":"reasoning_text", "text":"different"}),
        json!({"type":"reasoning_text", "text":"visible", "reasoning":"different"}),
        json!({"type":"reasoning_text", "text":7}),
    ] {
        let mut decoder = Decoder::new("model".into());
        decoder
            .feed(snapshot("added", item(Value::Null, Value::Null)))
            .unwrap();
        decoder
            .feed(event(
                "reasoning_text.delta",
                "content_index",
                "delta",
                json!("visible"),
            ))
            .unwrap();
        assert!(
            decoder
                .feed(terminal(item(Value::Null, json!([conflicting]))))
                .is_err()
        );
    }
    let mut decoder = Decoder::new("model".into());
    let native = json!({"type":"reasoning", "id":"r", "encrypted_content":"original"});
    decoder.feed(snapshot("added", native.clone())).unwrap();
    decoder.feed(snapshot("done", native)).unwrap();
    assert!(
        decoder
            .feed(terminal(
                json!({"type":"reasoning", "id":"r", "encrypted_content":"changed"})
            ))
            .is_err()
    );
}

#[test]
fn done_summary_can_move_to_terminal_content_without_duplicate_display() {
    let done = item(json!([{"type":"summary_text", "text":"same"}]), Value::Null);
    let terminal_item = item(
        Value::Null,
        json!([{"type":"reasoning_text", "text":"same"}]),
    );
    let items = assemble(vec![
        snapshot("added", item(Value::Null, Value::Null)),
        snapshot("done", done.clone()),
        snapshot("done", done),
        terminal(terminal_item.clone()),
    ]);
    assert_eq!(items[0].blocks.len(), 1);
    assert_eq!(items[0].blocks[0].id, "summary_0");
    assert_eq!(items[0].replay.as_ref().unwrap().payload, terminal_item);
}

#[test]
fn explicitly_supplied_namespaces_remain_distinct_even_for_equal_text() {
    let native = item(
        json!([{"type":"summary_text", "text":"same"}, {"type":"summary_text", "text":"brief"}]),
        json!([{"type":"reasoning_text", "text":"same"}, {"type":"output_text", "text":"details"}]),
    );
    let items = assemble(vec![terminal(native.clone())]);
    let ids: Vec<_> = items[0]
        .blocks
        .iter()
        .map(|block| block.id.as_str())
        .collect();
    assert_eq!(ids, ["summary_0", "content_0", "summary_1", "content_1"]);
    assert_eq!(items[0].replay.as_ref().unwrap().payload, native);
}

#[test]
fn migrated_namespace_splits_when_both_namespaces_become_explicit() {
    for stream_content in [false, true] {
        let (name, index_key, original_block) = if stream_content {
            ("reasoning_text.delta", "content_index", "content_0")
        } else {
            ("reasoning_summary_text.delta", "summary_index", "summary_0")
        };
        let migrated = if stream_content {
            item(
                json!([{"type":"summary_text", "text":"original"}]),
                Value::Null,
            )
        } else {
            item(
                Value::Null,
                json!([{"type":"reasoning_text", "text":"original"}]),
            )
        };
        // Equal text still denotes two namespaces when both are explicit.
        // Distinct new text belongs to the newly explicit namespace, not to
        // the stable block that originally received the stream.
        for (new_text, split_at_done) in [
            ("original", false),
            ("new namespace", false),
            ("original", true),
            ("new namespace", true),
        ] {
            let (summary, content) = if stream_content {
                (new_text, "original")
            } else {
                ("original", new_text)
            };
            let native = item(
                json!([{"type":"summary_text", "text":summary}]),
                json!([{"type":"reasoning_text", "text":content}]),
            );
            let mut decoder = Decoder::new("model".into());
            let mut assembler = ResponseAssembler::default();
            let mut started = Vec::new();
            let mut events = vec![
                snapshot("added", item(Value::Null, Value::Null)),
                event(name, index_key, "delta", json!("original")),
                snapshot("done", migrated.clone()),
                snapshot("done", migrated.clone()),
            ];
            if split_at_done {
                events.extend([
                    snapshot("done", native.clone()),
                    snapshot("done", native.clone()),
                ]);
            }
            events.push(terminal(native.clone()));
            for value in events {
                for chunk in decoder.feed(value).unwrap() {
                    if let ResponseChunk::BlockStarted { id, .. } = &chunk {
                        started.push(id.clone());
                    }
                    assembler.push(&chunk).unwrap();
                }
            }
            assert_eq!(started.len(), 2);
            assert_eq!(started[0], original_block);
            assert_ne!(started[0], started[1]);
            let items = assembler.finish().unwrap().0;
            assert_eq!(items[0].blocks.len(), 2);
            assert_eq!(items[0].blocks[0].id, "summary_0");
            assert_eq!(
                items[0].blocks[0].content,
                BlockContent::Reasoning {
                    text: summary.into()
                }
            );
            assert_eq!(items[0].blocks[1].id, "content_0");
            assert_eq!(
                items[0].blocks[1].content,
                BlockContent::Reasoning {
                    text: content.into()
                }
            );
            assert_eq!(items[0].replay.as_ref().unwrap().payload, native);
        }
    }
}

#[test]
fn splitting_migrated_namespaces_cannot_rewrite_original_streamed_text() {
    for stream_content in [false, true] {
        let (name, index_key) = if stream_content {
            ("reasoning_text.delta", "content_index")
        } else {
            ("reasoning_summary_text.delta", "summary_index")
        };
        let migrated = if stream_content {
            item(
                json!([{"type":"summary_text", "text":"original"}]),
                Value::Null,
            )
        } else {
            item(
                Value::Null,
                json!([{"type":"reasoning_text", "text":"original"}]),
            )
        };
        let (summary, content) = if stream_content {
            ("original", "conflicting")
        } else {
            ("conflicting", "original")
        };
        let mut decoder = Decoder::new("model".into());
        for value in [
            snapshot("added", item(Value::Null, Value::Null)),
            event(name, index_key, "delta", json!("original")),
            snapshot("done", migrated),
        ] {
            decoder.feed(value).unwrap();
        }
        assert!(
            decoder
                .feed(terminal(item(
                    json!([{"type":"summary_text", "text":summary}]),
                    json!([{"type":"reasoning_text", "text":content}]),
                )))
                .is_err()
        );
    }
}
