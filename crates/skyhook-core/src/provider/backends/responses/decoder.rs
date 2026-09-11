//! Stateful item/block lifecycle and HTTP/SSE framing validation.
use super::*;

impl Decoder {
    pub(crate) fn new(model: String) -> Self {
        Self {
            allow_omitted_terminal_output: false,
            model,
            items: BTreeMap::new(),
            completed: false,
        }
    }

    pub(crate) fn codex(model: String) -> Self {
        Self {
            allow_omitted_terminal_output: true,
            ..Self::new(model)
        }
    }

    pub(crate) fn decode(
        &mut self,
        event: &crate::provider::backends::transport::SseEvent,
    ) -> Result<Vec<ResponseChunk>, ProviderError> {
        self.decode_filtered(event, |_| false)
    }

    pub(in crate::provider::backends) fn decode_filtered(
        &mut self,
        event: &crate::provider::backends::transport::SseEvent,
        ignore: impl FnOnce(&Value) -> bool,
    ) -> Result<Vec<ResponseChunk>, ProviderError> {
        if event.data.trim() == "[DONE]" {
            return if self.completed {
                Ok(vec![])
            } else {
                Err(protocol("[DONE] before terminal response"))
            };
        }
        let value: Value =
            serde_json::from_str(&event.data).map_err(|_| protocol("invalid SSE JSON"))?;
        if ignore(&value) {
            return Ok(vec![]);
        }
        if let Some(name) = &event.event
            && !name.is_empty()
            && name != "message"
            && Some(name.as_str()) != value.get("type").and_then(Value::as_str)
        {
            return Err(protocol("SSE event name disagrees with payload type"));
        }
        self.feed(value)
    }

