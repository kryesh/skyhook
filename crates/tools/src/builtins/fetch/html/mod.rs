//! HTML read as markdown within budgets: the readable article when there is one,
//! otherwise the page body.

mod budget;
mod markdown;
mod writer;

use std::panic::AssertUnwindSafe;

use dom_query::{Document, NodeRef};
use dom_smoothie::Readability;
use html5ever::{LocalName, local_name};
use schemars::JsonSchema;
use serde::Serialize;
use tokio::sync::Semaphore;

pub(super) use budget::Limits;
use budget::{Budget, Spent};
use markdown::markdown;

use super::{ResponseBody, entity::ResponseEntity};
use skyhook_worker::op::HttpRequestUrl;

// Keep permits inside the blocking task: cancelling a caller must not let it
// queue unlimited parsers while previously-started work is still running.
static EXTRACTORS: Semaphore = Semaphore::const_new(2);

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

/// What `markdown` covers
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(super) enum Scope {
    /// The main article
    Article,
    /// The page body without scripts, navigation, form controls, footers and hidden elements
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

/// Why HTML could not be read as markdown
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(super) enum Unreadable {
    /// Over the extraction size, element, nesting or output limits
    TooLarge,
    /// No text, as on pages rendered by JavaScript
    NoText,
    ParserFailed,
}

impl From<Spent> for Unreadable {
    fn from(Spent: Spent) -> Self {
        Self::TooLarge
    }
}

/// Read HTML as markdown: the readable article when there is one, otherwise the
/// page body. A body that cannot be read keeps its decoded source.
/// The page `entity`'s HTML reads as, within `limits`.
pub(super) async fn extract(
    entity: ResponseEntity,
    url: &HttpRequestUrl,
    limits: Limits,
) -> ResponseBody {
    let html = match admit(entity, limits) {
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
        let page = std::panic::catch_unwind(AssertUnwindSafe(|| parse(&html, &url, limits)));
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

/// The decoded HTML, or `Err` with it when it is over its byte budget. The raw
/// bytes are dropped here, before any semaphore wait or worker queue.
fn admit(entity: ResponseEntity, limits: Limits) -> Result<String, String> {
    let html = entity.decode();
    if html.len() <= limits.decoded_bytes {
        Ok(html)
    } else {
        Err(html)
    }
}

fn parse(html: &str, url: &HttpRequestUrl, limits: Limits) -> Result<Page, Unreadable> {
    let budget = Budget::new(limits);
    let (source, base) = source(html, url.url(), &budget)?;
    let readability = |document| {
        Readability::with_document(document, None, None).map_err(|_| Unreadable::ParserFailed)
    };
    let page = readability(source)?;
    // Readability rewrites the document it parses, so an article is parsed from a
    // copy and the page falls back to the source.
    if page.is_probably_readable()
        && let Ok(article) = readability(page.doc.clone())?.parse()
        && let Ok(markdown) = article_markdown(&article.content, &base, limits)
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
fn source(html: &str, url: &url::Url, budget: &Budget) -> Result<(Document, url::Url), Unreadable> {
    let source = budget::parse(html, budget)?;
    let base = (source.select("base[href]").attr("href"))
        .and_then(|href| url.join(&href).ok())
        .unwrap_or_else(|| url.clone());
    source.select("base").remove();
    Ok((source, base))
}

/// Readability returns the article as HTML, read again under a budget of its own.
fn article_markdown(html: &str, base: &url::Url, limits: Limits) -> Result<String, Unreadable> {
    let budget = Budget::new(limits);
    let article = budget::parse(html, &budget)?;
    let body = article.body().ok_or(Unreadable::NoText)?;
    markdown(body, Scope::Article, base, &budget)
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use super::budget::MAX_HTML_DEPTH;
    use super::*;
    use crate::builtins::fetch::{ResponseFormat, entity::tests::entity};

    /// The limits these tests read under, a tenth of the default; sizes below are
    /// a tenth of what reaches the default's.
    const LIMITS: Limits = Limits::SMALL;

    async fn read(html: String) -> ResponseBody {
        let url = HttpRequestUrl::parse("https://example.com/article").unwrap();
        extract(entity(html.as_bytes(), Some("text/html")), &url, LIMITS).await
    }

    #[test]
    fn extraction_admits_html_up_to_its_exact_decoded_limit() {
        let html =
            |bytes: Vec<u8>| ResponseEntity::new(bytes, Some("text/html"), ResponseFormat::Auto);
        let exact = admit(html(vec![b'a'; LIMITS.decoded_bytes]), LIMITS).unwrap();
        assert_eq!(exact.len(), LIMITS.decoded_bytes);
        // Over the budget, the decoded source is kept.
        let oversized = admit(html(vec![b'a'; LIMITS.decoded_bytes + 1]), LIMITS).unwrap_err();
        assert_eq!(oversized.len(), LIMITS.decoded_bytes + 1);
        // Replacement characters expand fewer bytes past it.
        let replacements = html(vec![0xff; LIMITS.decoded_bytes / 3 + 1]);
        assert!(admit(replacements, LIMITS).is_err());
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
        assert_eq!(page.scope, Scope::Article);
        for expected in [
            "important article body",
            "about the topic. Readers",
            "[docs](https://example.com/docs)",
        ] {
            assert!(page.markdown.contains(expected), "{}", page.markdown);
        }
        for absent in ["SCRIPT_SENTINEL", "<p>"] {
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
        let oversized = "<i></i>".repeat(LIMITS.elements + 1);
        let long_base = format!(
            "<base href='https://example.com/{}/'>{}",
            "x".repeat((64 << 10) / 10),
            "<a href=a>link</a>".repeat(400)
        );
        let cases = [
            (shell.to_owned(), Unreadable::NoText),
            (oversized, Unreadable::TooLarge),
            ("<b>".repeat(4000), Unreadable::TooLarge),
            (long_base, Unreadable::TooLarge),
            // Output and work beyond the input: quote prefixes on every line, and
            // reconstructed formatting elements that each copy their attributes.
            (
                format!("{}{}", "<blockquote>".repeat(250), "a<br>".repeat(5_000)),
                Unreadable::TooLarge,
            ),
            (
                format!(
                    "<div><b c={}></div>{}",
                    "a".repeat(64 * 1024 / 10),
                    "<div>x</div>".repeat(400)
                ),
                Unreadable::TooLarge,
            ),
            // Template contents count, and parsing stops at the limits.
            (
                format!(
                    "<template>{}</template>",
                    "<i></i>".repeat(LIMITS.elements + 1)
                ),
                Unreadable::TooLarge,
            ),
            (
                format!("{0}<template>{0}", "<div>".repeat(MAX_HTML_DEPTH / 2)),
                Unreadable::TooLarge,
            ),
            // Links resolve only when written, so empty ones cost nothing.
            (
                format!(
                    "<base href='https://example.com/{}/'>{}",
                    "x".repeat(64 * 1024 / 10),
                    "<a href=a></a>".repeat(1000)
                ),
                Unreadable::NoText,
            ),
            // Each stray end tag costs the parser a scan of its open elements.
            ("</h1>".repeat(LIMITS.elements + 1), Unreadable::TooLarge),
        ];
        // Read as the extractors' capacity allows.
        let bodies = cases.map(async |(html, reason)| (read(html.clone()).await, html, reason));
        for (body, html, reason) in futures_util::future::join_all(bodies).await {
            assert_matches!(&body, ResponseBody::Unreadable { reason: found, text } if *found == reason && *text == html,
                "{body:?}"
            );
        }
    }

    #[tokio::test]
    async fn falls_back_to_the_page_when_the_article_is_over_the_limits() {
        let paragraph =
            "This is the important article body, with meaningful details about the topic. ";
        // Quote prefixes on every line, inside navigation the page leaves out.
        let navigation = format!("{}{}", "<blockquote>".repeat(245), "a<br>".repeat(4_500));
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
        // Each link resolves to about 6.4 KiB: 1.2 MiB of links, inside the 2 MiB built.
        let html = format!(
            "<base href='https://example.com/{}/'><p>{}</p>",
            "x".repeat((64 << 10) / 10),
            "<a href=a>l</a> ".repeat(192)
        );
        let ResponseBody::Page(page) = read(html).await else {
            panic!("expected a page")
        };
        assert_eq!(page.markdown.matches("](https://example.com/").count(), 192);
    }
}
