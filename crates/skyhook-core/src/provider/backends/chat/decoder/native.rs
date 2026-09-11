//! Parse native SSE envelopes and validate Chat wire shapes before state changes.
use super::super::wire;
use crate::provider::{
    ProviderError,
    backends::{errors, transport::SseEvent},
};
use serde_json::Value;

pub(super) fn decode(event: &SseEvent) -> Result<Option<wire::Chunk>, ProviderError> {
    if let Some(name) = event.event.as_deref()
        && !name.is_empty()
        && name != "message"
        && name != "error"
    {
        return Err(ProviderError::protocol("Unsupported Chat SSE event"));
    }
    if event.data.trim() == "[DONE]" {
        if event.event.as_deref() == Some("error") {
            return Err(ProviderError::protocol("Chat error event contained [DONE]"));
        }
        return Ok(None);
    }
    let value: Value = serde_json::from_str(&event.data)
        .map_err(|_| ProviderError::protocol("Invalid Chat SSE JSON"))?;
    if value.get("error").is_some_and(|value| !value.is_null())
        || event.event.as_deref() == Some("error")
    {
        return Err(errors::classify_error(None, &value));
    }
    // Serde structs can deserialize positional arrays. Wire chunks, choices,
    // and usage must be objects even when absent/null containers are allowed.
    if !value.is_object()
        || value
            .get("choices")
            .and_then(Value::as_array)
            .is_some_and(|choices| choices.iter().any(|choice| !choice.is_object()))
        || value
            .get("usage")
            .is_some_and(|usage| !usage.is_null() && !usage.is_object())
    {
        return Err(ProviderError::protocol("Invalid Chat chunk shape"));
    }
    let wire::Chunk {
        object,
        choices,
        usage,
    } = serde_json::from_value(value)
        .map_err(|_| ProviderError::protocol("Invalid Chat chunk shape"))?;
    if object
        .as_deref()
        .is_some_and(|kind| kind != "chat.completion.chunk")
    {
        return Err(ProviderError::protocol("Expected chat.completion.chunk"));
    }
    if choices.len() > 1 {
        return Err(ProviderError::protocol(
            "Chat codec supports exactly one choice",
        ));
    }
    for choice in &choices {
        if choice.message.is_some() || choice.text.is_some() {
            return Err(ProviderError::protocol(
                "Expected Chat streaming delta, not full output",
            ));
        }
        if !matches!(
            choice.index,
            wire::ChoiceIndex::Missing | wire::ChoiceIndex::Number(0)
        ) {
            return Err(ProviderError::protocol(
                "Chat choice index must be zero or absent",
            ));
        }
    }
    Ok(Some(wire::Chunk {
        object,
        choices,
        usage,
    }))
}

#[cfg(test)]
mod tests {
    use super::super::{Decoder, tests::*};
    use crate::provider::protocol::{BlockContent, StopReason};
    use serde_json::{Value, json};

    #[test]
    fn invalid_choices_are_rejected_before_and_after_finish() {
        let mut choices: Vec<_> = [
            Value::Null,
            json!("0"),
            json!(1),
            json!(-1),
            json!(0.5),
            json!(true),
        ]
        .into_iter()
        .map(|index| json!([{"index":index,"delta":{}}]))
        .collect();
        choices.extend([
            json!([{"delta":{}},{"delta":{}}]),
            json!([{"index":0,"delta":{}},{"index":0,"delta":{}}]),
            json!([{"index":0,"delta":{}},{"index":1,"delta":{}}]),
        ]);
        for choices in choices {
            for finished in [false, true] {
                let mut decoder = Decoder::new("test-model".into());
                let mut packet = json!({"choices":choices});
                if finished {
                    decoder.decode(&delta(json!({"content":"answer"}))).unwrap();
                    decoder.decode(&end("stop")).unwrap();
                    packet["usage"] = phantom_usage_chunk()["usage"].clone();
                }
                assert!(decoder.decode(&event(packet.clone())).is_err(), "{packet}");
                assert!(decoder.finish().is_err());
            }
        }
    }

