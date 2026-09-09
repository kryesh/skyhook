//! Bounded response decoding and optional readability extraction for `fetch`.

use dom_smoothie::{Config, Readability, TextMode};
use encoding_rs::{Encoding, UTF_8};
use schemars::JsonSchema;
use serde::Serialize;
use tokio::sync::Semaphore;

use crate::tool::ToolError;

// Keep permits inside the blocking task: cancelling a caller must not let it
// queue unlimited parsers while previously-started work is still running.
static EXTRACTORS: Semaphore = Semaphore::const_new(2);
const MAX_HTML_BYTES: usize = 10 * 1024 * 1024;
const MAX_HTML_ELEMENTS: usize = 50_000;

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct ExtractionMetadata {
    pub engine: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub byline: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub excerpt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub site_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

pub(super) struct ExtractedText {
    pub text: String,
    pub metadata: ExtractionMetadata,
}

fn media_type(content_type: Option<&str>) -> String {
    content_type
        .unwrap_or_default()
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

pub(super) fn is_html(bytes: &[u8], content_type: Option<&str>) -> bool {
    let mime = media_type(content_type);
    if matches!(mime.as_str(), "text/html" | "application/xhtml+xml") {
        return true;
    }
    // Respect an explicit non-HTML textual type: e.g. text/plain can be an HTML
    // source listing, and must not be silently interpreted as a document.
    if !matches!(mime.as_str(), "" | "application/octet-stream") {
        return false;
    }
    let prefix = String::from_utf8_lossy(&bytes[..bytes.len().min(1024)]);
    let prefix = prefix
        .trim_start_matches('\u{feff}')
        .trim_start()
        .to_ascii_lowercase();
    prefix.starts_with("<!doctype html")
        || prefix.starts_with("<html")
        || prefix.starts_with("<head")
        || prefix.starts_with("<body")
}

pub(super) fn is_textual(bytes: &[u8], content_type: Option<&str>) -> bool {
    let mime = media_type(content_type);
    if mime.starts_with("text/")
        || mime.ends_with("+json")
        || mime.ends_with("+xml")
        || matches!(
            mime.as_str(),
            "application/json"
                | "application/xml"
                | "application/javascript"
                | "application/x-javascript"
                | "application/x-www-form-urlencoded"
                | "application/graphql"
                | "application/sql"
                | "application/yaml"
                | "application/x-yaml"
        )
        || is_html(bytes, content_type)
    {
        return true;
    }
    if !mime.is_empty() {
        return false;
    }
    std::str::from_utf8(bytes).is_ok_and(|text| {
        !text
            .chars()
            .any(|ch| ch.is_control() && !matches!(ch, '\t' | '\n' | '\r'))
    })
}

fn charset_label(content_type: &str) -> Option<&str> {
    content_type.split(';').skip(1).find_map(|parameter| {
        let (name, value) = parameter.split_once('=')?;
        name.trim()
            .eq_ignore_ascii_case("charset")
            .then(|| value.trim().trim_matches(|ch| ch == '\'' || ch == '"'))
    })
}

// Encoding names are ASCII. Scan only initial meta tags, not arbitrary page
// text/scripts, for the common HTML5 charset and http-equiv charset forms.
fn html_encoding(bytes: &[u8]) -> Option<&'static Encoding> {
    let prefix = String::from_utf8_lossy(&bytes[..bytes.len().min(1024)]).to_ascii_lowercase();
    for fragment in prefix.split("<meta").skip(1) {
        let tag = fragment.split('>').next().unwrap_or_default();
        let Some((_, tail)) = tag.split_once("charset") else {
            continue;
        };
        let Some(tail) = tail.trim_start().strip_prefix('=') else {
            continue;
        };
        let label = tail.trim_start().trim_start_matches(['\'', '"']);
        let label = label
            .split(['\'', '"', ' ', '\t', '\r', '\n', ';', '/'])
            .next()?;
        if let Some(encoding) = Encoding::for_label(label.as_bytes()) {
            return Some(encoding);
        }
    }
    None
}

/// Decode the response entity, honoring a BOM, HTTP charset, then HTML meta
/// charset. Missing/unknown encodings use UTF-8 with replacement, never panic.
pub(super) fn decode(bytes: &[u8], content_type: Option<&str>) -> String {
    let encoding = Encoding::for_bom(bytes)
        .map(|(encoding, _)| encoding)
        .or_else(|| {
            charset_label(content_type.unwrap_or_default())
                .and_then(|label| Encoding::for_label(label.as_bytes()))
        })
        .or_else(|| {
            is_html(bytes, content_type)
                .then(|| html_encoding(bytes))
                .flatten()
        })
        .unwrap_or(UTF_8);
    encoding.decode(bytes).0.into_owned()
}

pub(super) async fn extract(html: String, url: String) -> Result<ExtractedText, ToolError> {
    if html.len() > MAX_HTML_BYTES {
        return Err(ToolError::Failed(
            "text extraction input exceeds 10 MiB; fetch without text or use a smaller response"
                .into(),
        ));
    }
    let permit = EXTRACTORS
        .acquire()
        .await
        .map_err(|_| ToolError::Failed("text extractor unavailable".into()))?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let config = Config {
            text_mode: TextMode::Formatted,
            max_elements_to_parse: MAX_HTML_ELEMENTS,
            ..Config::default()
        };
        let mut reader = Readability::new(html, Some(&url), Some(config))
            .map_err(extraction_error)?;
        let article = reader.parse().map_err(extraction_error)?;
        let text = article.text_content.trim().to_owned();
        if text.is_empty() {
            return Err(ToolError::Failed("text extraction failed: no readable content; retry with text:false to inspect the response".into()));
        }
        Ok(ExtractedText {
            text,
            metadata: ExtractionMetadata {
                engine: "dom_smoothie".into(),
                title: article.title,
                byline: article.byline,
                excerpt: article.excerpt,
                site_name: article.site_name,
                language: article.lang,
            },
        })
    })
    .await
    .map_err(|_| ToolError::Failed("text extraction worker failed".into()))?
}

