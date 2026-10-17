//! Terminal status, output completeness, usage accounting, and safe tool discard.
use super::events::Event;
use super::native::NativeItem;
use super::*;
use crate::provider::codec::{
    common::StopReason,
    usage::{Counters, InputAccounting, Observed, Spelling},
};

const USAGE: Spelling = Spelling {
    input: &["/input_tokens"],
    cached: &["/input_tokens_details/cached_tokens"],
    written: &["/input_tokens_details/cache_write_tokens"],
    output: &["/output_tokens"],
};

impl Decoder {
    /// Complete the response from its terminal event.
    pub(super) fn terminal(
        &mut self,
        event: &Value,
        kind: Event,
        events: &mut Vec<ResponseEvent>,
    ) -> Result<(), ProviderError> {
        let response = event
            .get("response")
            .ok_or_else(|| NATIVE.error("missing final response"))?;
        let reason = response
            .pointer("/incomplete_details/reason")
            .filter(|reason| !reason.is_null());
        // An incomplete response names why, and never a normal finish.
        let cut = || {
            Finish::of(
                reason
                    .and_then(Value::as_str)
                    .and_then(StopReason::read)
                    .filter(|reason| *reason != StopReason::Normal),
            )
        };
        let finish = match NATIVE.string(response, "status")? {
            "incomplete" => cut(),
            "completed" if kind == Event::Completed && reason.is_none() => Finish::Normal,
            // A completed response that still says why it stopped short was cut.
            "completed" if kind == Event::Completed => cut(),
            _ => return Err(NATIVE.error("terminal response status disagrees with event")),
        };
        // Some services leave out the items they streamed; those stand on their own.
        let output = match response.get("output") {
            None => &[][..],
            Some(_) => array(response, "output")?.as_slice(),
        };
        // The one report is final; the prompt total includes its cached part.
        let usage = response
            .get("usage")
            .and_then(|usage| Observed::read(usage, &USAGE))
            .map(|observed| Counters::new(InputAccounting::PromptTotal).observe(observed));
        self.complete_response(output, usage, finish, events)
    }

