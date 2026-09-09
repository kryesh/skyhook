//! Lossless shared input conversion and opaque reasoning provenance.
use crate::{
    media::ImageReference,
    provider::{ProviderError, ProviderErrorKind, protocol::ToolResult},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};

pub(crate) fn invalid(message: impl Into<String>) -> ProviderError {
    ProviderError {
        kind: ProviderErrorKind::InvalidRequest,
        message: message.into(),
    }
}

fn image_data(image: &ImageReference) -> Result<&str, ProviderError> {
    if !matches!(
        image.media_type.as_str(),
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    ) {
        return Err(invalid(format!(
            "unsupported image media type: {}",
            image.media_type
        )));
    }
    let data = image
        .data_base64
        .as_deref()
        .ok_or_else(|| invalid(format!("image {} has no request-local payload", image.name)))?;
    let decoded = STANDARD
        .decode(data)
        .map_err(|_| invalid(format!("image {} has invalid base64", image.name)))?;
    if decoded.is_empty() || decoded.len() as u64 != image.bytes {
        return Err(invalid(format!(
            "image {} payload does not match its byte length",
            image.name
        )));
    }
    Ok(data)
}

pub(crate) fn image_url(image: &ImageReference) -> Result<String, ProviderError> {
    Ok(format!(
        "data:{};base64,{}",
        image.media_type,
        image_data(image)?
    ))
}

pub(crate) fn anthropic_image(image: &ImageReference) -> Result<Value, ProviderError> {
    Ok(
        json!({"type":"image", "source":{"type":"base64", "media_type":image.media_type, "data":image_data(image)?}}),
    )
}

pub(crate) fn tool_text(tool: &ToolResult) -> String {
    // Keep the result and failure status distinguishable; a JSON result
    // that happens to contain similarly named keys must not overwrite metadata.
    json!({"result":tool.result,"is_error":tool.is_error}).to_string()
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
    for message in &mut request.messages {
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

    /// Exercise the actual journal boundary, not just a serde round trip. Keep
    /// this in backend tests so each codec verifies the resumed wire payload.
    pub(crate) async fn resume_request(
        request: &crate::provider::protocol::ModelRequest,
    ) -> crate::provider::protocol::ModelRequest {
        use crate::{
            identity::AgentId,
            session::{
                ContextMessage, ModelPurpose, SessionEvent, SessionStore, reconstruct_model_request,
            },
        };
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::create(directory.path()).await.unwrap();
        let agent = AgentId::root(store.id());
        let mut template = request.clone();
        template.messages.clear();
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
        let mut messages = Vec::new();
        for message in &request.messages {
            let record = store
                .append(
                    agent.clone(),
                    SessionEvent::MessageCommitted {
                        message: message.clone(),
                    },
                )
                .await
                .unwrap();
            messages.push(ContextMessage::Source {
                sequence: record.sequence,
            });
        }
        let call = store
            .append(
                agent,
                SessionEvent::ModelRequested {
                    context: context.sequence,
                    purpose: ModelPurpose::Agent,
                    messages,
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
        use crate::provider::protocol::{AssistantItem, Message, ModelRequest, ResponseChunk};
        let a = reasoning_scope("api", "https://a.example/v1/responses");
        let b = reasoning_scope("api", "https://b.example/v1/responses");
        assert_ne!(a, b);
        let mut chunk = ResponseChunk::ItemEnded {
            id: "reasoning-0".into(),
            replay: Some(reasoning_envelope(
                "responses",
                "same-model",
                json!({"type":"reasoning","encrypted_content":"private"}),
            )),
        };
        bind_reasoning_scope(&mut chunk, &a);
        let ResponseChunk::ItemEnded { replay, .. } = chunk else {
            unreachable!()
        };
        let mut block = AssistantItem::reasoning("reasoning-0", 0, "summary", replay);
        block
            .blocks
            .push(crate::provider::protocol::AssistantBlock {
                id: "summary-1".into(),
                position: 1,
                content: crate::provider::protocol::BlockContent::Reasoning {
                    text: "second summary".into(),
                },
            });
        let expected_blocks = block.blocks.clone();
        let request = ModelRequest {
            model: "same-model".into(),
            system: vec![],
            messages: vec![Message::Assistant(vec![block])],
            tools: vec![],
            response_schema: None,
            reasoning: None,
            max_output_tokens: None,
            correlation: None,
        };
        let mut matching = request.clone();
        filter_reasoning_scope(&mut matching, &a);
        assert_eq!(matching, request);
        let mut foreign = request;
        filter_reasoning_scope(&mut foreign, &b);
        let Message::Assistant(parts) = &foreign.messages[0] else {
            unreachable!()
        };
        assert!(parts[0].replay.is_none());
        assert_eq!(parts[0].blocks, expected_blocks);
    }
}
