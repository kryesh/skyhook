//! Output-item identity across the `added`, `done` and terminal restatements
//! of an item: stable native IDs first, wire indices where given, and aliases
//! only on unique semantic evidence.
use super::native::NativeItem;
use super::*;

impl Decoder {
    fn next_index(&self) -> Result<usize, ProviderError> {
        self.items.last_key_value().map_or(Ok(0), |(id, _)| {
            id.checked_add(1)
                .ok_or_else(|| NATIVE.error("output index overflow"))
        })
    }

    pub(super) fn item_by_id(&self, native_id: &str) -> Option<usize> {
        self.ids.get(native_id).copied()
    }

    pub(super) fn bind_wire_index(
        &mut self,
        id: usize,
        wire: Option<usize>,
    ) -> Result<(), ProviderError> {
        let Some(wire) = wire else {
            return Ok(());
        };
        let item = self.items.get_mut(&id).expect("known item");
        if item.wire_index.is_some_and(|old| old != wire)
            || self.wires.get(&wire).is_some_and(|owner| *owner != id)
        {
            return Err(NATIVE.error("contradictory output item index"));
        }
        item.wire_index = Some(wire);
        self.wires.insert(wire, id);
        Ok(())
    }

    pub(super) fn vacant_index(&self, wire: Option<usize>) -> Result<usize, ProviderError> {
        if let Some(wire) = wire {
            if self.wires.contains_key(&wire) {
                return Err(NATIVE.error("conflicting output item identity"));
            }
            if !self.items.contains_key(&wire) {
                return Ok(wire);
            }
        }
        self.next_index()
    }

    /// Only completed, semantically equivalent content can establish an alias
    /// for an output-item ID. Executable call IDs are never aliases.
    fn equivalent_snapshot(&self, item: &Item, header: NativeItem<'_>) -> bool {
        let native = header.raw;
        if header.kind != item.kind() {
            return false;
        }
        if let ItemBody::Function(state) = &item.body {
            let streaming = state.streaming().ok();
            let call = streaming
                .and_then(|state| state.call_id.as_deref())
                .or_else(|| state.snapshot()?.get("call_id")?.as_str());
            if call.is_none() || call != native.get("call_id").and_then(Value::as_str) {
                return false;
            }
            if streaming
                .and_then(|state| state.name.as_deref())
                .is_some_and(|name| Some(name) != native.get("name").and_then(Value::as_str))
            {
                return false;
            }
        }
        let Ok(parts) = final_parts(native, header.kind) else {
            return false;
        };
        if let ItemBody::Function(state) = &item.body
            && let FunctionPhase::Completed { call, .. } = &state.phase
        {
            return parts.as_slice() == [Content::ToolCall(call.clone())];
        }
        if let Some(old) = item.snapshot() {
            return final_parts(old, item.kind()).ok().as_ref() == Some(&parts);
        }
        if let ItemBody::Function(state) = &item.body {
            let Ok(streaming) = state.streaming() else {
                return false;
            };
            let observed = match &streaming.final_arguments {
                Some(FinalArguments::Object(arguments)) => Some(arguments.clone()),
                Some(FinalArguments::Incomplete(text)) => arguments(text).ok(),
                None => state
                    .part
                    .as_ref()
                    .and_then(|part| arguments(part.streamed()).ok()),
            };
            return observed.is_some() && observed == super::native::item_arguments(native).ok();
        }
        if item.parts().next().is_none() {
            return false;
        }
        let observed: Vec<_> = item
            .parts()
            .map(|(_, part)| part)
            .map(|part| {
                part.ended().cloned().unwrap_or_else(|| {
                    if item.kind() == ItemKind::Reasoning {
                        Content::Reasoning {
                            text: part.streamed().to_owned(),
                        }
                    } else {
                        Content::Text {
                            text: part.streamed().to_owned(),
                        }
                    }
                })
            })
            .collect();
        observed == parts
    }

