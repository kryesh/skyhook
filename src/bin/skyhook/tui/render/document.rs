//! Entry geometry and document/plain-text layout.

use super::*;

/// Geometry and framing shared by the streaming and document/plain paths.
pub(super) struct EntryGeometry {
    pub(super) x: u16,
    pub(super) block_width: u16,
    pub(super) row_width: u16,
    pub(super) body_width: u16,
    pub(super) surface: Surface,
    pub(super) entry: usize,
    entry_key: std::sync::Arc<model::EntryKey>,
    pub(super) selectable: bool,
    pub(super) tool_gutter: bool,
}
impl EntryGeometry {
    pub(super) fn new(entry: &model::Entry, width: u16, index: usize) -> Self {
        let block = matches!(entry.surface, Surface::User | Surface::Agent);
        let available = width.saturating_sub(5).max(1);
        // Message boxes keep one column on the sender's side and three on the
        // opposite side, leaving all remaining width available for content.
        let block_width = if block {
            width.saturating_sub(4).max(1)
        } else {
            available
        };
        let indent = entry.indent.min(available / 3);
        Self {
            x: if entry.surface == Surface::User {
                width.saturating_sub(block_width + 1)
            } else if block {
                1
            } else {
                2 + indent
            },
            block_width,
            row_width: block_width.saturating_sub(if block { 0 } else { indent }),
            body_width: block_width
                .saturating_sub(if block { 4 } else { indent })
                .max(1),
            surface: entry.surface,
            entry: index,
            entry_key: std::sync::Arc::new(entry.key().clone()),
            selectable: entry_selectable(entry),
            tool_gutter: false,
        }
    }
    pub(super) fn row(&self, line: Line<'static>, header: bool, continued: bool) -> Row {
        Row {
            line: std::sync::Arc::new(line),
            x: self.x,
            width: self.row_width,
            surface: self.surface,
            entry: self.entry,
            entry_key: self.entry_key.clone(),
            selectable: self.selectable,
            layout: markdown::RowLayout::default().with_flow(header, continued),
            inset: u16::from(self.tool_gutter && !header),
        }
    }
    pub(super) fn blank(&self, surface: Surface, x: u16, width: u16) -> Row {
        Row {
            line: std::sync::Arc::new(Line::default()),
            x,
            width,
            surface,
            entry: self.entry,
            entry_key: self.entry_key.clone(),
            selectable: self.selectable,
            layout: markdown::RowLayout::spacer(),
            inset: 0,
        }
    }
}

/// Non-streaming fallback is entry-local and handles only documents or plain text.
pub(super) fn layout_document_or_plain_with_expansion(
    entry: &model::Entry,
    width: u16,
    p: Palette,
    highlights: &super::super::tool_view::HighlightCache,
    index: usize,
    expanded: bool,
) -> Vec<Row> {
    let mut geometry = EntryGeometry::new(entry, width, index);
    geometry.tool_gutter = expanded && entry.expandable() && entry.surface == Surface::Tool;
    let block = matches!(entry.surface, Surface::User | Surface::Agent);
    let lines = if let Some(header) = entry.header() {
        let mut lines = vec![(
            super::super::tool_view::header_line(header),
            super::super::tool_view::Wrap::Hard,
        )];
        if let Some(body) = entry.document() {
            lines.extend(body.layout_lines(Some(highlights)));
        }
        lines
    } else if let Some(document) = entry.document() {
        document.layout_lines(Some(highlights))
    } else {
        debug_assert!(!block && entry.surface != Surface::Reasoning);
        // A running entry leaves its first cells to the spinner: after the
        // disclosure glyph of a title, or before an untitled body.
        let title = entry
            .title()
            .map(|title| model::clean(&title.line(entry.running)));
        let gutter = if title.is_none() && entry.running {
            "  "
        } else {
            ""
        };
        let body = model::clean(entry.body());
        let body = (title.is_none() || !body.is_empty()).then_some(body);
        let body = body.iter().flat_map(|body| body.split('\n')).enumerate();
        title
            .into_iter()
            .chain(body.map(|(index, line)| {
                let gutter = if index == 0 { gutter } else { "" };
                format!("{gutter}{line}")
            }))
            .map(|line| (Line::from(line), super::super::tool_view::Wrap::Hard))
            .collect()
    };
    let mut rows = Vec::new();
    if block {
        rows.push(geometry.blank(entry.surface, geometry.x, geometry.block_width));
    }
    let mut header = true;
    for (line_index, (mut line, wrapping)) in lines.into_iter().enumerate() {
        // Card headers were already built as line 0 above.
        if line_index == 0 && entry.surface == Surface::Tool && entry.header().is_none() {
            for span in &mut line.spans {
                span.style.fg = Some(p.content.fg);
            }
        }
        let body_width = geometry
            .body_width
            .saturating_sub(u16::from(geometry.tool_gutter))
            .max(1) as usize;
        let first_width = if line_index == 0 {
            geometry.body_width as usize
        } else {
            body_width
        };
        let wrapped = match wrapping {
            super::super::tool_view::Wrap::Hard => wrap_line_widths(line, first_width, body_width),
            super::super::tool_view::Wrap::Words => wrap_words(
                line,
                if line_index == 0 {
                    first_width
                } else {
                    body_width
                },
            ),
        };
        for (part, line) in wrapped.into_iter().enumerate() {
            rows.push(geometry.row(line, header, part > 0));
            header = false;
        }
    }
    if block {
        rows.push(geometry.blank(entry.surface, geometry.x, geometry.block_width));
    }
    if !entry.compact_after {
        rows.push(geometry.blank(Surface::Muted, 0, width));
    }
    rows
}