fn extraction_error(error: dom_smoothie::ReadabilityError) -> ToolError {
    ToolError::Failed(format!(
        "text extraction failed: {error}; retry with text:false to inspect the response"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_text_without_interpreting_plain_html_sources() {
        assert!(is_html(b"<!doctype HTML><html>", None));
        assert!(is_html(b"", Some("Text/HTML; charset=UTF-8")));
        assert!(!is_html(b"<html>literal source</html>", Some("text/plain")));
        assert!(is_textual(b"{}", Some("application/problem+json")));
        assert!(is_textual(b"hello", None));
        assert!(!is_textual(b"hello\0world", None));
        assert!(!is_textual(b"%PDF-1.7", Some("application/pdf")));
        assert!(!is_textual(b"hello", Some("application/octet-stream")));
    }

    #[test]
    fn decodes_http_meta_and_bom_encodings() {
        assert_eq!(
            decode(b"caf\xe9", Some("text/plain; CHARSET=\"windows-1252\"")),
            "caf\u{e9}"
        );
        assert!(
            decode(
                b"<html><meta charset=windows-1252><p>caf\xe9</p></html>",
                Some("text/html")
            )
            .contains("caf\u{e9}")
        );
        assert_eq!(
            decode(b"\xff\xfeh\0i\0", Some("text/plain; charset=windows-1252")),
            "hi"
        );
        assert_eq!(
            decode(b"hello", Some("text/plain; charset=not-real")),
            "hello"
        );
    }

    #[tokio::test]
    async fn extracts_readable_article_without_scripts_or_navigation() {
        let paragraph = "This is the important article body, with meaningful details about the topic. Readers should receive the actual article rather than navigation links, advertisements, styles, or executable code. ";
        let html = format!(
            "<!doctype html><html lang='en'><head><title>A useful article</title><style>.hidden {{color:red}}</style></head><body><nav><a href='/login'>NAVIGATION_SENTINEL</a></nav><main><article><h1>A useful article</h1><p>{}</p><p>A separate paragraph concludes the article with additional useful details.</p></article></main><script>SCRIPT_SENTINEL</script></body></html>",
            paragraph.repeat(8)
        );
        let result = extract(html, "https://example.com/article".into())
            .await
            .unwrap();
        assert!(result.text.contains("important article body"));
        assert!(result.text.contains("A separate paragraph"));
        assert!(!result.text.contains("SCRIPT_SENTINEL"));
        assert!(!result.text.contains("NAVIGATION_SENTINEL"));
        assert!(!result.text.contains("<p>"));
        assert_eq!(result.metadata.title, "A useful article");
        assert_eq!(result.metadata.engine, "dom_smoothie");
    }

    #[tokio::test]
    async fn malformed_html_is_tolerated_and_oversized_input_is_rejected() {
        let html = format!(
            "<html><title>Broken</title><article><p>{}",
            "Readable content, even in an unclosed HTML article. ".repeat(30)
        );
        assert!(
            extract(html, "https://example.com/".into())
                .await
                .unwrap()
                .text
                .contains("Readable content")
        );
        assert!(
            extract(
                "x".repeat(MAX_HTML_BYTES + 1),
                "https://example.com/".into()
            )
            .await
            .is_err()
        );
    }
}
