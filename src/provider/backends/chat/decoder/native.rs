//! Parse native SSE envelopes into the one Chat choice this codec follows.
use super::super::wire;
use crate::provider::{
    ProviderError,
    backends::{common::lenient_u64, errors, transport::SseEvent},
};
use serde_json::Value;

/// `None` marks `[DONE]`. Chunks that carry no recognizable choice or usage
/// decode as empty, so vendor keepalives and metadata packets are harmless.
pub(super) fn decode(event: &SseEvent) -> Result<Option<wire::Chunk>, ProviderError> {
    let is_error_event = event.event.as_deref() == Some("error");
    if event.data.trim() == "[DONE]" {
        if is_error_event {
            return Err(ProviderError::protocol("Chat error event contained [DONE]"));
        }
        return Ok(None);
    }
    let value: Value = serde_json::from_str(&event.data)
        .map_err(|_| ProviderError::protocol("Invalid Chat SSE JSON"))?;
    if value.get("error").is_some_and(|value| !value.is_null()) || is_error_event {
        return Err(errors::classify_error(None, &value));
    }
    // Follow choice zero; servers may omit its index or send extra choices.
    let choice = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| {
            choices
                .iter()
                .filter(|choice| choice.is_object())
                .find(|choice| match choice.get("index") {
                    None | Some(Value::Null) => true,
                    Some(index) => lenient_u64(index) == Some(0),
                })
        })
        .map(|choice| {
            serde_json::from_value::<wire::Choice>(choice.clone())
                .map_err(|_| ProviderError::protocol("Invalid Chat choice shape"))
        })
        .transpose()?;
    let usage = value
        .get("usage")
        .filter(|usage| usage.is_object())
        .and_then(wire::Usage::from_value);
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .map(str::to_owned);
    Ok(Some(wire::Chunk { id, choice, usage }))
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use crate::provider::protocol::{Outcome, ResponseEvent};
    use serde_json::{Value, json};

    /// Decodes `packet` before content, after content, and after a normal
    /// finish without producing output or failing the stream.
    fn assert_ignored_at_every_stage(packet: Value) {
        for stage in 0..3 {
            let mut decoder = decoder();
            let prefix = [delta(json!({"content":"answer"})), end("stop")];
            for frame in prefix.into_iter().take(stage) {
                decoder.decode(&frame).unwrap();
            }
            let events = decoder.decode(&event(packet.clone())).unwrap();
            assert!(
                events
                    .iter()
                    .all(|event| matches!(event, ResponseEvent::Usage(_))),
                "{stage}: {packet}"
            );
        }
    }

    #[test]
    fn choice_zero_is_selected_leniently() {
        for choices in [
            json!([{"index":null,"delta":{"content":"answer"}}]),
            json!([{"index":"0","delta":{"content":"answer"}}]),
            json!([{"index":0.0,"delta":{"content":"answer"}}]),
            json!([{"index":1,"delta":{"content":"other"}},{"index":0,"delta":{"content":"answer"}}]),
            json!([{"delta":{"content":"answer"}},{"delta":{"content":"other"}}]),
            json!([null, {"index":0,"delta":{"content":"answer"}}]),
        ] {
            let (items, _, _) = decode(vec![event(json!({"choices":choices})), end("stop")]);
            assert_eq!(contents(&items), [text("answer")], "{choices}");
        }
        for index in [json!(1), json!(-1), json!(0.5), json!(true)] {
            assert_ignored_at_every_stage(
                json!({"choices":[{"index":index,"delta":{"content":"x"}}]}),
            );
        }
    }

    #[test]
    fn malformed_placeholders_are_ignored_but_errors_are_not() {
        let mut packets = vec![
            Value::Null,
            json!(false),
            json!(0),
            json!(""),
            json!([]),
            json!({"choices":[[0, {}, null]]}),
            json!({"usage":[1, 1, 2]}),
            json!({"object":"chat.completion"}),
        ];
        for value in [json!(false), json!(0), json!(""), json!({})] {
            packets.push(json!({ "choices": value }));
            packets.push(json!({"choices":[{"delta":value}]}));
            packets.push(json!({"choices":[{"finish_reason":value}]}));
            packets.push(json!({ "usage": value }));
        }
        for field in [
            "role",
            "content",
            "refusal",
            "reasoning",
            "reasoning_content",
            "tool_calls",
        ] {
            for value in [json!(false), json!(0), json!({})] {
                let mut packet = json!({"choices":[{"delta":{}}]});
                packet["choices"][0]["delta"][field] = value;
                packets.push(packet);
            }
        }
        packets.into_iter().for_each(assert_ignored_at_every_stage);
        for choices in [None, Some(Value::Null), Some(json!([]))] {
            let mut packet = json!({"error":{"type":"server_error","message":"upstream"}});
            if let Some(choices) = &choices {
                packet["choices"] = choices.clone();
            }
            let mut decoder = decoder();
            assert!(decoder.decode(&event(packet)).is_err());
            assert!(decoder.finish().is_err());
        }
        let mut decoder = decoder();
        let invalid = crate::provider::backends::transport::SseEvent {
            event: None,
            data: "{not json".into(),
        };
        assert!(decoder.decode(&invalid).is_err());
    }

    #[test]
    fn non_stream_choice_output_is_decoded() {
        for choice in [
            json!({"message":{"role":"assistant","content":"answer"}}),
            json!({"message":{"content":"answer"}, "delta":null}),
            json!({"text":"answer", "delta":{}}),
        ] {
            let mut choice = choice;
            choice["finish_reason"] = json!("stop");
            let (items, _, outcome) = decode(vec![event(json!({ "choices": [choice] }))]);
            assert_eq!(outcome, Outcome::Answer);
            assert_eq!(contents(&items), [text("answer")]);
        }
        let (items, _, _) = decode(vec![event(
            json!({"choices":[{"finish_reason":"tool_calls",
            "message":{"tool_calls":[{"id":"c","type":"function","function":{"name":"one","arguments":"{}"}}]}}]}),
        )]);
        assert_eq!(items.len(), 1);
        // Two calls without IDs in one message stay two calls.
        let (items, _, _) = decode(vec![event(
            json!({"choices":[{"finish_reason":"tool_calls",
            "message":{"tool_calls":[
                {"type":"function","function":{"name":"a","arguments":"{}"}},
                {"type":"function","function":{"name":"b","arguments":"{}"}}]}}]}),
        )]);
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn empty_sse_event_name_uses_default_message_framing() {
        let frames = [
            event(json!({})),
            delta(json!({"content":"answer"})),
            end("stop"),
            done(),
        ];
        let frames = frames
            .into_iter()
            .map(|mut frame| {
                frame.event = Some(String::new());
                frame
            })
            .collect();
        let (items, _, outcome) = decode(frames);
        assert_eq!(outcome, Outcome::Answer);
        assert_eq!(contents(&items), [text("answer")]);
    }

    #[test]
    fn native_in_band_errors_are_classified_with_server_text() {
        use crate::provider::ProviderErrorKind;
        let error =
            json!({"type":"exceed_context_size_error","code":400,"message":"prompt too long"});
        let mut named = event(error.clone());
        named.event = Some("error".into());
        for frame in [event(json!({ "error": error })), named] {
            let mut decoder = decoder();
            let error = decoder.decode(&frame).unwrap_err();
            assert_eq!(error.kind, ProviderErrorKind::ContextWindowExceeded);
            assert!(error.message.ends_with(": prompt too long"));
            assert!(decoder.finish().is_err());
        }
    }

    #[test]
    fn full_message_beside_streamed_deltas_is_not_duplicated() {
        let (items, _, outcome) = decode(vec![
            delta(json!({"content":"hello"})),
            event(
                json!({"choices":[{"delta":{},"message":{"content":"hello"},"finish_reason":"stop"}]}),
            ),
        ]);
        assert_eq!(
            (outcome, contents(&items)),
            (Outcome::Answer, vec![text("hello")])
        );
        let call = json!([{"index":0,"id":"c","function":{"name":"run","arguments":"{\"a\":1}"}}]);
        let (items, _, outcome) = decode(vec![
            delta(json!({"tool_calls":call})),
            event(
                json!({"choices":[{"delta":{},"message":{"tool_calls":call},"finish_reason":"tool_calls"}]}),
            ),
        ]);
        assert_eq!((outcome, items.len()), (Outcome::ToolUse, 1));
    }

    #[test]
    fn full_message_after_streamed_deltas_adds_only_what_is_missing() {
        let call =
            json!([{"id":"c","type":"function","function":{"name":"run","arguments":"{\"a\":1}"}}]);
        let full = |message: Value, finish: &str| {
            event(json!({"choices":[{"delta":{},"message":message,"finish_reason":finish}]}))
        };
        // Only reasoning streamed: the answer and tool call come from the message.
        let (items, _, outcome) = decode(vec![
            delta(json!({"reasoning_content":"thinking"})),
            full(json!({"content":"answer","tool_calls":call}), "tool_calls"),
        ]);
        assert_eq!(outcome, Outcome::ToolUse);
        assert_eq!(items.len(), 3);
        assert_eq!(contents(&items)[1], text("answer"));
        // A repeat that adds a tool call adds just the call.
        let (items, _, _) = decode(vec![
            delta(json!({"content":"answer"})),
            full(json!({"content":"answer","tool_calls":call}), "tool_calls"),
        ]);
        assert_eq!(items.len(), 2);
        assert_eq!(contents(&items)[0], text("answer"));
        // Text the stream did not finish is completed.
        let (items, _, _) = decode(vec![
            delta(json!({"content":"hel"})),
            full(json!({"content":"hello"}), "stop"),
        ]);
        assert_eq!(contents(&items), [text("hello")]);
        // A message that differs from the stream (trimmed, reformatted, or
        // with different calls) leaves the streamed content as it is.
        for (streamed, message, expected) in [
            (
                json!({"content":"\n\nHello"}),
                json!({"content":"Hello"}),
                text("\n\nHello"),
            ),
            (
                json!({"content":"answer"}),
                json!({"content":"other"}),
                text("answer"),
            ),
        ] {
            let (items, _, _) = decode(vec![delta(streamed), full(message, "stop")]);
            assert_eq!(contents(&items), [expected]);
        }
        let streamed_call = json!({"tool_calls":[{"index":0,"id":"c","function":{"name":"run","arguments":"{\"a\":1}"}}]});
        let message = json!({"tool_calls":[{"id":"c","type":"function","function":{"name":"run","arguments":"{\"a\":2}"}}]});
        let (items, _, _) = decode(vec![delta(streamed_call), full(message, "tool_calls")]);
        let call = items[0].call().expect("a tool call");
        assert_eq!(Value::Object(call.arguments().clone()), json!({"a":1}));
    }
}
