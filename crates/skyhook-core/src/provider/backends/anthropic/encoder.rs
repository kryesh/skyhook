//! Anthropic Messages request encoding.
use super::super::common::{anthropic_image, invalid, opaque_payload, tool_text};
use super::native::validate_thinking;
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
    for message in &request.messages {
        let (role, content) = match message {
            Message::User(items) => {
                let mut blocks = Vec::new();
                for item in items {
                    blocks.push(match item {
                        UserContent::Text { text }
                        | UserContent::Runtime { text }
                        | UserContent::ParentInput { text }
                        | UserContent::Compaction { text } => json!({"type":"text", "text":text}),
                        UserContent::Image { image } => anthropic_image(image)?,
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
                                if call.id.is_empty()
                                    || call.name.is_empty()
                                    || !call.arguments.is_object()
                                {
                                    return Err(invalid(
                                        "Anthropic tool calls require nonempty id/name and object arguments",
                                    ));
                                }
                                blocks.push(json!({"type":"tool_use", "id":call.id, "name":call.name, "input":call.arguments}));
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
                        content.push(anthropic_image(image)?);
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
        media::ImageReference,
        provider::protocol::{
            AssistantItem, ResponseSchema, SystemSegment, ToolCall, ToolDefinition, ToolResult,
        },
    };
    fn request() -> ModelRequest {
        ModelRequest {
            correlation: Some("local-trace".into()),
            ..crate::provider::backends::common::tests::request("claude-test")
        }
    }

    fn image() -> ImageReference {
        ImageReference {
            sha256: "hash".into(),
            media_type: "image/png".into(),
            name: "test.png".into(),
            bytes: 1,
            data_base64: Some("eA==".into()),
        }
    }

    #[test]
    fn request_maps_native_schema_tools_system_cache_and_adaptive_effort() {
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
        request.response_schema = Some(ResponseSchema {
            name: "answer".into(),
            schema: json!({"type":"object", "properties":{}, "additionalProperties":false}),
        });
        request.reasoning = Some("high".into());
        request.messages = vec![
            Message::User(vec![
                UserContent::Text {
                    text: "look".into(),
                },
                UserContent::Image { image: image() },
            ]),
            Message::Assistant(vec![AssistantItem::tool_call(
                "0",
                0,
                ToolCall {
                    id: "tool_1".into(),
                    name: "look".into(),
                    arguments: json!({"path":"test.png"}),
                },
            )]),
            Message::Tool(vec![ToolResult {
                call_id: "tool_1".into(),
                name: "look".into(),
                result: json!({"ok":false}),
                images: vec![image()],
                is_error: true,
            }]),
        ];
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
        assert_eq!(
            body["output_config"]["format"]["schema"],
            request.response_schema.unwrap().schema
        );
        assert_eq!(body["output_config"]["effort"], "high");
        assert_eq!(body["thinking"], json!({"type":"adaptive"}));
        assert_eq!(body["messages"][0]["content"][0]["text"], "look");
        assert_eq!(body["messages"][0]["content"][1]["source"]["data"], "eA==");
        assert_eq!(body["messages"][1]["content"][0]["type"], "tool_use");
        let result = &body["messages"][2]["content"][0];
        assert_eq!(result["tool_use_id"], "tool_1");
        assert_eq!(result["is_error"], true);
        assert_eq!(result["content"][1]["type"], "image");
    }

    #[test]
    fn opaque_reasoning_replays_only_matching_version_protocol_and_model() {
        use crate::provider::protocol::AssistantBlock;
        let native = json!({"type":"thinking", "thinking":"private", "signature":"signature", "future_field":42});
        let mut request = request();
        let envelope = reasoning_envelope("anthropic", &request.model, native.clone());
        let mut envelopes = vec![(Some(envelope.clone()), true), (None, false)];
        let mut foreign = envelope.clone();
        foreign.version = 2;
        envelopes.push((Some(foreign), false));
        let mut foreign = envelope.clone();
        foreign.protocol = "responses".into();
        envelopes.push((Some(foreign), false));
        let mut foreign = envelope.clone();
        foreign.model = "another-model".into();
        envelopes.push((Some(foreign), false));
        for (replay, matches) in envelopes {
            let mut item = AssistantItem::reasoning("r", 0, "visible", replay);
            item.blocks.push(AssistantBlock {
                id: "second".into(),
                position: 1,
                content: BlockContent::Reasoning {
                    text: "second summary".into(),
                },
            });
            request.messages = vec![
                Message::Assistant(vec![item]),
                Message::User(vec![UserContent::Text {
                    text: "continue".into(),
                }]),
            ];
            let body = encode(&request).unwrap();
            if matches {
                assert_eq!(body["messages"][0]["content"], json!([native]));
            } else {
                assert!(!body.to_string().contains("visible"));
                assert!(!body.to_string().contains("private"));
            }
        }
        request.messages = vec![Message::Assistant(vec![AssistantItem::reasoning(
            "r",
            0,
            "",
            Some(reasoning_envelope(
                "anthropic",
                &request.model,
                json!({"type":"thinking","thinking":"x"}),
            )),
        )])];
        assert!(encode(&request).is_err());
    }
}
