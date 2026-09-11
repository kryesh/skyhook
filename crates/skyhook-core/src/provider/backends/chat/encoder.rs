//! Encode canonical history and provider-bound reasoning replay into Chat requests.
use super::{schema::validate_schema, valid_name, wire};
use crate::provider::{
    ProviderError,
    backends::{
        ChatReasoningReplay,
        common::{image_url, invalid, opaque_payload, tool_text},
    },
    protocol::{BlockContent, Message, ModelRequest, UserContent},
};
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub(crate) fn encode(
    request: &ModelRequest,
    replay: ChatReasoningReplay,
) -> Result<Value, ProviderError> {
    if request.model.is_empty() {
        return Err(invalid("Chat Completions requires a model"));
    }
    // SystemSegment.cache is a cross-protocol hint. OpenAI automatically caches
    // eligible prefixes and has no per-segment cache-control wire field.
    let mut messages = Vec::new();
    for segment in &request.system {
        messages.push(json!({"role": "system", "content": segment.text}));
    }
    for message in &request.messages {
        match message {
            Message::User(parts) => {
                let mut content = Vec::new();
                for part in parts {
                    content.push(match part {
                        UserContent::Text { text }
                        | UserContent::Runtime { text }
                        | UserContent::ParentInput { text }
                        | UserContent::Compaction { text } => json!({"type": "text", "text": text}),
                        UserContent::Image { image } => {
                            json!({"type": "image_url", "image_url": {"url": image_url(image)?}})
                        }
                    });
                }
                messages.push(json!({"role": "user", "content": content}));
            }
            Message::Assistant(parts) => {
                let mut text = String::new();
                let mut calls = Vec::new();
                let mut reasoning = String::new();
                for item in parts {
                    if replay != ChatReasoningReplay::Unsupported
                        && let Some(payload) =
                            opaque_payload(&item.replay, "chat_completions", &request.model)
                        && let Some(text) = payload.get("text").and_then(Value::as_str)
                    {
                        reasoning.push_str(text);
                    }
                    for part in &item.blocks {
                        match &part.content {
                            BlockContent::Text { text: fragment } => text.push_str(fragment),
                            // Only the originating envelope is replayable. Visible
                            // reasoning (including foreign summaries) is not provenance.
                            BlockContent::Reasoning { .. } => {}
                            BlockContent::ToolCall(call) => {
                                if call.id.is_empty()
                                    || !valid_name(&call.name)
                                    || !call.arguments.is_object()
                                {
                                    return Err(invalid(
                                        "Chat tool calls require an ID, a valid function name, and object arguments",
                                    ));
                                }
                                calls.push(json!({
                                "id": call.id, "type": "function",
                                "function": {"name": call.name, "arguments": call.arguments.to_string()}
                            }));
                            }
                        }
                    }
                }
                // Preserve reasoning-only turns when the selected profile can
                // replay their provider-bound state; never turn thoughts into content.
                if !text.is_empty() || !calls.is_empty() || !reasoning.is_empty() {
                    let mut message = json!({"role": "assistant", "content": if text.is_empty() { Value::Null } else { Value::String(text) }});
                    if !calls.is_empty() {
                        message["tool_calls"] = Value::Array(calls);
                    }
                    if !reasoning.is_empty() {
                        let field = match replay {
                            ChatReasoningReplay::ReasoningContent => "reasoning_content",
                            ChatReasoningReplay::Reasoning => "reasoning",
                            ChatReasoningReplay::Unsupported => unreachable!(),
                        };
                        message[field] = Value::String(reasoning);
                    }
                    messages.push(message);
                }
            }
            Message::Tool(results) => {
                // All tool replies must precede the synthetic user image message.
                let mut images = Vec::new();
                for result in results {
                    if result.call_id.is_empty() {
                        return Err(invalid("Chat tool results require a call ID"));
                    }
                    messages.push(json!({"role": "tool", "tool_call_id": result.call_id, "content": tool_text(result)}));
                    if !result.images.is_empty() {
                        images.push(json!({"type": "text", "text": format!("Images returned by tool call {}:", result.call_id)}));
                        for image in &result.images {
                            images.push(json!({"type": "image_url", "image_url": {"url": image_url(image)?}}));
                        }
                    }
                }
                if !images.is_empty() {
                    messages.push(json!({"role": "user", "content": images}));
                }
            }
        }
    }
    let mut body = serde_json::to_value(wire::Request {
        model: &request.model,
        messages,
        n: 1,
        stream: true,
        stream_options: wire::StreamOptions {
            include_usage: true,
        },
    })
    .map_err(|_| invalid("Unable to serialize Chat request"))?;
    if let Some(effort) = &request.reasoning {
        if !matches!(
            effort.as_str(),
            "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
        ) {
            return Err(invalid(format!(
                "Unsupported standard Chat reasoning_effort: {effort}"
            )));
        }
        body["reasoning_effort"] = json!(effort);
    }
    if let Some(limit) = request.max_output_tokens {
        if limit == 0 {
            return Err(invalid("max_output_tokens must be positive"));
        }
        body["max_completion_tokens"] = json!(limit);
    }
    if !request.tools.is_empty() {
        let mut names = BTreeSet::new();
        let mut tools = Vec::new();
        for tool in &request.tools {
            if !valid_name(&tool.name) || !names.insert(&tool.name) {
                return Err(invalid(
                    "Chat tools require unique function names of 1–64 ASCII letters, digits, underscores or hyphens",
                ));
            }
            if !tool.input_schema.is_object() {
                return Err(invalid(
                    "Chat function parameters must be a JSON Schema object",
                ));
            }
            tools.push(json!({"type": "function", "function": {
                "name": tool.name, "description": tool.description, "parameters": tool.input_schema
            }}));
        }
        body["tools"] = Value::Array(tools);
    }
    if let Some(schema) = &request.response_schema {
        if !valid_name(&schema.name) {
            return Err(invalid(
                "Chat response schema requires a name of 1–64 ASCII letters, digits, underscores or hyphens",
            ));
        }
        if schema.schema.get("type").and_then(Value::as_str) != Some("object")
            || schema.schema.get("anyOf").is_some()
        {
            return Err(invalid(
                "Chat strict response schema root must be an object, not anyOf",
            ));
        }
        validate_schema(&schema.schema, &schema.schema, 0)?;
        body["response_format"] = json!({"type": "json_schema", "json_schema": {
            "name": schema.name, "strict": true, "schema": schema.schema
        }});
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::super::{
        Decoder,
        decoder::tests::{delta, end},
    };
    use super::*;
    use crate::provider::backends::common::{reasoning_envelope, tests::request};
    use crate::{
        media::ImageReference,
        provider::protocol::{
            AssistantItem, ResponseAssembler, SystemSegment, ToolCall, ToolDefinition, ToolResult,
        },
    };

    fn image() -> ImageReference {
        ImageReference {
            sha256: "digest".into(),
            media_type: "image/png".into(),
            name: "image.png".into(),
            bytes: 3,
            data_base64: Some("AQID".into()),
        }
    }

    #[test]
    fn request_preserves_text_tools_images_and_reasoning_settings() {
        let mut request = request("gpt-5");
        request.system = vec![SystemSegment {
            text: "system".into(),
            cache: true,
        }];
        request.reasoning = Some("high".into());
        request.tools = vec![ToolDefinition {
            name: "inspect".into(),
            description: "Inspect".into(),
            input_schema: json!({"type":"object"}),
        }];
        request.messages = vec![
            Message::User(vec![
                UserContent::Text {
                    text: "look".into(),
                },
                UserContent::Image { image: image() },
            ]),
            Message::Assistant(vec![
                AssistantItem::text("text", 0, "Checking"),
                AssistantItem::tool_call(
                    "call",
                    1,
                    ToolCall {
                        id: "call-a".into(),
                        name: "inspect".into(),
                        arguments: json!({"path":"a"}),
                    },
                ),
            ]),
            Message::Tool(vec![ToolResult {
                call_id: "call-a".into(),
                name: "inspect".into(),
                result: json!({"ok":false, "error":null}),
                is_error: true,
                images: vec![image()],
            }]),
        ];
        let body = encode(&request, ChatReasoningReplay::Unsupported).unwrap();
        assert_eq!(body["messages"][0]["content"], "system");
        assert_eq!(body["messages"][1]["content"][0]["text"], "look");
        assert_eq!(
            body["messages"][1]["content"][1]["image_url"]["url"],
            "data:image/png;base64,AQID"
        );
        assert_eq!(body["messages"][2]["content"], "Checking");
        assert_eq!(body["messages"][2]["tool_calls"][0]["id"], "call-a");
        assert_eq!(body["messages"][3]["tool_call_id"], "call-a");
        let result: Value =
            serde_json::from_str(body["messages"][3]["content"].as_str().unwrap()).unwrap();
        assert_eq!(result, json!({"result":{"ok":false},"is_error":true}));
        assert_eq!(
            body["messages"][4]["content"][1]["image_url"]["url"],
            "data:image/png;base64,AQID"
        );
        assert_eq!(body["tools"][0]["function"]["name"], "inspect");
        assert_eq!(body["reasoning_effort"], "high");
    }

    fn history(items: Vec<AssistantItem>) -> ModelRequest {
        ModelRequest {
            messages: vec![Message::Assistant(items)],
            ..request("test-model")
        }
    }

    #[test]
    fn reasoning_envelope_roundtrip_is_scoped_and_policy_selected() {
        use crate::provider::backends::common::{
            bind_reasoning_scope, filter_reasoning_scope, reasoning_scope,
        };
        let scope = reasoning_scope("local", "http://localhost/v1/chat/completions");
        let mut decoder = Decoder::new("test-model".into());
        let mut assembler = ResponseAssembler::default();
        for frame in [
            delta(json!({"reasoning_content":"first "})),
            delta(json!({"reasoning":"second"})),
            end("stop"),
        ] {
            for mut chunk in decoder.decode(&frame).unwrap() {
                bind_reasoning_scope(&mut chunk, &scope);
                assembler.push(&chunk).unwrap();
            }
        }
        for chunk in decoder.finish().unwrap() {
            assembler.push(&chunk).unwrap();
        }
        let (items, _, _) = assembler.finish().unwrap();
        let envelope = items[0].replay.as_ref().unwrap();
        assert_eq!(envelope.version, 1);
        assert_eq!(envelope.protocol, "chat_completions");
        assert_eq!(envelope.model, "test-model");
        assert_eq!(envelope.scope, scope);
        assert_eq!(envelope.payload, json!({"text":"first second"}));
        let original = history(items);
        for (policy, field, absent) in [
            (
                ChatReasoningReplay::ReasoningContent,
                "reasoning_content",
                "reasoning",
            ),
            (
                ChatReasoningReplay::Reasoning,
                "reasoning",
                "reasoning_content",
            ),
        ] {
            let mut request = original.clone();
            filter_reasoning_scope(&mut request, &scope);
            let body = encode(&request, policy).unwrap();
            assert_eq!(body["messages"].as_array().unwrap().len(), 1);
            assert!(body["messages"][0]["content"].is_null());
            assert_eq!(body["messages"][0][field], "first second");
            assert!(body["messages"][0].get(absent).is_none());
            assert_eq!(body["n"], 1);
        }
        assert_eq!(
            encode(&original, ChatReasoningReplay::Unsupported).unwrap()["messages"],
            json!([])
        );
        for mutation in [
            "scope", "model", "protocol", "version", "missing", "payload",
        ] {
            let mut request = original.clone();
            let Message::Assistant(items) = &mut request.messages[0] else {
                unreachable!()
            };
            let envelope = items[0].replay.as_mut().unwrap();
            match mutation {
                "scope" => envelope.scope = "elsewhere".into(),
                "model" => envelope.model = "different-model".into(),
                "protocol" => envelope.protocol = "responses".into(),
                "version" => envelope.version += 1,
                "payload" => envelope.payload = json!({"text":42}),
                "missing" => items[0].replay = None,
                _ => unreachable!(),
            }
            filter_reasoning_scope(&mut request, &scope);
            assert_eq!(
                encode(&request, ChatReasoningReplay::ReasoningContent).unwrap()["messages"],
                json!([]),
                "{mutation}"
            );
        }
    }

    #[test]
    fn replay_uses_payload_not_visible_blocks_and_keeps_text_and_tools() {
        let item = AssistantItem::reasoning(
            "r",
            0,
            "visible summary not original",
            Some(reasoning_envelope(
                "chat_completions",
                "test-model",
                json!({"text":"original private thought"}),
            )),
        );
        let req = history(vec![
            item,
            AssistantItem::text("t", 1, "answer"),
            AssistantItem::tool_call(
                "c",
                2,
                ToolCall {
                    id: "call".into(),
                    name: "inspect".into(),
                    arguments: json!({}),
                },
            ),
        ]);
        let body = encode(&req, ChatReasoningReplay::Reasoning).unwrap();
        assert_eq!(body["messages"][0]["reasoning"], "original private thought");
        assert_eq!(body["messages"][0]["content"], "answer");
        assert_eq!(body["messages"][0]["tool_calls"][0]["id"], "call");
        assert!(!body.to_string().contains("visible summary"));
    }

    #[test]
    fn max_reasoning_effort_is_allowed_but_unknown_values_are_not() {
        let mut req = history(vec![]);
        req.reasoning = Some("max".into());
        assert_eq!(
            encode(&req, ChatReasoningReplay::Unsupported).unwrap()["reasoning_effort"],
            "max"
        );
        req.reasoning = Some("unbounded".into());
        assert!(encode(&req, ChatReasoningReplay::Unsupported).is_err());
    }

    #[test]
    fn tool_schemas_are_preserved_without_strict_response_schema_restrictions() {
        let mut request = request("gpt-5");
        let schema = json!({"type":"object", "properties":{"value":{}, "choice":{"oneOf":[{"const":true},{"type":"array"}]}}});
        request.tools.push(ToolDefinition {
            name: "fetch".into(),
            description: "Fetch".into(),
            input_schema: schema.clone(),
        });
        let body = encode(&request, ChatReasoningReplay::Unsupported).unwrap();
        assert_eq!(body["tools"][0]["function"]["parameters"], schema);
    }
}
