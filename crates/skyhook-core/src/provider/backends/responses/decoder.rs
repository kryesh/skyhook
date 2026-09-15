//! Stateful item/block lifecycle and HTTP/SSE framing validation.
use super::native::NativeItem;
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
        item: NativeItem<'_>,
        chunks: &mut Vec<ResponseChunk>,
    ) -> Result<(), ProviderError> {
        self.start_item(
            id,
            item.id,
            item.kind,
            (
                item.raw.get("call_id").and_then(Value::as_str),
                item.raw.get("name").and_then(Value::as_str),
            ),
            chunks,
        )
    }

    pub(super) fn start_item(
        &mut self,
        id: usize,
        native_id: &str,
        item_kind: ItemKind,
        identity: (Option<&str>, Option<&str>),
        chunks: &mut Vec<ResponseChunk>,
    ) -> Result<(), ProviderError> {
        if self.items.contains_key(&id) {
            return Err(protocol("duplicate output item index"));
        }
        if native_id.is_empty()
            || self
                .items
                .values()
                .any(|old| old.native_id == native_id || old.aliases.contains(native_id))
        {
            return Err(protocol("empty or duplicate output item ID"));
        }
        self.items.insert(
            id,
            Item {
                native_id: native_id.into(),
                aliases: BTreeSet::new(),
                wire_index: Some(id),
                body: match item_kind {
                    ItemKind::Text => ItemBody::Text(TextState::default()),
                    ItemKind::Reasoning => ItemBody::Reasoning(ReasoningState::default()),
                    ItemKind::ToolCall => ItemBody::Function(FunctionState {
                        phase: FunctionPhase::Streaming(StreamingFunction {
                            call_id: identity.0.filter(|s| !s.is_empty()).map(str::to_owned),
                            name: identity.1.filter(|s| !s.is_empty()).map(str::to_owned),
                            final_arguments: None,
                        }),
                        part: None,
                    }),
                },
            },
        );
        chunks.push(ResponseChunk::ItemStarted {
            id: native_id.into(),
            position: id,
            kind: item_kind,
        });
        Ok(())
    }

    pub(super) fn part(
        &mut self,
        id: usize,
        position: usize,
        chunks: &mut Vec<ResponseChunk>,
    ) -> Result<&mut Part, ProviderError> {
        let item = self.items.get_mut(&id).expect("checked item");
        let kind = item.kind();
        let start = || {
            chunks.push(ResponseChunk::BlockStarted {
                item: item.native_id.clone(),
                id: kind.part_id(position),
                position,
                kind: kind.block_kind(),
            });
            Part::default()
        };
        Ok(match &mut item.body {
            ItemBody::Text(state) => state.parts.entry(position).or_insert_with(start),
            ItemBody::Reasoning(state) => state.parts.entry(position).or_insert_with(start),
            ItemBody::Function(state) => {
                if position != 0 {
                    return Err(protocol("invalid function part index"));
                }
                state.part.get_or_insert_with(start)
            }
        })
    }

    pub(super) fn delta(
        &mut self,
        id: usize,
        position: usize,
        text: &str,
        chunks: &mut Vec<ResponseChunk>,
    ) -> Result<(), ProviderError> {
        let Part::Streaming { text: streamed, .. } = self.part(id, position, chunks)? else {
            return Err(protocol("delta after content part ended"));
        };
        streamed.push_str(text);
        let item = &self.items[&id];
        chunks.push(ResponseChunk::BlockDelta {
            item: item.native_id.clone(),
            block: item.kind().part_id(position),
            delta: match item.body {
                ItemBody::Function(_) => ContentDelta::JsonFragment(text.into()),
                _ => ContentDelta::Text(text.into()),
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
        let part = self.part(id, position, chunks)?;
        if let Part::Completed(old) = part {
            if old != &content {
                return Err(protocol("conflicting final content part"));
            }
            return Ok(());
        }
        let streamed = part.streamed();
        if !streamed.is_empty() {
            let valid = match &content {
                BlockContent::Text { text } | BlockContent::Reasoning { text } => text == streamed,
                BlockContent::ToolCall(call) => {
                    arguments(streamed).ok().as_ref() == Some(call.arguments())
                }
            };
            if !valid {
                return Err(protocol("final content disagrees with streamed deltas"));
            }
        }
        *part = Part::Completed(content.clone());
        let item = &self.items[&id];
        chunks.push(ResponseChunk::BlockEnded {
            item: item.native_id.clone(),
            block: item.kind().part_id(position),
            content,
        });
        Ok(())
    }

    pub(super) fn end(
        &mut self,
        id: usize,
        header: NativeItem<'_>,
        chunks: &mut Vec<ResponseChunk>,
        terminal: bool,
    ) -> Result<(), ProviderError> {
        let native = header.raw;
        let item = self
            .items
            .get(&id)
            .ok_or_else(|| protocol("end of unstarted output item"))?;
        if (header.id != item.native_id && !item.aliases.contains(header.id))
            || header.kind != item.kind()
        {
            return Err(protocol("final output item identity changed"));
        }
        match &item.body {
            ItemBody::Reasoning(_) => return self.end_reasoning(id, native, chunks, terminal),
            ItemBody::Function(_) => return self.end_function(id, native, chunks, terminal),
            ItemBody::Text(_) => {}
        }
        let parts = header.final_parts()?;
        if let Some(old) = item.snapshot() {
            if final_parts(old)? != parts {
                return Err(protocol("conflicting final output item"));
            }
            return Ok(());
        }
        if item.parts().any(|(position, _)| *position >= parts.len()) {
            return Err(protocol("final item omitted a streamed content part"));
        }
        for (position, content) in parts.into_iter().enumerate() {
            self.close_part(id, position, content, chunks)?;
        }
        let item = self.items.get_mut(&id).expect("checked item");
        if let ItemBody::Text(state) = &mut item.body {
            state.snapshot = Some(native.clone());
        }
        chunks.push(ResponseChunk::ItemEnded {
            id: item.native_id.clone(),
            replay: None,
        });
        Ok(())
    }

    fn end_function(
        &mut self,
        id: usize,
        native: &Value,
        chunks: &mut Vec<ResponseChunk>,
        terminal: bool,
    ) -> Result<(), ProviderError> {
        let state = self.items[&id].function()?;
        // A partial/non-object call can precede the terminal stop reason. It
        // must stay provisional, never become a completed executable ToolCall.
        let call = match native::function_call(native) {
            Ok(call) => call,
            Err(error) => {
                if !terminal && arguments(string(native, "arguments")?).is_err() {
                    if state.snapshot().is_some() {
                        return Err(protocol("duplicate final output item"));
                    }
                    self.items
                        .get_mut(&id)
                        .expect("checked item")
                        .function_mut()?
                        .phase = FunctionPhase::Provisional(native.clone());
                    return Ok(());
                }
                return Err(error);
            }
        };
        match &state.phase {
            FunctionPhase::Completed { call: old, .. } => {
                if old != &call {
                    return Err(protocol("conflicting final output item"));
                }
                return Ok(());
            }
            FunctionPhase::Provisional(old) => {
                // Preserve the rejection of a malformed prior item.done even
                // if a later normal terminal supplies a valid object.
                native::function_call(old)?;
                return Err(protocol("conflicting final output item"));
            }
            FunctionPhase::Streaming(_) => {}
        }
        if self.items.iter().any(|(other_id, other)| {
            *other_id != id
                && other
                    .function()
                    .ok()
                    .and_then(FunctionState::snapshot)
                    .is_some_and(|other| other.get("call_id") == native.get("call_id"))
        }) {
            return Err(protocol("duplicate function call ID"));
        }
        let streaming = state.streaming()?;
        if streaming
            .call_id
            .as_deref()
            .is_some_and(|id| id != call.id())
            || streaming
                .name
                .as_deref()
                .is_some_and(|name| name != call.name())
        {
            return Err(protocol("final function identity changed"));
        }
        self.validate_arguments(id, call.arguments())?;
        self.close_part(id, 0, BlockContent::ToolCall(call.clone()), chunks)?;
        let item = self.items.get_mut(&id).expect("checked item");
        item.function_mut()?.phase = FunctionPhase::Completed {
            native: native.clone(),
            call,
        };
        chunks.push(ResponseChunk::ItemEnded {
            id: item.native_id.clone(),
            replay: None,
        });
        Ok(())
    }

    pub(super) fn validate_arguments(
        &self,
        id: usize,
        value: &serde_json::Map<String, Value>,
    ) -> Result<(), ProviderError> {
        let state = self.items[&id].function()?;
        if let Some(old) = &state.streaming()?.final_arguments {
            let matches = match old {
                FinalArguments::Object(old) => old == value,
                FinalArguments::Incomplete(old) => &arguments(old)? == value,
            };
            if !matches {
                return Err(protocol("conflicting final function arguments"));
            }
        }
        // A completed part implies a completed phase, rejected by `streaming()` above.
        if let Some(Part::Streaming { text, .. }) = &state.part
            && !text.is_empty()
            && arguments(text).ok().as_ref() != Some(value)
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
    let kind = super::super::errors::classify_error(None, error).kind;
    // Keep useful machine diagnostics, but never arbitrary upstream messages.
    let mut message = format!("Responses request failed ({kind:?})");
    if let Some(code) = super::super::errors::safe_error_code(error) {
        message.push_str(&format!(" [code={code}]"));
    }
    ProviderError {
        kind,
        message,
        retry_after: None,
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;
    use crate::provider::backends::transport::SseEvent;
    use crate::provider::protocol::ResponseAssembler;

    fn feed_into(
        decoder: &mut Decoder,
        assembler: &mut ResponseAssembler,
        events: impl IntoIterator<Item = Value>,
    ) {
        for event in events {
            for chunk in decoder.feed(event).unwrap() {
                assembler.push(&chunk).unwrap();
            }
        }
    }

    fn sse(event: Option<&str>, data: impl Into<String>) -> SseEvent {
        SseEvent {
            event: event.map(str::to_owned),
            data: data.into(),
        }
    }

    #[test]
    fn function_completion_replaces_partial_identity_with_validated_call() {
        let mut decoder = Decoder::new("model".into());
        decoder
            .feed(added(0, json!({"type":"function_call", "id":"f"})))
            .unwrap();
        let chunks = decoder
            .feed(json!({"type":"response.function_call_arguments.done",
            "item_id":"f", "arguments":"{\"n\":1}"}))
            .unwrap();
        assert!(chunks.is_empty()); // Identity can legitimately arrive at item.done.
        let state = decoder.items[&0].function().unwrap();
        assert!(
            matches!(&state.streaming().unwrap().final_arguments, Some(FinalArguments::Object(value)) if value["n"] == 1)
        );
        assert!(state.part.is_none());
        let native = function("f", "call", "{\"n\":1}");
        let chunks = decoder.feed(done(0, native.clone())).unwrap();
        assert!(matches!(
            chunks.as_slice(),
            [
                ResponseChunk::BlockStarted { .. },
                ResponseChunk::BlockEnded { .. },
                ResponseChunk::ItemEnded { .. }
            ]
        ));
        let state = decoder.items[&0].function().unwrap();
        assert!(state.streaming().is_err());
        let FunctionPhase::Completed { call, .. } = &state.phase else {
            panic!("function call did not complete");
        };
        assert_eq!((call.id(), call.name()), ("call", "lookup"));
        assert_eq!(call.arguments()["n"], 1);
        assert!(matches!(
            state.part,
            Some(Part::Completed(BlockContent::ToolCall(_)))
        ));
        // Repeated semantically identical snapshots are idempotent.
        assert!(decoder.feed(done(0, native.clone())).unwrap().is_empty());
        decoder.feed(completed(vec![native])).unwrap();
        decoder.finish().unwrap();
    }

    #[test]
    fn provisional_function_snapshots_are_discarded_only_on_abnormal_stop() {
        let call = |arguments: Option<&str>| {
            let mut call =
                json!({"type":"function_call", "id":"f", "call_id":"call", "name":"run"});
            if let Some(arguments) = arguments {
                call["arguments"] = json!(arguments);
            }
            call
        };
        for reason in ["max_output_tokens", "content_filter"] {
            let mut decoder = Decoder::codex("model".into());
            let mut assembler = ResponseAssembler::default();
            feed_into(
                &mut decoder,
                &mut assembler,
                [
                    added(0, call(None)),
                    json!({"type":"response.function_call_arguments.delta", "item_id":"f", "delta":"{"}),
                    json!({"type":"response.function_call_arguments.done", "item_id":"f", "arguments":"{"}),
                    done(0, call(Some("{"))),
                ],
            );
            let state = decoder.items[&0].function().unwrap();
            assert!(matches!(state.phase, FunctionPhase::Provisional(_)));

            let chunks = decoder
                .feed(json!({"type":"response.incomplete", "response":{
                "status":"incomplete", "output":[], "incomplete_details":{"reason":reason}}}))
                .unwrap();
            let discarded = |chunk: &ResponseChunk| matches!(chunk, ResponseChunk::ItemDiscarded { id } if id == "f");
            assert!(chunks.iter().any(discarded));
            for chunk in chunks {
                assembler.push(&chunk).unwrap();
            }
            assert!(assembler.finish().unwrap().0.is_empty());
        }
        let mut decoder = Decoder::new("model".into());
        decoder.feed(added(0, call(Some("{")))).unwrap();
        decoder.feed(done(0, call(Some("{")))).unwrap();
        assert!(decoder.feed(completed(vec![call(Some("{}"))])).is_err());
    }

    #[test]
    fn completed_parts_do_not_retain_a_mutable_stream_or_announcement() {
        let mut decoder = Decoder::new("model".into());
        let done = json!({"type":"response.output_text.done", "item_id":"m", "text":"hello"});
        for event in [
            added(0, json!({"type":"message", "id":"m", "role":"assistant"})),
            json!({"type":"response.content_part.added", "item_id":"m", "content_index":0,
                "part":{"type":"output_text", "text":""}}),
            json!({"type":"response.output_text.delta", "item_id":"m", "delta":"hello"}),
            done.clone(),
        ] {
            decoder.feed(event).unwrap();
        }
        assert!(decoder.feed(done).unwrap().is_empty());
        let late = json!({"type":"response.output_text.delta", "item_id":"m", "delta":"late"});
        assert!(decoder.feed(late).is_err());
    }

    #[test]
    fn error_adapter_preserves_classification_and_sanitized_diagnostics() {
        for (field, identifier, kind) in [
            (
                "type",
                "invalid_request_error",
                ProviderErrorKind::InvalidRequest,
            ),
            ("code", "server_error", ProviderErrorKind::Response),
            ("code", "unknown_error_SECRET", ProviderErrorKind::Response),
        ] {
            let error =
                api_error(&json!({field: identifier, "message": "SECRET prompt credential"}));
            assert_eq!(error.kind, kind);
            let prefix = format!("Responses request failed ({kind:?})");
            assert!(error.message.starts_with(&prefix));
            assert_eq!(
                error.message.contains("[code="),
                !identifier.contains("SECRET")
            );
            assert!(!error.message.contains("SECRET"));
        }
    }

    #[test]
    fn out_of_order_items_keep_provider_indices_and_authoritative_blocks() {
        let mut decoder = Decoder::new("gpt-5".into());
        let mut assembler = ResponseAssembler::default();
        let reasoning = reasoning_item();
        let text = message("msg_1", "authoritative");
        feed_into(
            &mut decoder,
            &mut assembler,
            [
                added(1, message("msg_1", "")),
                added(0, reasoning.clone()),
                json!({"type":"response.output_text.delta", "output_index":1, "item_id":"msg_1", "content_index":0, "delta":"authoritative"}),
                done(1, text.clone()),
                completed(vec![reasoning, text]),
            ],
        );
        let (items, usage, stop) = assembler.finish().unwrap();
        assert_eq!(items[0].id, "rs_1");
        assert_eq!(items[1].text_content().as_deref(), Some("authoritative"));
        let tokens = (
            usage.input_tokens,
            usage.cached_input_tokens,
            usage.output_tokens,
        );
        assert_eq!((tokens, stop), ((8, 12, 7), StopReason::EndTurn));
        assert!(decoder.finish().unwrap().is_empty());
    }

    #[test]
    fn sse_validation_eof_and_errors_are_not_silent_success() {
        let mut decoder = Decoder::new("gpt-5".into());
        assert!(decoder.finish().is_err());
        for event in [
            sse(None, "[DONE]"),
            sse(None, "{"),
            sse(
                Some("response.created"),
                "{\"type\":\"response.completed\"}",
            ),
        ] {
            assert!(decoder.decode(&event).is_err());
        }
        for (code, kind) in [
            (
                "context_length_exceeded",
                ProviderErrorKind::ContextWindowExceeded,
            ),
            ("rate_limit_exceeded", ProviderErrorKind::RateLimited),
            ("invalid_api_key", ProviderErrorKind::Authentication),
            ("server_error", ProviderErrorKind::Response),
        ] {
            let error = decoder
                .feed(json!({"type":"error", "code":code,"message":"details"}))
                .unwrap_err();
            assert!(!error.message.contains("details"));
            assert_eq!(error.kind, kind);
        }
        let terminal = sse(Some("response.completed"), completed(vec![]).to_string());
        decoder.decode(&terminal).unwrap();
        assert!(decoder.decode(&sse(None, "[DONE]")).unwrap().is_empty());
        assert!(decoder.feed(completed(vec![])).is_err());
    }

    #[test]
    fn identical_item_done_and_empty_event_name_decode_one_item() {
        let output = message("msg", "hello");
        let mut decoder = Decoder::new("gpt-5".into());
        let mut assembler = ResponseAssembler::default();
        let events = [
            done(0, output.clone()),
            done(0, output.clone()),
            completed(vec![output]),
        ];
        feed_into(&mut decoder, &mut assembler, events);
        let (items, _, _) = assembler.finish().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text_content().as_deref(), Some("hello"));
        // An empty SSE event name means default message framing.
        let mut decoder = Decoder::new("test-model".into());
        let mut assembler = ResponseAssembler::default();
        let event = sse(
            Some(""),
            completed(vec![message("msg", "hello")]).to_string(),
        );
        for chunk in decoder.decode(&event).unwrap() {
            assembler.push(&chunk).unwrap();
        }
        let (items, _, _) = assembler.finish().unwrap();
        assert_eq!(items[0].text_content().as_deref(), Some("hello"));
    }
}