    pub(super) fn start(
        &mut self,
        id: usize,
        item: &Value,
        chunks: &mut Vec<ResponseChunk>,
    ) -> Result<(), ProviderError> {
        if self.items.contains_key(&id) {
            return Err(protocol("duplicate output item index"));
        }
        let native_id = string(item, "id")?.to_owned();
        if native_id.is_empty()
            || self
                .items
                .values()
                .any(|old| old.native_id == native_id || old.aliases.contains(&native_id))
        {
            return Err(protocol("empty or duplicate output item ID"));
        }
        let item_kind = kind(item)?;
        self.items.insert(
            id,
            Item {
                native_id: native_id.clone(),
                aliases: BTreeSet::new(),
                wire_index: Some(id),
                kind: item_kind,
                ended: None,
                parts: BTreeMap::new(),
                reasoning_aliases: BTreeMap::new(),
                call_id: item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned),
                name: item
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned),
                final_arguments: None,
            },
        );
        chunks.push(ResponseChunk::ItemStarted {
            id: native_id,
            position: id,
            kind: item_kind.item_kind(),
        });
        Ok(())
    }

    pub(super) fn part(
        &mut self,
        id: usize,
        position: usize,
        chunks: &mut Vec<ResponseChunk>,
    ) -> &mut Part {
        let item = self.items.get_mut(&id).expect("checked item");
        item.parts.entry(position).or_insert_with(|| {
            chunks.push(ResponseChunk::BlockStarted {
                item: item.native_id.clone(),
                id: item.kind.part_id(position),
                position,
                kind: item.kind.block_kind(),
            });
            Part::default()
        })
    }

    pub(super) fn delta(
        &mut self,
        id: usize,
        position: usize,
        text: &str,
        chunks: &mut Vec<ResponseChunk>,
    ) -> Result<(), ProviderError> {
        let part = self.part(id, position, chunks);
        if part.ended.is_some() {
            return Err(protocol("delta after content part ended"));
        }
        part.streamed.push_str(text);
        let item = &self.items[&id];
        chunks.push(ResponseChunk::BlockDelta {
            item: item.native_id.clone(),
            block: item.kind.part_id(position),
            delta: if item.kind == Kind::Function {
                ContentDelta::JsonFragment(text.into())
            } else {
                ContentDelta::Text(text.into())
            },
        });
        Ok(())
    }

    pub(super) fn close_part(
        &mut self,
        id: usize,
        position: usize,
        content: BlockContent,
        chunks: &mut Vec<ResponseChunk>,
    ) -> Result<(), ProviderError> {
        let part = self.part(id, position, chunks);
        if let Some(old) = &part.ended {
            if old != &content {
                return Err(protocol("conflicting final content part"));
            }
            return Ok(());
        }
        if !part.streamed.is_empty() {
            let valid = match &content {
                BlockContent::Text { text } | BlockContent::Reasoning { text } => {
                    text == &part.streamed
                }
                BlockContent::ToolCall(call) => {
                    arguments(&part.streamed).ok().as_ref() == Some(&call.arguments)
                }
            };
            if !valid {
                return Err(protocol("final content disagrees with streamed deltas"));
            }
        }
        part.ended = Some(content.clone());
        let item = &self.items[&id];
        chunks.push(ResponseChunk::BlockEnded {
            item: item.native_id.clone(),
            block: item.kind.part_id(position),
            content,
        });
        Ok(())
    }

    pub(super) fn active(&self, event: &Value, expected: Kind) -> Result<usize, ProviderError> {
        let id = index(event, "output_index")?;
        let item = self
            .items
            .get(&id)
            .ok_or_else(|| protocol("event for an unstarted output item"))?;
        if item.ended.is_some() {
            return Err(protocol("event after output item ended"));
        }
        if item.kind != expected {
            return Err(protocol("event does not match output item kind"));
        }
        if string(event, "item_id")? != item.native_id {
            return Err(protocol("event item ID mismatch"));
        }
        Ok(id)
    }

    pub(super) fn end(
        &mut self,
        id: usize,
        native: &Value,
        chunks: &mut Vec<ResponseChunk>,
        terminal: bool,
    ) -> Result<(), ProviderError> {
        let item = self
            .items
            .get(&id)
            .ok_or_else(|| protocol("end of unstarted output item"))?;
        if (string(native, "id")? != item.native_id
            && !item.aliases.contains(string(native, "id")?))
            || kind(native)? != item.kind
        {
            return Err(protocol("final output item identity changed"));
        }
        if item.kind == Kind::Reasoning {
            return self.end_reasoning(id, native, chunks, terminal);
        }
        // A tool may end with partial JSON before the terminal max-token or
        // filter reason arrives. Keep it provisional; an abnormal stop discards
        // it, while a normal terminal response must still validate its JSON.
        if !terminal
            && item.kind == Kind::Function
            && arguments(string(native, "arguments")?).is_err()
        {
            if item.ended.is_some() {
                return Err(protocol("duplicate final output item"));
            }
            self.items.get_mut(&id).expect("checked item").ended = Some(native.clone());
            return Ok(());
        }
        let parts = final_parts(native)?;
        if let Some(old) = &item.ended {
            if final_parts(old)? != parts {
                return Err(protocol("conflicting final output item"));
            }
            return Ok(());
        }
        if item.parts.keys().any(|position| *position >= parts.len()) {
            return Err(protocol("final item omitted a streamed content part"));
        }
        if item.kind == Kind::Function {
            if self.items.iter().any(|(other_id, other)| {
                *other_id != id
                    && other.kind == Kind::Function
                    && other
                        .ended
                        .as_ref()
                        .is_some_and(|other| other.get("call_id") == native.get("call_id"))
            }) {
                return Err(protocol("duplicate function call ID"));
            }
            if item
                .call_id
                .as_deref()
                .is_some_and(|id| Some(id) != native.get("call_id").and_then(Value::as_str))
                || item
                    .name
                    .as_deref()
                    .is_some_and(|name| Some(name) != native.get("name").and_then(Value::as_str))
            {
                return Err(protocol("final function identity changed"));
            }
            self.validate_arguments(id, string(native, "arguments")?)?;
        }
        for (position, content) in parts.into_iter().enumerate() {
            self.close_part(id, position, content, chunks)?;
        }
        let item = self.items.get_mut(&id).expect("checked item");
        item.ended = Some(native.clone());
        chunks.push(ResponseChunk::ItemEnded {
            id: item.native_id.clone(),
            replay: None,
        });
        Ok(())
    }

    pub(super) fn validate_arguments(&self, id: usize, text: &str) -> Result<(), ProviderError> {
        let value = arguments(text)?;
        let item = &self.items[&id];
        if let Some(old) = &item.final_arguments
            && arguments(old)? != value
        {
            return Err(protocol("conflicting final function arguments"));
        }
        if let Some(part) = item.parts.get(&0)
            && !part.streamed.is_empty()
            && arguments(&part.streamed).ok().as_ref() != Some(&value)
        {
            return Err(protocol("final function arguments disagree with deltas"));
        }
        Ok(())
    }

    pub(crate) fn finish(&mut self) -> Result<Vec<ResponseChunk>, ProviderError> {
        if !self.completed {
            return Err(protocol("stream ended before a terminal response"));
        }
        Ok(vec![])
    }
}

