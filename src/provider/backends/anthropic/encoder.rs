//! Anthropic Messages request encoding.
use super::super::common::{anthropic_image, invalid, opaque_payload, tool_text, user_parts};
use super::native::validate_thinking;
use crate::provider::{
    ProviderError,
    protocol::{AssistantItem, HistoryLifetime, Message, ModelRequest},
};
use serde_json::{Value, json};

pub(crate) fn encode(request: &ModelRequest) -> Result<Value, ProviderError> {
    let max_tokens = request
        .max_output_tokens
        .filter(|n| *n > 0)
        .ok_or_else(|| {
            invalid(
                "Anthropic requires max_output_tokens to be explicitly set to a positive integer",
            )
        })?;
    let mut body = json!({
        "model": request.model,
        "max_tokens": max_tokens,
        "stream": true,
    });
    let mut system = Vec::new();
    let mut cache_breakpoints = 0;
    for segment in &request.system {
        let mut block = json!({"type": "text", "text": segment.text});
        if segment.cache {
            cache_breakpoints += 1;
            block["cache_control"] = json!({"type": "ephemeral"});
        }
        system.push(block);
    }
    if !system.is_empty() {
        body["system"] = Value::Array(system);
    }
    let mut messages: Vec<Value> = Vec::new();
    for message in &request.history {
        push_message(request, &mut messages, message)?;
    }
    // History is an unchanged prefix of later requests (reads land only at breakpoints, so
    // even ending history is marked); the tail and detached history never are.
    if request.history_lifetime != HistoryLifetime::Detached {
        cache_breakpoints += usize::from(mark_last_cacheable(&mut messages));
    }
    if cache_breakpoints > 4 {
        return Err(invalid(
            "Anthropic supports at most four cache breakpoints, including one for history",
        ));
    }
    for message in &request.tail {
        push_message(request, &mut messages, message)?;
    }
    if messages.is_empty() {
        return Err(invalid("Anthropic requires at least one message"));
    }
    body["messages"] = Value::Array(messages);
    if !request.tools.is_empty() {
        let mut tools = Vec::new();
        for tool in &request.tools {
            if tool.name.is_empty() || !tool.input_schema.is_object() {
                return Err(invalid(
                    "Anthropic tools require a nonempty name and object input_schema",
                ));
            }
            let mut native = json!({"name":tool.name, "input_schema":tool.input_schema});
            if !tool.description.is_empty() {
                native["description"] = json!(tool.description);
            }
            tools.push(native);
        }
        body["tools"] = Value::Array(tools);
    }
    let mut output_config = serde_json::Map::new();
    if let Some(schema) = &request.response_schema {
        if !schema.schema.is_object() {
            return Err(invalid(
                "Anthropic response_schema must be a JSON Schema object",
            ));
        }
        // Messages uses output_config.format, not the retired output_format beta field.
        output_config.insert(
            "format".into(),
            json!({"type":"json_schema", "schema":schema.schema}),
        );
    }
    // Summarized thinking is the default only on older models; request it wherever thinking is on.
    if let Some(reasoning) = &request.reasoning {
        match reasoning.as_str() {
            "off" => body["thinking"] = json!({"type":"disabled"}),
            "adaptive" => body["thinking"] = json!({"type":"adaptive", "display":"summarized"}),
            "low" | "medium" | "high" | "xhigh" | "max" => {
                body["thinking"] = json!({"type":"adaptive", "display":"summarized"});
                output_config.insert("effort".into(), json!(reasoning));
            }
            _ => {
                return Err(invalid(
                    "Unsupported Anthropic reasoning setting: use off, adaptive, low, medium, high, xhigh, or max",
                ));
            }
        }
    }
    if !output_config.is_empty() {
        body["output_config"] = Value::Object(output_config);
    }
    Ok(body)
}

