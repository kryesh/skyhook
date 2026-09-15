//! Shape-based normalization for omitted Responses bookkeeping. Never infer a
//! tool call identity or resolve an ambiguous reference from array position.
use super::native::NativeItem;
use super::*;

/// Local item/part indices are produced only by this module's resolution
/// algorithm; they are not interchangeable with provider-supplied wire indices.
#[derive(Clone, Copy)]
struct ResolvedReference {
    item: usize,
    kind: ItemKind,
    part: usize,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum TerminalOutcome {
    Completed,
    MaxTokens,
    ContentFilter,
}

/// Only consumed fields are interpreted here. Native snapshots stay borrowed
/// whole, including unknown vendor extensions used by replay and reconciliation.
#[derive(Debug)]
pub(super) enum NormalizedEvent<'a> {
    Ignored,
    ItemAdded {
        index: usize,
        wire: Option<usize>,
        native: NativeItem<'a>,
    },
    ItemDone {
        index: usize,
        native: NativeItem<'a>,
    },
    /// `part` is the local `(item, position)` pair.
    Delta {
        part: (usize, usize),
        text: &'a str,
    },
    ArgumentsDelta {
        item: usize,
        text: &'a str,
    },
    ArgumentsDone {
        item: usize,
        text: &'a str,
    },
    PartAdded {
        part: (usize, usize),
        text: &'a str,
    },
    PartEnded {
        part: (usize, usize),
        content: BlockContent,
    },
    Terminal {
        output: &'a [Value],
        usage: Option<Usage>,
        outcome: TerminalOutcome,
    },
}

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
        let Ok(parts) = header.final_parts() else {
            return false;
        };
        if let ItemBody::Function(state) = &item.body
            && let FunctionPhase::Completed { call, .. } = &state.phase
        {
            return parts.as_slice() == [BlockContent::ToolCall(call.clone())];
        }
        if let Some(old) = item.snapshot() {
            return final_parts(old).ok().as_ref() == Some(&parts);
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
            return observed.is_some()
                && observed
                    == native
                        .get("arguments")
                        .and_then(Value::as_str)
                        .and_then(|text| arguments(text).ok());
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
                        BlockContent::Reasoning {
                            text: part.streamed().to_owned(),
                        }
                    } else {
                        BlockContent::Text {
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
        chunks: &mut Vec<ResponseChunk>,
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

    /// Admit raw wire fields and resolve references exactly once. The resulting
    /// event contains no synthetic JSON bookkeeping for dispatch to re-read.
    pub(super) fn normalize_event<'a>(
        &mut self,
        event: &'a Value,
        chunks: &mut Vec<ResponseChunk>,
    ) -> Result<NormalizedEvent<'a>, ProviderError> {
        let wire = optional_index(event, "output_index")?;
        let name = string(event, "type")?;
        if name == "response.output_item.added" {
            let index = self.vacant_index(wire)?;
            let native =
                NativeItem::parse(event.get("item").ok_or_else(|| protocol("missing item"))?)?;
            return Ok(NormalizedEvent::ItemAdded {
                index,
                wire,
                native,
            });
        }
        if name == "response.output_item.done" {
            let native =
                NativeItem::parse(event.get("item").ok_or_else(|| protocol("missing item"))?)?;
            let index = self.snapshot_index(native, wire, None, chunks)?;
            return Ok(NormalizedEvent::ItemDone { index, native });
        }
        let reference = self.resolve_reference(event, name, wire, chunks)?;
        // The resolver checks identities and wire/local-index contradictions.
        // This final state check is shared by every consumed live-item event.
        let active = |expected| -> Result<(usize, usize), ProviderError> {
            let reference = reference.ok_or_else(|| protocol("missing resolved item reference"))?;
            let item = &self.items[&reference.item];
            if item.snapshot().is_some() {
                return Err(protocol("event after output item ended"));
            }
            if reference.kind != expected {
                return Err(protocol("event does not match output item kind"));
            }
            Ok((reference.item, reference.part))
        };
        let text_content = |kind, text: &str| {
            if kind == ItemKind::Reasoning {
                BlockContent::Reasoning { text: text.into() }
            } else {
                BlockContent::Text { text: text.into() }
            }
        };
        match name {
            "response.created" | "response.in_progress" | "response.queued" => {
                if !event.get("response").is_some_and(Value::is_object) {
                    return Err(protocol("missing response object"));
                }
                Ok(NormalizedEvent::Ignored)
            }
            "response.output_text.delta" | "response.refusal.delta" => Ok(NormalizedEvent::Delta {
                part: active(ItemKind::Text)?,
                text: string(event, "delta")?,
            }),
            "response.reasoning_text.delta" | "response.reasoning_summary_text.delta" => {
                Ok(NormalizedEvent::Delta {
                    part: active(ItemKind::Reasoning)?,
                    text: string(event, "delta")?,
                })
            }
            "response.function_call_arguments.delta" => Ok(NormalizedEvent::ArgumentsDelta {
                item: active(ItemKind::ToolCall)?.0,
                text: string(event, "delta")?,
            }),
            "response.function_call_arguments.done" => Ok(NormalizedEvent::ArgumentsDone {
                item: active(ItemKind::ToolCall)?.0,
                text: string(event, "arguments")?,
            }),
            "response.output_text.done"
            | "response.refusal.done"
            | "response.reasoning_text.done"
            | "response.reasoning_summary_text.done" => {
                let kind = if name.starts_with("response.reasoning_") {
                    ItemKind::Reasoning
                } else {
                    ItemKind::Text
                };
                let part = active(kind)?;
                let text = if name == "response.reasoning_text.done" {
                    reasoning_text(event)?
                } else {
                    string(
                        event,
                        if name == "response.refusal.done" {
                            "refusal"
                        } else {
                            "text"
                        },
                    )?
                };
                Ok(NormalizedEvent::PartEnded {
                    part,
                    content: text_content(kind, text),
                })
            }
            "response.content_part.added"
            | "response.content_part.done"
            | "response.reasoning_part.added"
            | "response.reasoning_part.done"
            | "response.reasoning_summary_part.added"
            | "response.reasoning_summary_part.done" => {
                let reference =
                    reference.ok_or_else(|| protocol("missing resolved item reference"))?;
                let kind = reference.kind;
                if kind == ItemKind::ToolCall {
                    return Err(protocol("content part on function item"));
                }
                let resolved = active(kind)?;
                let part = event
                    .get("part")
                    .ok_or_else(|| protocol("missing content part"))?;
                let text = if kind == ItemKind::Reasoning {
                    readable_reasoning(part, name.starts_with("response.reasoning_summary_part."))?
                } else {
                    match string(part, "type")? {
                        "output_text" => string(part, "text")?,
                        "refusal" => string(part, "refusal")?,
                        _ => return Err(protocol("unsupported content part")),
                    }
                };
                if name.ends_with(".done") {
                    Ok(NormalizedEvent::PartEnded {
                        part: resolved,
                        content: text_content(kind, text),
                    })
                } else {
                    Ok(NormalizedEvent::PartAdded {
                        part: resolved,
                        text,
                    })
                }
            }
            "response.output_text.annotation.added" => {
                active(ItemKind::Text)?;
                index(event, "annotation_index")?;
                if !event.get("annotation").is_some_and(Value::is_object) {
                    return Err(protocol("missing annotation object"));
                }
                Ok(NormalizedEvent::Ignored)
            }
            "response.completed" | "response.incomplete" => self.normalize_terminal(event, name),
            "response.failed" => {
                let response = event
                    .get("response")
                    .ok_or_else(|| protocol("missing failed response"))?;
                Err(api_error(
                    response
                        .get("error")
                        .ok_or_else(|| protocol("missing response error"))?,
                ))
            }
            "error" => Err(api_error(event.get("error").unwrap_or(event))),
            other => {
                let name: String = other
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_'))
                    .take(96)
                    .collect();
                Err(protocol(format!("unsupported event: {name}")))
            }
        }
    }

    fn normalize_terminal<'a>(
        &self,
        event: &'a Value,
        name: &str,
    ) -> Result<NormalizedEvent<'a>, ProviderError> {
        let response = event
            .get("response")
            .ok_or_else(|| protocol("missing final response"))?;
        let outcome = match string(response, "status")? {
            "incomplete" => {
                let details = response
                    .get("incomplete_details")
                    .ok_or_else(|| protocol("missing incomplete details"))?;
                match string(details, "reason")? {
                    "max_output_tokens" => TerminalOutcome::MaxTokens,
                    "content_filter" => TerminalOutcome::ContentFilter,
                    _ => return Err(protocol("unsupported incomplete reason")),
                }
            }
            "completed" if name == "response.completed" => TerminalOutcome::Completed,
            _ => return Err(protocol("terminal response status disagrees with event")),
        };
        let output = if self.allow_omitted_terminal_output && response.get("output").is_none() {
            &[][..]
        } else {
            array(response, "output")?.as_slice()
        };
        let usage = response
            .get("usage")
            .filter(|u| !u.is_null())
            .map(|usage| {
                let count = |key: &str| {
                    usage
                        .get(key)
                        .and_then(Value::as_u64)
                        .ok_or_else(|| protocol(format!("missing or invalid usage.{key}")))
                };
                let cached = match usage.get("input_tokens_details").filter(|v| !v.is_null()) {
                    Some(details) => details
                        .get("cached_tokens")
                        .and_then(Value::as_u64)
                        .ok_or_else(|| protocol("invalid cached input token usage"))?,
                    None => 0,
                };
                let input = count("input_tokens")?
                    .checked_sub(cached)
                    .ok_or_else(|| protocol("cached tokens exceed input tokens"))?;
                Ok(Usage {
                    input_tokens: input,
                    cached_input_tokens: cached,
                    output_tokens: count("output_tokens")?,
                })
            })
            .transpose()?;
        Ok(NormalizedEvent::Terminal {
            output,
            usage,
            outcome,
        })
    }

    /// Resolve omitted wire references once without mutating the vendor event.
    /// Local indices are never treated as evidence of provider wire indices.
    fn resolve_reference(
        &mut self,
        event: &Value,
        name: &str,
        wire: Option<usize>,
        chunks: &mut Vec<ResponseChunk>,
    ) -> Result<Option<ResolvedReference>, ProviderError> {
        let wire_owner = wire.and_then(|wire| {
            self.items
                .iter()
                .find_map(|(id, item)| (item.wire_index == Some(wire)).then_some(*id))
        });
        let fixed = if name.starts_with("response.reasoning_") {
            Some(ItemKind::Reasoning)
        } else if name.starts_with("response.function_call_arguments.") {
            Some(ItemKind::ToolCall)
        } else if name.starts_with("response.output_text.") || name.starts_with("response.refusal.")
        {
            Some(ItemKind::Text)
        } else if name.starts_with("response.content_part.") {
            None
        } else {
            return Ok(None);
        };
        let native_id = optional_id(event, "item_id")?;
        // Generic part events can also address reasoning items. Identity,
        // not a provider label, disambiguates output_text inside reasoning.
        let expected = fixed.unwrap_or_else(|| {
            native_id
                .and_then(|id| self.item_by_id(id))
                .or(wire_owner)
                .map_or_else(
                    || match event.pointer("/part/type").and_then(Value::as_str) {
                        Some("reasoning_text" | "summary_text") => ItemKind::Reasoning,
                        _ => ItemKind::Text,
                    },
                    |id| self.items[&id].kind(),
                )
        });
        let known = native_id.and_then(|id| self.item_by_id(id));
        let id = if let Some(id) = known {
            let item = &self.items[&id];
            if item.kind() != expected {
                return Err(protocol("event does not match output item kind"));
            }
            self.bind_wire_index(id, wire)?;
            id
        } else if let Some(native_id) = native_id {
            let id = self.vacant_index(wire)?;
            self.start_item(id, native_id, expected, (None, None), chunks)?;
            self.items.get_mut(&id).expect("started item").wire_index = wire;
            id
        } else if let Some(id) = wire_owner {
            if self.items[&id].kind() != expected {
                return Err(protocol("event does not match output item kind"));
            }
            id
        } else {
            let candidates: Vec<_> = self
                .items
                .iter()
                .filter_map(|(id, item)| {
                    (item.kind() == expected
                        && item.snapshot().is_none()
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
        if expected == ItemKind::ToolCall {
            return Ok(Some(ResolvedReference {
                item: id,
                kind: expected,
                part: 0,
            }));
        }
        let summary = name.starts_with("response.reasoning_summary_");
        let key = if summary {
            "summary_index"
        } else {
            "content_index"
        };
        let position = match optional_index(event, key)? {
            Some(position) => position,
            None => {
                let item = &self.items[&id];
                let positions: Vec<_> = item
                    .parts()
                    .map(|(position, _)| position)
                    .filter_map(|position| {
                        if expected == ItemKind::Reasoning {
                            ((position.is_multiple_of(2)) == summary).then_some(*position / 2)
                        } else {
                            Some(*position)
                        }
                    })
                    .collect();
                match positions.as_slice() {
                    [] => 0,
                    [only] => *only,
                    _ => return Err(protocol("ambiguous missing content index")),
                }
            }
        };
        let position = if expected == ItemKind::Reasoning {
            reasoning_position(position, !summary)?
        } else {
            position
        };
        Ok(Some(ResolvedReference {
            item: id,
            kind: expected,
            part: position,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    #[test]
    fn lifecycle_events_require_a_response_and_unknown_or_failed_events_are_errors() {
        for tag in ["response.queued", "response.in_progress"] {
            let mut decoder = Decoder::new("model".into());
            let event = json!({"type":tag, "response":{"x-vendor":true}});
            assert!(decoder.feed(event).unwrap().is_empty());
            assert!(decoder.feed(json!({"type":tag})).is_err());
        }
        for event in [
            json!({"type":"response.failed", "response":{"error":{"code":"invalid_request_error"}}}),
            json!({"type":"response.future_unknown"}),
        ] {
            assert!(Decoder::new("model".into()).feed(event).is_err());
        }
    }

    #[test]
    fn normalized_references_distinguish_local_wire_and_reasoning_positions() {
        let mut decoder = Decoder::new("model".into());
        decoder
            .feed(unindexed_added(json!({"id":"first","type":"message"})))
            .unwrap();
        decoder
            .feed(added(0, json!({"id":"second","type":"reasoning"})))
            .unwrap();
        for (tag, key, position) in [
            ("response.reasoning_text.delta", "content_index", 7),
            ("response.reasoning_summary_text.delta", "summary_index", 6),
        ] {
            let mut event = json!({"type":tag,"item_id":"second","output_index":0,"delta":"text"});
            event[key] = json!(3);
            match decoder.normalize_event(&event, &mut vec![]).unwrap() {
                NormalizedEvent::Delta { part, .. } => assert_eq!(part, (1, position)),
                other => panic!("unexpected semantic event: {other:?}"),
            }
        }
    }

    fn delta(item_id: &str, text: &str) -> Value {
        json!({"type":"response.output_text.delta", "item_id":item_id, "delta":text})
    }

    fn unindexed_added(item: Value) -> Value {
        json!({"type":"response.output_item.added", "item":item})
    }

    fn unindexed_done(item: Value) -> Value {
        json!({"type":"response.output_item.done", "item":item})
    }

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
            let (items, _, reason) = assemble(events).unwrap();
            assert_eq!(reason, StopReason::EndTurn);
            assert_eq!(items.len(), expected.len(), "case {index}");
            for (item, (id, position, text)) in items.iter().zip(expected) {
                assert_eq!(item.id, id, "case {index}");
                assert_eq!(item.text_content().as_deref(), Some(text), "case {index}");
                if let Some(position) = position {
                    assert_eq!(item.position, position, "case {index}");
                }
            }
        }
    }

    #[test]
    fn missing_single_part_indices_work_for_text_and_reasoning_summaries() {
        for (native, family, part_family, part) in [
            (
                message("item", "visible"),
                "output_text",
                "content_part",
                json!({"type":"output_text", "text":"visible"}),
            ),
            (
                reasoning("item", "visible"),
                "reasoning_summary_text",
                "reasoning_summary_part",
                json!({"type":"summary_text", "text":"visible"}),
            ),
        ] {
            let (items, _, _) = assemble(vec![
                added(0, native.clone()),
                json!({"type":format!("response.{part_family}.added"), "item_id":"item",
                    "part":{"type":part["type"], "text":""}}),
                json!({"type":format!("response.{family}.delta"), "item_id":"item", "delta":"visible"}),
                json!({"type":format!("response.{family}.done"), "item_id":"item", "text":"visible"}),
                json!({"type":format!("response.{part_family}.done"), "item_id":"item", "part":part}),
                unindexed_done(native.clone()),
                completed(vec![native]),
            ])
            .unwrap();
            assert_eq!((items.len(), items[0].blocks.len()), (1, 1));
            let content = &items[0].blocks[0].content;
            let text = content
                .text_content()
                .or_else(|| content.reasoning_content());
            assert_eq!(text, Some("visible"));
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
            delta("message", "ond"),
            completed(vec![native]),
        ])
        .unwrap();
        let texts: Vec<_> = items[0]
            .blocks
            .iter()
            .map(|block| block.content.text_content())
            .collect();
        assert_eq!(texts, [Some("first"), Some("second")]);
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
        let (items, _, reason) = assemble(events).unwrap();
        assert_eq!(reason, StopReason::ToolUse);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].reasoning_content().as_deref(), Some("plan"));
        assert_eq!(items[1].text_content().as_deref(), Some("checking"));
        assert_eq!(items[2].tool_call_ref().unwrap().id(), "call-stable");
    }

    #[test]
    fn tool_argument_events_resolve_by_native_item_id() {
        let call = function("function", "call-stable", r#"{"key":"value"}"#);
        let arguments = |kind: &str, field: &str, value: Value| json!({"type":format!("response.function_call_arguments.{kind}"), "item_id":"function", field:value});
        let (items, _, reason) = assemble(vec![
            added(0, function("function", "call-stable", "")),
            arguments("delta", "delta", json!("{\"key\":")),
            arguments("delta", "delta", json!("\"value\"}")),
            arguments("done", "arguments", call["arguments"].clone()),
            unindexed_done(call.clone()),
            completed(vec![call]),
        ])
        .unwrap();
        assert_eq!(reason, StopReason::ToolUse);
        let call = items[0].tool_call_ref().unwrap();
        assert_eq!(call.id(), "call-stable");
        assert_eq!(
            Value::Object(call.arguments().clone()),
            json!({"key":"value"})
        );
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
        let ids: Vec<_> = items.iter().map(|item| item.id.as_str()).collect();
        assert_eq!(ids, ["reason-stream", "text-stream", "function-stream"]);
        let call = items[2].tool_call_ref().unwrap();
        assert_eq!(call.id(), "call-stable");
        assert_eq!(
            Value::Object(call.arguments().clone()),
            json!({"a":1, "b":2})
        );
    }

    /// Feeds events without calling finish: an unrelated missing-terminal error could
    /// mask accidental acceptance of the invalid reference under test.
    fn fails_while_feeding(events: Vec<Value>) -> bool {
        let mut decoder = Decoder::new("test-model".into());
        events.into_iter().any(|event| match decoder.feed(event) {
            Err(error) => {
                assert_eq!(error.kind, ProviderErrorKind::Protocol, "{error:?}");
                true
            }
            Ok(_) => false,
        })
    }

    #[test]
    fn unsafe_compatibility_normalization_is_a_protocol_error() {
        let streamed = |items: &[Value]| -> Vec<Value> {
            items
                .iter()
                .enumerate()
                .flat_map(|(position, item)| {
                    [added(position, item.clone()), done(position, item.clone())]
                })
                .collect()
        };
        let with = |mut events: Vec<Value>, terminal: Vec<Value>| {
            events.push(completed(terminal));
            events
        };
        let mut cases = vec![
            // Regenerated terminal ids cannot replace changed text, disambiguate
            // equivalent items, or take an id owned by another item kind.
            with(
                streamed(&[message("stream", "original")]),
                vec![message("terminal", "replacement")],
            ),
            with(
                streamed(&[message("first", "same"), message("second", "same")]),
                vec![message("new-first", "same"), message("new-second", "same")],
            ),
            with(
                streamed(&[
                    message("text", "checking"),
                    function("function", "call-stable", "{}"),
                ]),
                vec![
                    message("function", "checking"),
                    function("text", "call-stable", "{}"),
                ],
            ),
            // Explicit index and item id conflict.
            vec![
                added(0, message("first", "")),
                added(1, message("second", "")),
                json!({"type":"response.output_text.delta", "output_index":0,
                    "item_id":"second", "content_index":0, "delta":"wrong target"}),
            ],
            // Omitted item references, or invented positions, cannot pick among items.
            vec![
                added(0, message("first", "")),
                added(1, message("second", "")),
                json!({"type":"response.output_text.delta", "content_index":0, "delta":"ambiguous"}),
            ],
            vec![
                unindexed_added(message("first", "")),
                unindexed_added(message("second", "")),
                json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"ambiguous"}),
            ],
        ];
        let original = function("function", "call-original", r#"{"key":"value"}"#);
        for (output_id, at_terminal) in [
            ("function", false),
            ("function", true),
            ("regenerated", false),
            ("regenerated", true),
        ] {
            let changed = function(output_id, "call-changed", r#"{"key":"value"}"#);
            let done_item = if at_terminal {
                original.clone()
            } else {
                changed.clone()
            };
            cases.push(vec![
                added(0, original.clone()),
                done(0, done_item),
                completed(vec![changed]),
            ]);
        }
        for (field, value) in [
            ("name", json!("different_tool")),
            ("arguments", json!(r#"{"key":"other"}"#)),
        ] {
            let stable = function("function", "call-stable", r#"{"key":"value"}"#);
            let mut changed = function("regenerated", "call-stable", r#"{"key":"value"}"#);
            changed[field] = value;
            cases.push(with(streamed(&[stable]), vec![changed]));
        }
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
            cases.push(events);
        }
        // Invalid explicit indices are never treated as missing.
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
                cases.push(vec![
                    added(0, message("text", "")),
                    delta,
                    completed(vec![message("text", "x")]),
                ]);
            }
            let mut summary = json!({"type":"response.reasoning_summary_text.delta", "output_index":0,
                "summary_index":0, "item_id":"reason", "delta":"x"});
            summary["summary_index"] = invalid.clone();
            cases.push(vec![
                added(0, reasoning("reason", "")),
                summary,
                completed(vec![reasoning("reason", "x")]),
            ]);
            let mut start = added(0, message("text", "x"));
            start["output_index"] = invalid;
            cases.push(vec![start, completed(vec![message("text", "x")])]);
        }
        for (index, events) in cases.into_iter().enumerate() {
            assert!(fails_while_feeding(events), "case {index} was accepted");
        }
    }

    #[test]
    fn generic_reasoning_parts_infer_kind_from_explicit_wire_owner() {
        let output = json!({"type":"reasoning","id":"r","content":[{"type":"output_text","text":"thinking"}]});
        let part = |kind: &str| {
            json!({"type":format!("response.content_part.{kind}"),"output_index":0,"content_index":0,
                "part":{"type":"output_text","text":"thinking"}})
        };
        let (items, _, _) = assemble(vec![
            added(0, json!({"type":"reasoning","id":"r"})),
            part("added"),
            part("done"),
            done(0, output.clone()),
            completed(vec![output.clone()]),
        ])
        .unwrap();
        assert_eq!(items[0].reasoning_content().as_deref(), Some("thinking"));
        assert_eq!(items[0].replay.as_ref().unwrap().payload, output);
    }
}
