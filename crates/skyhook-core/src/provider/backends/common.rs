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
    if let crate::provider::protocol::ResponseChunk::ItemEnded {
        replay: Some(replay),
        ..
    } = chunk
    {
        replay.scope = scope.into();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_or_malformed_image_is_never_dropped() {
        let mut image = ImageReference {
            sha256: String::new(),
            media_type: "image/png".into(),
            name: "x".into(),
            bytes: 1,
            data_base64: None,
        };
        assert!(image_url(&image).is_err());
        image.data_base64 = Some("?".into());
        assert!(anthropic_image(&image).is_err());
        image.data_base64 = Some("YQ==".into());
        assert_eq!(image_url(&image).unwrap(), "data:image/png;base64,YQ==");
        image.bytes = 2;
        assert!(image_url(&image).is_err());
    }
    #[test]
    fn script_console_stays_nested_in_tool_result() {
        let tool = ToolResult {
            call_id: "script-1".into(),
            name: "script".into(),
            result: json!({"value":{"is_error":true},"console":"captured\n"}),
            images: Vec::new(),
            is_error: false,
        };
        assert_eq!(
            serde_json::from_str::<Value>(&tool_text(&tool)).unwrap(),
            json!({"result":{"value":{"is_error":true},"console":"captured\n"},"is_error":false})
        );
    }

    #[test]
    fn reasoning_is_protocol_and_model_bound() {
        let opaque = Some(reasoning_envelope(
            "responses",
            "model",
            json!({"summary":[]}),
        ));
        assert!(opaque_payload(&opaque, "responses", "model").is_some());
        assert!(opaque_payload(&opaque, "anthropic", "model").is_none());
        assert!(opaque_payload(&opaque, "responses", "other").is_none());
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
