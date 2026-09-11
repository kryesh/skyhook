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
            selectable: entry_selectable(entry),
            tool_gutter: false,
        }
    }
    pub(super) fn row(&self, line: Line<'static>, header: bool, continued: bool) -> Row {
        Row {
            line: std::sync::Arc::new(line),
            header,
            x: self.x,
            width: self.row_width,
            surface: self.surface,
            entry: self.entry,
            selectable: self.selectable,
            blank: false,
            continued,
            layout: markdown::RowLayout::default(),
            inset: u16::from(self.tool_gutter && !header),
        }
    }
    pub(super) fn blank(&self, surface: Surface, x: u16, width: u16) -> Row {
        Row {
            line: std::sync::Arc::new(Line::default()),
            header: false,
            x,
            width,
            surface,
            entry: self.entry,
            selectable: self.selectable,
            blank: true,
            continued: false,
            layout: markdown::RowLayout::default(),
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
    geometry.tool_gutter = expanded && entry.expandable && entry.surface == Surface::Tool;
    let block = matches!(entry.surface, Surface::User | Surface::Agent);
    let lines = if let Some(document) = &entry.document {
        document.layout_lines(Some(highlights), p.content.light)
    } else {
        debug_assert!(!block && entry.surface != Surface::Reasoning);
        model::clean(&entry.text)
            .split('\n')
            .map(|s| {
                (
                    Line::from(s.to_owned()),
                    super::super::tool_view::Wrap::Hard,
                )
            })
            .collect()
    };
    let mut rows = Vec::new();
    if block {
        rows.push(geometry.blank(entry.surface, geometry.x, geometry.block_width));
    }
    let mut header = true;
    for (line_index, (mut line, wrapping)) in lines.into_iter().enumerate() {
        if line_index == 0 && entry.surface == Surface::Tool {
            if let Some(header) = &entry.header {
                line = super::super::tool_view::header_line(header, p.content.light);
            } else {
                for span in &mut line.spans {
                    span.style.fg = Some(p.content.fg);
                }
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
    use super::*;

    fn assert_argument_reflow(
        document: &super::super::super::tool_view::Document,
        cache: &super::super::super::tool_view::HighlightCache,
        light: bool,
    ) {
        use super::super::super::tool_view::Wrap;
        let entry = model::Entry {
            key: "tool-wrap".into(),
            text: document.plain_text(),
            surface: Surface::Tool,
            expandable: true,
            default_open: true,
            running: false,
            footer: None,
            request: None,
            indent: 0,
            job: None,
            compact_after: true,
            header: None,
            document: Some(document.clone()),
        };
        for width in [0, 1, 2, 5, 12, 24, 40, 64, 120, 24] {
            let geometry = EntryGeometry::new(&entry, width, 0);
            let rows = layout_document_or_plain(&entry, width, Palette::new(light), cache, 0);
            let expected = document
                .layout_lines(Some(cache), light)
                .into_iter()
                .enumerate()
                .flat_map(|(index, (line, wrap))| {
                    let body = geometry.body_width.saturating_sub(1).max(1) as usize;
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
                .collect::<Vec<_>>();
            assert_eq!(
                rows.iter().map(Row::text).collect::<Vec<_>>(),
                expected
                    .iter()
                    .map(|line| line
                        .spans
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect::<String>())
                    .collect::<Vec<_>>(),
                "width {width}, light {light}"
            );
            let selection = (
                TextPosition { row: 0, byte: 0 },
                TextPosition {
                    row: rows.len() - 1,
                    byte: rows.last().unwrap().text().len(),
                },
            );
            let mut blocks = RowBlocks::default();
            *blocks.block_mut(0) = rows;
            blocks.finish_update(0);
            assert_eq!(
                selected_text(&blocks, selection),
                entry.text,
                "width {width}"
            );
            assert_eq!(
                selected_text(&blocks, (selection.1, selection.0)),
                entry.text
            );
        }
    }

    #[test]
    fn tool_argument_prose_wraps_words_without_interpreting_or_losing_text() {
        use super::super::super::tool_view::{Document, HighlightCache, Role};
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
        let cache = HighlightCache::default();
        let mut document = Document::default();
        document.line("agent", Role::ToolName);
        document.arguments("agent", &args);
        for light in [false, true] {
            assert_argument_reflow(&document, &cache, light);
            // A long prompt's short words must never be split, even though its
            // paragraphs are longer than the viewport and include indentation.
            for (line, wrapping) in document.layout_lines(None, light) {
                if wrapping != super::super::super::tool_view::Wrap::Words {
                    continue;
                }
                let original = line
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>();
                if original.contains("extraordinarily") {
                    continue;
                }
                for width in [24, 40, 64] {
                    let parts = wrap_words(line.clone(), width);
                    let words = parts
                        .iter()
                        .flat_map(|part| {
                            part.spans
                                .iter()
                                .map(|span| span.content.as_ref())
                                .collect::<String>()
                                .split_whitespace()
                                .map(str::to_owned)
                                .collect::<Vec<_>>()
                        })
                        .collect::<Vec<_>>();
                    assert_eq!(words, original.split_whitespace().collect::<Vec<_>>());
                }
            }
        }
        assert_eq!(args, original);
    }

    #[test]
    fn tool_argument_code_keeps_hard_wrap_and_copy_after_highlights_and_resize() {
        use super::super::super::tool_view::{Document, HighlightCache, Role};
        let source = "  const message = 'long words stay hard wrapped';\t// café 👩‍💻\n\n    console.log(message);  \n";
        let cases = [
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
            (
                "patch",
                serde_json::json!({"patch": "@@ -1 +1 @@\n-  old words\n+  new words  \n"}),
            ),
        ];
        let mut cache = HighlightCache::default();
        for (tool, args) in cases {
            let mut document = Document::default();
            document.line(tool, Role::ToolName);
            document.arguments(tool, &args);
            let original = document.clone();
            for light in [false, true] {
                assert_argument_reflow(&document, &cache, light);
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                loop {
                    cache.prepare(std::iter::once(&document), light);
                    if document.highlight_sources().next().is_none()
                        || cache.is_fully_highlighted(&document, light)
                    {
                        break;
                    }
                    assert!(
                        std::time::Instant::now() < deadline,
                        "highlight worker did not finish"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                assert_argument_reflow(&document, &cache, light);
                assert_eq!(document, original);
            }
        }
    }

    #[test]
    fn segmented_tool_headers_keep_roles_when_wrapped_or_in_documents() {
        use super::super::super::tool_view::{Document, Role, Run, Section, header_line};
        for light in [false, true] {
            let p = Palette::new(light);
            let header = vec![
                Run::new("▸", Role::Indicator),
                Run::new(" read ", Role::ToolName),
                Run::new("@remote", Role::Target),
                Run::new(" a long path ", Role::Plain),
                Run::new("· ", Role::Muted),
                Run::new("Completed", Role::Success),
                Run::new(" · #42", Role::Muted),
            ];
            let line = header_line(&header, light);
            let text = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            let entry = model::Entry {
                key: "tool-header".into(),
                text,
                surface: Surface::Tool,
                expandable: true,
                default_open: false,
                running: false,
                footer: None,
                request: None,
                indent: 0,
                job: None,
                document: None,
                header: Some(header.clone()),
                compact_after: false,
            };
            let mut expanded = entry.clone();
            expanded.document = Some(Document {
                sections: vec![Section::Line(header)],
            });
            let cache = super::super::super::tool_view::HighlightCache::default();
            for width in [8, 25, 100] {
                let rows = layout_document_or_plain(&entry, width, p, &cache, 0);
                let document_rows = layout_document_or_plain(&expanded, width, p, &cache, 0);
                let lines = |rows: &[Row]| {
                    rows.iter()
                        .map(|row| (row.line.clone(), row.continued))
                        .collect::<Vec<_>>()
                };
                assert_eq!(lines(&rows), lines(&document_rows));
                let name = rows
                    .iter()
                    .flat_map(|row| &row.line.spans)
                    .filter(|span| {
                        span.style.fg == Some(p.content.fg)
                            && span.style.add_modifier.contains(Modifier::BOLD)
                    })
                    .map(|span| span.content.as_ref())
                    .collect::<String>();
                assert!(name.contains("read"));
                assert!(
                    rows.iter()
                        .flat_map(|row| &row.line.spans)
                        .any(|span| span.content.contains("▸")
                            && span.style.fg == Some(p.content.primary))
                );
            }
        }
    }

    fn layout_document_or_plain(
        entry: &model::Entry,
        width: u16,
        p: Palette,
        highlights: &super::super::super::tool_view::HighlightCache,
        index: usize,
    ) -> Vec<Row> {
        layout_document_or_plain_with_expansion(
            entry,
            width,
            p,
            highlights,
            index,
            entry.default_open,
        )
    }
}
