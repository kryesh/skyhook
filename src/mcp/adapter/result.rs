//! Preserve MCP result envelopes and persist bounded image attachments.
use crate::{
    media::{
        BlobDigest, Image, ImageFormat, ImageRef, MAX_IMAGE_BYTES, MAX_IMAGE_BYTES_PER_SUBMISSION,
        MAX_IMAGES_PER_SUBMISSION, MediaError, decode_base64_bounded,
    },
    session::SessionStore,
    tool::{
        ToolError, ToolOutput,
        diagnostic::{DiagnosticContext, Effects, Operation, Subject},
    },
};
#[cfg(test)]
use base64::Engine as _;
use rmcp::model::{Annotations, CallToolResult, ContentBlock, MetaObject, ResultType};
use serde::Serialize;
use serde_json::Value;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SanitizedResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    result_type: Option<ResultType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    structured_content: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    is_error: Option<bool>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    meta: Option<MetaObject>,
    content: Vec<SanitizedBlock>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum SanitizedBlock {
    // Only map_result creates this, after excluding images. Preserve the complete
    // upstream non-image value instead of defining a closed text/resource/audio
    // subset: arbitrary metadata and structured values stay open.
    NonImage(ContentBlock),
    Image(SanitizedImage),
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SanitizedImage {
    #[serde(rename = "type")]
    kind: &'static str,
    mime_type: String,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    meta: Option<MetaObject>,
    #[serde(skip_serializing_if = "Option::is_none")]
    annotations: Option<Annotations>,
    #[serde(flatten)]
    status: ImageStatus,
}

#[derive(Serialize)]
#[serde(untagged)]
enum ImageStatus {
    Imported {
        image: McpImage,
    },
    Failed {
        #[serde(rename = "imageError")]
        message: String,
    },
}

/// The model-visible object for an imported image. Its shape belongs to the MCP
/// result, independent of how the journal serializes the stored `ImageRef`.
#[derive(Serialize)]
struct McpImage {
    sha256: BlobDigest,
    media_type: &'static str,
    name: String,
    bytes: u64,
}

impl McpImage {
    fn new(index: usize, image: &ImageRef) -> Self {
        let extension = match image.format {
            ImageFormat::Png => "png",
            ImageFormat::Jpeg => "jpg",
            ImageFormat::Gif => "gif",
            ImageFormat::WebP => "webp",
        };
        Self {
            sha256: image.blob.sha256,
            media_type: image.format.media_type(),
            name: format!("mcp-image-{index}.{extension}"),
            bytes: image.blob.bytes,
        }
    }
}

fn output(sanitized: &SanitizedResult, images: Vec<ImageRef>) -> Result<ToolOutput, ToolError> {
    Ok(ToolOutput::new(serde_json::to_value(sanitized)?).with_images(images))
}

/// Strip all image payloads before the first await, so the serializable tree never
/// holds a source; images persist as session blob references, not inline base64.
pub(super) async fn map_result(
    result: CallToolResult,
    store: &SessionStore,
) -> Result<ToolOutput, ToolError> {
    let mut sources = Vec::new();
    let content = result
        .content
        .into_iter()
        .enumerate()
        .map(|(index, block)| match block {
            ContentBlock::Image(image) => {
                sources.push((index, image.data));
                SanitizedBlock::Image(SanitizedImage {
                    kind: "image",
                    mime_type: image.mime_type,
                    meta: image.meta,
                    annotations: image.annotations,
                    status: ImageStatus::Failed {
                        message: "image not imported".into(),
                    },
                })
            }
            other => SanitizedBlock::NonImage(other),
        })
        .collect();
    let mut sanitized = SanitizedResult {
        result_type: result.result_type,
        structured_content: result.structured_content,
        is_error: result.is_error,
        meta: result.meta,
        content,
    };
    let mut images = Vec::new();
    let mut total_bytes = 0_u64;
    for (index, source) in sources {
        let SanitizedBlock::Image(block) = &mut sanitized.content[index] else {
            continue;
        };
        let subject = Subject::Label(format!("MCP image block {index}"));
        let reference = match import_image(
            &source,
            &block.mime_type,
            images.len(),
            total_bytes,
            store,
            &subject,
        )
        .await
        {
            Ok(reference) => reference,
            Err(error) => {
                block.status = ImageStatus::Failed {
                    message: error.to_string(),
                };
                return Err(error.with_result(output(&sanitized, images)?));
            }
        };
        total_bytes += reference.blob.bytes;
        block.status = ImageStatus::Imported {
            image: McpImage::new(index, &reference),
        };
        images.push(reference);
    }
    let output = output(&sanitized, images)?;
    if sanitized.is_error == Some(true) {
        Err(ToolError::with_output("MCP tool reported an error", output))
    } else {
        Ok(output)
    }
}

async fn import_image(
    source: &str,
    mime_type: &str,
    image_count: usize,
    total_bytes: u64,
    store: &SessionStore,
    subject: &Subject,
) -> Result<ImageRef, ToolError> {
    let context = |operation| {
        DiagnosticContext::new(operation, subject.clone()).effects(Effects::OutputIncomplete)
    };
    let import_context = context(Operation::Deserialize);
    let limit = usize::try_from(MAX_IMAGE_BYTES).expect("image limit fits usize");
    let bytes = decode_base64_bounded(source, limit).map_err(|error| {
        match error {
            MediaError::TooLarge => {
                ToolError::Failed("MCP images exceed attachment limits".to_owned())
            }
            other => ToolError::Failed(format!("invalid MCP image encoding: {other}")),
        }
        .context(import_context.clone())
    })?;
    let total_bytes = total_bytes + bytes.len() as u64;
    if image_count >= MAX_IMAGES_PER_SUBMISSION || total_bytes > MAX_IMAGE_BYTES_PER_SUBMISSION {
        return Err(
            ToolError::Failed("MCP images exceed attachment limits".to_owned())
                .context(import_context),
        );
    }
    // The declared type governs admission; bytes of another format are invalid
    // rather than silently re-typed.
    let declared = match mime_type {
        "image/png" => ImageFormat::Png,
        "image/jpeg" => ImageFormat::Jpeg,
        "image/gif" => ImageFormat::Gif,
        "image/webp" => ImageFormat::WebP,
        _ => {
            return Err(
                ToolError::Failed("unsupported MCP image type".into()).context(import_context)
            );
        }
    };
    let image = Image::new(bytes)
        .ok()
        .filter(|image| image.format() == declared)
        .ok_or_else(|| {
            ToolError::Failed(
                "invalid MCP image: data does not match its declared image type".into(),
            )
            .context(import_context)
        })?;
    store
        .store_image(None, &image)
        .await
        .map_err(|error| ToolError::from(error).context(context(Operation::StoreImage)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{tests::TestRuntime, tool::ToolOutput};
    use serde_json::json;

    /// A 1x1 PNG.
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a/1cAAAAASUVORK5CYII=";

    async fn map(value: &Value, runtime: &TestRuntime) -> Result<ToolOutput, ToolError> {
        map_result(
            serde_json::from_value(value.clone()).unwrap(),
            &runtime.store,
        )
        .await
    }

    fn failed(mapped: Result<ToolOutput, ToolError>) -> (String, ToolOutput) {
        let error = mapped.expect_err("MCP error must retain output");
        let message = error.to_string();
        let (_, output) = error.into_parts();
        (message, output.expect("MCP error must retain output"))
    }

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
            let mapped = map(&value, &runtime).await;
            let output = if flag == Some(true) {
                failed(mapped).1
            } else {
                mapped.unwrap()
            };
            assert_eq!(output.value, value);
            assert!(output.images.is_empty());
        }
    }

    #[tokio::test]
    async fn partial_image_failures_use_persistent_blobs_and_never_serialize_raw_sources() {
        for failed_index in 0..4 {
            let runtime = TestRuntime::new().await;
            let rejected = format!("RAW_REJECTED_MCP_IMAGE_{failed_index}!");
            let mut content = vec![json!({"type":"text","text":"before"})];
            for index in 0..3 {
                content.push(json!({"type":"image","mimeType":"image/png",
                    "data": if index == failed_index { &rejected } else { PNG },
                    "annotations":{"audience":["user"],"priority":0.75},
                    "_meta":{"vendor":{"index":index,"opaque":[true,null]}}
                }));
                content.push(json!({"type":"text","text":format!("after {index}")}));
            }
            let upstream = json!({"content":content,"isError":true,"resultType":"complete",
                "structuredContent":{"vendor":{"arbitrary":[1,"two",null]}},
                "_meta":{"vendor":{"opaque":true}}
            });
            // Even an entirely successful import keeps the upstream error flag.
            let (message, output) = failed(map(&upstream, &runtime).await);
            let imported = failed_index.min(3);
            assert_eq!(output.images.len(), imported);
            let serialized = serde_json::to_string(&output.value).unwrap();
            assert!(!serialized.contains(PNG));
            assert!(!serialized.contains(&rejected) && !message.contains(&rejected));
            let png = base64::engine::general_purpose::STANDARD
                .decode(PNG)
                .unwrap();
            for image in &output.images {
                assert_eq!(
                    (&image.file, image.format),
                    (&None, crate::media::ImageFormat::Png)
                );
                let stored = runtime
                    .store
                    .read_blob(&image.blob, MAX_IMAGE_BYTES as usize);
                assert_eq!(stored.await.unwrap(), png);
            }
            for index in 0..3 {
                let position = index * 2 + 1;
                let (block, source) = (
                    &output.value["content"][position],
                    &upstream["content"][position],
                );
                assert!(block.get("data").is_none());
                assert_eq!(block["annotations"], source["annotations"]);
                assert_eq!(block["_meta"], source["_meta"]);
                if index < imported {
                    assert!(block.get("imageError").is_none());
                    let stored = &output.images[index];
                    assert_eq!(
                        block["image"],
                        json!({
                            "sha256": stored.blob.sha256.to_string(),
                            "media_type": "image/png",
                            "name": format!("mcp-image-{position}.png"),
                            "bytes": stored.blob.bytes,
                        })
                    );
                } else if index == failed_index {
                    let error = block["imageError"].as_str().unwrap();
                    assert!(error.contains("invalid MCP image encoding"));
                } else {
                    assert_eq!(block["imageError"], "image not imported");
                }
                let text = position + 1;
                assert_eq!(output.value["content"][text], upstream["content"][text]);
            }
            assert_eq!(output.value["content"][0], upstream["content"][0]);
            for field in ["isError", "resultType", "structuredContent", "_meta"] {
                assert_eq!(output.value[field], upstream[field]);
            }
        }
    }

    #[tokio::test]
    async fn declared_image_type_governs_admission_and_must_match_the_bytes() {
        let runtime = TestRuntime::new().await;
        let jpeg = base64::engine::general_purpose::STANDARD.encode(b"\xff\xd8\xff\xe0 jpeg");
        let result = json!({"content":[{"type":"image","mimeType":"image/jpeg","data":jpeg}]});
        let output = map(&result, &runtime).await.unwrap();
        let [stored] = output.images.as_slice() else {
            panic!("expected the declared JPEG")
        };
        assert_eq!(stored.format, crate::media::ImageFormat::Jpeg);
        assert_eq!(
            output.value["content"][0]["image"],
            json!({
                "sha256": stored.blob.sha256.to_string(),
                "media_type": "image/jpeg",
                "name": "mcp-image-0.jpg",
                "bytes": stored.blob.bytes,
            })
        );
        // Valid PNG bytes under an unsupported or mismatched declaration are rejected.
        for (mime, message) in [
            ("image/svg+xml", "unsupported MCP image type"),
            (
                "image/jpeg",
                "invalid MCP image: data does not match its declared image type",
            ),
        ] {
            let result = json!({"content":[{"type":"image","mimeType":mime,"data":PNG}]});
            let (error, output) = failed(map(&result, &runtime).await);
            assert!(error.contains(message), "{error}");
            assert!(output.images.is_empty());
            assert_eq!(output.value["content"][0]["imageError"], error);
            assert_eq!(output.value["content"][0]["mimeType"], mime);
        }
    }

    #[tokio::test]
    async fn malformed_unsupported_and_oversized_images_fail_safely() {
        let runtime = TestRuntime::new().await;
        let oversized = "A".repeat((MAX_IMAGE_BYTES as usize).div_ceil(3) * 4 + 1);
        for (mime, data) in [
            ("image/png", "not base64!".to_owned()),
            ("image/svg+xml", "YWJj".to_owned()),
            ("image/png", oversized),
        ] {
            let result = json!({"content":[
                {"type":"image","mimeType":mime,"data":data},
                {"type":"image","mimeType":"image/png","data":"YWJj"},
                {"type":"text","text":"preserved"}
            ]});
            let (_, output) = failed(map(&result, &runtime).await);
            assert!(output.images.is_empty());
            for index in 0..2 {
                assert!(output.value["content"][index].get("data").is_none());
                assert!(output.value["content"][index]["imageError"].is_string());
            }
            assert_eq!(output.value["content"][2]["text"], "preserved");
        }
    }

    #[tokio::test]
    async fn image_storage_failure_preserves_the_envelope() {
        let runtime = TestRuntime::new().await;
        runtime.store.outputs().test_batch(
            "CREATE TEMP TRIGGER reject_mcp_image BEFORE INSERT ON blob \
             BEGIN SELECT RAISE(ABORT, 'rejected'); END;",
        );
        let upstream = json!({"content":[
            {"type":"image", "mimeType":"image/png", "data":PNG},
            {"type":"text", "text":"unchanged content"}
        ], "isError":false, "structuredContent":{"retained":true}});
        let error = map(&upstream, &runtime).await.unwrap_err();
        let context = error.diagnostic().context;
        assert_eq!(context.operation, Operation::StoreImage);
        assert_eq!(context.effects, Effects::OutputIncomplete);
        let (_, output) = failed(Err(error));
        assert!(output.images.is_empty());
        assert!(output.value["content"][0].get("data").is_none());
        assert_eq!(output.value["content"][1], upstream["content"][1]);
        assert_eq!(
            output.value["structuredContent"],
            upstream["structuredContent"]
        );
    }
}
