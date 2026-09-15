//! Anthropic Messages request encoding.
use super::super::common::{anthropic_image, attachment_text, invalid, opaque_payload, tool_text};
use super::native::validate_thinking;
use crate::media::AttachmentRef;
use crate::provider::{
    ProviderError,
    protocol::{BlockContent, ItemKind, Message, ModelRequest, UserContent},
};
use serde_json::{Value, json};

pub(crate) fn encode(request: &ModelRequest) -> Result<Value, ProviderError> {
    if request.model.trim().is_empty() {
        return Err(invalid("Anthropic requires a nonempty model"));
    }
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
    if cache_breakpoints > 4 {
        return Err(invalid("Anthropic supports at most four cache breakpoints"));
    }
    if !system.is_empty() {
        body["system"] = Value::Array(system);
    }
    let mut messages: Vec<Value> = Vec::new();
    for message in request.messages() {
        let (role, content) = match message {
            Message::User(items) => {
                let mut blocks = Vec::new();
                for item in items {
                    blocks.push(match item {
                        UserContent::Text { text }
                        | UserContent::Runtime { text }
                        | UserContent::ParentInput { text }
                        | UserContent::Compaction { text } => json!({"type":"text", "text":text}),
                        UserContent::Attachment { attachment } => match attachment {
                            AttachmentRef::Image(image) => anthropic_image(request, image)?,
                            AttachmentRef::Text(text) => {
                                json!({"type":"text", "text":attachment_text(request, text)?})
                            }
                        },
                    });
                }
                ("user", blocks)
            }
            Message::Assistant(items) => {
                let mut blocks = Vec::new();
                for item in items {
                    // Replay belongs to the native item, not to each display block.
                    if let Some(payload) = opaque_payload(&item.replay, "anthropic", &request.model)
                    {
                        validate_thinking(payload).map_err(|error| invalid(error.message))?;
                        blocks.push(payload.clone());
                        continue;
                    }
                    for block in &item.blocks {
                        match &block.content {
                            BlockContent::Text { text } => {
                                blocks.push(json!({"type":"text", "text":text}))
                            }
                            // Unsigned/foreign private reasoning is display-only.
                            BlockContent::Reasoning { .. } => {}
                            BlockContent::ToolCall(call) => {
                                blocks.push(json!({"type":"tool_use", "id":call.id(), "name":call.name(), "input":call.arguments()}));
                            }
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
            // Empty foreign redacted reasoning contributes no replayable content.
            if matches!(message, Message::Assistant(items) if !items.is_empty()
                && items.iter().all(|item| item.kind == ItemKind::Reasoning))
            {
                continue;
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
            tools.push(json!({"name":tool.name, "description":tool.description,
                "input_schema":tool.input_schema}));
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
    if let Some(reasoning) = &request.reasoning {
        match reasoning.as_str() {
            "off" => body["thinking"] = json!({"type":"disabled"}),
            "adaptive" => body["thinking"] = json!({"type":"adaptive"}),
            "low" | "medium" | "high" | "max" => {
                body["thinking"] = json!({"type":"adaptive"});
                output_config.insert("effort".into(), json!(reasoning));
            }
            value if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) => {
                let budget = value
                    .parse::<u64>()
                    .map_err(|_| invalid("Anthropic thinking budget exceeds u64"))?;
                if budget < 1024 || budget >= max_tokens {
                    return Err(invalid(
                        "Anthropic manual thinking budget must be >= 1024 and strictly less than max_output_tokens",
                    ));
                }
                body["thinking"] = json!({"type":"enabled", "budget_tokens":budget});
            }
            _ => {
                return Err(invalid(
                    "Unsupported Anthropic reasoning setting: use off, adaptive, low, medium, high, max, or an integer thinking-token budget",
                ));
            }
        }
    }
    if !output_config.is_empty() {
        body["output_config"] = Value::Object(output_config);
    }
    // correlation is local tracing information, not Anthropic user metadata.
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::backends::common::reasoning_envelope;
    use crate::{
        media::{AttachmentRef, BlobRef, ImageFormat, ImageRef, TextRef},
        provider::protocol::{
            AssistantBlock, AssistantItem, BlockContent, ReplayEnvelope, ResponseSchema,
            SystemSegment, ToolCall, ToolDefinition, ToolResult,
        },
    };

    fn request() -> ModelRequest {
        ModelRequest {
            correlation: Some("local-trace".into()),
            ..crate::provider::backends::common::tests::request("claude-test")
        }
    }

    fn text(text: &str) -> UserContent {
        UserContent::Text { text: text.into() }
    }

    fn continue_after(item: AssistantItem) -> Vec<Message> {
        vec![
            Message::Assistant(vec![item]),
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
        assert_eq!(body["thinking"], json!({"type":"adaptive"}));
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
        assert_eq!(text, json!({"result":{"ok":false},"is_error":true}));
        assert_eq!(result["content"][1]["type"], "image");
    }

    #[test]
    fn opaque_reasoning_replays_only_matching_version_protocol_and_model() {
        let native = json!({"type":"thinking", "thinking":"private", "signature":"signature", "future_field":42});
        let mut request = request();
        let envelope = reasoning_envelope("anthropic", &request.model, native.clone());
        let mutations: [fn(&mut ReplayEnvelope); 3] = [
            |envelope| envelope.version = 2,
            |envelope| envelope.protocol = "responses".into(),
            |envelope| envelope.model = "another-model".into(),
        ];
        let mut envelopes = vec![(Some(envelope.clone()), true), (None, false)];
        envelopes.extend(mutations.map(|mutate| {
            let mut foreign = envelope.clone();
            mutate(&mut foreign);
            (Some(foreign), false)
        }));
        for (replay, matches) in envelopes {
            let mut item = AssistantItem::reasoning("r", 0, "visible", replay);
            item.blocks.push(AssistantBlock {
                id: "second".into(),
                position: 1,
                content: BlockContent::Reasoning {
                    text: "second summary".into(),
                },
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
    fn empty_and_replay_only_reasoning_keep_historical_behavior() {
        let mut request = request();
        let native = json!({"type":"redacted_thinking", "data":"opaque"});
        let envelope = reasoning_envelope("anthropic", &request.model, native.clone());
        for replay in [None, Some(envelope)] {
            let has_replay = replay.is_some();
            request.history = continue_after(AssistantItem {
                id: "r".into(),
                position: 0,
                kind: ItemKind::Reasoning,
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
