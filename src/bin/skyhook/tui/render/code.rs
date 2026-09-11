//! Fence metadata for the shared, bounded asynchronous syntax cache.
//!
//! Committed Markdown blocks retain their immutable CodeSource allocations. Live
//! appends only parse the unstable suffix; ordinary prose needs no Markdown pass.
//! This never runs Syntect on the render thread.
use super::super::tool_view::{CodeSource, Document, MAX_SECTION, Role, Section};
use super::{markdown, model, stream};
use pulldown_cmark::{CodeBlockKind, Event, Parser, Tag, TagEnd};

#[derive(Default)]
pub(super) struct Fences {
    pub document: Document,
    len: usize,
    stable_bytes: usize,
    stable_sections: usize,
    detected: bool,
    global: bool,
}

impl Fences {
    pub fn update(&mut self, text: &str, append_from: Option<usize>) {
        let append = append_from == Some(self.len) && self.len <= text.len();
        if append && self.len == text.len() {
            return;
        }
        if !append {
            *self = Self::default();
        }
        let from = self.len;
        self.len = text.len();
        self.global |= text[from..]
            .chars()
            .any(|c| matches!(c, '[' | ']' | '<' | '>') || (c.is_control() && c != '\n'));
        // Include two previous bytes to catch a delimiter arriving one character
        // at a time. Byte windows need not start at a UTF-8 character boundary.
        self.detected |= text.as_bytes()[from.saturating_sub(2)..]
            .windows(3)
            .any(|s| s == b"```" || s == b"~~~");
        if !self.detected {
            return;
        }
        if self.global {
            self.stable_bytes = 0;
            self.stable_sections = 0;
        }
        self.document.sections.truncate(self.stable_sections);
        let suffix = &text[self.stable_bytes..];
        let boundary = if self.global {
            0
        } else {
            stream::stable_boundary(suffix)
        };
        if boundary > 0 {
            collect(&suffix[..boundary], &mut self.document);
            self.stable_bytes += boundary;
            self.stable_sections = self.document.sections.len();
        }
        collect(&suffix[boundary..], &mut self.document);
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
                        gutters: Vec::new(),
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
    use super::super::{Palette, stream::StreamLayout};
    use super::*;

    fn complete(cache: &mut HighlightCache, document: &Document, light: bool) {
        cache.prepare(std::iter::once(document), light);
        let expected: std::collections::HashSet<_> = document.highlight_sources().collect();
        let mut completed = std::collections::HashSet::new();
        let start = std::time::Instant::now();
        loop {
            cache.poll();
            completed.extend(cache.take_changed_sources());
            if expected.is_subset(&completed) {
                break;
            }
            assert!(
                start.elapsed() < std::time::Duration::from_secs(5),
                "highlight worker timed out"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    fn text(rows: &[markdown::LayoutLine]) -> Vec<(String, bool)> {
        rows.iter()
            .filter(|row| !row.layout.decorative)
            .map(|row| (row.line.to_string(), row.continued))
            .collect()
    }

    #[test]
    fn streamed_fence_metadata_matches_saved_at_every_character() {
        for source in [
            "Prose\n\n```rust extra\nlet café = 42;  \n\n```\n\nTail\n\n~~~python\nprint('yes')\n~~~\n",
            "> ```js\n> const x = true;\n> ```\n\n- ```sh\n  echo hi\n  ```\n",
            "```unknown\nvalue\n```\n\n```\nnot labelled\n```\n\n    indented\n",
            "```rust\nlet x = \u{1b}[31m42;\n```\n",
        ] {
            let mut live = Fences::default();
            let mut previous = 0;
            for end in source.char_indices().map(|(i, ch)| i + ch.len_utf8()) {
                live.update(&source[..end], Some(previous));
                let mut saved = Fences::default();
                saved.update(&source[..end], None);
                assert_eq!(live.document, saved.document, "{end}: {:?}", &source[..end]);
                previous = end;
            }
        }
    }

    #[test]
    fn committed_fence_sources_are_shared_and_oversized_sources_not_admitted() {
        let mut fences = Fences::default();
        let mut source = "```rust\nlet x = 42;\n```\n\nPlain tail\n".to_owned();
        fences.update(&source, None);
        assert!(fences.stable_bytes > 0);
        let Section::Code { source: first, .. } = &fences.document.sections[0] else {
            panic!()
        };
        let original = first.clone();
        let old = source.len();
        source.push_str("more text\n");
        fences.update(&source, Some(old));
        let Section::Code { source: first, .. } = &fences.document.sections[0] else {
            panic!()
        };
        assert_eq!(
            first.as_ptr(),
            original.as_ptr(),
            "committed source must not be copied or rehashed"
        );

        fences.update(
            &format!("```rust\n{}\n```", "x".repeat(MAX_SECTION + 1)),
            None,
        );
        assert!(fences.document.sections.is_empty());
        fences.update("Ordinary prose without a code fence", None);
        assert!(!fences.detected);
        assert!(fences.document.sections.is_empty());
    }

    #[test]
    fn async_saved_and_live_fences_preserve_layout_and_switch_themes() {
        let mut cache = HighlightCache::default();
        let mut source = String::new();
        let mut fences = Fences::default();
        let mut stream = StreamLayout::default();
        let mut rows = Vec::new();
        let chunks = [
            "Prose\n\n```rust\n",
            "let value = (true, 42, \"hello\");  \n",
            "\n// comment\n",
            "```\n\nTail",
        ];
        let p = Palette::new(false);
        for chunk in chunks {
            let old = source.len();
            source.push_str(chunk);
            fences.update(&source, Some(old));
            if let Some((at, suffix)) =
                stream.update_highlighted(&source, 18, p, Some(old), "↳ ", Some(&cache))
            {
                rows.truncate(at);
                rows.extend(suffix);
            }
        }
        let fallback = text(&rows);
        complete(&mut cache, &fences.document, false);
        // Completion invalidates the owning entry, including committed blocks.
        let (at, live) = stream
            .update_highlighted(&source, 18, p, None, "↳ ", Some(&cache))
            .unwrap();
        assert_eq!(at, 0);
        assert_eq!(text(&live), fallback);
        let (_, saved) = StreamLayout::default()
            .update_highlighted(&source, 18, p, None, "↳ ", Some(&cache))
            .unwrap();
        assert_eq!(live, saved);
        let colors: std::collections::HashSet<_> = live
            .iter()
            .flat_map(|row| &row.line.spans)
            .filter_map(|s| s.style.fg)
            .collect();
        assert!(
            colors.len() >= 3,
            "named fences should have distinct token roles: {colors:?}"
        );
        assert!(
            live.iter()
                .flat_map(|row| &row.line.spans)
                .all(|s| s.style.bg.is_none())
        );

        let light = Palette::new(true);
        complete(&mut cache, &fences.document, true);
        let (at, switched) = stream
            .update_highlighted(&source, 18, light, Some(source.len()), "↳ ", Some(&cache))
            .unwrap();
        assert_eq!(at, 0);
        assert_eq!(text(&switched), fallback);
        assert_ne!(live, switched);
        assert!(
            stream
                .update_highlighted(&source, 18, light, Some(source.len()), "↳ ", Some(&cache))
                .is_none()
        );
        // A full theme identity, not the old accent-only key, invalidates prose.
        let mut changed = light;
        changed.content.fg = ratatui::style::Color::Red;
        assert!(
            stream
                .update_highlighted(&source, 18, changed, Some(source.len()), "↳ ", Some(&cache))
                .is_some()
        );
    }
}
