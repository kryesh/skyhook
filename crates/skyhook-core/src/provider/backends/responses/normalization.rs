//! Shape-based normalization for omitted Responses bookkeeping. Never infer a
//! tool call identity or resolve an ambiguous reference from array position.
use super::*;

fn optional_index(value: &Value, key: &str) -> Result<Option<usize>, ProviderError> {
    value.get(key).map(|_| index(value, key)).transpose()
}

fn optional_id<'a>(value: &'a Value, key: &str) -> Result<Option<&'a str>, ProviderError> {
    value
        .get(key)
        .map(|_| {
            let id = string(value, key)?;
            if id.is_empty() {
                Err(protocol("empty item ID"))
            } else {
                Ok(id)
            }
        })
        .transpose()
}

impl Decoder {
    fn next_index(&self) -> Result<usize, ProviderError> {
        self.items.last_key_value().map_or(Ok(0), |(id, _)| {
            id.checked_add(1)
                .ok_or_else(|| protocol("output index overflow"))
        })
    }

    fn item_by_id(&self, native_id: &str) -> Option<usize> {
        self.items.iter().find_map(|(id, item)| {
            (item.native_id == native_id || item.aliases.contains(native_id)).then_some(*id)
        })
    }

    fn bind_wire_index(&mut self, id: usize, wire: Option<usize>) -> Result<(), ProviderError> {
        let Some(wire) = wire else {
            return Ok(());
        };
        if self.items[&id].wire_index.is_some_and(|old| old != wire)
            || self
                .items
                .iter()
                .any(|(other, item)| *other != id && item.wire_index == Some(wire))
        {
            return Err(protocol("contradictory output item index"));
        }
        self.items.get_mut(&id).expect("known item").wire_index = Some(wire);
        Ok(())
    }

    fn vacant_index(&self, wire: Option<usize>) -> Result<usize, ProviderError> {
        if let Some(wire) = wire {
            if self
                .items
                .values()
                .any(|item| item.wire_index == Some(wire))
            {
                return Err(protocol("conflicting output item identity"));
            }
            if !self.items.contains_key(&wire) {
                return Ok(wire);
            }
        }
        self.next_index()
    }

    /// Only completed, semantically equivalent content can establish an alias
    /// for an output-item ID. Executable call IDs are never aliases.
    fn equivalent_snapshot(&self, item: &Item, native: &Value) -> bool {
        if kind(native).ok() != Some(item.kind) {
            return false;
        }
        if item.kind == Kind::Function {
            let call = item
                .call_id
                .as_deref()
                .or_else(|| item.ended.as_ref()?.get("call_id")?.as_str());
            if call.is_none() || call != native.get("call_id").and_then(Value::as_str) {
                return false;
            }
            if item
                .name
                .as_deref()
                .is_some_and(|name| Some(name) != native.get("name").and_then(Value::as_str))
            {
                return false;
            }
        }
        let Ok(parts) = final_parts(native) else {
            return false;
        };
        if let Some(old) = &item.ended {
            return final_parts(old).ok().as_ref() == Some(&parts);
        }
        if item.kind == Kind::Function {
            let Some(text) = item
                .final_arguments
                .as_deref()
                .or_else(|| item.parts.get(&0).map(|p| p.streamed.as_str()))
            else {
                return false;
            };
            return arguments(text).ok().as_ref()
                == native
                    .get("arguments")
                    .and_then(Value::as_str)
                    .and_then(|s| arguments(s).ok())
                    .as_ref();
        }
        if item.parts.is_empty() {
            return false;
        }
        let observed: Vec<_> = item
            .parts
            .values()
            .map(|part| {
                part.ended.clone().unwrap_or_else(|| {
                    if item.kind == Kind::Reasoning {
                        BlockContent::Reasoning {
                            text: part.streamed.clone(),
                        }
                    } else {
                        BlockContent::Text {
                            text: part.streamed.clone(),
                        }
                    }
                })
            })
            .collect();
        observed == parts
    }

