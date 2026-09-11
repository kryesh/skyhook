//! Native streaming event dispatch, shared by SSE and Codex WebSocket.
use super::*;

impl Decoder {
    /// Feed a native Responses event (also used by Codex WebSocket transport).
    pub(crate) fn feed(&mut self, mut event: Value) -> Result<Vec<ResponseChunk>, ProviderError> {
        if self.completed {
            return Err(protocol("event after terminal response"));
        }
        let mut chunks = Vec::new();
        let supplied_output_index = event
            .get("output_index")
            .map(|_| index(&event, "output_index"))
            .transpose()?;
        self.normalize_event(&mut event, &mut chunks)?;
        match string(&event, "type")? {
            "response.created" | "response.in_progress" | "response.queued" => {
                if !event.get("response").is_some_and(Value::is_object) {
                    return Err(protocol("missing response object"));
                }
            }
            "response.output_item.added" => {
                let id = index(&event, "output_index")?;
                self.start(
                    id,
                    event.get("item").ok_or_else(|| protocol("missing item"))?,
                    &mut chunks,
                )?;
                self.items.get_mut(&id).expect("started item").wire_index = supplied_output_index;
            }
            "response.output_item.done" => {
                let id = index(&event, "output_index")?;
                self.end(
                    id,
                    event.get("item").ok_or_else(|| protocol("missing item"))?,
                    &mut chunks,
                    false,
                )?;
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                let id = self.active(&event, Kind::Text)?;
                self.delta(
                    id,
                    index(&event, "content_index")?,
                    string(&event, "delta")?,
                    &mut chunks,
                )?;
            }
            "response.reasoning_text.delta" | "response.reasoning_text.done" => {
                let id = self.active(&event, Kind::Reasoning)?;
                let position = reasoning_position(index(&event, "content_index")?, true)?;
                if string(&event, "type")?.ends_with(".delta") {
                    self.delta(id, position, string(&event, "delta")?, &mut chunks)?;
                } else {
                    self.close_part(
                        id,
                        position,
                        BlockContent::Reasoning {
                            text: reasoning_text(&event)?.into(),
                        },
                        &mut chunks,
                    )?;
                }
            }
            "response.reasoning_summary_text.delta" => {
                let id = self.active(&event, Kind::Reasoning)?;
                self.delta(
                    id,
                    reasoning_position(index(&event, "summary_index")?, false)?,
                    string(&event, "delta")?,
                    &mut chunks,
                )?;
            }
            "response.function_call_arguments.delta" => {
                let id = self.active(&event, Kind::Function)?;
                if self.items[&id].final_arguments.is_some() {
                    return Err(protocol("arguments delta after done"));
                }
                self.delta(id, 0, string(&event, "delta")?, &mut chunks)?;
            }
            "response.function_call_arguments.done" => {
                let id = self.active(&event, Kind::Function)?;
                let text = string(&event, "arguments")?;
                if arguments(text).is_err() {
                    let item = self.items.get_mut(&id).expect("checked item");
                    if item.final_arguments.is_some() {
                        return Err(protocol("conflicting final function arguments"));
                    }
                    item.final_arguments = Some(text.into());
                    return Ok(chunks);
                }
                self.validate_arguments(id, text)?;
                let item = self.items.get_mut(&id).expect("checked item");
                item.final_arguments = Some(text.into());
                if let (Some(call_id), Some(name)) = (&item.call_id, &item.name) {
                    let content = BlockContent::ToolCall(ToolCall {
                        id: call_id.clone(),
                        name: name.clone(),
                        arguments: arguments(text)?,
                    });
                    self.close_part(id, 0, content, &mut chunks)?;
                }
            }
            "response.output_text.done"
            | "response.refusal.done"
            | "response.reasoning_summary_text.done" => {
                let reasoning = string(&event, "type")? == "response.reasoning_summary_text.done";
                let id = self.active(
                    &event,
                    if reasoning {
                        Kind::Reasoning
                    } else {
                        Kind::Text
                    },
                )?;
                let position = index(
                    &event,
                    if reasoning {
                        "summary_index"
                    } else {
                        "content_index"
                    },
                )?;
                let position = if reasoning {
                    reasoning_position(position, false)?
                } else {
                    position
                };
                let text = string(
                    &event,
                    if string(&event, "type")? == "response.refusal.done" {
                        "refusal"
                    } else {
                        "text"
                    },
                )?
                .into();
                let content = if reasoning {
                    BlockContent::Reasoning { text }
                } else {
                    BlockContent::Text { text }
                };
                self.close_part(id, position, content, &mut chunks)?;
            }
            "response.content_part.added"
            | "response.content_part.done"
            | "response.reasoning_part.added"
            | "response.reasoning_part.done"
            | "response.reasoning_summary_part.added"
            | "response.reasoning_summary_part.done" => {
                let name = string(&event, "type")?;
                let summary = name.starts_with("response.reasoning_summary_part.");
                let output_index = index(&event, "output_index")?;
                let expected = if name.starts_with("response.reasoning_") {
                    Kind::Reasoning
                } else {
                    self.items
                        .get(&output_index)
                        .ok_or_else(|| protocol("event for an unstarted output item"))?
                        .kind
                };
                if expected == Kind::Function {
                    return Err(protocol("content part on function item"));
                }
                let id = self.active(&event, expected)?;
                let reasoning = expected == Kind::Reasoning;
                let position = index(
                    &event,
                    if summary {
                        "summary_index"
                    } else {
                        "content_index"
                    },
                )?;
                let position = if reasoning {
                    reasoning_position(position, !summary)?
                } else {
                    position
                };
                let part = event
                    .get("part")
                    .ok_or_else(|| protocol("missing content part"))?;
                let text = if reasoning {
                    readable_reasoning(part, summary)?
                } else {
                    match string(part, "type")? {
                        "output_text" => string(part, "text")?,
                        "refusal" => string(part, "refusal")?,
                        _ => return Err(protocol("unsupported content part")),
                    }
                }
                .to_owned();
                if name.ends_with(".done") {
                    let content = if reasoning {
                        BlockContent::Reasoning { text }
                    } else {
                        BlockContent::Text { text }
                    };
                    self.close_part(id, position, content, &mut chunks)?;
                } else {
                    let part = self.part(id, position, &mut chunks);
                    if part.added || part.ended.is_some() || !part.streamed.is_empty() {
                        return Err(protocol("duplicate or late content part added"));
                    }
                    part.added = true;
                    if !text.is_empty() {
                        self.delta(id, position, &text, &mut chunks)?;
                    }
                }
            }
            "response.output_text.annotation.added" => {
                // Annotations decorate text; they are not independent assistant content.
                self.active(&event, Kind::Text)?;
                index(&event, "content_index")?;
                index(&event, "annotation_index")?;
                if !event.get("annotation").is_some_and(Value::is_object) {
                    return Err(protocol("missing annotation object"));
                }
            }
            "response.completed" | "response.incomplete" => {
                self.complete_response(&event, &mut chunks)?;
            }
            "response.failed" => {
                let response = event
                    .get("response")
                    .ok_or_else(|| protocol("missing failed response"))?;
                return Err(api_error(
                    response
                        .get("error")
                        .ok_or_else(|| protocol("missing response error"))?,
                ));
            }
            "error" => return Err(api_error(event.get("error").unwrap_or(&event))),
            other => {
                let name: String = other
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_'))
                    .take(96)
                    .collect();
                return Err(protocol(format!("unsupported event: {name}")));
            }
        }
        Ok(chunks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call_item() -> Value {
        json!({"type":"function_call", "id":"fc_1", "call_id":"call_1", "name":"search",
            "arguments":"{\"query\":\"rust\"}", "status":"completed"})
    }

    fn completed(output: Vec<Value>) -> Value {
        json!({"type":"response.completed", "response":{"status":"completed", "output":output,
            "usage":{"input_tokens":20, "output_tokens":7, "input_tokens_details":{"cached_tokens":12}}}})
    }

    fn added(id: usize, item: Value) -> Value {
        json!({"type":"response.output_item.added", "output_index":id, "item":item})
    }

    fn done(id: usize, item: Value) -> Value {
        json!({"type":"response.output_item.done", "output_index":id, "item":item})
    }

    #[test]
    fn function_arguments_are_typed_validated_and_identity_can_arrive_at_item_end() {
        let mut decoder = Decoder::new("gpt-5".into());
        decoder
            .feed(added(
                0,
                json!({"id":"fc_1","type":"function_call","arguments":""}),
            ))
            .unwrap();
        let chunks = decoder.feed(json!({"type":"response.function_call_arguments.delta", "output_index":0, "item_id":"fc_1", "delta":"{\"query\":\"rust\"}"})).unwrap();
        assert!(
            matches!(&chunks[1], ResponseChunk::BlockDelta{delta:ContentDelta::JsonFragment(text),..} if text == "{\"query\":\"rust\"}")
        );
        assert!(decoder.feed(json!({"type":"response.function_call_arguments.done", "output_index":0, "item_id":"fc_1", "arguments":"{\"query\":\"rust\"}"})).unwrap().is_empty());
        let result = decoder.feed(done(0, call_item())).unwrap();
        assert!(
            matches!(&result[0], ResponseChunk::BlockEnded{content:BlockContent::ToolCall(call),..} if call.name == "search")
        );
        assert!(matches!(&result[1], ResponseChunk::ItemEnded { .. }));
        let terminal = decoder.feed(completed(vec![call_item()])).unwrap();
        assert!(matches!(
            terminal.last(),
            Some(ResponseChunk::ResponseEnded {
                stop_reason: StopReason::ToolUse
            })
        ));
    }
}
