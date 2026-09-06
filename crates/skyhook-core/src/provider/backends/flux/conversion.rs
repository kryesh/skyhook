use flux_core::{
    Chunk, ContentBlock, Error as FluxError, ImageSource, Message as FluxMessage, Role,
    ToolResultContent,
};
use flux_provider::{Effort, Request, RequestTrace};

use crate::{
    provider::protocol::{
        AssistantContent, Message, ModelRequest, ResponseChunk, ToolCall, Usage, UserContent,
    },
    provider::{ProviderError, ProviderErrorKind},
};

pub(super) fn convert_request(request: ModelRequest) -> Result<Request, ProviderError> {
    let effort = request.reasoning.as_deref().map(parse_effort).transpose()?;
    let messages = request
        .messages
        .into_iter()
        .map(convert_message)
        .collect::<Result<Vec<_>, _>>()?;
    let system_segments = request
        .system
        .into_iter()
        .map(|segment| flux_provider::SystemSegment {
            text: segment.text,
            cache: segment.cache,
        })
        .collect();
    let tools = request
        .tools
        .into_iter()
        .map(|tool| flux_provider::ToolDef {
            name: tool.name,
            description: tool.description,
            input_schema: tool.input_schema,
        })
        .collect();
    // Flux's OpenAI codecs use zero to omit the token-limit field entirely.
    let max_tokens = u32::try_from(request.max_output_tokens.unwrap_or(0)).map_err(|_| {
        ProviderError::protocol("max_output_tokens exceeds the Flux u32 request limit")
    })?;
    let trace = request.correlation.map(|session_id| RequestTrace {
        session_id,
        turn_id: 0,
        stage: "agent".to_owned(),
        round: 0,
    });
    let mut metadata = serde_json::Map::new();
    if let Some(schema) = request.response_schema {
        metadata.insert(
            super::schema::METADATA_KEY.to_owned(),
            serde_json::to_value(schema)
                .map_err(|error| ProviderError::protocol(error.to_string()))?,
        );
    }
    Ok(Request {
        model: request.model,
        system: None,
        system_segments,
        messages,
        tools,
        max_tokens,
        temperature: None,
        top_p: None,
        stop_sequences: Vec::new(),
        thinking: effort.is_some(),
        effort,
        trace,
        metadata,
        cache_tail: true,
    })
}

fn parse_effort(value: &str) -> Result<Effort, ProviderError> {
    match value.to_ascii_lowercase().as_str() {
        "low" => Ok(Effort::Low),
        "medium" => Ok(Effort::Medium),
        "high" => Ok(Effort::High),
        "xhigh" => Ok(Effort::Xhigh),
        "max" => Ok(Effort::Max),
        _ => Err(ProviderError::protocol(format!(
            "unsupported reasoning effort `{value}`"
        ))),
    }
}

fn convert_message(message: Message) -> Result<FluxMessage, ProviderError> {
    match message {
        Message::User(content) => Ok(FluxMessage::user(
            content
                .into_iter()
                .map(convert_user_content)
                .collect::<Result<Vec<_>, _>>()?,
        )),
        Message::Assistant(content) => Ok(FluxMessage::assistant(
            content.into_iter().map(convert_assistant_content).collect(),
        )),
        Message::Tool(results) => {
            let mut blocks = Vec::with_capacity(results.len());
            for result in results {
                let mut content = vec![ToolResultContent::Text {
                    text: serde_json::to_string(&result.result)
                        .map_err(|error| ProviderError::protocol(error.to_string()))?,
                }];
                if !result.console_output.is_empty() {
                    content.push(ToolResultContent::Text {
                        text: format!("Console output:\n{}", result.console_output),
                    });
                }
                for image in result.images {
                    let data = image.data_base64.ok_or_else(|| {
                        ProviderError::protocol(format!(
                            "image blob {} was not hydrated",
                            image.sha256
                        ))
                    })?;
                    content.push(ToolResultContent::Image {
                        source: ImageSource::Base64 {
                            media_type: image.media_type,
                            data,
                        },
                    });
                }
                blocks.push(ContentBlock::ToolResult {
                    tool_use_id: result.call_id,
                    content,
                    is_error: result.is_error,
                });
            }
            Ok(FluxMessage::new(Role::User, blocks))
        }
    }
}