    pub(super) fn snapshot_index(
        &mut self,
        native: &Value,
        wire_index: Option<usize>,
        alias_candidates: Option<&BTreeSet<usize>>,
        chunks: &mut Vec<ResponseChunk>,
    ) -> Result<usize, ProviderError> {
        let native_id = string(native, "id")?;
        if native_id.is_empty() {
            return Err(protocol("empty item ID"));
        }
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
            return Err(protocol("ambiguous final output item identity"));
        }
        if let Some(&id) = candidates.first() {
            self.bind_wire_index(id, wire_index)?;
            self.items
                .get_mut(&id)
                .expect("matched item")
                .aliases
                .insert(native_id.into());
            return Ok(id);
        }
        let id = self.vacant_index(wire_index)?;
        self.start(id, native, chunks)?;
        self.items.get_mut(&id).expect("started item").wire_index = wire_index;
        Ok(id)
    }

    /// Fill omitted indices/headers before the strict event handlers run.
    /// Internal positions are not evidence of provider-supplied wire indices.
    pub(super) fn normalize_event(
        &mut self,
        event: &mut Value,
        chunks: &mut Vec<ResponseChunk>,
    ) -> Result<(), ProviderError> {
        let name = string(event, "type")?.to_owned();
        if name == "response.output_item.added" {
            let wire = optional_index(event, "output_index")?;
            let id = self.vacant_index(wire)?;
            event["output_index"] = json!(id);
            return Ok(());
        }
        if name == "response.output_item.done" {
            let wire = optional_index(event, "output_index")?;
            let native = event
                .get("item")
                .ok_or_else(|| protocol("missing item"))?
                .clone();
            let id = self.snapshot_index(&native, wire, None, chunks)?;
            event["output_index"] = json!(id);
            return Ok(());
        }
        let wire = optional_index(event, "output_index")?;
        let wire_owner = wire.and_then(|wire| {
            self.items
                .iter()
                .find_map(|(id, item)| (item.wire_index == Some(wire)).then_some(*id))
        });
        let expected = if name.starts_with("response.reasoning_") {
            Some(Kind::Reasoning)
        } else if name.starts_with("response.function_call_arguments.") {
            Some(Kind::Function)
        } else if name.starts_with("response.output_text.") || name.starts_with("response.refusal.")
        {
            Some(Kind::Text)
        } else if name.starts_with("response.content_part.") {
            // Generic part events can also address reasoning items. Identity,
            // not a provider label, disambiguates output_text inside reasoning.
            optional_id(event, "item_id")?
                .and_then(|id| self.item_by_id(id))
                .or(wire_owner)
                .map(|id| self.items[&id].kind)
                .or_else(
                    || match event.pointer("/part/type").and_then(Value::as_str) {
                        Some("reasoning_text" | "summary_text") => Some(Kind::Reasoning),
                        _ => Some(Kind::Text),
                    },
                )
        } else {
            None
        };
        let Some(expected) = expected else {
            return Ok(());
        };
        let native_id = optional_id(event, "item_id")?.map(str::to_owned);
        let known = native_id.as_deref().and_then(|id| self.item_by_id(id));
        let id = if let Some(id) = known {
            let item = &self.items[&id];
            if item.kind != expected {
                return Err(protocol("event does not match output item kind"));
            }
            self.bind_wire_index(id, wire)?;
            id
        } else if let Some(native_id) = native_id.as_deref() {
            let id = self.vacant_index(wire)?;
            let native = match expected {
                Kind::Text => {
                    json!({"id":native_id,"type":"message","role":"assistant","content":[]})
                }
                Kind::Reasoning => json!({"id":native_id,"type":"reasoning","summary":[]}),
                Kind::Function => json!({"id":native_id,"type":"function_call"}),
            };
            self.start(id, &native, chunks)?;
            self.items.get_mut(&id).expect("started item").wire_index = wire;
            id
        } else if let Some(id) = wire_owner {
            if self.items[&id].kind != expected {
                return Err(protocol("event does not match output item kind"));
            }
            id
        } else {
            let candidates: Vec<_> = self
                .items
                .iter()
                .filter_map(|(id, item)| {
                    (item.kind == expected
                        && item.ended.is_none()
                        && wire.is_none_or(|wire| item.wire_index.is_none_or(|old| old == wire)))
                    .then_some(*id)
                })
                .collect();
            if candidates.len() != 1 {
                return Err(protocol("ambiguous or missing output item reference"));
            }
            self.bind_wire_index(candidates[0], wire)?;
            candidates[0]
        };
        event["output_index"] = json!(id);
        event["item_id"] = json!(self.items[&id].native_id);
        if expected == Kind::Function {
            return Ok(());
        }
        let summary = name.starts_with("response.reasoning_summary_");
        let key = if summary {
            "summary_index"
        } else {
            "content_index"
        };
        if optional_index(event, key)?.is_none() {
            let item = &self.items[&id];
            let positions: Vec<_> = item
                .parts
                .keys()
                .filter_map(|position| {
                    if expected == Kind::Reasoning {
                        ((position.is_multiple_of(2)) == summary).then_some(*position / 2)
                    } else {
                        Some(*position)
                    }
                })
                .collect();
            let position = match positions.as_slice() {
                [] => 0,
                [only] => *only,
                _ => return Err(protocol("ambiguous missing content index")),
            };
            event[key] = json!(position);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::protocol::{AssistantItem, ResponseAssembler};

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

    fn assemble(
        events: Vec<Value>,
    ) -> Result<(Vec<AssistantItem>, Usage, StopReason), ProviderError> {
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

    fn completed(output: Vec<Value>) -> Value {
        json!({"type":"response.completed", "response":{"status":"completed", "output":output,
            "usage":{"input_tokens":12, "output_tokens":3,
                "input_tokens_details":{"cached_tokens":4}}}})
    }

    fn done(position: usize, item: Value) -> Value {
        json!({"type":"response.output_item.done", "output_index":position, "item":item})
    }

    fn added(position: usize, item: Value) -> Value {
        json!({"type":"response.output_item.added", "output_index":position, "item":item})
    }

    fn function(id: &str, call_id: &str, arguments: &str) -> Value {
        json!({"type":"function_call", "id":id, "call_id":call_id,
            "name":"lookup", "arguments":arguments, "status":"completed"})
    }

    fn reasoning(id: &str, text: &str) -> Value {
        json!({"type":"reasoning", "id":id,
            "summary":[{"type":"summary_text", "text":text}]})
    }

    fn message(id: &str, text: &str) -> Value {
        json!({"type":"message", "id":id, "role":"assistant", "status":"completed",
            "content":[{"type":"output_text", "text":text, "annotations":[]}]})
    }

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

    #[test]
    fn regenerated_terminal_id_cannot_replace_changed_text() {
        assert_protocol_error(vec![
            added(0, message("stream", "original")),
            done(0, message("stream", "original")),
            completed(vec![message("terminal", "replacement")]),
        ]);
    }

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

    #[test]
    fn explicit_index_and_item_id_conflicts_are_errors() {
        assert_protocol_error(vec![
            added(0, message("first", "")),
            added(1, message("second", "")),
            json!({"type":"response.output_text.delta", "output_index":0,
                "item_id":"second", "content_index":0, "delta":"wrong target"}),
        ]);
    }

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

    #[test]
    fn omitted_item_reference_rejects_multiple_candidate_items() {
        assert_protocol_error(vec![
            added(0, message("first", "")),
            added(1, message("second", "")),
            json!({"type":"response.output_text.delta", "content_index":0, "delta":"ambiguous"}),
        ]);
    }

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
        let output = json!({"type":"reasoning","id":"r","content":[{"type":"output_text","text":"thinking"}]});
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
}
