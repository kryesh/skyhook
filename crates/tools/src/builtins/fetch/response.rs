//! Response bodies as the call presents them.
use base64::{Engine as _, engine::general_purpose::STANDARD};

use super::progress::FetchProgress;
use super::{ResponseBody, ResponseFormat, entity, html};
use skyhook_worker::diagnostic::{Operation, Subject};
use skyhook_worker::op::HttpRequestUrl;

pub(super) async fn response_body(
    bytes: Vec<u8>,
    content_type: Option<&str>,
    url: &HttpRequestUrl,
    format: ResponseFormat,
    progress: &mut FetchProgress,
) -> ResponseBody {
    use entity::{ContentClass, ResponseEntity};

    if bytes.is_empty() {
        return ResponseBody::Empty;
    }
    if format == ResponseFormat::Base64 {
        return ResponseBody::Base64 {
            data: STANDARD.encode(bytes),
        };
    }
    let entity = ResponseEntity::new(bytes, content_type, format);
    match (format, entity.class()) {
        (ResponseFormat::Text, _) => ResponseBody::Text {
            text: entity.decode(),
        },
        (_, ContentClass::Html) => {
            progress.step(Operation::Capture, Subject::Label("HTML extraction".into()));
            html::extract(entity, url, html::Limits::DEFAULT).await
        }
        (_, ContentClass::Binary) => ResponseBody::Base64 {
            data: STANDARD.encode(entity.bytes()),
        },
        (_, ContentClass::Text) => json_or_text(entity.decode()).await,
    }
}

async fn json_or_text(text: String) -> ResponseBody {
    // A body can be large, so it is parsed off the async workers.
    let parsed = tokio::task::spawn_blocking(move || {
        let json = crate::output::json::parse_text(&text);
        (text, json)
    })
    .await;
    let (text, json) = parsed.unwrap_or_else(|error| std::panic::resume_unwind(error.into_panic()));
    match json {
        Some(value) => ResponseBody::Json { value },
        None => ResponseBody::Text { text },
    }
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use super::*;
    use crate::error::CallError;

    #[tokio::test]
    async fn response_formats_decode_or_extract_by_format() {
        use ResponseFormat::{Auto, Readable, Text};
        let url = HttpRequestUrl::parse("https://example.org/article").unwrap();
        let html = format!(
            "<!doctype html><html><title>Sniffed article</title><article><p>{}</p></article></html>",
            "Meaningful article details, independent of media type spelling. ".repeat(30)
        );
        let url = &url;
        let read = |bytes: &[u8], content_type, format| {
            let bytes = bytes.to_vec();
            async move {
                let mut progress = FetchProgress::new();
                let body = response_body(bytes, content_type, url, format, &mut progress).await;
                let step = progress.failure(CallError::failed("probe")).diagnostic();
                (body, step.context.operation)
            }
        };
        let body = async |bytes, content_type, format| read(bytes, content_type, format).await.0;
        assert_matches!(
            body(&[0, 255, 3], Some("application/octet-stream"), Readable).await,
            ResponseBody::Base64 { data } if data == "AP8D"
        );
        assert_matches!(
            body(&[0xe9], Some("text/plain; charset=windows-1252"), Auto).await,
            ResponseBody::Text { text } if text == "\u{e9}"
        );
        assert_matches!(
            body(&[], Some("text/html"), Text).await,
            ResponseBody::Empty
        );
        let html = html.as_bytes();
        assert_matches!(
            body(html, Some("text/html"), Text).await,
            ResponseBody::Text { text } if text.as_bytes() == html
        );
        // Sniffed HTML extracts without a declared media type, and `readable`
        // also reads HTML served as another one.
        for (content_type, format) in [(None, Auto), (Some("text/plain"), Readable)] {
            let (ResponseBody::Page(page), Operation::Capture) =
                read(html, content_type, format).await
            else {
                panic!("expected a page extracted as a capture step for {content_type:?}")
            };
            assert!(page.markdown.contains("Meaningful article details"));
            assert_eq!(page.title, "Sniffed article");
        }
        assert_matches!(
            body(html, Some("text/plain"), Auto).await,
            ResponseBody::Text { .. }
        );
    }
}
