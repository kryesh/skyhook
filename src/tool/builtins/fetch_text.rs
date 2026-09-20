//! Bounded response decoding and optional readability extraction for `fetch`.

use dom_smoothie::{Config, Readability, TextMode};
use encoding_rs::{Encoding, UTF_8};
use schemars::JsonSchema;
use serde::Serialize;
use tokio::sync::Semaphore;

use super::fetch::HttpRequestUrl;

// Keep permits inside the blocking task: cancelling a caller must not let it
// queue unlimited parsers while previously-started work is still running.
static EXTRACTORS: Semaphore = Semaphore::const_new(2);
const MAX_RAW_HTML_BYTES: usize = 10 * 1024 * 1024;
const MAX_DECODED_HTML_BYTES: usize = 10 * 1024 * 1024;
const MAX_HTML_ELEMENTS: usize = 50_000;

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub(super) struct ExtractionMetadata {
    pub engine: String,
    pub title: String,
    pub byline: Option<String>,
    pub excerpt: Option<String>,
    pub site_name: Option<String>,
    pub language: Option<String>,
}

pub(super) struct ExtractedText {
    pub text: String,
    pub metadata: ExtractionMetadata,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ContentClass {
    Html,
    Text,
    Binary,
}

/// Owns the bytes and their one classification/encoding decision. Raw response
/// headers remain with the response envelope; this is not a DOM validity proof.
pub(super) struct ResponseEntity {
    bytes: Vec<u8>,
    class: ContentClass,
    encoding: &'static Encoding,
}

impl ResponseEntity {
    pub(super) fn new(bytes: Vec<u8>, content_type: Option<&str>) -> Self {
        let mut parts = content_type.unwrap_or_default().split(';');
        let mime = parts.next().unwrap_or_default().trim().to_ascii_lowercase();
        // Keep the first charset parameter, including an unknown label: a later
        // parameter must not override it. Match the existing HTTP policy.
        let http_encoding = parts
            .find_map(|parameter| {
                let (name, value) = parameter.split_once('=')?;
                name.trim()
                    .eq_ignore_ascii_case("charset")
                    .then(|| value.trim().trim_matches(|ch| ch == '\'' || ch == '"'))
            })
            .and_then(|label| Encoding::for_label(label.as_bytes()));
        let class = classify(&bytes, &mime);
        let encoding = Encoding::for_bom(&bytes)
            .map(|(encoding, _)| encoding)
            .or(http_encoding)
            .or_else(|| {
                (class == ContentClass::Html)
                    .then(|| html_encoding(&bytes))
                    .flatten()
            })
            .unwrap_or(UTF_8);
        Self {
            bytes,
            class,
            encoding,
        }
    }

    pub(super) fn class(&self) -> ContentClass {
        self.class
    }