    #[test]
    fn malformed_known_chunk_choice_and_delta_values_still_fail() {
        let mut packets = vec![
            Value::Null,
            json!(false),
            json!(0),
            json!(""),
            json!([]),
            json!([[], null]),
            json!({"choices":[[0, {}, null]]}),
            json!({"usage":[1, 1, 2]}),
        ];
        for value in [json!(false), json!(0), json!(""), json!({})] {
            packets.push(json!({"choices":value}));
        }
        for value in [Value::Null, json!(false), json!(0), json!(""), json!([])] {
            packets.push(json!({"choices":[value]}));
        }
        for value in [json!(false), json!(0), json!(""), json!([])] {
            packets.push(json!({"choices":[{"delta":value}]}));
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
        for value in [
            json!(false),
            json!(0),
            json!([]),
            json!({}),
            json!("unknown"),
        ] {
            packets.push(json!({"choices":[{"finish_reason":value}]}));
        }
        for packet in packets {
            for stage in 0..3 {
                let mut decoder = Decoder::new("test-model".into());
                if stage > 0 {
                    decoder.decode(&delta(json!({"content":"answer"}))).unwrap();
                }
                if stage > 1 {
                    decoder.decode(&end("stop")).unwrap();
                }
                assert!(
                    decoder.decode(&event(packet.clone())).is_err(),
                    "{stage}: {packet}"
                );
                assert!(decoder.finish().is_err());
            }
        }
    }

    #[test]
    fn empty_choices_do_not_bypass_usage_or_native_error_validation() {
        for choices in [None, Some(Value::Null), Some(json!([]))] {
            for payload in [
                json!({"usage":false}),
                json!({"usage":[]}),
                json!({"usage":{}}),
                json!({"usage":{"prompt_tokens":null,"completion_tokens":1}}),
                json!({"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":3}}),
                json!({"usage":{"prompt_tokens":1,"completion_tokens":1,"prompt_tokens_details":[]}}),
                json!({"error":{"type":"server_error","message":"secret"}}),
            ] {
                for stage in 0..3 {
                    let mut decoder = Decoder::new("test-model".into());
                    if stage > 0 {
                        decoder.decode(&delta(json!({"content":"answer"}))).unwrap();
                    }
                    if stage > 1 {
                        decoder.decode(&end("stop")).unwrap();
                    }
                    let mut packet = payload.clone();
                    if let Some(choices) = &choices {
                        packet["choices"] = choices.clone();
                    }
                    assert!(
                        decoder.decode(&event(packet.clone())).is_err(),
                        "{stage}: {packet}"
                    );
                    assert!(decoder.finish().is_err());
                }
            }
        }
    }

    #[test]
    fn non_stream_choice_output_is_not_silently_ignored() {
        for field in ["message", "text"] {
            for value in [
                json!(""),
                json!("answer"),
                json!({"role":"assistant","content":"answer"}),
                json!({}),
                json!(false),
            ] {
                for delta_value in [None, Some(Value::Null), Some(json!({}))] {
                    for stage in 0..3 {
                        let mut decoder = Decoder::new("test-model".into());
                        if stage > 0 {
                            decoder.decode(&delta(json!({"content":"answer"}))).unwrap();
                        }
                        if stage > 1 {
                            decoder.decode(&end("stop")).unwrap();
                        }
                        let mut choice = json!({});
                        choice[field] = value.clone();
                        if let Some(value) = &delta_value {
                            choice["delta"] = value.clone();
                        }
                        assert!(
                            decoder.decode(&event(json!({"choices":[choice]}))).is_err(),
                            "{stage}: {field}={value}"
                        );
                        assert!(decoder.finish().is_err());
                    }
                }
            }
        }
        let (items, _, stop) = decode(vec![
            event(json!({"choices":[{"message":null,"text":null}]})),
            delta(json!({"content":"answer"})),
            end("stop"),
            event(json!({"choices":[{"message":null,"text":null}]})),
        ]);
        assert_eq!(stop, StopReason::EndTurn);
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].blocks[0].content,
            BlockContent::Text {
                text: "answer".into()
            }
        );
    }

    #[test]
    fn empty_sse_event_name_uses_default_message_framing() {
        let frames = [
            event(json!({})),
            delta(json!({"content":"answer"})),
            end("stop"),
            done(),
        ]
        .into_iter()
        .map(|mut frame| {
            frame.event = Some(String::new());
            frame
        })
        .collect();
        let (items, _, stop) = decode(frames);
        assert_eq!(stop, StopReason::EndTurn);
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].blocks[0].content,
            BlockContent::Text {
                text: "answer".into()
            }
        );
    }

    #[test]
    fn native_in_band_errors_are_classified_without_exposing_server_text() {
        use crate::provider::ProviderErrorKind;
        for named_event in [false, true] {
            let error = json!({"type":"exceed_context_size_error","code":400,"message":"secret prompt and API key"});
            let mut frame = if named_event {
                event(error)
            } else {
                event(json!({"error":error}))
            };
            if named_event {
                frame.event = Some("error".into());
            }
            let mut decoder = Decoder::new("test-model".into());
            let error = decoder.decode(&frame).unwrap_err();
            assert_eq!(error.kind, ProviderErrorKind::ContextWindowExceeded);
            assert!(!error.message.contains("secret"));
            assert!(decoder.finish().is_err());
        }
    }
}
