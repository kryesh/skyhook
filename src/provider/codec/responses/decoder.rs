//! Stateful item/block lifecycle and HTTP/SSE framing validation.
use super::native::NativeItem;
use super::*;

impl Decoder {
    pub(crate) fn new(
        model: String,
        scope: Scope,
        dialect: &Dialect,
        errors: ErrorSignals,
    ) -> Self {
        Self {
            metadata_events: dialect.metadata_events,
            errors,
            model,
            scope,
            items: BTreeMap::new(),
            ids: HashMap::new(),
            wires: HashMap::new(),
            unfinished: BTreeSet::new(),
            completed: false,
        }
    }

    pub(crate) fn decode(
        &mut self,
        event: &crate::provider::http::transport::SseEvent,
    ) -> Result<Vec<ResponseEvent>, ProviderError> {
        if event.data.trim() == "[DONE]" {
            return if self.completed {
                Ok(vec![])
            } else {
                Err(NATIVE.error("[DONE] before terminal response"))
            };
        }
        let value: Value =
            serde_json::from_str(&event.data).map_err(|_| NATIVE.error("invalid SSE JSON"))?;
        if value
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| self.metadata_events.contains(&kind))
        {
            return Ok(vec![]);
        }
        if let Some(name) = &event.event
            && !name.is_empty()
            && name != "message"
            && Some(name.as_str()) != value.get("type").and_then(Value::as_str)
        {
            return Err(NATIVE.error("SSE event name disagrees with payload type"));
        }
        self.feed(value)
    }

    pub(super) fn start_item(
        &mut self,
        id: usize,
        wire: Option<usize>,
        native_id: &str,
        item_kind: ItemKind,
        raw: Option<&Value>,
    ) -> Result<(), ProviderError> {
        let identity = |key| {
            raw.and_then(|raw| raw.get(key))
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        };
        if self.items.contains_key(&id) {
            return Err(NATIVE.error("duplicate output item index"));
        }
        if self.ids.contains_key(native_id) {
            return Err(NATIVE.error("duplicate output item ID"));
        }
        let native_id = ItemId::try_from(native_id.to_owned())
            .map_err(|_| NATIVE.error("empty output item ID"))?;
        self.ids.insert(native_id.as_str().into(), id);
        self.wires.extend(wire.map(|wire| (wire, id)));
        self.items.insert(
            id,
            Item {
                native_id,
                wire_index: wire,
                body: match item_kind {
                    ItemKind::Text => ItemBody::Text(TextState::default()),
                    ItemKind::Reasoning => ItemBody::Reasoning(ReasoningState::default()),
                    ItemKind::ToolCall => ItemBody::Function(FunctionState {
                        phase: FunctionPhase::Streaming(StreamingFunction {
                            call_id: identity("call_id"),
                            name: identity("name"),
                            ..StreamingFunction::default()
                        }),
                    }),
                },
            },
        );
        Ok(())
    }

    pub(super) fn part(&mut self, id: usize, position: usize) -> Result<&mut Part, ProviderError> {
        let item = self.items.get_mut(&id).expect("checked item");
        Ok(match &mut item.body {
            ItemBody::Text(state) => state.parts.entry(position).or_default(),
            ItemBody::Reasoning(state) => state.parts.entry(position).or_default(),
            ItemBody::Function(_) => return Err(NATIVE.error("content part on function item")),
        })
    }

    /// Provisional text goes out immediately; the terminal item is authoritative.
    pub(super) fn delta(
        &mut self,
        id: usize,
        position: usize,
        text: &str,
        events: &mut Vec<ResponseEvent>,
    ) -> Result<(), ProviderError> {
        let Part::Streaming { text: streamed, .. } = self.part(id, position)? else {
            return Err(NATIVE.error("delta after content part ended"));
        };
        streamed.push_str(text);
        events.push(self.items[&id].delta(position, text));
        Ok(())
    }

    pub(super) fn close_part(
        &mut self,
        id: usize,
        position: usize,
        content: String,
    ) -> Result<(), ProviderError> {
        let part = self.part(id, position)?;
        if let Part::Completed(old) = part {
            if *old != content {
                return Err(NATIVE.error("conflicting final content part"));
            }
            return Ok(());
        }
        // The final content is authoritative when it disagrees with the deltas.
        *part = Part::Completed(content);
        Ok(())
    }

    pub(super) fn end(
        &mut self,
        id: usize,
        header: NativeItem<'_>,
        terminal: bool,
    ) -> Result<(), ProviderError> {
        let native = header.raw;
        let item = self
            .items
            .get(&id)
            .ok_or_else(|| NATIVE.error("end of unstarted output item"))?;
        if self.item_by_id(header.id) != Some(id) || header.kind != item.kind() {
            return Err(NATIVE.error("final output item identity changed"));
        }
        if header.unfinished {
            self.unfinished.insert(id);
        }
        match &item.body {
            ItemBody::Reasoning(_) => return self.end_reasoning(id, native),
            ItemBody::Function(_) => return self.end_function(id, native, terminal),
            ItemBody::Text(_) => {}
        }
        let parts = final_parts(native, header.kind)?;
        if let Some(old) = item.snapshot() {
            if final_parts(old, item.kind())? != parts {
                return Err(NATIVE.error("conflicting final output item"));
            }
            return Ok(());
        }
        if item.parts().any(|(position, _)| *position >= parts.len()) {
            return Err(NATIVE.error("final item omitted a streamed content part"));
        }
        for (position, content) in parts.into_iter().enumerate() {
            self.close_part(id, position, content)?;
        }
        let item = self.items.get_mut(&id).expect("checked item");
        if let ItemBody::Text(state) = &mut item.body {
            state.snapshot = Some(native.clone());
        }
        Ok(())
    }

    fn end_function(
        &mut self,
        id: usize,
        native: &Value,
        terminal: bool,
    ) -> Result<(), ProviderError> {
        let state = self.items[&id].function()?;
        // A partial/non-object call can precede the terminal stop reason. It
        // must stay provisional, never become a completed executable ToolCall.
        let call = match native::function_call(native) {
            Ok(call) => call,
            Err(error) => {
                if !terminal && native::item_arguments(native).is_err() {
                    if state.snapshot().is_some() {
                        return Err(NATIVE.error("duplicate final output item"));
                    }
                    return self.mark_provisional(id, native);
                }
                return Err(error);
            }
        };
        // Placeholder final arguments must not erase streamed ones.
        let omitted = call.arguments().is_empty();
        match &state.phase {
            FunctionPhase::Completed { call: old, .. } => {
                // A repeated snapshot omitting arguments agrees on identity alone.
                let same =
                    old == &call || (omitted && old.id() == call.id() && old.name() == call.name());
                if !same {
                    return Err(NATIVE.error("conflicting final output item"));
                }
                return Ok(());
            }
            FunctionPhase::Provisional(old) => {
                // Preserve the rejection of a malformed prior item.done even
                // if a later normal terminal supplies a valid object.
                native::function_call(old)?;
                return Err(NATIVE.error("conflicting final output item"));
            }
            FunctionPhase::Streaming(_) => {}
        }
        let call = if omitted {
            match self.streamed_arguments(id, call) {
                Ok(call) => call,
                Err(_) if !terminal => return self.mark_provisional(id, native),
                Err(error) => return Err(error),
            }
        } else {
            call
        };
        if self.items.iter().any(|(other_id, other)| {
            *other_id != id
                && other
                    .function()
                    .ok()
                    .and_then(FunctionState::snapshot)
                    .is_some_and(|other| other.get("call_id") == native.get("call_id"))
        }) {
            return Err(NATIVE.error("duplicate function call ID"));
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
            return Err(NATIVE.error("final function identity changed"));
        }
        self.validate_arguments(id, call.arguments())?;
        let item = self.items.get_mut(&id).expect("checked item");
        item.function_mut()?.phase = FunctionPhase::Completed {
            native: native.clone(),
            call,
        };
        Ok(())
    }

    /// Hold an unusable call until the stop reason decides: an abnormal stop
    /// discards it, a normal one fails.
    fn mark_provisional(&mut self, id: usize, native: &Value) -> Result<(), ProviderError> {
        self.items
            .get_mut(&id)
            .expect("checked item")
            .function_mut()?
            .phase = FunctionPhase::Provisional(native.clone());
        Ok(())
    }

    /// Arguments from `arguments.done` or deltas, for a final item without them.
    fn streamed_arguments(&self, id: usize, call: ToolCall) -> Result<ToolCall, ProviderError> {
        let state = self.items[&id].function()?;
        let streaming = state.streaming()?;
        let recovered = match &streaming.final_arguments {
            Some(FinalArguments::Object(object)) => Some(object.clone()),
            Some(FinalArguments::Incomplete(text)) => Some(arguments(text)?),
            None if !streaming.streamed.trim().is_empty() => Some(arguments(&streaming.streamed)?),
            None => None,
        };
        match recovered {
            Some(object) if !object.is_empty() => {
                ToolCall::new(call.id(), call.name(), Value::Object(object))
                    .map_err(|error| NATIVE.error(error.to_string()))
            }
            _ => Ok(call),
        }
    }

    pub(super) fn validate_arguments(
        &self,
        id: usize,
        value: &serde_json::Map<String, Value>,
    ) -> Result<(), ProviderError> {
        let streaming = self.items[&id].function()?.streaming()?;
        if let Some(old) = &streaming.final_arguments {
            let matches = match old {
                FinalArguments::Object(old) => old == value,
                FinalArguments::Incomplete(old) => &arguments(old)? == value,
            };
            if !matches {
                return Err(NATIVE.error("conflicting final function arguments"));
            }
        }
        // Final arguments may repair unparseable deltas, but two complete,
        // different statements of the input are a conflict, not a choice.
        if let Ok(streamed) = arguments(&streaming.streamed)
            && !streamed.is_empty()
            && &streamed != value
        {
            return Err(NATIVE.error("final function arguments disagree with deltas"));
        }
        Ok(())
    }

    pub(crate) fn finish(&mut self) -> Result<Vec<ResponseEvent>, ProviderError> {
        if !self.completed {
            return Err(NATIVE.error("stream ended before a terminal response"));
        }
        Ok(vec![])
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::*;
    use crate::provider::ProviderErrorKind;
    use crate::provider::http::transport::tests::sse;

    fn feed_into(
        decoder: &mut Decoder,
        events: &mut Vec<ResponseEvent>,
        frames: impl IntoIterator<Item = Value>,
    ) {
        for frame in frames {
            events.extend(decoder.feed(frame).unwrap());
        }
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
        for (reason, cut) in [
            ("max_output_tokens", CutReason::MaxTokens),
            ("content_filter", CutReason::Refusal),
        ] {
            let mut decoder = decoder();
            let mut events = Vec::new();
            feed_into(
                &mut decoder,
                &mut events,
                [
                    added(0, call(None)),
                    json!({"type":"response.function_call_arguments.delta", "item_id":"f", "delta":"{"}),
                    json!({"type":"response.function_call_arguments.done", "item_id":"f", "arguments":"{"}),
                    done(0, call(Some("{"))),
                ],
            );
            let state = decoder.items[&0].function().unwrap();
            assert!(matches!(state.phase, FunctionPhase::Provisional(_)));

            events.extend(
                decoder
                    .feed(json!({"type":"response.incomplete", "response":{
                    "status":"incomplete", "output":[], "incomplete_details":{"reason":reason}}}))
                    .unwrap(),
            );
            let reduced = reduce(events);
            // The provisional call streamed but never became executable.
            assert_eq!(reduced.streamed(ItemKind::ToolCall), ["{"]);
            assert_eq!(reduced.completion.outcome(), Outcome::Cut(cut));
            assert!(reduced.items().is_empty());
        }
        // A normal terminal fails them, restated, revised or omitted.
        for output in [vec![call(Some("{"))], vec![call(Some("{}"))], vec![]] {
            let mut decoder = decoder();
            decoder.feed(added(0, call(Some("{")))).unwrap();
            assert!(decoder.feed(done(0, call(Some("{")))).unwrap().is_empty());
            assert!(decoder.feed(completed(output)).is_err());
        }
    }

    #[test]
    fn completed_parts_do_not_retain_a_mutable_stream_or_announcement() {
        let mut decoder = decoder();
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
    fn out_of_order_items_keep_provider_indices_and_authoritative_blocks() {
        let mut decoder = decoder();
        let mut events = Vec::new();
        let reasoning = reasoning_item();
        let text = message("msg_1", "authoritative");
        feed_into(
            &mut decoder,
            &mut events,
            [
                added(1, message("msg_1", "")),
                added(0, reasoning.clone()),
                json!({"type":"response.output_text.delta", "output_index":1, "item_id":"msg_1", "content_index":0, "delta":"authoritative"}),
                done(1, text.clone()),
                completed(vec![reasoning, text]),
            ],
        );
        let reduced = reduce(events);
        let items = reduced.items();
        assert_eq!(items[0].id().as_str(), "rs_1");
        assert_eq!(items[1].text_content().as_deref(), Some("authoritative"));
        let usage = reduced.usage;
        let tokens = (
            usage.input_tokens.unwrap(),
            usage.cached_input_tokens.unwrap(),
            usage.output_tokens.unwrap(),
        );
        assert_eq!(
            (tokens, reduced.completion.outcome()),
            ((8, 12, 7), Outcome::Answer)
        );
        assert!(decoder.finish().unwrap().is_empty());
    }

    #[test]
    fn sse_validation_eof_and_errors_are_not_silent_success() {
        let mut decoder = decoder();
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
        let details = json!({"code":"context_length_exceeded", "message":"details"});
        for event in [
            json!({"type":"error", "code":"context_length_exceeded", "message":"details"}),
            json!({"type":"response.error", "error":details}),
        ] {
            let error = decoder.feed(event).unwrap_err();
            assert!(error.message.ends_with(": details"));
            assert_eq!(error.kind(), ProviderErrorKind::ContextWindowExceeded);
        }
        let terminal = sse(Some("response.completed"), completed(vec![]).to_string());
        decoder.decode(&terminal).unwrap();
        assert!(decoder.decode(&sse(None, "[DONE]")).unwrap().is_empty());
        assert!(decoder.feed(completed(vec![])).is_err());
    }

    #[test]
    fn identical_item_done_and_empty_event_name_decode_one_item() {
        let output = message("msg", "hello");
        let reduced = assemble(vec![
            done(0, output.clone()),
            done(0, output.clone()),
            completed(vec![output]),
        ])
        .unwrap();
        assert_eq!(reduced.items().len(), 1);
        assert_eq!(reduced.items()[0].text_content().as_deref(), Some("hello"));
        // An empty SSE event name means default message framing.
        let mut decoder = decoder();
        let event = sse(
            Some(""),
            completed(vec![message("msg", "hello")]).to_string(),
        );
        let reduced = reduce(decoder.decode(&event).unwrap());
        assert_eq!(reduced.items()[0].text_content().as_deref(), Some("hello"));
    }
}