pub(super) fn api_error(error: &Value) -> ProviderError {
    let code = error.get("code").and_then(Value::as_str).unwrap_or("");
    let kind = match code {
        "context_length_exceeded" | "context_window_exceeded" => {
            ProviderErrorKind::ContextWindowExceeded
        }
        "invalid_api_key" | "authentication_error" => ProviderErrorKind::Authentication,
        "rate_limit_exceeded" | "rate_limit_error" => ProviderErrorKind::RateLimited,
        "timeout" | "request_timeout" => ProviderErrorKind::Timeout,
        _ => ProviderErrorKind::Response,
    };
    // Never echo upstream messages or arbitrary codes: they may reflect prompts
    // or credentials. Only the locally classified category is safe to surface.
    ProviderError {
        kind,
        message: format!("Responses request failed ({kind:?})"),
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

    fn reasoning_item() -> Value {
        json!({"type":"reasoning", "id":"rs_1", "encrypted_content":"secret",
            "summary":[{"type":"summary_text", "text":"first"}, {"type":"summary_text", "text":"second"}]})
    }

    fn text_item(id: &str, text: &str) -> Value {
        json!({"type":"message", "id":id, "role":"assistant", "status":"completed",
            "content":[{"type":"output_text", "text":text, "annotations":[]}]})
    }

    #[test]
    fn out_of_order_items_keep_provider_indices_and_authoritative_blocks() {
        let mut decoder = Decoder::new("gpt-5".into());
        let mut assembler = crate::provider::protocol::ResponseAssembler::default();
        let reasoning = reasoning_item();
        let text = text_item("msg_1", "authoritative");
        for event in [
            added(1, text_item("msg_1", "")),
            added(0, reasoning.clone()),
            json!({"type":"response.output_text.delta", "output_index":1, "item_id":"msg_1", "content_index":0, "delta":"authoritative"}),
            done(1, text.clone()),
            completed(vec![reasoning, text]),
        ] {
            for chunk in decoder.feed(event).unwrap() {
                assembler.push(&chunk).unwrap();
            }
        }
        let (items, usage, stop) = assembler.finish().unwrap();
        assert_eq!(items[0].id, "rs_1");
        assert_eq!(items[1].text_content().as_deref(), Some("authoritative"));
        assert_eq!(
            usage,
            Usage {
                input_tokens: 8,
                cached_input_tokens: 12,
                output_tokens: 7
            }
        );
        assert_eq!(stop, StopReason::EndTurn);
        assert!(decoder.finish().unwrap().is_empty());
    }

    #[test]
    fn sse_validation_eof_and_errors_are_not_silent_success() {
        let mut decoder = Decoder::new("gpt-5".into());
        assert!(decoder.finish().is_err());
        for (name, data) in [
            (None, "[DONE]"),
            (None, "{"),
            (
                Some("response.created"),
                "{\"type\":\"response.completed\"}",
            ),
        ] {
            assert!(
                decoder
                    .decode(&crate::provider::backends::transport::SseEvent {
                        event: name.map(str::to_owned),
                        data: data.into()
                    })
                    .is_err()
            );
        }
        for code in [
            "context_length_exceeded",
            "rate_limit_exceeded",
            "invalid_api_key",
            "server_error",
        ] {
            let error = decoder
                .feed(json!({"type":"error", "code":code,"message":"details"}))
                .unwrap_err();
            assert!(!error.message.contains("details"));
            assert_eq!(
                error.kind,
                match code {
                    "context_length_exceeded" => ProviderErrorKind::ContextWindowExceeded,
                    "rate_limit_exceeded" => ProviderErrorKind::RateLimited,
                    "invalid_api_key" => ProviderErrorKind::Authentication,
                    _ => ProviderErrorKind::Response,
                }
            );
        }
        decoder
            .decode(&crate::provider::backends::transport::SseEvent {
                event: Some("response.completed".into()),
                data: completed(vec![]).to_string(),
            })
            .unwrap();
        assert!(
            decoder
                .decode(&crate::provider::backends::transport::SseEvent {
                    event: None,
                    data: "[DONE]".into()
                })
                .unwrap()
                .is_empty()
        );
        assert!(decoder.feed(completed(vec![])).is_err());
    }

    #[test]
    fn identical_item_done_is_idempotent() {
        let output = text_item("msg", "hello");
        let mut decoder = Decoder::new("gpt-5".into());
        let mut assembler = ResponseAssembler::default();
        for event in [
            done(0, output.clone()),
            done(0, output.clone()),
            completed(vec![output]),
        ] {
            for chunk in decoder.feed(event).unwrap() {
                assembler.push(&chunk).unwrap();
            }
        }
        let (items, _, _) = assembler.finish().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text_content().as_deref(), Some("hello"));
    }

    #[test]
    fn empty_sse_event_name_means_default_message_framing() {
        let mut decoder = Decoder::new("test-model".into());
        let mut assembler = ResponseAssembler::default();
        let event = crate::provider::backends::transport::SseEvent {
            event: Some(String::new()),
            data: completed(vec![text_item("msg", "hello")]).to_string(),
        };
        for chunk in decoder.decode(&event).unwrap() {
            assembler.push(&chunk).unwrap();
        }
        let (items, _, _) = assembler.finish().unwrap();
        assert_eq!(items[0].text_content().as_deref(), Some("hello"));
    }
}
