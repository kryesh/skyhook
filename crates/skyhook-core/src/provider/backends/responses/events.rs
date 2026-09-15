//! Native streaming event dispatch, shared by SSE and Codex WebSocket.
use super::normalization::NormalizedEvent;
use super::*;

impl Decoder {
    /// Feed a native Responses event (also used by Codex WebSocket transport).
    pub(crate) fn feed(&mut self, event: Value) -> Result<Vec<ResponseChunk>, ProviderError> {
        if self.completed {
            return Err(protocol("event after terminal response"));
        }
        let mut chunks = Vec::new();
        match self.normalize_event(&event, &mut chunks)? {
            NormalizedEvent::Ignored => {}
            NormalizedEvent::ItemAdded {
                index,
                wire,
                native,
            } => {
                self.start(index, native, &mut chunks)?;
                self.items.get_mut(&index).expect("started item").wire_index = wire;
            }
            NormalizedEvent::ItemDone { index, native } => {
                self.end(index, native, &mut chunks, false)?;
            }
            NormalizedEvent::Delta {
                part: (id, position),
                text,
            } => {
                self.delta(id, position, text, &mut chunks)?;
            }
            NormalizedEvent::ArgumentsDelta { item: id, text } => {
                if self.items[&id]
                    .function()?
                    .streaming()?
                    .final_arguments
                    .is_some()
                {
                    return Err(protocol("arguments delta after done"));
                }
                self.delta(id, 0, text, &mut chunks)?;
            }
            NormalizedEvent::ArgumentsDone { item: id, text } => {
                let arguments = match arguments(text) {
                    Ok(arguments) => arguments,
                    Err(_) => {
                        let item = self
                            .items
                            .get_mut(&id)
                            .expect("checked item")
                            .function_mut()?
                            .streaming_mut()?;
                        if item.final_arguments.is_some() {
                            return Err(protocol("conflicting final function arguments"));
                        }
                        item.final_arguments = Some(FinalArguments::Incomplete(text.into()));
                        return Ok(chunks);
                    }
                };
                self.validate_arguments(id, &arguments)?;
                let item = self
                    .items
                    .get_mut(&id)
                    .expect("checked item")
                    .function_mut()?
                    .streaming_mut()?;
                item.final_arguments = Some(FinalArguments::Object(arguments.clone()));
                if let (Some(call_id), Some(name)) = (&item.call_id, &item.name) {
                    let content = BlockContent::ToolCall(
                        ToolCall::new(call_id.clone(), name.clone(), Value::Object(arguments))
                            .map_err(|error| protocol(error.to_string()))?,
                    );
                    self.close_part(id, 0, content, &mut chunks)?;
                }
            }
            NormalizedEvent::PartEnded {
                part: (id, position),
                content,
            } => {
                self.close_part(id, position, content, &mut chunks)?;
            }
            NormalizedEvent::PartAdded {
                part: (id, position),
                text,
            } => {
                match self.part(id, position, &mut chunks)? {
                    Part::Streaming { text, added } if !*added && text.is_empty() => *added = true,
                    _ => return Err(protocol("duplicate or late content part added")),
                }
                if !text.is_empty() {
                    self.delta(id, position, text, &mut chunks)?;
                }
            }
            NormalizedEvent::Terminal {
                output,
                usage,
                outcome,
            } => {
                self.complete_response(output, usage, outcome, &mut chunks)?;
            }
        }
        Ok(chunks)
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

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
            matches!(&result[0], ResponseChunk::BlockEnded{content:BlockContent::ToolCall(call),..} if call.name() == "search")
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
