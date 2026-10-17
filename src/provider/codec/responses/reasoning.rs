//! Readable reasoning namespaces and exact native replay reconciliation.
use super::native::PartType;
use super::*;
use crate::provider::codec::common::{is_signed, tagged};

/// Readable content and summaries are independent wire namespaces. Interleaved
/// internal positions preserve both namespaces without exposing native state.
pub(super) fn reasoning_position(position: usize, content: bool) -> Result<usize, ProviderError> {
    position
        .checked_mul(2)
        .and_then(|position| position.checked_add(usize::from(content)))
        .ok_or_else(|| NATIVE.error("reasoning part index overflow"))
}

pub(super) fn readable_reasoning(part: &Value, summary: bool) -> Result<&str, ProviderError> {
    match (summary, tagged(part)) {
        (true, Some(PartType::SummaryText))
        | (false, Some(PartType::ReasoningText | PartType::OutputText)) => {}
        _ => return Err(NATIVE.error("unsupported reasoning content part")),
    }
    reasoning_text(part)
}

/// Some readable parts use `reasoning` instead of `text`. Do not guess when
/// both fields supply conflicting values, or silently accept invalid types.
pub(super) fn reasoning_text(value: &Value) -> Result<&str, ProviderError> {
    let field = |key| match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.as_str())),
        _ => Err(NATIVE.error("invalid readable reasoning text")),
    };
    match (field("text")?, field("reasoning")?) {
        (Some(text), None) | (None, Some(text)) => Ok(text),
        (Some(text), Some(reasoning)) if text == reasoning => Ok(text),
        _ => Err(NATIVE.error("missing or ambiguous readable reasoning text")),
    }
}

pub(super) fn reasoning_parts(item: &Value) -> Result<BTreeMap<usize, String>, ProviderError> {
    let mut parts = BTreeMap::new();
    for (field, content) in [("summary", false), ("content", true)] {
        let values = match item.get(field) {
            None | Some(Value::Null) => continue,
            Some(Value::Array(values)) => values,
            _ => return Err(NATIVE.error(format!("invalid reasoning {field}"))),
        };
        for (position, part) in values.iter().enumerate() {
            parts.insert(
                reasoning_position(position, content)?,
                readable_reasoning(part, !content)?.into(),
            );
        }
    }
    Ok(parts)
}

/// Plaintext is reconciled separately against display blocks. Native replay is
/// the latest received snapshot, which [`replayed`] fills only where it carries no
/// text: `terminal` may only fill what `previous` left null or an empty
/// `encrypted_content`, apart from the `skip`ped keys.
pub(super) fn native_enrichment(previous: &Value, terminal: &Value, skip: &[&str]) -> bool {
    if previous == terminal || previous.is_null() {
        return true;
    }
    match (previous.as_object(), terminal.as_object()) {
        (Some(previous), Some(terminal)) => previous.iter().all(|(key, value)| {
            skip.contains(&key.as_str())
                || terminal.get(key).is_some_and(|next| {
                    (key == "encrypted_content" && value.as_str() == Some("") && next.is_string())
                        || native_enrichment(value, next, &[])
                })
        }),
        _ => false,
    }
}

/// Whether a reasoning item carries encrypted state.
pub(crate) fn is_sealed(native: &Value) -> bool {
    is_signed(
        None,
        native.get("encrypted_content").and_then(Value::as_str),
    )
}

/// Whether a reasoning part position is in the content namespace, not the summary.
fn in_content(position: usize) -> bool {
    position & 1 == 1
}

/// The item that replays: the received snapshot, with readable text it lacks filled
/// from the decoded parts of that namespace. The snapshot's own text takes precedence,
/// and an encrypted item replays exactly as received.
pub(super) fn replayed(snapshot: &Value, parts: &BTreeMap<usize, Part>) -> Value {
    let mut native = snapshot.clone();
    let Some(object) = native.as_object_mut().filter(|_| !is_sealed(snapshot)) else {
        return native;
    };
    for (field, content, kind) in [
        ("summary", false, "summary_text"),
        ("content", true, "reasoning_text"),
    ] {
        if object
            .get(field)
            .and_then(Value::as_array)
            .is_some_and(|carried| !carried.is_empty())
        {
            continue;
        }
        let decoded: Vec<_> = parts
            .iter()
            .filter(|(position, _)| in_content(**position) == content)
            .filter_map(|(_, part)| part.ended())
            .map(|text| json!({"type": kind, "text": text}))
            .collect();
        if !decoded.is_empty() {
            object.insert(field.into(), Value::Array(decoded));
        }
    }
    native
}

