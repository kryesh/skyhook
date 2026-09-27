//! Native events normalized into the decoder's semantic events. An event on a
//! live item resolves its item and part exactly once. Never infer a tool call
//! identity or resolve an ambiguous reference from array position.
use super::events::Event;
use super::native::{NativeItem, PartType, is_foreign};
use super::*;
use crate::named_enum::NamedEnum;
use crate::provider::codec::{common::tagged, openai};
use crate::provider::http::errors;

/// Local item/part indices are produced only by this module's resolution
/// algorithm; they are not interchangeable with provider-supplied wire indices.
#[derive(Clone, Copy)]
struct ResolvedReference {
    item: usize,
    kind: ItemKind,
    part: usize,
}

impl ResolvedReference {
    fn part(self) -> (usize, usize) {
        (self.item, self.part)
    }

    fn content(self, text: &str) -> Content {
        let text = text.into();
        match self.kind {
            ItemKind::Reasoning => Content::Reasoning { text },
            ItemKind::Text | ItemKind::ToolCall => Content::Text { text },
        }
    }
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
        content: Content,
    },
    Terminal {
        output: &'a [Value],
        usage: Option<Usage>,
        finish: Finish,
    },
}

fn optional_index(value: &Value, key: &str) -> Result<Option<usize>, ProviderError> {
    value.get(key).map(|_| NATIVE.index(value, key)).transpose()
}

fn optional_id<'a>(value: &'a Value, key: &str) -> Result<Option<&'a str>, ProviderError> {
    value
        .get(key)
        .map(|_| {
            let id = NATIVE.string(value, key)?;
            if id.is_empty() {
                Err(NATIVE.error("empty item ID"))
            } else {
                Ok(id)
            }
        })
        .transpose()
}

