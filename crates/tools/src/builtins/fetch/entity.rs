//! A response body as its bytes and headers classify and decode it.

use encoding_rs::{Encoding, UTF_8};

use super::ResponseFormat;

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
        // parameter must not override it.
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
    /// decision. Unknown encodings and malformed bytes are replaced.
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

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub(in crate::builtins::fetch) fn entity(
        bytes: &[u8],
        content_type: Option<&str>,
    ) -> ResponseEntity {
        ResponseEntity::new(bytes.to_vec(), content_type, ResponseFormat::Auto)
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
}
