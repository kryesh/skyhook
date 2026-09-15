//! Encode conversation history and request options as native Responses input.
use super::*;
use crate::media::AttachmentRef;
use crate::provider::backends::common::{
    attachment_text, image_url, invalid, opaque_payload, tool_text,
};
use crate::provider::protocol::{BlockContent, Message, ModelRequest, UserContent};

/// Backend-private request envelope. Input is always an explicit array, including
/// an intentionally empty conversation; vendor items and settings remain opaque.
/// There is no raw-JSON constructor or deserializer that can omit this history.
#[derive(Clone, Debug)]
pub(crate) struct EncodedRequest {
    pub(crate) input: Vec<Value>,
    pub(crate) settings: serde_json::Map<String, Value>,
}

impl EncodedRequest {
    /// Lower the envelope only when handing a request to an HTTP/WS transport.
    pub(crate) fn into_wire(self) -> Value {
        let mut body = self.settings;
        // Keep the encoder's established key order (model, input, settings)
        // as well as its JSON semantics when preserve_order is enabled.
        body.shift_insert(1.min(body.len()), "input".into(), Value::Array(self.input));
        Value::Object(body)
    }
}

pub(crate) fn encode(request: &ModelRequest) -> Result<EncodedRequest, ProviderError> {
    if request.model.trim().is_empty() {
        return Err(invalid("Responses requires a nonempty model"));
    }
    // Cache hints need no wire field: OpenAI automatically caches matching
    // prefixes, and history precedes the per-request tail so the tail never
    // breaks the cached history prefix.
    let mut input = Vec::new();
    for message in request.messages() {
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
                        UserContent::Attachment { attachment } => match attachment {
                            AttachmentRef::Image(image) => {
                                Ok(json!({"type":"input_image", "image_url":image_url(request, image)?}))
                            }
                            AttachmentRef::Text(text) => {
                                Ok(json!({"type":"input_text", "text":attachment_text(request, text)?}))
                            }
                        },
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
                                != ItemKind::Reasoning
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
                                input.push(json!({"type":"function_call", "call_id":call.id(),
                                    "name":call.name(), "arguments":serde_json::to_string(call.arguments()).expect("JSON object serialization cannot fail")}));
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
                                .push(json!({"type":"input_image", "image_url":image_url(request, image)?}));
                        }
                        input.push(json!({"role":"user", "content":content}));
                    }
                }
            }
        }
    }
    let mut settings = serde_json::Map::from_iter([
        ("model".into(), json!(request.model)),
        ("stream".into(), json!(true)),
        ("store".into(), json!(false)),
        ("include".into(), json!(["reasoning.encrypted_content"])),
        ("reasoning".into(), json!({"summary":"auto"})),
    ]);
    if !request.system.is_empty() {
        settings.insert(
            "instructions".into(),
            Value::String(
                request
                    .system
                    .iter()
                    .map(|s| s.text.as_str())
                    .collect::<Vec<_>>()
                    .join("\n\n"),
            ),
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
        settings.insert("tools".into(), Value::Array(tools));
    }
    if let Some(schema) = &request.response_schema {
        if schema.name.is_empty() || !schema.schema.is_object() {
            return Err(invalid(
                "Responses structured output requires a name and object JSON Schema",
            ));
        }
        settings.insert(
            "text".into(),
            json!({"format":{"type":"json_schema", "name":schema.name,
            "schema":schema.schema, "strict":true}}),
        );
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
        settings.insert(
            "reasoning".into(),
            json!({"summary":"auto", "effort":effort}),
        );
    }
    if let Some(max) = request.max_output_tokens {
        if max == 0 {
            return Err(invalid("Responses max_output_tokens must be positive"));
        }
        settings.insert("max_output_tokens".into(), json!(max));
    }
    if let Some(correlation) = &request.correlation {
        settings.insert("prompt_cache_key".into(), json!(correlation));
    }
    Ok(EncodedRequest { input, settings })
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::{call_item, completed, reasoning_item};
    use super::*;
    use crate::provider::backends::common::tests::request;
    use crate::{
        media::{AttachmentRef, BlobRef, ImageFormat, ImageRef, TextRef},
        provider::protocol::{
            AssistantItem, ResponseAssembler, ResponseSchema, SystemSegment, ToolDefinition,
            ToolResult,
        },
    };

    fn assemble(output: Vec<Value>) -> Vec<AssistantItem> {
        let mut assembler = ResponseAssembler::default();
        for event in Decoder::new("gpt-5".into())
            .feed(completed(output))
            .unwrap()
        {
            assembler.push(&event).unwrap();
        }
        assembler.finish().unwrap().0
    }

    fn text_item(id: &str, text: &str) -> Value {
        json!({"type":"message", "id":id, "role":"assistant",
            "content":[{"type":"output_text", "text":text}]})
    }

    fn image() -> ImageRef {
        ImageRef {
            file: Some("image.png".into()),
            format: ImageFormat::Png,
            blob: BlobRef::of(b"a"),
        }
    }

    fn notes() -> TextRef {
        TextRef {
            file: Some("notes.txt".into()),
            blob: BlobRef::of(b"notes"),
        }
    }

    fn user(text: &str) -> Message {
        Message::User(vec![UserContent::Text { text: text.into() }])
    }

    fn tool_result(result: Value, images: Vec<ImageRef>, is_error: bool) -> Message {
        Message::Tool(vec![ToolResult {
            call_id: "call_1".into(),
            name: "search".into(),
            result,
            images,
            is_error,
        }])
    }

    #[test]
    fn empty_history_is_explicit_and_http_envelope_is_unchanged() {
        let mut req = request("gpt-5");
        req.history.clear();
        req.max_output_tokens = None;
        let encoded = encode(&req).unwrap();
        assert!(encoded.input.is_empty());
        assert!(!encoded.settings.contains_key("input"));
        assert_eq!(
            encoded.into_wire(),
            json!({
                "model":"gpt-5", "input":[], "stream":true, "store":false,
                "include":["reasoning.encrypted_content"],
                "reasoning":{"summary":"auto"}
            })
        );
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
        let schema = json!({"type":"object", "properties":{}, "additionalProperties":false});
        req.response_schema = Some(ResponseSchema {
            name: "answer".into(),
            schema: schema.clone(),
        });
        req.reasoning = Some("high".into());
        req.correlation = Some("session".into());
        let attach = |attachment| UserContent::Attachment { attachment };
        let Message::User(mut content) = user("look") else {
            unreachable!()
        };
        content.extend([
            attach(AttachmentRef::Image(image())),
            attach(AttachmentRef::Text(notes())),
        ]);
        req.history = vec![
            Message::User(content),
            tool_result(json!({"answer":42, "error":null}), vec![image()], true),
        ];
        assert!(encode(&req).is_err());
        // Load the fixture blobs as the session store would.
        req.blobs.insert(image().blob, b"a".to_vec());
        req.blobs.insert(notes().blob, b"notes".to_vec());
        let body = encode(&req).unwrap().into_wire();
        let image_url = "data:image/png;base64,YQ==";
        assert_eq!(body["instructions"], "system");
        assert_eq!(body["tools"][0]["name"], "search");
        assert_eq!(body["text"]["format"]["schema"], schema);
        assert_eq!(
            body["reasoning"],
            json!({"effort":"high", "summary":"auto"})
        );
        assert_eq!(body["prompt_cache_key"], "session");
        let content = &body["input"][0]["content"];
        assert_eq!(content[0]["text"], "look");
        assert_eq!(content[1]["image_url"], image_url);
        assert_eq!(
            content[2],
            json!({"type":"input_text", "text":"File: notes.txt\nnotes"})
        );
        assert_eq!(body["input"][1]["call_id"], "call_1");
        let result: Value =
            serde_json::from_str(body["input"][1]["output"].as_str().unwrap()).unwrap();
        assert_eq!(result, json!({"result":{"answer":42},"is_error":true}));
        assert_eq!(body["input"][2]["content"][1]["image_url"], image_url);
    }

    #[test]
    fn reasoning_from_other_providers_or_models_is_not_replayed() {
        for (provider, model) in [("anthropic", "gpt-5"), ("responses", "other-model")] {
            let mut req = request("gpt-5");
            let envelope = reasoning_envelope(provider, model, reasoning_item());
            let reasoning = AssistantItem::reasoning("r", 0, "private", Some(envelope));
            req.history = vec![Message::Assistant(vec![reasoning])];
            assert_eq!(encode(&req).unwrap().into_wire()["input"], json!([]));
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
        let mut assembler = ResponseAssembler::default();
        let chunks = Decoder::new(req.model.clone())
            .feed(completed(vec![native.clone(), call_item()]))
            .unwrap();
        for mut chunk in chunks {
            bind_reasoning_scope(&mut chunk, &scope);
            assembler.push(&chunk).unwrap();
        }
        let (items, _, reason) = assembler.finish().unwrap();
        assert_eq!((reason, items[0].blocks.len()), (StopReason::ToolUse, 1));
        req.history = vec![
            Message::Assistant(items),
            tool_result(json!({"found":true}), vec![], false),
        ];
        let original = resume_request(&req).await;
        let mut matching = original.clone();
        filter_reasoning_scope(&mut matching, &scope);
        let body = encode(&matching).unwrap().into_wire();
        let input = &body["input"];
        assert_eq!(input[0], native);
        assert_eq!(
            (&input[1]["type"], &input[2]["type"]),
            (&json!("function_call"), &json!("function_call_output"))
        );
        assert_eq!(input[1]["call_id"], input[2]["call_id"]);

        for foreign_scope in [
            reasoning_scope("other-provider", "https://api.example/v1/responses"),
            reasoning_scope("openai", "https://other.example/v1/responses"),
        ] {
            let mut foreign = original.clone();
            filter_reasoning_scope(&mut foreign, &foreign_scope);
            let Message::Assistant(items) = &foreign.history[0] else {
                unreachable!()
            };
            assert_eq!((items.len(), items[0].blocks.len()), (2, 1));
            assert!(items[0].replay.is_none());
            let wire = encode(&foreign).unwrap().into_wire();
            assert_eq!(wire["input"], json!([input[1].clone(), input[2].clone()]));
        }
        // Filtering a call-time clone must never destroy resumable journal state.
        assert_eq!(encode(&original).unwrap().into_wire()["input"][0], native);
    }

    #[test]
    fn reasoning_summary_and_effort_options() {
        let mut req = request("gpt-5");
        let efforts = ["none", "minimal", "low", "medium", "high", "xhigh", "max"];
        for effort in std::iter::once(None).chain(efforts.map(Some)) {
            req.reasoning = effort.map(str::to_owned);
            let body = encode(&req).unwrap().into_wire();
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
        req.history = vec![
            user("look it up"),
            Message::Assistant(items),
            tool_result(json!({"value":42}), vec![], false),
        ];
        let encoded = encode(&req).unwrap().into_wire();
        let input = encoded["input"].as_array().unwrap();
        let reasoning = input.iter().filter(|item| item["type"] == "reasoning");
        assert_eq!(reasoning.count(), 1);
        assert_eq!(input[1], native_reasoning);
        assert_eq!(
            input[2]["content"][0],
            json!({"type":"output_text", "text":"checking"})
        );
        assert_eq!(
            input[3],
            json!({"type":"function_call", "call_id":"call_1", "name":"search", "arguments":"{\"query\":\"rust\"}"})
        );
        assert_eq!(
            (&input[4]["type"], &input[4]["call_id"]),
            (&json!("function_call_output"), &json!("call_1"))
        );

        req.history.push(Message::Assistant(assemble(vec![text_item(
            "answer", "42",
        )])));
        req.history.push(user("thanks"));
        let replayed = encode(&req).unwrap().into_wire();
        let replayed = replayed["input"].as_array().unwrap();
        assert_eq!(&replayed[..input.len()], input.as_slice());
        assert_eq!(replayed[5]["content"][0]["text"], "42");
        assert_eq!(replayed[6]["content"][0]["text"], "thanks");
    }
}
