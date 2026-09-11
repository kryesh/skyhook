//! Encode conversation history and request options as native Responses input.
use super::*;
use crate::provider::backends::common::{image_url, invalid, opaque_payload, tool_text};
use crate::provider::protocol::{Message, ModelRequest, UserContent};

pub(crate) fn encode(request: &ModelRequest) -> Result<Value, ProviderError> {
    if request.model.trim().is_empty() {
        return Err(invalid("Responses requires a nonempty model"));
    }
    // System cache flags are cross-provider hints. OpenAI automatically caches
    // matching prefixes and has no per-segment cache-control field.
    let mut input = Vec::new();
    for message in &request.messages {
        match message {
            Message::User(parts) => {
                let content = parts
                    .iter()
                    .map(|part| match part {
                        UserContent::Text { text }
                        | UserContent::Runtime { text }
                        | UserContent::ParentInput { text }
                        | UserContent::Compaction { text } => {
                            Ok(json!({"type":"input_text", "text":text}))
                        }
                        UserContent::Image { image } => {
                            Ok(json!({"type":"input_image", "image_url":image_url(image)?}))
                        }
                    })
                    .collect::<Result<Vec<_>, ProviderError>>()?;
                input.push(json!({"role":"user", "content":content}));
            }
            Message::Assistant(items) => {
                for item in items {
                    if item.kind == ItemKind::Reasoning {
                        // Private replay belongs to the item, never to each display summary.
                        if let Some(native) =
                            opaque_payload(&item.replay, "responses", &request.model)
                        {
                            if kind(native).map_err(|error| invalid(error.message))?
                                != Kind::Reasoning
                            {
                                return Err(invalid(
                                    "Responses reasoning envelope contains a non-reasoning item",
                                ));
                            }
                            final_parts(native).map_err(|error| invalid(error.message))?;
                            input.push(native.clone());
                        }
                        continue;
                    }
                    for part in &item.blocks {
                        match &part.content {
                            BlockContent::Text { text } => input.push(json!({
                                "role":"assistant", "content":[{"type":"output_text", "text":text}]
                            })),
                            BlockContent::ToolCall(call) => {
                                if call.id.is_empty()
                                    || call.name.is_empty()
                                    || !call.arguments.is_object()
                                {
                                    return Err(invalid(
                                        "Responses function calls require call ID, name and object arguments",
                                    ));
                                }
                                input.push(json!({"type":"function_call", "call_id":call.id,
                                    "name":call.name, "arguments":call.arguments.to_string()}));
                            }
                            BlockContent::Reasoning { .. } => {}
                        }
                    }
                }
            }
            Message::Tool(results) => {
                for result in results {
                    if result.call_id.is_empty() {
                        return Err(invalid("Responses tool output requires a call ID"));
                    }
                    input.push(
                        json!({"type":"function_call_output", "call_id":result.call_id,
                        "output":tool_text(result)}),
                    );
                    if !result.images.is_empty() {
                        let mut content = vec![json!({"type":"input_text", "text":format!(
                            "Images from tool {} (call_id: {}):", result.name, result.call_id)})];
                        for image in &result.images {
                            content
                                .push(json!({"type":"input_image", "image_url":image_url(image)?}));
                        }
                        input.push(json!({"role":"user", "content":content}));
                    }
                }
            }
        }
    }
    let mut body = json!({"model":request.model, "input":input, "stream":true,
        "store":false, "include":["reasoning.encrypted_content"],
        "reasoning":{"summary":"auto"}});
    if !request.system.is_empty() {
        body["instructions"] = Value::String(
            request
                .system
                .iter()
                .map(|s| s.text.as_str())
                .collect::<Vec<_>>()
                .join("\n\n"),
        );
    }
    if !request.tools.is_empty() {
        let mut tools = Vec::new();
        for tool in &request.tools {
            if tool.name.is_empty() || !tool.input_schema.is_object() {
                return Err(invalid(
                    "Responses tools require a name and object JSON Schema",
                ));
            }
            tools.push(json!({"type":"function", "name":tool.name,
                "description":tool.description, "parameters":tool.input_schema, "strict":false}));
        }
        body["tools"] = Value::Array(tools);
    }
    if let Some(schema) = &request.response_schema {
        if schema.name.is_empty() || !schema.schema.is_object() {
            return Err(invalid(
                "Responses structured output requires a name and object JSON Schema",
            ));
        }
        body["text"] = json!({"format":{"type":"json_schema", "name":schema.name,
            "schema":schema.schema, "strict":true}});
    }
    if let Some(effort) = &request.reasoning {
        if !matches!(
            effort.as_str(),
            "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
        ) {
            return Err(invalid(format!(
                "Unsupported Responses reasoning effort: {effort}"
            )));
        }
        body["reasoning"]["effort"] = json!(effort);
    }
    if let Some(max) = request.max_output_tokens {
        if max == 0 {
            return Err(invalid("Responses max_output_tokens must be positive"));
        }
        body["max_output_tokens"] = json!(max);
    }
    if let Some(correlation) = &request.correlation {
        body["prompt_cache_key"] = json!(correlation);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::backends::common::tests::request;
    use crate::{
        media::ImageReference,
        provider::protocol::{
            AssistantItem, ResponseSchema, SystemSegment, ToolDefinition, ToolResult,
        },
    };

    fn assemble(output: Vec<Value>) -> Vec<AssistantItem> {
        let mut assembler = crate::provider::protocol::ResponseAssembler::default();
        for event in Decoder::new("gpt-5".into())
            .feed(completed(output))
            .unwrap()
        {
            assembler.push(&event).unwrap();
        }
        assembler.finish().unwrap().0
    }

    fn completed(output: Vec<Value>) -> Value {
        json!({"type":"response.completed", "response":{"status":"completed", "output":output,
            "usage":{"input_tokens":20, "output_tokens":7, "input_tokens_details":{"cached_tokens":12}}}})
    }

    fn text_item(id: &str, text: &str) -> Value {
        json!({"type":"message", "id":id, "role":"assistant",
            "content":[{"type":"output_text", "text":text}]})
    }

    fn call_item() -> Value {
        json!({"type":"function_call", "id":"fc_1", "call_id":"call_1", "name":"search",
            "arguments":"{\"query\":\"rust\"}", "status":"completed"})
    }

    fn reasoning_item() -> Value {
        json!({"type":"reasoning", "id":"rs_1", "encrypted_content":"secret",
            "summary":[{"type":"summary_text", "text":"first"}, {"type":"summary_text", "text":"second"}]})
    }

    fn image() -> ImageReference {
        ImageReference {
            sha256: "hash".into(),
            media_type: "image/png".into(),
            name: "image.png".into(),
            bytes: 1,
            data_base64: Some("YQ==".into()),
        }
    }

    #[test]
    fn request_transmits_native_tools_schema_reasoning_and_cache_key() {
        let mut req = request("gpt-5");
        req.system = vec![SystemSegment {
            text: "system".into(),
            cache: false,
        }];
        req.tools = vec![ToolDefinition {
            name: "search".into(),
            description: "Search".into(),
            input_schema: json!({"type":"object"}),
        }];
        req.response_schema = Some(ResponseSchema {
            name: "answer".into(),
            schema: json!({"type":"object", "properties":{}, "additionalProperties":false}),
        });
        req.reasoning = Some("high".into());
        req.correlation = Some("session".into());
        req.messages = vec![
            Message::User(vec![
                UserContent::Text {
                    text: "look".into(),
                },
                UserContent::Image { image: image() },
            ]),
            Message::Tool(vec![ToolResult {
                call_id: "call_1".into(),
                name: "search".into(),
                result: json!({"answer":42}),
                images: vec![image()],
                is_error: true,
            }]),
        ];
        let body = encode(&req).unwrap();
        assert_eq!(body["instructions"], "system");
        assert_eq!(body["tools"][0]["name"], "search");
        assert_eq!(
            body["text"]["format"]["schema"],
            req.response_schema.unwrap().schema
        );
        assert_eq!(
            body["reasoning"],
            json!({"effort":"high", "summary":"auto"})
        );
        assert_eq!(body["prompt_cache_key"], "session");
        assert_eq!(body["input"][0]["content"][0]["text"], "look");
        assert_eq!(
            body["input"][0]["content"][1]["image_url"],
            "data:image/png;base64,YQ=="
        );
        assert_eq!(body["input"][1]["call_id"], "call_1");
        let result: Value =
            serde_json::from_str(body["input"][1]["output"].as_str().unwrap()).unwrap();
        assert_eq!(result, json!({"result":{"answer":42},"is_error":true}));
        assert_eq!(
            body["input"][2]["content"][1]["image_url"],
            "data:image/png;base64,YQ=="
        );
    }

    #[test]
    fn reasoning_from_other_providers_or_models_is_not_replayed() {
        for (provider, model) in [("anthropic", "gpt-5"), ("responses", "other-model")] {
            let mut req = request("gpt-5");
            req.messages = vec![Message::Assistant(vec![AssistantItem::reasoning(
                "r",
                0,
                "private",
                Some(reasoning_envelope(provider, model, reasoning_item())),
            )])];
            assert_eq!(encode(&req).unwrap()["input"], json!([]));
        }
    }

    #[tokio::test]
    async fn encrypted_reasoning_and_tools_survive_save_resume_and_scope_changes() {
        use crate::provider::backends::common::{
            bind_reasoning_scope, filter_reasoning_scope, reasoning_scope, tests::resume_request,
        };
        let mut req = request("gpt-5");
        let scope = reasoning_scope("openai", "https://api.example/v1/responses");
        // No display summary is required for native reasoning to be replayable.
        let native = json!({"type":"reasoning", "id":"rs_opaque", "summary":[],
            "encrypted_content":"opaque+/=", "future_state":{"signature":"unchanged"},
            "content":[{"type":"reasoning_text", "text":"native reasoning text"}]});
        let mut assembler = crate::provider::protocol::ResponseAssembler::default();
        for mut chunk in Decoder::new(req.model.clone())
            .feed(completed(vec![native.clone(), call_item()]))
            .unwrap()
        {
            bind_reasoning_scope(&mut chunk, &scope);
            assembler.push(&chunk).unwrap();
        }
        let (items, _, reason) = assembler.finish().unwrap();
        assert_eq!(reason, StopReason::ToolUse);
        assert_eq!(items[0].blocks.len(), 1);
        req.messages = vec![
            Message::Assistant(items),
            Message::Tool(vec![ToolResult {
                call_id: "call_1".into(),
                name: "search".into(),
                result: json!({"found":true}),
                images: vec![],
                is_error: false,
            }]),
        ];
        let original = resume_request(&req).await;
        let mut matching = original.clone();
        filter_reasoning_scope(&mut matching, &scope);
        let body = encode(&matching).unwrap();
        assert_eq!(body["input"][0], native);
        assert_eq!(body["input"][1]["type"], "function_call");
        assert_eq!(body["input"][2]["type"], "function_call_output");
        assert_eq!(body["input"][1]["call_id"], body["input"][2]["call_id"]);

        for foreign_scope in [
            reasoning_scope("other-provider", "https://api.example/v1/responses"),
            reasoning_scope("openai", "https://other.example/v1/responses"),
        ] {
            let mut foreign = original.clone();
            filter_reasoning_scope(&mut foreign, &foreign_scope);
            let Message::Assistant(items) = &foreign.messages[0] else {
                unreachable!()
            };
            assert_eq!(items.len(), 2);
            assert_eq!(items[0].blocks.len(), 1);
            assert!(items[0].replay.is_none());
            assert_eq!(
                encode(&foreign).unwrap()["input"],
                json!([body["input"][1].clone(), body["input"][2].clone()])
            );
        }
        // Filtering a call-time clone must never destroy resumable journal state.
        assert_eq!(encode(&original).unwrap()["input"][0], native);
    }

    #[test]
    fn reasoning_summary_and_effort_options() {
        let mut req = request("gpt-5");
        for effort in [
            None,
            Some("none"),
            Some("minimal"),
            Some("low"),
            Some("medium"),
            Some("high"),
            Some("xhigh"),
            Some("max"),
        ] {
            req.reasoning = effort.map(str::to_owned);
            let body = encode(&req).unwrap();
            assert_eq!(body["reasoning"]["summary"], "auto");
            assert_eq!(
                body["reasoning"].get("effort"),
                effort.map(|e| json!(e)).as_ref()
            );
            assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        }
        req.reasoning = Some("invalid".into());
        assert!(encode(&req).is_err());
    }

    #[test]
    fn terminal_only_output_replays_across_tool_and_followup_turns() {
        let mut native_reasoning = reasoning_item();
        native_reasoning["encrypted_content"] = json!("mock-replay-state");
        let items = assemble(vec![
            native_reasoning.clone(),
            text_item("text-terminal", "checking"),
            call_item(),
        ]);
        let mut req = request("gpt-5");
        req.messages = vec![
            Message::User(vec![UserContent::Text {
                text: "look it up".into(),
            }]),
            Message::Assistant(items),
            Message::Tool(vec![ToolResult {
                call_id: "call_1".into(),
                name: "search".into(),
                result: json!({"value":42}),
                images: vec![],
                is_error: false,
            }]),
        ];
        let encoded = encode(&req).unwrap();
        let input = encoded["input"].as_array().unwrap();
        assert_eq!(
            input
                .iter()
                .filter(|item| item["type"] == "reasoning")
                .count(),
            1
        );
        assert_eq!(input[1], native_reasoning);
        assert_eq!(
            input[2]["content"][0],
            json!({"type":"output_text", "text":"checking"})
        );
        assert_eq!(
            input[3],
            json!({"type":"function_call", "call_id":"call_1", "name":"search", "arguments":"{\"query\":\"rust\"}"})
        );
        assert_eq!(input[4]["type"], "function_call_output");
        assert_eq!(input[4]["call_id"], "call_1");

        let followup = assemble(vec![text_item("answer", "42")]);
        req.messages.push(Message::Assistant(followup));
        req.messages.push(Message::User(vec![UserContent::Text {
            text: "thanks".into(),
        }]));
        let replayed = encode(&req).unwrap();
        let replayed = replayed["input"].as_array().unwrap();
        assert_eq!(&replayed[..input.len()], input.as_slice());
        assert_eq!(replayed[5]["content"][0]["text"], "42");
        assert_eq!(replayed[6]["content"][0]["text"], "thanks");
    }
}
