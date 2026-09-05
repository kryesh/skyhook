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
        metadata: serde_json::Map::new(),
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
        UserContent::Text { text } | UserContent::Runtime { text } => {
            Ok(ContentBlock::Text { text })
        }
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
                input_tokens: usage.input_tokens,
                cached_input_tokens: usage.cache_read_input_tokens,
                output_tokens: usage.output_tokens,
            },
        }),
        Chunk::MessageStart { .. }
        | Chunk::ToolInputDelta { .. }
        | Chunk::Done { .. }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::protocol::SystemSegment;

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
    fn stable_system_stays_cached_and_runtime_state_stays_in_conversation() {
        let converted = convert_request(ModelRequest {
            model: "test".to_owned(),
            system: vec![SystemSegment {
                text: "stable system".to_owned(),
                cache: true,
            }],
            messages: vec![Message::User(vec![
                UserContent::Text {
                    text: "hello".to_owned(),
                },
                UserContent::Runtime {
                    text: "<skyhook_state>{}</skyhook_state>".to_owned(),
                },
            ])],
            tools: Vec::new(),
            reasoning: None,
            max_output_tokens: None,
            correlation: Some("session/agent".to_owned()),
        })
        .unwrap();

        assert_eq!(converted.system, None);
        assert_eq!(converted.system_segments.len(), 1);
        assert_eq!(converted.system_segments[0].text, "stable system");
        assert!(converted.system_segments[0].cache);
        assert_eq!(converted.messages.len(), 1);
        assert_eq!(converted.messages[0].role, Role::User);
        assert_eq!(converted.messages[0].content.len(), 2);
        assert!(matches!(
            &converted.messages[0].content[1],
            ContentBlock::Text { text } if text.contains("<skyhook_state>")
        ));
    }
}
