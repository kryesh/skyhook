//! Entry geometry and layout.

use super::super::tool_view::{Document, HighlightCache, Wrap, header_line};
use super::*;

/// How an entry's text sits inside its rows.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Framing {
    /// User and agent messages: a box around padded text.
    Boxed,
    /// An expanded tool: the body hangs one cell inside its header.
    Hanging,
    Flush,
}

/// Geometry and framing shared by every entry layout.
pub(super) struct EntryGeometry {
    pub(super) x: u16,
    width: u16,
    row_width: u16,
    pub(super) body_width: u16,
    framing: Framing,
    surface: Surface,
    entry: usize,
    entry_key: std::sync::Arc<model::EntryKey>,
    selectable: bool,
}
impl EntryGeometry {
    pub(super) fn new(entry: &model::Entry, width: u16, index: usize) -> Self {
        let framing = match entry.surface {
            Surface::User | Surface::Agent => Framing::Boxed,
            Surface::Tool if entry.open() => Framing::Hanging,
            _ => Framing::Flush,
        };
        let boxed = framing == Framing::Boxed;
        let available = width.saturating_sub(5).max(1);
        // Message boxes keep one column on the sender's side and three on the
        // opposite side, leaving all remaining width available for content.
        let block_width = if boxed {
            width.saturating_sub(4).max(1)
        } else {
            available
        };
        let indent = entry.indent.min(available / 3);
        Self {
            x: if entry.surface == Surface::User {
                width.saturating_sub(block_width + 1)
            } else if boxed {
                1
            } else {
                2 + indent
            },
            width,
            row_width: block_width.saturating_sub(if boxed { 0 } else { indent }),
            body_width: block_width
                .saturating_sub(if boxed { 4 } else { indent })
                .max(1),
            framing,
            surface: entry.surface,
            entry: index,
            entry_key: std::sync::Arc::new(entry.key().clone()),
            selectable: entry.selectable(),
        }
    }
    pub(super) fn row(&self, line: Line<'static>, header: bool, continued: bool) -> Row {
        let (inset, padding) = match self.framing {
            Framing::Boxed => (2, 2),
            Framing::Hanging => (u16::from(!header), 0),
            Framing::Flush => (0, 0),
        };
        Row {
            line: std::sync::Arc::new(line),
            x: self.x,
            width: self.row_width,
            inset,
            text_width: self.row_width.saturating_sub(inset + padding),
            surface: self.surface,
            entry: self.entry,
            entry_key: self.entry_key.clone(),
            selectable: self.selectable,
            layout: markdown::RowLayout::default().with_flow(header, continued),
        }
    }
    /// A message box's top or bottom edge.
    fn edge(&self) -> Row {
        Row {
            inset: 0,
            text_width: self.row_width,
            layout: markdown::RowLayout::spacer(),
            ..self.row(Line::default(), false, false)
        }
    }
    /// The separator after an entry.
    fn spacer(&self) -> Row {
        Row {
            x: 0,
            width: self.width,
            text_width: self.width,
            surface: Surface::Muted,
            ..self.edge()
        }
    }
    /// Wrap logical lines; later lines and continuations share the hanging indent.
    fn push_wrapped(&self, rows: &mut Vec<Row>, lines: Vec<(Line<'static>, Wrap)>) {
        let body_width = self.body_width as usize;
        let indent = usize::from(self.framing == Framing::Hanging);
        let hanging = body_width.saturating_sub(indent).max(1);
        for (index, (line, wrap)) in lines.into_iter().enumerate() {
            let first = if index == 0 { body_width } else { hanging };
            let wrapped = match wrap {
                Wrap::Hard => wrap_line(line, first, hanging),
                Wrap::Words => wrap_words(line, first),
            };
            for (part, line) in wrapped.into_iter().enumerate() {
                rows.push(self.row(line, index == 0 && part == 0, part > 0));
            }
        }
    }
}

/// Replace `rows` with the entry's layout at `width`, returning the fenced
/// code its Markdown highlights.
pub(super) fn update_entry_rows(
    rows: &mut Vec<Row>,
    entry: &model::Entry,
    index: usize,
    width: u16,
    highlights: &HighlightCache,
    request_columns: RequestColumns,
) -> Document {
    let geometry = EntryGeometry::new(entry, width, index);
    let boxed = geometry.framing == Framing::Boxed;
    rows.clear();
    if let Some(request) = entry.request() {
        let line = request_columns.line(request, geometry.body_width);
        rows.push(geometry.row(line, true, false));
        return Document::default();
    }
    if boxed {
        rows.push(geometry.edge());
    }
    // A running entry leaves its first cells to the spinner: after the
    // disclosure glyph of a title, or before an untitled body.
    let title = entry.title().map(|title| title.line(entry.running));
    let gutter = if title.is_none() && entry.running {
        "  "
    } else {
        ""
    };
    let mut fences = Document::default();
    if let Some(header) = entry.header() {
        let mut lines = vec![(header_line(header), Wrap::Hard)];
        if let Some(body) = entry.document() {
            lines.extend(body.layout_lines(Some(highlights)));
        }
        geometry.push_wrapped(rows, lines);
    } else if matches!(
        entry.surface,
        Surface::User | Surface::Agent | Surface::Reasoning
    ) {
        if let Some(title) = title {
            let title = if boxed {
                Line::from(Span::styled(
                    title,
                    Style::default().add_modifier(Modifier::BOLD),
                ))
            } else {
                Line::from(title)
            };
            geometry.push_wrapped(rows, vec![(title, Wrap::Words)]);
        }
        let body = entry.body();
        // Expanded reasoning omits the empty body; message boxes retain it.
        if !entry.expandable() || !body.is_empty() || boxed {
            let body_width = geometry.body_width as usize;
            let layout = stream::layout_highlighted(body, body_width, gutter, Some(highlights));
            fences = layout.fences;
            for line in layout.lines {
                let header = rows.is_empty();
                let continued = line.layout.continued();
                let mut row = geometry.row(line.line, header, continued);
                row.layout = line.layout.with_flow(header, continued);
                rows.push(row);
            }
        }
    } else {
        let body = entry.body();
        let body = (title.is_none() || !body.is_empty()).then_some(body);
        let body = body.iter().flat_map(|body| body.split('\n')).enumerate();
        let lines = title
            .into_iter()
            .chain(body.map(|(index, line)| {
                let gutter = if index == 0 { gutter } else { "" };
                format!("{gutter}{line}")
            }))
            .map(|line| (Line::from(line), Wrap::Hard))
            .collect();
        geometry.push_wrapped(rows, lines);
    }
    if boxed {
        if let Some(footer) = &entry.footer {
            let footer = Span::styled(footer.to_string(), Style::default().fg(THEME.muted));
            for line in [Line::default(), Line::from(footer)] {
                let wrapped = wrap_words(line, geometry.body_width as usize);
                for (part, line) in wrapped.into_iter().enumerate() {
                    rows.push(geometry.row(line, false, part > 0));
                }
            }
        }
        rows.push(geometry.edge());
    }
    if !entry.compact_after {
        rows.push(geometry.spacer());
    }
    fences
}

#[cfg(test)]
mod tests {
    use super::super::super::tool_view::{Hints, Role, Run, Section};
    use super::super::tests::layout;
    use super::*;

