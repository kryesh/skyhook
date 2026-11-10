//! Schema descriptions shaped from doc comments.

/// `schema`, described by `docs`, the lines of a type's doc comment, unless it has a
/// description of its own.
pub fn described(mut schema: schemars::Schema, docs: &[&str]) -> schemars::Schema {
    let description = reflow(&docs.join("\n"));
    if !description.is_empty() && schema.get("description").is_none() {
        schema.insert("description".to_owned(), description.into());
    }
    schema
}

/// Doc comment `text` as one reader-facing description: the lines a paragraph wraps
/// over are joined, while blank-line paragraph breaks, list items and fenced code keep
/// their lines.
pub fn reflow(text: &str) -> String {
    #[derive(PartialEq)]
    enum Last {
        Nothing,
        Prose,
        Fence,
    }
    let item = |line: &str| {
        let numbered = line.trim_start_matches(|c: char| c.is_ascii_digit());
        ["- ", "* ", "+ "]
            .iter()
            .any(|marker| line.starts_with(marker))
            || (numbered.len() < line.len()
                && (numbered.starts_with(". ") || numbered.starts_with(") ")))
    };
    let (mut out, mut last, mut paragraph, mut fenced) =
        (String::new(), Last::Nothing, false, false);
    for line in text.lines() {
        if fenced {
            out.push('\n');
            out.push_str(line);
            fenced = !line.trim_start().starts_with("```");
            last = Last::Fence;
            continue;
        }
        let line = line.trim();
        if line.is_empty() {
            paragraph = last != Last::Nothing;
            continue;
        }
        let fence = line.starts_with("```");
        out.push_str(match last {
            Last::Nothing => "",
            _ if paragraph => "\n\n",
            Last::Prose if !fence && !item(line) => " ",
            Last::Prose | Last::Fence => "\n",
        });
        out.push_str(line);
        (fenced, paragraph) = (fence, false);
        last = if fence { Last::Fence } else { Last::Prose };
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wrapped lines join; paragraphs, list items and fenced code keep their breaks.
    #[test]
    fn doc_text_reflows_into_paragraphs() {
        let doc = "Markdown shown beside the option:\na short artifact.\n\nTwo kinds:\n- code,\n  wrapped\n1. a mockup\n```rust\nlet a = 1;\n  let b = 2;\n```\nAfter it.";
        assert_eq!(
            reflow(doc),
            "Markdown shown beside the option: a short artifact.\n\nTwo kinds:\n- code, wrapped\n1. a mockup\n```rust\nlet a = 1;\n  let b = 2;\n```\nAfter it."
        );
        assert_eq!(reflow("\n  One line.  \n\n"), "One line.");
    }
}
