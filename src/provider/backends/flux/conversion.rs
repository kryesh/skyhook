use flux_core::{
    Chunk, ContentBlock, Error as FluxError, ImageSource, Message as FluxMessage, Role,
    StopReason as FluxStopReason, ToolResultContent,
};
use flux_provider::{Effort, Request, RequestTrace};

use crate::{
    provider::protocol::{
        AssistantContent, Message, ModelRequest, ResponseChunk, StopReason, ToolCall, Usage,
        UserContent,
    },
    provider::{ProviderError, ProviderErrorKind, RetryAdvice},
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
    let max_tokens = u32::try_from(request.max_output_tokens.unwrap_or(8_192)).map_err(|_| {
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

pub(super) fn convert_chunk(chunk: Chunk) -> ResponseChunk {
    match chunk {
        Chunk::MessageStart { model } => ResponseChunk::MessageStart { model },
        Chunk::TextDelta(text) => ResponseChunk::TextDelta { text },
        Chunk::ThinkingDelta(text) => ResponseChunk::ReasoningDelta { text },
        Chunk::ToolInputDelta { name, partial_json } => {
            ResponseChunk::ToolInputDelta { name, partial_json }
        }
        Chunk::Block(block) => ResponseChunk::Block {
            block: convert_block(block),
        },
        Chunk::Usage(usage) => ResponseChunk::Usage {
            usage: Usage {
                input_tokens: usage.input_tokens,
                cached_input_tokens: usage.cache_read_input_tokens,
                output_tokens: usage.output_tokens,
            },
        },
        Chunk::Done { stop_reason } => ResponseChunk::Done {
            stop_reason: stop_reason.map(convert_stop_reason),
        },
        Chunk::StreamDiagnostic {
            dropped_frames,
            detail,
        } => ResponseChunk::Diagnostic {
            detail,
            dropped_frames,
        },
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

const fn convert_stop_reason(reason: FluxStopReason) -> StopReason {
    match reason {
        FluxStopReason::EndTurn | FluxStopReason::StopSequence => StopReason::Complete,
        FluxStopReason::MaxTokens => StopReason::MaxTokens,
        FluxStopReason::ToolUse | FluxStopReason::PauseTurn => StopReason::ToolUse,
        FluxStopReason::Refusal => StopReason::Refusal,
        FluxStopReason::Unknown => StopReason::Other,
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
    let retry = match kind {
        ProviderErrorKind::RateLimited | ProviderErrorKind::Transport => RetryAdvice::Backoff,
        _ => RetryAdvice::Never,
    };
    ProviderError {
        kind,
        message: error.to_string(),
        retry,
    }
}
