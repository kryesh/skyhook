//! Fence metadata for the shared, bounded asynchronous syntax cache.
//!
//! Sources are authoritative replacements. The former append-only checkpoints
//! had no production producer and are retired; ordinary prose still avoids a
//! Markdown pass. This never runs Syntect on the render thread.
use super::super::tool_view::{CodeSource, Document, MAX_SECTION, Role, Section};
use super::{markdown, model};
use pulldown_cmark::{CodeBlockKind, Event, Parser, Tag, TagEnd};

#[derive(Default)]
pub(super) struct Fences {
    pub document: Document,
}

impl Fences {
    pub fn update(&mut self, text: &str) {
        self.document.sections.clear();
        if text
            .as_bytes()
            .windows(3)
            .any(|s| s == b"```" || s == b"~~~")
        {
            collect(text, &mut self.document);
        }
    }
}

fn collect(text: &str, document: &mut Document) {
    let cleaned = model::clean(text);
    let mut fence: Option<(String, String)> = None;
    for event in Parser::new_ext(&cleaned, markdown::options()) {
        match event {
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(info))) => {
                let language = info.split_whitespace().next().unwrap_or_default();
                if !language.is_empty() {
                    fence = Some((language.to_owned(), String::new()));
                }
            }
            Event::Text(text) => {
                if let Some((_, source)) = &mut fence {
                    if source.len().saturating_add(text.len()) > MAX_SECTION {
                        fence = None;
                    } else {
                        source.push_str(&text);
                    }
                }
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some((language, source)) = fence.take() {
                    document.sections.push(Section::Code {
                        source: CodeSource::from(source.as_str()),
                        language,
                        indent: 0,
                        gutters: Default::default(),
                        role: Role::Plain,
                    });
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::tool_view::HighlightCache;
    use super::super::{Palette, stream};
    use super::*;
    use std::collections::HashSet;
    use std::time::{Duration, Instant};

    fn complete(cache: &mut HighlightCache, document: &Document) {
        cache.prepare(std::iter::once(document));
        let expected: HashSet<_> = document.highlight_sources().collect();
        let mut completed = HashSet::new();
        let start = Instant::now();
        while !expected.is_subset(&completed) {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "highlight worker timed out"
            );
            std::thread::sleep(Duration::from_millis(2));
            cache.poll();
            completed.extend(cache.take_changed_sources());
        }
    }

    fn text(rows: &[markdown::LayoutLine]) -> Vec<(String, bool)> {
        let rows = rows.iter().filter(|row| !row.layout.decorative());
        rows.map(|row| (row.line.to_string(), row.layout.continued()))
            .collect()
    }

    fn fence_metadata(fences: &Fences) -> Vec<(&str, &str)> {
        let sections = fences.document.sections.iter();
        sections
            .map(|section| {
                let Section::Code {
                    source, language, ..
                } = section
                else {
                    panic!("only code metadata")
                };
                (language.as_str(), source.as_ref())
            })
            .collect()
    }

    #[test]
    fn replacement_fence_metadata_preserves_named_nested_and_cleaned_sources() {
        for (source, expected) in [
            (
                "Prose\n\n```rust extra\nlet café = 42;  \n\n```\n\nTail\n\n~~~python\nprint('yes')\n~~~\n",
                &[
                    ("rust", "let café = 42;  \n\n"),
                    ("python", "print('yes')\n"),
                ][..],
            ),
            (
                "> ```js\n> const x = true;\n> ```\n\n- ```sh\n  echo hi\n  ```\n",
                &[("js", "const x = true;\n"), ("sh", "echo hi\n")],
            ),
            (
                "```unknown\nvalue\n```\n\n```\nnot labelled\n```\n\n    indented\n",
                &[("unknown", "value\n")],
            ),
            // Cleaning removes control characters, not ANSI sequences: ESC is
            // removed while the printable suffix remains.
            (
                "```rust\nlet x = \u{1b}[31m42;\n```\n",
                &[("rust", "let x = [31m42;\n")],
            ),
        ] {
            let mut fences = Fences::default();
            fences.update(source);
            assert_eq!(fence_metadata(&fences), expected);
        }
        // Oversized sources and ordinary prose clear old metadata.
        let mut fences = Fences::default();
        fences.update("```rust\nlet x = 42;\n```\n\nPlain tail\n");
        assert_eq!(fences.document.sections.len(), 1);
        for source in [
            format!("```rust\n{}\n```", "x".repeat(MAX_SECTION + 1)),
            "Ordinary prose without a code fence".into(),
        ] {
            fences.update(&source);
            assert!(fences.document.sections.is_empty());
        }
    }

    #[test]
    fn async_fence_completion_preserves_full_layout() {
        let mut cache = HighlightCache::default();
        let source =
            "Prose\n\n```rust\nlet value = (true, 42, \"hello\");  \n\n// comment\n```\n\nTail";
        let mut fences = Fences::default();
        fences.update(source);
        let p = Palette::new();
        let fallback = text(&stream::layout_highlighted(
            source,
            18,
            p,
            "↳ ",
            Some(&cache),
        ));
        complete(&mut cache, &fences.document);
        // Completion invalidates the owning entry; text, geometry and token roles follow.
        let live = stream::layout_highlighted(source, 18, p, "↳ ", Some(&cache));
        assert_eq!(text(&live), fallback);
        let spans: Vec<_> = live.iter().flat_map(|row| &row.line.spans).collect();
        let colors: HashSet<_> = spans.iter().filter_map(|s| s.style.fg).collect();
        assert!(
            colors.len() >= 3,
            "named fences should have distinct token roles: {colors:?}"
        );
        assert!(spans.iter().all(|s| s.style.bg.is_none()));
    }
}
