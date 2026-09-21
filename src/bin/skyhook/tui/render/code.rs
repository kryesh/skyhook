//! Fence metadata for the shared, bounded asynchronous syntax cache. Ordinary
//! prose avoids a Markdown pass, and Syntect never runs on the render thread.
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
    let mut fence: Option<(String, String, markdown::Fence)> = None;
    for (event, range) in Parser::new_ext(&cleaned, markdown::options()).into_offset_iter() {
        match event {
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(info))) => {
                let language = info.split_whitespace().next().unwrap_or_default();
                if !language.is_empty() {
                    fence = Some((
                        language.to_owned(),
                        String::new(),
                        markdown::Fence::new(&cleaned, range),
                    ));
                }
            }
            Event::Text(text) => {
                if let Some((_, source, ending)) = &mut fence {
                    if source.len().saturating_add(text.len()) > MAX_SECTION {
                        fence = None;
                    } else {
                        source.push_str(&text);
                        ending.text(range.end);
                    }
                }
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some((language, source, ending)) = fence.take()
                    && ending.closed(&cleaned)
                {
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
    use super::*;

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
    fn streaming_fences_are_admitted_only_after_the_closing_marker() {
        let mut fences = Fences::default();
        for (open, body, close) in [
            ("```rust\n", "let x = 42;\n", "```"),
            ("~~~~rust\n", "let x = 42;\n", "~~~~"),
            ("> ```rust\n", "> let x = 42;\n", "> ```"),
            ("- ```rust\n", "  let x = 42;\n", "  ```"),
            ("> - ~~~~rust\n", ">   let x = 42;\n", ">   ~~~~"),
        ] {
            let source = format!("{open}{body}{close}");
            for end in 0..source.len() {
                fences.update(&source[..end]);
                assert!(
                    fences.document.sections.is_empty(),
                    "premature highlight for {:?}",
                    &source[..end]
                );
            }
            for suffix in ["", "  \t", "\n", "\n\n```rust\nlet incomplete ="] {
                fences.update(&format!("{source}{suffix}"));
                assert_eq!(fence_metadata(&fences), [("rust", "let x = 42;\n")]);
            }
        }
    }

    #[test]
    fn implicit_code_block_ends_and_invalid_closing_markers_are_not_admitted() {
        for source in [
            "```rust\n   ",
            "~~~rust ~~~",
            "````rust\nlet x = 42;\n```",
            "```rust\nlet x = 42;\n~~~",
            "```rust\nlet x = 42;\n``` trailing",
            "```rust\nlet x = 42;\n    ```",
            "> ```rust\n> ",
            "> ```rust\n> let x = 42;\n> ",
            "> ```rust\n> let x = 42;\n\nOutside the quote",
            "- ```rust\n  let x = 42;\n\nOutside the list",
        ] {
            let mut fences = Fences::default();
            fences.update(source);
            assert!(fences.document.sections.is_empty(), "{source:?}");
        }
    }

    #[test]
    fn replacement_fence_metadata_preserves_named_nested_and_cleaned_sources() {
        for (source, expected) in [
            ("```rust\n````\n", &[("rust", "")][..]),
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
}
