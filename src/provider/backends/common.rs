//! Shared model-facing input conversion and opaque reasoning provenance.
use crate::{
    media::{AttachmentRef, ImageRef, MediaError, TextRef},
    provider::{
        ProviderError, ProviderErrorKind,
        protocol::{
            BlockContent, BlockKind, ItemKind, Message, ModelRequest, ReplayEnvelope,
            ResponseChunk, ToolResult, UserContent,
        },
    },
};
use serde_json::{Map, Value, json};

pub(crate) fn invalid(message: impl Into<String>) -> ProviderError {
    ProviderError {
        retry_after: None,
        kind: ProviderErrorKind::InvalidRequest,
        message: message.into(),
    }
}

/// Tool-argument text as a JSON object: empty or null is `{}`, and a
/// double-encoded object is unwrapped. Anything else is `None`.
pub(crate) fn parse_tool_arguments(raw: &str) -> Option<Map<String, Value>> {
    if raw.trim().is_empty() {
        return Some(Map::new());
    }
    match serde_json::from_str::<Value>(raw).ok()? {
        Value::Object(object) => Some(object),
        Value::String(inner) => parse_tool_arguments(&inner),
        Value::Null => Some(Map::new()),
        _ => None,
    }
}

/// A tool-arguments field: JSON text, or the decoded object some servers send.
/// Missing or null is `{}`; any other type is `None`.
pub(crate) fn arguments_field(value: Option<&Value>) -> Option<Map<String, Value>> {
    match value {
        None | Some(Value::Null) => Some(Map::new()),
        Some(Value::String(text)) => parse_tool_arguments(text),
        Some(Value::Object(object)) => Some(object.clone()),
        Some(_) => None,
    }
}

/// Read a non-negative counter that some servers encode as a float or string.
pub(crate) fn lenient_u64(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number.as_u64().or_else(|| {
            number
                .as_f64()
                .filter(|float| float.fract() == 0.0 && *float >= 0.0 && *float <= u64::MAX as f64)
                .map(|float| float as u64)
        }),
        Value::String(text) => lenient_u64(&serde_json::from_str(text.trim()).ok()?),
        _ => None,
    }
}

fn blob_error(error: MediaError) -> ProviderError {
    invalid(format!("attachment: {error}"))
}

pub(crate) fn image_url(request: &ModelRequest, image: &ImageRef) -> Result<String, ProviderError> {
    let data = request.blobs.base64(&image.blob).map_err(blob_error)?;
    Ok(format!("data:{};base64,{data}", image.format.media_type()))
}

pub(crate) fn anthropic_image(
    request: &ModelRequest,
    image: &ImageRef,
) -> Result<Value, ProviderError> {
    let data = request.blobs.base64(&image.blob).map_err(blob_error)?;
    Ok(json!({"type":"image", "source":{"type":"base64",
        "media_type":image.format.media_type(), "data":data}}))
}

/// Attached text as the model sees it: its source file, if any, then the content.
pub(crate) fn attachment_text(
    request: &ModelRequest,
    text: &TextRef,
) -> Result<String, ProviderError> {
    let content = request.blobs.text(text).map_err(blob_error)?;
    Ok(match &text.file {
        Some(file) => format!("File: {file}\n{content}"),
        None => content.to_owned(),
    })
}

/// User content parts, with `image` encoding the protocol's image part.
pub(crate) fn user_parts(
    request: &ModelRequest,
    parts: &[UserContent],
    text_type: &str,
    image: impl Fn(&ImageRef) -> Result<Value, ProviderError>,
) -> Result<Vec<Value>, ProviderError> {
    let text = |text: &str| json!({"type":text_type, "text":text});
    parts
        .iter()
        .map(|part| match part {
            UserContent::Text { text: value }
            | UserContent::Runtime { text: value }
            | UserContent::ParentInput { text: value }
            | UserContent::Compaction { text: value } => Ok(text(value)),
            UserContent::Attachment { attachment } => match attachment {
                AttachmentRef::Image(reference) => image(reference),
                AttachmentRef::Text(file) => Ok(text(&attachment_text(request, file)?)),
            },
        })
        .collect()
}

/// System segments as the single instruction text of the OpenAI protocols.
pub(crate) fn system_text(request: &ModelRequest) -> Option<String> {
    let segments: Vec<_> = request.system.iter().map(|s| s.text.as_str()).collect();
    (!segments.is_empty()).then(|| segments.join("\n\n"))
}