/// The namespace that only restates the other, if the two say the same text however
/// split into parts or spaced. The one the final item supplies stands, then the one
/// received live, then the summary.
fn restated(
    parts: &BTreeMap<usize, Part>,
    supplied: &BTreeMap<usize, String>,
    received: &[usize],
) -> Option<bool> {
    let text = |content: bool| -> String {
        let parts = parts
            .iter()
            .filter(|(position, _)| in_content(**position) == content);
        let text = parts
            .filter_map(|(_, part)| part.ended())
            .flat_map(str::chars);
        text.filter(|c| !c.is_whitespace()).collect()
    };
    let summary = text(false);
    if summary.is_empty() || summary != text(true) {
        return None;
    }
    let rank = |content: bool| {
        let ours = |position: &usize| in_content(*position) == content;
        (
            supplied.keys().any(ours),
            received.iter().any(ours),
            !content,
        )
    };
    Some(rank(false) > rank(true))
}

impl Decoder {
    /// Settle a reasoning item at its final snapshot: the snapshot's readable text is
    /// authoritative, and received display it omits stands, unless one namespace
    /// restates the other, which is then neither displayed nor replayed.
    pub(super) fn end_reasoning(&mut self, id: usize, native: &Value) -> Result<(), ProviderError> {
        let supplied = reasoning_parts(native)?;
        let item = &self.items[&id];
        if let Some(previous) = item.snapshot()
            // Identity aliases are checked by end(); status is lifecycle metadata.
            && !native_enrichment(previous, native, &["summary", "content", "id", "status"])
        {
            return Err(NATIVE.error("conflicting final reasoning state"));
        }
        let received: Vec<_> = item.reasoning()?.parts.keys().copied().collect();
        let unsupplied: Vec<_> = item
            .reasoning()?
            .parts
            .iter()
            .filter(|(position, _)| !supplied.contains_key(position))
            .map(|(position, part)| {
                let text = part.ended().unwrap_or(part.streamed());
                (*position, text.to_owned())
            })
            .collect();
        for (position, text) in supplied.clone().into_iter().chain(unsupplied) {
            self.close_part(id, position, text)?;
        }
        let reasoning = self
            .items
            .get_mut(&id)
            .expect("checked item")
            .reasoning_mut()?;
        if let Some(content) = restated(&reasoning.parts, &supplied, &received) {
            reasoning
                .parts
                .retain(|position, _| in_content(*position) != content);
        }
        // The latest snapshot is the replay; the terminal one may enrich it.
        reasoning.snapshot = Some(native.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{added, assemble, completed, decoder, done, reduce};
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
        push(added(0, item(json!([]), Value::Null)));
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
        push(done(0, native.clone()));
        push(done(0, native.clone()));
        push(completed(vec![native.clone()]));
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
                    vec![completed(vec![native.clone()])],
                    &native,
                    &[("content_0", "visible")],
                );
            }
        }
        for native in [json!({"type":"reasoning", "id":"r"}), empty()] {
            assert_display(vec![completed(vec![native.clone()])], &native, &[]);
        }
        // Namespaces that say different things both stand, even where a part repeats.
        let native = item(
            json!([{"type":"summary_text", "text":"same"}, {"type":"summary_text", "text":"brief"}]),
            json!([{"type":"reasoning_text", "text":"same"}, {"type":"output_text", "text":"details"}]),
        );
        assert_display(
            vec![completed(vec![native.clone()])],
            &native,
            &[
                ("summary_0", "same"),
                ("content_0", "same"),
                ("summary_1", "brief"),
                ("content_1", "details"),
            ],
        );
    }

    #[test]
    fn terminal_can_add_plaintext_after_done_or_omit_received_display() {
        for final_has_text in [false, true] {
            let mut native = json!({"type":"reasoning", "id":"r", "encrypted_content":"cipher"});
            let mut events = vec![added(0, empty())];
            if final_has_text {
                native["content"] = json!([{"type":"reasoning_text", "text":"visible"}]);
            } else {
                events.push(content_delta("visible"));
            }
            events.extend([done(0, empty()), completed(vec![native.clone()])]);
            assert_display(events, &native, &[("content_0", "visible")]);
        }
        // Unencrypted reasoning replays the received display its final item omits.
        let native = json!({"type":"reasoning", "id":"r", "summary":[]});
        let mut replayed = native.clone();
        replayed["content"] = json!([{"type":"reasoning_text", "text":"visible"}]);
        let events = vec![
            added(0, empty()),
            content_delta("visible"),
            done(0, native.clone()),
            completed(vec![native]),
        ];
        assert_display(events, &replayed, &[("content_0", "visible")]);
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
                    added(0, empty()),
                    part("added", json!({"type":part_type,"reasoning":"vis"})),
                    content_delta("ible"),
                    part("done", json!({"type":part_type,"reasoning":"visible"})),
                    part(
                        "done",
                        json!({"type":part_type,"text":"visible","reasoning":"visible"}),
                    ),
                    done(0, native.clone()),
                    completed(vec![native.clone()]),
                ];
                assert_display(events, &native, &[("content_0", "visible")]);
            }
        }
    }

    /// Summary and content indices are separate namespaces: summary 1 is not
    /// content 0's slot, and display follows each part's position.
    #[test]
    fn indexed_parts_stay_in_their_namespace() {
        let summary = |index: usize, text: &str| {
            let mut summary = event(
                "reasoning_summary_text.delta",
                "summary_index",
                "delta",
                json!(text),
            );
            summary["summary_index"] = json!(index);
            summary
        };
        let native = item(
            json!([{"type":"summary_text", "text":"first"}, {"type":"summary_text", "text":"second"}]),
            json!([{"type":"reasoning_text", "text":"raw"}]),
        );
        assert_display(
            vec![
                added(0, empty()),
                summary(1, "second"),
                content_delta("raw"),
                summary(0, "first"),
                done(0, native.clone()),
                completed(vec![native.clone()]),
            ],
            &native,
            &[
                ("summary_0", "first"),
                ("content_0", "raw"),
                ("summary_1", "second"),
            ],
        );
    }

    #[test]
    fn final_plaintext_supersedes_deltas_but_conflicting_aliases_fail() {
        for (conflicting, valid) in [
            // The final snapshot is authoritative over streamed deltas.
            (json!({"type":"reasoning_text", "text":"different"}), true),
            // A part may name its text `reasoning`.
            (json!({"type":"output_text", "reasoning":"visible"}), true),
            // Aliases within one snapshot must still agree and be text.
            (
                json!({"type":"reasoning_text", "text":"visible", "reasoning":"different"}),
                false,
            ),
            (json!({"type":"reasoning_text", "text":7}), false),
        ] {
            let terminal = item(Value::Null, json!([conflicting.clone()]));
            let result = assemble(vec![
                added(0, empty()),
                content_delta("visible"),
                completed(vec![terminal]),
            ]);
            assert_eq!(result.is_ok(), valid, "{conflicting}");
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
            let result = assemble(vec![
                added(0, empty()),
                content_delta("native"),
                event(
                    "reasoning_text.done",
                    "content_index",
                    "text",
                    json!(done_text),
                ),
                done(0, readable(None, content)),
                completed(vec![]),
            ]);
            assert_eq!(
                result.is_ok(),
                valid,
                "content={content:?}, done={done_text}: {:?}",
                result.err()
            );
        }
    }

    #[test]
    fn terminal_reasoning_may_enrich_but_not_replace_native_state() {
        for placeholder in [Value::Null, json!("")] {
            let old = json!({"type":"reasoning", "id":"r", "summary":[], "encrypted_content":placeholder});
            let final_item = json!({"type":"reasoning", "id":"r", "summary":[], "encrypted_content":"ciphertext"});
            let events = vec![
                added(0, old.clone()),
                done(0, old),
                completed(vec![final_item.clone()]),
            ];
            let reduced = assemble(events).unwrap();
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
            let events = vec![
                added(0, old.clone()),
                done(0, old.clone()),
                completed(vec![final_item]),
            ];
            assert!(assemble(events).is_err());
        }
    }

    /// A proxy streams one namespace, then its final snapshot restates the same
    /// text in the other, however spaced or split into parts. It is displayed and
    /// replayed once, as the snapshot says.
    #[test]
    fn reasoning_restated_in_the_other_namespace_is_kept_once() {
        let text = "User wants me to call run.\n";
        let summary_event = |name: &str, value_key: &str, value: Value| {
            event(name, "summary_index", value_key, value)
        };
        let summary_delta = |index: usize, text: &str| {
            let mut delta = summary_event("reasoning_summary_text.delta", "delta", json!(text));
            delta["summary_index"] = json!(index);
            delta
        };
        for (streamed, native, expected) in [
            (
                content_delta(text),
                readable(Some(text), None),
                ("summary_0", text),
            ),
            (
                content_delta("t\n"),
                readable(Some("t"), None),
                ("summary_0", "t"),
            ),
        ] {
            let events = vec![
                added(0, empty()),
                streamed,
                done(0, native.clone()),
                completed(vec![native.clone()]),
            ];
            assert_display(events, &native, &[expected]);
        }
        let native = readable(None, Some("ab"));
        let events = vec![
            added(0, empty()),
            summary_delta(0, "a"),
            summary_delta(1, "b"),
            done(0, native.clone()),
            completed(vec![native.clone()]),
        ];
        assert_display(events, &native, &[("content_0", "ab")]);
        let native = readable(Some(text), Some(text));
        assert_display(
            vec![
                added(0, empty()),
                summary_event(
                    "reasoning_summary_part.added",
                    "part",
                    json!({"type":"summary_text", "text":""}),
                ),
                summary_event("reasoning_summary_text.delta", "delta", json!(text)),
                summary_event("reasoning_summary_text.done", "text", json!(text)),
                done(0, native.clone()),
                completed(vec![native.clone()]),
            ],
            &native,
            &[("summary_0", text)],
        );
    }
}
