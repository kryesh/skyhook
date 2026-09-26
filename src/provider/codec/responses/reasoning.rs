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

pub(super) fn reasoning_parts(item: &Value) -> Result<BTreeMap<usize, Content>, ProviderError> {
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
                Content::Reasoning {
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
    pub(super) fn end_reasoning(&mut self, id: usize, native: &Value) -> Result<(), ProviderError> {
        let supplied = reasoning_parts(native)?;
        let item = &self.items[&id];
        if let Some(previous) = item.snapshot()
            && !native_enrichment(previous, native)
        {
            return Err(protocol("conflicting final reasoning state"));
        }
        // A migration alias is only valid while the snapshots present one
        // namespace. Once both are explicit, each needs its own display block.
        self.items
            .get_mut(&id)
            .expect("checked item")
            .reasoning_mut()?
            .aliases
            .retain(|position, _| {
                !(supplied.contains_key(position) && supplied.contains_key(&(position ^ 1)))
            });
        for (position, content) in &supplied {
            let sibling = position ^ 1;
            let is_content = position & 1 == 1;
            let reasoning = self.items[&id].reasoning()?;
            // Some servers send one reasoning text as both summary and content;
            // an unstreamed namespace does not repeat its sibling's text.
            if supplied.get(&sibling) == Some(content)
                && !reasoning.parts.contains_key(position)
                && (is_content || reasoning.parts.contains_key(&sibling))
            {
                continue;
            }
            let mut target = reasoning
                .aliases
                .get(position)
                .copied()
                .unwrap_or(*position);
            if !reasoning.parts.contains_key(&target) && !supplied.contains_key(&sibling) {
                // A snapshot can move the same readable text from summary to
                // content (or vice versa). Reuse its live block, but do not
                // collapse independently supplied summary/content namespaces.
                if let Some(other) = reasoning.parts.get(&sibling) {
                    let text_matches = match content {
                        Content::Reasoning { text } => other
                            .ended()
                            .map_or(other.streamed() == text, |old| old == content),
                        _ => false,
                    };
                    if text_matches {
                        target = sibling;
                        self.items
                            .get_mut(&id)
                            .expect("checked item")
                            .reasoning_mut()?
                            .aliases
                            .insert(*position, target);
                    }
                }
            }
            self.close_part(id, target, content.clone())?;
        }
        // Missing final plaintext is not evidence that live display was wrong.
        // Close received display locally without adding it to the native replay.
        let remaining: Vec<_> = self.items[&id]
            .reasoning()?
            .parts
            .iter()
            .filter(|(_, part)| part.ended().is_none())
            .map(|(position, part)| (*position, part.streamed().to_owned()))
            .collect();
        for (position, text) in remaining {
            self.close_part(id, position, Content::Reasoning { text })?;
        }
        // The latest snapshot is the replay; the terminal one may enrich it.
        let item = self.items.get_mut(&id).expect("checked item");
        item.reasoning_mut()?.snapshot = Some(native.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{assemble, decoder, reduce};
    use super::*;

    fn event(name: &str, index_key: &str, value_key: &str, value: Value) -> Value {
        let mut event = json!({"type":format!("response.{name}"), "output_index":0, "item_id":"r"});
        event[index_key] = json!(0);
        event[value_key] = value;
        event
    }

    fn content_delta(text: &str) -> Value {
        event(
            "reasoning_text.delta",
            "content_index",
            "delta",
            json!(text),
        )
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
        let parts = |kind, text: Option<&str>| {
            text.map_or(Value::Null, |text| json!([{"type":kind, "text":text}]))
        };
        item(
            parts("summary_text", summary),
            parts("reasoning_text", content),
        )
    }

    fn empty() -> Value {
        readable(None, None)
    }

    fn namespace(content: bool) -> Value {
        if content {
            content_delta("original")
        } else {
            event(
                "reasoning_summary_text.delta",
                "summary_index",
                "delta",
                json!("original"),
            )
        }
    }

    /// The done snapshot after streaming into one namespace moves text to the other.
    fn migrated(content: bool, text: &str) -> Value {
        if content {
            readable(Some(text), None)
        } else {
            readable(None, Some(text))
        }
    }

    fn feed(decoder: &mut Decoder, events: Vec<Value>) -> Result<(), ProviderError> {
        events
            .into_iter()
            .try_for_each(|event| decoder.feed(event).map(drop))
    }

    /// The single reasoning item's (block id, text) display in position order.
    fn display(item: &AssistantItem) -> Vec<(&str, &str)> {
        let AssistantItem::Reasoning { blocks, .. } = item else {
            panic!("expected a reasoning item: {item:?}")
        };
        blocks
            .iter()
            .map(|block| (block.id.as_str(), block.text.as_str()))
            .collect()
    }

    /// Asserts the single item's exact replay payload and its (block id, text) display.
    fn assert_display(events: Vec<Value>, native: &Value, expected: &[(&str, &str)]) {
        let reduced = assemble(events).unwrap();
        let item = &reduced.items()[0];
        assert_eq!(item.replay().unwrap().payload, *native);
        assert_eq!(display(item), expected);
    }

    #[test]
    fn native_text_is_live_and_independent_of_summary() {
        let mut decoder = decoder();
        let mut events = Vec::new();
        let mut push = |event| {
            let emitted = decoder.feed(event).unwrap();
            events.extend(emitted.clone());
            emitted
        };
        push(snapshot("added", item(json!([]), Value::Null)));
        let live = push(content_delta("raw"));
        assert!(live.iter().any(|event| matches!(event,
            ResponseEvent::Delta { block, kind: ItemKind::Reasoning, text }
                if block.block.as_str() == "content_0" && text == "raw")));
        let native = readable(Some("brief"), Some("raw"));
        push(event(
            "reasoning_summary_text.delta",
            "summary_index",
            "delta",
            json!("brief"),
        ));
        push(event(
            "reasoning_text.done",
            "content_index",
            "text",
            json!("raw"),
        ));
        push(snapshot("done", native.clone()));
        push(snapshot("done", native.clone()));
        push(terminal(native.clone()));
        let reduced = reduce(events);
        assert_eq!(reduced.streamed(ItemKind::Reasoning), ["raw", "brief"]);
        let item = &reduced.items()[0];
        let ids: Vec<_> = display(item).into_iter().map(|(id, _)| id).collect();
        assert_eq!(ids, ["summary_0", "content_0"]);
        assert_eq!(item.replay().unwrap().payload, native);
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
                assert_display(
                    vec![terminal(native.clone())],
                    &native,
                    &[("content_0", "visible")],
                );
            }
        }
        for native in [json!({"type":"reasoning", "id":"r"}), empty()] {
            assert_display(vec![terminal(native.clone())], &native, &[]);
        }
        let native = item(
            json!([{"type":"summary_text", "text":"same"}, {"type":"summary_text", "text":"brief"}]),
            json!([{"type":"reasoning_text", "text":"same"}, {"type":"output_text", "text":"details"}]),
        );
        assert_display(
            vec![terminal(native.clone())],
            &native,
            &[
                ("summary_0", "same"),
                ("summary_1", "brief"),
                ("content_1", "details"),
            ],
        );
    }

    #[test]
    fn namespace_migration_reuses_live_block_and_preserves_exact_replay() {
        for (content, block) in [(false, "summary_0"), (true, "content_0")] {
            let native = migrated(content, "original");
            let events = vec![
                snapshot("added", empty()),
                namespace(content),
                snapshot("done", native.clone()),
                terminal(native.clone()),
            ];
            assert_display(events, &native, &[(block, "original")]);
        }
        // A done summary can move to terminal content without duplicate display.
        let done = readable(Some("same"), None);
        let native = readable(None, Some("same"));
        let events = vec![
            snapshot("added", empty()),
            snapshot("done", done.clone()),
            snapshot("done", done),
            terminal(native.clone()),
        ];
        assert_display(events, &native, &[("summary_0", "same")]);
    }

    #[test]
    fn terminal_can_add_plaintext_after_done_or_omit_received_display() {
        for final_has_text in [false, true] {
            let mut native = json!({"type":"reasoning", "id":"r", "encrypted_content":"cipher"});
            let mut events = vec![snapshot("added", empty())];
            if final_has_text {
                native["content"] = json!([{"type":"reasoning_text", "text":"visible"}]);
            } else {
                events.push(content_delta("visible"));
            }
            events.extend([snapshot("done", empty()), terminal(native.clone())]);
            assert_display(events, &native, &[("content_0", "visible")]);
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
                let part = |phase: &str, part: Value| {
                    event(&format!("{family}.{phase}"), "content_index", "part", part)
                };
                let events = vec![
                    snapshot("added", empty()),
                    part("added", json!({"type":part_type,"reasoning":"vis"})),
                    content_delta("ible"),
                    part("done", json!({"type":part_type,"reasoning":"visible"})),
                    part(
                        "done",
                        json!({"type":part_type,"text":"visible","reasoning":"visible"}),
                    ),
                    snapshot("done", native.clone()),
                    terminal(native.clone()),
                ];
                assert_display(events, &native, &[("content_0", "visible")]);
            }
        }
    }

    #[test]
    fn final_plaintext_supersedes_deltas_but_conflicting_aliases_fail() {
        for (conflicting, valid) in [
            // The final snapshot is authoritative over streamed deltas.
            (json!({"type":"reasoning_text", "text":"different"}), true),
            // Aliases within one snapshot must still agree and be text.
            (
                json!({"type":"reasoning_text", "text":"visible", "reasoning":"different"}),
                false,
            ),
            (json!({"type":"reasoning_text", "text":7}), false),
        ] {
            let mut decoder = decoder();
            feed(
                &mut decoder,
                vec![snapshot("added", empty()), content_delta("visible")],
            )
            .unwrap();
            let result = decoder.feed(terminal(item(Value::Null, json!([conflicting.clone()]))));
            assert_eq!(result.is_ok(), valid, "{conflicting}");
        }
    }

    #[test]
    fn migrated_namespace_splits_when_both_namespaces_become_explicit() {
        for content in [false, true] {
            // Distinct text adds a namespace at item.done or terminal; equal text does not.
            for (new_text, split_at_done) in [
                ("original", false),
                ("new namespace", false),
                ("original", true),
                ("new namespace", true),
            ] {
                let (summary, text) = if content {
                    (new_text, "original")
                } else {
                    ("original", new_text)
                };
                let native = readable(Some(summary), Some(text));
                let prior = migrated(content, "original");
                let mut events = vec![
                    snapshot("added", empty()),
                    namespace(content),
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
                let expected: &[(&str, &str)] = if summary == text {
                    if content {
                        &[("content_0", text)]
                    } else {
                        &[("summary_0", summary)]
                    }
                } else {
                    &[("summary_0", summary), ("content_0", text)]
                };
                assert_display(events, &native, expected);
            }
            // Splitting cannot rewrite the original streamed text.
            let (summary, text) = if content {
                ("original", "conflicting")
            } else {
                ("conflicting", "original")
            };
            let mut decoder = decoder();
            let events = vec![
                snapshot("added", empty()),
                namespace(content),
                snapshot("done", migrated(content, "original")),
            ];
            feed(&mut decoder, events).unwrap();
            assert!(
                decoder
                    .feed(terminal(readable(Some(summary), Some(text))))
                    .is_err()
            );
        }
    }

    #[test]
    fn codex_omitted_output_uses_done_plaintext_without_accepting_conflicts() {
        // Codex may omit terminal output entirely. Done text supersedes deltas,
        // but two final texts must agree.
        for (content, done_text, valid) in [
            (None, "native", true),
            (Some("native"), "native", true),
            (Some("different"), "native", false),
            (None, "changed", true),
        ] {
            let mut decoder = Decoder::new(
                "model".into(),
                super::super::tests::scope(),
                &super::super::tests::streamed_only(),
                ErrorSignals::NONE,
            );
            let result = feed(
                &mut decoder,
                vec![
                    snapshot("added", empty()),
                    content_delta("native"),
                    event(
                        "reasoning_text.done",
                        "content_index",
                        "text",
                        json!(done_text),
                    ),
                    snapshot("done", readable(None, content)),
                    json!({"type":"response.completed", "response":{"status":"completed", "output":[]}}),
                ],
            );
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
            let mut decoder = decoder();
            feed(
                &mut decoder,
                vec![snapshot("added", old.clone()), snapshot("done", old)],
            )
            .unwrap();
            let reduced = reduce(decoder.feed(terminal(final_item.clone())).unwrap());
            assert_eq!(reduced.items()[0].replay().unwrap().payload, final_item);
        }
        let mut old = readable(Some("first"), None);
        old["encrypted_content"] = json!("secret");
        let changes: [fn(&mut Value); 3] = [
            |item| item["encrypted_content"] = json!("different"),
            |item| drop(item.as_object_mut().unwrap().remove("encrypted_content")),
            |item| item["summary"][0]["text"] = json!("different"),
        ];
        for change in changes {
            let mut final_item = old.clone();
            change(&mut final_item);
            let mut decoder = decoder();
            feed(
                &mut decoder,
                vec![
                    snapshot("added", old.clone()),
                    snapshot("done", old.clone()),
                ],
            )
            .unwrap();
            assert!(decoder.feed(terminal(final_item)).is_err());
        }
    }

    /// A proxy streams only the summary, then its final snapshot repeats the
    /// same text as reasoning content. It is displayed once.
    #[test]
    fn a_summary_repeated_as_content_is_displayed_once() {
        let summary_event = |name: &str, value_key: &str, value: Value| {
            event(name, "summary_index", value_key, value)
        };
        let text = "User wants me to call run.\n";
        let native = readable(Some(text), Some(text));
        assert_display(
            vec![
                snapshot("added", empty()),
                summary_event(
                    "reasoning_summary_part.added",
                    "part",
                    json!({"type":"summary_text", "text":""}),
                ),
                summary_event("reasoning_summary_text.delta", "delta", json!(text)),
                summary_event("reasoning_summary_text.done", "text", json!(text)),
                snapshot("done", native.clone()),
                terminal(native.clone()),
            ],
            &native,
            &[("summary_0", text)],
        );
    }
}