    fn complete_response(
        &mut self,
        output: &[Value],
        usage: Option<Usage>,
        finish: Finish,
        events: &mut Vec<ResponseEvent>,
    ) -> Result<(), ProviderError> {
        // An error finish fails the attempt whatever state its output is in.
        if let Finish::Error(_) = finish {
            return finish.complete(Vec::new()).map(drop);
        }
        let truncated = finish != Finish::Normal;
        let omitted = output.is_empty()
            && self.items.values().all(|item| {
                item.snapshot().is_some() || (truncated && item.kind() == ItemKind::ToolCall)
            });
        // Reserve stable identities before attempting semantic aliases:
        // a new terminal-only item with equal text must not steal an
        // existing item that is also explicitly present in the output.
        let retained: BTreeSet<_> = output
            .iter()
            .filter_map(|native| self.item_by_id(native.get("id")?.as_str()?))
            .collect();
        let alias_candidates = self
            .items
            .keys()
            .filter(|id| !retained.contains(id))
            .copied()
            .collect();
        let mut seen = BTreeSet::new();
        for native in output
            .iter()
            .filter(|native| !super::native::is_foreign(native))
        {
            let native = NativeItem::parse(native)?;
            // Stable native IDs take precedence over terminal array
            // position. Regenerated IDs require unique semantic evidence.
            let id = self.snapshot_index(native, None, Some(&alias_candidates))?;
            if !seen.insert(id) {
                return Err(NATIVE.error("duplicate terminal output item"));
            }
            if truncated && self.items[&id].kind() == ItemKind::ToolCall {
                if native.kind != ItemKind::ToolCall {
                    return Err(NATIVE.error("final output item identity changed"));
                }
                continue;
            }
            self.end(id, native, true)?;
        }
        // Tolerating omitted fields never overrides an item's own word that it stopped short.
        if !truncated && !self.unfinished.is_empty() {
            return Err(NATIVE.error("normal response holds an unfinished output item"));
        }
        if !omitted
            && self.items.iter().any(|(id, item)| {
                !seen.contains(id) && !(truncated && item.kind() == ItemKind::ToolCall)
            })
        {
            return Err(NATIVE.error("terminal response omitted a streamed output item"));
        }
        // Even syntactically complete tools are unsafe on an abnormal stop;
        // they never become executable calls. Every other item is complete
        // from its received state, whether the terminal output repeated it or not.
        let items = self
            .items
            .iter()
            .filter(|(_, item)| !(truncated && item.kind() == ItemKind::ToolCall))
            .map(|(id, item)| item.build(*id, &self.model, &self.scope))
            .collect::<Result<Vec<_>, _>>()?;
        if let Some(usage) = usage {
            events.push(ResponseEvent::Usage(usage));
        }
        self.completed = true;
        events.push(ResponseEvent::End(finish.complete(items)?));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::*;

    #[test]
    fn abnormal_terminal_discards_tools_but_preserves_reasoning_and_usage() {
        for tag in ["response.incomplete", "response.completed"] {
            for restated in [true, false] {
                for output_done in [false, true] {
                    for (detail, reason) in [
                        ("max_output_tokens", CutReason::MaxTokens),
                        ("content_filter", CutReason::Refusal),
                    ] {
                        for args in ["{\"query\":", "{\"query\":\"rust\"}"] {
                            let decoder = decoder();
                            let native = reasoning_item();
                            let text = message("text", "partial answer");
                            let mut call = call_item();
                            call["arguments"] = json!(args);
                            let mut initial_call = call.clone();
                            initial_call["arguments"] = json!("");
                            let mut frames = vec![
                                added(0, native.clone()),
                                done(0, native.clone()),
                                added(1, initial_call),
                                added(2, text.clone()),
                                done(2, text.clone()),
                                json!({"type":"response.function_call_arguments.delta", "output_index":1,
                                "item_id":"fc_1", "delta":args}),
                                json!({"type":"response.function_call_arguments.done", "output_index":1,
                                "item_id":"fc_1", "arguments":args}),
                            ];
                            if output_done {
                                frames.push(done(1, call.clone()));
                            }
                            // Cached tokens are clamped to the prompt, and an
                            // unusable counter is absent.
                            let (reported, cached) = if tag == "response.completed" {
                                (json!(30), Some(20))
                            } else {
                                (json!("garbage"), None)
                            };
                            let usage = json!({"input_tokens":20,"output_tokens":11,
                                "input_tokens_details":{"cached_tokens":reported}});
                            frames.push(json!({"type":tag, "response":{
                            "status":"incomplete", "incomplete_details":{"reason":detail},
                            "output":if restated { vec![native.clone(), call, text] } else { vec![] },
                            "usage":usage}}));
                            let reduced = assemble_with(decoder, frames).unwrap();
                            assert_eq!(reduced.completion.outcome(), Outcome::Cut(reason));
                            let usage = reduced.usage;
                            assert_eq!(usage.output_tokens.unwrap(), 11);
                            let input = 20 - cached.unwrap_or_default();
                            assert_eq!(usage.input_tokens, Some(input));
                            assert_eq!(usage.cached_input_tokens, cached);
                            let items = reduced.items();
                            assert_eq!(items.len(), 2);
                            assert!(items.iter().all(|item| item.call().is_none()));
                            assert_eq!(items[1].text_content().as_deref(), Some("partial answer"));
                            assert_eq!(items[0].replay().unwrap().payload, native);
                            // The call streamed for display before it was cut.
                            assert_eq!(reduced.streamed(ItemKind::ToolCall), [args]);
                        }
                    }
                }
            }
        }
    }

    /// A terminal output may leave out streamed items only once every one has finished.
    #[test]
    fn an_empty_terminal_output_needs_every_streamed_item_finished() {
        let text = message("msg", "hello");
        let finished = vec![added(0, text.clone()), done(0, text), completed(vec![])];
        assert!(assemble_with(decoder(), finished).is_ok());
        let open = json!({"type":"message", "id":"msg", "role":"assistant", "content":[]});
        let frames = vec![added(0, open), delta("msg", "hel"), completed(vec![])];
        let error = assemble_with(decoder(), frames).err().unwrap();
        assert!(error.message.contains("omitted a streamed output item"));
    }

    /// An item's own word that it stopped short contradicts a normal finish, so it
    /// never runs; a cut keeps such text and drops such a call, as for any cut.
    #[test]
    fn an_unfinished_item_fails_a_normal_finish_and_survives_a_cut() {
        for status in ["incomplete", "in_progress"] {
            for item in [call_item(), message("msg", "partial")] {
                let text = item["type"] == "message";
                let mut unfinished = item.clone();
                unfinished["status"] = json!(status);
                for restated in [true, false] {
                    let output = if restated {
                        vec![unfinished.clone()]
                    } else {
                        vec![]
                    };
                    let frames = |terminal| {
                        vec![
                            added(0, item.clone()),
                            done(0, unfinished.clone()),
                            terminal,
                        ]
                    };
                    let error = assemble(frames(completed(output.clone()))).err().unwrap();
                    assert!(error.message.contains("unfinished output item"), "{status}");
                    let cut = json!({"type":"response.incomplete", "response":{"status":"incomplete",
                        "incomplete_details":{"reason":"max_output_tokens"}, "output":output}});
                    let reduced = assemble(frames(cut)).unwrap();
                    assert_eq!(
                        reduced.completion.outcome(),
                        Outcome::Cut(CutReason::MaxTokens)
                    );
                    let items = reduced.items();
                    assert!(items.iter().all(|item| item.call().is_none()));
                    assert_eq!(items.len(), usize::from(text), "{status}");
                }
            }
        }
        let mut unknown = call_item();
        unknown["status"] = json!("paused");
        assert!(assemble(vec![added(0, unknown)]).is_err());
    }

    /// A completed status that still says why the response stopped is a cut; a null
    /// reason says nothing.
    #[test]
    fn a_completed_status_with_a_reason_to_stop_is_a_cut() {
        for (details, outcome, calls) in [
            (
                json!({"reason":"max_output_tokens"}),
                Outcome::Cut(CutReason::MaxTokens),
                0,
            ),
            (json!({"reason":null}), Outcome::ToolUse, 1),
            (Value::Null, Outcome::ToolUse, 1),
        ] {
            let frames = vec![
                added(0, call_item()),
                done(0, call_item()),
                json!({"type":"response.completed", "response":{"status":"completed",
                "incomplete_details":details, "output":[call_item()]}}),
            ];
            let reduced = assemble(frames).unwrap();
            assert_eq!(reduced.completion.outcome(), outcome);
            let items = reduced.items();
            assert_eq!(
                items.iter().filter(|item| item.call().is_some()).count(),
                calls
            );
        }
    }

    #[test]
    fn context_overflow_wins_over_open_items() {
        let text = json!({"type":"message", "id":"msg", "role":"assistant", "content":[]});
        let frames = vec![
            added(0, text),
            delta("msg", "partial"),
            json!({"type":"response.incomplete", "response":{"status":"incomplete",
            "incomplete_details":{"reason":"model_context_window_exceeded"}, "output":[]}}),
        ];
        let error = assemble_with(decoder(), frames).err().unwrap();
        assert_eq!(
            error.kind(),
            crate::provider::ProviderErrorKind::ContextWindowExceeded
        );
    }
}
