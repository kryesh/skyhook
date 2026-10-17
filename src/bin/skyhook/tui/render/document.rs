//! Entry geometry and layout.

use super::super::tool_view::{
    Document, DocumentLine, HighlightCache, Role, Run, Wrap, header_line,
};
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

/// Cells reserved for the counter painted at the right edge of a live entry's
/// header row, as wide as "in 9h 59m 59s".
pub(super) const COUNTER_WIDTH: u16 = 13;
/// Cells a spinner and its gap take before untitled live text.
const SPINNER_GUTTER: u16 = 2;

/// Geometry and framing shared by every entry layout.
pub(super) struct EntryGeometry {
    pub(super) x: u16,
    width: u16,
    row_width: u16,
    pub(super) body_width: u16,
    /// The header row's text width: a live entry's leaves its counter and a gap.
    header_width: u16,
    framing: Framing,
    surface: Surface,
    entry: usize,
    entry_key: std::sync::Arc<model::EntryKey>,
    selectable: bool,
    /// Cells before the text of every row, left to a spinner.
    gutter: u16,
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
        let body_width = block_width
            .saturating_sub(if boxed { 4 } else { indent })
            .max(1);
        let counter = u16::from(entry.timing.live()) * (COUNTER_WIDTH + 1);
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
            body_width,
            header_width: body_width.saturating_sub(counter).max(1),
            framing,
            surface: entry.surface,
            entry: index,
            entry_key: std::sync::Arc::new(entry.key().clone()),
            selectable: entry.selectable(),
            gutter: 0,
        }
    }
    /// Inset the text past a spinner painted at the start of each row: the
    /// gutter is geometry, never part of the text.
    fn reserve_spinner(&mut self) {
        self.gutter = SPINNER_GUTTER;
        self.body_width = self.body_width.saturating_sub(SPINNER_GUTTER).max(1);
        self.header_width = self.header_width.saturating_sub(SPINNER_GUTTER).max(1);
    }
    pub(super) fn row(
        &self,
        line: Line<'static>,
        header: bool,
        layout: markdown::RowLayout,
    ) -> Row {
        let (inset, padding) = match self.framing {
            Framing::Boxed => (2, 2),
            Framing::Hanging => (u16::from(!header), 0),
            Framing::Flush => (0, 0),
        };
        let inset = inset + self.gutter;
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
            layout: layout.with_header(header),
            spinner: None,
        }
    }
    /// A message box's top or bottom edge.
    fn edge(&self) -> Row {
        Row {
            inset: 0,
            text_width: self.row_width,
            ..self.row(Line::default(), false, markdown::RowLayout::spacer())
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
    /// Wrap logical lines, the first as the header row; later lines and
    /// continuations share the hanging indent. A line's lead is decoration
    /// before its first row, and its continuations align under its text.
    /// Returns how much of the header's lead was kept.
    fn push_wrapped(&self, rows: &mut Vec<Row>, lines: Vec<DocumentLine>) -> usize {
        let mut kept = 0;
        let indent = usize::from(self.framing == Framing::Hanging);
        let hanging = (self.body_width as usize).saturating_sub(indent).max(1);
        let mut ids = markdown::TextIds::default();
        for (index, DocumentLine { lead, text, wrap }) in lines.into_iter().enumerate() {
            let id = match index {
                0 => markdown::LineId::default(),
                _ => ids.next(&text),
            };
            let width = if index == 0 {
                self.header_width as usize
            } else {
                hanging
            };
            // As with code padding, source wins over decoration: the lead,
            // blank on continuations, leaves room for the widest grapheme on
            // every row of the line, including the narrower hanging rows.
            let widest = (text.spans.iter())
                .flat_map(|span| cells(&span.content))
                .map(|(_, _, width)| width)
                .max()
                .unwrap_or(0);
            let room = width.min(hanging).saturating_sub(widest.max(1));
            let mut lead = truncated(lead, room);
            lead.spans.retain(|span| !span.content.is_empty());
            let lead_width = lead.width();
            if index == 0 {
                kept = lead_width;
            }
            let first = width - lead_width;
            let rest = hanging.saturating_sub(lead_width).max(1);
            let wrapped = match wrap {
                Wrap::Hard => wrap_line(text, first, rest),
                Wrap::Words => wrap_words(text, first),
            };
            for (part, line) in markdown::LayoutLine::wrapped(wrapped, id).enumerate() {
                let mut row = self.row(line.line, index == 0 && part == 0, line.layout);
                row.layout.prefix = match part {
                    0 => lead.clone(),
                    _ if lead_width == 0 => Line::default(),
                    _ => Line::from(" ".repeat(lead_width)),
                };
                rows.push(row);
            }
        }
        kept
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
    let mut geometry = EntryGeometry::new(entry, width, index);
    let boxed = geometry.framing == Framing::Boxed;
    rows.clear();
    if let Some(request) = entry.request() {
        let line = request_columns.line(request, geometry.body_width, geometry.header_width);
        let mut row = geometry.row(line, true, markdown::RowLayout::default());
        // Request columns leave their first cells to the spinner.
        row.spinner = Some(row.inset);
        rows.push(row);
        return Document::default();
    }
    if boxed {
        rows.push(geometry.edge());
    }
    // A live entry leaves its first cells to the spinner: after a title's
    // disclosure glyph, both decoration in the title's lead, or before an
    // untitled body, which is inset past them. Neither is selected.
    let live = entry.timing.live();
    let title = entry.title();
    let title_lead = |title: &model::Title| Line::from(title.prefix(live));
    // A spinner paints over the header row, which a message box's edge
    // precedes: only an unboxed untitled body gives it the gutter.
    if title.is_none() && live && entry.header().is_none() && !boxed {
        geometry.reserve_spinner();
    }
    let mut fences = Document::default();
    let mut kept = 0;
    if let Some(header) = entry.header() {
        let glyph = entry.disclosure().map(model::Disclosure::glyph);
        let glyph = glyph.map(|glyph| Run::new(glyph, Role::Indicator));
        let mut lines = vec![DocumentLine {
            lead: header_line(&Vec::from_iter(glyph)),
            text: header_line(header),
            wrap: Wrap::Hard,
        }];
        if let Some(body) = entry.document() {
            lines.extend(body.layout_lines(Some(highlights)));
        }
        kept = geometry.push_wrapped(rows, lines);
    } else if matches!(
        entry.surface,
        Surface::User | Surface::Agent | Surface::Reasoning
    ) {
        if let Some(title) = title {
            let mut text = match &title.label {
                model::Label::Plain(label) => Line::from(label.to_string()),
                model::Label::Markdown {
                    source,
                    definitions,
                } => {
                    markdown::title(&format!("{}{}", &**definitions, &**source)).unwrap_or_default()
                }
            };
            text.spans.extend(title.suffix.map(Span::raw));
            if boxed {
                text = text.style(Style::default().add_modifier(Modifier::BOLD));
            }
            let lead = title_lead(title);
            let line = DocumentLine {
                lead,
                text,
                wrap: Wrap::Words,
            };
            kept = geometry.push_wrapped(rows, vec![line]);
        }
        let body = entry.body();
        // Expanded reasoning omits the empty body; message boxes retain it.
        if !entry.expandable() || !body.is_empty() || boxed {
            // A body that starts at the header row wraps whole at the header's width.
            let width = if rows.is_empty() {
                geometry.header_width
            } else {
                geometry.body_width
            };
            let layout = markdown::layout_highlighted(body, true, width.into(), Some(highlights));
            fences = layout.fences;
            for line in layout.lines {
                let layout = line.layout.in_segment(markdown::Segment::Markdown);
                rows.push(geometry.row(line.line, rows.is_empty(), layout));
            }
        }
    } else {
        let body = entry.body();
        let body = (title.is_none() || !body.is_empty()).then_some(body);
        let body = body.iter().flat_map(|body| body.split('\n'));
        let lines = title
            .map(|title| DocumentLine {
                lead: title_lead(title),
                text: Line::from(title.heading()),
                wrap: Wrap::Hard,
            })
            .into_iter()
            .chain(body.map(|line| DocumentLine {
                lead: Line::default(),
                text: Line::from(line.to_owned()),
                wrap: Wrap::Hard,
            }))
            .collect();
        kept = geometry.push_wrapped(rows, lines);
    }
    // The spinner paints only where layout kept room: over a card's state
    // glyph after its kept disclosure glyph, in the gap of a title's lead
    // when the lead kept it, or in an untitled body's gutter.
    if let Some(row) = rows.iter_mut().find(|row| row.layout.header()) {
        let kept = kept as u16;
        row.spinner = match (entry.header(), title) {
            (Some(_), _) => Some(row.inset + kept),
            (None, Some(title)) => {
                let glyph = title.prefix(false).width() as u16;
                (kept > glyph).then_some(row.inset + glyph)
            }
            (None, None) => Some(row.inset.saturating_sub(geometry.gutter)),
        };
    }
    if boxed {
        if let Some(footer) = &entry.footer {
            let footer = Span::styled(footer.to_string(), Style::default().fg(THEME.muted));
            for (index, line) in [Line::default(), Line::from(footer)]
                .into_iter()
                .enumerate()
            {
                let wrapped = wrap_words(line, geometry.body_width as usize);
                let id = markdown::LineId::at(index);
                for line in markdown::LayoutLine::wrapped(wrapped, id) {
                    let layout = line.layout.in_segment(markdown::Segment::Footer);
                    rows.push(geometry.row(line.line, false, layout));
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
    use super::super::tests::{layout, selected_text, text_x};
    use super::*;

    fn arguments(tool: &str, args: &serde_json::Value) -> Document {
        let mut document = Document::default();
        document.line(tool, Role::ToolName);
        document.arguments(Hints::new(tool, args));
        document
    }

    /// `document` as an expanded card copies `expected` at every width.
    fn assert_argument_reflow(document: &Document, expected: &str) {
        let mut body = document.clone();
        let Section::Text { runs: header, .. } = body.sections.remove(0) else {
            panic!("argument fixture starts with its tool header");
        };
        let mut entry = model::Entry::card(model::EntryKey::UnsavedStatus(1), header, Some(body));
        entry.compact_after = true;
        for width in [0, 1, 2, 8, 12, 40, 120, 24] {
            let rows = layout(&entry, width);
            for row in &rows {
                let lead = row.layout.prefix.width() as u16;
                assert!(lead < row.text_width.max(1), "width {width}");
            }
            let start = TextPosition { row: 0, byte: 0 };
            let end = TextPosition {
                row: rows.len() - 1,
                byte: rows.last().unwrap().text().len(),
            };
            let mut blocks = RowBlocks::default();
            blocks.replace_entry(0, rows, Vec::new());
            assert_eq!(
                selected_text(&blocks, (start, end)),
                expected,
                "width {width}"
            );
            assert_eq!(selected_text(&blocks, (end, start)), expected);
        }
    }

    /// A diff's indent and change markers are painted beside its lines, but
    /// selection, copying and the selection highlight cover only the source.
    #[test]
    fn diff_markers_and_indents_are_painted_but_not_selected() {
        let (old, new) = ("let a = 1;\nlet b = 2;", "let a = 3;");
        let args = serde_json::json!({"path": "file.rs", "old": old, "new": new});
        for width in [40, 20] {
            let rows = argument_card("replace", &args, width);
            let marked: Vec<_> = rows
                .iter()
                .filter(|row| {
                    let lead = row.layout.prefix.to_string();
                    ["− ", "+ "].iter().any(|marker| lead.ends_with(marker))
                })
                .collect();
            assert!(marked.len() >= 3, "width {width}");
            for row in &rows {
                let text = row.text();
                assert!(!text.contains("− ") && !text.contains("+ "), "{text:?}");
            }
            for row in marked {
                // A press on the marker starts at the source; the highlight
                // starts where the source does.
                let lead = row.layout.prefix.width() as u16;
                assert_eq!(row.byte_at_column(row.paragraph_x()), 0);
                assert_eq!(text_x(row), row.paragraph_x() + lead);
            }
            let copied = copy_from(rows, "let a = 1;");
            assert_eq!(
                copied,
                format!("{old}\nnew (added)\n{new}"),
                "width {width}"
            );
        }
    }

    /// The expanded card of `tool` with `args`, and its rows at `width`.
    fn argument_card(tool: &str, args: &serde_json::Value, width: u16) -> Vec<Row> {
        let mut document = arguments(tool, args);
        let Section::Text { runs: header, .. } = document.sections.remove(0) else {
            panic!("argument fixture starts with its tool header");
        };
        let key = model::EntryKey::UnsavedStatus(1);
        layout(&model::Entry::card(key, header, Some(document)), width)
    }

    /// Copy from the first row that starts `from` to the end of the card.
    fn copy_from(rows: Vec<Row>, from: &str) -> String {
        let texts: Vec<_> = rows.iter().map(Row::text).collect();
        let first =
            (texts.iter()).position(|text| !text.is_empty() && from.starts_with(text.as_str()));
        let first = first.unwrap_or_else(|| panic!("{from:?} in {texts:?}"));
        let last = rows.len() - 1;
        let mut blocks = RowBlocks::default();
        blocks.replace_entry(0, rows, Vec::new());
        let start = TextPosition {
            row: first,
            byte: 0,
        };
        let end = TextPosition {
            row: last,
            byte: usize::MAX,
        };
        selected_text(&blocks, (start, end))
    }

    /// A card's disclosure glyph is painted before its header, not selected.
    #[test]
    fn card_disclosure_glyphs_are_decoration() {
        let args = serde_json::json!({"path": "f"});
        let rows = argument_card("read", &args, 40);
        assert_eq!(rows[0].layout.prefix.to_string(), "▾ ");
        assert_eq!(copy_from(rows, "read"), "read\nArguments\npath  f");
    }

    /// A diff line paints its marker before its source; however narrow, the
    /// marker gives way so the widest source grapheme still fits.
    #[test]
    fn narrow_diffs_paint_wide_source_graphemes() {
        let args = serde_json::json!({"path": "f", "old": "界x", "new": "y"});
        for (width, painted_source) in [(40, "− 界"), (12, "界")] {
            let rows = argument_card("replace", &args, width);
            let painted: Vec<String> = rows
                .iter()
                .map(|row| {
                    let area = Rect::new(0, 0, width, 1);
                    let mut buffer = Buffer::empty(area);
                    let text = Rect::new(row.paragraph_x(), 0, row.text_width, 1);
                    render_row_line(row, text, &mut buffer, Style::default());
                    (0..width).map(|x| buffer[(x, 0)].symbol()).collect()
                })
                .collect();
            let shown = |text: &str| painted.iter().any(|row| row.contains(text));
            assert!(shown(painted_source) && shown("x"), "{painted:#?}");
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
        let expected = format!(
            "agent\nArguments\nprompt\n{}\n\n  Keep **literal** text, café e\u{301}lan 世界 👩‍💻 together.  \n\n\
             description  short inline prose\nnested\nitems\n1.  nested array prose\n2.\n\
             first paragraph\n  second paragraph  \n\
             token        extraordinarilylongunbrokentoken世界👩‍💻e\u{301}",
            "Please inspect the current implementation and explain every relevant change. "
                .repeat(5)
        );
        assert_argument_reflow(&document, &expected);
        assert_eq!(args, original);
    }

    #[test]
    fn tool_argument_code_keeps_hard_wrap_and_copy_after_resize() {
        let source = "  const message = 'long words stay hard wrapped';\t// café 👩‍💻\n\n    console.log(message);  \n";
        // Sources are shown with tabs expanded; indentation inside them is theirs.
        let shown = "  const message = 'long words stay hard wrapped';    // café 👩‍💻\n\n    console.log(message);  \n";
        for (tool, args, expected) in [
            (
                "script",
                serde_json::json!({"source": source, "description": "prose next to source"}),
                format!("script\nArguments\nsource\n{shown}\ndescription  prose next to source"),
            ),
            (
                "exec",
                serde_json::json!({"command": "  printf '%s  %s'  first second\t\n\n"}),
                "exec\nArguments\ncommand\n  printf '%s  %s'  first second    \n\n".to_owned(),
            ),
            (
                "exec",
                serde_json::json!({"command": ["sh", "-c", "  echo long shell words  \n"], "nested": {"source": source}}),
                format!(
                    "exec\nArguments\ncommand\n1.  sh\n2.  -c\n3.\n  echo long shell words  \n\nnested\nsource\n{shown}"
                ),
            ),
            (
                "write",
                serde_json::json!({"path": "file.js", "content": source}),
                format!("write\nArguments\npath     file.js\ncontent\n{shown}"),
            ),
            (
                "replace",
                serde_json::json!({"path": "file.js", "old": source, "new": "  replacement text  \n"}),
                format!(
                    "replace\nArguments\npath  file.js\nold (removed)\n{shown}\nnew (added)\n  replacement text  \n"
                ),
            ),
        ] {
            let document = arguments(tool, &args);
            let original = document.clone();
            assert_argument_reflow(&document, &expected);
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
        // The glyph leads the entry's text, but is drawn as decoration.
        let header_text = header_line(entry.header().unwrap()).to_string();
        assert_eq!(entry.text(), format!("▸ {header_text}"));
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
            let glyph = &rows[0].layout.prefix.spans[0];
            assert_eq!(
                (&*glyph.content, glyph.style.fg),
                ("▸ ", Some(THEME.primary))
            );
            assert_eq!(styled(rows), styled(layout(&expanded, width)));
        }
    }

    /// A live entry's header row leaves room for its painted counter; a settled
    /// entry's fills the row.
    #[test]
    fn live_header_rows_reserve_their_counter() {
        let text = "word ".repeat(40);
        for surface in [Surface::Muted, Surface::Reasoning] {
            for (timing, live) in [
                (model::Timing::Since(0), true),
                (model::Timing::Untimed, false),
            ] {
                let key = model::EntryKey::UnsavedStatus(1);
                let mut entry = model::Entry::new(key, text.clone(), surface);
                entry.timing = timing;
                let rows = layout(&entry, 60);
                let free = rows[0].text_width - rows[0].text().trim_end().width() as u16;
                assert_eq!(free > COUNTER_WIDTH, live, "{surface:?}");
            }
        }
    }

    #[test]
    fn markdown_titles_keep_their_styling_between_disclosure_and_suffix() {
        let mut title = model::Title::markdown("**Inspecting scripts**", "", false);
        title.suffix = Some(" · incomplete");
        let key = model::EntryKey::UnsavedStatus(1);
        let entry = model::Entry::titled(key, title, String::new(), Surface::Reasoning);
        let rows = layout(&entry, 60);
        // The disclosure glyph is painted before the title, not selected with it.
        assert_eq!(rows[0].layout.prefix.to_string(), "▸ ");
        assert_eq!(rows[0].text().trim_end(), "Inspecting scripts · incomplete");
        let spans = &rows[0].line.spans;
        let fg = |text: &str| {
            spans
                .iter()
                .find(|span| span.content == text)
                .map(|span| span.style.fg)
        };
        assert_eq!(fg("Inspecting scripts"), Some(Some(THEME.strong)));
        assert_eq!(fg(" · incomplete"), Some(None));
    }
}
