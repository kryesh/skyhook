//! Anthropic Messages request encoding.
use super::{Dialect, native::validate_thinking};
use crate::provider::{
    ProviderError,
    codec::{
        CodecName,
        common::{
            anthropic_image, check_tool, invalid, own_replay, signed_context, tool_text, user_parts,
        },
        placement::{breakpoint, path},
    },
    protocol::{AssistantItem, Binding, HistoryLifetime, Message, ModelRequest, ReplayFormat},
};
use serde_json::{Map, Value, json};

/// The body fields this codec writes.
pub(crate) const BODY_FIELDS: &[&str] = &[
    "model",
    "stream",
    "system",
    "messages",
    "tools",
    "output_config.format",
    "thinking",
];

pub(crate) fn encode(
    request: &ModelRequest,
    dialect: &Dialect,
) -> Result<Map<String, Value>, ProviderError> {
    let max_tokens = request
        .max_output_tokens
        .filter(|n| *n > 0)
        .ok_or_else(|| {
            invalid(
                "Anthropic requires max_output_tokens to be explicitly set to a positive integer",
            )
        })?;
    let mut body = Map::new();
    body.insert("model".into(), json!(request.model));
    body.insert("stream".into(), json!(true));
    dialect.output_limit.set(&mut body, json!(max_tokens))?;
    let mut system = Vec::new();
    let mut cache_breakpoints = 0;
    let breakpoint = || breakpoint(dialect.cache_ttl);
    for segment in &request.system {
        let mut block = json!({"type": "text", "text": segment.text});
        if segment.cache {
            cache_breakpoints += 1;
            block["cache_control"] = breakpoint();
        }
        system.push(block);
    }
    if !system.is_empty() {
        body.insert("system".into(), Value::Array(system));
    }
    let signed_only = signed_context(request, ReplayFormat::Messages);
    let mut messages: Vec<Value> = Vec::new();
    for message in &request.history {
        push_message(request, signed_only, &mut messages, message)?;
    }
    // History is an unchanged prefix of later requests (reads land only at breakpoints, so
    // even ending history is marked); the tail and detached history never are.
    if request.history_lifetime != HistoryLifetime::Detached {
        cache_breakpoints += usize::from(mark_last_cacheable(&mut messages, breakpoint()));
    }
    if cache_breakpoints > 4 {
        return Err(invalid(
            "Anthropic supports at most four cache breakpoints, including one for history",
        ));
    }
    for message in &request.tail {
        push_message(request, signed_only, &mut messages, message)?;
    }
    if messages.is_empty() {
        return Err(invalid("Anthropic requires at least one message"));
    }
    body.insert("messages".into(), Value::Array(messages));
    if !request.tools.is_empty() {
        let mut tools = Vec::new();
        for tool in &request.tools {
            check_tool(tool, dialect.tool_names, CodecName::Messages)?;
            let mut native = json!({"name":tool.name, "input_schema":tool.input_schema});
            if !tool.description.is_empty() {
                native["description"] = json!(tool.description);
            }
            tools.push(native);
        }
        body.insert("tools".into(), Value::Array(tools));
    }
    if let Some(schema) = &request.response_schema {
        if !schema.schema.is_object() {
            return Err(invalid(
                "Anthropic response_schema must be a JSON Schema object",
            ));
        }
        // Messages uses output_config.format, not the retired output_format beta field.
        let format = json!({"type":"json_schema", "schema":schema.schema});
        const { path("output_config.format") }.set(&mut body, format)?;
    }
    // Summarized thinking is the default only on older models; request it wherever thinking is on.
    if let Some(reasoning) = &request.reasoning {
        let mut adaptive = json!({"type":"adaptive", "display":"summarized"});
        if dialect.thinking_binding == super::ThinkingBinding::DropOnMismatch {
            adaptive["block_binding"] = json!({"prefix_mismatch_behavior": "drop_block"});
        }
        let thinking = match reasoning.as_str() {
            "off" => json!({"type":"disabled"}),
            "adaptive" => adaptive,
            level => {
                dialect.effort.place(&mut body, level)?;
                adaptive
            }
        };
        body.insert("thinking".into(), thinking);
    }
    Ok(body)
}

