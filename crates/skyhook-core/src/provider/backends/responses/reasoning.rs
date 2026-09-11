//! Readable reasoning namespaces and exact native replay reconciliation.
use super::*;

/// Readable content and summaries are independent wire namespaces. Interleaved
/// internal positions preserve both namespaces without exposing native state.
pub(super) fn reasoning_position(position: usize, content: bool) -> Result<usize, ProviderError> {
    position
        .checked_mul(2)
        .and_then(|position| position.checked_add(usize::from(content)))
        .ok_or_else(|| protocol("reasoning part index overflow"))
}

pub(super) fn readable_reasoning(part: &Value, summary: bool) -> Result<&str, ProviderError> {
    match (summary, string(part, "type")?) {
        (true, "summary_text") | (false, "reasoning_text" | "output_text") => {}
        _ => return Err(protocol("unsupported reasoning content part")),
    }
    reasoning_text(part)
}

/// Some readable parts use `reasoning` instead of `text`. Do not guess when
/// both fields supply conflicting values, or silently accept invalid types.
pub(super) fn reasoning_text(value: &Value) -> Result<&str, ProviderError> {
    let field = |key| match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.as_str())),
        _ => Err(protocol("invalid readable reasoning text")),
    };
    match (field("text")?, field("reasoning")?) {
        (Some(text), None) | (None, Some(text)) => Ok(text),
        (Some(text), Some(reasoning)) if text == reasoning => Ok(text),
        _ => Err(protocol("missing or ambiguous readable reasoning text")),
    }
}

pub(super) fn reasoning_parts(
    item: &Value,
) -> Result<BTreeMap<usize, BlockContent>, ProviderError> {
    let mut parts = BTreeMap::new();
    for (field, content) in [("summary", false), ("content", true)] {
        let values = match item.get(field) {
            None | Some(Value::Null) => continue,
            Some(Value::Array(values)) => values,
            _ => return Err(protocol(format!("invalid reasoning {field}"))),
        };
        for (position, part) in values.iter().enumerate() {
            parts.insert(
                reasoning_position(position, content)?,
                BlockContent::Reasoning {
                    text: readable_reasoning(part, !content)?.into(),
                },
            );
        }
    }
    Ok(parts)
}

/// Plaintext is reconciled separately against display blocks. Native replay is
/// always an unchanged received snapshot, not a reconstruction from those blocks.
pub(super) fn native_enrichment(previous: &Value, terminal: &Value) -> bool {
    fn enrich(previous: &Value, terminal: &Value) -> bool {
        if previous == terminal || previous.is_null() {
            return true;
        }
        match (previous.as_object(), terminal.as_object()) {
            (Some(previous), Some(terminal)) => previous.iter().all(|(key, value)| {
                terminal.get(key).is_some_and(|next| {
                    (key == "encrypted_content" && value.as_str() == Some("") && next.is_string())
                        || enrich(value, next)
                })
            }),
            _ => false,
        }
    }
    match (previous.as_object(), terminal.as_object()) {
        (Some(previous), Some(terminal)) => previous.iter().all(|(key, value)| {
            // Identity aliases are checked by end(); status is lifecycle metadata.
            matches!(key.as_str(), "summary" | "content" | "id" | "status")
                || terminal.get(key).is_some_and(|next| {
                    (key == "encrypted_content" && value.as_str() == Some("") && next.is_string())
                        || enrich(value, next)
                })
        }),
        _ => false,
    }
}