fn convert_user_content(content: UserContent) -> Result<ContentBlock, ProviderError> {
    match content {
        UserContent::Text { text }
        | UserContent::Runtime { text }
        | UserContent::ParentInput { text }
        | UserContent::Compaction { text } => Ok(ContentBlock::Text { text }),
        UserContent::Image { image } => Ok(ContentBlock::Image {
            source: ImageSource::Base64 {
                media_type: image.media_type,
                data: image.data_base64.ok_or_else(|| {
                    ProviderError::protocol(format!("image blob {} was not hydrated", image.sha256))
                })?,
            },
        }),
    }
}

fn convert_assistant_content(content: AssistantContent) -> ContentBlock {
    match content {
        AssistantContent::Text { text } => ContentBlock::Text { text },
        AssistantContent::Reasoning { text, opaque } => {
            if let Some(data) = opaque
                .as_ref()
                .and_then(|value| value.get("redacted"))
                .and_then(serde_json::Value::as_str)
            {
                ContentBlock::RedactedThinking {
                    data: data.to_owned(),
                }
            } else {
                ContentBlock::Thinking {
                    thinking: text,
                    signature: opaque
                        .and_then(|value| {
                            value
                                .get("signature")
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_owned)
                        })
                        .unwrap_or_default(),
                }
            }
        }
        AssistantContent::ToolCall(call) => ContentBlock::ToolUse {
            id: call.id,
            name: call.name,
            input: call.arguments,
        },
    }
}

pub(super) fn convert_chunk(chunk: Chunk) -> Option<ResponseChunk> {
    match chunk {
        Chunk::TextDelta(text) => Some(ResponseChunk::TextDelta { text }),
        Chunk::ThinkingDelta(text) => Some(ResponseChunk::ReasoningDelta { text }),
        Chunk::Block(block) => Some(ResponseChunk::Block {
            block: convert_block(block),
        }),
        Chunk::Usage(usage) => Some(ResponseChunk::Usage {
            usage: Usage {
                input_tokens: usage
                    .input_tokens
                    .saturating_add(usage.cache_creation_input_tokens),
                cached_input_tokens: usage.cache_read_input_tokens,
                output_tokens: usage.output_tokens,
            },
        }),
        Chunk::Done { stop_reason } => Some(ResponseChunk::Finished {
            truncated: stop_reason == Some(flux_core::StopReason::MaxTokens),
        }),
        Chunk::MessageStart { .. }
        | Chunk::ToolInputDelta { .. }
        | Chunk::StreamDiagnostic { .. } => None,
    }
}

fn convert_block(block: ContentBlock) -> AssistantContent {
    match block {
        ContentBlock::Text { text } => AssistantContent::Text { text },
        ContentBlock::Thinking {
            thinking,
            signature,
        } => AssistantContent::Reasoning {
            text: thinking,
            opaque: Some(serde_json::json!({ "signature": signature })),
        },
        ContentBlock::RedactedThinking { data } => AssistantContent::Reasoning {
            text: String::new(),
            opaque: Some(serde_json::json!({ "redacted": data })),
        },
        ContentBlock::ToolUse { id, name, input } => AssistantContent::ToolCall(ToolCall {
            id,
            name,
            arguments: input,
        }),
        ContentBlock::ToolResult { .. } | ContentBlock::Image { .. } => AssistantContent::Text {
            text: "[unsupported provider response block]".to_owned(),
        },
    }
}

pub(super) fn map_error(error: &FluxError) -> ProviderError {
    let kind = match error {
        FluxError::Auth(_) => ProviderErrorKind::Authentication,
        FluxError::Api { status: 429, .. } => ProviderErrorKind::RateLimited,
        FluxError::Api {
            status: 400 | 413 | 422,
            message,
        } if is_context_overflow(message) => ProviderErrorKind::ContextWindowExceeded,
        FluxError::Http(_) | FluxError::Io(_) => ProviderErrorKind::Transport,
        FluxError::Serde(_) | FluxError::StreamDecode(_) => ProviderErrorKind::Protocol,
        FluxError::Config(_) => ProviderErrorKind::InvalidRequest,
        _ => ProviderErrorKind::Response,
    };
    ProviderError {
        kind,
        message: error.to_string(),
    }
}