impl Decoder {
    /// Admit raw wire fields. The resulting event contains no synthetic JSON
    /// bookkeeping for dispatch to re-read.
    pub(super) fn normalize_event<'a>(
        &mut self,
        event: &'a Value,
    ) -> Result<NormalizedEvent<'a>, ProviderError> {
        let wire = optional_index(event, "output_index")?;
        let name = NATIVE.string(event, "type")?;
        // New event types (hosted tools, progress, ...) carry nothing we use.
        let Some(kind) = Event::parse(name) else {
            return Ok(NormalizedEvent::Ignored);
        };
        let signals = self.errors;
        let failure = |native| {
            let reading = openai::read::<Code>(native);
            errors::classify(None, native, reading, signals, None)
        };
        match kind {
            Event::ItemAdded | Event::ItemDone => {
                let item = event
                    .get("item")
                    .ok_or_else(|| NATIVE.error("missing item"))?;
                if is_foreign(item) {
                    return Ok(NormalizedEvent::Ignored);
                }
                let native = NativeItem::parse(item)?;
                if kind == Event::ItemAdded {
                    let index = self.vacant_index(wire)?;
                    return Ok(NormalizedEvent::ItemAdded {
                        index,
                        wire,
                        native,
                    });
                }
                let index = self.snapshot_index(native, wire, None)?;
                Ok(NormalizedEvent::ItemDone { index, native })
            }
            Event::Created | Event::InProgress | Event::Queued => {
                if !event.get("response").is_some_and(Value::is_object) {
                    return Err(NATIVE.error("missing response object"));
                }
                Ok(NormalizedEvent::Ignored)
            }
            Event::TextDelta
            | Event::RefusalDelta
            | Event::ReasoningDelta
            | Event::SummaryDelta => {
                let part = self.live_part(event, kind, wire)?.part();
                let text = NATIVE.string(event, "delta")?;
                Ok(NormalizedEvent::Delta { part, text })
            }
            Event::ArgumentsDelta => Ok(NormalizedEvent::ArgumentsDelta {
                item: self.live_part(event, kind, wire)?.item,
                text: NATIVE.string(event, "delta")?,
            }),
            Event::ArgumentsDone => Ok(NormalizedEvent::ArgumentsDone {
                item: self.live_part(event, kind, wire)?.item,
                text: NATIVE.string(event, "arguments")?,
            }),
            Event::TextDone | Event::RefusalDone | Event::ReasoningDone | Event::SummaryDone => {
                let reference = self.live_part(event, kind, wire)?;
                let text = match kind {
                    Event::ReasoningDone => reasoning_text(event)?,
                    Event::RefusalDone => NATIVE.string(event, "refusal")?,
                    _ => NATIVE.string(event, "text")?,
                };
                Ok(NormalizedEvent::PartEnded {
                    part: reference.part(),
                    content: reference.content(text),
                })
            }
            Event::PartAdded
            | Event::PartDone
            | Event::ReasoningPartAdded
            | Event::ReasoningPartDone
            | Event::SummaryPartAdded
            | Event::SummaryPartDone => {
                let reference = self.live_part(event, kind, wire)?;
                let part = event
                    .get("part")
                    .ok_or_else(|| NATIVE.error("missing content part"))?;
                let text = match (reference.kind, tagged(part)) {
                    (ItemKind::ToolCall, _) => {
                        return Err(NATIVE.error("content part on function item"));
                    }
                    (ItemKind::Reasoning, _) => readable_reasoning(part, kind.is_summary())?,
                    (ItemKind::Text, Some(PartType::OutputText)) => NATIVE.string(part, "text")?,
                    (ItemKind::Text, Some(PartType::Refusal)) => NATIVE.string(part, "refusal")?,
                    (ItemKind::Text, _) => return Err(NATIVE.error("unsupported content part")),
                };
                Ok(if kind.is_done() {
                    NormalizedEvent::PartEnded {
                        part: reference.part(),
                        content: reference.content(text),
                    }
                } else {
                    NormalizedEvent::PartAdded {
                        part: reference.part(),
                        text,
                    }
                })
            }
            Event::Annotation => {
                self.live_part(event, kind, wire)?;
                NATIVE.index(event, "annotation_index")?;
                if !event.get("annotation").is_some_and(Value::is_object) {
                    return Err(NATIVE.error("missing annotation object"));
                }
                Ok(NormalizedEvent::Ignored)
            }
            Event::Completed | Event::Incomplete => self.normalize_terminal(event, kind),
            // The failed response is the envelope around its `error`, which a
            // server may annotate beside it.
            Event::Failed => {
                let response = event
                    .get("response")
                    .ok_or_else(|| NATIVE.error("missing failed response"))?;
                if response.get("error").is_none() {
                    return Err(NATIVE.error("missing response error"));
                }
                Err(failure(response))
            }
            Event::Error => Err(failure(event)),
            // A top-level event ending the response abnormally must not be
            // mistaken for progress, or the stream would appear to stall.
            Event::Aborted => Err(NATIVE.error(format_args!("response ended abnormally: {name}"))),
        }
    }

    /// The live item and part an event addresses.
    fn live_part(
        &mut self,
        event: &Value,
        kind: Event,
        wire: Option<usize>,
    ) -> Result<ResolvedReference, ProviderError> {
        let reference = self.resolve_reference(event, kind, wire)?;
        if self.items[&reference.item].snapshot().is_some() {
            return Err(NATIVE.error("event after output item ended"));
        }
        Ok(reference)
    }

    /// Resolve omitted wire references once without mutating the vendor event.
    /// Local indices are never treated as evidence of provider wire indices.
    fn resolve_reference(
        &mut self,
        event: &Value,
        kind: Event,
        wire: Option<usize>,
    ) -> Result<ResolvedReference, ProviderError> {
        let wire_owner = wire.and_then(|wire| self.wires.get(&wire).copied());
        let native_id = optional_id(event, "item_id")?;
        // Generic part events can also address reasoning items. Identity,
        // not a provider label, disambiguates output_text inside reasoning.
        let expected = kind.item_kind().unwrap_or_else(|| {
            native_id
                .and_then(|id| self.item_by_id(id))
                .or(wire_owner)
                .map_or_else(
                    || match event.get("part").and_then(tagged) {
                        Some(PartType::ReasoningText | PartType::SummaryText) => {
                            ItemKind::Reasoning
                        }
                        _ => ItemKind::Text,
                    },
                    |id| self.items[&id].kind(),
                )
        });
        let known = native_id.and_then(|id| self.item_by_id(id));
        let id = if let Some(id) = known {
            let item = &self.items[&id];
            if item.kind() != expected {
                return Err(NATIVE.error("event does not match output item kind"));
            }
            self.bind_wire_index(id, wire)?;
            id
        } else if let Some(native_id) = native_id {
            let id = self.vacant_index(wire)?;
            self.start_item(id, wire, native_id, expected, (None, None))?;
            id
        } else if let Some(id) = wire_owner {
            if self.items[&id].kind() != expected {
                return Err(NATIVE.error("event does not match output item kind"));
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
                return Err(NATIVE.error("ambiguous or missing output item reference"));
            }
            self.bind_wire_index(candidates[0], wire)?;
            candidates[0]
        };
        if expected == ItemKind::ToolCall {
            return Ok(ResolvedReference {
                item: id,
                kind: expected,
                part: 0,
            });
        }
        let summary = kind.is_summary();
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
                    _ => return Err(NATIVE.error("ambiguous missing content index")),
                }
            }
        };
        let position = if expected == ItemKind::Reasoning {
            reasoning_position(position, !summary)?
        } else {
            position
        };
        Ok(ResolvedReference {
            item: id,
            kind: expected,
            part: position,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::*;
    use crate::provider::ProviderErrorKind;

    #[test]
    fn lifecycle_events_require_a_response_and_failed_events_are_errors() {
        for tag in ["response.queued", "response.in_progress"] {
            let mut decoder = decoder();
            let event = json!({"type":tag, "response":{"x-vendor":true}});
            assert!(decoder.feed(event).unwrap().is_empty());
            assert!(decoder.feed(json!({"type":tag})).is_err());
        }
        let failed = json!({"type":"response.failed", "response":{"error":{"code":"invalid_request_error"}}});
        let error = decoder().feed(failed).unwrap_err();
        assert_eq!(error.kind(), ProviderErrorKind::InvalidRequest);
    }

    #[test]
    fn unknown_events_and_output_item_types_are_ignored() {
        let hosted = json!({"type":"web_search_call", "id":"ws_1", "status":"completed"});
        let mut decoder = decoder();
        for event in [
            json!({"type":"response.future_unknown"}),
            json!({"type":"response.output_item.added", "output_index":0, "item":hosted}),
            json!({"type":"response.web_search_call.in_progress", "output_index":0, "item_id":"ws_1"}),
            json!({"type":"response.output_item.done", "output_index":0, "item":hosted}),
        ] {
            assert!(decoder.feed(event).unwrap().is_empty());
        }
        let output = json!([hosted, {"type":"message", "id":"msg", "role":"assistant",
            "content":[{"type":"output_text", "text":"answer"}, {"type":"output_audio"}]}]);
        let reduced = reduce(
            decoder
                .feed(json!({"type":"response.completed",
                "response":{"status":"completed", "output":output}}))
                .unwrap(),
        );
        assert_eq!(reduced.completion.outcome(), Outcome::Answer);
        // Incomplete for an unnamed reason ends without executable tools.
        let reduced = assemble(vec![
            added(0, function("fc_1", "call_1", "")),
            done(0, function("fc_1", "call_1", "{}")),
            json!({"type":"response.incomplete",
                "response":{"status":"incomplete", "output":[function("fc_1", "call_1", "{}")]}}),
        ])
        .unwrap();
        assert_eq!(
            (reduced.completion.outcome(), reduced.items().len()),
            (Outcome::Cut(CutReason::Incomplete), 0)
        );
    }

    #[test]
    fn untyped_items_and_abnormal_top_level_events_fail() {
        let untyped = json!({"id":"fc_1", "call_id":"call_1", "name":"lookup", "arguments":"{}"});
        assert!(decoder().feed(added(0, untyped.clone())).is_err());
        assert!(
            decoder()
                .feed(json!({"type":"response.completed",
                    "response":{"status":"completed", "output":[untyped]}}))
                .is_err()
        );
        for name in ["response.aborted", "response.cancelled"] {
            assert!(decoder().feed(json!({"type":name})).is_err(), "{name}");
        }
        // Sub-events of hosted tools are not the response ending.
        let hosted = json!({"type":"response.web_search_call.failed", "item_id":"ws_1"});
        assert!(decoder().feed(hosted).unwrap().is_empty());
    }

    #[test]
    fn a_final_item_without_arguments_keeps_the_streamed_ones() {
        let streamed = |final_arguments: Option<Value>, done_event: bool| {
            let mut final_item = function("fc_1", "call_1", "");
            match final_arguments {
                Some(arguments) => final_item["arguments"] = arguments,
                None => {
                    final_item.as_object_mut().unwrap().remove("arguments");
                }
            }
            let mut events = vec![
                added(0, function("fc_1", "call_1", "")),
                json!({"type":"response.function_call_arguments.delta", "output_index":0,
                    "item_id":"fc_1", "delta":"{\"path\":\"/etc\"}"}),
            ];
            if done_event {
                events.push(json!({"type":"response.function_call_arguments.done",
                    "output_index":0, "item_id":"fc_1", "arguments":"{\"path\":\"/etc\"}"}));
            }
            events.extend([done(0, final_item.clone()), completed(vec![final_item])]);
            let reduced = assemble(events).unwrap();
            assert_eq!(reduced.completion.outcome(), Outcome::ToolUse);
            let call = reduced.items()[0].call().expect("expected a tool call");
            Value::Object(call.arguments().clone())
        };
        // Missing, blank, and placeholder values that decode to `{}`.
        for final_arguments in [
            None,
            Some(json!("")),
            Some(json!("  ")),
            Some(json!("{}")),
            Some(json!("null")),
            Some(json!("\"\"")),
            Some(Value::Null),
            Some(json!({})),
        ] {
            for done_event in [false, true] {
                assert_eq!(
                    streamed(final_arguments.clone(), done_event),
                    json!({"path":"/etc"}),
                    "{final_arguments:?}"
                );
            }
        }
        // Arguments sent as a decoded object are the call's input.
        assert_eq!(
            streamed(Some(json!({"path":"/etc"})), false),
            json!({"path":"/etc"})
        );
    }

    #[test]
    fn object_arguments_are_accepted_and_conflicting_arguments_fail() {
        let item = |arguments: Value| {
            json!({"type":"function_call", "id":"fc_1", "call_id":"call_1",
                "name":"lookup", "arguments":arguments, "status":"completed"})
        };
        // Terminal-only output with object arguments.
        let reduced = assemble(vec![completed(vec![item(json!({"path":"/etc"}))])]).unwrap();
        let call = reduced.items()[0].call().expect("expected a tool call");
        assert_eq!(
            Value::Object(call.arguments().clone()),
            json!({"path":"/etc"})
        );
        // Other non-string types are not arguments.
        for arguments in [json!([1]), json!(5), json!(true)] {
            assert!(
                assemble(vec![completed(vec![item(arguments.clone())])]).is_err(),
                "{arguments}"
            );
        }
        // Two complete, different statements of the input conflict.
        let events = vec![
            added(0, function("fc_1", "call_1", "")),
            json!({"type":"response.function_call_arguments.delta", "output_index":0,
                "item_id":"fc_1", "delta":"{\"path\":\"/etc\"}"}),
            done(0, function("fc_1", "call_1", "{\"path\":\"/tmp\"}")),
        ];
        assert!(assemble(events).is_err());
    }

    #[test]
    fn placeholder_arguments_done_is_ignored_before_or_after_real_arguments() {
        let delta = |text: &str| {
            json!({"type":"response.function_call_arguments.delta", "output_index":0,
                "item_id":"fc_1", "delta":text})
        };
        let args_done = |text: &str| {
            json!({"type":"response.function_call_arguments.done", "output_index":0,
                "item_id":"fc_1", "arguments":text})
        };
        let real = function("fc_1", "call_1", "{\"p\":1}");
        for events in [
            vec![delta("{\"p\":1}"), args_done("{\"p\":1}"), args_done("{}")],
            vec![args_done("{}"), delta("{\"p\":1}")],
            vec![args_done("{}")],
        ] {
            let mut all = vec![added(0, function("fc_1", "call_1", ""))];
            all.extend(events);
            all.extend([done(0, real.clone()), completed(vec![real.clone()])]);
            let reduced = assemble(all).unwrap();
            let call = reduced.items()[0].call().expect("expected a tool call");
            assert_eq!(Value::Object(call.arguments().clone()), json!({"p":1}));
        }
    }

    #[test]
    fn output_messages_must_be_from_the_assistant() {
        let mut item = message("msg", "hello");
        item["role"] = json!("user");
        assert!(assemble(vec![completed(vec![item.clone()])]).is_err());
        item.as_object_mut().unwrap().remove("role");
        assert!(assemble(vec![completed(vec![item])]).is_ok());
    }

    #[test]
    fn truncated_streamed_arguments_are_discarded_on_an_abnormal_stop() {
        let mut final_item = function("fc_1", "call_1", "");
        final_item.as_object_mut().unwrap().remove("arguments");
        let prefix = || {
            vec![
                added(0, function("fc_1", "call_1", "")),
                json!({"type":"response.function_call_arguments.delta", "output_index":0,
                    "item_id":"fc_1", "delta":"{\"path\":"}),
                done(0, final_item.clone()),
            ]
        };
        let mut events = prefix();
        events.push(
            json!({"type":"response.incomplete", "response":{"status":"incomplete",
            "incomplete_details":{"reason":"max_output_tokens"}, "output":[final_item.clone()]}}),
        );
        let reduced = assemble(events).unwrap();
        assert_eq!(
            (reduced.completion.outcome(), reduced.items().len()),
            (Outcome::Cut(CutReason::MaxTokens), 0)
        );
        // A normal stop cannot execute a call whose input never completed.
        let mut events = prefix();
        events.push(completed(vec![final_item.clone()]));
        assert!(assemble(events).is_err());
    }

    #[test]
    fn normalized_references_distinguish_local_wire_and_reasoning_positions() {
        let mut decoder = decoder();
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
            match decoder.normalize_event(&event).unwrap() {
                NormalizedEvent::Delta { part, .. } => assert_eq!(part, (1, position)),
                other => panic!("unexpected semantic event: {other:?}"),
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
            let reduced = assemble(vec![
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
            let items = reduced.items();
            assert_eq!(items.len(), 1);
            let text = items[0]
                .text_content()
                .or_else(|| items[0].reasoning_text());
            assert_eq!(text.as_deref(), Some("visible"));
            assert_eq!(reduced.streamed(items[0].kind()), ["visible"]);
        }
    }

    #[test]
    fn omitted_part_index_selects_the_unique_existing_part() {
        let mut native = message("message", "first");
        native["content"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type":"output_text", "text":"second"}));
        let reduced = assemble(vec![
            added(0, message("message", "")),
            json!({"type":"response.output_text.delta", "output_index":0, "item_id":"message",
                "content_index":1, "delta":"sec"}),
            delta("message", "ond"),
            completed(vec![native]),
        ])
        .unwrap();
        let AssistantItem::Text { blocks, .. } = &reduced.items()[0] else {
            panic!("expected a text item")
        };
        let texts: Vec<_> = blocks.iter().map(|block| block.text.as_str()).collect();
        assert_eq!(texts, ["first", "second"]);
        assert_eq!(reduced.streamed(ItemKind::Text), ["second"]);
    }

    #[test]
    fn tool_argument_events_resolve_by_native_item_id() {
        let call = function("function", "call-stable", r#"{"key":"value"}"#);
        let arguments = |kind: &str, field: &str, value: Value| json!({"type":format!("response.function_call_arguments.{kind}"), "item_id":"function", field:value});
        let reduced = assemble(vec![
            added(0, function("function", "call-stable", "")),
            arguments("delta", "delta", json!("{\"key\":")),
            arguments("delta", "delta", json!("\"value\"}")),
            arguments("done", "arguments", call["arguments"].clone()),
            unindexed_done(call.clone()),
            completed(vec![call]),
        ])
        .unwrap();
        assert_eq!(reduced.completion.outcome(), Outcome::ToolUse);
        assert_eq!(
            reduced.streamed(ItemKind::ToolCall),
            ["{\"key\":\"value\"}"]
        );
        let call = reduced.items()[0].call().unwrap();
        assert_eq!(call.id(), "call-stable");
        assert_eq!(
            Value::Object(call.arguments().clone()),
            json!({"key":"value"})
        );
    }

    /// Feeds events without calling finish: an unrelated missing-terminal error could
    /// mask accidental acceptance of the invalid reference under test.
    fn fails_while_feeding(events: Vec<Value>) -> bool {
        let mut decoder = decoder();
        events.into_iter().any(|event| match decoder.feed(event) {
            Err(error) => {
                assert_eq!(error.kind(), ProviderErrorKind::Protocol, "{error:?}");
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
        let reduced = assemble(vec![
            added(0, json!({"type":"reasoning","id":"r"})),
            part("added"),
            part("done"),
            done(0, output.clone()),
            completed(vec![output.clone()]),
        ])
        .unwrap();
        let item = &reduced.items()[0];
        assert_eq!(item.reasoning_text().as_deref(), Some("thinking"));
        assert_eq!(item.replay().unwrap().payload, output);
    }
}