    pub(super) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Consume the entity using its retained BOM > HTTP > HTML meta > UTF-8
    /// decision. Unknown encodings and malformed bytes retain replacement policy.
    pub(super) fn decode(self) -> String {
        self.encoding.decode(&self.bytes).0.into_owned()
    }
}

fn classify(bytes: &[u8], mime: &str) -> ContentClass {
    if matches!(mime, "text/html" | "application/xhtml+xml")
        || (matches!(mime, "" | "application/octet-stream") && sniff_html(bytes))
    {
        return ContentClass::Html;
    }
    // Explicit non-HTML text is a source listing, never a document to extract.
    if mime.starts_with("text/")
        || mime.ends_with("+json")
        || mime.ends_with("+xml")
        || matches!(
            mime,
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
        || (mime.is_empty()
            && std::str::from_utf8(bytes).is_ok_and(|text| {
                !text
                    .chars()
                    .any(|ch| ch.is_control() && !matches!(ch, '\t' | '\n' | '\r'))
            }))
    {
        ContentClass::Text
    } else {
        ContentClass::Binary
    }
}

fn sniff_html(bytes: &[u8]) -> bool {
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

/// An owned HTML entity within both extraction budgets, bound to its admitted
/// request URL. Neither successful parsing nor total heap use is proven.
pub(super) struct ExtractableHtml {
    html: String,
    url: HttpRequestUrl,
}

impl ExtractableHtml {
    /// Callers admit only HTML-classified entities; `None` is a budget rejection.
    pub(super) fn admit(entity: ResponseEntity, url: &HttpRequestUrl) -> Option<Self> {
        if entity.bytes.len() > MAX_RAW_HTML_BYTES {
            return None;
        }
        let html = entity.decode();
        // Raw bytes have been dropped before any semaphore wait or worker queue.
        (html.len() <= MAX_DECODED_HTML_BYTES).then(|| Self {
            html,
            url: url.clone(),
        })
    }
}

/// Failure details are never surfaced: callers report one classified diagnostic.
pub(super) async fn extract(input: ExtractableHtml) -> Option<ExtractedText> {
    let permit = EXTRACTORS.acquire().await.ok()?;
    tokio::task::spawn_blocking(move || {
        // A cancelled waiter cannot release parser capacity while this worker runs.
        let _permit = permit;
        parse(input)
    })
    .await
    .ok()?
}

fn parse(input: ExtractableHtml) -> Option<ExtractedText> {
    let config = Config {
        text_mode: TextMode::Formatted,
        max_elements_to_parse: MAX_HTML_ELEMENTS,
        ..Config::default()
    };
    let mut reader = Readability::new(input.html, Some(input.url.as_str()), Some(config)).ok()?;
    let article = reader.parse().ok()?;
    let text = article.text_content.trim().to_owned();
    if text.is_empty() {
        return None;
    }
    Some(ExtractedText {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entity(bytes: &[u8], content_type: Option<&str>) -> ResponseEntity {
        ResponseEntity::new(bytes.to_vec(), content_type)
    }

    fn admitted(html: String) -> ExtractableHtml {
        let url = HttpRequestUrl::parse("https://example.com/article").unwrap();
        ExtractableHtml::admit(entity(html.as_bytes(), Some("text/html")), &url).unwrap()
    }

    #[test]
    fn classifies_text_without_interpreting_plain_html_sources() {
        use ContentClass::{Binary, Html, Text};
        let cases: &[(&[u8], Option<&str>, ContentClass)] = &[
            (b"<!doctype HTML><html>", None, Html),
            (b"", Some("Text/HTML; charset=UTF-8"), Html),
            (b"<html>literal source</html>", Some("text/plain"), Text),
            (b"{}", Some("application/problem+json"), Text),
            (b"<x/>", Some("application/problem+xml"), Text),
            (b"hello", None, Text),
            (b"hello\0world", None, Binary),
            (b"hello\nworld\t", None, Text),
            (b"\xff", None, Binary),
            (b"%PDF-1.7", Some("application/pdf"), Binary),
            (b"hello", Some("application/octet-stream"), Binary),
            (
                b"<html>article</html>",
                Some("application/octet-stream"),
                Html,
            ),
            (b"\xef\xbb\xbf  <HEAD>", None, Html),
            (b"<html>source</html>", Some("application/json"), Text),
            (b"<html>source</html>", Some("application/pdf"), Binary),
        ];
        for (bytes, content_type, class) in cases {
            assert_eq!(
                entity(bytes, *content_type).class(),
                *class,
                "{content_type:?}"
            );
        }
    }

    #[test]
    fn decodes_http_meta_and_bom_encodings() {
        let html: &[u8] = b"<html><meta charset=windows-1252><p>caf\xe9</p></html>";
        let http_equiv: &[u8] = b"<html><meta http-equiv='Content-Type' content='text/html; charset=windows-1252'><p>caf\xe9</p></html>";
        let bom_html = [&b"\xef\xbb\xbf"[..], html].concat();
        let late_meta = [&[b' '; 1024][..], html].concat();
        let (latin, replaced) = ("caf\u{e9}", "caf\u{fffd}");
        for (bytes, content_type, expected) in [
            (html, "text/html", latin),
            (http_equiv, "text/html", latin),
            // HTTP overrides HTML meta; a BOM overrides both HTTP and meta.
            (html, "text/html; charset=utf-8", replaced),
            (&bom_html, "text/html; charset=windows-1252", replaced),
            // Only the first HTTP charset is considered, even when its label is unknown.
            (html, "text/html; charset=unknown; charset=utf-8", latin),
            (html, "text/plain", replaced),
            (html, "application/octet-stream", latin),
            (&late_meta, "text/html", replaced),
        ] {
            assert!(
                entity(bytes, Some(content_type))
                    .decode()
                    .contains(expected),
                "{content_type}"
            );
        }
        for (bytes, content_type, expected) in [
            (
                &b"caf\xe9"[..],
                "text/plain; CHARSET=\"windows-1252\"",
                "caf\u{e9}",
            ),
            (b"\xff\xfeh\0i\0", "text/plain; charset=windows-1252", "hi"),
            (b"hello", "text/plain; charset=not-real", "hello"),
            (b"\xff", "text/plain; charset=unknown", "\u{fffd}"),
        ] {
            assert_eq!(entity(bytes, Some(content_type)).decode(), expected);
        }
    }

    #[test]
    fn extraction_admission_has_independent_raw_and_decoded_exact_limits() {
        let url = HttpRequestUrl::parse("https://example.com/base/page#fragment").unwrap();
        let html = |bytes: Vec<u8>| ResponseEntity::new(bytes, Some("text/html"));
        let exact = ExtractableHtml::admit(html(vec![b'a'; MAX_RAW_HTML_BYTES]), &url).unwrap();
        assert_eq!(exact.html.len(), MAX_DECODED_HTML_BYTES);
        assert_eq!(exact.url.as_str(), "https://example.com/base/page");
        let oversized = html(vec![b'a'; MAX_RAW_HTML_BYTES + 1]);
        assert!(ExtractableHtml::admit(oversized, &url).is_none());
        // Replacement characters expand below the raw limit but past the decoded one.
        let replacements = html(vec![0xff; MAX_DECODED_HTML_BYTES / 3 + 1]);
        assert!(replacements.bytes().len() <= MAX_RAW_HTML_BYTES);
        assert!(ExtractableHtml::admit(replacements, &url).is_none());
    }

    #[test]
    fn extraction_metadata_keeps_unknown_fields_as_null() {
        let metadata = ExtractionMetadata {
            engine: "dom_smoothie".into(),
            title: String::new(),
            byline: None,
            excerpt: None,
            site_name: None,
            language: None,
        };
        assert_eq!(
            serde_json::to_value(metadata).unwrap(),
            serde_json::json!({
                "engine":"dom_smoothie", "title":"", "byline":null,
                "excerpt":null, "site_name":null, "language":null
            })
        );
    }

    #[tokio::test]
    async fn extracts_readable_articles_tolerating_malformed_html_and_rejecting_oversized_dom() {
        let paragraph = "This is the important article body, with meaningful details about the topic. Readers should receive the actual article rather than navigation links, advertisements, styles, or executable code. ";
        let html = format!(
            "<!doctype html><html lang='en'><head><title>A useful article</title><style>.hidden {{color:red}}</style></head><body><nav><a href='/login'>NAVIGATION_SENTINEL</a></nav><main><article><h1>A useful article</h1><p>{}</p><p>A separate paragraph concludes the article with additional useful details.</p></article></main><script>SCRIPT_SENTINEL</script></body></html>",
            paragraph.repeat(8)
        );
        let result = extract(admitted(html)).await.unwrap();
        for expected in ["important article body", "A separate paragraph"] {
            assert!(result.text.contains(expected));
        }
        for absent in ["SCRIPT_SENTINEL", "NAVIGATION_SENTINEL", "<p>"] {
            assert!(!result.text.contains(absent));
        }
        assert_eq!(result.metadata.title, "A useful article");
        assert_eq!(result.metadata.engine, "dom_smoothie");
        let malformed = format!(
            "<html><title>Broken</title><article><p>{}",
            "Readable content, even in an unclosed HTML article. ".repeat(30)
        );
        assert!(
            extract(admitted(malformed))
                .await
                .unwrap()
                .text
                .contains("Readable content")
        );
        let oversized = admitted("<i></i>".repeat(MAX_HTML_ELEMENTS + 1));
        assert!(extract(oversized).await.is_none());
    }
}