// Flux retains provider error bodies as text. Recognize only explicit context-limit
// errors; unrelated bad requests and HTTP payload-size errors must not compact.
fn is_context_overflow(message: &str) -> bool {
    if let Ok(body) = serde_json::from_str::<serde_json::Value>(message) {
        let error = body.get("error").unwrap_or(&body);
        if error.get("code").and_then(serde_json::Value::as_str) == Some("context_length_exceeded")
        {
            return true;
        }
        return error
            .get("message")
            .and_then(serde_json::Value::as_str)
            .is_some_and(is_context_overflow_description);
    }
    is_context_overflow_description(message)
}

fn is_context_overflow_description(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.starts_with("prompt is too long:")
        || message.contains("maximum context length is")
        || message.contains("exceeds the model's maximum context length")
        || message.contains("exceeds the context window")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::protocol::SystemSegment;

    #[test]
    fn normalized_usage_includes_cache_creation_without_double_counting_subsets() {
        let provider_usage = flux_core::Usage {
            input_tokens: 100,
            cache_creation_input_tokens: 200,
            cache_creation_1h_input_tokens: 150,
            cache_read_input_tokens: 300,
            output_tokens: 50,
            reasoning_tokens: 25,
            ..Default::default()
        };
        let Some(ResponseChunk::Usage { usage }) =
            convert_chunk(Chunk::Usage(provider_usage.clone()))
        else {
            panic!("expected normalized usage");
        };
        assert_eq!(usage.input_tokens, 300);
        assert_eq!(usage.cached_input_tokens, 300);
        assert_eq!(
            usage.input_tokens + usage.cached_input_tokens,
            provider_usage.context_tokens()
        );
        assert_eq!(usage.output_tokens, 50);
    }

    #[test]
    fn generated_messages_keep_provenance_but_use_the_user_role_on_the_wire() {
        for content in [
            UserContent::ParentInput {
                text: "parent steering".to_owned(),
            },
            UserContent::Compaction {
                text: "preserved context".to_owned(),
            },
        ] {
            let saved = serde_json::to_value(&content).unwrap();
            assert!(matches!(
                saved["type"].as_str(),
                Some("parent_input" | "compaction")
            ));
            let converted = convert_message(Message::User(vec![content])).unwrap();
            assert_eq!(converted.role, Role::User);
            assert!(
                matches!(&converted.content[0], ContentBlock::Text { text } if text == saved["text"].as_str().unwrap())
            );
        }
    }

    #[test]
    fn finish_reason_preserves_truncation() {
        for reason in [
            None,
            Some(flux_core::StopReason::EndTurn),
            Some(flux_core::StopReason::MaxTokens),
            Some(flux_core::StopReason::ToolUse),
        ] {
            assert_eq!(
                convert_chunk(Chunk::Done {
                    stop_reason: reason
                }),
                Some(ResponseChunk::Finished {
                    truncated: reason == Some(flux_core::StopReason::MaxTokens),
                })
            );
        }
    }

    #[test]
    fn only_explicit_context_errors_are_classified_for_compaction() {
        for message in [
            r#"{"error":{"code":"context_length_exceeded","message":"Too many tokens"}}"#,
            r#"{"error":{"type":"invalid_request_error","message":"prompt is too long: 140000 tokens > 128000 maximum"}}"#,
            "This model's maximum context length is 128000 tokens.",
        ] {
            assert_eq!(
                map_error(&FluxError::Api {
                    status: 400,
                    message: message.to_owned()
                })
                .kind,
                ProviderErrorKind::ContextWindowExceeded,
            );
        }
        for (status, message) in [
            (400, "invalid tools schema"),
            (400, "max_tokens must be positive"),
            (413, "request body too large"),
            (500, "maximum context length is unavailable"),
            (429, "maximum context length is 128000 tokens"),
        ] {
            assert_ne!(
                map_error(&FluxError::Api {
                    status,
                    message: message.to_owned()
                })
                .kind,
                ProviderErrorKind::ContextWindowExceeded,
            );
        }
    }

    #[test]
    fn output_limit_is_only_sent_when_configured() {
        use flux_provider::WireCodec;
        use flux_providers::openai::{OpenAiChat, OpenAiResponses};

        for limit in [None, Some(32_768)] {
            for (model, codec, field) in [
                ("qwen36-35", &OpenAiChat as &dyn WireCodec, "max_tokens"),
                (
                    "gpt-5",
                    &OpenAiChat as &dyn WireCodec,
                    "max_completion_tokens",
                ),
                (
                    "gpt-5",
                    &OpenAiResponses { codex: false } as &dyn WireCodec,
                    "max_output_tokens",
                ),
            ] {
                let request = convert_request(ModelRequest {
                    model: model.to_owned(),
                    system: Vec::new(),
                    messages: vec![Message::User(vec![UserContent::Text {
                        text: "hello".to_owned(),
                    }])],
                    tools: Vec::new(),
                    response_schema: None,
                    reasoning: None,
                    max_output_tokens: limit,
                    correlation: None,
                })
                .unwrap();
                let body = codec.build_body(&request).unwrap();
                if let Some(limit) = limit {
                    assert_eq!(body[field], limit);
                } else {
                    for field in ["max_tokens", "max_completion_tokens", "max_output_tokens"] {
                        assert!(body.get(field).is_none(), "unexpected limit: {body}");
                    }
                }
            }
        }
    }

    #[test]
    fn console_output_is_a_separate_tool_text_block() {
        for is_error in [false, true] {
            for console_output in ["", "Processed 12 files\n"] {
                let converted =
                    convert_message(Message::Tool(vec![crate::provider::protocol::ToolResult {
                        call_id: "script-1".to_owned(),
                        name: "script".to_owned(),
                        result: serde_json::json!({"changed":3}),
                        console_output: console_output.to_owned(),
                        images: Vec::new(),
                        is_error,
                    }]))
                    .unwrap();
                let ContentBlock::ToolResult {
                    content,
                    is_error: actual_error,
                    ..
                } = &converted.content[0]
                else {
                    panic!("expected tool result");
                };
                assert_eq!(*actual_error, is_error);
                assert!(
                    matches!(&content[0], ToolResultContent::Text { text } if text == r#"{"changed":3}"#)
                );
                if console_output.is_empty() {
                    assert_eq!(content.len(), 1);
                } else {
                    assert_eq!(content.len(), 2);
                    assert!(
                        matches!(&content[1], ToolResultContent::Text { text } if text == "Console output:\nProcessed 12 files\n")
                    );
                }
            }
        }
    }

    #[test]
    fn stable_system_stays_cached_and_transient_state_remains_a_request_suffix() {
        let converted = convert_request(ModelRequest {
            model: "test".to_owned(),
            system: vec![SystemSegment {
                text: "stable system".to_owned(),
                cache: true,
            }],
            messages: vec![
                Message::User(vec![UserContent::Text {
                    text: "hello".to_owned(),
                }]),
                Message::User(vec![UserContent::Runtime {
                    text: "<skyhook_state>{}</skyhook_state>".to_owned(),
                }]),
            ],
            tools: Vec::new(),
            response_schema: None,
            reasoning: None,
            max_output_tokens: None,
            correlation: Some("session/agent".to_owned()),
        })
        .unwrap();

        assert_eq!(converted.system, None);
        assert_eq!(converted.system_segments.len(), 1);
        assert_eq!(converted.system_segments[0].text, "stable system");
        assert!(converted.system_segments[0].cache);
        assert!(converted.cache_tail);
        assert_eq!(converted.messages.len(), 2);
        assert_eq!(converted.messages[0].role, Role::User);
        assert_eq!(converted.messages[0].content.len(), 1);
        assert_eq!(converted.messages[1].role, Role::User);
        assert!(matches!(
            &converted.messages[1].content[0],
            ContentBlock::Text { text } if text.contains("<skyhook_state>")
        ));
    }
}