fn push_message(
    request: &ModelRequest,
    messages: &mut Vec<Value>,
    message: &Message,
) -> Result<(), ProviderError> {
    let (role, content) = match message {
        Message::User(items) => {
            let image = |image: &_| anthropic_image(request, image);
            ("user", user_parts(request, items, "text", image)?)
        }
        Message::Assistant(items) => {
            let mut blocks = Vec::new();
            for item in items {
                match item {
                    // Replay belongs to the native item, not to each display block.
                    // Unsigned/foreign private reasoning is display-only.
                    AssistantItem::Reasoning { replay, .. } => {
                        if let Some(payload) = opaque_payload(replay, "anthropic", &request.model) {
                            validate_thinking(payload).map_err(|error| invalid(error.message))?;
                            blocks.push(payload.clone());
                        }
                    }
                    // A blank block carries nothing and is not universally
                    // accepted (a trailing one is a prefill error), so it is
                    // dropped rather than replayed.
                    AssistantItem::Text { blocks: text, .. } => blocks.extend(
                        text.iter()
                            .filter(|block| !block.text.trim().is_empty())
                            .map(|block| json!({"type":"text", "text":block.text})),
                    ),
                    AssistantItem::ToolCall { call, .. } => {
                        blocks.push(json!({"type":"tool_use", "id":call.id(), "name":call.name(), "input":call.arguments()}));
                    }
                }
            }
            ("assistant", blocks)
        }
        Message::Tool(results) => {
            let mut blocks = Vec::new();
            for result in results {
                if result.call_id.is_empty() {
                    return Err(invalid("Anthropic tool results require a nonempty call_id"));
                }
                let mut content = vec![json!({"type":"text", "text":tool_text(result)})];
                for image in &result.images {
                    content.push(anthropic_image(request, image)?);
                }
                blocks.push(json!({"type":"tool_result", "tool_use_id":result.call_id,
                    "content":content, "is_error":result.is_error}));
            }
            ("user", blocks)
        }
    };
    if content.is_empty() {
        // An assistant turn that encodes to nothing is skipped: history is
        // append-only, so rejecting it would fail every later request.
        if matches!(message, Message::Assistant(_)) {
            return Ok(());
        }
        return Err(invalid(
            "Anthropic messages must contain at least one content block",
        ));
    }
    if let Some(previous) = messages
        .last_mut()
        .filter(|previous| previous["role"] == role)
    {
        previous["content"]
            .as_array_mut()
            .expect("constructed array")
            .extend(content);
    } else {
        messages.push(json!({"role":role, "content":content}));
    }
    Ok(())
}

