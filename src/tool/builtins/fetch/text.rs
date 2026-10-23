//! Bounded response decoding and HTML-to-markdown extraction for `fetch`.

use std::panic::AssertUnwindSafe;

use dom_query::{Document, NodeRef};
use dom_smoothie::Readability;
use encoding_rs::{Encoding, UTF_8};
use html5ever::{LocalName, local_name};
use schemars::JsonSchema;
use serde::Serialize;
use tokio::sync::Semaphore;

use super::budget::{self, Budget, Spent};
use super::markdown::markdown;
use super::validation::HttpRequestUrl;
use super::{ResponseBody, ResponseFormat};

// Keep permits inside the blocking task: cancelling a caller must not let it
// queue unlimited parsers while previously-started work is still running.
static EXTRACTORS: Semaphore = Semaphore::const_new(2);
const MAX_RAW_HTML_BYTES: usize = 10 * 1024 * 1024;
const MAX_DECODED_HTML_BYTES: usize = 10 * 1024 * 1024;

#[serde_with::skip_serializing_none]
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub(super) struct Page {
    pub title: String,
    pub byline: Option<String>,
    pub site_name: Option<String>,
    pub language: Option<String>,
    pub scope: Scope,
    pub markdown: String,
}

/// What `markdown` covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(super) enum Scope {
    /// The main article.
    Article,
    /// The page body without scripts, navigation, form controls, footers and hidden elements.
    Page,
}

impl Scope {
    /// Markdown never holds scripts or styles; page markdown also leaves out site
    /// chrome, form controls and what the page hides.
    pub(super) fn skips(self, element: &NodeRef, tag: &LocalName) -> bool {
        matches!(
            *tag,
            local_name!("script")
                | local_name!("style")
                | local_name!("noscript")
                | local_name!("template")
        ) || (self == Self::Page
            && (matches!(
                *tag,
                local_name!("nav")
                    | local_name!("aside")
                    | local_name!("footer")
                    | local_name!("button")
                    | local_name!("input")
                    | local_name!("select")
                    | local_name!("textarea")
                    | local_name!("svg")
                    | local_name!("iframe")
                    | local_name!("dialog")
            ) || hidden(element)))
    }
}

/// Whether the element hides itself: by attribute, or by an inline style whose
/// last `display` is `none` or last `visibility` is `hidden` or `collapse`.
fn hidden(element: &NodeRef) -> bool {
    if element.has_attr("hidden")
        || (element.attr("aria-hidden"))
            .is_some_and(|hidden| hidden.trim().eq_ignore_ascii_case("true"))
    {
        return true;
    }
    let Some(style) = element.attr("style") else {
        return false;
    };
    // Whether each property's winning declaration is important, and hides: an
    // important one wins over any other, else the last does.
    let (mut display, mut visibility) = ((false, false), (false, false));
    for declaration in style.split(';') {
        let Some((property, value)) = declaration.split_once(':') else {
            continue;
        };
        let (value, important) = match value.split_once('!') {
            Some((value, priority)) if priority.trim().eq_ignore_ascii_case("important") => {
                (value, true)
            }
            Some(_) => continue,
            None => (value, false),
        };
        let is = |keyword: &str| value.trim().eq_ignore_ascii_case(keyword);
        let (winner, hides) = match property.trim().to_ascii_lowercase().as_str() {
            "display" => (&mut display, is("none")),
            "visibility" => (&mut visibility, is("hidden") || is("collapse")),
            _ => continue,
        };
        if important >= winner.0 {
            *winner = (important, hides);
        }
    }
    display.1 || visibility.1
}

/// Why HTML could not be read as markdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(super) enum Unreadable {
    /// Over the extraction size, element, nesting or output limits.
    TooLarge,
    /// No text, as on pages rendered by JavaScript.
    NoText,
    ParserFailed,
}

