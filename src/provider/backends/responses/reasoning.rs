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
    /// Some servers finish a reasoning item only when the whole response ends.
    /// Once a later item starts, earlier reasoning is complete for display, so
    /// its streamed blocks close now; the item still awaits its snapshot.
    pub(super) fn close_superseded_reasoning(
        &mut self,
        next: usize,
        chunks: &mut Vec<ResponseChunk>,
    ) -> Result<(), ProviderError> {
        let open: Vec<(usize, usize, String)> = self
            .items
            .iter()
            .filter(|(id, item)| **id != next && item.snapshot().is_none())
            .filter_map(|(id, item)| Some((*id, item.reasoning().ok()?)))
            .flat_map(|(id, reasoning)| {
                reasoning
                    .parts
                    .iter()
                    .filter(|(_, part)| part.ended().is_none() && !part.streamed().is_empty())
                    .map(move |(position, part)| (id, *position, part.streamed().to_owned()))
            })
            .collect();
        for (id, position, text) in open {
            self.close_part(id, position, BlockContent::Reasoning { text }, chunks)?;
            self.items
                .get_mut(&id)
                .expect("open item")
                .reasoning_mut()?
                .shown_early
                .insert(position);
        }
        Ok(())
    }

    /// Whether a reasoning part (or the part its namespace migrated to) was
    /// closed for display when a later item started.
    pub(super) fn shown_early(&self, id: usize, position: usize) -> bool {
        self.items[&id].reasoning().is_ok_and(|reasoning| {
            let target = reasoning
                .aliases
                .get(&position)
                .copied()
                .unwrap_or(position);
            reasoning.shown_early.contains(&target)
        })
    }

    pub(super) fn end_reasoning(
        &mut self,
        id: usize,
        native: &Value,
        chunks: &mut Vec<ResponseChunk>,
        terminal: bool,
    ) -> Result<(), ProviderError> {
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
                        BlockContent::Reasoning { text } => other
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
            // Displayed when a later item started; the snapshot is the replay.
            if self.shown_early(id, target) {
                continue;
            }
            self.close_part(id, target, content.clone(), chunks)?;
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
            self.close_part(id, position, BlockContent::Reasoning { text }, chunks)?;
        }
        let item = self.items.get_mut(&id).expect("checked item");
        item.reasoning_mut()?.snapshot = Some(native.clone());
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
    use crate::provider::protocol::ResponseAssembler;

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

    /// Asserts the single item's exact replay payload and its (block id, text) display.
    fn assert_display(events: Vec<Value>, native: &Value, expected: &[(&str, &str)]) {
        let items = super::fixtures::assemble(events).unwrap().0;
        assert_eq!(items[0].replay.as_ref().unwrap().payload, *native);
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
        assert_eq!(display, expected);
    }

    #[test]
    fn native_text_is_live_and_independent_of_summary() {
        let mut decoder = Decoder::new("model".into());
        let mut assembler = ResponseAssembler::default();
        let mut push = |event| {
            let chunks = decoder.feed(event).unwrap();
            chunks
                .iter()
                .for_each(|chunk| assembler.push(chunk).unwrap());
            chunks
        };
        push(snapshot("added", item(json!([]), Value::Null)));
        let chunks = push(content_delta("raw"));
        assert!(chunks.iter().any(|chunk| matches!(chunk,
            ResponseChunk::BlockStarted { id, kind: BlockKind::Reasoning, .. } if id == "content_0")));
        assert!(chunks.iter().any(|chunk| matches!(chunk,
            ResponseChunk::BlockDelta { block, delta: ContentDelta::Text(text), .. }
                if block == "content_0" && text == "raw")));
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
        let items = assembler.finish().unwrap().0;
        let ids: Vec<_> = items[0]
            .blocks
            .iter()
            .map(|block| block.id.as_str())
            .collect();
        assert_eq!(ids, ["summary_0", "content_0"]);
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
            let mut decoder = Decoder::new("model".into());
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
            let mut decoder = Decoder::new("model".into());
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
            let mut decoder = Decoder::codex("model".into());
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
            let mut decoder = Decoder::new("gpt-5".into());
            feed(
                &mut decoder,
                vec![snapshot("added", old.clone()), snapshot("done", old)],
            )
            .unwrap();
            let chunks = decoder.feed(terminal(final_item.clone())).unwrap();
            assert!(chunks.iter().any(|chunk| matches!(chunk,
                ResponseChunk::ItemEnded { replay: Some(replay), .. } if replay.payload == final_item)));
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
            let mut decoder = Decoder::new("gpt-5".into());
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

    /// A server that sends reasoning's `output_item.done` only at the end of
    /// the response: the reasoning display closes when the answer starts.
    #[test]
    fn reasoning_display_closes_when_a_later_item_starts() {
        let reasoning = json!({"id":"rs","type":"reasoning","summary":[],"content":[],
            "encrypted_content":"","status":"in_progress"});
        let finished = json!({"id":"rs","type":"reasoning","summary":[],
            "content":[{"type":"reasoning_text","text":"think"}],"status":"completed"});
        let message = json!({"id":"msg","type":"message","role":"assistant","content":[]});
        let answer = json!({"id":"msg","type":"message","role":"assistant",
            "content":[{"type":"output_text","text":"answer"}]});
        let mut decoder = Decoder::new("model".into());
        let mut chunks = Vec::new();
        for event in [
            json!({"type":"response.output_item.added","item":reasoning}),
            json!({"type":"response.reasoning_text.delta","item_id":"rs","delta":"think"}),
            json!({"type":"response.output_item.added","item":message}),
            json!({"type":"response.content_part.added","item_id":"msg",
                "part":{"type":"output_text","text":""}}),
            json!({"type":"response.output_text.delta","item_id":"msg","delta":"answer"}),
            json!({"type":"response.output_item.done","item":finished}),
            json!({"type":"response.output_item.done","item":answer}),
            json!({"type":"response.completed","response":{"status":"completed",
                "output":[finished, answer]}}),
        ] {
            chunks.extend(decoder.feed(event).unwrap());
        }
        let position =
            |wanted: &dyn Fn(&ResponseChunk) -> bool| chunks.iter().position(wanted).unwrap();
        let reasoning_closed = position(
            &|chunk| matches!(chunk, ResponseChunk::BlockEnded { item, .. } if item == "rs"),
        );
        let answer_started = position(
            &|chunk| matches!(chunk, ResponseChunk::ItemStarted { id, .. } if id == "msg"),
        );
        assert!(reasoning_closed < answer_started);
        let mut assembler = ResponseAssembler::default();
        for chunk in &chunks {
            assembler.push(chunk).unwrap();
        }
        let (items, _, _) = assembler.finish().unwrap();
        assert_eq!(items[0].blocks.len(), 1);
        assert_eq!(
            items[0].blocks[0].content,
            BlockContent::Reasoning {
                text: "think".into()
            }
        );
        // The replay is still the item's final snapshot.
        assert_eq!(items[0].replay.as_ref().unwrap().payload, finished);
    }

    /// A late final-text event for reasoning already closed for display may
    /// differ from what streamed (e.g. trimmed); it must not fail the response.
    #[test]
    fn late_final_text_for_reasoning_shown_early_is_ignored() {
        let reasoning = json!({"id":"rs","type":"reasoning","summary":[],"content":[]});
        let finished = json!({"id":"rs","type":"reasoning","summary":[],
            "content":[{"type":"reasoning_text","text":"think"}]});
        let message = json!({"id":"msg","type":"message","role":"assistant","content":[]});
        let answer = json!({"id":"msg","type":"message","role":"assistant",
            "content":[{"type":"output_text","text":"answer"}]});
        let mut decoder = Decoder::new("model".into());
        let mut assembler = ResponseAssembler::default();
        for event in [
            json!({"type":"response.output_item.added","item":reasoning}),
            json!({"type":"response.reasoning_text.delta","item_id":"rs","delta":"think "}),
            json!({"type":"response.output_item.added","item":message}),
            json!({"type":"response.reasoning_text.done","item_id":"rs","text":"think"}),
            json!({"type":"response.content_part.added","item_id":"msg",
                "part":{"type":"output_text","text":""}}),
            json!({"type":"response.output_text.delta","item_id":"msg","delta":"answer"}),
            json!({"type":"response.output_item.done","item":finished}),
            json!({"type":"response.output_item.done","item":answer}),
            json!({"type":"response.completed","response":{"status":"completed",
                "output":[finished, answer]}}),
        ] {
            for chunk in decoder.feed(event).unwrap() {
                assembler.push(&chunk).unwrap();
            }
        }
        let (items, _, _) = assembler.finish().unwrap();
        assert_eq!(
            items[0].blocks[0].content,
            BlockContent::Reasoning {
                text: "think ".into()
            }
        );
    }
}