/// Mark the last block that accepts `cache_control`; thinking and empty text blocks cannot.
fn mark_last_cacheable(messages: &mut [Value]) -> bool {
    let block = messages
        .iter_mut()
        .rev()
        .flat_map(|message| {
            let content = message["content"].as_array_mut();
            content.expect("constructed array").iter_mut().rev()
        })
        .find(|block| {
            !matches!(
                block["type"].as_str(),
                Some("thinking" | "redacted_thinking")
            ) && block["text"] != ""
        });
    let Some(block) = block else {
        return false;
    };
    block["cache_control"] = json!({"type": "ephemeral"});
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::backends::common::{replay as scoped_replay, tests::scope};
    use crate::{
        media::{AttachmentRef, BlobRef, ImageFormat, ImageRef, TextRef},
        provider::protocol::{
            AssistantItem, Binding, BlockId, HistoryLifetime, ItemId, Replay, ResponseSchema,
            SystemSegment, TextBlock, ToolCall, ToolDefinition, ToolResult, UserContent,
        },
    };

    fn reasoning_envelope(protocol: &str, model: &str, payload: serde_json::Value) -> Replay {
        scoped_replay(protocol, model, &scope(), payload, Binding::Conversation)
    }

    fn request() -> ModelRequest {
        ModelRequest {
            ..crate::provider::backends::common::tests::request("claude-test")
        }
    }

    fn text(text: &str) -> UserContent {
        UserContent::Text { text: text.into() }
    }

    fn continue_after(item: AssistantItem) -> Vec<Message> {
        continue_after_items(vec![item])
    }

    fn continue_after_items(items: Vec<AssistantItem>) -> Vec<Message> {
        vec![
            Message::Assistant(items),
            Message::User(vec![text("continue")]),
        ]
    }

    #[test]
    fn request_maps_native_schema_tools_system_cache_and_adaptive_effort() {
        let image = ImageRef {
            file: Some("image.png".into()),
            format: ImageFormat::Png,
            blob: BlobRef::of(b"x"),
        };
        let notes = TextRef {
            file: Some("notes.txt".into()),
            blob: BlobRef::of(b"notes"),
        };
        let mut request = request();
        request.system = vec![SystemSegment {
            text: "cached".into(),
            cache: true,
        }];
        request.tools = vec![ToolDefinition {
            name: "look".into(),
            description: "Look".into(),
            input_schema: json!({"type":"object"}),
        }];
        let schema = json!({"type":"object", "properties":{}, "additionalProperties":false});
        request.response_schema = Some(ResponseSchema {
            name: "answer".into(),
            schema: schema.clone(),
        });
        request.reasoning = Some("high".into());
        let attach = |attachment| UserContent::Attachment { attachment };
        let call = ToolCall::new("tool_1", "look", json!({"path":"test.png"})).unwrap();
        request.history = vec![
            Message::User(vec![
                text("look"),
                attach(AttachmentRef::Image(image.clone())),
                attach(AttachmentRef::Text(notes.clone())),
            ]),
            Message::Assistant(vec![AssistantItem::tool_call("0", 0, call)]),
            Message::Tool(vec![ToolResult {
                call_id: "tool_1".into(),
                name: "look".into(),
                result: json!({"ok":false, "error":null}),
                images: vec![image.clone()],
                is_error: true,
            }]),
        ];
        request.tail = vec![Message::User(vec![UserContent::Runtime {
            text: "state".into(),
        }])];
        assert!(encode(&request).is_err());
        // Load the fixture blobs as the session store would.
        request.blobs.insert(image.blob, b"x".to_vec());
        request.blobs.insert(notes.blob, b"notes".to_vec());
        let body = encode(&request).unwrap();
        assert_eq!(body["max_tokens"], 8192);
        assert_eq!(
            body["system"][0]["cache_control"],
            json!({"type":"ephemeral"})
        );
        assert_eq!(
            body["tools"][0]["input_schema"],
            request.tools[0].input_schema
        );
        assert_eq!(body["output_config"]["format"]["schema"], schema);
        assert_eq!(body["output_config"]["effort"], "high");
        assert_eq!(
            body["thinking"],
            json!({"type":"adaptive", "display":"summarized"})
        );
        let user = &body["messages"][0]["content"];
        assert_eq!(user[0]["text"], "look");
        assert_eq!(user[1]["source"]["data"], "eA==");
        assert_eq!(
            user[2],
            json!({"type":"text", "text":"File: notes.txt\nnotes"})
        );
        assert_eq!(body["messages"][1]["content"][0]["type"], "tool_use");
        let result = &body["messages"][2]["content"][0];
        assert_eq!(
            (&result["tool_use_id"], &result["is_error"]),
            (&json!("tool_1"), &json!(true))
        );
        let text: Value =
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(
            text,
            json!({"result":{"ok":false,"error":null},"is_error":true})
        );
        assert_eq!(result["content"][1]["type"], "image");
        // The history breakpoint precedes the tail, which merges into the same user turn.
        assert_eq!(result["cache_control"], json!({"type":"ephemeral"}));
        assert_eq!(
            body["messages"][2]["content"][1],
            json!({"type":"text", "text":"state"})
        );
        let markers = body["messages"]
            .to_string()
            .matches("cache_control")
            .count();
        assert_eq!(markers, 1);
    }

    #[test]
    fn history_breakpoint_skips_thinking_and_respects_budget() {
        let mut request = request();
        let native = json!({"type":"thinking", "thinking":"private", "signature":"signature"});
        let envelope = reasoning_envelope("anthropic", &request.model, native.clone());
        let reasoning = AssistantItem::reasoning("r", 0, "", Some(envelope));
        request.history.push(Message::Assistant(vec![reasoning]));
        request.tail = vec![Message::User(vec![text("state")])];
        let body = encode(&request).unwrap();
        let messages = &body["messages"];
        assert_eq!(
            messages[0]["content"][0]["cache_control"],
            json!({"type":"ephemeral"})
        );
        assert_eq!(messages[1]["content"], json!([native]));
        assert_eq!(
            messages[2]["content"],
            json!([{"type":"text", "text":"state"}])
        );
        let unmarked = |request: &ModelRequest| {
            let body = encode(request).unwrap().to_string();
            !body.contains("cache_control")
        };
        // Detached history shares no cached prefix with any other request.
        request.history_lifetime = HistoryLifetime::Detached;
        assert!(unmarked(&request));
        request.history_lifetime = HistoryLifetime::Extends;
        // A tail-only request has no history to cache.
        request.history.clear();
        assert!(unmarked(&request));
        let cached = SystemSegment {
            text: "cached".into(),
            cache: true,
        };
        request.system = vec![cached; 4];
        assert!(encode(&request).is_ok());
        request.history = vec![Message::User(vec![text("hello")])];
        assert!(encode(&request).is_err());
    }

    #[test]
    fn configured_thinking_requests_summaries() {
        let mut request = request();
        request.history = vec![Message::User(vec![text("hello")])];
        for (reasoning, thinking) in [
            (
                "adaptive",
                json!({"type":"adaptive", "display":"summarized"}),
            ),
            ("off", json!({"type":"disabled"})),
        ] {
            request.reasoning = Some(reasoning.into());
            assert_eq!(encode(&request).unwrap()["thinking"], thinking);
        }
        // Manual budgets require thinking that compaction may have removed.
        request.reasoning = Some("2048".into());
        assert!(encode(&request).is_err());
    }

    #[test]
    fn opaque_reasoning_replays_only_matching_protocol_and_model() {
        let native = json!({"type":"thinking", "thinking":"private", "signature":"signature", "future_field":42});
        let mut request = request();
        let envelope = reasoning_envelope("anthropic", &request.model, native.clone());
        let mutations: [fn(&mut Replay); 2] = [
            |envelope| envelope.provenance.protocol = "responses".into(),
            |envelope| envelope.provenance.model = "another-model".into(),
        ];
        let mut envelopes = vec![(Some(envelope.clone()), true), (None, false)];
        envelopes.extend(mutations.map(|mutate| {
            let mut foreign = envelope.clone();
            mutate(&mut foreign);
            (Some(foreign), false)
        }));
        for (replay, matches) in envelopes {
            let mut item = AssistantItem::reasoning("r", 0, "visible", replay);
            let AssistantItem::Reasoning { blocks, .. } = &mut item else {
                unreachable!()
            };
            blocks.push(TextBlock {
                id: BlockId::try_from("second".to_owned()).unwrap(),
                position: 1.into(),
                text: "second summary".into(),
            });
            request.history = continue_after(item);
            let body = encode(&request).unwrap();
            if matches {
                assert_eq!(body["messages"][0]["content"], json!([native]));
            } else {
                assert!(!body.to_string().contains("visible"));
                assert!(!body.to_string().contains("private"));
            }
        }
        let unsigned = json!({"type":"thinking","thinking":"x"});
        let unsigned = reasoning_envelope("anthropic", &request.model, unsigned);
        let item = AssistantItem::reasoning("r", 0, "", Some(unsigned));
        request.history = vec![Message::Assistant(vec![item])];
        assert!(encode(&request).is_err());
    }

    #[test]
    fn blank_text_is_skipped_and_a_message_encoding_to_nothing_is_dropped() {
        let mut request = request();
        // A blank block is dropped, but its siblings still encode.
        let call = ToolCall::new("call", "shell", json!({})).unwrap();
        request.history = continue_after_items(vec![
            AssistantItem::text("blank", 0, "   "),
            AssistantItem::tool_call("t", 1, call),
        ]);
        let body = encode(&request).unwrap();
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "tool_use");
        // A turn that encodes to nothing is skipped, never rejected: rejecting it
        // would fail every later request in an append-only session forever.
        for text in ["", "   "] {
            request.history = continue_after(AssistantItem::text("blank", 0, text));
            let body = encode(&request).unwrap();
            assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        }
    }

    #[test]
    fn empty_and_replay_only_reasoning_keep_historical_behavior() {
        let mut request = request();
        let native = json!({"type":"redacted_thinking", "data":"opaque"});
        let envelope = reasoning_envelope("anthropic", &request.model, native.clone());
        for replay in [None, Some(envelope)] {
            let has_replay = replay.is_some();
            request.history = continue_after(AssistantItem::Reasoning {
                id: ItemId::try_from("r".to_owned()).unwrap(),
                position: 0.into(),
                blocks: vec![],
                replay,
            });
            let body = encode(&request).unwrap();
            let messages = body["messages"].as_array().unwrap();
            assert_eq!(messages.len(), 1 + usize::from(has_replay));
            if has_replay {
                assert_eq!(messages[0]["content"], json!([native]));
            }
        }
    }
}
