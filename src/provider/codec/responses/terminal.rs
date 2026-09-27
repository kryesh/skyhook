//! Terminal status, output completeness, usage accounting, and safe tool discard.
use super::events::Event;
use super::native::NativeItem;
use super::normalization::NormalizedEvent;
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
    pub(super) fn normalize_terminal<'a>(
        &self,
        event: &'a Value,
        kind: Event,
    ) -> Result<NormalizedEvent<'a>, ProviderError> {
        let response = event
            .get("response")
            .ok_or_else(|| NATIVE.error("missing final response"))?;
        let finish = match NATIVE.string(response, "status")? {
            // An incomplete response names why, and never a normal finish.
            "incomplete" => Finish::of(
                response
                    .pointer("/incomplete_details/reason")
                    .and_then(Value::as_str)
                    .and_then(StopReason::read)
                    .filter(|reason| *reason != StopReason::Normal),
            ),
            "completed" if kind == Event::Completed => Finish::Normal,
            _ => return Err(NATIVE.error("terminal response status disagrees with event")),
        };
        let output = if self.terminal_output == TerminalOutput::StreamedOnly
            && response.get("output").is_none()
        {
            &[][..]
        } else {
            array(response, "output")?.as_slice()
        };
        // The one report is final; the prompt total includes its cached part.
        let usage = response
            .get("usage")
            .and_then(|usage| Observed::read(usage, &USAGE))
            .map(|observed| Counters::new(InputAccounting::PromptTotal).observe(observed));
        Ok(NormalizedEvent::Terminal {
            output,
            usage,
            finish,
        })
    }

    pub(super) fn complete_response(
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
        let omitted = self.terminal_output == TerminalOutput::StreamedOnly
            && output.is_empty()
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

    const TERMINALS: [TerminalOutput; 2] = [TerminalOutput::Restated, TerminalOutput::StreamedOnly];

    #[test]
    fn abnormal_terminal_discards_tools_but_preserves_reasoning_and_usage() {
        for tag in ["response.incomplete", "response.completed"] {
            for terminal in TERMINALS {
                for output_done in [false, true] {
                    for (detail, reason) in [
                        ("max_output_tokens", CutReason::MaxTokens),
                        ("content_filter", CutReason::Refusal),
                    ] {
                        for args in ["{\"query\":", "{\"query\":\"rust\"}"] {
                            let decoder = decoder_for(terminal);
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
                                (json!(30), 20)
                            } else {
                                (json!("garbage"), 0)
                            };
                            let usage = json!({"input_tokens":20,"output_tokens":11,
                                "input_tokens_details":{"cached_tokens":reported}});
                            frames.push(json!({"type":tag, "response":{
                            "status":"incomplete", "incomplete_details":{"reason":detail},
                            "output":match terminal {
                                TerminalOutput::Restated => vec![native.clone(), call, text],
                                TerminalOutput::StreamedOnly => vec![],
                            },
                            "usage":usage}}));
                            let reduced = assemble_with(decoder, frames).unwrap();
                            assert_eq!(reduced.completion.outcome(), Outcome::Cut(reason));
                            let usage = reduced.usage;
                            assert_eq!(usage.output_tokens, 11);
                            assert_eq!(usage.input_tokens, 20 - cached);
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

    #[test]
    fn context_overflow_wins_over_open_items() {
        for terminal in TERMINALS {
            let text = json!({"type":"message", "id":"msg", "role":"assistant", "content":[]});
            let frames = vec![
                added(0, text),
                delta("msg", "partial"),
                json!({"type":"response.incomplete", "response":{"status":"incomplete",
                "incomplete_details":{"reason":"model_context_window_exceeded"}, "output":[]}}),
            ];
            let error = assemble_with(decoder_for(terminal), frames).err().unwrap();
            assert_eq!(
                error.kind(),
                crate::provider::ProviderErrorKind::ContextWindowExceeded
            );
        }
    }

    #[test]
    fn deferred_malformed_tools_still_fail_on_normal_terminal() {
        for terminal in TERMINALS {
            let mut decoder = decoder_for(terminal);
            let mut call = call_item();
            call["arguments"] = json!("{\"query\":");
            decoder.feed(added(0, call.clone())).unwrap();
            assert!(decoder.feed(done(0, call.clone())).unwrap().is_empty());
            assert!(
                decoder
                    .feed(completed(match terminal {
                        TerminalOutput::Restated => vec![call],
                        TerminalOutput::StreamedOnly => vec![],
                    }))
                    .is_err()
            );
        }
    }
}