impl From<Spent> for Unreadable {
    fn from(Spent: Spent) -> Self {
        Self::TooLarge
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ContentClass {
    Html,
    Text,
    Binary,
}

/// Owns the bytes and their one classification/encoding decision.
pub(super) struct ResponseEntity {
    bytes: Vec<u8>,
    class: ContentClass,
    encoding: &'static Encoding,
}

impl ResponseEntity {
    pub(super) fn new(bytes: Vec<u8>, content_type: Option<&str>, format: ResponseFormat) -> Self {
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
        let declared = matches!(mime.as_str(), "text/html" | "application/xhtml+xml");
        // The opening as text, under a BOM's encoding, else the HTTP charset's: any
        // other the meta charset can name reads markup and control bytes alike.
        let head = &bytes[..bytes.len().min(1024)];
        let head = http_encoding
            .unwrap_or(UTF_8)
            .decode(head)
            .0
            .to_ascii_lowercase();
        let class = classify(&bytes, &head, &mime, declared, format);
        let encoding = Encoding::for_bom(&bytes)
            .map(|(encoding, _)| encoding)
            .or(http_encoding)
            .or_else(|| {
                (declared || class == ContentClass::Html)
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

fn classify(
    bytes: &[u8],
    head: &str,
    mime: &str,
    declared: bool,
    format: ResponseFormat,
) -> ContentClass {
    // A body opening with bytes no text holds is binary, whatever it was served as.
    // Otherwise `readable` reads a body declared HTML as HTML, and any other that
    // sniffs as HTML. Otherwise a body declared HTML is HTML when it opens with
    // markup, so an API that labels JSON as HTML is still read as JSON, and any
    // other is HTML only unlabelled and sniffing as HTML: explicit non-HTML text is
    // a source listing, never a document to extract.
    if binary(head) {
        return ContentClass::Binary;
    }
    let html = match (format, declared) {
        (ResponseFormat::Readable, true) => true,
        (ResponseFormat::Readable, false) => sniff_html(head),
        (_, true) => markup(head).is_some(),
        (_, false) => matches!(mime, "" | "application/octet-stream") && sniff_html(head),
    };
    if html {
        return ContentClass::Html;
    }
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

/// Whether `head` holds a binary data byte of the WHATWG MIME sniffing standard.
fn binary(head: &str) -> bool {
    head.contains(|ch| matches!(ch, '\0'..='\x08' | '\x0b' | '\x0e'..='\x1a' | '\x1c'..='\x1f'))
}

/// What follows the `<` the body's opening `head` opens with, after any XML
/// declaration and whitespace.
fn markup(head: &str) -> Option<&str> {
    let mut head = head.trim_start();
    if let Some((_, rest)) = head
        .strip_prefix("<?xml")
        .and_then(|xml| xml.split_once("?>"))
    {
        head = rest.trim_start();
    }
    head.strip_prefix('<')
}

/// Whether the body's opening `head` opens with one of the HTML signatures of the
/// WHATWG MIME sniffing standard, after any XML declaration.
fn sniff_html(head: &str) -> bool {
    const TAGS: &[&str] = &[
        "!doctype html",
        "html",
        "head",
        "script",
        "iframe",
        "h1",
        "div",
        "font",
        "table",
        "a",
        "style",
        "title",
        "b",
        "body",
        "br",
        "p",
    ];
    let Some(tag) = markup(head) else {
        return false;
    };
    // A comment's signature is its opener alone; a tag's is its name, ended.
    tag.starts_with("!--")
        || TAGS.iter().any(|name| {
            tag.strip_prefix(name).is_some_and(|rest| {
                rest.starts_with(|ch: char| ch == '>' || ch.is_ascii_whitespace())
            })
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

/// Read HTML as markdown: the readable article when there is one, otherwise the
/// page body. A body that cannot be read keeps its decoded source.
pub(super) async fn extract(entity: ResponseEntity, url: &HttpRequestUrl) -> ResponseBody {
    let html = match admit(entity) {
        Ok(html) => html,
        Err(text) => {
            return ResponseBody::Unreadable {
                reason: Unreadable::TooLarge,
                text,
            };
        }
    };
    let url = url.clone();
    let permit = EXTRACTORS
        .acquire()
        .await
        .expect("the extractor semaphore is never closed");
    let worker = tokio::task::spawn_blocking(move || {
        // A cancelled waiter cannot release parser capacity while this worker runs.
        let _permit = permit;
        let page = std::panic::catch_unwind(AssertUnwindSafe(|| parse(&html, &url)));
        (html, page.unwrap_or(Err(Unreadable::ParserFailed)))
    });
    let (html, page) = worker
        .await
        .unwrap_or_else(|error| std::panic::resume_unwind(error.into_panic()));
    match page {
        Ok(page) => ResponseBody::Page(page),
        Err(reason) => ResponseBody::Unreadable { reason, text: html },
    }
}

/// The decoded HTML, or `Err` with it when it is over either byte budget. The
/// raw bytes are dropped here, before any semaphore wait or worker queue.
fn admit(entity: ResponseEntity) -> Result<String, String> {
    let within_raw = entity.bytes.len() <= MAX_RAW_HTML_BYTES;
    let html = entity.decode();
    if within_raw && html.len() <= MAX_DECODED_HTML_BYTES {
        Ok(html)
    } else {
        Err(html)
    }
}

fn parse(html: &str, url: &HttpRequestUrl) -> Result<Page, Unreadable> {
    let budget = Budget::default();
    let (source, base) = source(html, url.url(), &budget)?;
    let readability = |document| {
        Readability::with_document(document, None, None).map_err(|_| Unreadable::ParserFailed)
    };
    let page = readability(source)?;
    // Readability rewrites the document it parses, so an article is parsed from a
    // copy and the page falls back to the source.
    if page.is_probably_readable()
        && let Ok(article) = readability(page.doc.clone())?.parse()
        && let Ok(markdown) = article_markdown(&article.content, &base)
    {
        return Ok(Page {
            title: article.title,
            byline: article.byline,
            site_name: article.site_name,
            language: article.lang,
            scope: Scope::Article,
            markdown,
        });
    }
    let metadata = page.get_article_metadata(page.parse_json_ld());
    let body = page.doc.body().ok_or(Unreadable::NoText)?;
    Ok(Page {
        title: metadata.title,
        byline: metadata.byline,
        site_name: metadata.site_name,
        language: metadata.lang,
        scope: Scope::Page,
        markdown: markdown(body, Scope::Page, &base, &budget)?,
    })
}

/// The document without its `<base>`, and the base its links resolve against.
/// Readability gets no URL, so links are resolved only by `markdown`.
fn source(
    html: &str,
    url: &reqwest::Url,
    budget: &Budget,
) -> Result<(Document, reqwest::Url), Unreadable> {
    let source = budget::parse(html, budget)?;
    let base = (source.select("base[href]").attr("href"))
        .and_then(|href| url.join(&href).ok())
        .unwrap_or_else(|| url.clone());
    source.select("base").remove();
    Ok((source, base))
}

/// Readability returns the article as HTML, read again under a budget of its own.
fn article_markdown(html: &str, base: &reqwest::Url) -> Result<String, Unreadable> {
    let budget = Budget::default();
    let article = budget::parse(html, &budget)?;
    let body = article.body().ok_or(Unreadable::NoText)?;
    markdown(body, Scope::Article, base, &budget)
}

#[cfg(test)]
mod tests {
    use super::super::budget::{MAX_HTML_DEPTH, MAX_HTML_ELEMENTS};
    use super::*;

    fn entity(bytes: &[u8], content_type: Option<&str>) -> ResponseEntity {
        ResponseEntity::new(bytes.to_vec(), content_type, ResponseFormat::Auto)
    }

    async fn read(html: String) -> ResponseBody {
        let url = HttpRequestUrl::parse("https://example.com/article").unwrap();
        extract(entity(html.as_bytes(), Some("text/html")), &url).await
    }

    #[test]
    fn classifies_text_without_interpreting_plain_html_sources() {
        use ContentClass::{Binary, Html, Text};
        let cases: &[(&[u8], Option<&str>, ContentClass)] = &[
            (b"<!doctype HTML><html>", None, Html),
            (b"<P>text", Some("Text/HTML; charset=UTF-8"), Html),
            (
                b"<?xml version='1.0'?>\n<html>",
                Some("application/xhtml+xml"),
                Html,
            ),
            (br#"{"a":[1]}"#, Some("text/html"), Text),
            (b"\n<main><h2>partial</h2>", Some("text/html"), Html),
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
            (b"\xff\xfe<\0p\0>\0", Some("text/html"), Html),
            (b"<html>source</html>", Some("application/json"), Text),
            (b"<html>source</html>", Some("application/pdf"), Binary),
            // A comment opens HTML whatever follows its opener.
            (
                b"<!--generated--><html>",
                Some("application/octet-stream"),
                Html,
            ),
            (b"<!---->", None, Html),
            (b"<!-x>", None, Text),
        ];
        for (bytes, content_type, class) in cases {
            assert_eq!(
                entity(bytes, *content_type).class(),
                *class,
                "{content_type:?}"
            );
        }
        // `readable` reads HTML whatever media type it was served as, and whatever a
        // body declared HTML opens with, unless it is binary.
        for (bytes, content_type, class) in [
            (&b"<html>article</html>"[..], "text/plain", Html),
            (br#"{"a":[1]}"#, "text/html", Html),
            (b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR", "text/html", Binary),
        ] {
            let readable =
                ResponseEntity::new(bytes.to_vec(), Some(content_type), ResponseFormat::Readable);
            assert_eq!(readable.class(), class, "{content_type}");
        }
    }

    #[test]
    fn decodes_http_meta_and_bom_encodings() {
        let html: &[u8] = b"<html><meta charset=windows-1252><p>caf\xe9</p></html>";
        let http_equiv: &[u8] = b"<meta http-equiv='Content-Type' content='text/html; charset=windows-1252'><p>caf\xe9</p>";
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
        let html =
            |bytes: Vec<u8>| ResponseEntity::new(bytes, Some("text/html"), ResponseFormat::Auto);
        let exact = admit(html(vec![b'a'; MAX_RAW_HTML_BYTES])).unwrap();
        assert_eq!(exact.len(), MAX_DECODED_HTML_BYTES);
        // Over either budget, the decoded source is kept.
        let oversized = admit(html(vec![b'a'; MAX_RAW_HTML_BYTES + 1])).unwrap_err();
        assert_eq!(oversized.len(), MAX_RAW_HTML_BYTES + 1);
        // Replacement characters expand below the raw limit but past the decoded one.
        let replacements = html(vec![0xff; MAX_DECODED_HTML_BYTES / 3 + 1]);
        assert!(replacements.bytes().len() <= MAX_RAW_HTML_BYTES);
        assert!(admit(replacements).is_err());
    }

    #[tokio::test]
    async fn reads_articles_then_pages_and_keeps_the_source_of_unreadable_html() {
        let paragraph = "This is the important article body, with meaningful details about the topic. Readers should receive the actual article rather than navigation links, advertisements, styles, or executable code. ";
        let article = format!(
            "<!doctype html><html lang='en'><head><title>A useful article</title><style>.hidden {{color:red}}</style></head><body><nav><a href='/login'>NAVIGATION_SENTINEL</a></nav><main><article><h1>A useful article</h1><p>{}</p><p>A separate paragraph links the <a href='/docs'>docs</a>.</p></article></main><script>SCRIPT_SENTINEL</script></body></html>",
            paragraph.repeat(8)
        );
        let ResponseBody::Page(page) = read(article).await else {
            panic!("expected an article")
        };
        assert_eq!(
            (page.scope, page.title.as_str()),
            (Scope::Article, "A useful article")
        );
        assert_eq!(page.language.as_deref(), Some("en"));
        for expected in [
            "important article body",
            "about the topic. Readers",
            "[docs](https://example.com/docs)",
        ] {
            assert!(page.markdown.contains(expected), "{}", page.markdown);
        }
        for absent in ["SCRIPT_SENTINEL", "NAVIGATION_SENTINEL", "<p>"] {
            assert!(!page.markdown.contains(absent), "{}", page.markdown);
        }

        // A page without an article keeps its whole body, even inside a form, minus
        // the site chrome, form controls and inline image data.
        let index = "<html><title>Index</title><base href='/root/'><body><a href='#top'>top</a><nav>NAVIGATION_SENTINEL</nav><form><h2>Crates</h2><table><tr><th>Name</th></tr><tr><td>serde</td></tr></table><ul><li><a href='/a'>alpha</a></li></ul><img src='data:image/png;base64,SU1BR0U='><img src=' DATA:image/png;base64,SU1BR0U='><button>CONTROL_SENTINEL</button></form><footer>FOOTER_SENTINEL</footer><script>SCRIPT_SENTINEL</script></body></html>";
        let ResponseBody::Page(page) = read(index.into()).await else {
            panic!("expected a page")
        };
        assert_eq!((page.scope, page.title.as_str()), (Scope::Page, "Index"));
        for expected in [
            "## Crates",
            "| Name |\n| --- |\n| serde |",
            "[alpha](https://example.com/a)",
            "[top](https://example.com/root/#top)",
        ] {
            assert!(page.markdown.contains(expected), "{}", page.markdown);
        }
        for absent in [
            "SCRIPT_SENTINEL",
            "NAVIGATION_SENTINEL",
            "FOOTER_SENTINEL",
            "CONTROL_SENTINEL",
            "SU1BR0U=",
        ] {
            assert!(!page.markdown.contains(absent), "{}", page.markdown);
        }

        let shell = "<html><body><div id='app'></div><script>render()</script></body></html>";
        let oversized = "<i></i>".repeat(MAX_HTML_ELEMENTS + 1);
        let long_base = format!(
            "<base href='https://example.com/{}/'>{}",
            "x".repeat(4 << 20),
            "<a href=a>link</a>".repeat(8)
        );
        let cases = [
            (shell.to_owned(), Unreadable::NoText),
            (oversized, Unreadable::TooLarge),
            ("<b>".repeat(4000), Unreadable::TooLarge),
            (long_base, Unreadable::TooLarge),
            // Output and work beyond the input: quote prefixes on every line, and
            // reconstructed formatting elements that each copy their attributes.
            (
                format!("{}{}", "<blockquote>".repeat(250), "a<br>".repeat(50_000)),
                Unreadable::TooLarge,
            ),
            (
                format!(
                    "<div><b c={}></div>{}",
                    "a".repeat(64 * 1024),
                    "<div>x</div>".repeat(400)
                ),
                Unreadable::TooLarge,
            ),
            // Template contents count, and parsing stops at the limits.
            (
                format!(
                    "<template>{}</template>",
                    "<i></i>".repeat(MAX_HTML_ELEMENTS + 1)
                ),
                Unreadable::TooLarge,
            ),
            (
                format!("{0}<template>{0}", "<div>".repeat(MAX_HTML_DEPTH / 2)),
                Unreadable::TooLarge,
            ),
            ("<div>".repeat(200_000), Unreadable::TooLarge),
            // Every line of a block carries its container's prefixes.
            (
                format!(
                    "{}<pre>{}",
                    "<blockquote>".repeat(250),
                    "a\n".repeat(50_000)
                ),
                Unreadable::TooLarge,
            ),
            // Reconstructed elements copy every attribute, valued or not.
            (
                format!(
                    "<div><b {}></div>{}",
                    (0..2000).map(|n| format!("a{n} ")).collect::<String>(),
                    "<div>x</div>".repeat(400)
                ),
                Unreadable::TooLarge,
            ),
            // Links resolve only when written, so empty ones cost nothing.
            (
                format!(
                    "<base href='https://example.com/{}/'>{}",
                    "x".repeat(64 * 1024),
                    "<a href=a></a>".repeat(1000)
                ),
                Unreadable::NoText,
            ),
            // Each stray end tag costs the parser a scan of its open elements.
            ("</h1>".repeat(MAX_HTML_ELEMENTS + 1), Unreadable::TooLarge),
        ];
        // Read as the extractors' capacity allows.
        let bodies = cases.map(async |(html, reason)| (read(html.clone()).await, html, reason));
        for (body, html, reason) in futures_util::future::join_all(bodies).await {
            assert!(
                matches!(&body, ResponseBody::Unreadable { reason: found, text } if *found == reason && *text == html),
                "{body:?}"
            );
        }
    }

    #[tokio::test]
    async fn falls_back_to_the_page_when_the_article_is_over_the_limits() {
        let paragraph =
            "This is the important article body, with meaningful details about the topic. ";
        // Quote prefixes on every line, inside navigation the page leaves out.
        let navigation = format!("{}{}", "<blockquote>".repeat(245), "a<br>".repeat(45_000));
        let html = format!(
            "<html><body><article><p>{0}</p><p>{0}</p><nav>{navigation}</nav></article></body></html>",
            paragraph.repeat(10)
        );
        let ResponseBody::Page(page) = read(html).await else {
            panic!("expected a page")
        };
        assert_eq!(page.scope, Scope::Page);
        assert!(page.markdown.contains("important article body"));
    }

    #[tokio::test]
    async fn charges_each_link_destination_once() {
        // Each link resolves to about 64 KiB: 12 MiB of links, inside the 20 MiB built.
        let html = format!(
            "<base href='https://example.com/{}/'><p>{}</p>",
            "x".repeat(64 << 10),
            "<a href=a>l</a> ".repeat(192)
        );
        let ResponseBody::Page(page) = read(html).await else {
            panic!("expected a page")
        };
        assert_eq!(page.markdown.matches("](https://example.com/").count(), 192);
    }
}