impl Decoder {
    pub(super) fn end_reasoning(
        &mut self,
        id: usize,
        native: &Value,
        chunks: &mut Vec<ResponseChunk>,
        terminal: bool,
    ) -> Result<(), ProviderError> {
        let supplied = reasoning_parts(native)?;
        let item = &self.items[&id];
        if let Some(previous) = &item.ended
            && !native_enrichment(previous, native)
        {
            return Err(protocol("conflicting final reasoning state"));
        }
        // A migration alias is only valid while the snapshots present one
        // namespace. Once both are explicit, each needs its own display block.
        // Retain the original block at its original position: close_part still
        // rejects any attempt to rewrite text actually received there. The
        // formerly aliased namespace may now supply its own distinct text.
        self.items
            .get_mut(&id)
            .expect("checked item")
            .reasoning_aliases
            .retain(|position, _| {
                !(supplied.contains_key(position) && supplied.contains_key(&(position ^ 1)))
            });
        for (position, content) in &supplied {
            let item = &self.items[&id];
            let mut target = item
                .reasoning_aliases
                .get(position)
                .copied()
                .unwrap_or(*position);
            if !item.parts.contains_key(&target) && !supplied.contains_key(&(position ^ 1)) {
                // A snapshot can move the same readable text from summary to
                // content (or vice versa). Reuse its live block, but do not
                // collapse independently supplied summary/content namespaces.
                if let Some(other) = item.parts.get(&(position ^ 1)) {
                    let text_matches = match content {
                        BlockContent::Reasoning { text } => other
                            .ended
                            .as_ref()
                            .map_or(other.streamed == *text, |old| old == content),
                        _ => false,
                    };
                    if text_matches {
                        target = position ^ 1;
                        self.items
                            .get_mut(&id)
                            .expect("checked item")
                            .reasoning_aliases
                            .insert(*position, target);
                    }
                }
            }
            self.close_part(id, target, content.clone(), chunks)?;
        }
        // Missing final plaintext is not evidence that live display was wrong.
        // Close received display locally without adding it to the native replay.
        let remaining: Vec<_> = self.items[&id]
            .parts
            .iter()
            .filter(|(_, part)| part.ended.is_none())
            .map(|(position, part)| (*position, part.streamed.clone()))
            .collect();
        for (position, text) in remaining {
            self.close_part(id, position, BlockContent::Reasoning { text }, chunks)?;
        }
        let item = self.items.get_mut(&id).expect("checked item");
        item.ended = Some(native.clone());
        // Keep the display item open until the terminal snapshot: it may supply
        // additional readable content absent from output_item.done.
        if terminal {
            chunks.push(ResponseChunk::ItemEnded {
                id: item.native_id.clone(),
                replay: Some(reasoning_envelope("responses", &self.model, native.clone())),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::protocol::{AssistantItem, ResponseAssembler};

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

    fn event(name: &str, index_key: &str, value_key: &str, value: Value) -> Value {
        let mut event = json!({"type":format!("response.{name}"), "output_index":0, "item_id":"r"});
        event[index_key] = json!(0);
        event[value_key] = value;
        event
    }

    fn terminal(native: Value) -> Value {
        json!({"type":"response.completed", "response":{"status":"completed", "output":[native]}})
    }

    fn snapshot(phase: &str, native: Value) -> Value {
        json!({"type":format!("response.output_item.{phase}"), "output_index":0, "item":native})
    }

    fn item(summary: Value, content: Value) -> Value {
        json!({"type":"reasoning", "id":"r", "summary":summary, "content":content})
    }

    fn readable(summary: Option<&str>, content: Option<&str>) -> Value {
        item(
            summary.map_or(
                Value::Null,
                |text| json!([{"type":"summary_text", "text":text}]),
            ),
            content.map_or(
                Value::Null,
                |text| json!([{"type":"reasoning_text", "text":text}]),
            ),
        )
    }

    fn namespace(content: bool) -> (&'static str, &'static str, &'static str) {
        if content {
            ("reasoning_text.delta", "content_index", "content_0")
        } else {
            ("reasoning_summary_text.delta", "summary_index", "summary_0")
        }
    }

    fn migrated(content: bool, text: &str) -> Value {
        if content {
            readable(Some(text), None)
        } else {
            readable(None, Some(text))
        }
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
        for native in [json!({"type":"reasoning", "id":"r"}), readable(None, None)] {
            let items = assemble(vec![terminal(native.clone())]);
            assert!(items[0].blocks.is_empty());
            assert_eq!(items[0].replay.as_ref().unwrap().payload, native);
        }
    }

    #[test]
    fn namespace_migration_reuses_live_block_and_preserves_exact_replay() {
        for stream_content in [false, true] {
            let (name, index_key, block) = namespace(stream_content);
            let native = migrated(stream_content, "same");
            let items = assemble(vec![
                snapshot("added", readable(None, None)),
                event(name, index_key, "delta", json!("same")),
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
            let mut events = vec![snapshot("added", readable(None, None))];
            if !final_has_text {
                events.push(event(
                    "reasoning_text.delta",
                    "content_index",
                    "delta",
                    json!("visible"),
                ));
            }
            events.extend([
                snapshot("done", readable(None, None)),
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
                    snapshot("added", readable(None, None)),
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
    fn conflicting_supplied_plaintext_and_aliases_fail() {
        for conflicting in [
            json!({"type":"reasoning_text", "text":"different"}),
            json!({"type":"reasoning_text", "text":"visible", "reasoning":"different"}),
            json!({"type":"reasoning_text", "text":7}),
        ] {
            let mut decoder = Decoder::new("model".into());
            decoder
                .feed(snapshot("added", readable(None, None)))
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
    }

    #[test]
    fn done_summary_can_move_to_terminal_content_without_duplicate_display() {
        let done = item(json!([{"type":"summary_text", "text":"same"}]), Value::Null);
        let terminal_item = item(
            Value::Null,
            json!([{"type":"reasoning_text", "text":"same"}]),
        );
        let items = assemble(vec![
            snapshot("added", readable(None, None)),
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
            let (name, index_key, _) = namespace(stream_content);
            // Equal and distinct text both create a second namespace, at either
            // item.done or terminal; repeated snapshots must stay idempotent.
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
                let native = readable(Some(summary), Some(content));
                let prior = migrated(stream_content, "original");
                let mut events = vec![
                    snapshot("added", readable(None, None)),
                    event(name, index_key, "delta", json!("original")),
                    snapshot("done", prior.clone()),
                    snapshot("done", prior),
                ];
                if split_at_done {
                    events.extend([
                        snapshot("done", native.clone()),
                        snapshot("done", native.clone()),
                    ]);
                }
                events.push(terminal(native.clone()));
                let items = assemble(events);
                let display: Vec<_> = items[0]
                    .blocks
                    .iter()
                    .map(|block| {
                        (
                            block.id.as_str(),
                            block.content.reasoning_content().unwrap(),
                        )
                    })
                    .collect();
                assert_eq!(display, [("summary_0", summary), ("content_0", content)]);
                assert_eq!(items[0].replay.as_ref().unwrap().payload, native);
            }
        }
    }

    #[test]
    fn splitting_migrated_namespaces_cannot_rewrite_original_streamed_text() {
        for stream_content in [false, true] {
            let (name, index_key, _) = namespace(stream_content);
            let (summary, content) = if stream_content {
                ("original", "conflicting")
            } else {
                ("conflicting", "original")
            };
            let mut decoder = Decoder::new("model".into());
            for value in [
                snapshot("added", readable(None, None)),
                event(name, index_key, "delta", json!("original")),
                snapshot("done", migrated(stream_content, "original")),
            ] {
                decoder.feed(value).unwrap();
            }
            assert!(
                decoder
                    .feed(terminal(readable(Some(summary), Some(content))))
                    .is_err()
            );
        }
    }

    #[test]
    fn codex_omitted_output_uses_done_plaintext_without_accepting_conflicts() {
        // Normal terminal snapshots and live text are covered above. Codex may
        // omit output entirely, but received done text must still agree.
        for (content, done_text, valid) in [
            (None, "native", true),
            (Some("native"), "native", true),
            (Some("different"), "native", false),
            (None, "changed", false),
        ] {
            let mut decoder = Decoder::codex("model".into());
            decoder
                .feed(snapshot("added", readable(None, None)))
                .unwrap();
            decoder
                .feed(event(
                    "reasoning_text.delta",
                    "content_index",
                    "delta",
                    json!("native"),
                ))
                .unwrap();
            let result = decoder.feed(event("reasoning_text.done", "content_index", "text", json!(done_text)))
                .and_then(|_| decoder.feed(snapshot("done", readable(None, content))))
                .and_then(|_| decoder.feed(json!({"type":"response.completed", "response":{"status":"completed", "output":[]}})));
            assert_eq!(
                result.is_ok(),
                valid,
                "content={content:?}, done={done_text}: {result:?}"
            );
        }
    }

    #[test]
    fn terminal_reasoning_may_enrich_but_not_replace_native_state() {
        for placeholder in [Value::Null, json!("")] {
            let old = json!({"type":"reasoning", "id":"r", "summary":[], "encrypted_content":placeholder});
            let final_item = json!({"type":"reasoning", "id":"r", "summary":[], "encrypted_content":"ciphertext"});
            let mut decoder = Decoder::new("gpt-5".into());
            decoder.feed(snapshot("added", old.clone())).unwrap();
            decoder.feed(snapshot("done", old)).unwrap();
            let chunks = decoder.feed(terminal(final_item.clone())).unwrap();
            assert!(chunks.iter().any(|chunk| matches!(chunk,
                ResponseChunk::ItemEnded { replay: Some(replay), .. } if replay.payload == final_item)));
        }
        let mut old = readable(Some("first"), None);
        old["encrypted_content"] = json!("secret");
        for final_item in [
            {
                let mut v = old.clone();
                v["encrypted_content"] = json!("different");
                v
            },
            {
                let mut v = old.clone();
                v.as_object_mut().unwrap().remove("encrypted_content");
                v
            },
            {
                let mut v = old.clone();
                v["summary"][0]["text"] = json!("different");
                v
            },
        ] {
            let mut decoder = Decoder::new("gpt-5".into());
            decoder.feed(snapshot("added", old.clone())).unwrap();
            decoder.feed(snapshot("done", old.clone())).unwrap();
            assert!(decoder.feed(terminal(final_item)).is_err());
        }
    }
}
