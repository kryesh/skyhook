//! Encode canonical history and provider-bound reasoning replay into Chat requests.
use super::{
    dialect::{Cache, Dialect, EmptyContent, ReasoningReplay, SystemRole, UsageRequest},
    schema::validate_schema,
};
use crate::provider::{
    ProviderError,
    codec::{
        CodecName, SchemaConstraint, ToolNames,
        common::{
            attach_runtime_tail, check_tool, image_url, invalid, own_replay, signed_context,
            system_text, tool_text, user_parts,
        },
        placement::breakpoint,
    },
    protocol::{AssistantItem, Binding, HistoryLifetime, Message, ModelRequest, Replay},
};
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;

/// The body fields this codec writes, routing included.
pub(crate) const BODY_FIELDS: &[&str] = &[
    "model",
    "messages",
    "stream",
    "stream_options",
    "tools",
    "response_format",
    "provider",
    "models",
];

/// The assistant message fields this codec writes.
pub(crate) const ASSISTANT_FIELDS: &[&str] = &["role", "content", "tool_calls"];

pub(crate) fn encode(
    request: &ModelRequest,
    dialect: &Dialect,
) -> Result<Map<String, Value>, ProviderError> {
    // History precedes the per-request tail so the tail never breaks a cached
    // history prefix. Runtime state joins the final history turn, so only that
    // turn is re-read.
    let mut messages = Vec::new();
    // One leading system message: many open-model chat templates reject
    // repeated or non-leading system turns.
    if let Some(text) = system_text(request) {
        let role = match dialect.system {
            SystemRole::System => "system",
            SystemRole::Developer => "developer",
        };
        let content = match dialect.cache {
            Cache::ContentPartBreakpoints { ttl }
                if request.system.iter().any(|segment| segment.cache) =>
            {
                let parts = request.system.iter().map(|segment| {
                    let mut part = json!({"type": "text", "text": segment.text});
                    if segment.cache {
                        part["cache_control"] = breakpoint(ttl);
                    }
                    part
                });
                Value::Array(parts.collect())
            }
            _ => Value::String(text),
        };
        messages.push(json!({"role": role, "content": content}));
    }
    let signed_only = signed_context(request, dialect.reasoning_replay.format());
    for message in &request.history {
        push_message(request, dialect, signed_only, &mut messages, message)?;
    }
    // History is an unchanged prefix of later requests; detached history never is.
    if let Cache::ContentPartBreakpoints { ttl } = dialect.cache
        && request.history_lifetime != HistoryLifetime::Detached
    {
        mark_last_cacheable(&mut messages, breakpoint(ttl));
    }
    for message in &request.tail {
        if attach_runtime_tail(&mut messages, message, "text", |item| {
            (item["role"] == "tool").then(|| &mut item["content"])
        }) {
            continue;
        }
        push_message(request, dialect, signed_only, &mut messages, message)?;
    }
    flatten_text_content(&mut messages);
    let mut root = Map::new();
    root.insert("model".into(), json!(request.model));
    root.insert("messages".into(), Value::Array(messages));
    root.insert("stream".into(), json!(true));
    match dialect.usage_request {
        UsageRequest::StreamOptions => {
            root.insert("stream_options".into(), json!({"include_usage": true}));
        }
        UsageRequest::Implicit => {}
    }
    if let Some(effort) = &request.reasoning {
        dialect.effort.place(&mut root, effort)?;
    }
    if let Some(limit) = request.max_output_tokens {
        if limit == 0 {
            return Err(invalid("max_output_tokens must be positive"));
        }
        if let Some(path) = &dialect.output_limit {
            path.set(&mut root, json!(limit))?;
        }
    }
    if let Some(path) = &dialect.tool_stream {
        path.set(&mut root, json!(true))?;
    }
    if !request.tools.is_empty() {
        let mut names = BTreeSet::new();
        let mut tools = Vec::new();
        for tool in &request.tools {
            check_tool(tool, dialect.tool_names, CodecName::ChatCompletions)?;
            if !names.insert(&tool.name) {
                return Err(invalid("Chat tools require unique function names"));
            }
            let mut function = json!({"name": tool.name, "parameters": tool.input_schema});
            if !tool.description.is_empty() {
                function["description"] = json!(tool.description);
            }
            tools.push(json!({"type": "function", "function": function}));
        }
        root.insert("tools".into(), Value::Array(tools));
    }
    if let Some(schema) = &request.response_schema {
        let format = match dialect.schema {
            SchemaConstraint::OpenAiStrict => {
                if !ToolNames::OpenAi.accepts(&schema.name) {
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
                json!({"name": schema.name, "strict": true, "schema": schema.schema})
            }
            // The server compiles the whole schema into a grammar; nothing is dropped.
            SchemaConstraint::Grammar => {
                if schema.name.trim().is_empty() || !schema.schema.is_object() {
                    return Err(invalid(
                        "Chat response schema requires a name and a JSON Schema object",
                    ));
                }
                json!({"name": schema.name, "schema": schema.schema})
            }
        };
        root.insert(
            "response_format".into(),
            json!({"type": "json_schema", "json_schema": format}),
        );
    }
    if let Some(routing) = &dialect.routing {
        let mut provider = routing.provider.clone();
        if request.response_schema.is_some() {
            provider.require_parameters = Some(true);
        }
        if provider != Default::default() {
            let provider = serde_json::to_value(provider).expect("preferences serialize");
            root.insert("provider".into(), provider);
        }
        if !routing.fallback_models.is_empty() {
            root.insert("models".into(), json!(routing.fallback_models));
        }
    }
    Ok(root)
}

fn push_message(
    request: &ModelRequest,
    dialect: &Dialect,
    signed_only: bool,
    messages: &mut Vec<Value>,
    message: &Message,
) -> Result<(), ProviderError> {
    match message {
        Message::User(parts) => {
            let content = user_parts(request, parts, "text", |image| {
                Ok(json!({"type": "image_url", "image_url": {"url": image_url(request, image)?}}))
            })?;
            messages.push(json!({"role": "user", "content": content}));
        }
        Message::Assistant(parts) => {
            let mut text = String::new();
            let mut calls = Vec::new();
            // Only the originating replay is sent. Visible reasoning (including
            // foreign summaries) is not provenance.
            let mut replays = Vec::new();
            for item in parts {
                match item {
                    AssistantItem::Text { blocks, .. } => {
                        blocks.iter().for_each(|block| text.push_str(&block.text));
                    }
                    AssistantItem::Reasoning { replay, .. } => replays.extend(own_replay(
                        replay.as_ref(),
                        dialect.reasoning_replay.format(),
                        &request.model,
                    )),
                    AssistantItem::ToolCall { call, .. } => {
                        // Results pair by call ID, so sanitizing an invalid name is safe.
                        calls.push(json!({
                            "id": call.id(), "type": "function",
                            "function": {"name": wire_name(call.name(), dialect.tool_names), "arguments": serde_json::to_string(call.arguments()).expect("JSON object serialization cannot fail")}
                        }));
                    }
                }
            }
            let content = match (text.is_empty(), dialect.empty_content) {
                (true, EmptyContent::Null) => Value::Null,
                _ => Value::String(text),
            };
            let mut message = Map::new();
            message.insert("role".into(), json!("assistant"));
            message.insert("content".into(), content);
            if !calls.is_empty() {
                message.insert("tool_calls".into(), Value::Array(calls));
            }
            // Once the context is signed, thinking blocks go back one by one if
            // signed, and a detail sequence goes back whole if any detail is.
            let signed = |replay: &&Replay| replay.binding == Binding::Conversation;
            let payloads = |replays: Vec<&Replay>| -> Vec<Value> {
                replays
                    .into_iter()
                    .map(|replay| replay.payload.clone())
                    .collect()
            };
            let reasoning = match &dialect.reasoning_replay {
                ReasoningReplay::Unsupported => None,
                ReasoningReplay::Text(field) => {
                    let text: String = replays
                        .iter()
                        .filter_map(|replay| replay.payload.get("text").and_then(Value::as_str))
                        .collect();
                    (!text.is_empty()).then(|| (field, Value::String(text)))
                }
                ReasoningReplay::ThinkingBlocks(field) => {
                    replays.retain(|replay| !signed_only || signed(replay));
                    (!replays.is_empty()).then(|| (field, Value::Array(payloads(replays))))
                }
                ReasoningReplay::Details(field) => {
                    let whole = !signed_only || replays.iter().any(signed);
                    (whole && !replays.is_empty()).then(|| (field, Value::Array(payloads(replays))))
                }
            };
            if let Some((field, reasoning)) = reasoning {
                field.set(&mut message, reasoning)?;
            }
            messages.push(Value::Object(message));
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
    Ok(())
}

/// Mark the last nonempty text of history as a cache breakpoint, in parts form.
fn mark_last_cacheable(messages: &mut [Value], breakpoint: Value) {
    for message in messages.iter_mut().rev() {
        if let Some(text) = message["content"].as_str() {
            if text.is_empty() {
                continue;
            }
            let part = json!({"type": "text", "text": text, "cache_control": breakpoint});
            message["content"] = Value::Array(vec![part]);
            return;
        }
        let parts = message["content"].as_array_mut().into_iter().flatten();
        if let Some(part) = parts
            .rev()
            .find(|part| part["type"] == "text" && part["text"] != "")
        {
            part["cache_control"] = breakpoint;
            return;
        }
    }
}

/// A historical tool-call name within the endpoint's function-name alphabet:
/// invalid characters become `_` and the name is capped at 64 bytes.
fn wire_name(name: &str, names: ToolNames) -> String {
    if names.accepts(name) {
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

/// Text-only content is sent as a plain string, which every compatible server
/// and chat template accepts; image turns and cache breakpoints keep the parts array.
fn flatten_text_content(messages: &mut [Value]) {
    for message in messages {
        let Some(parts) = message["content"].as_array() else {
            continue;
        };
        if parts
            .iter()
            .all(|part| part["type"] == "text" && part.get("cache_control").is_none())
        {
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
    /// A server that rejects unknown assistant keys, like the official API.
    fn unsupported() -> Dialect {
        Dialect {
            reasoning_replay: ReasoningReplay::Unsupported,
            ..Dialect::compatible()
        }
    }
    /// A server spelling the replay field `reasoning`.
    fn reasoning_field() -> Dialect {
        Dialect {
            reasoning_replay: ReasoningReplay::Text(crate::provider::codec::path("reasoning")),
            ..Dialect::compatible()
        }
    }
    use super::super::{
        Decoder,
        decoder::tests::{delta, end},
        dialect::{ProviderPreferences, Routing},
    };
    use super::*;
    use crate::provider::codec::common::tests::{envelope, image, notes, request};
    use crate::{
        media::AttachmentRef,
        provider::protocol::{
            AssistantItem, Binding, Replay, ReplayFormat, ResponseSchema, SystemSegment, ToolCall,
            ToolDefinition, ToolResult, UserContent,
        },
    };

    #[test]
    fn dialect_places_each_selected_convention() {
        use crate::provider::codec::{CacheKey, Codec, Identity, path};
        let mut request = request("model");
        request.system = vec![SystemSegment {
            text: "system".into(),
            cache: false,
        }];
        request.reasoning = Some("high".into());
        request.response_schema = Some(ResponseSchema {
            name: "answer".into(),
            schema: json!({"anyOf":[{"type":"object"}, {"type":"string"}]}),
        });
        let developer = Dialect {
            system: SystemRole::Developer,
            output_limit: None,
            tool_stream: Some(path("tool_stream")),
            identity: Identity {
                cache_key: CacheKey::Body(path("prompt_cache_key")),
                user_id: Some(path("metadata.user_id")),
            },
            ..Dialect::compatible()
        };
        let context = "session".parse().unwrap();
        let encoded = Codec::ChatCompletions(developer)
            .encode(&request, &context)
            .unwrap();
        let body = &encoded.body;
        assert_eq!(body["messages"][0]["role"], "developer");
        assert!(body.get("max_completion_tokens").is_none());
        assert_eq!(body["tool_stream"], true);
        assert_eq!(body["prompt_cache_key"], "session");
        assert_eq!(body["metadata"]["user_id"], "session");
        assert!(encoded.headers.is_empty());
        // A grammar server takes the schema as written, without the strict subset.
        let format = &body["response_format"]["json_schema"];
        assert_eq!(
            format["schema"],
            request.response_schema.as_ref().unwrap().schema
        );
        assert!(format.get("strict").is_none());
        let strict = Dialect {
            schema: SchemaConstraint::OpenAiStrict,
            empty_content: EmptyContent::Null,
            ..Dialect::compatible()
        };
        assert!(encode(&request, &strict).is_err());
        request.response_schema.as_mut().unwrap().schema = json!({
            "type":"object", "properties":{"ok":{"type":"boolean"}},
            "required":["ok"], "additionalProperties":false
        });
        let body = encode(&request, &strict).unwrap();
        assert_eq!(body["response_format"]["json_schema"]["strict"], true);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["max_completion_tokens"], 8192);
        // A tool-call turn without text spells its absence as the dialect does.
        let turn = history(vec![AssistantItem::tool_call(
            "c",
            0,
            inspect("call", json!({})),
        )]);
        assert_eq!(
            encode(&turn, &Dialect::compatible()).unwrap()["messages"][0]["content"],
            ""
        );
        assert_eq!(
            encode(&turn, &strict).unwrap()["messages"][0]["content"],
            Value::Null
        );
    }

    #[test]
    fn thinking_blocks_replay_and_cache_breakpoints_take_the_parts_form() {
        use crate::provider::codec::path;
        use crate::provider::protocol::HistoryLifetime;
        let claude = Dialect {
            reasoning_replay: ReasoningReplay::ThinkingBlocks(path("thinking_blocks")),
            cache: Cache::ContentPartBreakpoints { ttl: None },
            ..Dialect::compatible()
        };
        let block = json!({"type":"thinking","thinking":"private","signature":"sig"});
        let signed = envelope(
            ReplayFormat::ChatThinkingBlock,
            "test-model",
            block.clone(),
            Binding::Conversation,
        );
        let mut request = request("test-model");
        request.system = vec![
            SystemSegment {
                text: "stable".into(),
                cache: true,
            },
            SystemSegment {
                text: "volatile".into(),
                cache: false,
            },
        ];
        request.history.extend([
            Message::Assistant(vec![
                AssistantItem::reasoning("r", 0, "summary", Some(signed)),
                AssistantItem::reasoning(
                    "t",
                    1,
                    "text-only",
                    Some(reasoning_envelope(
                        ReplayFormat::ChatText,
                        "test-model",
                        json!({"text":"t"}),
                    )),
                ),
                AssistantItem::tool_call("c", 2, inspect("call", json!({}))),
            ]),
            Message::Tool(vec![ToolResult {
                call_id: "call".into(),
                name: "inspect".into(),
                result: json!("ok"),
                is_error: false,
                images: vec![],
            }]),
        ]);
        request.tail = vec![Message::User(vec![UserContent::Runtime {
            text: "<state>".into(),
        }])];
        let body = encode(&request, &claude).unwrap();
        let messages = body["messages"].as_array().unwrap();
        // Marked system segments keep their parts; the text-only replay is foreign here.
        assert_eq!(
            messages[0]["content"],
            json!([
                {"type":"text","text":"stable","cache_control":{"type":"ephemeral"}},
                {"type":"text","text":"volatile"}
            ])
        );
        assert_eq!(messages[1]["content"], "hello");
        assert_eq!(messages[2]["thinking_blocks"], json!([block]));
        assert!(messages[2].get("reasoning_content").is_none());
        // The last history text (the tool result) takes the breakpoint, and the
        // runtime tail joins it after the marked part.
        let parts = messages[3]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["cache_control"], json!({"type":"ephemeral"}));
        assert!(
            parts[0]["text"]
                .as_str()
                .unwrap()
                .contains("\"result\":\"ok\"")
        );
        assert_eq!(parts[1], json!({"type":"text","text":"<state>"}));
        assert_eq!(messages.len(), 4);
        request.history_lifetime = HistoryLifetime::Detached;
        let detached = encode(&request, &claude).unwrap();
        assert!(detached["messages"][3]["content"].is_string());
        assert!(detached["messages"][0]["content"].is_array());
        // Without a cache convention, the same request flattens to strings, and a
        // text-replay dialect leaves signed blocks out.
        let plain = encode(&request, &Dialect::compatible()).unwrap();
        assert_eq!(plain["messages"][0]["content"], "stable\n\nvolatile");
        assert!(plain["messages"][2].get("thinking_blocks").is_none());
        assert_eq!(plain["messages"][2]["reasoning_content"], "t");
    }

    #[test]
    fn a_signed_context_filters_thinking_blocks_singly_and_detail_sequences_by_turn() {
        use crate::provider::codec::path;
        let (free, bound) = (Binding::Free, Binding::Conversation);
        let turn = |format, natives: &[(Value, Binding)], said: &str| {
            let mut items: Vec<_> = natives
                .iter()
                .enumerate()
                .map(|(index, (payload, binding))| {
                    let replay = envelope(format, "test-model", payload.clone(), *binding);
                    AssistantItem::reasoning(index.to_string(), index as u32, "", Some(replay))
                })
                .collect();
            let next = items.len() as u32;
            items.push(AssistantItem::text("t", next, said));
            items.push(AssistantItem::tool_call(
                "c",
                next + 1,
                inspect(said, json!({})),
            ));
            Message::Assistant(items)
        };
        let encode_turns = |dialect: &Dialect, turns: Vec<Message>| {
            let mut request = request("test-model");
            for turn in turns {
                request.history.extend([
                    turn,
                    Message::User(vec![UserContent::Text {
                        text: "next".into(),
                    }]),
                ]);
            }
            let body = encode(&request, dialect).unwrap();
            let assistant: Vec<_> = body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|message| message["role"] == "assistant")
                .cloned()
                .collect();
            for (message, said) in assistant.iter().zip(["one", "two"]) {
                assert_eq!(message["content"], said);
                assert_eq!(message["tool_calls"][0]["id"], said);
            }
            assistant
        };
        // Details: an unsigned conversation replays every sequence; once one is
        // signed, a turn with a signed detail replays its sequence whole and in
        // order, and a turn without one keeps only its text and calls.
        let router = Dialect {
            reasoning_replay: ReasoningReplay::Details(path("reasoning_details")),
            ..Dialect::compatible()
        };
        let summary = json!({"type":"reasoning.summary","summary":"Sum","index":0});
        let encrypted = json!({"type":"reasoning.encrypted","data":"opaque","index":1});
        let detail = ReplayFormat::ChatReasoningDetail;
        let unsigned = [(summary.clone(), free)];
        let turns = encode_turns(
            &router,
            vec![
                turn(detail, &unsigned, "one"),
                turn(detail, &unsigned, "two"),
            ],
        );
        for message in &turns {
            assert_eq!(message["reasoning_details"], json!([summary]));
        }
        let mixed = [(summary.clone(), free), (encrypted.clone(), bound)];
        let turns = encode_turns(
            &router,
            vec![turn(detail, &unsigned, "one"), turn(detail, &mixed, "two")],
        );
        assert!(turns[0].get("reasoning_details").is_none());
        assert_eq!(turns[1]["reasoning_details"], json!([summary, encrypted]));
        // Thinking blocks are filtered one by one.
        let claude = Dialect {
            reasoning_replay: ReasoningReplay::ThinkingBlocks(path("thinking_blocks")),
            ..Dialect::compatible()
        };
        let open = json!({"type":"thinking","thinking":"open"});
        let sealed = json!({"type":"thinking","thinking":"sealed","signature":"sig"});
        let block = ReplayFormat::ChatThinkingBlock;
        let turns = encode_turns(&claude, vec![turn(block, &[(open.clone(), free)], "one")]);
        assert_eq!(turns[0]["thinking_blocks"], json!([open]));
        let blocks = [(open.clone(), free), (sealed.clone(), bound)];
        let turns = encode_turns(
            &claude,
            vec![
                turn(block, &[(open, free)], "one"),
                turn(block, &blocks, "two"),
            ],
        );
        assert!(turns[0].get("thinking_blocks").is_none());
        assert_eq!(turns[1]["thinking_blocks"], json!([sealed]));
    }

    #[test]
    fn router_conventions_place_reasoning_object_details_ttl_and_routing() {
        use crate::provider::codec::path;
        let routing = Routing {
            provider: ProviderPreferences {
                order: vec!["anthropic".into()],
                zdr: Some(true),
                ..Default::default()
            },
            fallback_models: vec!["x/y".into()],
        };
        let router = Dialect {
            effort: crate::provider::codec::Effort {
                path: path("reasoning.effort"),
                levels: crate::provider::codec::OPENAI_EFFORT,
            },
            reasoning_replay: ReasoningReplay::Details(path("reasoning_details")),
            usage_request: UsageRequest::Implicit,
            cache: Cache::ContentPartBreakpoints {
                ttl: Some(crate::provider::codec::CacheTtl::OneHour),
            },
            routing: Some(routing),
            ..Dialect::compatible()
        };
        let details = [
            json!({"type":"reasoning.text","text":"t","signature":"s","index":0}),
            json!({"type":"reasoning.encrypted","data":"d","index":1}),
        ];
        let mut request = request("test-model");
        request.reasoning = Some("high".into());
        request.system = vec![SystemSegment {
            text: "stable".into(),
            cache: true,
        }];
        request.history.push(Message::Assistant(
            details
                .iter()
                .enumerate()
                .map(|(index, detail)| {
                    let replay = envelope(
                        ReplayFormat::ChatReasoningDetail,
                        "test-model",
                        detail.clone(),
                        Binding::Conversation,
                    );
                    AssistantItem::reasoning(index.to_string(), index as u32, "", Some(replay))
                })
                .collect(),
        ));
        let body = encode(&request, &router).unwrap();
        assert_eq!(body["reasoning"], json!({"effort":"high"}));
        assert!(body.get("reasoning_effort").is_none());
        assert!(body.get("stream_options").is_none());
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"],
            json!({"type":"ephemeral","ttl":"1h"})
        );
        // Details replay verbatim, in order, on their turn; the last history text
        // takes the breakpoint.
        assert_eq!(body["messages"][2]["reasoning_details"], json!(details));
        assert_eq!(
            body["messages"][1]["content"][0]["cache_control"],
            json!({"type":"ephemeral","ttl":"1h"})
        );
        assert_eq!(body["provider"], json!({"order":["anthropic"],"zdr":true}));
        assert_eq!(body["models"], json!(["x/y"]));
        // A schema forces parameter support on the routed endpoint.
        request.response_schema = Some(ResponseSchema {
            name: "answer".into(),
            schema: json!({"type":"object","properties":{},"required":[],"additionalProperties":false}),
        });
        let body = encode(&request, &router).unwrap();
        assert_eq!(body["provider"]["require_parameters"], true);
    }

    fn reasoning_envelope(format: ReplayFormat, model: &str, payload: serde_json::Value) -> Replay {
        envelope(format, model, payload, Binding::Free)
    }

    fn history(items: Vec<AssistantItem>) -> ModelRequest {
        ModelRequest {
            history: vec![Message::Assistant(items)],
            ..request("test-model")
        }
    }

    fn inspect(id: &str, arguments: Value) -> ToolCall {
        ToolCall::new(id, "inspect", arguments).unwrap()
    }

    #[test]
    fn request_preserves_text_tools_images_and_reasoning_settings() {
        let notes = notes();
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
        assert!(encode(&request, &unsupported()).is_err());
        // Load the fixture blobs as the session store would.
        request.blobs.insert(image().blob, b"\x01\x02\x03".to_vec());
        request.blobs.insert(notes.blob, b"notes".to_vec());
        let body = encode(&request, &unsupported()).unwrap();
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
        assert!(body.get("n").is_none());
        // Text-only turns and system segments go as plain strings; an empty tool
        // description is left out.
        let mut minimal = crate::provider::codec::common::tests::request("model");
        minimal.system = ["one", "two"]
            .map(|text| SystemSegment {
                text: text.into(),
                cache: false,
            })
            .to_vec();
        minimal.tools = vec![ToolDefinition {
            name: "inspect".into(),
            description: String::new(),
            input_schema: json!({"type":"object"}),
        }];
        let body = encode(&minimal, &unsupported()).unwrap();
        assert_eq!(
            body["messages"][0],
            json!({"role":"system","content":"one\n\ntwo"})
        );
        assert_eq!(
            body["messages"][1],
            json!({"role":"user","content":"hello"})
        );
        assert!(body["tools"][0]["function"].get("description").is_none());
        for (effort, valid) in [("max", true), ("unbounded", false)] {
            let mut request = history(vec![]);
            request.reasoning = Some(effort.into());
            let body = encode(&request, &unsupported());
            assert_eq!(
                body.ok().map(|body| body["reasoning_effort"].clone()),
                valid.then(|| json!(effort))
            );
        }
        // Historical call names are sent as recorded where any name is accepted, and
        // sanitized into the endpoint's alphabet where one is enforced; results pair by id.
        let strict = Dialect {
            tool_names: ToolNames::OpenAi,
            ..unsupported()
        };
        let long = "x".repeat(80);
        for (name, expected) in [
            ("vendor.tool/雪", "vendor_tool__".to_owned()),
            (long.as_str(), "x".repeat(64)),
        ] {
            let call = ToolCall::new("call", name, json!({"x-vendor": [null, 1]})).unwrap();
            let request = history(vec![AssistantItem::tool_call("item", 0, call)]);
            for (dialect, sent) in [(&unsupported(), name), (&strict, expected.as_str())] {
                let body = encode(&request, dialect).unwrap();
                let call = &body["messages"][0]["tool_calls"][0];
                assert_eq!(
                    (&call["id"], &call["function"]["name"]),
                    (&json!("call"), &json!(sent))
                );
            }
        }
    }

    #[test]
    fn reasoning_replay_roundtrip_is_policy_selected() {
        let scope = crate::provider::codec::common::tests::scope();
        let mut decoder = Decoder::new(
            "test-model".into(),
            scope.clone(),
            Dialect::compatible().reasoning_replay.format(),
            crate::provider::http::errors::ErrorSignals::NONE,
        );
        let frames = [
            delta(json!({"reasoning_content":"first "})),
            delta(json!({"reasoning":"second"})),
            delta(json!({"content":"ok"})),
            end("stop"),
        ];
        let mut events = Vec::new();
        for frame in frames {
            events.extend(decoder.decode(&frame).unwrap());
        }
        events.extend(decoder.finish().unwrap());
        let items = crate::provider::codec::common::tests::reduce(events)
            .items()
            .to_vec();
        let envelope = items[0].replay().unwrap();
        assert_eq!(
            (
                envelope.provenance.format,
                &*envelope.provenance.model,
                &envelope.provenance.scope
            ),
            (ReplayFormat::ChatText, "test-model", &scope)
        );
        assert_eq!(envelope.payload, json!({"text":"first second"}));
        let original = history(items);
        for (dialect, field, absent) in [
            (Dialect::compatible(), "reasoning_content", "reasoning"),
            (reasoning_field(), "reasoning", "reasoning_content"),
        ] {
            let body = encode(&original, &dialect).unwrap();
            assert_eq!(body["messages"].as_array().unwrap().len(), 1);
            let message = &body["messages"][0];
            assert_eq!(message["content"], "ok");
            assert_eq!(message[field], "first second");
            assert!(message.get(absent).is_none());
            assert!(body.get("n").is_none());
        }
        let unsupported = encode(&original, &unsupported()).unwrap();
        assert_eq!(
            unsupported["messages"],
            json!([{"role":"assistant","content":"ok"}])
        );
        let mutations: [fn(&mut Replay); 3] = [
            |envelope| envelope.provenance.model = "different-model".into(),
            |envelope| envelope.provenance.format = ReplayFormat::Responses,
            |envelope| envelope.payload = json!({"text":42}),
        ];
        for mutation in mutations.map(Some).into_iter().chain([None]) {
            let mut request = original.clone();
            let Message::Assistant(items) = &mut request.history[0] else {
                unreachable!()
            };
            let AssistantItem::Reasoning { replay, .. } = &mut items[0] else {
                unreachable!()
            };
            match mutation {
                Some(mutate) => mutate(replay.as_mut().unwrap()),
                None => *replay = None,
            }
            let body = encode(&request, &Dialect::compatible()).unwrap();
            assert_eq!(
                body["messages"],
                json!([{"role":"assistant","content":"ok"}])
            );
        }
    }

    #[test]
    fn replay_uses_payload_not_visible_blocks_and_keeps_text_and_tools() {
        let envelope = |text| {
            reasoning_envelope(
                ReplayFormat::ChatText,
                "test-model",
                json!({ "text": text }),
            )
        };
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
        let body = encode(&req, &reasoning_field()).unwrap();
        assert_eq!(body["messages"][0]["reasoning"], "original private thought");
        assert_eq!(body["messages"][0]["content"], "answer");
        assert_eq!(body["messages"][0]["tool_calls"][0]["id"], "call");
        assert!(!json!(body).to_string().contains("visible summary"));
    }

    #[test]
    fn runtime_tail_joins_the_final_turn_instead_of_posing_as_the_user() {
        let text = "<skyhook_state>";
        let state = || Message::User(vec![UserContent::Runtime { text: text.into() }]);
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
        let without_tail = encode(&req, &unsupported()).unwrap();
        req.tail = vec![state()];
        let body = encode(&req, &unsupported()).unwrap();
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0], without_tail["messages"][0]);
        let tool = without_tail["messages"][1]["content"].as_str().unwrap();
        assert_eq!(messages[1]["content"], format!("{tool}\n\n{text}"));
        // A user turn gains a part; an instruction or an assistant turn keeps its own message.
        let mut req = request("test-model");
        req.tail = vec![state()];
        let body = encode(&req, &unsupported()).unwrap();
        assert_eq!(
            body["messages"],
            json!([{"role":"user","content":format!("hello\n\n{text}")}])
        );
        req.tail.push(Message::User(vec![UserContent::Text {
            text: "compact".into(),
        }]));
        let body = encode(&req, &unsupported()).unwrap();
        assert_eq!(body["messages"].as_array().unwrap().len(), 2);
        let mut req = history(vec![AssistantItem::text("t", 0, "done")]);
        req.tail = vec![state()];
        let body = encode(&req, &unsupported()).unwrap();
        assert_eq!(body["messages"][1]["role"], "user");
    }
}
