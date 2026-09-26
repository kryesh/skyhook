//! Terminal status, output completeness, usage accounting, and safe tool discard.
use super::native::NativeItem;
use super::*;

impl Decoder {
    pub(super) fn complete_response(
        &mut self,
        output: &[Value],
        usage: Option<Usage>,
        finish: Finish,
        events: &mut Vec<ResponseEvent>,
    ) -> Result<(), ProviderError> {
        let truncated = finish != Finish::Normal;
        let omitted = self.terminal_output == TerminalOutput::StreamedOnly
            && output.is_empty()
            && self.items.values().all(|item| {
                item.snapshot().is_some() || (truncated && item.kind() == ItemKind::ToolCall)
            });
        // Reserve stable identities before attempting semantic aliases:
        // a new terminal-only item with equal text must not steal an
        // existing item that is also explicitly present in the output.
        let alias_candidates = self
            .items
            .iter()
            .filter_map(|(id, item)| {
                let retained = output.iter().any(|native| {
                    native
                        .get("id")
                        .and_then(Value::as_str)
                        .is_some_and(|native_id| item.is(native_id))
                });
                (!retained).then_some(*id)
            })
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
                return Err(protocol("duplicate terminal output item"));
            }
            if truncated && self.items[&id].kind() == ItemKind::ToolCall {
                if native.kind != ItemKind::ToolCall {
                    return Err(protocol("final output item identity changed"));
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
            return Err(protocol("terminal response omitted a streamed output item"));
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
            for codex in [false, true] {
                for output_done in [false, true] {
                    for (detail, reason) in [
                        ("max_output_tokens", CutReason::MaxTokens),
                        ("content_filter", CutReason::Refusal),
                    ] {
                        for args in ["{\"query\":", "{\"query\":\"rust\"}"] {
                            let decoder = if codex {
                                Decoder::new(
                                    "gpt-5".into(),
                                    scope(),
                                    &super::super::tests::streamed_only(),
                                    ErrorSignals::NONE,
                                )
                            } else {
                                Decoder::new(
                                    "gpt-5".into(),
                                    scope(),
                                    &Dialect::stateless(),
                                    ErrorSignals::NONE,
                                )
                            };
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
                            let cached = if tag == "response.completed" { 4 } else { 0 };
                            let mut usage = json!({"input_tokens":20,"output_tokens":11});
                            if cached != 0 {
                                usage["input_tokens_details"] = json!({"cached_tokens":cached});
                            }
                            frames.push(json!({"type":tag, "response":{
                            "status":"incomplete", "incomplete_details":{"reason":detail},
                            "output":if codex { vec![] } else { vec![native.clone(), call, text] },
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
    fn deferred_malformed_tools_still_fail_on_normal_terminal() {
        for codex in [false, true] {
            let mut decoder = if codex {
                Decoder::new(
                    "gpt-5".into(),
                    scope(),
                    &super::super::tests::streamed_only(),
                    ErrorSignals::NONE,
                )
            } else {
                Decoder::new(
                    "gpt-5".into(),
                    scope(),
                    &Dialect::stateless(),
                    ErrorSignals::NONE,
                )
            };
            let mut call = call_item();
            call["arguments"] = json!("{\"query\":");
            decoder.feed(added(0, call.clone())).unwrap();
            assert!(decoder.feed(done(0, call.clone())).unwrap().is_empty());
            assert!(
                decoder
                    .feed(completed(if codex { vec![] } else { vec![call] }))
                    .is_err()
            );
        }
    }
}
