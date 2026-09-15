//! Shared model-facing input conversion and opaque reasoning provenance.
use crate::{
    job::omit_null_fields,
    media::{ImageRef, MediaError, TextRef},
    provider::{
        ProviderError, ProviderErrorKind,
        protocol::{ModelRequest, ToolResult},
    },
};
use serde_json::{Value, json};

pub(crate) fn invalid(message: impl Into<String>) -> ProviderError {
    ProviderError {
        retry_after: None,
        kind: ProviderErrorKind::InvalidRequest,
        message: message.into(),
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

pub(crate) fn tool_text(tool: &ToolResult) -> String {
    // Keep the result and failure status distinguishable; a JSON result
    // that happens to contain similarly named keys must not overwrite metadata.
    let mut value = json!({"result":tool.result,"is_error":tool.is_error});
    omit_null_fields(&mut value);
    value.to_string()
}

pub(crate) fn opaque_payload<'a>(
    replay: &'a Option<crate::provider::protocol::ReplayEnvelope>,
    protocol: &str,
    model: &str,
) -> Option<&'a Value> {
    let envelope = replay.as_ref()?;
    (envelope.version == 1 && envelope.protocol == protocol && envelope.model == model)
        .then_some(&envelope.payload)
}

pub(crate) fn reasoning_envelope(
    protocol: &str,
    model: &str,
    payload: Value,
) -> crate::provider::protocol::ReplayEnvelope {
    crate::provider::protocol::ReplayEnvelope {
        version: 1,
        protocol: protocol.into(),
        model: model.into(),
        scope: String::new(),
        payload,
    }
}

/// Provider-bound provenance prevents replaying private reasoning to a different
/// endpoint even when protocol and model names happen to match.
pub(crate) fn reasoning_scope(name: &str, endpoint: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(format!("{name}\0{endpoint}"))
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(crate) fn filter_reasoning_scope(
    request: &mut crate::provider::protocol::ModelRequest,
    scope: &str,
) {
    use crate::provider::protocol::Message;
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

pub(crate) fn bind_reasoning_scope(
    chunk: &mut crate::provider::protocol::ResponseChunk,
    scope: &str,
) {
    use crate::provider::protocol::ResponseChunk;
    match chunk {
        ResponseChunk::ItemEnded {
            replay: Some(replay),
            ..
        }
        | ResponseChunk::ItemReplayUpdated { replay, .. } => replay.scope = scope.into(),
        _ => {}
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::media::{BlobRef, ImageFormat};
    use crate::provider::protocol::{
        AssistantBlock, AssistantItem, BlockContent, Message, ModelRequest, ResponseChunk, ToolCall,
    };

    #[test]
    fn replay_binding_and_filtering_include_text_tool_and_late_enrichment() {
        let call = ToolCall::new("call", "lookup", json!({})).unwrap();
        let items = [
            AssistantItem::text("text", 0, "visible"),
            AssistantItem::reasoning("reasoning", 1, "summary", None),
            AssistantItem::tool_call("tool", 2, call),
        ];
        for mut item in items {
            for update in [false, true] {
                let replay = reasoning_envelope("responses", "model", json!({"opaque":[1,null]}));
                let id = item.id.clone();
                let mut chunk = if update {
                    ResponseChunk::ItemReplayUpdated { id, replay }
                } else {
                    let replay = Some(replay);
                    ResponseChunk::ItemEnded { id, replay }
                };
                bind_reasoning_scope(&mut chunk, "expected");
                item.replay = match chunk {
                    ResponseChunk::ItemEnded { replay, .. } => replay,
                    ResponseChunk::ItemReplayUpdated { replay, .. } => Some(replay),
                    _ => unreachable!(),
                };
                assert!(opaque_payload(&item.replay, "responses", "model").is_some());
                let mut request = request("model");
                request.history = vec![Message::Assistant(vec![item.clone()])];
                filter_reasoning_scope(&mut request, "foreign");
                let Message::Assistant(filtered) = &request.history[0] else {
                    unreachable!()
                };
                assert!(filtered[0].replay.is_none());
                assert_eq!(filtered[0].blocks, item.blocks);
            }
        }
    }

    /// Minimal request shared by codec/provider tests; cases override only the
    /// inputs relevant to the behavior under test.
    pub(crate) fn request(model: &str) -> crate::provider::protocol::ModelRequest {
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

    /// Exercise the actual journal boundary, not just a serde round trip. Keep
    /// this in backend tests so each codec verifies the resumed wire payload.
    pub(crate) async fn resume_request(
        request: &crate::provider::protocol::ModelRequest,
    ) -> crate::provider::protocol::ModelRequest {
        use crate::{
            identity::AgentId,
            session::{ModelPurpose, SessionEvent, SessionStore, reconstruct_model_request},
        };
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::create(directory.path()).await.unwrap();
        let agent = AgentId::root(store.id());
        let mut template = request.clone();
        (template.history, template.tail) = (Vec::new(), Vec::new());
        template.history_lifetime = Default::default();
        let context = store
            .append(
                agent.clone(),
                SessionEvent::ModelContext {
                    provider: "native-replay-test".into(),
                    template,
                },
            )
            .await
            .unwrap();
        let mut history = Vec::new();
        for message in &request.history {
            let record = store
                .append(
                    agent.clone(),
                    SessionEvent::MessageCommitted {
                        message: message.clone(),
                    },
                )
                .await
                .unwrap();
            history.push(record.sequence);
        }
        let call = store
            .append(
                agent,
                SessionEvent::ModelRequested {
                    context: context.sequence,
                    purpose: ModelPurpose::Agent,
                    history,
                    tail: request.tail.clone(),
                    history_lifetime: request.history_lifetime,
                },
            )
            .await
            .unwrap();
        let id = store.id();
        drop(store);
        let (_store, records) = SessionStore::open(directory.path(), id).await.unwrap();
        let (_, resumed) = reconstruct_model_request(&records, call.sequence).unwrap();
        assert_eq!(&resumed, request);
        resumed
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
