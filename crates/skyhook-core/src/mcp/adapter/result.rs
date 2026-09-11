//! Preserve MCP result envelopes and persist bounded image attachments.
use super::super::manager::McpError;
use crate::{
    media::{MAX_IMAGE_BYTES, MAX_IMAGE_BYTES_PER_SUBMISSION, MAX_IMAGES_PER_SUBMISSION},
    session::SessionStore,
    tool::{ToolError, ToolOutput},
};
use base64::Engine as _;
use rmcp::model::CallToolResult;
use serde_json::json;

pub(super) fn map_error(error: McpError) -> ToolError {
    match error {
        McpError::Cancelled => ToolError::Cancelled,
        other => ToolError::Failed(other.to_string()),
    }
}

/// Keep the MCP result envelope (including structured content, error flag and
/// unknown resource/audio content). Image payloads use normal session blobs and
/// attachments instead of leaking large base64 strings into textual output.
pub(super) async fn map_result(
    result: CallToolResult,
    store: &SessionStore,
) -> Result<ToolOutput, ToolError> {
    let is_error = result.is_error == Some(true);
    let mut value = serde_json::to_value(&result)?;
    let mut images = Vec::new();
    let mut total_bytes = 0_u64;
    // Strip every payload before importing any of them. A failed import must
    // not return the rejected (possibly oversized) image or later images as
    // base64 text through FailedWithOutput.
    for (index, block) in result.content.iter().enumerate() {
        if block.as_image().is_some()
            && let Some(block) = value["content"][index].as_object_mut()
        {
            block.remove("data");
            block.insert("imageError".to_owned(), json!("image not imported"));
        }
    }
    for (index, block) in result.content.iter().enumerate() {
        let Some(image) = block.as_image() else {
            continue;
        };
        let reference = match import_image(image, index, images.len(), total_bytes, store).await {
            Ok(reference) => reference,
            Err(error) => {
                value["content"][index]["imageError"] = json!(error.to_string());
                return Err(ToolError::with_output(
                    error.to_string(),
                    ToolOutput::new(value).with_images(images),
                ));
            }
        };
        total_bytes += reference.bytes;
        if let Some(block) = value["content"][index].as_object_mut() {
            block.remove("imageError");
            block.insert("image".to_owned(), serde_json::to_value(&reference)?);
        }
        images.push(reference);
    }
    let output = ToolOutput::new(value).with_images(images);
    if is_error {
        Err(ToolError::with_output("MCP tool reported an error", output))
    } else {
        Ok(output)
    }
}

async fn import_image(
    image: &rmcp::model::ImageContent,
    index: usize,
    image_count: usize,
    total_bytes: u64,
    store: &SessionStore,
) -> Result<crate::media::ImageReference, ToolError> {
    let limit = usize::try_from(MAX_IMAGE_BYTES).expect("image limit fits usize");
    if image.data.len() > limit.div_ceil(3) * 4 {
        return Err(ToolError::Failed(
            "MCP image exceeds the image byte limit".to_owned(),
        ));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&image.data)
        .map_err(|error| ToolError::Failed(format!("invalid MCP image encoding: {error}")))?;
    let total_bytes = total_bytes + bytes.len() as u64;
    if bytes.len() > limit
        || image_count >= MAX_IMAGES_PER_SUBMISSION
        || total_bytes > MAX_IMAGE_BYTES_PER_SUBMISSION
    {
        return Err(ToolError::Failed(
            "MCP images exceed attachment limits".to_owned(),
        ));
    }
    let extension = match image.mime_type.as_str() {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => {
            return Err(ToolError::Failed(format!(
                "unsupported MCP image type: {}",
                image.mime_type
            )));
        }
    };
    store
        .import_blob(
            &bytes,
            format!("mcp-image-{index}.{extension}"),
            image.mime_type.clone(),
        )
        .await
        .map_err(|error| ToolError::Failed(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::TestRuntime;

    #[tokio::test]
    async fn result_envelope_and_errors_preserve_upstream_content() {
        let runtime = TestRuntime::new().await;
        for flag in [None, Some(false), Some(true)] {
            let mut value = json!({
                "content":[{"type":"text","text":"details"},{"type":"resource","resource":{"uri":"test:///item","text":"body"}},{"type":"audio","mimeType":"audio/wav","data":"YWJj"}],
                "structuredContent":{"answer":42},"_meta":{"example":"value"}
            });
            if let Some(flag) = flag {
                value["isError"] = json!(flag);
            }
            let result: CallToolResult = serde_json::from_value(value.clone()).unwrap();
            let mapped = map_result(result, &runtime.store).await;
            let output = if flag == Some(true) {
                let ToolError::FailedWithOutput { output, .. } = mapped.unwrap_err() else {
                    panic!("MCP error must retain output")
                };
                *output
            } else {
                mapped.unwrap()
            };
            assert_eq!(output.value, value);
            assert!(output.images.is_empty());
        }
        assert!(matches!(
            map_error(McpError::Cancelled),
            ToolError::Cancelled
        ));
        assert!(matches!(map_error(McpError::Timeout), ToolError::Failed(_)));
    }

    #[tokio::test]
    async fn images_use_persistent_blobs_even_in_failed_results() {
        let runtime = TestRuntime::new().await;
        let encoded = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a/1cAAAAASUVORK5CYII=";
        let result = serde_json::from_value(json!({
            "content":[{"type":"text","text":"image follows"},{"type":"image","mimeType":"image/png","data":encoded}],
            "structuredContent":{"ok":false}, "isError":true
        })).unwrap();
        let ToolError::FailedWithOutput { output, .. } =
            map_result(result, &runtime.store).await.unwrap_err()
        else {
            panic!("expected failed output")
        };
        assert_eq!(output.images.len(), 1);
        assert!(output.images[0].data_base64.is_none());
        assert_eq!(
            runtime.store.read_blob(&output.images[0]).await.unwrap(),
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .unwrap()
        );
        assert_eq!(output.value["content"][0]["text"], "image follows");
        assert!(output.value["content"][1].get("data").is_none());
        assert_eq!(
            output.value["content"][1]["image"]["sha256"],
            output.images[0].sha256
        );
        assert_eq!(output.value["structuredContent"], json!({"ok":false}));
    }

    #[tokio::test]
    async fn malformed_unsupported_and_oversized_images_fail_safely() {
        let runtime = TestRuntime::new().await;
        for (mime, data) in [
            ("image/png", "not base64!".to_owned()),
            ("image/svg+xml", "YWJj".to_owned()),
            (
                "image/png",
                "A".repeat((MAX_IMAGE_BYTES as usize).div_ceil(3) * 4 + 1),
            ),
        ] {
            let result = serde_json::from_value(json!({"content":[
                {"type":"image","mimeType":mime,"data":data},
                {"type":"image","mimeType":"image/png","data":"YWJj"},
                {"type":"text","text":"preserved"}
            ]}))
            .unwrap();
            let ToolError::FailedWithOutput { output, .. } =
                map_result(result, &runtime.store).await.unwrap_err()
            else {
                panic!("expected failed output")
            };
            assert!(output.images.is_empty());
            for index in 0..2 {
                assert!(output.value["content"][index].get("data").is_none());
                assert!(output.value["content"][index]["imageError"].is_string());
            }
            assert_eq!(output.value["content"][2]["text"], "preserved");
        }
    }

    // A real stdio peer ensures the adapter exercises the manager's catalog and
    // dispatch rather than a second, adapter-only mock transport.
}
