//! Encode canonical history and provider-bound reasoning replay into Chat requests.
use super::{schema::validate_schema, valid_name, wire};
use crate::provider::{
    ProviderError,
    backends::{
        ChatReasoningReplay,
        common::{
            attach_runtime_tail, image_url, invalid, opaque_payload, system_text, tool_text,
            user_parts, validate_openai_effort,
        },
    },
    protocol::{BlockContent, Message, ModelRequest},
};
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub(crate) fn encode(
    request: &ModelRequest,
    replay: ChatReasoningReplay,
) -> Result<Value, ProviderError> {
    // Cache hints need no wire field: OpenAI automatically caches matching
    // prefixes, and history precedes the per-request tail so the tail never
    // breaks the cached history prefix. Runtime state joins the final history turn,
    // so only that turn is re-read.
    let mut messages = Vec::new();
    // One leading system message: many open-model chat templates reject
    // repeated or non-leading system turns.
    if let Some(text) = system_text(request) {
        messages.push(json!({"role": "system", "content": text}));
    }
    for (index, message) in request.messages().enumerate() {
        if index >= request.history.len()
            && attach_runtime_tail(&mut messages, message, "text", |item| {
                (item["role"] == "tool").then(|| &mut item["content"])
            })
        {
            continue;
        }
        match message {
            Message::User(parts) => {
                let content = user_parts(request, parts, "text", |image| {
                    Ok(
                        json!({"type": "image_url", "image_url": {"url": image_url(request, image)?}}),
                    )
                })?;
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
                                // Results pair by call ID, so sanitizing an invalid name is safe.
                                calls.push(json!({
                                    "id": call.id(), "type": "function",
                                    "function": {"name": wire_name(call.name()), "arguments": serde_json::to_string(call.arguments()).expect("JSON object serialization cannot fail")}
                                }));
                            }
                        }
                    }
                }
                // Preserve reasoning-only turns when the selected profile can
                // replay their provider-bound state; never turn thoughts into content.
                if !text.is_empty() || !calls.is_empty() || !reasoning.is_empty() {
                    // Some chat templates fail on null or missing content.
                    let mut message = json!({"role": "assistant", "content": text});
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
                            images.push(json!({"type": "image_url", "image_url": {"url": image_url(request, image)?}}));
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
        stream: true,
        stream_options: wire::StreamOptions {
            include_usage: true,
        },
    })
    .map_err(|_| invalid("Unable to serialize Chat request"))?;
    flatten_text_content(&mut body["messages"]);
    if let Some(effort) = &request.reasoning {
        validate_openai_effort(effort)?;
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
            let mut function = json!({"name": tool.name, "parameters": tool.input_schema});
            if !tool.description.is_empty() {
                function["description"] = json!(tool.description);
            }
            tools.push(json!({"type": "function", "function": function}));
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

/// A historical tool-call name within the Chat function-name alphabet:
/// invalid characters become `_` and the name is capped at 64 bytes.
fn wire_name(name: &str) -> String {
    if valid_name(name) {
        return name.to_owned();
    }
    let sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    if sanitized.is_empty() {
        "_".to_owned()
    } else {
        sanitized
    }
}

/// Text-only user content is sent as a plain string, which every compatible
/// server and chat template accepts; image turns keep the parts array.
fn flatten_text_content(messages: &mut Value) {
    for message in messages.as_array_mut().into_iter().flatten() {
        let Some(parts) = message["content"].as_array() else {
            continue;
        };
        if parts.iter().all(|part| part["type"] == "text") {
            let text = parts
                .iter()
                .filter_map(|part| part["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n\n");
            message["content"] = Value::String(text);
        }
    }
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
        media::{AttachmentRef, BlobRef, ImageFormat, ImageRef, TextRef},
        provider::protocol::{
            AssistantItem, ItemKind, ReplayEnvelope, ResponseAssembler, SystemSegment, ToolCall,
            ToolDefinition, ToolResult, UserContent,
        },
    };

    fn image() -> ImageRef {
        ImageRef {
            file: Some("image.png".into()),
            format: ImageFormat::Png,
            blob: BlobRef::of(b"\x01\x02\x03"),
        }
    }

    fn history(items: Vec<AssistantItem>) -> ModelRequest {
        ModelRequest {
            tail: Vec::new(),
            history_lifetime: Default::default(),
            history: vec![Message::Assistant(items)],
            ..request("test-model")
        }
    }

    fn inspect(id: &str, arguments: Value) -> ToolCall {
        ToolCall::new(id, "inspect", arguments).unwrap()
    }

    #[test]
    fn request_preserves_text_tools_images_and_reasoning_settings() {
        let notes = TextRef {
            file: Some("notes.txt".into()),
            blob: BlobRef::of(b"notes"),
        };
        let mut request = request("gpt-5");
        request.system = vec![SystemSegment {
            text: "system".into(),
            cache: true,
        }];
        request.reasoning = Some("high".into());
        let schema = json!({"type":"object", "properties":{"value":{}, "choice":{"oneOf":[{"const":true},{"type":"array"}]}}});
        request.tools = vec![ToolDefinition {
            name: "inspect".into(),
            description: "Inspect".into(),
            input_schema: schema.clone(),
        }];
        let attach = |attachment| UserContent::Attachment { attachment };
        request.history = vec![
            Message::User(vec![
                UserContent::Text {
                    text: "look".into(),
                },
                attach(AttachmentRef::Image(image())),
                attach(AttachmentRef::Text(notes.clone())),
            ]),
            Message::Assistant(vec![
                AssistantItem::text("text", 0, "Checking"),
                AssistantItem::tool_call("call", 1, inspect("call-a", json!({"path":"a"}))),
            ]),
            Message::Tool(vec![ToolResult {
                call_id: "call-a".into(),
                name: "inspect".into(),
                result: json!({"ok":false, "error":null}),
                is_error: true,
                images: vec![image()],
            }]),
        ];
        assert!(encode(&request, ChatReasoningReplay::Unsupported).is_err());
        // Load the fixture blobs as the session store would.
        request.blobs.insert(image().blob, b"\x01\x02\x03".to_vec());
        request.blobs.insert(notes.blob, b"notes".to_vec());
        let body = encode(&request, ChatReasoningReplay::Unsupported).unwrap();
        let messages = &body["messages"];
        let image_url = "data:image/png;base64,AQID";
        assert_eq!(messages[0]["content"], "system");
        assert_eq!(messages[1]["content"][0]["text"], "look");
        assert_eq!(messages[1]["content"][1]["image_url"]["url"], image_url);
        assert_eq!(
            messages[1]["content"][2],
            json!({"type":"text", "text":"File: notes.txt\nnotes"})
        );
        assert_eq!(messages[2]["content"], "Checking");
        assert_eq!(messages[2]["tool_calls"][0]["id"], "call-a");
        assert_eq!(messages[3]["tool_call_id"], "call-a");
        let result: Value = serde_json::from_str(messages[3]["content"].as_str().unwrap()).unwrap();
        assert_eq!(
            result,
            json!({"result":{"ok":false,"error":null},"is_error":true})
        );
        assert_eq!(messages[4]["content"][1]["image_url"]["url"], image_url);
        assert!(messages[4]["content"].is_array());
        // Tool schemas are preserved without strict response-schema restrictions.
        assert_eq!(body["tools"][0]["function"]["name"], "inspect");
        assert_eq!(body["tools"][0]["function"]["parameters"], schema);
        assert_eq!(body["reasoning_effort"], "high");
        for (effort, valid) in [("max", true), ("unbounded", false)] {
            let mut request = history(vec![]);
            request.reasoning = Some(effort.into());
            let body = encode(&request, ChatReasoningReplay::Unsupported);
            assert_eq!(
                body.ok().map(|body| body["reasoning_effort"].clone()),
                valid.then(|| json!(effort))
            );
        }
        let long = "x".repeat(80);
        for (name, expected) in [
            ("vendor.tool/雪", "vendor_tool__".to_owned()),
            (long.as_str(), "x".repeat(64)),
        ] {
            let call = ToolCall::new("call", name, json!({"x-vendor": [null, 1]})).unwrap();
            let request = history(vec![AssistantItem::tool_call("item", 0, call)]);
            let body = encode(&request, ChatReasoningReplay::Unsupported).unwrap();
            let call = &body["messages"][0]["tool_calls"][0];
            assert_eq!(
                (&call["id"], &call["function"]["name"]),
                (&json!("call"), &json!(expected))
            );
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
        let frames = [
            delta(json!({"reasoning_content":"first "})),
            delta(json!({"reasoning":"second"})),
            end("stop"),
        ];
        for frame in frames {
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
        assert_eq!(
            (
                envelope.version,
                &*envelope.protocol,
                &*envelope.model,
                &envelope.scope
            ),
            (1, "chat_completions", "test-model", &scope)
        );
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
            let message = &body["messages"][0];
            assert_eq!(message["content"], "");
            assert_eq!(message[field], "first second");
            assert!(message.get(absent).is_none());
            assert!(body.get("n").is_none());
        }
        let unsupported = encode(&original, ChatReasoningReplay::Unsupported).unwrap();
        assert_eq!(unsupported["messages"], json!([]));
        let mutations: [fn(&mut ReplayEnvelope); 5] = [
            |envelope| envelope.scope = "elsewhere".into(),
            |envelope| envelope.model = "different-model".into(),
            |envelope| envelope.protocol = "responses".into(),
            |envelope| envelope.version += 1,
            |envelope| envelope.payload = json!({"text":42}),
        ];
        for mutation in mutations.map(Some).into_iter().chain([None]) {
            let mut request = original.clone();
            let Message::Assistant(items) = &mut request.history[0] else {
                unreachable!()
            };
            match mutation {
                Some(mutate) => mutate(items[0].replay.as_mut().unwrap()),
                None => items[0].replay = None,
            }
            filter_reasoning_scope(&mut request, &scope);
            let body = encode(&request, ChatReasoningReplay::ReasoningContent).unwrap();
            assert_eq!(body["messages"], json!([]));
        }
    }

    #[test]
    fn replay_uses_payload_not_visible_blocks_and_keeps_text_and_tools() {
        let envelope =
            |text| reasoning_envelope("chat_completions", "test-model", json!({ "text": text }));
        let item = AssistantItem::reasoning(
            "r",
            0,
            "visible summary not original",
            Some(envelope("original private thought")),
        );
        let req = history(vec![
            item,
            AssistantItem::text("t", 1, "answer"),
            AssistantItem::tool_call("c", 2, inspect("call", json!({}))),
        ]);
        let body = encode(&req, ChatReasoningReplay::Reasoning).unwrap();
        assert_eq!(body["messages"][0]["reasoning"], "original private thought");
        assert_eq!(body["messages"][0]["content"], "answer");
        assert_eq!(body["messages"][0]["tool_calls"][0]["id"], "call");
        assert!(!body.to_string().contains("visible summary"));
        // Empty reasoning items are omitted; replay-only items keep their payload.
        for replay in [None, Some(envelope("private"))] {
            let has_replay = replay.is_some();
            let item = AssistantItem {
                id: "r".into(),
                position: 0,
                kind: ItemKind::Reasoning,
                blocks: vec![],
                replay,
            };
            let body = encode(&history(vec![item]), ChatReasoningReplay::Reasoning).unwrap();
            let messages = body["messages"].as_array().unwrap();
            assert_eq!(messages.len(), usize::from(has_replay));
            if has_replay {
                assert_eq!(messages[0]["reasoning"], "private");
            }
        }
    }

    #[test]
    fn runtime_tail_joins_the_final_turn_instead_of_posing_as_the_user() {
        let state = || {
            Message::User(vec![UserContent::Runtime {
                text: "<skyhook_state>".into(),
            }])
        };
        let call = AssistantItem::tool_call("c", 0, inspect("call", json!({})));
        let result = Message::Tool(vec![ToolResult {
            call_id: "call".into(),
            name: "inspect".into(),
            result: json!("ok"),
            images: vec![],
            is_error: false,
        }]);
        let mut req = history(vec![call]);
        req.history.push(result);
        let without_tail = encode(&req, ChatReasoningReplay::Unsupported).unwrap();
        req.tail = vec![state()];
        let body = encode(&req, ChatReasoningReplay::Unsupported).unwrap();
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0], without_tail["messages"][0]);
        let tool = without_tail["messages"][1]["content"].as_str().unwrap();
        assert_eq!(messages[1]["content"], format!("{tool}\n\n<skyhook_state>"));
        // A user turn gains a part; an instruction or an assistant turn keeps its own message.
        let mut req = request("test-model");
        req.tail = vec![state()];
        let body = encode(&req, ChatReasoningReplay::Unsupported).unwrap();
        assert_eq!(
            body["messages"],
            json!([{"role":"user","content":"hello\n\n<skyhook_state>"}])
        );
        req.tail.push(Message::User(vec![UserContent::Compaction {
            text: "compact".into(),
        }]));
        let body = encode(&req, ChatReasoningReplay::Unsupported).unwrap();
        assert_eq!(body["messages"].as_array().unwrap().len(), 2);
        let mut req = history(vec![AssistantItem::text("t", 0, "done")]);
        req.tail = vec![state()];
        let body = encode(&req, ChatReasoningReplay::Unsupported).unwrap();
        assert_eq!(body["messages"][1]["role"], "user");
    }

    #[test]
    fn requests_use_the_minimal_widely_accepted_shape() {
        let mut request = request("model");
        request.system = ["one", "two"]
            .map(|text| SystemSegment {
                text: text.into(),
                cache: false,
            })
            .to_vec();
        request.tools = vec![ToolDefinition {
            name: "inspect".into(),
            description: String::new(),
            input_schema: json!({"type":"object"}),
        }];
        request
            .history
            .push(Message::Assistant(vec![AssistantItem::tool_call(
                "call",
                0,
                inspect("call-a", json!({})),
            )]));
        let body = encode(&request, ChatReasoningReplay::Unsupported).unwrap();
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[0], json!({"role":"system","content":"one\n\ntwo"}));
        assert_eq!(messages[1], json!({"role":"user","content":"hello"}));
        assert_eq!(messages[2]["content"], "");
        assert!(body["tools"][0]["function"].get("description").is_none());
        assert!(body.get("n").is_none());
    }
}