    fn arguments(tool: &str, args: &serde_json::Value) -> Document {
        let mut document = Document::default();
        document.line(tool, Role::ToolName);
        document.arguments(Hints::new(tool, args));
        document
    }

    fn assert_argument_reflow(document: &Document) {
        let mut body = document.clone();
        let Section::Line(header) = body.sections.remove(0) else {
            panic!("argument fixture starts with its tool header");
        };
        let mut entry = model::Entry::card(model::EntryKey::UnsavedStatus(1), header, Some(body));
        entry.compact_after = true;
        let mut document = document.clone();
        document.sections[0] = Section::Line(entry.header().unwrap().to_vec());
        for width in [0, 1, 2, 8, 12, 40, 120, 24] {
            let geometry = EntryGeometry::new(&entry, width, 0);
            let rows = layout(&entry, width);
            let body = geometry.body_width.saturating_sub(1).max(1) as usize;
            let lines = document.layout_lines(None).into_iter().enumerate();
            let expected: Vec<_> = lines
                .flat_map(|(index, (line, wrap))| {
                    let first = if index == 0 {
                        geometry.body_width as usize
                    } else {
                        body
                    };
                    match wrap {
                        Wrap::Hard => wrap_line(line, first, body),
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
        assert_argument_reflow(&document);
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
        for (tool, args) in [
            (
                "script",
                serde_json::json!({"source": source, "description": "prose next to source"}),
            ),
            (
                "exec",
                serde_json::json!({"command": "  printf '%s  %s'  first second\t\n\n"}),
            ),
            (
                "exec",
                serde_json::json!({"command": ["sh", "-c", "  echo long shell words  \n"], "nested": {"source": source}}),
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
            assert_argument_reflow(&document);
            assert_eq!(document, original);
        }
    }

    #[test]
    fn segmented_tool_headers_keep_roles_when_wrapped_or_in_documents() {
        let header = vec![
            Run::new("read ", Role::ToolName),
            Run::new("@remote", Role::Target),
            Run::new(" a long path ", Role::Plain),
            Run::new("· ", Role::Muted),
            Run::new("Completed", Role::Success),
            Run::new(" · #42", Role::Muted),
        ];
        let key = model::EntryKey::UnsavedStatus(1);
        let entry = model::Entry::card(key.clone(), header.clone(), None);
        assert_eq!(
            entry.text(),
            header_line(entry.header().unwrap()).to_string()
        );
        let expanded = model::Entry::card(key, header, Some(Document::default()));
        // An expanded body only changes where the header wraps, not its styles.
        let styled = |rows: Vec<Row>| {
            let spans = rows.into_iter().flat_map(|row| row.line.spans.clone());
            let chars = spans.flat_map(|span| {
                let style = span.style;
                span.content
                    .chars()
                    .map(move |ch| (ch, style))
                    .collect::<Vec<_>>()
            });
            chars.collect::<Vec<_>>()
        };
        for width in [8, 25, 100] {
            let rows = layout(&entry, width);
            let spans: Vec<_> = rows.iter().flat_map(|row| &row.line.spans).collect();
            let bold = |span: &Span<'_>| {
                span.style.fg == Some(THEME.fg) && span.style.add_modifier.contains(Modifier::BOLD)
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
                    .any(|span| span.content.contains("▸") && span.style.fg == Some(THEME.primary))
            );
            // Past the disclosure glyph.
            assert_eq!(styled(rows)[1..], styled(layout(&expanded, width))[1..]);
        }
    }

    #[test]
    fn full_entry_replacements_preserve_unicode_on_resize() {
        let body =
            "words 界 👩‍💻\n\n```rust\nlet n = 4;\n```\n\n| A | B |\n| - | - |\n| x | long words |";
        for surface in [Surface::Reasoning, Surface::User, Surface::Agent] {
            let entry = model::Entry::titled(
                model::EntryKey::UnsavedStatus(1),
                model::Title::disclosed("Title", true),
                body.to_owned(),
                surface,
            );
            for width in [24, 8, 40] {
                let rows = layout(&entry, width);
                assert!(rows.iter().any(|row| row.text().contains('界')));
                assert!(rows.iter().all(|row| row.width <= width));
            }
        }
    }
}
