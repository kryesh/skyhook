//! Shared model-facing input conversion and opaque reasoning provenance.
use crate::{
    media::{AttachmentRef, ImageRef, MediaError, TextRef},
    provider::{
        ProviderError, ProviderErrorKind,
        protocol::{
            AssistantItem, Binding, BlockId, BlockRef, Completion, CompletionError, CutReason,
            ItemId, ItemKind, Message, ModelRequest, Position, Provenance, Replay, ResponseEvent,
            Scope, TextBlock, ToolResult, UserContent,
        },
    },
};
use serde_json::{Map, Value, json};

pub(crate) fn invalid(message: impl Into<String>) -> ProviderError {
    ProviderError {
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

/// The wire identity of an index-addressed item.
pub(crate) fn item_id(index: usize) -> ItemId {
    ItemId::try_from(index.to_string()).expect("digits are nonblank")
}

/// The single block of an index-addressed item.
pub(crate) fn block_ref(index: usize) -> BlockRef {
    BlockRef {
        item: item_id(index),
        block: BlockId::try_from("0".to_owned()).expect("literal is nonblank"),
    }
}

pub(crate) fn position(index: usize) -> Result<Position, ProviderError> {
    Position::try_from(index).map_err(|error| ProviderError::protocol(error.to_string()))
}

/// The one text block of an index-addressed item, keyed like its deltas.
pub(crate) fn single_block(index: usize, text: String) -> Vec<TextBlock> {
    vec![TextBlock {
        id: block_ref(index).block,
        position: 0.into(),
        text,
    }]
}

pub(crate) fn delta(block: BlockRef, kind: ItemKind, text: impl Into<String>) -> ResponseEvent {
    ResponseEvent::Delta {
        block,
        kind,
        text: text.into(),
    }
}

/// How a response's finish settled: normally, with its tool calls executable, or
/// cut, after which the decoder keeps text and reasoning but no calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Finish {
    Normal,
    Cut(CutReason),
}

impl Finish {
    pub(crate) fn complete(self, items: Vec<AssistantItem>) -> Result<Completion, CompletionError> {
        match self {
            Self::Normal => Completion::finished(items),
            Self::Cut(reason) => Completion::cut(items, reason),
        }
    }
}

pub(crate) fn opaque_payload<'a>(
    replay: &'a Option<Replay>,
    protocol: &str,
    model: &str,
) -> Option<&'a Value> {
    let replay = replay.as_ref()?;
    (replay.provenance.protocol == protocol && replay.provenance.model == model)
        .then_some(&replay.payload)
}

pub(crate) fn replay(
    protocol: &str,
    model: &str,
    scope: &Scope,
    payload: Value,
    binding: Binding,
) -> Replay {
    Replay {
        provenance: Provenance {
            protocol: protocol.into(),
            model: model.into(),
            scope: scope.clone(),
        },
        payload,
        binding,
    }
}

/// Provider-bound provenance prevents replaying private reasoning to a different
/// endpoint even when protocol and model names happen to match.
pub(crate) fn reasoning_scope(name: &str, endpoint: &str) -> Scope {
    Scope::try_from(crate::sha256_hex(format!("{name}\0{endpoint}"))).expect("digest is nonblank")
}

pub(crate) fn filter_reasoning_scope(request: &mut ModelRequest, scope: &Scope) {
    for message in request.messages_mut() {
        if let Message::Assistant(items) = message {
            for item in items {
                if let AssistantItem::Reasoning { replay, .. } = item
                    && replay
                        .as_ref()
                        .is_some_and(|replay| replay.provenance.scope != *scope)
                {
                    *replay = None;
                }
            }
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::media::{BlobRef, ImageFormat};
    use crate::provider::protocol::{
        AssistantItem, Completion, LiveBlock, LiveResponse, Message, ModelRequest, Step, TextBlock,
        Usage,
    };

    /// The scope test decoders issue replay under.
    pub(crate) fn scope() -> Scope {
        Scope::try_from("scope".to_owned()).unwrap()
    }

    /// What a consumer sees of a decoded event sequence that reached its end.
    pub(crate) struct Reduced {
        pub(crate) blocks: Vec<LiveBlock>,
        pub(crate) usage: Usage,
        pub(crate) completion: Completion,
    }

    impl Reduced {
        pub(crate) fn items(&self) -> &[AssistantItem] {
            self.completion.items()
        }

        /// Provisional text per block, in arrival order.
        pub(crate) fn streamed(&self, kind: ItemKind) -> Vec<&str> {
            self.blocks
                .iter()
                .filter(|block| block.kind == kind)
                .map(|block| block.text.as_str())
                .collect()
        }
    }

    /// Reduce events as the runtime does; a missing or non-final `End` is a decoder bug.
    pub(crate) fn reduce(events: impl IntoIterator<Item = ResponseEvent>) -> Reduced {
        let mut live = LiveResponse::default();
        let mut ended = None;
        for event in events {
            assert!(ended.is_none(), "event after End: {event:?}");
            // Keep what streamed, not the authoritative view the end supplies.
            let streamed = live.blocks().to_vec();
            match std::mem::take(&mut live).push(event) {
                Step::Open(open) => live = open,
                Step::Ended {
                    completion, usage, ..
                } => {
                    ended = Some(Reduced {
                        blocks: streamed,
                        usage,
                        completion,
                    });
                }
            }
        }
        ended.expect("response ended")
    }

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
            blobs: Default::default(),
        }
    }

    #[test]
    fn endpoint_scope_is_required_and_preserves_only_matching_private_state() {
        let a = reasoning_scope("api", "https://a.example/v1/responses");
        let b = reasoning_scope("api", "https://b.example/v1/responses");
        assert_ne!(a, b);
        let native = json!({"type":"reasoning","encrypted_content":"private"});
        let replay = replay("responses", "same-model", &a, native, Binding::Free);
        let mut item = AssistantItem::reasoning("reasoning-0", 0, "summary", Some(replay));
        let AssistantItem::Reasoning { blocks, .. } = &mut item else {
            unreachable!()
        };
        blocks.push(TextBlock {
            id: BlockId::try_from("summary-1".to_owned()).unwrap(),
            position: 1.into(),
            text: "second summary".into(),
        });
        let expected_text = item.reasoning_text();
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
        assert!(parts[0].replay().is_none());
        assert_eq!(parts[0].reasoning_text(), expected_text);
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
