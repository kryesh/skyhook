//! Terminal status, output completeness, usage accounting, and safe tool discard.
use super::native::NativeItem;
use super::normalization::TerminalOutcome;
use super::*;

impl Decoder {
    pub(super) fn complete_response(
        &mut self,
        output: &[Value],
        usage: Option<Usage>,
        outcome: TerminalOutcome,
        chunks: &mut Vec<ResponseChunk>,
    ) -> Result<(), ProviderError> {
        let truncated = !matches!(outcome, TerminalOutcome::Completed);
        let omitted = self.allow_omitted_terminal_output
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
                        .is_some_and(|native_id| {
                            native_id == item.native_id || item.aliases.contains(native_id)
                        })
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
            let id = self.snapshot_index(native, None, Some(&alias_candidates), chunks)?;
            if !seen.insert(id) {
                return Err(protocol("duplicate terminal output item"));
            }
            if truncated && self.items[&id].kind() == ItemKind::ToolCall {
                if native.kind != ItemKind::ToolCall {
                    return Err(protocol("final output item identity changed"));
                }
                continue;
            }
            self.end(id, native, chunks, true)?;
        }
        if !omitted
            && self.items.iter().any(|(id, item)| {
                !seen.contains(id) && !(truncated && item.kind() == ItemKind::ToolCall)
            })
        {
            return Err(protocol("terminal response omitted a streamed output item"));
        }
        if truncated {
            // Even syntactically complete tools are unsafe on an
            // abnormal stop; never expose them as executable calls.
            for item in self
                .items
                .values()
                .filter(|item| item.kind() == ItemKind::ToolCall)
            {
                chunks.push(ResponseChunk::ItemDiscarded {
                    id: item.native_id.clone(),
                });
            }
        }
        // Reasoning display stays open through item.done so the final
        // snapshot can supply previously absent readable content. Close
        // it from its actual received snapshot on omitted-output streams.
        if omitted {
            let snapshots: Vec<_> = self
                .items
                .iter()
                .filter(|(_, item)| !(truncated && item.kind() == ItemKind::ToolCall))
                .map(|(id, item)| (*id, item.snapshot().expect("checked ended").clone()))
                .collect();
            for (id, native) in snapshots {
                self.end(id, NativeItem::parse(&native)?, chunks, true)?;
            }
        }
        if let Some(usage) = usage {
            chunks.push(ResponseChunk::UsageUpdated { usage });
        }
        self.completed = true;
        let stop_reason = match outcome {
            TerminalOutcome::MaxTokens => StopReason::MaxTokens,
            TerminalOutcome::ContentFilter => StopReason::ContentFilter,
            TerminalOutcome::Incomplete => StopReason::Other("incomplete".into()),
            TerminalOutcome::Completed
                if self
                    .items
                    .values()
                    .any(|item| item.kind() == ItemKind::ToolCall) =>
            {
                StopReason::ToolUse
            }
            TerminalOutcome::Completed => StopReason::EndTurn,
        };
        chunks.push(ResponseChunk::ResponseEnded { stop_reason });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;
    use crate::provider::protocol::ResponseAssembler;

    #[test]
    fn abnormal_terminal_discards_tools_but_preserves_reasoning_and_usage() {
        for tag in ["response.incomplete", "response.completed"] {
            for codex in [false, true] {
                for output_done in [false, true] {
                    for (detail, reason) in [
                        ("max_output_tokens", StopReason::MaxTokens),
                        ("content_filter", StopReason::ContentFilter),
                    ] {
                        for args in ["{\"query\":", "{\"query\":\"rust\"}"] {
                            let mut decoder = if codex {
                                Decoder::codex("gpt-5".into())
                            } else {
                                Decoder::new("gpt-5".into())
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
                            let mut assembler = ResponseAssembler::default();
                            for frame in frames {
                                for chunk in decoder.feed(frame).unwrap() {
                                    assembler.push(&chunk).unwrap();
                                }
                            }
                            decoder.finish().unwrap();
                            let (items, usage, actual_reason) = assembler.finish().unwrap();
                            assert_eq!(actual_reason, reason);
                            assert_eq!(usage.output_tokens, 11);
                            assert_eq!(usage.input_tokens, 20 - cached);
                            assert_eq!(usage.cached_input_tokens, cached);
                            assert_eq!(items.len(), 2);
                            assert!(items.iter().all(|item| item.tool_call_ref().is_none()));
                            assert_eq!(items[1].text_content().as_deref(), Some("partial answer"));
                            assert_eq!(items[0].replay.as_ref().unwrap().payload, native);
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
                Decoder::codex("gpt-5".into())
            } else {
                Decoder::new("gpt-5".into())
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