/// Borrow cached spans while matching Paragraph's unwrapped clipping and styling.
/// Unlike Line's Widget implementation, Paragraph leaves wide continuation cells
/// in the surface style and skips individual graphemes wider than the viewport.
#[cfg(test)]
mod tests {
    use super::super::super::tool_view::{
        Document, HighlightCache, Role, Run, Section, Wrap, header_line,
    };
    use super::*;

    fn rows_of(entry: &model::Entry, width: u16, cache: &HighlightCache) -> Vec<Row> {
        let p = Palette::new();
        layout_document_or_plain_with_expansion(entry, width, p, cache, 0, entry.default_open)
    }

    fn arguments(tool: &str, args: &serde_json::Value) -> Document {
        let mut document = Document::default();
        document.line(tool, Role::ToolName);
        document.arguments(tool, args);
        document
    }

    fn assert_argument_reflow(document: &Document, cache: &HighlightCache) {
        let mut body = document.clone();
        let Section::Line(header) = body.sections.remove(0) else {
            panic!("argument fixture starts with its tool header");
        };
        let mut entry = model::Entry::card(model::EntryKey::UnsavedStatus(1), header, Some(body));
        entry.default_open = true;
        entry.compact_after = true;
        for width in [0, 1, 2, 8, 12, 40, 120, 24] {
            let geometry = EntryGeometry::new(&entry, width, 0);
            let rows = rows_of(&entry, width, cache);
            let body = geometry.body_width.saturating_sub(1).max(1) as usize;
            let lines = document.layout_lines(Some(cache)).into_iter().enumerate();
            let expected: Vec<_> = lines
                .flat_map(|(index, (line, wrap))| {
                    let first = if index == 0 {
                        geometry.body_width as usize
                    } else {
                        body
                    };
                    match wrap {
                        Wrap::Hard => wrap_line_widths(line, first, body),
                        Wrap::Words => wrap_words(line, first),
                    }
                })
                .map(|line| line.to_string())
                .collect();
            assert_eq!(
                rows.iter().map(Row::text).collect::<Vec<_>>(),
                expected,
                "width {width}"
            );
            let start = TextPosition { row: 0, byte: 0 };
            let end = TextPosition {
                row: rows.len() - 1,
                byte: rows.last().unwrap().text().len(),
            };
            let mut blocks = RowBlocks::default();
            blocks.replace_entry(0, rows, Vec::new());
            assert_eq!(
                selected_text(&blocks, (start, end)),
                entry.text(),
                "width {width}"
            );
            assert_eq!(selected_text(&blocks, (end, start)), entry.text());
        }
    }