fn push_message(
    request: &ModelRequest,
    signed_only: bool,
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
                    // Foreign private reasoning is display-only.
                    AssistantItem::Reasoning { replay, .. } => {
                        if let Some(replay) =
                            own_replay(replay.as_ref(), ReplayFormat::Messages, &request.model)
                            && (!signed_only || replay.binding == Binding::Conversation)
                        {
                            validate_thinking(&replay.payload)
                                .map_err(|error| invalid(error.message))?;
                            blocks.push(replay.payload.clone());
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
fn mark_last_cacheable(messages: &mut [Value], breakpoint: Value) -> bool {
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
    block["cache_control"] = breakpoint;
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::codec::common::tests::{envelope, image, notes};
    use crate::{
        media::AttachmentRef,
        provider::protocol::{
            AssistantItem, Binding, BlockId, HistoryLifetime, Replay, ResponseSchema,
            SystemSegment, TextBlock, ToolCall, ToolDefinition, ToolResult, UserContent,
        },
    };

    fn signed_envelope(model: &str, payload: serde_json::Value) -> Replay {
        envelope(
            ReplayFormat::Messages,
            model,
            payload,
            Binding::Conversation,
        )
    }

    fn request() -> ModelRequest {
        crate::provider::codec::common::tests::request("claude-test")
    }

    fn text(text: &str) -> UserContent {
        UserContent::Text { text: text.into() }
    }

    fn continue_after(items: Vec<AssistantItem>) -> Vec<Message> {
        vec![
            Message::Assistant(items),
            Message::User(vec![text("continue")]),
        ]
    }

    #[test]
    fn tool_names_follow_the_endpoint_alphabet() {
        let mut request = request();
        let tool = |name: &str| ToolDefinition {
            name: name.into(),
            description: String::new(),
            input_schema: json!({"type":"object"}),
        };
        request.tools = vec![tool(&"a".repeat(128))];
        assert!(encode(&request, &Dialect::anthropic()).is_ok());
        for name in ["a".repeat(129), "vendor.tool".into()] {
            request.tools = vec![tool(&name)];
            assert!(encode(&request, &Dialect::anthropic()).is_err(), "{name}");
        }
        let lenient = Dialect {
            tool_names: crate::provider::codec::ToolNames::Any,
            ..Dialect::anthropic()
        };
        assert!(encode(&request, &lenient).is_ok());
    }

    #[test]
    fn request_maps_native_schema_tools_system_cache_and_adaptive_effort() {
        let (image, notes) = (image(), notes());
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
        assert!(encode(&request, &Dialect::anthropic()).is_err());
        // Load the fixture blobs as the session store would.
        request.blobs.insert(image.blob, b"\x01\x02\x03".to_vec());
        request.blobs.insert(notes.blob, b"notes".to_vec());
        let body = encode(&request, &Dialect::anthropic()).unwrap();
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
        assert_eq!(user[1]["source"]["data"], "AQID");
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
        // A limit placed inside output_config sits beside the format and effort.
        let nested = Dialect {
            output_limit: const { path("output_config.max_tokens") },
            ..Dialect::anthropic()
        };
        let body = encode(&request, &nested).unwrap();
        let format = json!({"type":"json_schema", "schema":schema});
        assert_eq!(
            body["output_config"],
            json!({"max_tokens":8192, "format":format, "effort":"high"})
        );
    }

    #[test]
    fn unsigned_thinking_replays_until_the_context_is_signed() {
        let unsigned = json!({"type":"thinking", "thinking":"open"});
        let signed = json!({"type":"thinking", "thinking":"private", "signature":"sig"});
        let mut request = request();
        // The decoder binds exactly the signed blocks to the conversation.
        let item = |id: &str, native: &Value| {
            let binding = if native.get("signature").is_some() {
                Binding::Conversation
            } else {
                Binding::Free
            };
            let replay = envelope(
                ReplayFormat::Messages,
                &request.model,
                native.clone(),
                binding,
            );
            AssistantItem::reasoning(id, 0, "", Some(replay))
        };
        let said = |text: &str| AssistantItem::text(text, 1, text);
        request
            .history
            .push(Message::Assistant(vec![item("a", &unsigned), said("one")]));
        // The last history text also takes the cache breakpoint.
        let body = encode(&request, &Dialect::anthropic()).unwrap();
        assert_eq!(
            body["messages"][1]["content"],
            json!([unsigned, {"type":"text","text":"one","cache_control":{"type":"ephemeral"}}])
        );
        // A signed block anywhere in the context makes unsigned ones unsendable.
        request.history.extend([
            Message::User(vec![text("more")]),
            Message::Assistant(vec![item("b", &signed), said("two")]),
        ]);
        let body = encode(&request, &Dialect::anthropic()).unwrap();
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(
            messages[1]["content"],
            json!([{"type":"text","text":"one"}])
        );
        assert_eq!(messages[3]["content"][0], signed);
        assert!(!json!(body).to_string().contains("open"));
        // An empty signature is neither signed nor unsigned: it is a broken block.
        let broken = json!({"type":"thinking", "thinking":"x", "signature":""});
        let item = |id: &str, native: &Value| {
            let replay = envelope(
                ReplayFormat::Messages,
                &request.model,
                native.clone(),
                Binding::Free,
            );
            AssistantItem::reasoning(id, 0, "", Some(replay))
        };
        request.history = vec![Message::Assistant(vec![item("c", &broken), said("x")])];
        assert!(encode(&request, &Dialect::anthropic()).is_err());
    }

    #[test]
    fn history_breakpoint_skips_thinking_and_respects_budget() {
        let mut request = request();
        let native = json!({"type":"thinking", "thinking":"private", "signature":"signature"});
        let envelope = signed_envelope(&request.model, native.clone());
        let reasoning = AssistantItem::reasoning("r", 0, "", Some(envelope));
        request.history.push(Message::Assistant(vec![reasoning]));
        request.tail = vec![Message::User(vec![text("state")])];
        let body = encode(&request, &Dialect::anthropic()).unwrap();
        let messages = &body["messages"];
        assert_eq!(
            messages[0]["content"][0]["cache_control"],
            json!({"type":"ephemeral"})
        );
        assert_eq!(messages[1]["content"], json!([native]));
        let timed = Dialect {
            cache_ttl: Some(crate::provider::codec::CacheTtl::OneHour),
            ..Dialect::anthropic()
        };
        assert_eq!(
            encode(&request, &timed).unwrap()["messages"][0]["content"][0]["cache_control"],
            json!({"type":"ephemeral", "ttl":"1h"})
        );
        assert_eq!(
            messages[2]["content"],
            json!([{"type":"text", "text":"state"}])
        );
        let unmarked = |request: &ModelRequest| {
            let body = json!(encode(request, &Dialect::anthropic()).unwrap()).to_string();
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
        assert!(encode(&request, &Dialect::anthropic()).is_ok());
        request.history = vec![Message::User(vec![text("hello")])];
        assert!(encode(&request, &Dialect::anthropic()).is_err());
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
            assert_eq!(
                encode(&request, &Dialect::anthropic()).unwrap()["thinking"],
                thinking
            );
        }
        // Manual budgets require thinking that compaction may have removed.
        request.reasoning = Some("2048".into());
        assert!(encode(&request, &Dialect::anthropic()).is_err());
        // Letting the service drop mismatched blocks is announced on the request.
        request.reasoning = Some("high".into());
        let dropping = Dialect {
            thinking_binding: super::super::ThinkingBinding::DropOnMismatch,
            ..Dialect::anthropic()
        };
        assert_eq!(
            encode(&request, &dropping).unwrap()["thinking"]["block_binding"],
            json!({"prefix_mismatch_behavior":"drop_block"})
        );
    }

    #[test]
    fn opaque_reasoning_replays_only_matching_format_and_model() {
        let native = json!({"type":"thinking", "thinking":"private", "signature":"signature", "future_field":42});
        let mut request = request();
        let envelope = signed_envelope(&request.model, native.clone());
        let mutations: [fn(&mut Replay); 2] = [
            |envelope| envelope.provenance.format = ReplayFormat::Responses,
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
            request.history = continue_after(vec![item, AssistantItem::text("said", 1, "answer")]);
            let body = encode(&request, &Dialect::anthropic()).unwrap();
            let content = body["messages"][0]["content"].as_array().unwrap();
            if matches {
                assert_eq!(content[0], native);
                assert_eq!(content.len(), 2);
            } else {
                assert_eq!(content.len(), 1);
                assert!(!json!(body).to_string().contains("visible"));
                assert!(!json!(body).to_string().contains("private"));
            }
        }
    }

    #[test]
    fn blank_text_beside_a_call_is_not_sent_and_an_empty_turn_is_refused() {
        let mut request = request();
        // The service rejects whitespace-only blocks; the call carries the turn.
        let call = ToolCall::new("call", "exec", json!({})).unwrap();
        request.history = continue_after(vec![
            AssistantItem::text("blank", 0, "   "),
            AssistantItem::tool_call("t", 1, call),
        ]);
        let body = encode(&request, &Dialect::anthropic()).unwrap();
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "tool_use");
        // The runtime never journals a turn without content; one that reaches
        // the codec is an error, not something to paper over.
        request.history = continue_after(vec![AssistantItem::text("blank", 0, "   ")]);
        assert!(encode(&request, &Dialect::anthropic()).is_err());
    }
}
