//! Standalone Markdown titles that split prose into sections.
use super::parser::{events, parse};
use pulldown_cmark::{Event, Tag};
use ratatui::text::Line;
use std::ops::Range;

pub(in crate::tui) struct Section<'a> {
    /// The title line that opens the section.
    pub title: Option<Source<'a>>,
    pub body: Source<'a>,
}

/// Part of a text and the text's link definitions from outside that part, so
/// its references resolve as they did in the whole text. Definitions lead, where
/// an unclosed fence in the part cannot swallow them.
pub(in crate::tui) struct Source<'a> {
    pub text: &'a str,
    pub definitions: String,
}

impl Source<'_> {
    pub fn markdown(&self) -> String {
        format!("{}{}", self.definitions, self.text)
    }
}

/// Splits `text` at its titles: top-level headings or wholly bold/italic
/// paragraphs rendering as one line and followed by a blank line. Until that blank
/// line arrives, a streaming title may still continue its paragraph.
pub(in crate::tui) fn sections(text: &str) -> Vec<Section<'_>> {
    let events = events(text);
    let mut definitions: Vec<_> = (events.reference_definitions().iter())
        .map(|(_, definition)| definition.span.clone())
        .collect();
    definitions.sort_unstable_by_key(|span| span.start);
    let source = |part: Range<usize>| Source {
        text: text[part.clone()].trim_matches(['\r', '\n']),
        definitions: (definitions.iter())
            .filter(|span| !part.contains(&span.start))
            .map(|span| format!("{}\n\n", &text[span.clone()]))
            .collect(),
    };
    let blank_after = |start: usize| {
        let next = text[start..].split_inclusive('\n').nth(1);
        next.is_some_and(|next| next.ends_with('\n') && next.trim().is_empty())
    };
    let mut sections = Vec::new();
    // The body runs from `start` to the end of its last rendered block, so
    // link definitions alone leave it empty.
    let (mut opened, mut start, mut end, mut depth) = (None, 0, 0, 0usize);
    let mut candidate = None;
    let mut events = events.peekable();
    while let Some((event, range)) = events.next() {
        match event {
            Event::Start(tag) => {
                let shaped = depth == 0
                    && match tag {
                        Tag::Heading { .. } => true,
                        Tag::Paragraph => matches!(
                            events.peek(),
                            Some((Event::Start(Tag::Strong | Tag::Emphasis), inner))
                                if text[inner.clone()] == *text[range.clone()].trim()
                        ),
                        _ => false,
                    };
                if shaped && blank_after(range.start) {
                    candidate = Some(range);
                }
                depth += 1;
            }
            Event::End(_) => {
                depth -= 1;
                if depth > 0 {
                    continue;
                }
                match candidate
                    .take()
                    .filter(|range| title(&text[range.clone()]).is_some())
                {
                    Some(range) => {
                        let title = opened.replace(range.clone()).map(&source);
                        sections.push(Section {
                            title,
                            body: source(start..end),
                        });
                        (start, end) = (range.end, range.end);
                    }
                    None => end = range.end,
                }
            }
            // The one top-level block without start and end events.
            Event::Rule => end = range.end,
            _ => {}
        }
    }
    sections.push(Section {
        title: opened.map(&source),
        body: source(start..end),
    });
    sections.retain(|section| section.title.is_some() || !section.body.text.is_empty());
    sections
}

/// A title's styled line: the one line of visible text `source` renders as.
pub(in super::super) fn title(source: &str) -> Option<Line<'static>> {
    let [parsed] = <[_; 1]>::try_from(parse(source, false, usize::MAX, None).lines).ok()?;
    (!parsed.line.to_string().trim().is_empty()).then_some(parsed.line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_are_standalone_top_level_emphasis_or_headings() {
        let body = "Body **not** a title.\n```\n# comment\n```\n- **item**\n\n**a** and **b**";
        let text = format!("Intro\n\n**First**\n\n{body}\n\n## Second\n\n*Third `x`*\r\n\r\ntail");
        let split: Vec<_> = sections(&text)
            .into_iter()
            .map(|section| (section.title.map(|title| title.text), section.body.text))
            .collect();
        assert_eq!(
            split,
            [
                (None, "Intro"),
                (Some("**First**"), body),
                (Some("## Second"), ""),
                (Some("*Third `x`*"), "tail"),
            ]
        );
        let untitled = [
            "Setext\n---\n\n**open",
            "Intro\n\n**Streaming**\n",
            "**Bold**\nnext line",
            "## Heading\nnext line\n\nmore",
            "**One&#10;Two**\n\nbody",
            "#\n\nbody",
        ];
        for text in untitled {
            let untitled = sections(text);
            assert!(
                untitled.len() == 1 && untitled[0].title.is_none(),
                "{text:?}"
            );
        }
    }

    #[test]
    fn references_resolve_against_definitions_in_other_sections() {
        let definition = "[ref]: https://example.com";
        let text = format!(
            "**See [docs][ref]**\n\nBody\n\n## Next\n\nMore [docs][ref]\n\n{definition}\n\nEnd"
        );
        let definition = format!("{definition}\n\n");
        let definition = definition.as_str();
        let [first, next] = sections(&text).try_into().ok().unwrap();
        let heading = first.title.unwrap();
        assert_eq!(
            (&*heading.definitions, &*first.body.definitions),
            (definition, definition)
        );
        let styled = title(&heading.markdown()).unwrap().to_string();
        assert!(styled.contains("https://example.com"));
        // A definition within a part is not repeated.
        assert!(next.body.definitions.is_empty());
        // A body of definitions alone renders nothing, so it is empty.
        let only = sections("## Only\n\n[ref]: https://example.com");
        assert!(only.len() == 1 && only[0].body.text.is_empty());
        // An unclosed fence in a streaming part cannot swallow definitions.
        let text = "Intro [x][ref]\n\n[ref]: https://example.com\n\n## Code\n\n```\nlet x;";
        let body = sections(text).pop().unwrap().body.markdown();
        let lines = parse(&body, false, 80, None).lines;
        assert!(
            lines
                .iter()
                .all(|line| !line.line.to_string().contains("[ref]"))
        );
    }
}