pub(crate) fn validate_openai_effort(effort: &str) -> Result<(), ProviderError> {
    if matches!(
        effort,
        "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
    ) {
        return Ok(());
    }
    Err(invalid(format!("Unsupported reasoning effort: {effort}")))
}

pub(crate) fn tool_text(tool: &ToolResult) -> String {
    // Keep the result and failure status distinguishable; a JSON result
    // that happens to contain similarly named keys must not overwrite metadata.
    json!({"result":tool.result,"is_error":tool.is_error}).to_string()
}

/// Attach a runtime-only tail message to the last encoded item when that item is a
/// user turn or a tool output. Sent as its own user message, each request's state
/// reads to the model as the user speaking again after every tool call. Returns false
/// when the message must be encoded standalone.
pub(crate) fn attach_runtime_tail(
    items: &mut [Value],
    message: &Message,
    text_type: &str,
    tool_output: fn(&mut Value) -> Option<&mut Value>,
) -> bool {
    let Message::User(parts) = message else {
        return false;
    };
    let texts: Option<Vec<&str>> = parts
        .iter()
        .map(|part| match part {
            UserContent::Runtime { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let (Some(texts), Some(last)) = (texts.filter(|texts| !texts.is_empty()), items.last_mut())
    else {
        return false;
    };
    if last["role"] == "user"
        && let Some(content) = last["content"].as_array_mut()
    {
        content.extend(
            texts
                .into_iter()
                .map(|text| json!({"type": text_type, "text": text})),
        );
        return true;
    }
    if let Some(Value::String(output)) = tool_output(last) {
        for text in texts {
            output.push_str("\n\n");
            output.push_str(text);
        }
        return true;
    }
    false
}

/// Start events of a single-block item identified by its index.
pub(crate) fn start_item(id: usize, kind: ItemKind, block_kind: BlockKind) -> [ResponseChunk; 2] {
    [
        ResponseChunk::ItemStarted {
            id: id.to_string(),
            position: id,
            kind,
        },
        ResponseChunk::BlockStarted {
            item: id.to_string(),
            id: "0".into(),
            position: 0,
            kind: block_kind,
        },
    ]
}

/// End events matching [`start_item`].
pub(crate) fn end_item(
    id: usize,
    content: BlockContent,
    replay: Option<ReplayEnvelope>,
) -> [ResponseChunk; 2] {
    [
        ResponseChunk::BlockEnded {
            item: id.to_string(),
            block: "0".into(),
            content,
        },
        ResponseChunk::ItemEnded {
            id: id.to_string(),
            replay,
        },
    ]
}

pub(crate) fn opaque_payload<'a>(
    replay: &'a Option<ReplayEnvelope>,
    protocol: &str,
    model: &str,
) -> Option<&'a Value> {
    let envelope = replay.as_ref()?;
    (envelope.version == 1 && envelope.protocol == protocol && envelope.model == model)
        .then_some(&envelope.payload)
}

pub(crate) fn reasoning_envelope(protocol: &str, model: &str, payload: Value) -> ReplayEnvelope {
    ReplayEnvelope {
        version: 1,
        protocol: protocol.into(),
        model: model.into(),
        scope: String::new(),
        payload,
        conversation_bound: false,
    }
}

/// Provider-bound provenance prevents replaying private reasoning to a different
/// endpoint even when protocol and model names happen to match.
pub(crate) fn reasoning_scope(name: &str, endpoint: &str) -> String {
    crate::sha256_hex(format!("{name}\0{endpoint}"))
}

pub(crate) fn filter_reasoning_scope(request: &mut ModelRequest, scope: &str) {
    for message in request.messages_mut() {
        if let Message::Assistant(items) = message {
            for item in items {
                if item
                    .replay
                    .as_ref()
                    .is_some_and(|replay| replay.scope != scope)
                {
                    item.replay = None;
                }
            }
        }
    }
}

pub(crate) fn bind_reasoning_scope(chunk: &mut ResponseChunk, scope: &str) {
    if let ResponseChunk::ItemEnded {
        replay: Some(replay),
        ..
    } = chunk
    {
        replay.scope = scope.into();
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::media::{BlobRef, ImageFormat};
    use crate::provider::protocol::{
        AssistantBlock, AssistantItem, BlockContent, Message, ModelRequest, ResponseChunk,
    };

    #[test]
    fn tool_arguments_tolerate_empty_null_and_double_encoded_objects() {
        for raw in ["", "  ", "null", "\"\"", "{}"] {
            assert_eq!(parse_tool_arguments(raw), Some(Map::new()), "{raw:?}");
        }
        let object = json!({"cmd":"ls","n":[1,{"x":null}]});
        assert_eq!(
            parse_tool_arguments(&object.to_string()).map(Value::Object),
            Some(object.clone())
        );
        let double = Value::String(object.to_string()).to_string();
        assert_eq!(
            parse_tool_arguments(&double).map(Value::Object),
            Some(object)
        );
        for raw in ["{\"cmd\":", "[]", "1", "\"text\"", "\"[1]\""] {
            assert_eq!(parse_tool_arguments(raw), None, "{raw:?}");
        }
    }

    #[test]
    fn counters_accept_integral_floats_and_numeric_strings() {
        for (value, expected) in [
            (json!(7), Some(7)),
            (json!(7.0), Some(7)),
            (json!("7"), Some(7)),
            (json!(" 12 "), Some(12)),
            (json!(7.5), None),
            (json!(-1), None),
            (json!("-1"), None),
            (json!("seven"), None),
            (json!(null), None),
        ] {
            assert_eq!(lenient_u64(&value), expected, "{value}");
        }
    }

    /// Minimal request shared by codec/provider tests; cases override only the
    /// inputs relevant to the behavior under test.
    pub(crate) fn request(model: &str) -> ModelRequest {
        use crate::provider::protocol::{Message, ModelRequest, UserContent};
        ModelRequest {
            model: model.into(),
            system: vec![],
            tail: Vec::new(),
            history_lifetime: Default::default(),
            history: vec![Message::User(vec![UserContent::Text {
                text: "hello".into(),
            }])],
            tools: vec![],
            response_schema: None,
            reasoning: None,
            max_output_tokens: Some(8192),
            correlation: None,
            blobs: Default::default(),
        }
    }

    #[test]
    fn endpoint_scope_is_required_and_preserves_only_matching_private_state() {
        let a = reasoning_scope("api", "https://a.example/v1/responses");
        let b = reasoning_scope("api", "https://b.example/v1/responses");
        assert_ne!(a, b);
        let native = json!({"type":"reasoning","encrypted_content":"private"});
        let replay = Some(reasoning_envelope("responses", "same-model", native));
        let mut chunk = ResponseChunk::ItemEnded {
            id: "reasoning-0".into(),
            replay,
        };
        bind_reasoning_scope(&mut chunk, &a);
        let ResponseChunk::ItemEnded { replay, .. } = chunk else {
            unreachable!()
        };
        let mut item = AssistantItem::reasoning("reasoning-0", 0, "summary", replay);
        let content = BlockContent::Reasoning {
            text: "second summary".into(),
        };
        item.blocks.push(AssistantBlock {
            id: "summary-1".into(),
            position: 1,
            content,
        });
        let expected_blocks = item.blocks.clone();
        let messages = vec![Message::Assistant(vec![item])];
        let request = ModelRequest {
            history: messages,
            ..request("same-model")
        };
        let mut matching = request.clone();
        filter_reasoning_scope(&mut matching, &a);
        assert_eq!(matching, request);
        let mut foreign = request;
        filter_reasoning_scope(&mut foreign, &b);
        let Message::Assistant(parts) = &foreign.history[0] else {
            unreachable!()
        };
        assert!(parts[0].replay.is_none());
        assert_eq!(parts[0].blocks, expected_blocks);
    }

    #[test]
    fn direct_provider_wire_parity_requires_a_loaded_blob() {
        let image = ImageRef {
            file: Some("fixture.png".into()),
            format: ImageFormat::Png,
            blob: BlobRef::of(b"x"),
        };
        let text = TextRef {
            file: Some("notes.txt".into()),
            blob: BlobRef::of(b"notes"),
        };
        let mut request = request("model");
        let missing = image_url(&request, &image).unwrap_err();
        assert_eq!(missing.kind, ProviderErrorKind::InvalidRequest);
        assert!(anthropic_image(&request, &image).is_err());
        assert!(attachment_text(&request, &text).is_err());
        request.blobs.insert(image.blob, b"x".to_vec());
        request.blobs.insert(text.blob, b"notes".to_vec());
        assert_eq!(
            image_url(&request, &image).unwrap(),
            "data:image/png;base64,eA=="
        );
        assert_eq!(
            anthropic_image(&request, &image).unwrap(),
            json!({"type":"image", "source":{"type":"base64", "media_type":"image/png", "data":"eA=="}})
        );
        assert_eq!(
            attachment_text(&request, &text).unwrap(),
            "File: notes.txt\nnotes"
        );
        let pasted = TextRef { file: None, ..text };
        assert_eq!(attachment_text(&request, &pasted).unwrap(), "notes");
    }
}