    #[test]
    fn tool_argument_prose_wraps_words_without_interpreting_or_losing_text() {
        let prompt = "Please inspect the current implementation and explain every relevant change. "
            .repeat(5)
            + "\n\n  Keep **literal** text, café e\u{301}lan 世界 👩‍💻 together.  \n";
        let args = serde_json::json!({
            "prompt": prompt,
            "description": "short inline prose",
            "nested": {"items": ["nested array prose", "first paragraph\n  second paragraph  "]},
            "token": "extraordinarilylongunbrokentoken世界👩‍💻e\u{301}",
        });
        let original = args.clone();
        let document = arguments("agent", &args);
        assert_argument_reflow(&document, &HighlightCache::default());
        // A long prompt's short words must never be split, even though its
        // paragraphs are longer than the viewport and include indentation.
        for (line, wrapping) in document.layout_lines(None) {
            let text = line.to_string();
            if wrapping != Wrap::Words || text.contains("extraordinarily") {
                continue;
            }
            for width in [24, 40, 64] {
                let parts: Vec<_> = wrap_words(line.clone(), width)
                    .iter()
                    .map(ToString::to_string)
                    .collect();
                let words = parts.join(" ");
                let words: Vec<_> = words.split_whitespace().collect();
                assert_eq!(words, text.split_whitespace().collect::<Vec<_>>());
            }
        }
        assert_eq!(args, original);
    }

    #[test]
    fn tool_argument_code_keeps_hard_wrap_and_copy_after_resize() {
        let source = "  const message = 'long words stay hard wrapped';\t// café 👩‍💻\n\n    console.log(message);  \n";
        let cache = HighlightCache::default();
        for (tool, args) in [
            (
                "script",
                serde_json::json!({"source": source, "description": "prose next to source"}),
            ),
            (
                "shell",
                serde_json::json!({"command": "  printf '%s  %s'  first second\t\n\n"}),
            ),
            (
                "exec",
                serde_json::json!({"argv": ["sh", "-c", "  echo long shell words  \n"], "nested": {"source": source}}),
            ),
            (
                "write",
                serde_json::json!({"path": "file.js", "content": source}),
            ),
            (
                "replace",
                serde_json::json!({"path": "file.js", "old": source, "new": "  replacement text  \n"}),
            ),
        ] {
            let document = arguments(tool, &args);
            let original = document.clone();
            assert_argument_reflow(&document, &cache);
            assert_eq!(document, original);
        }
    }

    #[test]
    fn segmented_tool_headers_keep_roles_when_wrapped_or_in_documents() {
        let p = Palette::new();
        let header = vec![
            Run::new("▸", Role::Indicator),
            Run::new(" read ", Role::ToolName),
            Run::new("@remote", Role::Target),
            Run::new(" a long path ", Role::Plain),
            Run::new("· ", Role::Muted),
            Run::new("Completed", Role::Success),
            Run::new(" · #42", Role::Muted),
        ];
        let text = header_line(&header).to_string();
        let key = model::EntryKey::UnsavedStatus(1);
        let entry = model::Entry::card(key.clone(), header.clone(), None);
        assert_eq!(entry.text(), text);
        let expanded = model::Entry::card(key, header, Some(Document::default()));
        let cache = HighlightCache::default();
        let lines = |rows: Vec<Row>| {
            let rows = rows.into_iter();
            rows.map(|row| (row.line.clone(), row.layout.continued()))
                .collect::<Vec<_>>()
        };
        for width in [8, 25, 100] {
            let rows = rows_of(&entry, width, &cache);
            let spans: Vec<_> = rows.iter().flat_map(|row| &row.line.spans).collect();
            let bold = |span: &Span<'_>| {
                span.style.fg == Some(p.content.fg)
                    && span.style.add_modifier.contains(Modifier::BOLD)
            };
            let name: String = spans
                .iter()
                .filter(|span| bold(span))
                .map(|span| span.content.as_ref())
                .collect();
            assert!(name.contains("read"));
            assert!(
                spans
                    .iter()
                    .any(|span| span.content.contains("▸")
                        && span.style.fg == Some(p.content.primary))
            );
            assert_eq!(lines(rows), lines(rows_of(&expanded, width, &cache)));
        }
    }
}
