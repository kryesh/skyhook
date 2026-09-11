//! Terminal status, output completeness, usage accounting, and safe tool discard.
use super::*;

impl Decoder {
    pub(super) fn complete_response(
        &mut self,
        event: &Value,
        chunks: &mut Vec<ResponseChunk>,
    ) -> Result<(), ProviderError> {
        let response = event
            .get("response")
            .ok_or_else(|| protocol("missing final response"))?;
        // A terminal event ends the stream; its status determines whether
        // the generation succeeded. Never turn an incomplete snapshot into
        // a successful tool-bearing response merely because of its tag.
        let truncated = match string(response, "status")? {
            "incomplete" => true,
            "completed" if string(event, "type")? == "response.completed" => false,
            _ => return Err(protocol("terminal response status disagrees with event")),
        };
        if truncated {
            let details = response
                .get("incomplete_details")
                .ok_or_else(|| protocol("missing incomplete details"))?;
            match string(details, "reason")? {
                "max_output_tokens" | "content_filter" => {}
                _ => return Err(protocol("unsupported incomplete reason")),
            }
        }
        let empty = Vec::new();
        let output = if self.allow_omitted_terminal_output && response.get("output").is_none() {
            &empty
        } else {
            array(response, "output")?
        };
        let omitted = self.allow_omitted_terminal_output
            && output.is_empty()
            && self
                .items
                .values()
                .all(|item| item.ended.is_some() || (truncated && item.kind == Kind::Function));
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
        for native in output {
            // Stable native IDs take precedence over terminal array
            // position. Regenerated IDs require unique semantic evidence.
            let id = self.snapshot_index(native, None, Some(&alias_candidates), chunks)?;
            if !seen.insert(id) {
                return Err(protocol("duplicate terminal output item"));
            }
            if truncated && self.items[&id].kind == Kind::Function {
                if kind(native)? != Kind::Function {
                    return Err(protocol("final output item identity changed"));
                }
                continue;
            }
            self.end(id, native, chunks, true)?;
        }
        if !omitted
            && self
                .items
                .iter()
                .any(|(id, item)| !seen.contains(id) && !(truncated && item.kind == Kind::Function))
        {
            return Err(protocol("terminal response omitted a streamed output item"));
        }
        if truncated {
            // Even syntactically complete tools are unsafe on an
            // abnormal stop; never expose them as executable calls.
            for item in self
                .items
                .values()
                .filter(|item| item.kind == Kind::Function)
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
                .filter(|(_, item)| !(truncated && item.kind == Kind::Function))
                .map(|(id, item)| (*id, item.ended.clone().expect("checked ended")))
                .collect();
            for (id, native) in snapshots {
                self.end(id, &native, chunks, true)?;
            }
        }
        if let Some(usage) = response.get("usage").filter(|u| !u.is_null()) {
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
            let total_input = count("input_tokens")?;
            let uncached_input = total_input
                .checked_sub(cached)
                .ok_or_else(|| protocol("cached tokens exceed input tokens"))?;
            // Internal input_tokens excludes reads served from cache.
            let usage = Usage {
                input_tokens: uncached_input,
                cached_input_tokens: cached,
                output_tokens: count("output_tokens")?,
            };
            chunks.push(ResponseChunk::UsageUpdated { usage });
        }
        self.completed = true;
        let stop_reason = if truncated {
            match response["incomplete_details"]["reason"].as_str() {
                Some("max_output_tokens") => StopReason::MaxTokens,
                Some("content_filter") => StopReason::ContentFilter,
                _ => unreachable!("validated reason"),
            }
        } else if self.items.values().any(|item| item.kind == Kind::Function) {
            StopReason::ToolUse
        } else {
            StopReason::EndTurn
        };
        chunks.push(ResponseChunk::ResponseEnded { stop_reason });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::protocol::ResponseAssembler;

    fn completed(output: Vec<Value>) -> Value {
        json!({"type":"response.completed", "response":{"status":"completed", "output":output,
            "usage":{"input_tokens":20, "output_tokens":7, "input_tokens_details":{"cached_tokens":12}}}})
    }

    fn done(id: usize, item: Value) -> Value {
        json!({"type":"response.output_item.done", "output_index":id, "item":item})
    }

    fn added(id: usize, item: Value) -> Value {
        json!({"type":"response.output_item.added", "output_index":id, "item":item})
    }

    fn call_item() -> Value {
        json!({"type":"function_call", "id":"fc_1", "call_id":"call_1", "name":"search",
            "arguments":"{\"query\":\"rust\"}", "status":"completed"})
    }

    fn reasoning_item() -> Value {
        json!({"type":"reasoning", "id":"rs_1", "encrypted_content":"secret",
            "summary":[{"type":"summary_text", "text":"first"}, {"type":"summary_text", "text":"second"}]})
    }

    fn message(id: &str, text: &str) -> Value {
        json!({"type":"message", "id":id, "role":"assistant", "status":"completed",
            "content":[{"type":"output_text", "text":text, "annotations":[]}]})
    }

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