    pub(super) fn snapshot_index(
        &mut self,
        native: NativeItem<'_>,
        wire_index: Option<usize>,
        alias_candidates: Option<&BTreeSet<usize>>,
    ) -> Result<usize, ProviderError> {
        let native_id = native.id;
        if let Some(id) = self.item_by_id(native_id) {
            self.bind_wire_index(id, wire_index)?;
            return Ok(id);
        }
        let candidates: Vec<_> = self
            .items
            .iter()
            .filter_map(|(id, item)| {
                let eligible = alias_candidates.map_or_else(
                    || wire_index.is_some_and(|wire| item.wire_index == Some(wire)),
                    |candidates| candidates.contains(id),
                );
                (eligible && self.equivalent_snapshot(item, native)).then_some(*id)
            })
            .collect();
        if candidates.len() > 1 {
            return Err(NATIVE.error("ambiguous final output item identity"));
        }
        if let Some(&id) = candidates.first() {
            self.bind_wire_index(id, wire_index)?;
            self.ids.insert(native_id.into(), id);
            return Ok(id);
        }
        let id = self.vacant_index(wire_index)?;
        self.start(id, wire_index, native)?;
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::*;
    use crate::provider::protocol::Position;

    #[test]
    fn compatible_references_assemble_distinct_items_in_order() {
        let same = |id| message(id, "same");
        let pair = |a, b| vec![same(a), same(b)];
        let (first, second) = (
            message("message-a", "first"),
            message("message-b", "second"),
        );
        let (retained, fresh) = (same("retained"), same("fresh"));
        // Expected (id, position when asserted, text).
        let cases = vec![
            // Missing output indices preserve distinct identity and order.
            (
                vec![
                    unindexed_added(message("message-a", "")),
                    unindexed_added(message("message-b", "")),
                    delta("message-b", "second"),
                    delta("message-a", "first"),
                    unindexed_done(second.clone()),
                    unindexed_done(first.clone()),
                    completed(vec![first, second]),
                ],
                vec![
                    ("message-a", Some(0), "first"),
                    ("message-b", Some(1), "second"),
                ],
            ),
            // A lazy text start from a delta needs no added event.
            (
                vec![
                    json!({"type":"response.output_text.delta", "output_index":0,
                        "item_id":"lazy-message", "delta":"hello "}),
                    delta("lazy-message", "world"),
                    completed(vec![message("lazy-message", "hello world")]),
                ],
                vec![("lazy-message", None, "hello world")],
            ),
            // An omitted output index can be established later by identity.
            (
                vec![
                    unindexed_added(message("msg", "")),
                    json!({"type":"response.output_text.delta","item_id":"msg","output_index":7,"delta":"hello"}),
                    done(7, message("msg", "hello")),
                    completed(vec![message("msg", "hello")]),
                ],
                vec![("msg", Some(0), "hello")],
            ),
            // An explicit wire owner precedes unbound items.
            (
                vec![
                    added(0, message("first", "")),
                    unindexed_added(message("second", "")),
                    json!({"type":"response.output_text.delta","output_index":0,"delta":"hello"}),
                    done(0, message("first", "hello")),
                    unindexed_done(message("second", "world")),
                    completed(vec![message("first", "hello"), message("second", "world")]),
                ],
                vec![("first", None, "hello"), ("second", None, "world")],
            ),
            // Equal terminal-only, done-only (with or without indices) and
            // unchanged streamed items remain distinct.
            (
                vec![completed(pair("first", "second"))],
                vec![("first", None, "same"), ("second", None, "same")],
            ),
            (
                vec![
                    done(0, same("first")),
                    done(1, same("second")),
                    completed(pair("first", "second")),
                ],
                vec![("first", None, "same"), ("second", None, "same")],
            ),
            (
                vec![
                    unindexed_done(same("first")),
                    unindexed_done(same("second")),
                    completed(pair("first", "second")),
                ],
                vec![("first", None, "same"), ("second", None, "same")],
            ),
            (
                vec![
                    added(0, same("first")),
                    done(0, same("first")),
                    added(1, same("second")),
                    done(1, same("second")),
                    completed(pair("first", "second")),
                ],
                vec![("first", None, "same"), ("second", None, "same")],
            ),
            // Stable terminal ids are reserved before semantic alias matching.
            (
                vec![
                    added(0, retained.clone()),
                    done(0, retained.clone()),
                    completed(vec![fresh.clone(), retained.clone()]),
                ],
                vec![("retained", None, "same"), ("fresh", None, "same")],
            ),
            (
                vec![
                    added(0, retained.clone()),
                    done(0, retained.clone()),
                    completed(vec![retained, fresh]),
                ],
                vec![("retained", None, "same"), ("fresh", None, "same")],
            ),
        ];
        for (index, (events, expected)) in cases.into_iter().enumerate() {
            let reduced = assemble(events).unwrap();
            assert_eq!(reduced.completion.outcome(), Outcome::Answer);
            let items = reduced.items();
            assert_eq!(items.len(), expected.len(), "case {index}");
            for (item, (id, position, text)) in items.iter().zip(expected) {
                assert_eq!(item.id().as_str(), id, "case {index}");
                assert_eq!(item.text_content().as_deref(), Some(text), "case {index}");
                if let Some(position) = position {
                    assert_eq!(item.position(), Position::from(position), "case {index}");
                }
            }
        }
    }

    #[test]
    fn lazy_item_done_starts_all_supported_item_kinds() {
        let output = vec![
            reasoning("reason", "plan"),
            message("text", "checking"),
            function("function", "call-stable", r#"{"key":"value"}"#),
        ];
        let mut events: Vec<_> = output.iter().cloned().map(unindexed_done).collect();
        events.push(completed(output));
        let reduced = assemble(events).unwrap();
        assert_eq!(reduced.completion.outcome(), Outcome::ToolUse);
        let items = reduced.items();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].reasoning_text().as_deref(), Some("plan"));
        assert_eq!(items[1].text_content().as_deref(), Some("checking"));
        assert_eq!(items[2].call().unwrap().id(), "call-stable");
    }

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
        let reduced = assemble(events).unwrap();
        assert_eq!(reduced.completion.outcome(), Outcome::ToolUse);
        let items = reduced.items();
        let ids: Vec<_> = items.iter().map(|item| item.id().as_str()).collect();
        assert_eq!(ids, ["reason-stream", "text-stream", "function-stream"]);
        let call = items[2].call().unwrap();
        assert_eq!(call.id(), "call-stable");
        assert_eq!(
            Value::Object(call.arguments().clone()),
            json!({"a":1, "b":2})
        );
    }
}
